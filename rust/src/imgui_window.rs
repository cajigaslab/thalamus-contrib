//! Owns a native window (via ThalamusAPI, never SDL directly) plus the
//! swapchain built against it, and drives an `imgui-rs-vulkan-renderer`
//! Renderer + `ImguiPlatform` on top of Thalamus's shared VkInstance/VkDevice.
//!
//! `render_frame` must be called periodically (e.g. from a Thalamus timer)
//! on the same thread that created this window -- nothing here spawns its
//! own thread or event loop; scheduling is the caller's responsibility.
//!
//! Besides drawing imgui's own widgets, callers can register their own
//! Vulkan-backed textures (e.g. a video frame) via `register_texture` and
//! display them with `ui.image()`/`draw_list.add_image_quad()`. Updating a
//! registered texture's pixel data must happen through `render_frame`'s
//! `prepare` hook, which runs on the exact command buffer (and frame-in-flight
//! slot) that frame will use -- see its doc comment for why.

use ash::khr;
use ash::vk;
use imgui::Context;
use imgui_rs_vulkan_renderer::{Options, Renderer};

use crate::api::{SDLWindow, ThalamusAPI};
use crate::ffi::{THALAMUS_SDL_WINDOW_RESIZABLE, THALAMUS_SDL_WINDOW_VULKAN};
use crate::imgui_platform::ImguiPlatform;

/// Number of frames-in-flight this window (and hence its swapchain sync
/// objects and command buffers) supports. Callers maintaining their own
/// per-frame resources (e.g. a texture updated from `render_frame`'s
/// `prepare` hook) should size those arrays to match, indexed by the
/// `frame_idx` that hook and `build_ui` receive.
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

pub struct ImguiWindow {
  api: ThalamusAPI,
  window: SDLWindow,
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
  // One per swapchain image, not per frame-in-flight -- a binary semaphore
  // must be unsignaled when signaled again, but presents happen
  // asynchronously relative to the in-flight fence, so a frame-in-flight
  // indexed semaphore can still be mid-present (per a previous, different
  // image index) when this frame tries to reuse and re-signal it. Rebuilt
  // alongside the swapchain images themselves.
  render_finished: Vec<vk::Semaphore>,
  render_pass: vk::RenderPass,
  cmd_pool: vk::CommandPool,
  cmd_buffers: Vec<vk::CommandBuffer>,
  image_available: Vec<vk::Semaphore>,
  in_flight: Vec<vk::Fence>,
  frame: usize,
  dirty: bool,

  // Separate from imgui's own internal (font-only) descriptor pool -- sized
  // for whatever custom textures callers register via `register_texture`.
  custom_desc_layout: vk::DescriptorSetLayout,
  custom_desc_pool: vk::DescriptorPool,

  pub ctx: Context,
  platform: ImguiPlatform,
  renderer: Renderer,
  last_frame_time: std::time::Instant,
}

/// Max number of caller-registered custom textures (e.g. video preview
/// frames) this window's dedicated descriptor pool supports.
const MAX_CUSTOM_TEXTURES: u32 = 16;

