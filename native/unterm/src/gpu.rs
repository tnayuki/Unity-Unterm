//! Process-global GPU + font resources shared across all terminal windows.
//!
//! A terminal editor can hold many windows at once; each is its own native
//! terminal in the registry but they all render through one wgpu `Device`/
//! `Queue` and one `FontSystem`. Sharing keeps opening the Nth window cheap
//! (only a fresh IOSurface target is allocated) and lets the glyph atlas be
//! warmed once. All of this lives in `OnceLock`s so it survives — like the
//! terminal registry — across Unity C# domain reloads.

use glyphon::{Cache, FontSystem};
use std::sync::{Mutex, OnceLock};
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLDevice as ObjcMTLDevice;
use objc2_metal::MTLCommandQueue as ObjcMTLCommandQueue;
use foreign_types::ForeignType;

/// sRGB target so Unity's external texture (created with `linear=false`)
/// hardware-decodes on sample. The Metal IOSurface uses `RGBA8Unorm_sRGB`.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Shared glyphon cache (pipelines/bind layouts) for this device.
    pub cache: Cache,
}

/// Create an owned metal::Device from a borrowed raw pointer.
///
/// Ownership chain (no direct retain/release calls):
///   Retained::retain(ptr)  — +1 refcount via objc2 smart pointer
///   Retained::into_raw()   — extract pointer without Drop (keeps +1)
///   Device::from_ptr()     — transfer ownership to metal-rs (Drop will -1)
unsafe fn metal_device_from_raw(ptr: *mut c_void) -> metal::Device {
    let retained =
        Retained::<ProtocolObject<dyn ObjcMTLDevice>>::retain(ptr as *mut _)
            .expect("unterm: null MTLDevice pointer");
    let raw = Retained::into_raw(retained);
    metal::Device::from_ptr(raw as *mut _)
}

/// Same ownership transfer pattern for MTLCommandQueue.
unsafe fn metal_queue_from_raw(ptr: *mut c_void) -> metal::CommandQueue {
    let retained =
        Retained::<ProtocolObject<dyn ObjcMTLCommandQueue>>::retain(ptr as *mut _)
            .expect("unterm: null MTLCommandQueue pointer");
    let raw = Retained::into_raw(retained);
    metal::CommandQueue::from_ptr(raw as *mut _)
}

fn init_gpu() -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("unterm: no suitable GPU adapter");

    let dev_ptr = crate::shadow_device_ptr();
    let queue_ptr = crate::shadow_queue_ptr();

    let (device, queue) = if !dev_ptr.is_null() {
        let raw_device = unsafe { metal_device_from_raw(dev_ptr) };
        let raw_queue = if !queue_ptr.is_null() {
            unsafe { metal_queue_from_raw(queue_ptr) }
        } else {
            raw_device.new_command_queue()
        };

        // Timestamp period copied from wgpu-hal's Metal adapter.rs:
        // Intel GPUs use 83.333ns/tick, Apple Silicon and AMD use 1.0ns/tick.
        let timestamp_period = if raw_device.name().starts_with("Intel") {
            83.333
        } else {
            1.0
        };

        let features = wgpu::Features::empty();
        let hal_device =
            unsafe { wgpu::hal::metal::Device::device_from_raw(raw_device, features) };
        let hal_queue =
            unsafe { wgpu::hal::metal::Queue::queue_from_raw(raw_queue, timestamp_period) };

        let open_device = wgpu::hal::OpenDevice {
            device: hal_device,
            queue: hal_queue,
        };

        unsafe {
            adapter.create_device_from_hal::<wgpu::hal::api::Metal>(
                open_device,
                &wgpu::DeviceDescriptor {
                    label: Some("unterm-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
        }
        .expect("unterm: failed to create device from Unity's MTLDevice")
    } else {
        // Fallback: standard wgpu initialization (standalone / headless tests)
        pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("unterm-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .expect("unterm: failed to create device")
    };

    let cache = Cache::new(&device);
    Gpu { device, queue, cache }
}

/// The shared GPU context, created on first use.
pub fn gpu() -> &'static Gpu {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    GPU.get_or_init(init_gpu)
}

/// The shared font database. Locked briefly during layout/render.
pub fn font_system() -> &'static Mutex<FontSystem> {
    static FS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    FS.get_or_init(|| Mutex::new(FontSystem::new()))
}

