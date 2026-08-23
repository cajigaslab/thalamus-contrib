//! Nests a live camera preview inside an imgui UI (window chrome, a rotation
//! slider) on top of `ImguiWindow`. The camera frame is uploaded into a
//! Vulkan texture and displayed via `imgui`'s custom-texture support; imgui
//! has no built-in way to rotate an image, so rotation is done by drawing a
//! rotated quad (`DrawListMut::add_image_quad`) instead of the plain
//! `ui.image()` widget -- the standard Dear ImGui trick for this.

use ash::vk;

use crate::api::{ImageFormat, ThalamusAPI};
use crate::imgui_window::{ImguiWindow, MAX_FRAMES_IN_FLIGHT};

/// Number of tightly-packed bytes per pixel for the formats this viewer
/// knows how to display, or `None` for formats it doesn't (yet) support
/// (the YUV variants).
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

/// One frame-in-flight slot's worth of sampled texture data. Doubled up
/// (one per `MAX_FRAMES_IN_FLIGHT` slot) so uploading a new frame never
/// stomps on a texture the GPU might still be reading from a previous
/// submit -- see `ImguiWindow::render_frame`'s doc comment.
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

/// Locks the shared Vulkan queue for a one-shot command buffer, submits it,
/// and waits for it to finish -- used for the layout transition a texture
/// needs right after creation.
fn transition_layout(
  api: ThalamusAPI,
  device: &ash::Device,
  cmd_pool: vk::CommandPool,
  image: vk::Image,
  from: vk::ImageLayout,
  to: vk::ImageLayout,
) -> Result<(), String> {
  unsafe {
    let cb = device
      .allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(cmd_pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1))
      .map_err(|e| format!("{e:?}"))?[0];
    device
      .begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
      .map_err(|e| format!("{e:?}"))?;

    let (src_access, dst_access, src_stage, dst_stage) = if from == vk::ImageLayout::UNDEFINED {
      (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER)
    } else {
      (vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER)
    };
    record_barrier(device, cb, image, from, to, src_access, dst_access, src_stage, dst_stage);

    device.end_command_buffer(cb).map_err(|e| format!("{e:?}"))?;
    let guard = api.lock_vulkan_queue();
    let cmd_buffers = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cmd_buffers);
    device.queue_submit(guard.queue(), &[submit], vk::Fence::null()).map_err(|e| format!("{e:?}"))?;
    device.queue_wait_idle(guard.queue()).map_err(|e| format!("{e:?}"))?;
    device.free_command_buffers(cmd_pool, &[cb]);
  }
  Ok(())
}

