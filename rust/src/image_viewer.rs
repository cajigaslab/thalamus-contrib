//! Nests a live camera preview inside an imgui UI (window chrome, a rotation
//! slider) on top of `ImguiWindow`. The camera frame is uploaded into a
//! Vulkan texture and displayed via `imgui`'s custom-texture support; imgui
//! has no built-in way to rotate an image, so rotation is done by drawing a
//! rotated quad (`DrawListMut::add_image_quad`) instead of the plain
//! `ui.image()` widget -- the standard Dear ImGui trick for this.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ash::vk;

use crate::api::{
  ImageData, ImageFormat, MainThreadOnly, MainThreadToken, State, StateKey, StateValue,
  TaskScope, ThalamusAPI, run_task,
};
use crate::imgui_window::{ImguiWindow, MAX_FRAMES_IN_FLIGHT};

/// Number of tightly-packed bytes per pixel for the formats this viewer
/// knows how to display, or `None` for formats it doesn't (yet) support
/// (the YUV variants).
fn channels_for_format(format: ImageFormat) -> Option<u32> {
  match format {
    ImageFormat::Gray => Some(1),
    ImageFormat::RGB | ImageFormat::BGR => Some(3),
    ImageFormat::YUYV422
    | ImageFormat::YUV420P
    | ImageFormat::YUVJ420P
    | ImageFormat::NV12
    | ImageFormat::MJPEG
    | ImageFormat::MPEG1
    | ImageFormat::MPEG4 => None,
  }
}

fn find_mem_type(
  instance: &ash::Instance,
  phys: vk::PhysicalDevice,
  type_bits: u32,
  props: vk::MemoryPropertyFlags,
) -> u32 {
  let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };
  for i in 0..mem_props.memory_type_count {
    if (type_bits & (1 << i)) != 0
      && mem_props.memory_types[i as usize]
        .property_flags
        .contains(props)
    {
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
      .create_buffer(
        &vk::BufferCreateInfo::default()
          .size(size)
          .usage(usage)
          .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None,
      )
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
    device
      .bind_buffer_memory(buf, mem, 0)
      .map_err(|e| format!("{e:?}"))?;
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
    device.cmd_pipeline_barrier(
      cb,
      src_stage,
      dst_stage,
      vk::DependencyFlags::empty(),
      &[],
      &[],
      &[barrier],
    );
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
      .allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
          .command_pool(cmd_pool)
          .level(vk::CommandBufferLevel::PRIMARY)
          .command_buffer_count(1),
      )
      .map_err(|e| format!("{e:?}"))?[0];
    device
      .begin_command_buffer(
        cb,
        &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
      )
      .map_err(|e| format!("{e:?}"))?;

    let (src_access, dst_access, src_stage, dst_stage) = if from == vk::ImageLayout::UNDEFINED {
      (
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_WRITE,
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::TRANSFER,
      )
    } else {
      (
        vk::AccessFlags::TRANSFER_WRITE,
        vk::AccessFlags::SHADER_READ,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
      )
    };
    record_barrier(
      device, cb, image, from, to, src_access, dst_access, src_stage, dst_stage,
    );

    device
      .end_command_buffer(cb)
      .map_err(|e| format!("{e:?}"))?;
    let guard = api.lock_vulkan_queue();
    let cmd_buffers = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cmd_buffers);
    device
      .queue_submit(guard.queue(), &[submit], vk::Fence::null())
      .map_err(|e| format!("{e:?}"))?;
    device
      .queue_wait_idle(guard.queue())
      .map_err(|e| format!("{e:?}"))?;
    device.free_command_buffers(cmd_pool, &[cb]);
  }
  Ok(())
}

