//! Unity native-plugin integration: capture the editor's graphics device at
//! `UnityPluginLoad` so the zero-copy renderer can build wgpu on Unity's own GPU.
//!
//! Metal on macOS, D3D11/D3D12 on Windows — both via the [`unity_native_plugin`]
//! crate (one plugin entry point, the device capture branches per platform). The
//! host binds to the SAME image Unity loads the plugin into, so the captured
//! device is read here in-process; no cross-image bridge is needed.
//!
//! (Device + queue sharing originated in @aosoft's PR #1; the macOS shadow-copy
//! split it bridged across was later dropped, and the two platforms' capture was
//! consolidated onto @aosoft's `unity-native-plugin` crate.)

use std::sync::OnceLock;

use unity_native_plugin::interface::UnityInterfaces;

static UNITY_INTERFACES: OnceLock<&'static UnityInterfaces> = OnceLock::new();

unity_native_plugin::unity_native_plugin_entry_point! {
    fn unity_plugin_load(interfaces: &'static UnityInterfaces) {
        crate::init_log();
        let _ = UNITY_INTERFACES.set(interfaces);
        #[cfg(windows)]
        windows_gfx::on_plugin_load(interfaces);
    }
    fn unity_plugin_unload() {
        #[cfg(windows)]
        windows_gfx::on_plugin_unload();
    }
}

// ============================== macOS (Metal) ==============================
#[cfg(target_os = "macos")]
mod metal {
    use super::UNITY_INTERFACES;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{MTLCommandQueue, MTLDevice};
    use unity_native_plugin::metal::{
        UnityGraphicsMetalV1, UnityGraphicsMetalV1Interface, UnityGraphicsMetalV2,
        UnityGraphicsMetalV2Interface,
    };

    /// The editor's Metal device, captured at `UnityPluginLoad` (`None` until
    /// Unity's graphics are up, e.g. in headless tests). Read directly by
    /// `gpu::init_gpu` — the renderer shares this image, so no bridge is needed.
    pub fn unity_device() -> Option<Retained<ProtocolObject<dyn MTLDevice>>> {
        let interfaces = UNITY_INTERFACES.get()?;
        if let Some(metal) = interfaces.interface::<UnityGraphicsMetalV2>() {
            if let Some(device) = metal.metal_device() {
                return Some(device);
            }
        }
        if let Some(metal) = interfaces.interface::<UnityGraphicsMetalV1>() {
            if let Some(device) = metal.metal_device() {
                return Some(device);
            }
        }
        None
    }

    /// The editor's command queue (only the V2 interface exposes it; `gpu` makes
    /// its own queue on the device when absent).
    pub fn unity_queue() -> Option<Retained<ProtocolObject<dyn MTLCommandQueue>>> {
        let interfaces = UNITY_INTERFACES.get()?;
        if let Some(metal) = interfaces.interface::<UnityGraphicsMetalV2>() {
            return metal.command_queue();
        }
        None
    }
}

#[cfg(target_os = "macos")]
pub use metal::{unity_device, unity_queue};

/// Touched by the C# loader (`[DllImport]`) to make Unity load the `.dylib` as a
/// native plugin and run `UnityPluginLoad` (capturing the editor's device) before
/// the renderer binds. Returns whether the device is captured.
#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "C" fn unterm_unity_metal_init() -> bool {
    unity_device().is_some()
}

