//! Windows shared render target: zero-copy hand-off to Unity, no CPU readback.
//!
//! wgpu renders on its own D3D12 device. To let Unity sample the result without
//! a CPU copy, we create the render target as a *shared* D3D12 committed
//! resource, take an NT handle (`CreateSharedHandle`), and open that handle on
//! Unity's own graphics device (captured by [`crate::unity::UnityPluginLoad`]):
//! D3D11 via `OpenSharedResource1`, D3D12 via `OpenSharedHandle`. The opened
//! texture is what `Renderer::raw_texture` hands Unity for `CreateExternalTexture`.
//!
//! The editor graphics device is always D3D11 or D3D12, so this is the only
//! display path — there is no readback fallback. If the Unity device isn't
//! captured yet (it is captured at editor startup, long before any terminal
//! window opens) or a call fails, the buffer is a plain offscreen texture and
//! `raw_texture` reports null; the host shows a status and retries, and a later
//! resize rebuilds the target once the device is available.
//!
//! Like the macOS IOSurface path, this is **single-buffered and synchronous**:
//! the renderer draws into a private wgpu target, copies the finished frame into
//! the one shared texture, and `present` blocks until the GPU is done, so Unity
//! always samples a complete frame. Uses the same `windows` (windows-rs) crate
//! wgpu-hal is built on, so the raw `ID3D12Resource` handed to wgpu-hal matches.

use std::ffi::c_void;
use std::ptr;

use windows::core::{IUnknown, Interface};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Device1, ID3D11Texture2D};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device, ID3D12Resource, D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_HEAP_FLAG_SHARED,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_MEMORY_POOL_UNKNOWN, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_TEXTURE_LAYOUT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC};

use crate::unity;

/// Mirrors the macOS `IOSurfaceRef` type so `renderer.rs` stays platform-agnostic.
pub type IOSurfaceRef = *const c_void;

/// The shared (Unity-visible) texture. wgpu never renders into this directly — the
/// renderer draws into a private target and the finished frame is *copied* in (see
/// `SharedSurface::finish_frame`), so it always gets a clean, complete overwrite.
struct Buffer {
    /// The wgpu side of the shared D3D12 resource; the copy destination.
    texture: wgpu::Texture,
    /// Unity-device texture (`ID3D11Texture2D` / `ID3D12Resource`) aliasing
    /// `texture`, or null if the Unity device wasn't available when this target was
    /// built (a placeholder until the next rebuild picks the device up).
    raw_texture: *mut c_void,
    /// Keeps the Unity-device texture alive (windows-rs RAII releases on drop);
    /// `None` for a placeholder.
    _keep_alive: Option<IUnknown>,
}

/// A single-buffered, synchronous render target (the macOS IOSurface model).
pub struct SharedSurface {
    /// The one shared texture Unity samples (the copy destination).
    buffer: Buffer,
    /// Private, wgpu-owned render target the renderer draws into. A normal (not
    /// imported) texture, so `LoadOp::Clear` reliably clears it every frame; its
    /// result is copied into the shared texture each frame.
    render: wgpu::Texture,
    render_view: wgpu::TextureView,
    /// Kept so a placeholder buffer (built before the Unity device was available)
    /// can be rebuilt into a real shared texture on a later frame.
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

// The terminal that owns this surface only ever renders from the host's main
// thread (serialized by the registry mutex), so moving the raw COM pointers
// between threads is sound — same reasoning as the macOS IOSurface path.
unsafe impl Send for SharedSurface {}

impl SharedSurface {
    /// No IOSurface on Windows.
    pub fn surface(&self) -> IOSurfaceRef {
        ptr::null()
    }

    /// The shared texture Unity samples (null until the device is captured and the
    /// target rebuilt; the host shows a status and retries).
    pub fn raw_texture(&self) -> *mut c_void {
        self.buffer.raw_texture
    }

    /// The private render target the renderer draws into this frame.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.render_view
    }

    /// Copy the just-rendered private target into the shared texture. Recorded into
    /// the render encoder before submit, so Unity's texture gets a full, clean
    /// overwrite every frame — no stale pixels accumulating from an unreliable clear
    /// on the imported D3D12 resource.
    pub fn finish_frame(&self, encoder: &mut wgpu::CommandEncoder) {
        let dst = &self.buffer.texture;
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.render,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: dst,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            dst.size(),
        );
    }

    /// Before a render: if we're on a placeholder (the Unity device wasn't
    /// available when this target was built, or a rebuild failed), try to upgrade
    /// to a real shared texture now. This self-heals a blank/"GPU not ready"
    /// terminal once the device appears — important for windows restored at editor
    /// startup, before Unity's graphics device is captured — without needing a
    /// resize to force a rebuild.
    pub fn begin_frame(&mut self) {
        if self.buffer.raw_texture.is_null() && !unity::unity_device().is_null() {
            if let Some(b) = unsafe {
                try_shared_buffer(&crate::gpu::gpu().device, self.width, self.height, self.format)
            } {
                self.buffer = b;
                log::info!("unterm: shared texture upgraded after the Unity device became available");
            }
        }
    }

    /// Block until the GPU finishes the submitted frame (render + copy) so Unity
    /// samples a complete texture. Same as the macOS path.
    pub fn present(&mut self) {
        let _ = crate::gpu::gpu().device.poll(wgpu::PollType::wait_indefinitely());
    }

    /// Single-buffered — nothing to advance on idle ticks.
    pub fn advance(&mut self) -> bool {
        false
    }
}