/// (Re)builds `tex` at the given size/format, leaving its image in
/// `UNDEFINED` layout: the caller must transition it (as `build_texture` does,
/// or with a barrier in its own command buffer) before sampling it.
/// `channels` is the number of bytes per pixel to store in the texture itself
/// (1 or 4) -- RGB source data is expanded to RGBA before upload since
/// VK_FORMAT_R8G8B8_UNORM sampling support isn't guaranteed.
fn create_texture(
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
  tex: &mut Texture,
  w: u32,
  h: u32,
  channels: u32,
) -> Result<(), String> {
  tex.destroy(device);

  let size = (w as vk::DeviceSize) * (h as vk::DeviceSize) * (channels as vk::DeviceSize);
  let format = if channels == 1 {
    vk::Format::R8_UNORM
  } else {
    vk::Format::R8G8B8A8_UNORM
  };

  let (stage_buf, stage_mem) = make_buffer(
    device,
    instance,
    physical_device,
    size,
    vk::BufferUsageFlags::TRANSFER_SRC,
    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
  )?;
  let stage_mapped = unsafe { device.map_memory(stage_mem, 0, size, vk::MemoryMapFlags::empty()) }
    .map_err(|e| format!("{e:?}"))?;

  let image = unsafe {
    device.create_image(
      &vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
          width: w,
          height: h,
          depth: 1,
        })
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
        .memory_type_index(find_mem_type(
          instance,
          physical_device,
          req.memory_type_bits,
          vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )),
      None,
    )
  }
  .map_err(|e| format!("{e:?}"))?;
  unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|e| format!("{e:?}"))?;

  let components = if channels == 1 {
    vk::ComponentMapping {
      r: vk::ComponentSwizzle::R,
      g: vk::ComponentSwizzle::R,
      b: vk::ComponentSwizzle::R,
      a: vk::ComponentSwizzle::ONE,
    }
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
        .subresource_range(vk::ImageSubresourceRange {
          aspect_mask: vk::ImageAspectFlags::COLOR,
          base_mip_level: 0,
          level_count: 1,
          base_array_layer: 0,
          layer_count: 1,
        }),
      None,
    )
  }
  .map_err(|e| format!("{e:?}"))?;

  *tex = Texture {
    w,
    h,
    channels,
    image,
    memory,
    view,
    stage_buf,
    stage_mem,
    stage_mapped,
  };
  Ok(())
}

/// Like `create_texture`, but also transitions the image to
/// `SHADER_READ_ONLY_OPTIMAL` so it can be sampled right away. Blocks until
/// the transition has run on the GPU.
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
  create_texture(device, instance, physical_device, tex, w, h, channels)?;
  transition_layout(
    api,
    device,
    cmd_pool,
    tex.image,
    vk::ImageLayout::UNDEFINED,
    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
  )
}