impl ImguiWindow {
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
      let formats = surface_loader
        .get_physical_device_surface_formats(physical_device, surface)
        .map_err(|e| format!("{e:?}"))?;
      formats
        .iter()
        .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
        .copied()
        .unwrap_or(formats[0])
    };

    let render_pass = unsafe { Self::create_render_pass(&device, surface_format.format) }.map_err(|e| format!("{e:?}"))?;
    let cmd_pool = api.create_vulkan_command_pool();

    let mut ctx = Context::create();
    let platform = ImguiPlatform::new(api, &mut ctx, window_id);

    let renderer = {
      let guard = api.lock_vulkan_queue();
      unsafe {
        Renderer::with_default_allocator(
          &instance,
          physical_device,
          device.clone(),
          guard.queue(),
          cmd_pool,
          render_pass,
          &mut ctx,
          Some(Options { in_flight_frames: MAX_FRAMES_IN_FLIGHT, ..Default::default() }),
        )
      }
      .map_err(|e| e.to_string())?
    };

    let custom_desc_layout = imgui_rs_vulkan_renderer::vulkan::create_vulkan_descriptor_set_layout(&device).map_err(|e| e.to_string())?;
    // Not imgui_rs_vulkan_renderer::vulkan::create_vulkan_descriptor_pool:
    // it hardcodes descriptor_count to 1 regardless of the max_sets argument,
    // so the pool it returns can only ever satisfy a single allocation no
    // matter what's passed -- every registration after the first fails
    // (VUID-VkDescriptorSetAllocateInfo-apiVersion-07896, pool exhausted).
    let custom_desc_pool = unsafe {
      let sizes = [vk::DescriptorPoolSize::default().ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(MAX_CUSTOM_TEXTURES)];
      device.create_descriptor_pool(
        &vk::DescriptorPoolCreateInfo::default().pool_sizes(&sizes).max_sets(MAX_CUSTOM_TEXTURES).flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET),
        None,
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let cmd_buffers = unsafe {
      device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
          .command_pool(cmd_pool)
          .level(vk::CommandBufferLevel::PRIMARY)
          .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let (image_available, in_flight) =
      unsafe { Self::create_sync_objects(&device, MAX_FRAMES_IN_FLIGHT) }.map_err(|e| format!("{e:?}"))?;

    let mut result = ImguiWindow {
      api,
      window,
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
      render_finished: Vec::new(),
      render_pass,
      cmd_pool,
      cmd_buffers,
      image_available,
      in_flight,
      frame: 0,
      dirty: true,
      custom_desc_layout,
      custom_desc_pool,
      ctx,
      platform,
      renderer,
      last_frame_time: std::time::Instant::now(),
    };
    result.recreate_swapchain()?;
    Ok(result)
  }

  pub fn should_close(&self) -> bool {
    self.platform.should_close()
  }

  /// Current window position and size (`(x, y, w, h)`).
  pub fn position_size(&self) -> (i32, i32, i32, i32) {
    let (x, y) = self.window.position();
    let (w, h) = self.window.size();
    (x, y, w, h)
  }

  /// Handles a caller may need to build its own Vulkan resources (e.g. a
  /// custom texture) against the same instance/device this window uses.
  pub fn device(&self) -> &ash::Device {
    &self.device
  }
  pub fn instance(&self) -> &ash::Instance {
    &self.instance
  }
  pub fn physical_device(&self) -> vk::PhysicalDevice {
    self.physical_device
  }
  pub fn cmd_pool(&self) -> vk::CommandPool {
    self.cmd_pool
  }

  /// Registers a Vulkan-backed texture (image view + sampler) for display via
  /// `ui.image()`/`draw_list.add_image_quad()`, returning its `TextureId`
  /// alongside the raw `vk::DescriptorSet` backing it -- callers that need to
  /// rebind the descriptor later (e.g. after rebuilding the underlying image
  /// at a new size) do so directly with that handle via
  /// `ash::Device::update_descriptor_sets`, using the `&ash::Device` `prepare`
  /// already receives, rather than calling back into this window (which is
  /// already mutably borrowed as the `render_frame` receiver at that point).
  pub fn register_texture(&mut self, image_view: vk::ImageView, sampler: vk::Sampler) -> Result<(imgui::TextureId, vk::DescriptorSet), String> {
    let set = imgui_rs_vulkan_renderer::vulkan::create_vulkan_descriptor_set(&self.device, self.custom_desc_layout, self.custom_desc_pool, image_view, sampler)
      .map_err(|e| e.to_string())?;
    let id = self.renderer.textures().insert(set);
    Ok((id, set))
  }

  /// Pumps pending SDL events into imgui, then builds+renders one frame and
  /// presents it. Call once per tick from the main thread.
  ///
  /// `prepare(device, cmd, frame_idx)` runs after the frame's command buffer
  /// is opened but before the render pass begins -- the only place commands
  /// that can't run inside a render pass instance (buffer-to-image copies,
  /// most pipeline barriers) are allowed. `frame_idx` is this window's
  /// current frame-in-flight slot (`0..MAX_FRAMES_IN_FLIGHT`): recording a
  /// texture upload into slot `frame_idx`'s own command buffer means it
  /// inherits this window's existing fence wait for that slot, so a caller
  /// keeping one texture per `MAX_FRAMES_IN_FLIGHT` slot (indexed the same
  /// way) never writes to a texture a previous frame's submission might
  /// still be sampling from, with no extra synchronization needed. The same
  /// `frame_idx` is then passed to `build_ui`, along with whatever `prepare`
  /// returned, so it can select the matching texture's `TextureId` -- threading
  /// data through this return value (rather than a variable both closures
  /// capture) sidesteps a borrow conflict: the two closures are constructed
  /// together as arguments to this call, so the borrow checker requires their
  /// captures to coexist even though `prepare` fully runs and returns before
  /// `build_ui` starts.
  pub fn render_frame<R>(
    &mut self,
    prepare: impl FnOnce(&ash::Device, vk::CommandBuffer, usize) -> R,
    build_ui: impl FnOnce(&imgui::Ui, usize, R),
  ) -> Result<(), String> {
    let now = std::time::Instant::now();
    let delta = (now - self.last_frame_time).as_secs_f32();
    self.last_frame_time = now;
    self.platform.new_frame(&mut self.ctx, delta);

    if self.dirty {
      self.recreate_swapchain()?;
      if self.dirty {
        return Ok(()); // zero-size (e.g. minimized) -- nothing to draw this tick
      }
    }

    // ImguiPlatform doesn't track window size, only events -- without this,
    // io.display_size stays at its zeroed default and ctx.frame() below trips
    // Dear ImGui's "Invalid DisplaySize value!" assertion (it requires >= 0,
    // but more to the point a real UI needs the actual extent to lay out
    // against). Pixel dimensions, matching the swapchain; no separate
    // display_framebuffer_scale handling (HiDPI) is done anywhere else here.
    self.ctx.io_mut().display_size = [self.extent.width as f32, self.extent.height as f32];

    let frame_idx = self.frame % self.in_flight.len();
    unsafe {
      self
        .device
        .wait_for_fences(&[self.in_flight[frame_idx]], true, u64::MAX)
        .map_err(|e| format!("{e:?}"))?;

      let image_index = match self.swapchain_loader.acquire_next_image(
        self.swapchain,
        u64::MAX,
        self.image_available[frame_idx],
        vk::Fence::null(),
      ) {
        Ok((index, _suboptimal)) => index,
        Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
          self.dirty = true;
          return Ok(());
        }
        Err(e) => return Err(format!("{e:?}")),
      };

      self.device.reset_fences(&[self.in_flight[frame_idx]]).map_err(|e| format!("{e:?}"))?;

      let cmd = self.cmd_buffers[frame_idx];
      self
        .device
        .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
        .map_err(|e| format!("{e:?}"))?;
      self
        .device
        .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
        .map_err(|e| format!("{e:?}"))?;

      let prepared = prepare(&self.device, cmd, frame_idx);

      let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } }];
      let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(self.render_pass)
        .framebuffer(self.framebuffers[image_index as usize])
        .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
        .clear_values(&clear);
      self.device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);

      let ui = self.ctx.frame();
      build_ui(ui, frame_idx, prepared);
      let draw_data = self.ctx.render();
      self.renderer.cmd_draw(cmd, draw_data).map_err(|e| e.to_string())?;

      self.device.cmd_end_render_pass(cmd);
      self.device.end_command_buffer(cmd).map_err(|e| format!("{e:?}"))?;

      let guard = self.api.lock_vulkan_queue();
      let wait_semaphores = [self.image_available[frame_idx]];
      let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
      let signal_semaphores = [self.render_finished[image_index as usize]];
      let cmd_buffers = [cmd];
      let submit_info = vk::SubmitInfo::default()
        .wait_semaphores(&wait_semaphores)
        .wait_dst_stage_mask(&wait_stages)
        .command_buffers(&cmd_buffers)
        .signal_semaphores(&signal_semaphores);
      self
        .device
        .queue_submit(guard.queue(), &[submit_info], self.in_flight[frame_idx])
        .map_err(|e| format!("{e:?}"))?;

      let swapchains = [self.swapchain];
      let image_indices = [image_index];
      let present_info = vk::PresentInfoKHR::default()
        .wait_semaphores(&signal_semaphores)
        .swapchains(&swapchains)
        .image_indices(&image_indices);
      match self.swapchain_loader.queue_present(guard.queue(), &present_info) {
        Ok(suboptimal) => {
          if suboptimal {
            self.dirty = true;
          }
        }
        Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
        Err(e) => return Err(format!("{e:?}")),
      }
    }

    self.frame = self.frame.wrapping_add(1);
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
          let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.surface_format.format)
            .subresource_range(vk::ImageSubresourceRange {
              aspect_mask: vk::ImageAspectFlags::COLOR,
              base_mip_level: 0,
              level_count: 1,
              base_array_layer: 0,
              layer_count: 1,
            });
          self.device.create_image_view(&view_info, None)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.framebuffers = self
        .image_views
        .iter()
        .map(|&view| {
          let attachments = [view];
          let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(&attachments)
            .width(self.extent.width)
            .height(self.extent.height)
            .layers(1);
          self.device.create_framebuffer(&fb_info, None)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.render_finished = (0..images.len())
        .map(|_| self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e:?}"))?;

      self.dirty = false;
      Ok(())
    }
  }

  unsafe fn destroy_swapchain_deps(&mut self) {
    unsafe {
      for &s in &self.render_finished {
        self.device.destroy_semaphore(s, None);
      }
      self.render_finished.clear();
      for &fb in &self.framebuffers {
        self.device.destroy_framebuffer(fb, None);
      }
      self.framebuffers.clear();
      for &view in &self.image_views {
        self.device.destroy_image_view(view, None);
      }
      self.image_views.clear();
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
    let subpass = [vk::SubpassDescription::default()
      .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
      .color_attachments(&color_ref)];
    let dependency = [vk::SubpassDependency::default()
      .src_subpass(vk::SUBPASS_EXTERNAL)
      .dst_subpass(0)
      .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
      .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
      .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    unsafe {
      device.create_render_pass(
        &vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpass).dependencies(&dependency),
        None,
      )
    }
  }

  unsafe fn create_sync_objects(
    device: &ash::Device,
    count: usize,
  ) -> Result<(Vec<vk::Semaphore>, Vec<vk::Fence>), vk::Result> {
    let mut image_available = Vec::with_capacity(count);
    let mut in_flight = Vec::with_capacity(count);
    for _ in 0..count {
      image_available.push(unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?);
      in_flight.push(unsafe {
        device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)
      }?);
    }
    Ok((image_available, in_flight))
  }
}

