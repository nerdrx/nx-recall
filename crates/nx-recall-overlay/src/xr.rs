//! The overlay session: captions as a composition layer inside somebody else's
//! OpenXR frame.
//!
//! ## Read this before you trust any of it
//!
//! **This module has never been executed.** Not once, not partially. The
//! machine it was written on runs WiVRn, which — measured, see docs/OVERLAY.md
//! — *does* advertise `XR_EXTX_overlay`, but creating any session at all needs
//! the WiVRn compositor to be up with a headset attached, and the gate for this
//! work was explicitly "do not start an XR session on a machine somebody may be
//! wearing a headset on". Monado's standalone `monado-service`, which would
//! have given a runtime to test against with no hardware, is not installed.
//!
//! So: the probe is a measurement, `--feed` and `--render` are tested against
//! the real protocol, and everything below is code that compiles and has never
//! met a runtime. Treat the first run as a bring-up, not as a regression.
//!
//! ## Why it is shaped like this
//!
//! `XR_EXTX_overlay` is one struct: `XrSessionCreateInfoOverlayEXTX`, chained
//! onto `XrSessionCreateInfo`. A session created with it does not own the
//! frame — it submits layers that the runtime composites over whatever the main
//! application drew, and it does not get to say when a frame happens. That is
//! the whole extension, and it is why this is a small file with a large warning
//! on it rather than a renderer.
//!
//! The `openxr` crate's safe `create_session` builds the create-info itself and
//! offers no `next` hook, so the handle is made by hand here and handed back to
//! `Session::from_raw`. That is a real finding and not a workaround: any Rust
//! program that wants this extension has to do the same.
//!
//! Vulkan is `ash`, not wgpu. OpenXR hands out `VkImage` handles from its own
//! swapchain, so a renderer's job here would only ever be to get a buffer into
//! one of them — `vkCmdCopyBufferToImage` is that, in twenty lines, with no
//! `wgpu-hal` interop layer standing in between. The pixels come from
//! `raster.rs`, which is testable and tested.

use std::ptr;
use std::sync::mpsc::{Receiver, TryRecvError};

use anyhow::{Context, Result, bail};
// Both `as_raw`/`from_raw` families live on traits: `ash::vk::Handle` for the
// Vulkan handles and `openxr::sys::Handle` for the XR ones. Nothing here uses
// either trait's other methods; they are imported so a u64 can cross between
// the two APIs, which is the entire point of this module.
use ash::vk::{self, Handle as _};
use openxr::sys::Handle as _;

use crate::raster::{Renderer, Surface};

/// What the quad looks like in the world.
pub struct Placement {
    /// Metres in front of the eye (head-locked) or of the local origin.
    pub distance: f32,
    /// Metres wide at that distance. The height follows the texture's aspect.
    pub width: f32,
    /// Carry it on the head, or leave it in the room.
    pub world_locked: bool,
}

/// Everything the session needs that is not a caption.
pub struct Config {
    pub placement: Placement,
    pub font: Option<std::path::PathBuf>,
    pub size: f32,
    pub opacity: f32,
}

