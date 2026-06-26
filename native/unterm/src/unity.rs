//! Unity native-plugin interface (Windows only): capture the editor's graphics
//! device when the plugin loads, so the zero-copy path can open shared textures
//! on Unity's own D3D11/D3D12 device.
//!
//! Unity calls [`UnityPluginLoad`] with an `IUnityInterfaces*` registry when it
//! loads a native plugin. We pull `IUnityGraphics` to learn the renderer kind,
//! then the matching `IUnityGraphicsD3D11`/`D3D12` to grab the `ID3D11Device*` /
//! `ID3D12Device*`. The exact GUIDs and vtable layouts come from Unity's bundled
//! PluginAPI headers (IUnityInterface.h / IUnityGraphics.h / *D3D11.h / *D3D12.h).
//!
//! The host binds to *this* image — the one Unity itself loaded — rather than a
//! private shadow copy, so `UnityPluginLoad` runs in the same image the terminal
//! renders in and the captured [`unity_device`] is available to the surface code
//! directly. No cross-image bridging is needed.
//!
//! `UNITY_INTERFACE_API` is `__stdcall`, which is a no-op on x86-64, so the C ABI
//! (`extern "C"`) matches. We only ever read the first vtable slot(s) we need, so
//! each interface struct declares just enough fields to reach them.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory4, DXGI_ADAPTER_DESC,
};

/// `UnityGfxRenderer` values we care about (from IUnityGraphics.h).
pub const KIND_NONE: i32 = 0;
pub const KIND_D3D11: i32 = 2;
pub const KIND_D3D12: i32 = 18;

// Interface GUIDs (high, low), verbatim from the PluginAPI headers.
const GUID_IUNITY_GRAPHICS: (u64, u64) = (0x7CBA_0A9C_A4DD_B544, 0x8C5A_D492_6EB1_7B11);
const GUID_IUNITY_GRAPHICS_D3D11: (u64, u64) = (0xAAB3_7EF8_7A87_D748, 0xBF76_967F_07EF_B177);
/// All `IUnityGraphicsD3D12vN` interfaces expose `GetDevice()` as their first
/// method, so we try newest-first and take whichever the runtime provides.
const GUIDS_IUNITY_GRAPHICS_D3D12: &[(u64, u64)] = &[
    (0x9d30_3045_d00d_4cfd, 0x8feb_b429_68b4_23b6), // v8
    (0x4624_B0DA_41B6_4AAC, 0x915A_ABCB_9BC3_F0D3), // v7
    (0xA396_DCE5_8CAC_4D78, 0xAFDD_9B28_1F20_B840), // v6
    (0xF5C8_D8A3_7D37_BC42, 0xB02D_FE93_B506_4A27), // v5
    (0x498F_FCC1_3EC9_4006, 0xB18F_8B0F_F677_78C8), // v4
    (0x57C3_FAFE_59E5_E843, 0xBF4F_5998_474B_B600), // v3
    (0xEC39_D2F1_8446_C745, 0xB1A2_6266_41D6_B11F), // v2
    (0xEF4C_EC88_A45F_4C4C, 0xBD29_5B6F_2A38_D9DE), // obsolete
];

#[repr(C)]
#[allow(dead_code)]
struct IUnityInterfaces {
    // GetInterface/RegisterInterface take the GUID by value (16 bytes); we use
    // the Split variants instead, so these two slots are left opaque for layout.
    get_interface: *const c_void,
    register_interface: *const c_void,
    get_interface_split: extern "C" fn(u64, u64) -> *mut c_void,
    register_interface_split: *const c_void,
}

/// `IUnityInterfaces::GetInterfaceSplit` — fetches an interface by (split) GUID.
type GetFn = extern "C" fn(u64, u64) -> *mut c_void;

#[repr(C)]
struct IUnityGraphics {
    get_renderer: extern "C" fn() -> i32,
    // void RegisterDeviceEventCallback(void (*callback)(UnityGfxDeviceEventType));
    register_device_event_callback: extern "C" fn(extern "C" fn(i32)),
    // remaining vtable slots unused
}

// UnityGfxDeviceEventType values (from IUnityGraphics.h).
const GFX_EVENT_INITIALIZE: i32 = 0;
const GFX_EVENT_SHUTDOWN: i32 = 1;
const GFX_EVENT_BEFORE_RESET: i32 = 2;
const GFX_EVENT_AFTER_RESET: i32 = 3;

/// Both `IUnityGraphicsD3D11` and every `IUnityGraphicsD3D12vN` start with
/// `GetDevice()`, so one shape covers fetching the device pointer.
#[repr(C)]
struct IUnityGraphicsDevice {
    get_device: extern "C" fn() -> *mut c_void,
    // remaining vtable slots unused
}

static GFX_KIND: AtomicI32 = AtomicI32::new(KIND_NONE);
static DEVICE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
// `IUnityInterfaces::GetInterfaceSplit`, stashed at plugin load so the graphics
// device-event callback can re-fetch the device when it initializes later.
static GET_INTERFACE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

// PCI vendor/device id of the adapter Unity's device runs on. wgpu must pick the
// SAME physical adapter (see gpu.rs): a shared texture handle opened across two
// different adapters (e.g. discrete vs integrated GPU) yields a garbled image.
static ADAPTER_VENDOR: AtomicU32 = AtomicU32::new(0);
static ADAPTER_DEVICE: AtomicU32 = AtomicU32::new(0);
static ADAPTER_KNOWN: AtomicBool = AtomicBool::new(false);

