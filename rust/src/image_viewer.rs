//! Nests a live camera preview inside an imgui UI (window chrome, a rotation
//! combo box) on top of `ImguiWindow`. Each camera frame's raw bytes are
//! copied into a GPU buffer as-is, converted to RGBA by a compute shader
//! (shaders/convert.comp) into a Vulkan texture, and displayed via `imgui`'s
//! custom-texture support. Every uncompressed ImageFormat is supported;
//! MJPEG, MPEG1 and MPEG4 frames are dropped. imgui
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
  OnDrop, TaskScope, ThalamusAPI, run_task,
};
use crate::imgui_window::{ImguiWindow, MAX_FRAMES_IN_FLIGHT};

/// Format codes understood by the conversion shader; must match the
/// constants in shaders/convert.comp.
#[repr(u32)]
#[derive(Clone, Copy)]
enum ShaderFormat {
  Gray = 0,
  Rgb = 1,
  Bgr = 2,
  Yuyv422 = 3,
  Yuv420p = 4,
  Yuvj420p = 5,
  Nv12 = 6,
  Gray16Le = 7,
  Rgb16Le = 8,
  Gray16Be = 9,
  Rgb16Be = 10,
}

/// 16-bit formats are in the native byte order of the platform that produced
/// them, i.e. this one.
const BIG_ENDIAN: bool = cfg!(target_endian = "big");

/// The planes of a frame as the conversion shader reads them: each plane's
/// bytes and row stride.
struct ShaderInput<'a> {
  format: ShaderFormat,
  planes: [(&'a [u8], u32); 3],
  plane_count: usize,
}

impl ShaderInput<'_> {
  fn len(&self) -> usize {
    self.planes[..self.plane_count]
      .iter()
      .map(|(bytes, _)| bytes.len())
      .sum()
  }
}

/// Splits `image` into the planes the conversion shader reads, or `None` if
/// it can't be displayed: compressed formats (MJPEG, MPEG1, MPEG4), empty
/// images, and planes too short for the image's size.
///
/// Row strides aren't part of ImageData, so each plane's stride is its length
/// divided by its row count. Planar formats delivered as a single plane are
/// assumed to be tightly packed.
fn shader_input(image: &dyn ImageData) -> Option<ShaderInput<'_>> {
  let (w, h) = (image.width() as usize, image.height() as usize);
  if w == 0 || h == 0 {
    return None;
  }
  let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
  // Per plane: the minimum bytes per row and the number of rows.
  let (format, dims, plane_count) = match image.format() {
    ImageFormat::Gray => (ShaderFormat::Gray, [(w, h), (0, 0), (0, 0)], 1),
    ImageFormat::RGB => (ShaderFormat::Rgb, [(3 * w, h), (0, 0), (0, 0)], 1),
    ImageFormat::BGR => (ShaderFormat::Bgr, [(3 * w, h), (0, 0), (0, 0)], 1),
    ImageFormat::YUYV422 => (ShaderFormat::Yuyv422, [(4 * cw, h), (0, 0), (0, 0)], 1),
    ImageFormat::YUV420P => (ShaderFormat::Yuv420p, [(w, h), (cw, ch), (cw, ch)], 3),
    ImageFormat::YUVJ420P => (ShaderFormat::Yuvj420p, [(w, h), (cw, ch), (cw, ch)], 3),
    ImageFormat::NV12 => (ShaderFormat::Nv12, [(w, h), (2 * cw, ch), (0, 0)], 2),
    ImageFormat::Gray16 => (
      if BIG_ENDIAN { ShaderFormat::Gray16Be } else { ShaderFormat::Gray16Le },
      [(2 * w, h), (0, 0), (0, 0)],
      1,
    ),
    ImageFormat::RGB16 => (
      if BIG_ENDIAN { ShaderFormat::Rgb16Be } else { ShaderFormat::Rgb16Le },
      [(6 * w, h), (0, 0), (0, 0)],
      1,
    ),
    ImageFormat::MJPEG | ImageFormat::MPEG1 | ImageFormat::MPEG4 => return None,
  };

  let mut planes: [(&[u8], u32); 3] = [(&[], 0); 3];
  if image.num_planes() as usize >= plane_count {
    for (i, &(row_bytes, rows)) in dims[..plane_count].iter().enumerate() {
      let bytes = image.plane(i as i32);
      let stride = bytes.len() / rows;
      if stride < row_bytes {
        return None;
      }
      planes[i] = (&bytes[..stride * rows], stride as u32);
    }
  } else {
    let mut rest = image.plane(0);
    for (i, &(row_bytes, rows)) in dims[..plane_count].iter().enumerate() {
      if rest.len() < row_bytes * rows {
        return None;
      }
      let (bytes, tail) = rest.split_at(row_bytes * rows);
      planes[i] = (bytes, row_bytes as u32);
      rest = tail;
    }
  }
  Some(ShaderInput {
    format,
    planes,
    plane_count,
  })
}

