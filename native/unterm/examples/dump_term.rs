//! Headless check: spawn a terminal, run a command, render, dump a PNG.
//!
//! Verifies the whole path (PTY -> parser -> grid -> GPU render) without Unity.
//! Run with: `cargo run -p unterm --example dump_term`

use std::ffi::CString;
use std::time::Duration;

// Re-use the crate's C ABI directly (the rlib exposes the same symbols).
use unterm::*;

fn main() {
    env_logger::try_init().ok();

    let cwd = CString::new(std::env::current_dir().unwrap().to_string_lossy().to_string()).unwrap();
    let id = unsafe { unterm_create(1000, 600, 2.0, cwd.as_ptr()) };
    assert!(id != 0, "create failed");

    // Mirror the Unity host: load an explicit monospace family.
    let font = CString::new("/System/Library/Fonts/Menlo.ttc").unwrap();
    unsafe { unterm_set_font(id, font.as_ptr()) };

    // Drive a few commands and let the shell echo + run them.
    for cmd in [
        "printf '\\033[1;32mhello\\033[0m from \\033[1;34munterm\\033[0m\\n'\n",
        "echo 'colors:'; for i in 1 2 3 4 5 6; do printf '\\033[4%dm  \\033[0m' $i; done; echo\n",
        "ls -la | head -8\n",
    ] {
        let s = CString::new(cmd).unwrap();
        unsafe { unterm_send_text(id, s.as_ptr()) };
        std::thread::sleep(Duration::from_millis(400));
    }
    std::thread::sleep(Duration::from_millis(400));

    // Zero-copy has no CPU readback, so just drive the full pipeline a few times
    // (PTY -> parser -> grid -> GPU render into the IOSurface target) and confirm it
    // doesn't panic — a runtime check of the objc2-metal IOSurface path on wgpu 29.
    for _ in 0..3 {
        unterm_render(id);
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut w = 0u32;
    let mut h = 0u32;
    unsafe { unterm_size(id, &mut w as *mut u32, &mut h as *mut u32) };
    let raw = unsafe { unterm_raw_texture(id) };
    println!("rendered {w}x{h}; IOSurface MTLTexture ptr = {raw:?}");
    assert!(!raw.is_null(), "IOSurface texture was null (zero-copy target failed)");

    unterm_destroy(id);
    println!("OK: render pipeline ran on wgpu 29 without panicking");
}