/// Open an overlay session and submit captions until the runtime says stop.
///
/// `surfaces` is fed by the caller's feed thread: every time the caption stack
/// changes, a freshly rasterised surface arrives. Between arrivals the same
/// texture is submitted unchanged, which is the right thing — a composition
/// layer has to be resubmitted every frame whether or not it moved.
pub fn run(cfg: Config, renderer: &Renderer, surfaces: Receiver<Surface>) -> Result<()> {
    let entry = unsafe { openxr::Entry::load() }.context("no OpenXR loader")?;
    let available = entry.enumerate_extensions()?;
    if !available.extx_overlay {
        bail!("this runtime does not advertise XR_EXTX_overlay");
    }
    if !available.khr_vulkan_enable2 {
        bail!("this runtime does not advertise XR_KHR_vulkan_enable2, which this binary needs");
    }

    let mut wanted = openxr::ExtensionSet::default();
    wanted.extx_overlay = true;
    wanted.khr_vulkan_enable2 = true;
    let xr = entry.create_instance(
        &openxr::ApplicationInfo {
            application_name: "nx-recall-overlay",
            application_version: 0,
            engine_name: "nx-recall",
            engine_version: 0,
            api_version: openxr::Version::new(1, 0, 0),
        },
        &wanted,
        &[],
    )?;
    let system = xr.system(openxr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
    // Mandatory before any Vulkan call in this sequence: the runtime will
    // refuse to create an instance for a caller that has not asked what it
    // supports (XR_KHR_vulkan_enable2).
    let reqs = xr.graphics_requirements::<openxr::Vulkan>(system)?;

    let vk = unsafe { Vk::new(&xr, system, reqs) }?;
    let session = unsafe { create_overlay_session(&xr, system, &vk) }?;
    let (session, mut waiter, mut stream) = session;

    // Head-locked by default: a caption you have to turn round to read is not a
    // caption. World-locked is for the case where you want to put it somewhere
    // and leave it, which is a different and rarer want.
    let space = session.create_reference_space(
        if cfg.placement.world_locked {
            openxr::ReferenceSpaceType::LOCAL
        } else {
            openxr::ReferenceSpaceType::VIEW
        },
        openxr::Posef::IDENTITY,
    )?;

    let style = renderer.style.clone();
    let mut swapchain = session.create_swapchain(&openxr::SwapchainCreateInfo {
        create_flags: openxr::SwapchainCreateFlags::EMPTY,
        // TRANSFER_DST because the only thing that ever touches these images is
        // a buffer copy; COLOR_ATTACHMENT because some runtimes require it on
        // anything they composite.
        usage_flags: openxr::SwapchainUsageFlags::COLOR_ATTACHMENT
            | openxr::SwapchainUsageFlags::TRANSFER_DST,
        format: vk::Format::R8G8B8A8_SRGB.as_raw() as u32,
        sample_count: 1,
        width: style.width,
        height: style.height,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    })?;
    let images: Vec<vk::Image> = swapchain
        .enumerate_images()?
        .into_iter()
        .map(vk::Image::from_raw)
        .collect();

    let mut upload = unsafe { Upload::new(&vk, style.width, style.height) }?;
    // The first frame must not be a black rectangle in somebody's face: an
    // empty surface is transparent everywhere, which is exactly the right thing
    // to submit until somebody says something.
    let mut current = renderer.render(&[], None);
    let mut have_new = true;

    let aspect = style.height as f32 / style.width as f32;
    let mut running = false;
    let mut event_storage = openxr::EventDataBuffer::new();

    loop {
        while let Some(event) = xr.poll_event(&mut event_storage)? {
            use openxr::Event::*;
            match event {
                SessionStateChanged(e) => match e.state() {
                    openxr::SessionState::READY => {
                        session.begin(openxr::ViewConfigurationType::PRIMARY_STEREO)?;
                        running = true;
                    }
                    openxr::SessionState::STOPPING => {
                        session.end()?;
                        running = false;
                    }
                    openxr::SessionState::EXITING | openxr::SessionState::LOSS_PENDING => {
                        return Ok(());
                    }
                    _ => {}
                },
                InstanceLossPending(_) => return Ok(()),
                _ => {}
            }
        }
        if !running {
            std::thread::sleep(std::time::Duration::from_millis(80));
            continue;
        }

        // Drain the feed: only the newest stack matters, and a slow frame must
        // not leave a backlog of surfaces nobody will ever see.
        loop {
            match surfaces.try_recv() {
                Ok(next) => {
                    current = next;
                    have_new = true;
                }
                Err(TryRecvError::Empty) => break,
                // The feed thread is gone — the daemon went away. Keep the last
                // captions up rather than blanking: they were true when they
                // were said, and a bar that empties itself on a socket hiccup
                // is a bar that looks broken.
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let state = waiter.wait()?;
        stream.begin()?;
        if !state.should_render {
            stream.end(
                state.predicted_display_time,
                openxr::EnvironmentBlendMode::OPAQUE,
                &[],
            )?;
            continue;
        }

        let index = swapchain.acquire_image()?;
        swapchain.wait_image(openxr::Duration::INFINITE)?;
        if have_new {
            unsafe { upload.copy(&vk, &current, images[index as usize]) }?;
            have_new = false;
        }
        swapchain.release_image()?;

        let quad = openxr::CompositionLayerQuad::new()
            .space(&space)
            .eye_visibility(openxr::EyeVisibility::BOTH)
            // The bar is composited over a scene somebody else drew, so it is
            // blended, never opaque: UNPREMULTIPLIED matches what raster.rs
            // produces (straight alpha, see Surface::put).
            .layer_flags(
                openxr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA
                    | openxr::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA,
            )
            .sub_image(
                openxr::SwapchainSubImage::new()
                    .swapchain(&swapchain)
                    .image_array_index(0)
                    .image_rect(openxr::Rect2Di {
                        offset: openxr::Offset2Di { x: 0, y: 0 },
                        extent: openxr::Extent2Di {
                            width: style.width as i32,
                            height: style.height as i32,
                        },
                    }),
            )
            .pose(openxr::Posef {
                orientation: openxr::Quaternionf::IDENTITY,
                position: openxr::Vector3f {
                    x: 0.0,
                    // Below the line of sight, not across it: the point is to
                    // read captions without losing the person you are talking
                    // to behind them.
                    y: -cfg.placement.width * aspect * 0.7,
                    z: -cfg.placement.distance,
                },
            })
            .size(openxr::Extent2Df {
                width: cfg.placement.width,
                height: cfg.placement.width * aspect,
            });

        stream.end(
            state.predicted_display_time,
            openxr::EnvironmentBlendMode::OPAQUE,
            &[&quad],
        )?;
    }
}

/// `xrCreateSession` with `XrSessionCreateInfoOverlayEXTX` in the chain.
///
/// By hand, because the `openxr` crate's `Vulkan::create_session` builds its own
/// `XrSessionCreateInfo` and exposes no way to extend it — which is the single
/// thing standing between this extension and the safe API.
///
/// # Safety
///
/// Every handle in `vk` must be live, and the caller must not create a second
/// session on the same instance.
unsafe fn create_overlay_session(
    xr: &openxr::Instance,
    system: openxr::SystemId,
    vk: &Vk,
) -> Result<(
    openxr::Session<openxr::Vulkan>,
    openxr::FrameWaiter,
    openxr::FrameStream<openxr::Vulkan>,
)> {
    use openxr::sys;

    let binding = sys::GraphicsBindingVulkanKHR {
        ty: sys::GraphicsBindingVulkanKHR::TYPE,
        next: ptr::null(),
        instance: vk.instance.handle().as_raw() as _,
        physical_device: vk.physical.as_raw() as _,
        device: vk.device.handle().as_raw() as _,
        queue_family_index: vk.queue_family,
        queue_index: 0,
    };
    let overlay = sys::SessionCreateInfoOverlayEXTX {
        ty: sys::SessionCreateInfoOverlayEXTX::TYPE,
        next: &binding as *const _ as *const _,
        create_flags: Default::default(),
        // Above the application's own layers. The captions are the thing the
        // user asked to see on top; a session that submitted them underneath
        // would be a session that submitted nothing.
        session_layers_placement: 1,
    };
    let info = sys::SessionCreateInfo {
        ty: sys::SessionCreateInfo::TYPE,
        next: &overlay as *const _ as *const _,
        create_flags: Default::default(),
        system_id: system,
    };
    let mut handle = sys::Session::NULL;
    let status = unsafe { (xr.fp().create_session)(xr.as_raw(), &info, &mut handle) };
    if status != sys::Result::SUCCESS {
        bail!("xrCreateSession with XR_EXTX_overlay failed: {status:?}");
    }
    Ok(unsafe { openxr::Session::from_raw(xr.clone(), handle, Box::new(())) })
}

/// The Vulkan device OpenXR told us to use.
///
/// Every handle here comes out of the runtime rather than out of `ash`: with
/// `XR_KHR_vulkan_enable2` the runtime creates the instance and the device on
/// the caller's behalf, so that it can add the extensions it needs. `ash` is
/// only the function loader.
struct Vk {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
}

impl Vk {
    /// # Safety
    ///
    /// `xr` and `system` must be live, and `graphics_requirements` must already
    /// have been called for this system.
    unsafe fn new(
        xr: &openxr::Instance,
        system: openxr::SystemId,
        reqs: openxr::vulkan::Requirements,
    ) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("no Vulkan loader")?;
        let app_name = c"nx-recall-overlay";
        let api = vk::make_api_version(
            0,
            reqs.min_api_version_supported.major() as u32,
            reqs.min_api_version_supported.minor() as u32,
            0,
        );
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(0)
            .engine_name(app_name)
            .engine_version(0)
            .api_version(api);
        let instance_ci = vk::InstanceCreateInfo::default().application_info(&app_info);

        let raw_instance = unsafe {
            xr.create_vulkan_instance(
                system,
                std::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "system" fn(
                        openxr::sys::platform::VkInstance,
                        *const std::os::raw::c_char,
                    ) -> Option<unsafe extern "system" fn()>,
                >(entry.static_fn().get_instance_proc_addr),
                &instance_ci as *const _ as *const _,
            )
        }?
        .map_err(|e| anyhow::anyhow!("the runtime could not create a VkInstance: {e}"))?;
        let instance = unsafe {
            ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(raw_instance as _),
            )
        };

        let physical = vk::PhysicalDevice::from_raw(unsafe {
            xr.vulkan_graphics_device(system, instance.handle().as_raw() as _)
        }? as _);

        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .enumerate()
            .find(|(_, p)| p.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .map(|(i, _)| i as u32)
            .context("the device OpenXR chose has no graphics queue")?;

        let priorities = [1.0f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let device_ci = vk::DeviceCreateInfo::default().queue_create_infos(&queue_ci);
        let raw_device = unsafe {
            xr.create_vulkan_device(
                system,
                std::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "system" fn(
                        openxr::sys::platform::VkInstance,
                        *const std::os::raw::c_char,
                    ) -> Option<unsafe extern "system" fn()>,
                >(entry.static_fn().get_instance_proc_addr),
                physical.as_raw() as _,
                &device_ci as *const _ as *const _,
            )
        }?
        .map_err(|e| anyhow::anyhow!("the runtime could not create a VkDevice: {e}"))?;
        let device = unsafe {
            ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(raw_device as _))
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };

        Ok(Self {
            _entry: entry,
            instance,
            physical,
            device,
            queue,
            queue_family,
            memory,
        })
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.memory.memory_type_count)
            .find(|i| {
                bits & (1 << i) != 0
                    && self.memory.memory_types[*i as usize]
                        .property_flags
                        .contains(want)
            })
            .context("no Vulkan memory type on this device is host-visible and coherent")
    }
}

/// One staging buffer, one command buffer, one fence: everything needed to get
/// a CPU surface into an OpenXR swapchain image, reused for the life of the run.
struct Upload {
    /// A clone of the device handle, held only so `Drop` can give the memory
    /// back. An overlay that leaked a mapped staging buffer every time the
    /// runtime asked it to stop would be a slow leak in a process people leave
    /// running all evening.
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    width: u32,
    height: u32,
}

impl Drop for Upload {
    fn drop(&mut self) {
        unsafe {
            // Everything recorded into this pool has already been waited on:
            // `copy` is synchronous on its fence.
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

impl Upload {
    /// # Safety
    ///
    /// `vk` must be live for as long as the returned value.
    unsafe fn new(vk: &Vk, width: u32, height: u32) -> Result<Self> {
        let size = (width as u64) * (height as u64) * 4;
        let buffer = unsafe {
            vk.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }?;
        let req = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let memory = unsafe {
            vk.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(vk.memory_type(
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )
        }?;
        unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) }?;
        let mapped = unsafe {
            vk.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        }? as *mut u8;

        let pool = unsafe {
            vk.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(vk.queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        let cmd = unsafe {
            vk.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let fence = unsafe {
            vk.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }?;

        Ok(Self {
            device: vk.device.clone(),
            buffer,
            memory,
            mapped,
            pool,
            cmd,
            fence,
            width,
            height,
        })
    }

    /// # Safety
    ///
    /// `image` must be an image this run acquired from the OpenXR swapchain and
    /// has not yet released.
    unsafe fn copy(&mut self, vk: &Vk, surface: &Surface, image: vk::Image) -> Result<()> {
        anyhow::ensure!(
            surface.width == self.width && surface.height == self.height,
            "a {}x{} surface arrived for a {}x{} swapchain",
            surface.width,
            surface.height,
            self.width,
            self.height
        );
        unsafe {
            ptr::copy_nonoverlapping(surface.pixels.as_ptr(), self.mapped, surface.pixels.len());

            vk.device.reset_fences(&[self.fence])?;
            vk.device
                .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())?;
            vk.device.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            // UNDEFINED as the old layout on purpose: the whole image is being
            // overwritten, so its previous contents are worth nothing and
            // saying so is what lets the driver skip a decompress.
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .image(image)
                .subresource_range(range);
            vk.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );

            vk.device.cmd_copy_buffer_to_image(
                self.cmd,
                self.buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: self.width,
                        height: self.height,
                        depth: 1,
                    })],
            );

            // Back to what the compositor expects to read. OpenXR requires the
            // image be in COLOR_ATTACHMENT_OPTIMAL when it is released.
            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_READ)
                .image(image)
                .subresource_range(range);
            vk.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );

            vk.device.end_command_buffer(self.cmd)?;
            let cmds = [self.cmd];
            vk.device.queue_submit(
                vk.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                self.fence,
            )?;
            // Synchronous, and deliberately: this runs at most once per turn —
            // a few times a minute — and a fence wait is a great deal simpler
            // to reason about than a pipeline nobody can test.
            vk.device
                .wait_for_fences(&[self.fence], true, 1_000_000_000)?;
        }
        Ok(())
    }
}

/// The font the overlay draws with, resolved the same way `--render` does.
pub fn renderer_for(cfg: &Config) -> Result<Renderer> {
    Renderer::new(
        crate::raster::Style {
            size: cfg.size,
            opacity: cfg.opacity,
            ..crate::raster::Style::default()
        },
        cfg.font.as_deref(),
    )
}
