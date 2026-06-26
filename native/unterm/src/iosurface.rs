//! IOSurface-backed Metal render target for zero-copy sharing with Unity.
//!
//! We create an `IOSurface`, wrap it in an `MTLTexture` on wgpu's own Metal
//! device, and hand that texture to wgpu via `create_texture_from_hal` so the
//! panel renders straight into the surface. Unity samples the result with no CPU
//! readback. The Metal side uses the same `objc2-metal` bindings wgpu-hal 29 is
//! built on; IOSurface creation stays on the plain CoreFoundation FFI.

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};

/// Raw IOSurface pointer handed across the surface abstraction (the FFI shape the
/// rest of the crate uses); distinct from objc2's typed `IOSurfaceRef`.
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
    /// Raw `id<MTLTexture>` (owned by `texture`; valid while it lives). Handed to
    /// Unity for `CreateExternalTexture`.
    raw_texture: *mut c_void,
    /// Owns the wgpu texture's retain on the Metal texture / IOSurface; never read
    /// directly, but must outlive the surface (see `Drop`).
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
        let _ = crate::gpu::gpu().device.poll(wgpu::PollType::wait_indefinitely());
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

    // Pull wgpu's own MTLDevice so the texture lands on the same device.
    let mtl_device: Retained<ProtocolObject<dyn MTLDevice>> = unsafe {
        let hal_device = device
            .as_hal::<wgpu::hal::api::Metal>()
            .expect("wgpu device is not Metal");
        hal_device.raw_device().clone()
    };

    let desc = unsafe {
        let desc = MTLTextureDescriptor::new();
        desc.setTextureType(MTLTextureType::Type2D);
        // Must match the wgpu FORMAT (Rgba8UnormSrgb) so wgpu accepts the texture.
        desc.setPixelFormat(MTLPixelFormat::RGBA8Unorm_sRGB);
        desc.setWidth(width as usize);
        desc.setHeight(height as usize);
        desc.setMipmapLevelCount(1);
        desc.setStorageMode(MTLStorageMode::Shared);
        desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        desc
    };

    // Bridge the CoreFoundation IOSurface pointer into objc2's typed reference.
    let surf_ref: &objc2_io_surface::IOSurfaceRef =
        unsafe { &*(surface as *const objc2_io_surface::IOSurfaceRef) };
    let mtl_tex: Retained<ProtocolObject<dyn MTLTexture>> = mtl_device
        .newTextureWithDescriptor_iosurface_plane(&desc, surf_ref, 0)
        .expect("newTextureWithDescriptor:iosurface: returned nil");

    // Raw pointer for Unity; valid while the wgpu texture (below) holds the retain.
    let raw_texture = Retained::as_ptr(&mtl_tex) as *mut c_void;

    let hal_tex = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            mtl_tex,
            format,
            MTLTextureType::Type2D,
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