/// Push constants for the conversion shader; layout matches `Params` in
/// shaders/convert.comp (whose six scalars after `height` are these two
/// arrays).
#[repr(C)]
#[derive(Clone, Copy)]
struct ShaderParams {
  format: u32,
  width: u32,
  height: u32,
  offsets: [u32; 3],
  strides: [u32; 3],
}

// Compiled from shaders/convert.comp by build.rs.
static CONVERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/convert.comp.spv"));

/// Workgroup size of the conversion shader in each dimension.
const CONVERT_GROUP_SIZE: u32 = 16;

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

/// One texture uploads rotate through: a host-visible buffer each frame's
/// raw bytes are copied into, and the RGBA image the conversion shader
/// writes from it and imgui samples.
#[derive(Default)]
struct Texture {
  w: u32,
  h: u32,
  image: vk::Image,
  memory: vk::DeviceMemory,
  view: vk::ImageView,
  buf: vk::Buffer,
  buf_mem: vk::DeviceMemory,
  buf_size: vk::DeviceSize,
  buf_mapped: *mut std::ffi::c_void,
}

impl Texture {
  fn destroy_image(&mut self, device: &ash::Device) {
    unsafe {
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
    self.w = 0;
    self.h = 0;
  }

  fn destroy_buffer(&mut self, device: &ash::Device) {
    unsafe {
      if !self.buf_mapped.is_null() {
        device.unmap_memory(self.buf_mem);
        self.buf_mapped = std::ptr::null_mut();
      }
      if self.buf != vk::Buffer::null() {
        device.destroy_buffer(self.buf, None);
        self.buf = vk::Buffer::null();
      }
      if self.buf_mem != vk::DeviceMemory::null() {
        device.free_memory(self.buf_mem, None);
        self.buf_mem = vk::DeviceMemory::null();
      }
    }
    self.buf_size = 0;
  }

  fn destroy(&mut self, device: &ash::Device) {
    self.destroy_image(device);
    self.destroy_buffer(device);
  }
}

/// Locks the shared Vulkan queue for a one-shot command buffer that moves a
/// freshly created image to `SHADER_READ_ONLY_OPTIMAL`, submits it, and waits
/// for it to finish. Only used for the placeholder textures at startup.
fn make_sampleable(
  api: ThalamusAPI,
  device: &ash::Device,
  cmd_pool: vk::CommandPool,
  image: vk::Image,
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
    record_barrier(
      device,
      cb,
      image,
      vk::ImageLayout::UNDEFINED,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
      vk::AccessFlags::empty(),
      vk::AccessFlags::SHADER_READ,
      vk::PipelineStageFlags::TOP_OF_PIPE,
      vk::PipelineStageFlags::FRAGMENT_SHADER,
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

/// (Re)builds `tex`'s image at `w`x`h`, leaving it in `UNDEFINED` layout.
/// RGBA8 because storage-image writes to it are guaranteed to be supported.
fn create_image(
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
  tex: &mut Texture,
  w: u32,
  h: u32,
) -> Result<(), String> {
  tex.destroy_image(device);
  let format = vk::Format::R8G8B8A8_UNORM;

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
        .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
        .initial_layout(vk::ImageLayout::UNDEFINED),
      None,
    )
  }
  .map_err(|e| format!("{e:?}"))?;
  tex.image = image;

  let req = unsafe { device.get_image_memory_requirements(image) };
  tex.memory = unsafe {
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
  unsafe { device.bind_image_memory(image, tex.memory, 0) }.map_err(|e| format!("{e:?}"))?;

  tex.view = unsafe {
    device.create_image_view(
      &vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
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
  tex.w = w;
  tex.h = h;
  Ok(())
}

/// (Re)builds `tex`'s source buffer with room for at least `size` bytes,
/// mapped for the lifetime of the buffer.
fn create_buffer(
  device: &ash::Device,
  instance: &ash::Instance,
  physical_device: vk::PhysicalDevice,
  tex: &mut Texture,
  size: vk::DeviceSize,
) -> Result<(), String> {
  tex.destroy_buffer(device);
  // The shader reads whole uints.
  let size = size.max(4).next_multiple_of(4);
  let (buf, buf_mem) = make_buffer(
    device,
    instance,
    physical_device,
    size,
    vk::BufferUsageFlags::STORAGE_BUFFER,
    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
  )?;
  tex.buf = buf;
  tex.buf_mem = buf_mem;
  tex.buf_mapped = unsafe { device.map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty()) }
    .map_err(|e| format!("{e:?}"))?;
  tex.buf_size = size;
  Ok(())
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
  /// imgui's descriptor set for sampling `texture.image`.
  descriptor_set: vk::DescriptorSet,
  /// The conversion shader's descriptor set: `texture.buf` in,
  /// `texture.image` out.
  convert_set: vk::DescriptorSet,
  /// Number of the last frame that sampled this texture (see
  /// TexturePool::frames_completed); it can't be rewritten until that frame
  /// has finished.
  read_frame: Option<u64>,
  /// Records this texture's uploads. Per texture, so an upload never has to
  /// wait for another texture's upload to finish before reusing it.
  cmd: vk::CommandBuffer,
  /// Signals when this texture's last upload has finished on the GPU (its
  /// source buffer and `cmd` are reusable). Created signaled.
  upload_fence: vk::Fence,
}

/// Points imgui's descriptor set for `tex` at its current image view.
fn write_sampler_descriptor(
  device: &ash::Device,
  set: vk::DescriptorSet,
  sampler: vk::Sampler,
  tex: &Texture,
) {
  let image_info = [vk::DescriptorImageInfo::default()
    .sampler(sampler)
    .image_view(tex.view)
    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
  let write = [vk::WriteDescriptorSet::default()
    .dst_set(set)
    .dst_binding(0)
    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
    .image_info(&image_info)];
  unsafe { device.update_descriptor_sets(&write, &[]) };
}

/// Points the conversion shader's descriptor set for `tex` at its current
/// buffer and image view.
fn write_convert_descriptor(device: &ash::Device, set: vk::DescriptorSet, tex: &Texture) {
  let buffer_info = [vk::DescriptorBufferInfo::default()
    .buffer(tex.buf)
    .offset(0)
    .range(vk::WHOLE_SIZE)];
  let image_info = [vk::DescriptorImageInfo::default()
    .image_view(tex.view)
    .image_layout(vk::ImageLayout::GENERAL)];
  let writes = [
    vk::WriteDescriptorSet::default()
      .dst_set(set)
      .dst_binding(0)
      .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
      .buffer_info(&buffer_info),
    vk::WriteDescriptorSet::default()
      .dst_set(set)
      .dst_binding(1)
      .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
      .image_info(&image_info),
  ];
  unsafe { device.update_descriptor_sets(&writes, &[]) };
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
  // The conversion shader and the pool its descriptor sets come from.
  convert_set_layout: vk::DescriptorSetLayout,
  convert_layout: vk::PipelineLayout,
  convert_pipeline: vk::Pipeline,
  descriptor_pool: vk::DescriptorPool,
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

    // Thalamus picks its queue by graphics support alone and doesn't expose
    // which family it picked, so require every graphics family to also do
    // compute (true of every desktop GPU).
    let families =
      unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    if families.iter().any(|f| {
      f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        && !f.queue_flags.contains(vk::QueueFlags::COMPUTE)
    }) {
      return Err("the GPU has a graphics queue without compute support".to_string());
    }

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

    let bindings = [
      vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE),
      vk::DescriptorSetLayoutBinding::default()
        .binding(1)
        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE),
    ];
    let convert_set_layout = unsafe {
      device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;
    let set_layouts = [convert_set_layout];
    let push_ranges = [vk::PushConstantRange::default()
      .stage_flags(vk::ShaderStageFlags::COMPUTE)
      .offset(0)
      .size(std::mem::size_of::<ShaderParams>() as u32)];
    let convert_layout = unsafe {
      device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default()
          .set_layouts(&set_layouts)
          .push_constant_ranges(&push_ranges),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let code = ash::util::read_spv(&mut std::io::Cursor::new(CONVERT_SPV))
      .map_err(|e| format!("{e:?}"))?;
    let module = unsafe {
      device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
    }
    .map_err(|e| format!("{e:?}"))?;
    let pipeline_info = vk::ComputePipelineCreateInfo::default()
      .stage(
        vk::PipelineShaderStageCreateInfo::default()
          .stage(vk::ShaderStageFlags::COMPUTE)
          .module(module)
          .name(c"main"),
      )
      .layout(convert_layout);
    let pipelines = unsafe {
      device.create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    };
    unsafe { device.destroy_shader_module(module, None) };
    let convert_pipeline = pipelines.map_err(|(_, e)| format!("{e:?}"))?[0];

    let pool_sizes = [
      vk::DescriptorPoolSize {
        ty: vk::DescriptorType::STORAGE_BUFFER,
        descriptor_count: TEXTURE_COUNT as u32,
      },
      vk::DescriptorPoolSize {
        ty: vk::DescriptorType::STORAGE_IMAGE,
        descriptor_count: TEXTURE_COUNT as u32,
      },
    ];
    let descriptor_pool = unsafe {
      device.create_descriptor_pool(
        &vk::DescriptorPoolCreateInfo::default()
          .max_sets(TEXTURE_COUNT as u32)
          .pool_sizes(&pool_sizes),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;
    let convert_sets = unsafe {
      device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
          .descriptor_pool(descriptor_pool)
          .set_layouts(&[convert_set_layout; TEXTURE_COUNT]),
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
    for (cmd, convert_set) in cmds.into_iter().zip(convert_sets) {
      let mut texture = Texture::default();
      create_image(&device, &instance, physical_device, &mut texture, 1, 1)?;
      create_buffer(&device, &instance, physical_device, &mut texture, 4)?;
      make_sampleable(api, &device, cmd_pool, texture.image)?;
      write_convert_descriptor(&device, convert_set, &texture);
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
        convert_set,
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
      convert_set_layout,
      convert_layout,
      convert_pipeline,
      descriptor_pool,
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

  /// Copies `image`'s raw bytes into a free texture's buffer and submits the
  /// shader that converts them into the texture's RGBA image, all on the
  /// calling thread, then makes it the newest texture. The only CPU work is
  /// the copy. Never waits on a fence: if no texture is free, or `image`
  /// can't be displayed (see `shader_input`), the frame is dropped. Frames
  /// drawn later see the finished conversion because they're submitted to the
  /// same queue after it, and the barriers recorded here order them.
  fn upload(&mut self, image: &dyn ImageData) -> Result<(), String> {
    let Some(input) = shader_input(image) else {
      return Ok(());
    };
    let (w, h) = (image.width() as u32, image.height() as u32);
    let Some(index) = self.free_texture() else {
      return Ok(());
    };

    let device = &self.device;
    let tex = &mut self.textures[index];

    // Rebuilding is safe here: the texture is free, so no frame or upload is
    // using it or its descriptor sets.
    let image_rebuilt = w != tex.texture.w || h != tex.texture.h;
    if image_rebuilt {
      create_image(
        device,
        &self.instance,
        self.physical_device,
        &mut tex.texture,
        w,
        h,
      )?;
      write_sampler_descriptor(device, tex.descriptor_set, self.sampler, &tex.texture);
    }
    let len = input.len();
    let buffer_rebuilt = len as vk::DeviceSize > tex.texture.buf_size;
    if buffer_rebuilt {
      create_buffer(
        device,
        &self.instance,
        self.physical_device,
        &mut tex.texture,
        len as vk::DeviceSize,
      )?;
    }
    if image_rebuilt || buffer_rebuilt {
      write_convert_descriptor(device, tex.convert_set, &tex.texture);
    }

    let mut params = ShaderParams {
      format: input.format as u32,
      width: w,
      height: h,
      offsets: [0; 3],
      strides: [0; 3],
    };
    let mut offset = 0;
    for (i, &(bytes, stride)) in input.planes[..input.plane_count].iter().enumerate() {
      // SAFETY: the buffer holds at least `len` bytes, the sum of the plane
      // lengths, and the GPU isn't reading it (its upload fence signaled).
      unsafe {
        std::ptr::copy_nonoverlapping(
          bytes.as_ptr(),
          (tex.texture.buf_mapped as *mut u8).add(offset),
          bytes.len(),
        );
      }
      params.offsets[i] = offset as u32;
      params.strides[i] = stride;
      offset += bytes.len();
    }
    // SAFETY: ShaderParams is repr(C) plain u32s, with no padding.
    let param_bytes = unsafe {
      std::slice::from_raw_parts(
        &params as *const ShaderParams as *const u8,
        std::mem::size_of::<ShaderParams>(),
      )
    };

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

    // A freshly built image has no contents or earlier readers to wait for.
    // Otherwise earlier frames on this queue may still be sampling it, and
    // this barrier orders the shader's writes after them. The buffer needs no
    // barrier: the submit makes host writes to coherent memory visible.
    let (from, src_stage) = if image_rebuilt {
      (vk::ImageLayout::UNDEFINED, vk::PipelineStageFlags::TOP_OF_PIPE)
    } else {
      (
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
      )
    };
    record_barrier(
      device,
      tex.cmd,
      tex.texture.image,
      from,
      vk::ImageLayout::GENERAL,
      vk::AccessFlags::empty(),
      vk::AccessFlags::SHADER_WRITE,
      src_stage,
      vk::PipelineStageFlags::COMPUTE_SHADER,
    );
    unsafe {
      device.cmd_bind_pipeline(tex.cmd, vk::PipelineBindPoint::COMPUTE, self.convert_pipeline);
      device.cmd_bind_descriptor_sets(
        tex.cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.convert_layout,
        0,
        &[tex.convert_set],
        &[],
      );
      device.cmd_push_constants(
        tex.cmd,
        self.convert_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        param_bytes,
      );
      device.cmd_dispatch(
        tex.cmd,
        w.div_ceil(CONVERT_GROUP_SIZE),
        h.div_ceil(CONVERT_GROUP_SIZE),
        1,
      );
    }
    record_barrier(
      device,
      tex.cmd,
      tex.texture.image,
      vk::ImageLayout::GENERAL,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
      vk::AccessFlags::SHADER_WRITE,
      vk::AccessFlags::SHADER_READ,
      vk::PipelineStageFlags::COMPUTE_SHADER,
      vk::PipelineStageFlags::FRAGMENT_SHADER,
    );

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
      self.device.destroy_pipeline(self.convert_pipeline, None);
      self.device.destroy_pipeline_layout(self.convert_layout, None);
      // Also frees the conversion descriptor sets.
      self.device.destroy_descriptor_pool(self.descriptor_pool, None);
      self
        .device
        .destroy_descriptor_set_layout(self.convert_set_layout, None);
      // Also frees the textures' command buffers.
      self.device.destroy_command_pool(self.cmd_pool, None);
    }
    // imgui's descriptor sets belong to the window's pool and go with it.
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

  fn set_title(&self, title: &str) {
    self.window.set_title(title);
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
/// The node's name, used as the viewer's window title.
fn read_name(state: &State) -> String {
  match state.get(StateKey::String("name".to_string())) {
    Some(StateValue::String(name)) => name,
    _ => "Image Viewer".to_string(),
  }
}

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
  new_title: Rc<RefCell<Option<String>>>,
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

    if let Some(title) = new_title.borrow_mut().take() {
      window.set_title(&title);
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
/// ImageSink::update), titled with the node's `name`. Its position and size
/// are read from, and saved to, the node's `view_geometry`; pressing its X button sets the node's `View` to
/// false. The window, its render loop and its textures live exactly as long
/// as this value: drop it to close them. Main thread only.
pub struct ImageViewer {
  task: Option<TaskScope>,
  window: Option<Rc<RefCell<ViewerWindow>>>,
  sink: ImageSink,
  // Flags renames of the node for the render loop, which retitles the window.
  _name_connection: OnDrop,
}

impl ImageViewer {
  pub fn new(
    api: ThalamusAPI,
    state: State,
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
    let title = read_name(&state);
    let window = Rc::new(RefCell::new(ViewerWindow::new(
      api, &title, x, y, w, h, sink,
    )?));
    // The connection is recursive; comparing the source to the node's own
    // state skips `name` keys in nested collections.
    let new_title = Rc::new(RefCell::new(None));
    let pending = new_title.clone();
    let node_state = state.clone();
    let name_connection = state.connect(move |source, _action, key, value| {
      if source == node_state && key == StateValue::String("name".to_string()) {
        if let StateValue::String(name) = value {
          *pending.borrow_mut() = Some(name);
        }
      }
    });
    let task = run_task(tick_loop(
      api,
      Rc::downgrade(&window),
      state,
      token,
      geometry,
      new_title,
    ));
    Ok(ImageViewer {
      task: Some(task),
      window: Some(window),
      sink: sink.clone(),
      _name_connection: name_connection,
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
