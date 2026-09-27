//! Vulkan device for Flutter's Vulkan renderer (Impeller or Skia), used
//! instead of `gl` when the app asks for it.
//!
//! On the phone this talks to Turnip over `/dev/kgsl-3d0` directly, without
//! Zink in between. Flutter renders into one of two images; once Flutter has
//! waited for the GPU, the embedder copies the image into a host-visible
//! buffer on the GPU and from there into AERA's frame slot. Vulkan's origin
//! is top-left and the images are B8G8R8A8, so rows and bytes already match
//! AERA's layout.
//!
//! Flutter and the embedder share one queue, so `vkQueueSubmit` and
//! `vkQueueWaitIdle` are handed to Flutter wrapped in a lock.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use ash::vk;

const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

struct Target {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

/// Flutter's image layout after it has drawn a frame for presentation.
const PRESENTED: vk::ImageLayout = vk::ImageLayout::PRESENT_SRC_KHR;

static QUEUE_LOCK: Mutex<()> = Mutex::new(());
static REAL: OnceLock<RealFunctions> = OnceLock::new();

struct RealFunctions {
    get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
    queue_submit: vk::PFN_vkQueueSubmit,
    queue_wait_idle: vk::PFN_vkQueueWaitIdle,
    get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
}

/// Impeller's Linux build refuses to start unless the embedder says the
/// instance has a window-system surface (wayland, xcb or xlib) and the device
/// has VK_KHR_swapchain. It never makes a swapchain here, so these are only
/// named to Flutter, never enabled or called.
const REPORTED_INSTANCE_EXTENSIONS: [&CStr; 2] = [c"VK_KHR_surface", c"VK_KHR_wayland_surface"];
const REPORTED_DEVICE_EXTENSIONS: [&CStr; 1] = [c"VK_KHR_swapchain"];

unsafe extern "system" fn locked_queue_submit(
    queue: vk::Queue,
    count: u32,
    submits: *const vk::SubmitInfo,
    fence: vk::Fence,
) -> vk::Result {
    let _lock = QUEUE_LOCK.lock().unwrap();
    (REAL.get().unwrap().queue_submit)(queue, count, submits, fence)
}

unsafe extern "system" fn locked_queue_wait_idle(queue: vk::Queue) -> vk::Result {
    let _lock = QUEUE_LOCK.lock().unwrap();
    (REAL.get().unwrap().queue_wait_idle)(queue)
}

/// Flutter may look up queue functions through vkGetDeviceProcAddr too.
unsafe extern "system" fn wrapped_get_device_proc_addr(
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    wrapped(CStr::from_ptr(name)).or_else(|| (REAL.get().unwrap().get_device_proc_addr)(device, name))
}

/// Impeller fetches vkGetInstanceProcAddr once and resolves everything,
/// queue functions included, through it.
unsafe extern "system" fn wrapped_get_instance_proc_addr(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    wrapped(CStr::from_ptr(name)).or_else(|| (REAL.get().unwrap().get_instance_proc_addr)(instance, name))
}

fn wrapped(name: &CStr) -> vk::PFN_vkVoidFunction {
    unsafe {
        match name.to_bytes() {
            b"vkQueueSubmit" => Some(std::mem::transmute::<vk::PFN_vkQueueSubmit, unsafe extern "system" fn()>(
                locked_queue_submit,
            )),
            b"vkQueueWaitIdle" => Some(std::mem::transmute::<vk::PFN_vkQueueWaitIdle, unsafe extern "system" fn()>(
                locked_queue_wait_idle,
            )),
            b"vkGetInstanceProcAddr" => Some(std::mem::transmute::<vk::PFN_vkGetInstanceProcAddr, unsafe extern "system" fn()>(
                wrapped_get_instance_proc_addr,
            )),
            b"vkGetDeviceProcAddr" => Some(std::mem::transmute::<vk::PFN_vkGetDeviceProcAddr, unsafe extern "system" fn()>(
                wrapped_get_device_proc_addr,
            )),
            _ => None,
        }
    }
}

fn reported(enabled: &[CString], extra: &[&'static CStr]) -> Vec<*const c_char> {
    let mut names: Vec<*const c_char> = enabled.iter().map(|e| e.as_ptr()).collect();
    for name in extra {
        if !enabled.iter().any(|e| e.as_c_str() == *name) {
            names.push(name.as_ptr());
        }
    }
    names
}

struct Readback {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pixels: *const u8,
    pool: vk::CommandPool,
    commands: vk::CommandBuffer,
    fence: vk::Fence,
}

pub struct Vulkan {
    entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    api_version: u32,
    instance_extensions: Vec<CString>,
    device_extensions: Vec<CString>,
    targets: [Target; 2],
    readback: Readback,
    next: AtomicUsize,
    width: u32,
    height: u32,
}

// The raw pointers are mappings owned by this struct and only read.
unsafe impl Send for Vulkan {}
unsafe impl Sync for Vulkan {}

impl Vulkan {
    pub fn new(width: u32, height: u32) -> Result<Vulkan, String> {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("Vulkan loader: {e}"))?;
        let loader_version = unsafe { entry.try_enumerate_instance_version() }
            .ok()
            .flatten()
            .unwrap_or(vk::API_VERSION_1_0);
        if loader_version < vk::API_VERSION_1_1 {
            return Err("Vulkan 1.1 is required".into());
        }
        let application = vk::ApplicationInfo::default()
            .application_name(c"aera-flutter")
            .api_version(vk::API_VERSION_1_1.max(loader_version.min(vk::API_VERSION_1_3)));
        // Impeller refuses to start without VK_KHR_surface even though frames
        // never reach a real surface; enable it (and headless) when present.
        let available_instance: Vec<CString> = unsafe { entry.enumerate_instance_extension_properties(None) }
            .unwrap_or_default()
            .iter()
            .map(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }.to_owned())
            .collect();
        let wanted_instance: [&CStr; 2] = [c"VK_KHR_surface", c"VK_EXT_headless_surface"];
        let instance_extensions: Vec<CString> = wanted_instance
            .iter()
            .filter(|w| available_instance.iter().any(|a| a.as_c_str() == **w))
            .map(|w| (*w).to_owned())
            .collect();
        let instance_pointers: Vec<*const c_char> = instance_extensions.iter().map(|e| e.as_ptr()).collect();
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&application)
                    .enabled_extension_names(&instance_pointers),
                None,
            )
        }
        .map_err(|e| format!("vkCreateInstance: {e}"))?;

        let fail = |message: String| -> String {
            unsafe { instance.destroy_instance(None) };
            message
        };
        let physical = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| fail(format!("no Vulkan devices: {e}")))?
            .into_iter()
            .next()
            .ok_or_else(|| fail("no Vulkan devices".into()))?;
        let properties = unsafe { instance.get_physical_device_properties(physical) };
        let api_version = properties.api_version.min(application.api_version);
        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or_else(|| fail("no graphics queue".into()))? as u32;

        // Enable what Flutter can use when the device has it.
        let available: Vec<CString> = unsafe { instance.enumerate_device_extension_properties(physical) }
            .unwrap_or_default()
            .iter()
            .map(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }.to_owned())
            .collect();
        let wanted: [&CStr; 3] = [c"VK_KHR_swapchain", c"VK_EXT_pipeline_creation_feedback", c"VK_KHR_portability_subset"];
        let device_extensions: Vec<CString> =
            wanted.iter().filter(|w| available.iter().any(|a| a.as_c_str() == **w)).map(|w| (*w).to_owned()).collect();
        let extension_pointers: Vec<*const c_char> = device_extensions.iter().map(|e| e.as_ptr()).collect();
        let priorities = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities)];
        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&extension_pointers),
                None,
            )
        }
        .map_err(|e| fail(format!("vkCreateDevice: {e}")))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let _ = REAL.set(RealFunctions {
            get_device_proc_addr: instance.fp_v1_0().get_device_proc_addr,
            queue_submit: device.fp_v1_0().queue_submit,
            queue_wait_idle: device.fp_v1_0().queue_wait_idle,
            get_instance_proc_addr: entry.static_fn().get_instance_proc_addr,
        });

        let memory_properties = unsafe { instance.get_physical_device_memory_properties(physical) };
        let mut targets = Vec::new();
        for _ in 0..2 {
            match unsafe { make_target(&device, &memory_properties, width, height) } {
                Ok(target) => targets.push(target),
                Err(error) => {
                    unsafe {
                        for target in &targets {
                            destroy_target(&device, target);
                        }
                        device.destroy_device(None);
                        instance.destroy_instance(None);
                    }
                    return Err(error);
                }
            }
        }
        let targets: [Target; 2] = targets.try_into().ok().unwrap();
        let bytes = width as u64 * height as u64 * 4;
        let readback = match unsafe { make_readback(&device, &memory_properties, queue_family, bytes) } {
            Ok(readback) => readback,
            Err(error) => {
                unsafe {
                    for target in &targets {
                        destroy_target(&device, target);
                    }
                    device.destroy_device(None);
                    instance.destroy_instance(None);
                }
                return Err(error);
            }
        };
        Ok(Vulkan {
            entry,
            instance,
            physical,
            device,
            queue,
            queue_family,
            api_version,
            instance_extensions,
            device_extensions,
            targets,
            readback,
            next: AtomicUsize::new(0),
            width,
            height,
        })
    }

    pub fn device_name(&self) -> String {
        let properties = unsafe { self.instance.get_physical_device_properties(self.physical) };
        unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy().into_owned()
    }

    pub fn api_version(&self) -> u32 {
        self.api_version
    }

    pub fn handles(&self) -> (*mut c_void, *mut c_void, *mut c_void, u32, *mut c_void) {
        use ash::vk::Handle;
        (
            self.instance.handle().as_raw() as *mut c_void,
            self.physical.as_raw() as *mut c_void,
            self.device.handle().as_raw() as *mut c_void,
            self.queue_family,
            self.queue.as_raw() as *mut c_void,
        )
    }

    /// Instance extensions as named to Flutter: the enabled ones plus the
    /// surface extensions Impeller insists on.
    pub fn reported_instance_extensions(&self) -> Vec<*const c_char> {
        reported(&self.instance_extensions, &REPORTED_INSTANCE_EXTENSIONS)
    }

    pub fn reported_device_extensions(&self) -> Vec<*const c_char> {
        reported(&self.device_extensions, &REPORTED_DEVICE_EXTENSIONS)
    }

    pub fn instance_proc_address(&self, instance: *mut c_void, name: &CStr) -> *mut c_void {
        use ash::vk::Handle;
        if let Some(function) = wrapped(name) {
            return unsafe { std::mem::transmute::<vk::PFN_vkVoidFunction, *mut c_void>(Some(function)) };
        }
        let instance = vk::Instance::from_raw(instance as u64);
        unsafe {
            std::mem::transmute::<vk::PFN_vkVoidFunction, *mut c_void>(
                (self.entry.static_fn().get_instance_proc_addr)(instance, name.as_ptr()),
            )
        }
    }

    /// The image Flutter draws the next frame into, and its format.
    pub fn next_image(&self) -> (u64, u32) {
        use ash::vk::Handle;
        let index = self.next.fetch_add(1, Ordering::Relaxed) % 2;
        (self.targets[index].image.as_raw(), FORMAT.as_raw() as u32)
    }

    /// Copies a finished image into `out` as AERA's top-down BGRA frame.
    /// Flutter has already waited for its own GPU work before presenting.
    pub fn read_frame(&self, image: u64, out: &mut [u8]) -> Result<(), String> {
        use ash::vk::Handle;
        let image = vk::Image::from_raw(image);
        if !self.targets.iter().any(|t| t.image == image) {
            return Err("Flutter presented an image this embedder does not own".into());
        }
        let bytes = self.width as usize * self.height as usize * 4;
        assert!(out.len() >= bytes);
        let r = &self.readback;
        let d = &self.device;
        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_copy = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(PRESENTED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color);
        let back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(PRESENTED)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color);
        let host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(r.buffer)
            .size(vk::WHOLE_SIZE);
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
            .image_extent(vk::Extent3D { width: self.width, height: self.height, depth: 1 });
        let failed = |what: &str, e: vk::Result| format!("frame readback: {what}: {e}");
        unsafe {
            d.reset_command_buffer(r.commands, vk::CommandBufferResetFlags::empty()).map_err(|e| failed("reset", e))?;
            d.begin_command_buffer(
                r.commands,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|e| failed("begin", e))?;
            d.cmd_pipeline_barrier(
                r.commands,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_copy],
            );
            d.cmd_copy_image_to_buffer(r.commands, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, r.buffer, &[region]);
            d.cmd_pipeline_barrier(
                r.commands,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[host],
                &[back],
            );
            d.end_command_buffer(r.commands).map_err(|e| failed("end", e))?;
            let commands = [r.commands];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            {
                let _lock = QUEUE_LOCK.lock().unwrap();
                d.queue_submit(self.queue, &[submit], r.fence).map_err(|e| failed("submit", e))?;
            }
            d.wait_for_fences(&[r.fence], true, u64::MAX).map_err(|e| failed("wait", e))?;
            d.reset_fences(&[r.fence]).map_err(|e| failed("reset fence", e))?;
            // Harmless on coherent memory; required on cached, non-coherent memory.
            let _ = d.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                .memory(r.memory)
                .size(vk::WHOLE_SIZE)]);
            std::ptr::copy_nonoverlapping(r.pixels, out.as_mut_ptr(), bytes);
        }
        Ok(())
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for target in &self.targets {
                destroy_target(&self.device, target);
            }
            let r = &self.readback;
            self.device.destroy_fence(r.fence, None);
            self.device.destroy_command_pool(r.pool, None);
            self.device.unmap_memory(r.memory);
            self.device.destroy_buffer(r.buffer, None);
            self.device.free_memory(r.memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

unsafe fn make_target(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
) -> Result<Target, String> {
    let image = device
        .create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(FORMAT)
                .extent(vk::Extent3D { width, height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(
                    vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::TRANSFER_SRC
                        | vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::SAMPLED,
                )
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
        .map_err(|e| format!("B8G8R8A8 render target: {e}"))?;
    let requirements = device.get_image_memory_requirements(image);
    let Some(memory_type) = pick_memory(memory_properties, requirements.memory_type_bits, &[
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        vk::MemoryPropertyFlags::empty(),
    ]) else {
        device.destroy_image(image, None);
        return Err("no memory for the render target".into());
    };
    let memory = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(memory_type),
        None,
    ) {
        Ok(memory) => memory,
        Err(e) => {
            device.destroy_image(image, None);
            return Err(format!("render target memory: {e}"));
        }
    };
    if let Err(e) = device.bind_image_memory(image, memory, 0) {
        device.free_memory(memory, None);
        device.destroy_image(image, None);
        return Err(format!("bind render target: {e}"));
    }
    Ok(Target { image, memory })
}

fn pick_memory(
    properties: &vk::PhysicalDeviceMemoryProperties,
    allowed: u32,
    preferences: &[vk::MemoryPropertyFlags],
) -> Option<u32> {
    preferences.iter().find_map(|&flags| {
        (0..properties.memory_type_count).find(|&i| {
            allowed & (1 << i) != 0 && properties.memory_types[i as usize].property_flags.contains(flags)
        })
    })
}

unsafe fn make_readback(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    queue_family: u32,
    bytes: u64,
) -> Result<Readback, String> {
    let buffer = device
        .create_buffer(
            &vk::BufferCreateInfo::default().size(bytes).usage(vk::BufferUsageFlags::TRANSFER_DST),
            None,
        )
        .map_err(|e| format!("readback buffer: {e}"))?;
    let requirements = device.get_buffer_memory_requirements(buffer);
    // Reading uncached memory is very slow on ARM CPUs; prefer cached.
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE;
    let memory_type = pick_memory(memory_properties, requirements.memory_type_bits, &[
        host | vk::MemoryPropertyFlags::HOST_CACHED,
        host,
    ])
    .ok_or("no host-visible memory for frame readback")?;
    let memory = device
        .allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(memory_type),
            None,
        )
        .map_err(|e| format!("readback memory: {e}"))?;
    device.bind_buffer_memory(buffer, memory, 0).map_err(|e| format!("bind readback buffer: {e}"))?;
    let pixels = device
        .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
        .map_err(|e| format!("map readback buffer: {e}"))? as *const u8;
    let pool = device
        .create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
        .map_err(|e| format!("command pool: {e}"))?;
    let commands = device
        .allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1),
        )
        .map_err(|e| format!("command buffer: {e}"))?[0];
    let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(|e| format!("fence: {e}"))?;
    Ok(Readback { buffer, memory, pixels, pool, commands, fence })
}

unsafe fn destroy_target(device: &ash::Device, target: &Target) {
    device.destroy_image(target.image, None);
    device.free_memory(target.memory, None);
}