/// (Re)builds `tex` at the given size/format. `channels` is the number of
/// bytes per pixel to store in the texture itself (1 or 4) -- RGB source
/// data is expanded to RGBA before upload since VK_FORMAT_R8G8B8_UNORM
/// sampling support isn't guaranteed.
fn build_texture(
  api: ThalamusAPI,
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
  cmd_pool: vk::CommandPool,
  tex: &mut Texture,
  w: u32,
  h: u32,
  channels: u32,
) -> Result<(), String> {
  tex.destroy(device);

  let size = (w as vk::DeviceSize) * (h as vk::DeviceSize) * (channels as vk::DeviceSize);
  let format = if channels == 1 { vk::Format::R8_UNORM } else { vk::Format::R8G8B8A8_UNORM };

  let (stage_buf, stage_mem) = make_buffer(
    device,
    instance,
    physical_device,
    size,
    vk::BufferUsageFlags::TRANSFER_SRC,
    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
  )?;
  let stage_mapped = unsafe { device.map_memory(stage_mem, 0, size, vk::MemoryMapFlags::empty()) }.map_err(|e| format!("{e:?}"))?;

  let image = unsafe {
    device.create_image(
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

  let req = unsafe { device.get_image_memory_requirements(image) };
  let memory = unsafe {
    device.allocate_memory(
      &vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(find_mem_type(instance, physical_device, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)),
      None,
    )
  }
  .map_err(|e| format!("{e:?}"))?;
  unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|e| format!("{e:?}"))?;

  transition_layout(api, device, cmd_pool, image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)?;

  let components = if channels == 1 {
    vk::ComponentMapping { r: vk::ComponentSwizzle::R, g: vk::ComponentSwizzle::R, b: vk::ComponentSwizzle::R, a: vk::ComponentSwizzle::ONE }
  } else {
    vk::ComponentMapping::default()
  };
  let view = unsafe {
    device.create_image_view(
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

  *tex = Texture { w, h, channels, image, memory, view, stage_buf, stage_mem, stage_mapped };
  Ok(())
}

/// Uploads `plane` into `tex`, rebuilding it first if its size/format
/// changed. Records the copy (and the layout-transition barriers around it)
/// into `cb`. Returns `true` if the texture was rebuilt, meaning its
/// `vk::ImageView` handle changed and any descriptor set referencing it
/// needs to be rebound.
fn upload_texture(
  api: ThalamusAPI,
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
  cmd_pool: vk::CommandPool,
  tex: &mut Texture,
  cb: vk::CommandBuffer,
  plane: &[u8],
  w: u32,
  h: u32,
  src_channels: u32,
) -> Result<bool, String> {
  let channels = if src_channels == 1 { 1 } else { 4 };
  let rebuilt = w != tex.w || h != tex.h || channels != tex.channels;
  if rebuilt {
    build_texture(api, device, instance, physical_device, cmd_pool, tex, w, h, channels)?;
  }

  unsafe {
    let dst = tex.stage_mapped;
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

  record_barrier(
    device,
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
    device.cmd_copy_buffer_to_image(cb, tex.stage_buf, tex.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
  }

  record_barrier(
    device,
    cb,
    tex.image,
    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
    vk::AccessFlags::TRANSFER_WRITE,
    vk::AccessFlags::SHADER_READ,
    vk::PipelineStageFlags::TRANSFER,
    vk::PipelineStageFlags::FRAGMENT_SHADER,
  );

  Ok(rebuilt)
}

pub struct ImageViewer {
  window: ImguiWindow,
  api: ThalamusAPI,
  instance: ash::Instance,
  physical_device: vk::PhysicalDevice,
  cmd_pool: vk::CommandPool,
  sampler: vk::Sampler,
  textures: [Texture; MAX_FRAMES_IN_FLIGHT],
  texture_ids: [imgui::TextureId; MAX_FRAMES_IN_FLIGHT],
  descriptor_sets: [vk::DescriptorSet; MAX_FRAMES_IN_FLIGHT],
  rotation_degrees: f32,
}

impl ImageViewer {
  pub fn new(api: ThalamusAPI, title: &str, x: i32, y: i32, width: i32, height: i32) -> Result<Self, String> {
    let mut window = ImguiWindow::new(api, title, x, y, width, height)?;
    let instance = window.instance().clone();
    let physical_device = window.physical_device();
    let cmd_pool = window.cmd_pool();
    let device = window.device().clone();

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

    // Placeholder 1x1 textures so a real descriptor set (and TextureId)
    // exists from the start; update() rebuilds them at the real size on the
    // first frame that arrives.
    let mut textures: [Texture; MAX_FRAMES_IN_FLIGHT] = Default::default();
    let mut texture_ids = [imgui::TextureId::from(usize::MAX); MAX_FRAMES_IN_FLIGHT];
    let mut descriptor_sets = [vk::DescriptorSet::null(); MAX_FRAMES_IN_FLIGHT];
    for i in 0..MAX_FRAMES_IN_FLIGHT {
      build_texture(api, &device, &instance, physical_device, cmd_pool, &mut textures[i], 1, 1, 1)?;
      let (id, set) = window.register_texture(textures[i].view, sampler)?;
      texture_ids[i] = id;
      descriptor_sets[i] = set;
    }

    Ok(ImageViewer {
      window,
      api,
      instance,
      physical_device,
      cmd_pool,
      sampler,
      textures,
      texture_ids,
      descriptor_sets,
      rotation_degrees: 0.0,
    })
  }

  pub fn should_close(&self) -> bool {
    self.window.should_close()
  }

  /// Current window position and size (`(x, y, w, h)`).
  pub fn position_size(&self) -> (i32, i32, i32, i32) {
    self.window.position_size()
  }

  /// Uploads `frame` (if given, and in a supported format) and renders one
  /// tick of the UI: a window containing a 0-360 degree rotation slider and
  /// the image drawn at that rotation. Safe to call with `frame: None` when
  /// no new data has arrived since the last call -- the previous frame's
  /// texture simply stays on screen. Call periodically (e.g. from a
  /// repeating timer) on the thread that created this viewer.
  pub fn update(&mut self, frame: Option<ImageFrame>) {
    let api = self.api;
    let instance = &self.instance;
    let physical_device = self.physical_device;
    let cmd_pool = self.cmd_pool;
    let sampler = self.sampler;
    let textures = &mut self.textures;
    let descriptor_sets = &self.descriptor_sets;
    let texture_ids = &self.texture_ids;
    let rotation_degrees = &mut self.rotation_degrees;

    let result = self.window.render_frame(
      // Returns the texture's current (w, h) rather than leaving `build_ui`
      // to read `textures` itself: both closures are constructed together as
      // arguments to this call, so if `build_ui` also captured `textures`
      // (even just to read it) the borrow checker would see that alongside
      // this closure's `&mut textures[..]` as a live conflict, despite the
      // two closures never actually running at the same time.
      |device, cmd, frame_idx| -> (u32, u32) {
        let Some(frame) = &frame else {
          let tex = &textures[frame_idx];
          return (tex.w, tex.h);
        };
        let Some(src_channels) = channels_for_format(frame.format) else {
          let tex = &textures[frame_idx];
          return (tex.w, tex.h);
        };
        let tex = &mut textures[frame_idx];
        match upload_texture(api, device, instance, physical_device, cmd_pool, tex, cmd, frame.data, frame.width, frame.height, src_channels) {
          Ok(true) => {
            let image_info = [vk::DescriptorImageInfo::default().sampler(sampler).image_view(tex.view).image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = [vk::WriteDescriptorSet::default()
              .dst_set(descriptor_sets[frame_idx])
              .dst_binding(0)
              .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
              .image_info(&image_info)];
            unsafe { device.update_descriptor_sets(&write, &[]) };
          }
          Ok(false) => {}
          Err(e) => println!("ImageViewer: upload_texture failed: {e}"),
        }
        (tex.w, tex.h)
      },
      |ui, frame_idx, (tex_w, tex_h)| {
        // Thalamus gives each imgui-hosted node its own OS window, so this
        // window IS that window's content area -- always Always-positioned
        // and Always-sized to the full display, with no title bar/border of
        // its own, rather than a movable/resizable panel floating inside it.
        let display_size = ui.io().display_size;
        ui.window("Thorcam")
          .position([0.0, 0.0], imgui::Condition::Always)
          .size(display_size, imgui::Condition::Always)
          .no_decoration()
          .build(|| {
            ui.slider("Rotation", 0.0f32, 360.0f32, rotation_degrees);

            let avail = ui.content_region_avail();
            if avail[0] <= 1.0 || avail[1] <= 1.0 {
              return;
            }
            let img_aspect = (tex_w.max(1) as f32) / (tex_h.max(1) as f32);
            let avail_aspect = avail[0] / avail[1];
            let (dw, dh) = if avail_aspect > img_aspect { (avail[1] * img_aspect, avail[1]) } else { (avail[0], avail[0] / img_aspect) };

            let origin = ui.cursor_screen_pos();
            let (cx, cy) = (origin[0] + dw / 2.0, origin[1] + dh / 2.0);
            let (sin, cos) = rotation_degrees.to_radians().sin_cos();
            let rotate = |lx: f32, ly: f32| [cx + lx * cos - ly * sin, cy + lx * sin + ly * cos];
            let (hw, hh) = (dw / 2.0, dh / 2.0);

            // Clipped to the image's unrotated footprint so corners that
            // swing outside it at non-90-degree angles get cut off there,
            // rather than spilling over the rest of the window. Only one
            // DrawListMut may be live at a time (imgui-rs panics on a
            // second concurrent call to get_window_draw_list()), so this is
            // fetched once and reused for both the clip and the draw call.
            let clip_max = [origin[0] + dw, origin[1] + dh];
            let draw_list = ui.get_window_draw_list();
            draw_list.with_clip_rect_intersect(origin, clip_max, || {
              draw_list
                .add_image_quad(texture_ids[frame_idx], rotate(-hw, -hh), rotate(hw, -hh), rotate(hw, hh), rotate(-hw, hh))
                .build();
            });

            // Reserves the image's on-screen footprint so the window's
            // content size / scrollbars account for it.
            ui.dummy(avail);
          });
      },
    );
    if let Err(e) = result {
      println!("ImageViewer: render_frame failed: {e}");
    }
  }
}

impl Drop for ImageViewer {
  fn drop(&mut self) {
    let device = self.window.device();
    unsafe {
      let _ = device.device_wait_idle();
      for tex in &mut self.textures {
        tex.destroy(device);
      }
      device.destroy_sampler(self.sampler, None);
    }
    // `window` (and hence its custom-texture descriptor pool, which owns
    // `self.descriptor_sets`) destroys itself via ImguiWindow's own Drop.
  }
}