/// Private render target the renderer draws into: a render attachment that's also
/// the copy source for the per-frame blit into the shared texture.
const RENDER_USAGE: wgpu::TextureUsages =
    wgpu::TextureUsages::RENDER_ATTACHMENT.union(wgpu::TextureUsages::COPY_SRC);

/// Shared (Unity-visible) texture usage: only ever a full-overwrite copy
/// destination, never a render target (see `Buffer`).
const SHARED_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::COPY_DST;

/// Create the target: a Unity-shared texture when the device is available, else a
/// plain offscreen placeholder (raw_texture null) until a later rebuild.
pub fn create_shared_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> SharedSurface {
    let width = width.max(1);
    let height = height.max(1);

    let buffer = match unsafe { try_shared_buffer(device, width, height, format) } {
        Some(b) => {
            log::info!("unterm: using D3D zero-copy shared texture");
            b
        }
        None => {
            log::error!("unterm: Unity D3D device unavailable; awaiting device rebuild");
            placeholder_buffer(device, width, height, format)
        }
    };

    let render = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("unterm-render-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: RENDER_USAGE,
        view_formats: &[],
    });
    let render_view = render.create_view(&wgpu::TextureViewDescriptor::default());

    SharedSurface {
        buffer,
        render,
        render_view,
        width,
        height,
        format,
    }
}

/// A plain offscreen target used only when Unity's device isn't captured yet, so
/// the renderer has a valid view to draw into. `raw_texture` is null, so the host
/// displays nothing and retries; a later resize rebuilds as a shared target.
fn placeholder_buffer(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> Buffer {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("unterm-placeholder-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: SHARED_USAGE,
        view_formats: &[],
    });
    Buffer {
        texture,
        raw_texture: ptr::null_mut(),
        _keep_alive: None,
    }
}

/// Build one shared buffer, or `None` if the Unity device isn't captured yet or any
/// call fails.
unsafe fn try_shared_buffer(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> Option<Buffer> {
    let kind = unity::unity_gfx_kind();
    let unity_dev = unity::unity_device();
    if unity_dev.is_null() || kind == unity::KIND_NONE {
        return None;
    }

    // wgpu's own ID3D12Device (clone bumps the refcount; the original stays alive
    // via wgpu).
    let wgpu_dev: ID3D12Device = {
        let hal = device.as_hal::<wgpu::hal::api::Dx12>()?;
        hal.raw_device().clone()
    };

    // --- producer: a shared, render-target D3D12 texture on wgpu's device ---
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        // SIMULTANEOUS_ACCESS makes the cross-API read well-defined: Microsoft
        // documents it for D3D11-on-12 interop, which is exactly this path (wgpu
        // D3D12 writes via copy; Unity's D3D11 device samples).
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    };

    let mut resource: Option<ID3D12Resource> = None;
    if let Err(e) = wgpu_dev.CreateCommittedResource(
        &heap,
        D3D12_HEAP_FLAG_SHARED,
        &desc,
        D3D12_RESOURCE_STATE_COMMON,
        None,
        &mut resource,
    ) {
        log::warn!("unterm: CreateCommittedResource(shared) failed: {e}");
        return None;
    }
    let resource = resource?;

    let handle = match wgpu_dev.CreateSharedHandle(&resource, None, GENERIC_ALL.0, None) {
        Ok(h) => h,
        Err(e) => {
            log::warn!("unterm: CreateSharedHandle failed: {e}");
            return None;
        }
    };

    // --- consumer: open the handle on Unity's device ---
    let opened = match kind {
        unity::KIND_D3D11 => open_on_d3d11(unity_dev, handle),
        unity::KIND_D3D12 => open_on_d3d12(unity_dev, handle),
        _ => None,
    };
    let _ = CloseHandle(handle);

    let (raw_texture, keep_alive) = opened?;

    // Hand the D3D12 resource to wgpu as the copy destination (moves our reference;
    // the wgpu texture owns it from here).
    let hal_tex = wgpu::hal::dx12::Device::texture_from_raw(
        resource,
        format,
        wgpu::TextureDimension::D2,
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        1,
        1,
    );
    let texture = device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
        hal_tex,
        &wgpu::TextureDescriptor {
            label: Some("unterm-d3d-shared-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: SHARED_USAGE,
            view_formats: &[],
        },
    );

    Some(Buffer {
        texture,
        raw_texture,
        _keep_alive: Some(keep_alive),
    })
}

/// Open the shared handle on Unity's D3D11 device → `ID3D11Texture2D`.
unsafe fn open_on_d3d11(unity_dev: *mut c_void, handle: HANDLE) -> Option<(*mut c_void, IUnknown)> {
    let dev: &ID3D11Device = ID3D11Device::from_raw_borrowed(&unity_dev)?;
    let dev1: ID3D11Device1 = dev.cast().ok()?;
    let tex: ID3D11Texture2D = match dev1.OpenSharedResource1(handle) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("unterm: OpenSharedResource1 failed: {e}");
            return None;
        }
    };
    let raw = tex.as_raw();
    Some((raw, tex.into()))
}

/// Open the shared handle on Unity's D3D12 device → `ID3D12Resource`.
unsafe fn open_on_d3d12(unity_dev: *mut c_void, handle: HANDLE) -> Option<(*mut c_void, IUnknown)> {
    let dev: &ID3D12Device = ID3D12Device::from_raw_borrowed(&unity_dev)?;
    let mut res: Option<ID3D12Resource> = None;
    if let Err(e) = dev.OpenSharedHandle(handle, &mut res) {
        log::warn!("unterm: OpenSharedHandle failed: {e}");
        return None;
    }
    let res = res?;
    let raw = res.as_raw();
    Some((raw, res.into()))
}
