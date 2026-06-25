//! Process-global GPU + font resources shared across all terminal windows.
//!
//! A terminal editor can hold many windows at once; each is its own native
//! terminal in the registry but they all render through one wgpu `Device`/
//! `Queue` and one `FontSystem`. Sharing keeps opening the Nth window cheap
//! (only a fresh IOSurface target is allocated) and lets the glyph atlas be
//! warmed once. All of this lives in `OnceLock`s so it survives — like the
//! terminal registry — across Unity C# domain reloads.

use glyphon::{fontdb, Cache, FontSystem};
use std::sync::{Mutex, OnceLock};

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
        ..Default::default()
    });
    let adapter = pick_adapter(&instance);

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("unterm-device"),
            required_features: wgpu::Features::empty(),
            // Use the adapter's real limits, not downlevel defaults — the latter cap
            // max_texture_dimension_2d at 2048, so a window wider/taller than that
            // failed to build its render target (panicking the create, which left the
            // terminal showing only "ready").
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
        },
        None,
    ))
    .expect("unterm: failed to create device");

    let cache = Cache::new(&device);
    Gpu { device, queue, cache }
}

/// Pick the GPU adapter. On Windows the zero-copy surface shares a texture handle
/// between wgpu's device and Unity's, which only works on the *same* physical
/// adapter — opening the handle on a different adapter (a discrete vs integrated
/// GPU) yields a garbled image. So match Unity's adapter by PCI vendor/device id
/// (captured in `UnityPluginLoad`); fall back to high-performance if unknown or
/// unmatched. Elsewhere there's no cross-device sharing, so just take the
/// high-performance adapter.
fn pick_adapter(instance: &wgpu::Instance) -> wgpu::Adapter {
    #[cfg(windows)]
    if let Some((vendor, device)) = crate::unity::unity_adapter_ids() {
        for a in instance.enumerate_adapters(wgpu::Backends::DX12) {
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
        // Mirror cosmic-text's `FontSystem::new()` database setup, but build it with
        // a normalized locale so its Han-unification fallback resolves CJK ideographs
        // to the right regional font (see `normalized_locale`).
        let mut db = fontdb::Database::new();
        db.set_monospace_family("Fira Mono");
        db.set_sans_serif_family("Fira Sans");
        db.set_serif_family("DejaVu Serif");
        db.load_system_fonts();
        let locale = normalized_locale();
        Mutex::new(FontSystem::new_with_locale_and_db(locale, db))
    })
}

/// The system locale, normalized for cosmic-text's exact-match Han-unification
/// fallback. cosmic-text keys CJK ideograph fallback off the bare language for
/// `ja`/`ko`, but keeps the region only for Chinese (`zh-HK`/`zh-TW`); `sys_locale`
/// returns region-tagged values like `ja-JP`, which never match, so kanji wrongly
/// fall back to a Chinese font. Reduce to the primary subtag, except for Chinese
/// where the region (Traditional vs Simplified) must be preserved.
fn normalized_locale() -> String {
    let raw = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
    let primary = raw.split(['-', '_']).next().unwrap_or("en");
    if primary.eq_ignore_ascii_case("zh") {
        let upper = raw.to_ascii_uppercase();
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
