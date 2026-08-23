//! Rust port of Thalamus's C++ ImageViewer (`src/thalamus/image_viewer.cpp`
//! in the main Thalamus repo): an SDL + Vulkan window that renders the most
//! recent frame handed to it as a single, aspect-ratio-preserving textured
//! quad (a fullscreen triangle clipped to a letterboxed viewport, matching
//! the original's `texture.vert`/`texture.frag` shaders).
//!
//! Unlike the C++ version, this doesn't own an event pump (Thalamus already
//! runs one -- see `ThalamusAPI::subscribe_sdl_events`) and doesn't forward
//! keyboard/mouse input back into a source node; it only tracks resize and
//! close so `update()` can rebuild the swapchain and callers can know when
//! to drop the viewer.

use ash::khr;
use ash::vk;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::api::{ImageFormat, OnDrop, SDLWindow, ThalamusAPI};
use crate::ffi::{
  THALAMUS_SDL_EVENT_QUIT, THALAMUS_SDL_EVENT_WINDOW_CLOSE_REQUESTED, THALAMUS_SDL_EVENT_WINDOW_RESIZED,
  THALAMUS_SDL_WINDOW_RESIZABLE, THALAMUS_SDL_WINDOW_VULKAN, ThalamusSDLEvent,
};

const MAX_FRAMES: usize = 2;

static VERT_SPV: &[u8] = include_bytes!("shaders/texture.vert.spv");
static FRAG_SPV: &[u8] = include_bytes!("shaders/texture.frag.spv");

fn spirv_words(bytes: &[u8]) -> Vec<u32> {
  bytes.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Number of tightly-packed bytes per pixel for the formats this viewer
/// knows how to display, or `None` for formats it doesn't (yet) support
/// (the YUV variants -- mirrors `channels_for_format` in image_viewer.cpp).
fn channels_for_format(format: ImageFormat) -> Option<u32> {
  match format {
    ImageFormat::Gray => Some(1),
    ImageFormat::RGB => Some(3),
    ImageFormat::YUYV422 | ImageFormat::YUV420P | ImageFormat::YUVJ420P => None,
  }
}

/// A single frame to display, borrowed for the duration of `update`.
pub struct ImageFrame<'a> {
  pub data: &'a [u8],
  pub width: u32,
  pub height: u32,
  pub format: ImageFormat,
}

fn find_mem_type(
  instance: &ash::Instance,
  phys: vk::PhysicalDevice,
  type_bits: u32,
  props: vk::MemoryPropertyFlags,
) -> u32 {
  let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };
  for i in 0..mem_props.memory_type_count {
    if (type_bits & (1 << i)) != 0 && mem_props.memory_types[i as usize].property_flags.contains(props) {
      return i;
    }
  }
  panic!("No suitable Vulkan memory type");
}

fn make_buffer(
  device: &ash::Device,
  instance: &ash::Instance,
  phys: vk::PhysicalDevice,
  size: vk::DeviceSize,
  usage: vk::BufferUsageFlags,
  props: vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory), String> {
  unsafe {
    let buf = device
      .create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE), None)
      .map_err(|e| format!("{e:?}"))?;
    let req = device.get_buffer_memory_requirements(buf);
    let mem = device
      .allocate_memory(
        &vk::MemoryAllocateInfo::default()
          .allocation_size(req.size)
          .memory_type_index(find_mem_type(instance, phys, req.memory_type_bits, props)),
        None,
      )
      .map_err(|e| format!("{e:?}"))?;
    device.bind_buffer_memory(buf, mem, 0).map_err(|e| format!("{e:?}"))?;
    Ok((buf, mem))
  }
}

