//! Process-global GPU + font resources shared across all terminal windows.
//!
//! A terminal editor can hold many windows at once; each is its own native
//! terminal in the registry but they all render through one wgpu `Device`/
//! `Queue` and one `FontSystem`. Sharing keeps opening the Nth window cheap
//! (only a fresh IOSurface target is allocated) and lets the glyph atlas be
//! warmed once. All of this lives in `OnceLock`s so it survives — like the
//! terminal registry — across Unity C# domain reloads.

use glyphon::cosmic_text::{Fallback, PlatformFallback};
use glyphon::{fontdb, Cache, FontSystem};
use std::sync::{Mutex, OnceLock};
use unicode_script::Script;

/// sRGB target so Unity's external texture (created with `linear=false`)
/// hardware-decodes on sample. The Metal IOSurface uses `RGBA8Unorm_sRGB`.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Shared glyphon cache (pipelines/bind layouts) for this device.
    pub cache: Cache,
}

fn init_gpu() -> Gpu {
    // Windows must run on D3D12: the zero-copy surface opens wgpu's render target
    // on Unity's device via `as_hal::<Dx12>` (see surface::d3d), which only works
    // if wgpu itself picked the D3D12 backend (PRIMARY could otherwise pick
    // Vulkan). Elsewhere, PRIMARY (Metal on macOS) is correct.
    #[cfg(windows)]
    let backends = wgpu::Backends::DX12;
    #[cfg(not(windows))]
    let backends = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let (device, queue) = open_device(&instance);

    let cache = Cache::new(&device);
    Gpu { device, queue, cache }
}

/// The device descriptor shared by every adapter open. Uses the adapter's real
/// limits, not downlevel defaults — the latter cap `max_texture_dimension_2d` at
/// 2048, so a window wider/taller than that failed to build its render target
/// (panicking the create, which left the terminal showing only "ready").
fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("unterm-device"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }
}

/// Open the render device. Off macOS, take the high-performance adapter (Windows
/// matches Unity's adapter inside `pick_adapter`).
#[cfg(not(target_os = "macos"))]
fn open_device(instance: &wgpu::Instance) -> (wgpu::Device, wgpu::Queue) {
    let adapter = pick_adapter(instance);
    pollster::block_on(adapter.request_device(&device_descriptor(&adapter)))
        .expect("unterm: failed to create device")
}

/// Open the render device on macOS, on the editor's own `MTLDevice`/queue.
///
/// The IOSurface target is a Metal texture created on *this* wgpu device, then
/// handed to Unity for `CreateExternalTexture`; a texture is bound to its origin
/// device, so the two must be the same GPU. So we build wgpu directly on the
/// device (and command queue) Unity captured at `UnityPluginLoad` and bridged in
/// (see lib.rs `unity_metal`). With no captured device (headless tests, or Unity
/// not ready yet), fall back to the default adapter.
#[cfg(target_os = "macos")]
fn open_device(instance: &wgpu::Instance) -> (wgpu::Device, wgpu::Queue) {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("unterm: no suitable GPU adapter");

    let device_ptr = crate::shadow_device_ptr();
    if device_ptr.is_null() {
        return pollster::block_on(adapter.request_device(&device_descriptor(&adapter)))
            .expect("unterm: failed to create device");
    }

    // `create_device_from_hal` uses the OpenDevice's device/queue (not the
    // adapter's), so the adapter only serves as the backend/parent handle here.
    let open = unsafe { unity_open_device(device_ptr, crate::shadow_queue_ptr()) };
    log::info!("unterm: rendering on the editor's own MTLDevice");
    unsafe { adapter.create_device_from_hal::<wgpu::hal::api::Metal>(open, &device_descriptor(&adapter)) }
        .expect("unterm: failed to create device from Unity's MTLDevice")
}

/// Build a wgpu-hal `OpenDevice` from Unity's raw `id<MTLDevice>` and (optional)
/// `id<MTLCommandQueue>`. We retain both (the Obj-C runtime is process-global, so
/// retaining a pointer made in the original image is fine); wgpu then owns the
/// retains for the device's lifetime.
#[cfg(target_os = "macos")]
unsafe fn unity_open_device(
    device_ptr: *mut std::ffi::c_void,
    queue_ptr: *mut std::ffi::c_void,
) -> wgpu::hal::OpenDevice<wgpu::hal::api::Metal> {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{MTLCommandQueue, MTLDevice};

    let device: Retained<ProtocolObject<dyn MTLDevice>> =
        Retained::retain(device_ptr.cast()).expect("unterm: null Unity MTLDevice");

    let queue: Retained<ProtocolObject<dyn MTLCommandQueue>> = if queue_ptr.is_null() {
        device
            .newCommandQueue()
            .expect("unterm: newCommandQueue failed")
    } else {
        Retained::retain(queue_ptr.cast()).expect("unterm: null Unity MTLCommandQueue")
    };

    // unterm never issues GPU timestamp queries, so this period is cosmetic; 1.0
    // (ns/tick) is correct for Apple Silicon and AMD (Intel would be 83.333).
    let timestamp_period = 1.0;

    wgpu::hal::OpenDevice {
        device: wgpu::hal::metal::Device::device_from_raw(device, wgpu::Features::empty()),
        queue: wgpu::hal::metal::Queue::queue_from_raw(queue, timestamp_period),
    }
}