/// Called by Unity when the native plugin is loaded.
#[no_mangle]
pub unsafe extern "C" fn UnityPluginLoad(interfaces: *mut c_void) {
    crate::init_log();
    if interfaces.is_null() {
        return;
    }
    let ifaces = &*(interfaces as *const IUnityInterfaces);
    let get = ifaces.get_interface_split;
    GET_INTERFACE.store(get as usize as *mut c_void, Ordering::Release);

    let gfx = get(GUID_IUNITY_GRAPHICS.0, GUID_IUNITY_GRAPHICS.1);
    if gfx.is_null() {
        log::warn!("unterm: IUnityGraphics unavailable");
        return;
    }
    // Register for device init/reset so we capture the device even if it isn't
    // ready at load time — the case for a terminal window restored during early
    // editor startup, before the graphics device is created.
    ((*(gfx as *const IUnityGraphics)).register_device_event_callback)(on_gfx_device_event);
    // Also capture now, in case the device is already initialized (Unity won't
    // re-fire the Initialize event for an already-created device).
    capture_device(get);
}

/// Unity graphics device lifecycle callback: (re)capture the device when it
/// initializes/resets, drop it on shutdown.
extern "C" fn on_gfx_device_event(event_type: i32) {
    match event_type {
        GFX_EVENT_INITIALIZE | GFX_EVENT_AFTER_RESET => {
            let ptr = GET_INTERFACE.load(Ordering::Acquire);
            if !ptr.is_null() {
                let get: GetFn = unsafe { std::mem::transmute(ptr) };
                unsafe { capture_device(get) };
            }
        }
        GFX_EVENT_SHUTDOWN | GFX_EVENT_BEFORE_RESET => {
            DEVICE.store(std::ptr::null_mut(), Ordering::Release);
        }
        _ => {}
    }
}

/// Fetch and store Unity's current D3D device (and its adapter). A null device
/// (graphics not ready yet) is fine — the device-event callback retries on init.
unsafe fn capture_device(get: GetFn) {
    let gfx = get(GUID_IUNITY_GRAPHICS.0, GUID_IUNITY_GRAPHICS.1);
    if gfx.is_null() {
        return;
    }
    let kind = ((*(gfx as *const IUnityGraphics)).get_renderer)();
    let device = match kind {
        KIND_D3D11 => fetch_device(get, std::slice::from_ref(&GUID_IUNITY_GRAPHICS_D3D11)),
        KIND_D3D12 => fetch_device(get, GUIDS_IUNITY_GRAPHICS_D3D12),
        _ => std::ptr::null_mut(),
    };

    GFX_KIND.store(kind, Ordering::Release);
    DEVICE.store(device, Ordering::Release);

    if !device.is_null() {
        if let Some((vendor, dev_id)) = adapter_ids(kind, device) {
            ADAPTER_VENDOR.store(vendor, Ordering::Release);
            ADAPTER_DEVICE.store(dev_id, Ordering::Release);
            ADAPTER_KNOWN.store(true, Ordering::Release);
            log::info!(
                "unterm: captured Unity device kind={kind} adapter=vendor:0x{vendor:04x} device:0x{dev_id:04x}"
            );
        } else {
            log::warn!("unterm: captured Unity device kind={kind} but adapter id unavailable");
        }
    } else {
        log::info!("unterm: Unity graphics device not ready yet (kind={kind}); will retry on init");
    }
}

/// The PCI vendor/device id of the adapter Unity's captured device runs on, or
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

/// Resolve the (vendor_id, device_id) of the adapter behind Unity's device.
/// D3D11: via `IDXGIDevice::GetAdapter`. D3D12: look the device's LUID up through
/// a DXGI factory. Returns `None` on any COM failure.
unsafe fn adapter_ids(kind: i32, device: *mut c_void) -> Option<(u32, u32)> {
    let d: DXGI_ADAPTER_DESC = match kind {
        KIND_D3D11 => {
            let dev: &ID3D11Device = ID3D11Device::from_raw_borrowed(&device)?;
            let dxgi_dev: IDXGIDevice = dev.cast().ok()?;
            let adapter: IDXGIAdapter = dxgi_dev.GetAdapter().ok()?;
            adapter.GetDesc().ok()?
        }
        KIND_D3D12 => {
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

/// Try each interface GUID in turn; return the device from the first that the
/// runtime hands back.
unsafe fn fetch_device(
    get: extern "C" fn(u64, u64) -> *mut c_void,
    guids: &[(u64, u64)],
) -> *mut c_void {
    for &(h, l) in guids {
        let iface = get(h, l);
        if !iface.is_null() {
            return ((*(iface as *const IUnityGraphicsDevice)).get_device)();
        }
    }
    std::ptr::null_mut()
}

/// Called by Unity when the native plugin is unloaded.
#[no_mangle]
pub extern "C" fn UnityPluginUnload() {
    DEVICE.store(std::ptr::null_mut(), Ordering::Release);
    GFX_KIND.store(KIND_NONE, Ordering::Release);
}

/// Probe for C#: writes the captured renderer kind (2 = D3D11, 18 = D3D12, 0 =
/// none) and returns the captured device pointer (null until `UnityPluginLoad`).
#[no_mangle]
pub extern "C" fn unterm_unity_gfx(kind_out: *mut i32) -> *mut c_void {
    if !kind_out.is_null() {
        unsafe { *kind_out = GFX_KIND.load(Ordering::Acquire) };
    }
    DEVICE.load(Ordering::Acquire)
}

/// The captured Unity device (null until `UnityPluginLoad` runs), for the
/// in-process zero-copy path.
pub fn unity_device() -> *mut c_void {
    DEVICE.load(Ordering::Acquire)
}

/// The captured renderer kind ([`KIND_D3D11`] / [`KIND_D3D12`] / [`KIND_NONE`]).
pub fn unity_gfx_kind() -> i32 {
    GFX_KIND.load(Ordering::Acquire)
}