fn record_barrier(
  device: &ash::Device,
  cb: vk::CommandBuffer,
  image: vk::Image,
  from: vk::ImageLayout,
  to: vk::ImageLayout,
  src_access: vk::AccessFlags,
  dst_access: vk::AccessFlags,
  src_stage: vk::PipelineStageFlags,
  dst_stage: vk::PipelineStageFlags,
) {
  let barrier = vk::ImageMemoryBarrier::default()
    .old_layout(from)
    .new_layout(to)
    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
    .image(image)
    .subresource_range(vk::ImageSubresourceRange {
      aspect_mask: vk::ImageAspectFlags::COLOR,
      base_mip_level: 0,
      level_count: 1,
      base_array_layer: 0,
      layer_count: 1,
    })
    .src_access_mask(src_access)
    .dst_access_mask(dst_access);
  unsafe {
    device.cmd_pipeline_barrier(cb, src_stage, dst_stage, vk::DependencyFlags::empty(), &[], &[], &[barrier]);
  }
}

/// Everything needed to sample one frame's worth of texture data, doubled up
/// (one per frame-in-flight) so uploading a new frame never stomps on a
/// texture the GPU might still be reading from a previous submit.
#[derive(Default)]
struct Texture {
  w: u32,
  h: u32,
  channels: u32, // bytes per pixel stored in the VkImage itself: 1 or 4
  image: vk::Image,
  memory: vk::DeviceMemory,
  view: vk::ImageView,
  stage_buf: vk::Buffer,
  stage_mem: vk::DeviceMemory,
  stage_mapped: *mut std::ffi::c_void,
}

impl Texture {
  fn destroy(&mut self, device: &ash::Device) {
    unsafe {
      if !self.stage_mapped.is_null() {
        device.unmap_memory(self.stage_mem);
        self.stage_mapped = std::ptr::null_mut();
      }
      if self.stage_buf != vk::Buffer::null() {
        device.destroy_buffer(self.stage_buf, None);
        self.stage_buf = vk::Buffer::null();
      }
      if self.stage_mem != vk::DeviceMemory::null() {
        device.free_memory(self.stage_mem, None);
        self.stage_mem = vk::DeviceMemory::null();
      }
      if self.view != vk::ImageView::null() {
        device.destroy_image_view(self.view, None);
        self.view = vk::ImageView::null();
      }
      if self.image != vk::Image::null() {
        device.destroy_image(self.image, None);
        self.image = vk::Image::null();
      }
      if self.memory != vk::DeviceMemory::null() {
        device.free_memory(self.memory, None);
        self.memory = vk::DeviceMemory::null();
      }
    }
  }
}

pub struct ImageViewer {
  api: ThalamusAPI,
  window: SDLWindow,
  window_id: u32,
  #[allow(dead_code)]
  entry: ash::Entry, // kept alive; ash::Instance/Device borrow its function table
  instance: ash::Instance,
  device: ash::Device,
  physical_device: vk::PhysicalDevice,
  surface_loader: khr::surface::Instance,
  surface: vk::SurfaceKHR,
  swapchain_loader: khr::swapchain::Device,
  swapchain: vk::SwapchainKHR,
  surface_format: vk::SurfaceFormatKHR,
  extent: vk::Extent2D,
  image_views: Vec<vk::ImageView>,
  framebuffers: Vec<vk::Framebuffer>,
  render_done: Vec<vk::Semaphore>, // one per swapchain image, not per frame-in-flight
  render_pass: vk::RenderPass,
  cmd_pool: vk::CommandPool,
  cmd_buffers: [vk::CommandBuffer; MAX_FRAMES],
  image_avail: [vk::Semaphore; MAX_FRAMES],
  in_flight: [vk::Fence; MAX_FRAMES],
  frame: usize,
  dirty: bool,
  should_close: bool,

  sampler: vk::Sampler,
  desc_layout: vk::DescriptorSetLayout,
  desc_pool: vk::DescriptorPool,
  desc_sets: [vk::DescriptorSet; MAX_FRAMES],
  pipe_layout: vk::PipelineLayout,
  pipeline: vk::Pipeline,
  textures: [Texture; MAX_FRAMES],

  events: Rc<RefCell<VecDeque<ThalamusSDLEvent>>>,
  _event_subscription: OnDrop,
}

