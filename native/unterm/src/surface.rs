//! Platform-selected shared render target.
//!
//! The renderer draws into a [`SharedSurface`] that Unity samples zero-copy: on
//! macOS an IOSurface-backed `MTLTexture` (see [`crate::iosurface`]), on Windows
//! a shared D3D12 texture opened on Unity's device (see [`d3d`]). Both expose the
//! same `surface()`, `raw_texture()`, and `view()`, so `renderer.rs` is
//! platform-agnostic.
//!
//! Only these zero-copy platforms are supported — there is no CPU-readback path,
//! so any other target is a hard compile error rather than a silent degraded build.

#[cfg(target_os = "macos")]
pub use crate::iosurface::{create_shared_target, IOSurfaceRef, SharedSurface};

// Windows: zero-copy via a shared D3D12 texture (see [`d3d`]).
#[cfg(windows)]
mod d3d;
#[cfg(windows)]
pub use d3d::{create_shared_target, IOSurfaceRef, SharedSurface};

#[cfg(all(not(target_os = "macos"), not(windows)))]
compile_error!("unterm supports only macOS and Windows (zero-copy display platforms)");
