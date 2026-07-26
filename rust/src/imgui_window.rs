//! Owns a native window (via ThalamusAPI, never SDL directly) plus the
//! swapchain built against it, and drives an `imgui-rs-vulkan-renderer`
//! Renderer + `ImguiPlatform` on top of Thalamus's shared VkInstance/VkDevice.
//!
//! `render_frame` must be called periodically (e.g. from a Thalamus timer)
//! on the same thread that created this window -- nothing here spawns its
//! own thread or event loop; scheduling is the caller's responsibility.

use ash::khr;
use ash::vk;
use imgui::Context;
use imgui_rs_vulkan_renderer::{Options, Renderer};

use crate::api::{SDLWindow, ThalamusAPI};
use crate::ffi::{THALAMUS_SDL_WINDOW_RESIZABLE, THALAMUS_SDL_WINDOW_VULKAN};
use crate::imgui_platform::ImguiPlatform;

const MAX_FRAMES_IN_FLIGHT: usize = 2;

pub struct ImguiWindow {
  api: ThalamusAPI,
  #[allow(dead_code)]
  window: SDLWindow, // kept alive; destroyed via its own Drop
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
  render_pass: vk::RenderPass,
  cmd_pool: vk::CommandPool,
  cmd_buffers: Vec<vk::CommandBuffer>,
  image_available: Vec<vk::Semaphore>,
  render_finished: Vec<vk::Semaphore>,
  in_flight: Vec<vk::Fence>,
  frame: usize,
  dirty: bool,

  pub ctx: Context,
  platform: ImguiPlatform,
  renderer: Renderer,
  last_frame_time: std::time::Instant,
}

impl ImguiWindow {
  pub fn new(api: ThalamusAPI, title: &str, width: i32, height: i32) -> Result<Self, String> {
    let window = api.create_sdl_window(title, width, height, THALAMUS_SDL_WINDOW_VULKAN | THALAMUS_SDL_WINDOW_RESIZABLE)?;
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

    let cmd_buffers = unsafe {
      device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
          .command_pool(cmd_pool)
          .level(vk::CommandBufferLevel::PRIMARY)
          .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32),
      )
    }
    .map_err(|e| format!("{e:?}"))?;

    let (image_available, render_finished, in_flight) =
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
      render_pass,
      cmd_pool,
      cmd_buffers,
      image_available,
      render_finished,
      in_flight,
      frame: 0,
      dirty: true,
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

  /// Pumps pending SDL events into imgui, builds+renders one frame, and
  /// presents it. Call once per tick from the main thread.
  pub fn render_frame(&mut self, build_ui: impl FnOnce(&imgui::Ui)) -> Result<(), String> {
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

      let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } }];
      let rp_begin = vk::RenderPassBeginInfo::default()
        .render_pass(self.render_pass)
        .framebuffer(self.framebuffers[image_index as usize])
        .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
        .clear_values(&clear);
      self.device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);

      let ui = self.ctx.frame();
      build_ui(ui);
      let draw_data = self.ctx.render();
      self.renderer.cmd_draw(cmd, draw_data).map_err(|e| e.to_string())?;

      self.device.cmd_end_render_pass(cmd);
      self.device.end_command_buffer(cmd).map_err(|e| format!("{e:?}"))?;

      let guard = self.api.lock_vulkan_queue();
      let wait_semaphores = [self.image_available[frame_idx]];
      let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
      let signal_semaphores = [self.render_finished[frame_idx]];
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
  ) -> Result<(Vec<vk::Semaphore>, Vec<vk::Semaphore>, Vec<vk::Fence>), vk::Result> {
    let mut image_available = Vec::with_capacity(count);
    let mut render_finished = Vec::with_capacity(count);
    let mut in_flight = Vec::with_capacity(count);
    for _ in 0..count {
      image_available.push(unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?);
      render_finished.push(unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?);
      in_flight.push(unsafe {
        device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)
      }?);
    }
    Ok((image_available, render_finished, in_flight))
  }
}

impl Drop for ImguiWindow {
  fn drop(&mut self) {
    unsafe {
      let _ = self.device.device_wait_idle();
      for &s in &self.image_available {
        self.device.destroy_semaphore(s, None);
      }
      for &s in &self.render_finished {
        self.device.destroy_semaphore(s, None);
      }
      for &f in &self.in_flight {
        self.device.destroy_fence(f, None);
      }
      self.destroy_swapchain_deps();
      if self.swapchain != vk::SwapchainKHR::null() {
        self.swapchain_loader.destroy_swapchain(self.swapchain, None);
      }
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