impl ImageViewer {
  pub fn new(api: ThalamusAPI, title: &str, x: i32, y: i32, width: i32, height: i32) -> Result<Self, String> {
    let window = api.create_sdl_window(title, width, height, THALAMUS_SDL_WINDOW_VULKAN | THALAMUS_SDL_WINDOW_RESIZABLE)?;
    window.set_position(x, y);
    let window_id = window.id();

    let entry = unsafe { ash::Entry::load() }.map_err(|e| e.to_string())?;
    let instance = unsafe { api.load_vulkan_instance(&entry) };
    let physical_device = api.vulkan_physical_device();
    let device = unsafe { api.load_vulkan_device(&instance) };

    let surface = window.create_vulkan_surface(instance.handle())?;
    let surface_loader = khr::surface::Instance::new(&entry, &instance);
    let swapchain_loader = khr::swapchain::Device::new(&instance, &device);

    let surface_format = unsafe {
      let formats = surface_loader.get_physical_device_surface_formats(physical_device, surface).map_err(|e| format!("{e:?}"))?;
      formats.iter().find(|f| f.format == vk::Format::B8G8R8A8_UNORM).copied().unwrap_or(formats[0])
    };

    let render_pass = unsafe { Self::create_render_pass(&device, surface_format.format) }.map_err(|e| format!("{e:?}"))?;
    let cmd_pool = api.create_vulkan_command_pool();

    let sampler = unsafe {
      device.create_sampler(
        &vk::SamplerCreateInfo::default()
          .mag_filter(vk::Filter::LINEAR)
          .min_filter(vk::Filter::LINEAR)
          .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
          .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let desc_layout = unsafe {
      let binding = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
      device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)
    }
    .map_err(|e| format!("{e:?}"))?;

    let desc_pool = unsafe {
      let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(MAX_FRAMES as u32)];
      device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(MAX_FRAMES as u32).pool_sizes(&sizes), None)
    }
    .map_err(|e| format!("{e:?}"))?;

