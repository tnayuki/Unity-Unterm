//! IOSurface-backed Metal render target for zero-copy sharing with Unity.
//!
//! We create an `IOSurface`, wrap it in an `MTLTexture` on wgpu's own Metal
//! device, and hand that texture to wgpu via `create_texture_from_hal` so the
//! panel renders straight into the surface. The same `IOSurface` (and, in
//! Stage 2, a sibling `MTLTexture` created on Unity's device) lets Unity sample
//! the result with no CPU readback.

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;
use foreign_types::ForeignType;
use objc::{msg_send, sel, sel_impl};

pub type IOSurfaceRef = *const c_void;

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;
    fn IOSurfaceCreate(properties: CFDictionaryRef) -> IOSurfaceRef;
}

extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// 'RGBA' four-char code; matches MTLPixelFormat::RGBA8Unorm (4 bytes/pixel).
const PIXEL_FORMAT_RGBA: i32 = 0x5247_4241;

/// An IOSurface plus the wgpu texture that renders into it.
pub struct SharedSurface {
    surface: IOSurfaceRef,
    /// Raw `id<MTLTexture>` (owned by `texture`; valid while it lives).
    /// Stage 2 hands Unity a sibling texture made from the same IOSurface.
    raw_texture: *mut c_void,
    /// Owns the wgpu texture's retain on the IOSurface; never read directly, but
    /// must outlive the surface (see `Drop`), so it's held for its lifetime.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

// The IOSurface/MTLTexture raw handles are reference-counted Obj-C objects.
// The terminal that owns this surface is only ever rendered from the host's
// main thread (serialized by the registry mutex); the PTY reader thread never
// touches the renderer. So moving the surface between threads is sound.
unsafe impl Send for SharedSurface {}

impl SharedSurface {
    pub fn surface(&self) -> IOSurfaceRef {
        self.surface
    }

    pub fn raw_texture(&self) -> *mut c_void {
        self.raw_texture
    }

    /// The render target view (single-buffered on macOS).
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The renderer draws straight into the IOSurface texture, so there's nothing
    /// to blit afterwards (unlike the Windows copy-into-shared-texture path).
    pub fn finish_frame(&self, _encoder: &mut wgpu::CommandEncoder) {}

    /// No swapchain on macOS — the IOSurface is sampled directly.
    pub fn begin_frame(&mut self) {}

    /// Block until the frame is done so Unity samples a finished IOSurface.
    pub fn present(&mut self) {
        crate::gpu::gpu().device.poll(wgpu::Maintain::Wait);
    }

    /// Single-buffered — nothing to advance on idle ticks.
    pub fn advance(&mut self) -> bool {
        false
    }
}

impl Drop for SharedSurface {
    fn drop(&mut self) {
        // `texture` keeps its own retain on the IOSurface, so releasing our
        // reference here (before the field drops) is safe.
        unsafe { CFRelease(self.surface) };
    }
}

/// Create an IOSurface-backed wgpu render target on `device`'s Metal device.
pub fn create_shared_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> SharedSurface {
    let surface = unsafe { create_iosurface(width, height) };
    assert!(!surface.is_null(), "IOSurfaceCreate failed");

    // Pull the raw MTLDevice out of wgpu so the texture lands on the same device.
    let raw_device: metal::Device = unsafe {
        device
            .as_hal::<wgpu::hal::api::Metal, _, _>(|d| {
                d.expect("wgpu device is not Metal")
                    .raw_device()
                    .lock()
                    .clone()
            })
            .expect("as_hal returned None")
    };

    let desc = metal::TextureDescriptor::new();
    desc.set_texture_type(metal::MTLTextureType::D2);
    // Must match the wgpu FORMAT (Rgba8UnormSrgb) so wgpu accepts the texture.
    desc.set_pixel_format(metal::MTLPixelFormat::RGBA8Unorm_sRGB);
    desc.set_width(width as u64);
    desc.set_height(height as u64);
    desc.set_mipmap_level_count(1);
    desc.set_storage_mode(metal::MTLStorageMode::Shared);
    desc.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);

    // newTextureWithDescriptor:iosurface:plane: — not exposed by metal-rs 0.29.
    let raw_tex: metal::Texture = unsafe {
        let dev_obj = raw_device.as_ptr() as *mut objc::runtime::Object;
        let desc_obj = desc.as_ptr() as *mut objc::runtime::Object;
        let tex_id: *mut objc::runtime::Object = msg_send![
            dev_obj,
            newTextureWithDescriptor: desc_obj
            iosurface: surface
            plane: 0usize
        ];
        assert!(!tex_id.is_null(), "newTextureWithDescriptor:iosurface: returned nil");
        metal::Texture::from_ptr(tex_id as *mut _)
    };

    let raw_texture = raw_tex.as_ptr() as *mut c_void;

    let hal_tex = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw_tex,
            format,
            metal::MTLTextureType::D2,
            1,
            1,
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
        )
    };

    let tex_desc = wgpu::TextureDescriptor {
        label: Some("unterm-iosurface-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };

    let texture =
        unsafe { device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal_tex, &tex_desc) };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    SharedSurface {
        surface,
        raw_texture,
        texture,
        view,
    }
}

unsafe fn create_iosurface(width: u32, height: u32) -> IOSurfaceRef {
    let props = CFDictionary::from_CFType_pairs(&[
        (
            CFString::wrap_under_get_rule(kIOSurfaceWidth),
            CFNumber::from(width as i32),
        ),
        (
            CFString::wrap_under_get_rule(kIOSurfaceHeight),
            CFNumber::from(height as i32),
        ),
        (
            CFString::wrap_under_get_rule(kIOSurfaceBytesPerElement),
            CFNumber::from(4i32),
        ),
        (
            CFString::wrap_under_get_rule(kIOSurfacePixelFormat),
            CFNumber::from(PIXEL_FORMAT_RGBA),
        ),
    ]);
    IOSurfaceCreate(props.as_concrete_TypeRef())
}