// ========================== Windows (D3D11 / D3D12) ==========================
#[cfg(windows)]
mod windows_gfx {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};

    use unity_native_plugin::d3d11::{UnityGraphicsD3D11, UnityGraphicsD3D11Interface};
    use unity_native_plugin::d3d12::{
        UnityGraphicsD3D12v2, UnityGraphicsD3D12v2Interface, UnityGraphicsD3D12v3,
        UnityGraphicsD3D12v4, UnityGraphicsD3D12v5, UnityGraphicsD3D12v6, UnityGraphicsD3D12v7,
        UnityGraphicsD3D12v8,
    };
    use unity_native_plugin::graphics::{
        GfxDeviceEventType, GfxRenderer, GraphicsDeviceEventCallback, UnityGraphics,
        UnityGraphicsInterface,
    };
    use unity_native_plugin::interface::UnityInterfaces;

    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::ID3D11Device;
    use windows::Win32::Graphics::Direct3D12::ID3D12Device;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory4, DXGI_ADAPTER_DESC,
    };

    /// `UnityGfxRenderer` values exposed to the surface code as raw `i32` (so
    /// `surface::d3d` and the C# probe don't depend on the crate's enum).
    /// `KIND_NONE` is our own 0 sentinel for "no device captured" (distinct from
    /// any real renderer), the initial value of `GFX_KIND`.
    pub const KIND_NONE: i32 = 0;
    pub const KIND_D3D11: i32 = GfxRenderer::D3D11 as i32;
    pub const KIND_D3D12: i32 = GfxRenderer::D3D12 as i32;

    static GFX_KIND: AtomicI32 = AtomicI32::new(KIND_NONE);
    static DEVICE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    // PCI vendor/device id of the adapter Unity's device runs on. wgpu must pick
    // the SAME physical adapter (see gpu.rs): a shared texture handle opened across
    // two adapters (discrete vs integrated GPU) yields a garbled image.
    static ADAPTER_VENDOR: AtomicU32 = AtomicU32::new(0);
    static ADAPTER_DEVICE: AtomicU32 = AtomicU32::new(0);
    static ADAPTER_KNOWN: AtomicBool = AtomicBool::new(false);

    /// Called from `UnityPluginLoad`: register for device init/reset so we capture
    /// even if graphics aren't ready yet (a terminal restored during early editor
    /// startup), then capture now in case the device already exists (Unity won't
    /// re-fire Initialize for an already-created device).
    pub fn on_plugin_load(interfaces: &'static UnityInterfaces) {
        if let Some(gfx) = interfaces.interface::<UnityGraphics>() {
            gfx.register_device_event_callback(Some(
                on_gfx_device_event as GraphicsDeviceEventCallback,
            ));
        }
        capture_device();
    }

    pub fn on_plugin_unload() {
        DEVICE.store(std::ptr::null_mut(), Ordering::Release);
        GFX_KIND.store(KIND_NONE, Ordering::Release);
    }

    /// Unity graphics device lifecycle callback: (re)capture on init/reset, drop
    /// on shutdown.
    extern "system" fn on_gfx_device_event(event_type: GfxDeviceEventType) {
        match event_type {
            GfxDeviceEventType::Initialize | GfxDeviceEventType::AfterReset => capture_device(),
            GfxDeviceEventType::Shutdown | GfxDeviceEventType::BeforeReset => {
                DEVICE.store(std::ptr::null_mut(), Ordering::Release);
            }
        }
    }

    /// Fetch and store Unity's current D3D device (and its adapter). A null device
    /// (graphics not ready) is fine — the device-event callback retries on init.
    fn capture_device() {
        let Some(interfaces) = super::UNITY_INTERFACES.get() else {
            return;
        };
        let Some(gfx) = interfaces.interface::<UnityGraphics>() else {
            return;
        };
        let kind = gfx.renderer();
        let device = match kind {
            GfxRenderer::D3D11 => interfaces
                .interface::<UnityGraphicsD3D11>()
                .map_or(std::ptr::null_mut(), |d| unsafe { d.device() }),
            GfxRenderer::D3D12 => fetch_d3d12_device(interfaces),
            _ => std::ptr::null_mut(),
        };

        GFX_KIND.store(kind as i32, Ordering::Release);
        DEVICE.store(device, Ordering::Release);

        if !device.is_null() {
            if let Some((vendor, dev_id)) = unsafe { adapter_ids(kind, device) } {
                ADAPTER_VENDOR.store(vendor, Ordering::Release);
                ADAPTER_DEVICE.store(dev_id, Ordering::Release);
                ADAPTER_KNOWN.store(true, Ordering::Release);
                log::info!(
                    "unterm: captured Unity device kind={kind:?} adapter=vendor:0x{vendor:04x} device:0x{dev_id:04x}"
                );
            } else {
                log::warn!("unterm: captured Unity device kind={kind:?} but adapter id unavailable");
            }
        } else {
            log::info!("unterm: Unity graphics device not ready yet (kind={kind:?}); will retry on init");
        }
    }

    /// Every `IUnityGraphicsD3D12vN` exposes `GetDevice()`, so take the newest the
    /// runtime provides.
    fn fetch_d3d12_device(interfaces: &UnityInterfaces) -> *mut c_void {
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v8>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v7>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v6>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v5>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v4>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v3>() {
            return unsafe { d.device() };
        }
        if let Some(d) = interfaces.interface::<UnityGraphicsD3D12v2>() {
            return unsafe { d.device() };
        }
        std::ptr::null_mut()
    }

    /// Resolve the (vendor_id, device_id) of the adapter behind Unity's device.
    /// D3D11: via `IDXGIDevice::GetAdapter`. D3D12: look the device's LUID up
    /// through a DXGI factory. Returns `None` on any COM failure.
    unsafe fn adapter_ids(kind: GfxRenderer, device: *mut c_void) -> Option<(u32, u32)> {
        let d: DXGI_ADAPTER_DESC = match kind {
            GfxRenderer::D3D11 => {
                let dev: &ID3D11Device = ID3D11Device::from_raw_borrowed(&device)?;
                let dxgi_dev: IDXGIDevice = dev.cast().ok()?;
                let adapter: IDXGIAdapter = dxgi_dev.GetAdapter().ok()?;
                adapter.GetDesc().ok()?
            }
            GfxRenderer::D3D12 => {
                let dev: &ID3D12Device = ID3D12Device::from_raw_borrowed(&device)?;
                let luid = dev.GetAdapterLuid();
                let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
                let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(luid).ok()?;
                adapter.GetDesc().ok()?
            }
            _ => return None,
        };
        Some((d.VendorId, d.DeviceId))
    }

    /// The PCI vendor/device id of the adapter behind Unity's captured device, or
    /// `None` until `UnityPluginLoad` has resolved it.
    pub fn unity_adapter_ids() -> Option<(u32, u32)> {
        if ADAPTER_KNOWN.load(Ordering::Acquire) {
            Some((
                ADAPTER_VENDOR.load(Ordering::Acquire),
                ADAPTER_DEVICE.load(Ordering::Acquire),
            ))
        } else {
            None
        }
    }

    /// The captured Unity device (null until `UnityPluginLoad` runs).
    pub fn unity_device() -> *mut c_void {
        DEVICE.load(Ordering::Acquire)
    }

    /// The captured renderer kind ([`KIND_D3D11`] / [`KIND_D3D12`] / [`KIND_NONE`]).
    pub fn unity_gfx_kind() -> i32 {
        GFX_KIND.load(Ordering::Acquire)
    }
}

#[cfg(windows)]
pub use windows_gfx::{
    unity_adapter_ids, unity_device, unity_gfx_kind, KIND_D3D11, KIND_D3D12, KIND_NONE,
};

/// Probe for C#: writes the captured renderer kind (2 = D3D11, 18 = D3D12, 0 =
/// none) and returns the captured device pointer (null until `UnityPluginLoad`).
/// Touching this `[DllImport]` export also makes Unity map the plugin (running
/// `UnityPluginLoad`) before the host binds to the same image.
#[cfg(windows)]
#[no_mangle]
pub extern "C" fn unterm_unity_gfx(kind_out: *mut i32) -> *mut std::ffi::c_void {
    if !kind_out.is_null() {
        unsafe { *kind_out = windows_gfx::unity_gfx_kind() };
    }
    windows_gfx::unity_device()
}