/// Uploads `plane` into `tex`, rebuilding it first if its size/format
/// changed. Records the copy (and the layout-transition barriers around it)
/// into `cb`; doesn't submit or wait for anything. Returns `true` if the
/// texture was rebuilt, meaning its `vk::ImageView` handle changed and any
/// descriptor set referencing it needs to be rebound.
fn upload_texture(
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
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
    create_texture(device, instance, physical_device, tex, w, h, channels)?;
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

  // A freshly built image has no contents or earlier readers to wait for.
  // Otherwise earlier frames on this queue may still be sampling it, and this
  // barrier orders the copy after them.
  if rebuilt {
    record_barrier(
      device,
      cb,
      tex.image,
      vk::ImageLayout::UNDEFINED,
      vk::ImageLayout::TRANSFER_DST_OPTIMAL,
      vk::AccessFlags::empty(),
      vk::AccessFlags::TRANSFER_WRITE,
      vk::PipelineStageFlags::TOP_OF_PIPE,
      vk::PipelineStageFlags::TRANSFER,
    );
  } else {
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
  }

  let region = vk::BufferImageCopy::default()
    .image_subresource(vk::ImageSubresourceLayers {
      aspect_mask: vk::ImageAspectFlags::COLOR,
      mip_level: 0,
      base_array_layer: 0,
      layer_count: 1,
    })
    .image_extent(vk::Extent3D {
      width: w,
      height: h,
      depth: 1,
    });
  unsafe {
    device.cmd_copy_buffer_to_image(
      cb,
      tex.stage_buf,
      tex.image,
      vk::ImageLayout::TRANSFER_DST_OPTIMAL,
      &[region],
    );
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

/// How often the main thread redraws the viewer.
const REFRESH_INTERVAL: Duration = Duration::from_millis(33);

/// Textures uploads rotate through: the newest one, plus one per frame in
/// flight that may still be sampling an older one, plus one that is then
/// always free for the next upload.
const TEXTURE_COUNT: usize = MAX_FRAMES_IN_FLIGHT + 2;

struct PoolTexture {
  texture: Texture,
  id: imgui::TextureId,
  descriptor_set: vk::DescriptorSet,
  /// Number of the last frame that sampled this texture (see
  /// TexturePool::frames_completed); it can't be rewritten until that frame
  /// has finished.
  read_frame: Option<u64>,
  /// Records this texture's uploads. Per texture, so an upload never has to
  /// wait for another texture's upload to finish before reusing it.
  cmd: vk::CommandBuffer,
  /// Signals when this texture's last upload has finished on the GPU (its
  /// staging buffer and `cmd` are reusable). Created signaled.
  upload_fence: vk::Fence,
}

/// The viewer's textures, shared between the render loop (main thread) and
/// ImageSink::update (any thread). Only ever accessed through ImageSink's
/// mutex.
struct TexturePool {
  api: ThalamusAPI,
  device: ash::Device,
  instance: ash::Instance,
  physical_device: vk::PhysicalDevice,
  // Separate from the window's pool: command pools can't be shared between
  // threads, and uploads run on the caller's thread.
  cmd_pool: vk::CommandPool,
  sampler: vk::Sampler,
  textures: Vec<PoolTexture>,
  newest: Option<usize>,
  /// When the render loop will next draw; uploads that would be replaced
  /// before then are skipped.
  next_render: Instant,
  /// Frames the render loop has started recording, numbered from 1.
  frames_started: u64,
  /// Every frame up to this number has finished on the GPU. Tracked by the
  /// render loop rather than by checking its in-flight fences from the
  /// uploading thread: the render loop resets and submits those fences, and
  /// Vulkan requires that to be externally synchronized with any other use.
  frames_completed: u64,
  /// The frame each of the window's frame-in-flight slots last carried.
  slot_frames: [Option<u64>; MAX_FRAMES_IN_FLIGHT],
}

// SAFETY: every field is a Vulkan handle or loader, or a ThalamusAPI used only
// for lock_vulkan_queue (a mutex in Thalamus, also taken from non-main threads
// by the C++ ImageViewer). None of it is tied to the thread that created it,
// and ImageSink's mutex serializes all access.
unsafe impl Send for TexturePool {}

impl TexturePool {
  fn new(api: ThalamusAPI, window: &mut ImguiWindow) -> Result<Self, String> {
    let device = window.device().clone();
    let instance = window.instance().clone();
    let physical_device = window.physical_device();

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

    let cmd_pool = api.create_vulkan_command_pool();
    let cmds = unsafe {
      device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
          .command_pool(cmd_pool)
          .level(vk::CommandBufferLevel::PRIMARY)
          .command_buffer_count(TEXTURE_COUNT as u32),
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    // Placeholder 1x1 textures so real descriptor sets (and TextureIds) exist
    // from the start; uploads rebuild them at the real size.
    let mut textures = Vec::with_capacity(TEXTURE_COUNT);
    for cmd in cmds {
      let mut texture = Texture::default();
      build_texture(
        api,
        &device,
        &instance,
        physical_device,
        cmd_pool,
        &mut texture,
        1,
        1,
        1,
      )?;
      let (id, descriptor_set) = window.register_texture(texture.view, sampler)?;
      let upload_fence = unsafe {
        device.create_fence(
          &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
          None,
        )
      }
      .map_err(|e| format!("{e:?}"))?;
      textures.push(PoolTexture {
        texture,
        id,
        descriptor_set,
        read_frame: None,
        cmd,
        upload_fence,
      });
    }

    Ok(TexturePool {
      api,
      device,
      instance,
      physical_device,
      cmd_pool,
      sampler,
      textures,
      newest: None,
      next_render: Instant::now(),
      frames_started: 0,
      frames_completed: 0,
      slot_frames: [None; MAX_FRAMES_IN_FLIGHT],
    })
  }

  /// Non-blocking check of whether `fence` has signaled. Only for fences
  /// this pool owns (the upload fences), which are only ever used under
  /// ImageSink's mutex.
  fn signaled(&self, fence: vk::Fence) -> bool {
    unsafe { self.device.get_fence_status(fence) }.unwrap_or(false)
  }

  /// A texture that isn't the newest, that no unfinished frame sampled, and
  /// whose last upload has finished.
  fn free_texture(&self) -> Option<usize> {
    (0..self.textures.len()).find(|&i| {
      let tex = &self.textures[i];
      Some(i) != self.newest
        && tex.read_frame.is_none_or(|frame| frame <= self.frames_completed)
        && self.signaled(tex.upload_fence)
    })
  }

  /// Called by the render loop when it starts recording frame-in-flight slot
  /// `slot`, right after waiting on that slot's fence. Returns the new frame's
  /// number.
  fn start_frame(&mut self, slot: usize) -> u64 {
    // The frame this slot carried before has finished (its fence was just
    // waited on), and the queue runs frames in order, so so has every frame
    // before it.
    if let Some(finished) = self.slot_frames[slot] {
      self.frames_completed = self.frames_completed.max(finished);
    }
    self.frames_started += 1;
    self.slot_frames[slot] = Some(self.frames_started);
    self.frames_started
  }

  /// Copies `image` into a free texture and submits the GPU copy, all on the
  /// calling thread, then makes it the newest texture. Never waits on a
  /// fence: if no texture is free the frame is dropped. Frames drawn later
  /// see the finished copy because they're submitted to the same queue after
  /// it, and `upload_texture`'s barriers order them.
  fn upload(&mut self, image: &dyn ImageData) -> Result<(), String> {
    let Some(src_channels) = channels_for_format(image.format()) else {
      return Ok(());
    };
    let (w, h) = (image.width() as u32, image.height() as u32);
    let plane = image.plane(0);
    let needed = (w as usize) * (h as usize) * (src_channels as usize);
    if w == 0 || h == 0 || plane.len() < needed {
      return Ok(());
    }
    let Some(index) = self.free_texture() else {
      return Ok(());
    };

    let device = &self.device;
    let tex = &mut self.textures[index];
    unsafe {
      device
        .reset_command_buffer(tex.cmd, vk::CommandBufferResetFlags::empty())
        .map_err(|e| format!("{e:?}"))?;
      device
        .begin_command_buffer(
          tex.cmd,
          &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .map_err(|e| format!("{e:?}"))?;
    }
    let rebuilt = upload_texture(
      device,
      &self.instance,
      self.physical_device,
      &mut tex.texture,
      tex.cmd,
      plane,
      w,
      h,
      src_channels,
    )?;

    if rebuilt {
      // The texture has a new image view; point its descriptor at it. Safe
      // here: it's free, so no frame is using the descriptor set.
      let image_info = [vk::DescriptorImageInfo::default()
        .sampler(self.sampler)
        .image_view(tex.texture.view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
      let write = [vk::WriteDescriptorSet::default()
        .dst_set(tex.descriptor_set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(&image_info)];
      unsafe { device.update_descriptor_sets(&write, &[]) };
    }

    unsafe {
      device
        .end_command_buffer(tex.cmd)
        .map_err(|e| format!("{e:?}"))?;
      // Signaled (checked by free_texture), so resetting it can't block.
      device
        .reset_fences(&[tex.upload_fence])
        .map_err(|e| format!("{e:?}"))?;
      let guard = self.api.lock_vulkan_queue();
      let cmd_buffers = [tex.cmd];
      let submit = vk::SubmitInfo::default().command_buffers(&cmd_buffers);
      device
        .queue_submit(guard.queue(), &[submit], tex.upload_fence)
        .map_err(|e| format!("{e:?}"))?;
    }
    tex.read_frame = None;
    self.newest = Some(index);
    Ok(())
  }
}

impl Drop for TexturePool {
  fn drop(&mut self) {
    unsafe {
      let _ = self.device.device_wait_idle();
      for tex in &mut self.textures {
        tex.texture.destroy(&self.device);
        self.device.destroy_fence(tex.upload_fence, None);
      }
      self.device.destroy_sampler(self.sampler, None);
      // Also frees the textures' command buffers.
      self.device.destroy_command_pool(self.cmd_pool, None);
    }
    // The descriptor sets belong to the window's pool and go with it.
  }
}

/// Where a node sends frames for its ImageViewer. Cheap to clone and usable
/// from any thread; frames sent while no viewer is open are ignored.
#[derive(Clone, Default)]
pub struct ImageSink {
  pool: Arc<Mutex<Option<TexturePool>>>,
}

impl ImageSink {
  pub fn new() -> Self {
    Self::default()
  }

  /// Writes `image` into the viewer's texture right away, on the calling
  /// thread, if a viewer is open and `image` will still be the latest frame
  /// when the viewer next renders (now + its frame interval is at or after the
  /// next render). Frames that would be replaced before then are skipped
  /// without being copied. Unsupported formats are ignored.
  ///
  /// Never waits on the GPU or the render loop: the frame is dropped instead
  /// if the render loop is using the textures right now or none is free. The
  /// only lock it can wait on is Thalamus's Vulkan queue lock, held briefly
  /// around the submit.
  pub fn update(&self, image: &dyn ImageData) {
    let Ok(mut guard) = self.pool.try_lock() else {
      return;
    };
    let Some(pool) = guard.as_mut() else {
      return;
    };
    if Instant::now() + image.frame_interval() < pool.next_render {
      return;
    }
    if let Err(e) = pool.upload(image) {
      println!("ImageViewer: texture upload failed: {e}");
    }
  }
}

/// Rotations offered in the viewer, in clockwise quarter turns: index `i` is
/// `ROTATIONS[i]` degrees.
const ROTATIONS: [&str; 4] = ["0", "90", "180", "270"];

/// The preview window itself: draws the newest uploaded texture once per
/// `render` call. Main thread only.
struct ViewerWindow {
  window: ImguiWindow,
  sink: ImageSink,
  /// Index into ROTATIONS, i.e. the number of clockwise quarter turns.
  quarter_turns: usize,
}

impl ViewerWindow {
  fn new(
    api: ThalamusAPI,
    title: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    sink: &ImageSink,
  ) -> Result<Self, String> {
    let mut window = ImguiWindow::new(api, title, x, y, width, height)?;
    let pool = TexturePool::new(api, &mut window)?;
    *sink.pool.lock().unwrap() = Some(pool);
    Ok(ViewerWindow {
      window,
      sink: sink.clone(),
      quarter_turns: 0,
    })
  }

  fn should_close(&self) -> bool {
    self.window.should_close()
  }

  /// Current window position and size (`(x, y, w, h)`).
  fn position_size(&self) -> (i32, i32, i32, i32) {
    self.window.position_size()
  }

  /// Renders one tick of the UI: a window containing a rotation combo box
  /// (0, 90, 180 or 270 degrees) and the newest uploaded image drawn at that
  /// rotation, scaled to fit the space below it.
  fn render(&mut self) {
    let sink = &self.sink;
    let quarter_turns = &mut self.quarter_turns;

    let result = self.window.render_frame(
      // Picks the texture to draw and marks it as sampled by this frame, so
      // uploads leave it alone until the frame has finished. Returned
      // rather than read by `build_ui` itself: both closures are constructed
      // together as arguments to this call, so they can't both borrow `sink`
      // mutably.
      |_device, _cmd, frame_idx| -> Option<(imgui::TextureId, u32, u32)> {
        let mut guard = sink.pool.lock().unwrap();
        let pool = guard.as_mut()?;
        pool.next_render = Instant::now() + REFRESH_INTERVAL;
        let frame = pool.start_frame(frame_idx);
        let tex = &mut pool.textures[pool.newest?];
        tex.read_frame = Some(frame);
        Some((tex.id, tex.texture.w, tex.texture.h))
      },
      |ui, _frame_idx, shown| {
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
            ui.combo_simple_string("Rotation", quarter_turns, &ROTATIONS);

            let Some((texture_id, tex_w, tex_h)) = shown else {
              return;
            };
            let avail = ui.content_region_avail();
            if avail[0] <= 1.0 || avail[1] <= 1.0 {
              return;
            }
            // A quarter or three-quarter turn swaps the image's width and
            // height on screen.
            let turns = *quarter_turns % 4;
            let (img_w, img_h) = if turns % 2 == 1 {
              (tex_h.max(1) as f32, tex_w.max(1) as f32)
            } else {
              (tex_w.max(1) as f32, tex_h.max(1) as f32)
            };
            let scale = (avail[0] / img_w).min(avail[1] / img_h);
            let (dw, dh) = (img_w * scale, img_h * scale);

            // Centered in the available area.
            let origin = ui.cursor_screen_pos();
            let x0 = origin[0] + (avail[0] - dw) / 2.0;
            let y0 = origin[1] + (avail[1] - dh) / 2.0;
            let (x1, y1) = (x0 + dw, y0 + dh);

            // The screen rectangle stays upright; the rotation is applied by
            // rotating which texture corner lands on each screen corner.
            // Screen corners in order: top-left, top-right, bottom-right,
            // bottom-left. Turning the image clockwise by a quarter moves its
            // bottom-left corner to the screen's top-left, and so on.
            const UV_CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
            let uv = |corner: usize| UV_CORNERS[(corner + 4 - turns) % 4];
            ui.get_window_draw_list()
              .add_image_quad(texture_id, [x0, y0], [x1, y0], [x1, y1], [x0, y1])
              .uv(uv(0), uv(1), uv(2), uv(3))
              .build();

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

/// Reads `view_geometry` as `(x, y, w, h)` if the key exists and is a list
/// with (at least) 4 int elements -- mirrors `read_geometry` in
/// image_viewer.cpp.
fn read_geometry(state: &State) -> Option<(i32, i32, i32, i32)> {
  let StateValue::List(list) = state.get(StateKey::String("view_geometry".to_string()))? else {
    return None;
  };
  let mut values = Vec::with_capacity(4);
  for entry in &list {
    if let StateValue::Int(v) = entry.val {
      values.push(v);
    }
  }
  if values.len() < 4 {
    return None;
  }
  Some((
    values[0] as i32,
    values[1] as i32,
    values[2] as i32,
    values[3] as i32,
  ))
}

/// Replaces `view_geometry` with a freshly built `[x, y, w, h]` list --
/// mirrors `write_geometry` in image_viewer.cpp, which likewise always
/// reassigns the whole array rather than mutating elements in place.
fn write_geometry(api: ThalamusAPI, state: &State, (x, y, w, h): (i32, i32, i32, i32)) {
  let list = State::make_list(api);
  list.push_int(x as i64);
  list.push_int(y as i64);
  list.push_int(w as i64);
  list.push_int(h as i64);
  state.set(
    StateKey::String("view_geometry".to_string()),
    StateValue::List(list),
  );
}

/// Redraws the preview window on the main thread every REFRESH_INTERVAL, and
/// once a second persists a moved/resized window into `view_geometry`. Ends
/// when the ImageViewer is dropped (the window is then gone) or when the
/// window's X button is pressed, which also sets the node's `View` to false.
async fn tick_loop(
  api: ThalamusAPI,
  window: Weak<RefCell<ViewerWindow>>,
  state: State,
  token: MainThreadToken,
  initial_geometry: (i32, i32, i32, i32),
) {
  let timer = api.create_timer();
  let mut last_geometry_check = Instant::now();
  let mut last_geometry = initial_geometry;

  loop {
    let _ = timer.sleep(REFRESH_INTERVAL).await;

    let Some(window) = window.upgrade() else { break };
    let mut window = window.borrow_mut();

    if window.should_close() {
      // The node reacts to View = false by dropping its ImageViewer, and
      // with it this task's TaskScope. Doing that from inside this task's
      // own poll would deadlock (TaskScope::drop locks the same Task state
      // the poll is holding), so set View once the poll has returned.
      let state = MainThreadOnly::new(state.clone(), token);
      api.post_to_main(move |token| {
        state.take(token).set("View", false);
      });
      break;
    }

    window.render();

    let now = Instant::now();
    let geometry_to_write = if now.duration_since(last_geometry_check) >= Duration::from_secs(1) {
      last_geometry_check = now;
      let geometry = window.position_size();
      if geometry != last_geometry {
        last_geometry = geometry;
        Some(geometry)
      } else {
        None
      }
    } else {
      None
    };
    drop(window);

    if let Some(geometry) = geometry_to_write {
      write_geometry(api, &state, geometry);
    }
  }
}

/// A preview window showing the images sent to an ImageSink (see
/// ImageSink::update). Its position and size are read from, and saved to, the
/// node's `view_geometry`; pressing its X button sets the node's `View` to
/// false. The window, its render loop and its textures live exactly as long
/// as this value: drop it to close them. Main thread only.
pub struct ImageViewer {
  task: Option<TaskScope>,
  window: Option<Rc<RefCell<ViewerWindow>>>,
  sink: ImageSink,
}

impl ImageViewer {
  pub fn new(
    api: ThalamusAPI,
    state: State,
    title: &str,
    sink: &ImageSink,
    token: MainThreadToken,
  ) -> Result<Self, String> {
    // Writes a default view_geometry if there isn't one yet, matching the
    // C++ ImageViewer constructor.
    let geometry = match read_geometry(&state) {
      Some(geometry) => geometry,
      None => {
        let default_geometry = (100, 100, 400, 400);
        write_geometry(api, &state, default_geometry);
        default_geometry
      }
    };
    let (x, y, w, h) = geometry;
    let window = Rc::new(RefCell::new(ViewerWindow::new(
      api, title, x, y, w, h, sink,
    )?));
    let task = run_task(tick_loop(
      api,
      Rc::downgrade(&window),
      state,
      token,
      geometry,
    ));
    Ok(ImageViewer {
      task: Some(task),
      window: Some(window),
      sink: sink.clone(),
    })
  }
}

impl Drop for ImageViewer {
  fn drop(&mut self) {
    // Stop rendering, then detach the textures from the sink (waiting for any
    // upload in progress on another thread) so no more uploads start, then
    // destroy the window -- which waits for the GPU -- before the textures its
    // last frames may still reference.
    self.task = None;
    let pool = self.sink.pool.lock().unwrap().take();
    self.window = None;
    drop(pool);
  }
}
