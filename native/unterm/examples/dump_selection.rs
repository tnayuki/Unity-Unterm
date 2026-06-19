//! Headless check for mouse selection + copy.
//!
//! Prints a known line, drags a full-buffer selection across it, verifies the
//! copied text round-trips, and dumps a PNG so the highlight can be eyeballed.
//! Run with: `cargo run -p unterm --example dump_selection`

use std::ffi::{CStr, CString};
use std::time::Duration;

use unterm::*;

fn main() {
    env_logger::try_init().ok();

    let cwd = CString::new(std::env::current_dir().unwrap().to_string_lossy().to_string()).unwrap();
    let id = unsafe { unterm_create(1000, 600, 2.0, cwd.as_ptr()) };
    assert!(id != 0, "create failed");

    let font = CString::new("/System/Library/Fonts/Menlo.ttc").unwrap();
    unsafe { unterm_set_font(id, font.as_ptr()) };

    // A unique token so we can confirm the selection captured the output.
    let cmd = CString::new("printf 'alpha beta gamma SELECTME delta\\n'\n").unwrap();
    unsafe { unterm_send_text(id, cmd.as_ptr()) };
    std::thread::sleep(Duration::from_millis(600));

    unterm_render(id);

    // Drag a Simple (by-character) selection across the whole visible buffer:
    // top-left to a point past the bottom-right, which clamps to the last cell.
    unterm_selection_start(id, 0.0, 0.0, 0);
    unterm_selection_update(id, 1.0e6, 1.0e6);

    let mut len = 0usize;
    let ptr = unsafe { unterm_selection_text(id, &mut len as *mut usize) };
    assert!(!ptr.is_null() && len > 0, "no selection text");
    let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    assert!(text.contains("SELECTME"), "selection missing token; got:\n{text}");
    println!("selection round-trip OK ({len} bytes), contains SELECTME");

    // Render the highlighted frame for visual inspection.
    unterm_render(id);
    let mut plen = 0usize;
    let px = unsafe { unterm_get_pixels(id, &mut plen as *mut usize) };
    assert!(!px.is_null() && plen > 0, "no pixels");
    let mut w = 0u32;
    let mut h = 0u32;
    unsafe { unterm_size(id, &mut w as *mut u32, &mut h as *mut u32) };
    let data = unsafe { std::slice::from_raw_parts(px, plen) };
    image::save_buffer("unterm_sel.png", data, w, h, image::ExtendedColorType::Rgba8)
        .expect("png save");
    println!("wrote unterm_sel.png ({w}x{h})");

    // Clearing drops the highlight (selection text becomes empty).
    unterm_selection_clear(id);
    let mut clen = 0usize;
    let cptr = unsafe { unterm_selection_text(id, &mut clen as *mut usize) };
    assert!(clen == 0 || cptr.is_null(), "selection not cleared");
    println!("clear OK");

    unterm_destroy(id);
}