impl Drop for ImguiWindow {
  fn drop(&mut self) {
    unsafe {
      let _ = self.device.device_wait_idle();
      for &s in &self.image_available {
        self.device.destroy_semaphore(s, None);
      }
      for &f in &self.in_flight {
        self.device.destroy_fence(f, None);
      }
      // Also destroys render_finished (one per swapchain image -- see its
      // field doc comment for why it isn't alongside image_available/in_flight).
      self.destroy_swapchain_deps();
      if self.swapchain != vk::SwapchainKHR::null() {
        self.swapchain_loader.destroy_swapchain(self.swapchain, None);
      }
      self.device.destroy_descriptor_pool(self.custom_desc_pool, None);
      self.device.destroy_descriptor_set_layout(self.custom_desc_layout, None);
      self.device.destroy_render_pass(self.render_pass, None);
      self.device.destroy_command_pool(self.cmd_pool, None);
      self.surface_loader.destroy_surface(self.surface, None);
      // `renderer` destroys its own pipeline/descriptors/font texture via its
      // own Drop impl (it holds its own ash::Device clone -- valid as long as
      // Thalamus's real VkDevice is alive, which we never touch here).
      // `window` destroys itself via SDLWindow's Drop.
      // instance/device themselves are Thalamus's -- never destroyed here.
    }
  }
}