    let desc_sets: [vk::DescriptorSet; MAX_FRAMES] = {
      let layouts = [desc_layout; MAX_FRAMES];
      let sets = unsafe {
        device.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default().descriptor_pool(desc_pool).set_layouts(&layouts))
      }
      .map_err(|e| format!("{e:?}"))?;
      sets.try_into().map_err(|_| "unexpected descriptor set count".to_string())?
    };

    let (pipe_layout, pipeline) = unsafe { Self::create_pipeline(&device, desc_layout, render_pass) }.map_err(|e| format!("{e:?}"))?;

    let cmd_buffers: [vk::CommandBuffer; MAX_FRAMES] = unsafe {
      device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default().command_pool(cmd_pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(MAX_FRAMES as u32),
      )
    }
    .map_err(|e| format!("{e:?}"))?
    .try_into()
    .map_err(|_| "unexpected command buffer count".to_string())?;

    let mut image_avail = [vk::Semaphore::null(); MAX_FRAMES];
    let mut in_flight = [vk::Fence::null(); MAX_FRAMES];
    unsafe {
      for i in 0..MAX_FRAMES {
        image_avail[i] = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(|e| format!("{e:?}"))?;
        in_flight[i] = device
          .create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)
          .map_err(|e| format!("{e:?}"))?;
      }
    }

    let events: Rc<RefCell<VecDeque<ThalamusSDLEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let events_for_cb = events.clone();
    let event_subscription = api.subscribe_sdl_events(move |event| {
      events_for_cb.borrow_mut().push_back(*event);
    });

    let mut result = ImageViewer {
      api,
      window,
      window_id,
      entry,
      instance,
      device,
      physical_device,
      surface_loader,
      surface,
      swapchain_loader,
      swapchain: vk::SwapchainKHR::null(),
      surface_format,
      extent: vk::Extent2D::default(),
      image_views: Vec::new(),
      framebuffers: Vec::new(),
      render_done: Vec::new(),
      render_pass,
      cmd_pool,
      cmd_buffers,
      image_avail,
      in_flight,
      frame: 0,
      dirty: true,
      should_close: false,
      sampler,
      desc_layout,
      desc_pool,
      desc_sets,
      pipe_layout,
      pipeline,
      textures: Default::default(),
      events,
      _event_subscription: event_subscription,
    };
    result.recreate_swapchain()?;
    Ok(result)
  }

  pub fn should_close(&self) -> bool {
    self.should_close
  }

  /// Current window position and size (`(x, y, w, h)`), in the same units
  /// `new()`'s `x`/`y`/`width`/`height` take.
  pub fn position_size(&self) -> (i32, i32, i32, i32) {
    let (x, y) = self.window.position();
    let (w, h) = self.window.size();
    (x, y, w, h)
  }

  /// Drains pending SDL events, rebuilds the swapchain if needed, and -- if
  /// `frame` holds a new image in a supported format -- uploads and presents
  /// it as a single aspect-ratio-preserving textured quad. Safe to call with
  /// `frame: None` when no new data has arrived since the last call; the
  /// previously presented frame simply stays on screen. Call periodically
  /// (e.g. from a repeating timer) on the thread that created this viewer.
  pub fn update(&mut self, frame: Option<ImageFrame>) {
    self.apply_pending_events();

    if self.dirty {
      if let Err(e) = self.recreate_swapchain() {
        println!("ImageViewer: recreate_swapchain failed: {e}");
        return;
      }
      if self.dirty {
        return; // zero-size (e.g. minimized) -- nothing to draw this tick
      }
    }
    if self.extent.width == 0 || self.extent.height == 0 {
      return;
    }

    let f = self.frame % MAX_FRAMES;
    match unsafe { self.device.get_fence_status(self.in_flight[f]) } {
      Ok(true) => {}
      _ => return, // still in use by the GPU -- drop this tick's frame
    }

    let Some(frame) = frame else { return }; // nothing new to draw yet
    let Some(src_channels) = channels_for_format(frame.format) else { return }; // unsupported pixel format

    let (image_index, suboptimal) = match unsafe {
      self.swapchain_loader.acquire_next_image(self.swapchain, 0, self.image_avail[f], vk::Fence::null())
    } {
      Ok(v) => v,
      Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
        self.dirty = true;
        return;
      }
      Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => return, // no image ready right now
      Err(e) => {
        println!("ImageViewer: acquire_next_image failed: {e:?}");
        return;
      }
    };
    let _ = suboptimal;

    unsafe {
      if let Err(e) = self.device.reset_fences(&[self.in_flight[f]]) {
        println!("ImageViewer: reset_fences failed: {e:?}");
        return;
      }

      let cb = self.cmd_buffers[f];
      let _ = self.device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty());
      if let Err(e) = self.device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()) {
        println!("ImageViewer: begin_command_buffer failed: {e:?}");
        return;
      }

      if let Err(e) = self.upload_texture(f, cb, frame.data, frame.width, frame.height, src_channels) {
        println!("ImageViewer: upload_texture failed: {e}");
        let _ = self.device.end_command_buffer(cb);
        return;
      }

      let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } }];
      let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(self.render_pass)
        .framebuffer(self.framebuffers[image_index as usize])
        .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
        .clear_values(&clear);
      self.device.cmd_begin_render_pass(cb, &rp_begin, vk::SubpassContents::INLINE);
      self.device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.pipeline);

      // Aspect-ratio preserving viewport (letterboxed).
      let img_aspect = self.textures[f].w as f32 / self.textures[f].h as f32;
      let win_aspect = self.extent.width as f32 / self.extent.height as f32;
      let (vp_w, vp_h, vp_x, vp_y) = if win_aspect > img_aspect {
        let h = self.extent.height as f32;
        let w = h * img_aspect;
        ((w), h, (self.extent.width as f32 - w) / 2.0, 0.0)
      } else {
        let w = self.extent.width as f32;
        let h = w / img_aspect;
        (w, h, 0.0, (self.extent.height as f32 - h) / 2.0)
      };
      let viewport = [vk::Viewport { x: vp_x, y: vp_y, width: vp_w, height: vp_h, min_depth: 0.0, max_depth: 1.0 }];
      let scissor =
        [vk::Rect2D { offset: vk::Offset2D { x: vp_x as i32, y: vp_y as i32 }, extent: vk::Extent2D { width: vp_w as u32, height: vp_h as u32 } }];
      self.device.cmd_set_viewport(cb, 0, &viewport);
      self.device.cmd_set_scissor(cb, 0, &scissor);

      self.device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, self.pipe_layout, 0, &[self.desc_sets[f]], &[]);
      self.device.cmd_draw(cb, 3, 1, 0, 0);
      self.device.cmd_end_render_pass(cb);
      if let Err(e) = self.device.end_command_buffer(cb) {
        println!("ImageViewer: end_command_buffer failed: {e:?}");
        return;
      }

      let guard = self.api.lock_vulkan_queue();
      let wait_semaphores = [self.image_avail[f]];
      let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
      let signal_semaphores = [self.render_done[image_index as usize]];
      let cmd_buffers = [cb];
      let submit_info = vk::SubmitInfo::default()
        .wait_semaphores(&wait_semaphores)
        .wait_dst_stage_mask(&wait_stages)
        .command_buffers(&cmd_buffers)
        .signal_semaphores(&signal_semaphores);
      if let Err(e) = self.device.queue_submit(guard.queue(), &[submit_info], self.in_flight[f]) {
        println!("ImageViewer: queue_submit failed: {e:?}");
        return;
      }

      let swapchains = [self.swapchain];
      let image_indices = [image_index];
      let present_info =
        vk::PresentInfoKHR::default().wait_semaphores(&signal_semaphores).swapchains(&swapchains).image_indices(&image_indices);
      match self.swapchain_loader.queue_present(guard.queue(), &present_info) {
        Ok(suboptimal) => {
          if suboptimal {
            self.dirty = true;
          }
        }
        Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
        Err(e) => println!("ImageViewer: queue_present failed: {e:?}"),
      }
    }

    self.frame = (f + 1) % MAX_FRAMES;
  }

  fn apply_pending_events(&mut self) {
    let pending: Vec<ThalamusSDLEvent> = self.events.borrow_mut().drain(..).collect();
    for event in &pending {
      match event.event_type() {
        THALAMUS_SDL_EVENT_QUIT => {
          self.should_close = true;
        }
        THALAMUS_SDL_EVENT_WINDOW_CLOSE_REQUESTED if unsafe { event.as_window() }.window_id == self.window_id => {
          self.should_close = true;
        }
        THALAMUS_SDL_EVENT_WINDOW_RESIZED if unsafe { event.as_window() }.window_id == self.window_id => {
          self.dirty = true;
        }
        _ => {}
      }
    }
  }

  fn upload_texture(
    &mut self,
    slot: usize,
    cb: vk::CommandBuffer,
    plane: &[u8],
    w: u32,
    h: u32,
    src_channels: u32,
  ) -> Result<(), String> {
    let channels = if src_channels == 1 { 1 } else { 4 };
    if w != self.textures[slot].w || h != self.textures[slot].h || channels != self.textures[slot].channels {
      self.build_texture(slot, w, h, channels)?;
    }

    unsafe {
      let dst = self.textures[slot].stage_mapped;
      if src_channels == 1 {
        std::ptr::copy_nonoverlapping(plane.as_ptr(), dst as *mut u8, (w as usize) * (h as usize));
      } else {
        let pixel_count = (w as usize) * (h as usize);
        let dst = std::slice::from_raw_parts_mut(dst as *mut u8, pixel_count * 4);
        for i in 0..pixel_count {
          dst[4 * i] = plane[3 * i];
          dst[4 * i + 1] = plane[3 * i + 1];
          dst[4 * i + 2] = plane[3 * i + 2];
          dst[4 * i + 3] = 255;
        }
      }
    }

    let tex = &self.textures[slot];
    record_barrier(
      &self.device,
      cb,
      tex.image,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
      vk::ImageLayout::TRANSFER_DST_OPTIMAL,
      vk::AccessFlags::SHADER_READ,
      vk::AccessFlags::TRANSFER_WRITE,
      vk::PipelineStageFlags::FRAGMENT_SHADER,
      vk::PipelineStageFlags::TRANSFER,
    );

    let region = vk::BufferImageCopy::default()
      .image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
      .image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
    unsafe {
      self.device.cmd_copy_buffer_to_image(cb, tex.stage_buf, tex.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
    }

    record_barrier(
      &self.device,
      cb,
      tex.image,
      vk::ImageLayout::TRANSFER_DST_OPTIMAL,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
      vk::AccessFlags::TRANSFER_WRITE,
      vk::AccessFlags::SHADER_READ,
      vk::PipelineStageFlags::TRANSFER,
      vk::PipelineStageFlags::FRAGMENT_SHADER,
    );

    Ok(())
  }

  /// `channels` is the number of bytes per pixel to store in the texture
  /// itself (1 or 4) -- RGB source data is expanded to RGBA before upload
  /// since VK_FORMAT_R8G8B8_UNORM sampling support isn't guaranteed.
  fn build_texture(&mut self, slot: usize, w: u32, h: u32, channels: u32) -> Result<(), String> {
    self.textures[slot].destroy(&self.device);

    let size = (w as vk::DeviceSize) * (h as vk::DeviceSize) * (channels as vk::DeviceSize);
    let format = if channels == 1 { vk::Format::R8_UNORM } else { vk::Format::R8G8B8A8_UNORM };

    let (stage_buf, stage_mem) = make_buffer(
      &self.device,
      &self.instance,
      self.physical_device,
      size,
      vk::BufferUsageFlags::TRANSFER_SRC,
      vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    let stage_mapped =
      unsafe { self.device.map_memory(stage_mem, 0, size, vk::MemoryMapFlags::empty()) }.map_err(|e| format!("{e:?}"))?;

    let image = unsafe {
      self.device.create_image(
        &vk::ImageCreateInfo::default()
          .image_type(vk::ImageType::TYPE_2D)
          .format(format)
          .extent(vk::Extent3D { width: w, height: h, depth: 1 })
          .mip_levels(1)
          .array_layers(1)
          .samples(vk::SampleCountFlags::TYPE_1)
          .tiling(vk::ImageTiling::OPTIMAL)
          .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
          .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let req = unsafe { self.device.get_image_memory_requirements(image) };
    let memory = unsafe {
      self.device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
          .allocation_size(req.size)
          .memory_type_index(find_mem_type(&self.instance, self.physical_device, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;
    unsafe { self.device.bind_image_memory(image, memory, 0) }.map_err(|e| format!("{e:?}"))?;

    // transition_layout takes the queue lock itself -- don't take it here too
    // (the lock isn't reentrant).
    self.transition_layout(image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)?;

    let components = if channels == 1 {
      vk::ComponentMapping { r: vk::ComponentSwizzle::R, g: vk::ComponentSwizzle::R, b: vk::ComponentSwizzle::R, a: vk::ComponentSwizzle::ONE }
    } else {
      vk::ComponentMapping::default()
    };
    let view = unsafe {
      self.device.create_image_view(
        &vk::ImageViewCreateInfo::default()
          .image(image)
          .view_type(vk::ImageViewType::TYPE_2D)
          .format(format)
          .components(components)
          .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 }),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let image_info = [vk::DescriptorImageInfo::default().sampler(self.sampler).image_view(view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let write = [vk::WriteDescriptorSet::default()
      .dst_set(self.desc_sets[slot])
      .dst_binding(0)
      .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
      .image_info(&image_info)];
    unsafe { self.device.update_descriptor_sets(&write, &[]) };

    self.textures[slot] = Texture { w, h, channels, image, memory, view, stage_buf, stage_mem, stage_mapped };
    Ok(())
  }

  fn transition_layout(&self, image: vk::Image, from: vk::ImageLayout, to: vk::ImageLayout) -> Result<(), String> {
    unsafe {
      let cb = self
        .device
        .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(self.cmd_pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1))
        .map_err(|e| format!("{e:?}"))?[0];
      self.device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).map_err(|e| format!("{e:?}"))?;

      let (src_access, dst_access, src_stage, dst_stage) = if from == vk::ImageLayout::UNDEFINED {
        (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER)
      } else {
        (vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER)
      };
      record_barrier(&self.device, cb, image, from, to, src_access, dst_access, src_stage, dst_stage);

      self.device.end_command_buffer(cb).map_err(|e| format!("{e:?}"))?;
      let guard = self.api.lock_vulkan_queue();
      let cmd_buffers = [cb];
      let submit = vk::SubmitInfo::default().command_buffers(&cmd_buffers);
      self.device.queue_submit(guard.queue(), &[submit], vk::Fence::null()).map_err(|e| format!("{e:?}"))?;
      self.device.queue_wait_idle(guard.queue()).map_err(|e| format!("{e:?}"))?;
      self.device.free_command_buffers(self.cmd_pool, &[cb]);
    }
    Ok(())
  }

  fn recreate_swapchain(&mut self) -> Result<(), String> {
    unsafe {
      self.device.device_wait_idle().map_err(|e| format!("{e:?}"))?;
      self.destroy_swapchain_deps();

      let caps = self
        .surface_loader
        .get_physical_device_surface_capabilities(self.physical_device, self.surface)
        .map_err(|e| format!("{e:?}"))?;

      let (pw, ph) = self.window.size_in_pixels();
      self.extent = if caps.current_extent.width != u32::MAX {
        caps.current_extent
      } else {
        vk::Extent2D {
          width: (pw as u32).clamp(caps.min_image_extent.width, caps.max_image_extent.width.max(1)),
          height: (ph as u32).clamp(caps.min_image_extent.height, caps.max_image_extent.height.max(1)),
        }
      };
      if self.extent.width == 0 || self.extent.height == 0 {
        self.dirty = true;
        return Ok(());
      }

      let mut image_count = caps.min_image_count + 1;
      if caps.max_image_count > 0 {
        image_count = image_count.min(caps.max_image_count);
      }

      let old_swapchain = self.swapchain;
      let create_info = vk::SwapchainCreateInfoKHR::default()
        .surface(self.surface)
        .min_image_count(image_count)
        .image_format(self.surface_format.format)
        .image_color_space(self.surface_format.color_space)
        .image_extent(self.extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
        .present_mode(vk::PresentModeKHR::FIFO)
        .clipped(true)
        .old_swapchain(old_swapchain);

      self.swapchain = self.swapchain_loader.create_swapchain(&create_info, None).map_err(|e| format!("{e:?}"))?;
      if old_swapchain != vk::SwapchainKHR::null() {
        self.swapchain_loader.destroy_swapchain(old_swapchain, None);
      }

      let images = self.swapchain_loader.get_swapchain_images(self.swapchain).map_err(|e| format!("{e:?}"))?;
      self.image_views = images
        .iter()
        .map(|&image| {
          self.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
              .image(image)
              .view_type(vk::ImageViewType::TYPE_2D)
              .format(self.surface_format.format)
              .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 }),
            None,
          )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.framebuffers = self
        .image_views
        .iter()
        .map(|&view| {
          let attachments = [view];
          self.device.create_framebuffer(
            &vk::FramebufferCreateInfo::default().render_pass(self.render_pass).attachments(&attachments).width(self.extent.width).height(self.extent.height).layers(1),
            None,
          )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.render_done = (0..images.len())
        .map(|_| self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.dirty = false;
      Ok(())
    }
  }

  unsafe fn destroy_swapchain_deps(&mut self) {
    unsafe {
      for &fb in &self.framebuffers {
        self.device.destroy_framebuffer(fb, None);
      }
      self.framebuffers.clear();
      for &view in &self.image_views {
        self.device.destroy_image_view(view, None);
      }
      self.image_views.clear();
      for &s in &self.render_done {
        self.device.destroy_semaphore(s, None);
      }
      self.render_done.clear();
    }
  }

  unsafe fn create_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass, vk::Result> {
    let attachment = vk::AttachmentDescription::default()
      .format(format)
      .samples(vk::SampleCountFlags::TYPE_1)
      .load_op(vk::AttachmentLoadOp::CLEAR)
      .store_op(vk::AttachmentStoreOp::STORE)
      .initial_layout(vk::ImageLayout::UNDEFINED)
      .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
    let attachments = [attachment];
    let color_ref = [vk::AttachmentReference::default().attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color_ref)];
    let dependency = [vk::SubpassDependency::default()
      .src_subpass(vk::SUBPASS_EXTERNAL)
      .dst_subpass(0)
      .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
      .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
      .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    unsafe { device.create_render_pass(&vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpass).dependencies(&dependency), None) }
  }

  unsafe fn create_pipeline(
    device: &ash::Device,
    desc_layout: vk::DescriptorSetLayout,
    render_pass: vk::RenderPass,
  ) -> Result<(vk::PipelineLayout, vk::Pipeline), vk::Result> {
    let vert_words = spirv_words(VERT_SPV);
    let frag_words = spirv_words(FRAG_SPV);
    let vert_module = unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert_words), None) }?;
    let frag_module = unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag_words), None) }?;

    let entry_point = c"main";
    let stages = [
      vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vert_module).name(entry_point),
      vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(frag_module).name(entry_point),
    ];

    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
      .polygon_mode(vk::PolygonMode::FILL)
      .cull_mode(vk::CullModeFlags::NONE)
      .front_face(vk::FrontFace::CLOCKWISE)
      .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);

    let set_layouts = [desc_layout];
    let pipe_layout = unsafe { device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts), None) }?;

    let create_info = [vk::GraphicsPipelineCreateInfo::default()
      .stages(&stages)
      .vertex_input_state(&vertex_input)
      .input_assembly_state(&input_assembly)
      .viewport_state(&viewport_state)
      .rasterization_state(&rasterization)
      .multisample_state(&multisample)
      .color_blend_state(&color_blend)
      .dynamic_state(&dynamic_state)
      .layout(pipe_layout)
      .render_pass(render_pass)];
    let pipeline = unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &create_info, None) }
      .map_err(|(_, e)| e)?[0];

    unsafe {
      device.destroy_shader_module(vert_module, None);
      device.destroy_shader_module(frag_module, None);
    }

    Ok((pipe_layout, pipeline))
  }
}

impl Drop for ImageViewer {
  fn drop(&mut self) {
    unsafe {
      let _ = self.device.device_wait_idle();
      for tex in &mut self.textures {
        tex.destroy(&self.device);
      }
      for &s in &self.image_avail {
        self.device.destroy_semaphore(s, None);
      }
      for &f in &self.in_flight {
        self.device.destroy_fence(f, None);
      }
      self.destroy_swapchain_deps();
      if self.swapchain != vk::SwapchainKHR::null() {
        self.swapchain_loader.destroy_swapchain(self.swapchain, None);
      }
      self.device.destroy_pipeline(self.pipeline, None);
      self.device.destroy_pipeline_layout(self.pipe_layout, None);
      self.device.destroy_descriptor_pool(self.desc_pool, None);
      self.device.destroy_descriptor_set_layout(self.desc_layout, None);
      self.device.destroy_sampler(self.sampler, None);
      self.device.destroy_render_pass(self.render_pass, None);
      self.device.destroy_command_pool(self.cmd_pool, None);
      self.surface_loader.destroy_surface(self.surface, None);
      // `window` destroys itself via SDLWindow's Drop.
      // instance/device themselves are Thalamus's -- never destroyed here.
    }
  }
}