/// Pick the GPU adapter. On Windows the zero-copy surface shares a texture handle
/// between wgpu's device and Unity's, which only works on the *same* physical
/// adapter — opening the handle on a different adapter (a discrete vs integrated
/// GPU) yields a garbled image. So match Unity's adapter by PCI vendor/device id
/// (captured in `UnityPluginLoad`); fall back to high-performance if unknown or
/// unmatched. Elsewhere there's no cross-device sharing, so just take the
/// high-performance adapter.
#[cfg(not(target_os = "macos"))]
fn pick_adapter(instance: &wgpu::Instance) -> wgpu::Adapter {
    #[cfg(windows)]
    if let Some((vendor, device)) = crate::unity::unity_adapter_ids() {
        for a in pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12)) {
            let info = a.get_info();
            if info.vendor == vendor && info.device == device {
                log::info!(
                    "unterm: matched wgpu adapter to Unity's: {} (vendor:0x{vendor:04x} device:0x{device:04x})",
                    info.name
                );
                return a;
            }
        }
        log::warn!(
            "unterm: no wgpu adapter matched Unity's (vendor:0x{vendor:04x} device:0x{device:04x}); \
             zero-copy may garble across adapters"
        );
    } else {
        log::warn!("unterm: Unity adapter unknown at GPU init; using high-performance adapter");
    }

    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("unterm: no suitable GPU adapter")
}

/// The shared GPU context, created on first use.
pub fn gpu() -> &'static Gpu {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    GPU.get_or_init(init_gpu)
}

/// The shared font database. Locked briefly during layout/render.
pub fn font_system() -> &'static Mutex<FontSystem> {
    static FS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    FS.get_or_init(|| {
        // Mirror cosmic-text's `FontSystem::new()` database setup, but install a
        // custom CJK fallback (see `LocaleFallback`).
        let mut db = fontdb::Database::new();
        db.set_monospace_family("Fira Mono");
        db.set_sans_serif_family("Fira Sans");
        db.set_serif_family("DejaVu Serif");
        db.load_system_fonts();
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
        Mutex::new(FontSystem::new_with_locale_and_db_and_fallback(
            locale,
            db,
            LocaleFallback,
        ))
    })
}

/// cosmic-text's built-in Han-unification fallback matches the locale *exactly*
/// (`ja`/`ko`/`zh-HK`/`zh-TW`), but `sys_locale` returns region-tagged values like
/// `ja-JP` that never match — so CJK ideographs wrongly fall back to a Chinese
/// font. This wraps the platform fallback and normalizes the locale first (the
/// region is kept only for Chinese, where it picks Traditional vs Simplified).
struct LocaleFallback;

impl Fallback for LocaleFallback {
    fn common_fallback(&self) -> &[&'static str] {
        PlatformFallback.common_fallback()
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        PlatformFallback.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        PlatformFallback.script_fallback(script, &normalize_locale(locale))
    }
}

/// Reduce a locale to the subtag cosmic-text's Han-unification fallback matches on:
/// the primary language for everything except Chinese, where the region is kept
/// (`zh-TW`/`zh-HK` = Traditional, anything else = Simplified).
fn normalize_locale(locale: &str) -> String {
    let primary = locale.split(['-', '_']).next().unwrap_or("en");
    if primary.eq_ignore_ascii_case("zh") {
        let upper = locale.to_ascii_uppercase();
        if upper.contains("HK") {
            "zh-HK".to_string()
        } else if upper.contains("TW") || upper.contains("HANT") {
            "zh-TW".to_string()
        } else {
            "zh-CN".to_string() // Simplified — cosmic-text's default arm
        }
    } else {
        primary.to_ascii_lowercase()
    }
}

/// Whether a font spec is a file path to load from disk (vs. a family name that's
/// already in the shared [`font_system`], e.g. a system UI font referenced by name).
pub fn is_font_path(s: &str) -> bool {
    s.contains('/')
        || s.contains('\\')
        || s.ends_with(".ttf")
        || s.ends_with(".ttc")
        || s.ends_with(".otf")
}
