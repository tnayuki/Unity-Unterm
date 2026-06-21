//! C ABI for the Unterm native terminal, consumed by Unity via P/Invoke.
//!
//! Terminals live in a process-global registry keyed by a stable `u64` id, so
//! each one (its shell, scrollback, and GPU surface) survives Unity C# domain
//! reloads; the host serializes the id and re-adopts after a reload. Many
//! terminals coexist — one per editor window — sharing a single wgpu device
//! (see [`gpu`]). The id-based surface mirrors the durable-session pattern from
//! the prior agent panel; here the whole terminal is durable.
//!
//! ```c
//! uint64_t unterm_create(uint32_t w, uint32_t h, float scale, const char* cwd);
//! void     unterm_destroy(uint64_t id);
//! void     unterm_render(uint64_t id);
//! const void* unterm_raw_texture(uint64_t id); // id<MTLTexture> for zero-copy
//! ```

mod gpu;
mod iosurface;
mod keys;
mod palette;
mod pty;
mod quads;
mod renderer;
mod term;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use term::Terminal;

/// Initialize logging once. Safe to call repeatedly.
fn init_log() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = env_logger::try_init();
    });
}

type Registry = HashMap<u64, Box<Terminal>>;

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Unpack 0x00RRGGBB into [r, g, b].
fn rgb(packed: u32) -> [u8; 3] {
    [(packed >> 16) as u8, (packed >> 8) as u8, packed as u8]
}

/// Run `f` against the terminal with `id`, returning `default` if absent.
fn with_term<R>(id: u64, default: R, f: impl FnOnce(&mut Terminal) -> R) -> R {
    match registry().lock().unwrap().get_mut(&id) {
        Some(t) => f(t),
        None => default,
    }
}

/// Create a terminal sized to `width`x`height` physical px at `scale`, running
/// `$SHELL` rooted at `cwd` (empty = inherit). Returns a stable id (0 on error).
#[no_mangle]
pub unsafe extern "C" fn unterm_create(
    width: u32,
    height: u32,
    scale: f32,
    cwd: *const c_char,
) -> u64 {
    init_log();
    let cwd = cstr(cwd);
    let terminal = Terminal::new(width, height, scale, &cwd, "");
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    registry().lock().unwrap().insert(id, Box::new(terminal));
    id
}

/// Like `unterm_create`, but launches `command` directly in the PTY (via the
/// login+interactive `$SHELL`) instead of an interactive shell. Used to start
/// `claude` without typing into a shell. Returns a stable id (0 on error).
#[no_mangle]
pub unsafe extern "C" fn unterm_create_command(
    width: u32,
    height: u32,
    scale: f32,
    cwd: *const c_char,
    command: *const c_char,
) -> u64 {
    init_log();
    let cwd = cstr(cwd);
    let command = cstr(command);
    let terminal = Terminal::new(width, height, scale, &cwd, &command);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    registry().lock().unwrap().insert(id, Box::new(terminal));
    id
}

/// Whether a terminal id is still live (used to re-adopt after a reload).
#[no_mangle]
pub extern "C" fn unterm_exists(id: u64) -> bool {
    registry().lock().unwrap().contains_key(&id)
}

/// Destroy a terminal (kills its shell and frees the GPU surface).
#[no_mangle]
pub extern "C" fn unterm_destroy(id: u64) {
    registry().lock().unwrap().remove(&id);
}

/// Resize to `width`x`height` physical px at `scale`.
#[no_mangle]
pub extern "C" fn unterm_resize(id: u64, width: u32, height: u32, scale: f32) {
    with_term(id, (), |t| t.resize(width, height, scale));
}

/// Set the HiDPI scale (pixels per point) without changing the pixel size.
#[no_mangle]
pub extern "C" fn unterm_set_scale(id: u64, scale: f32) {
    with_term(id, (), |t| t.set_scale(scale));
}

/// Load a font file (TTF/OTF/TTC) as the primary monospace family.
#[no_mangle]
pub unsafe extern "C" fn unterm_set_font(id: u64, path: *const c_char) {
    let path = cstr(path);
    with_term(id, (), |t| t.set_font(&path));
}

/// Set the monospace font size in points.
#[no_mangle]
pub extern "C" fn unterm_set_font_size(id: u64, points: f32) {
    with_term(id, (), |t| t.set_font_size(points));
}

/// Set the default fg/bg/cursor colors (each packed 0x00RRGGBB).
#[no_mangle]
pub extern "C" fn unterm_set_colors(id: u64, fg: u32, bg: u32, cursor: u32) {
    with_term(id, (), |t| t.set_colors(rgb(fg), rgb(bg), rgb(cursor)));
}

/// Whether the window owning this terminal is focused (affects the cursor).
#[no_mangle]
pub extern "C" fn unterm_set_focus(id: u64, focused: bool) {
    with_term(id, (), |t| t.set_focused(focused));
}

/// Write printable UTF-8 text to the shell (typing / pasted text / IME commit).
#[no_mangle]
pub unsafe extern "C" fn unterm_send_text(id: u64, text: *const c_char) {
    if text.is_null() {
        return;
    }
    let bytes = unsafe { CStr::from_ptr(text) }.to_bytes().to_vec();
    with_term(id, (), |t| t.send(&bytes));
}

/// Send a named special key (Enter/Up/F5/...) with modifier flags.
#[no_mangle]
pub unsafe extern "C" fn unterm_send_key(
    id: u64,
    name: *const c_char,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
    let name = cstr(name);
    with_term(id, (), |t| t.send_key(&name, ctrl, alt, shift));
}

/// Scroll the viewport by `delta` lines (positive = back into history).
#[no_mangle]
pub extern "C" fn unterm_scroll(id: u64, delta: i32) {
    with_term(id, (), |t| t.scroll(delta));
}

/// Clear scrollback and redraw a fresh prompt (emulator-level clear).
#[no_mangle]
pub extern "C" fn unterm_clear(id: u64) {
    with_term(id, (), |t| t.clear());
}

/// Paste UTF-8 text (honors bracketed-paste mode).
#[no_mangle]
pub unsafe extern "C" fn unterm_paste(id: u64, text: *const c_char) {
    if text.is_null() {
        return;
    }
    let s = cstr(text);
    with_term(id, (), |t| t.paste(&s));
}

/// Begin a selection at physical-pixel point (x, y). `mode`: 0 = by character,
/// 1 = by word (double-click), 2 = by line (triple-click).
#[no_mangle]
pub extern "C" fn unterm_selection_start(id: u64, x: f32, y: f32, mode: u8) {
    with_term(id, (), |t| t.selection_start(x, y, mode));
}

/// Extend the active selection to physical-pixel point (x, y) (mouse drag).
#[no_mangle]
pub extern "C" fn unterm_selection_update(id: u64, x: f32, y: f32) {
    with_term(id, (), |t| t.selection_update(x, y));
}

/// Clear the active selection (drops the highlight).
#[no_mangle]
pub extern "C" fn unterm_selection_clear(id: u64) {
    with_term(id, (), |t| t.selection_clear());
}

/// The selected text as a stable NUL-terminated UTF-8 string, valid until the
/// next call on this terminal (empty if nothing is selected). Writes the length.
#[no_mangle]
pub unsafe extern "C" fn unterm_selection_text(id: u64, out_len: *mut usize) -> *const c_char {
    let mut map = registry().lock().unwrap();
    let Some(t) = map.get_mut(&id) else {
        if !out_len.is_null() {
            unsafe { *out_len = 0 };
        }
        return std::ptr::null();
    };
    let c = t.selection_text_cstr();
    if !out_len.is_null() {
        unsafe { *out_len = c.to_bytes().len() };
    }
    c.as_ptr()
}

/// Render the current grid into the IOSurface and clear the dirty flag.
#[no_mangle]
pub extern "C" fn unterm_render(id: u64) {
    with_term(id, (), |t| t.render());
}

/// Whether new output (or a state change) is waiting to be rendered.
#[no_mangle]
pub extern "C" fn unterm_dirty(id: u64) -> bool {
    with_term(id, false, |t| t.dirty())
}

/// Whether the shell is still running.
#[no_mangle]
pub extern "C" fn unterm_is_alive(id: u64) -> bool {
    with_term(id, false, |t| t.is_alive())
}

/// The `IOSurfaceRef` backing the current render target (zero-copy display).
#[no_mangle]
pub extern "C" fn unterm_iosurface(id: u64) -> *const c_void {
    with_term(id, std::ptr::null(), |t| t.renderer().iosurface())
}

/// The raw `id<MTLTexture>` of the current render target.
#[no_mangle]
pub extern "C" fn unterm_raw_texture(id: u64) -> *mut c_void {
    with_term(id, std::ptr::null_mut(), |t| t.renderer().raw_texture())
}

/// Pointer to the last rendered RGBA8 pixels (readback fallback). Writes the
/// byte length; returns null if nothing rendered yet.
#[no_mangle]
pub unsafe extern "C" fn unterm_get_pixels(id: u64, out_len: *mut usize) -> *const u8 {
    let mut map = registry().lock().unwrap();
    let Some(t) = map.get_mut(&id) else {
        return std::ptr::null();
    };
    let px = t.read_pixels();
    if !out_len.is_null() {
        unsafe { *out_len = px.len() };
    }
    if px.is_empty() {
        std::ptr::null()
    } else {
        px.as_ptr()
    }
}

/// Write the current pixel size into `width`/`height` (either may be null).
#[no_mangle]
pub unsafe extern "C" fn unterm_size(id: u64, width: *mut u32, height: *mut u32) {
    let map = registry().lock().unwrap();
    if let Some(t) = map.get(&id) {
        let (w, h) = t.renderer().size();
        if !width.is_null() {
            unsafe { *width = w };
        }
        if !height.is_null() {
            unsafe { *height = h };
        }
    }
}

/// Write the cursor rect (physical px) into x/y/w/h; returns false if the
/// cursor is hidden. Used to place the IME composition window.
#[no_mangle]
pub unsafe extern "C" fn unterm_cursor_px(
    id: u64,
    x: *mut f32,
    y: *mut f32,
    w: *mut f32,
    h: *mut f32,
) -> bool {
    let map = registry().lock().unwrap();
    let Some(t) = map.get(&id) else {
        return false;
    };
    match t.cursor_px() {
        Some(r) => {
            unsafe {
                if !x.is_null() {
                    *x = r[0];
                }
                if !y.is_null() {
                    *y = r[1];
                }
                if !w.is_null() {
                    *w = r[2];
                }
                if !h.is_null() {
                    *h = r[3];
                }
            }
            true
        }
        None => false,
    }
}

/// Write the current grid size into `cols`/`rows` (either may be null).
#[no_mangle]
pub unsafe extern "C" fn unterm_grid_size(id: u64, cols: *mut u32, rows: *mut u32) {
    let map = registry().lock().unwrap();
    if let Some(t) = map.get(&id) {
        if !cols.is_null() {
            unsafe { *cols = t.cols() as u32 };
        }
        if !rows.is_null() {
            unsafe { *rows = t.rows() as u32 };
        }
    }
}

/// Write the scrollback geometry for a scrollbar into `history` (scrollback
/// lines above the screen), `offset` (lines the viewport is scrolled up from
/// the live bottom; 0 = pinned to the bottom), and `screen` (visible rows).
/// Any pointer may be null.
#[no_mangle]
pub unsafe extern "C" fn unterm_scroll_state(
    id: u64,
    history: *mut u32,
    offset: *mut u32,
    screen: *mut u32,
) {
    let map = registry().lock().unwrap();
    if let Some(t) = map.get(&id) {
        let (h, o, s) = t.scroll_state();
        unsafe {
            if !history.is_null() {
                *history = h as u32;
            }
            if !offset.is_null() {
                *offset = o as u32;
            }
            if !screen.is_null() {
                *screen = s as u32;
            }
        }
    }
}

/// The window title set by the shell (OSC 0/2), as a stable NUL-terminated
/// UTF-8 string valid until the next call on this terminal. Writes the length.
#[no_mangle]
pub unsafe extern "C" fn unterm_title(id: u64, out_len: *mut usize) -> *const c_char {
    let mut map = registry().lock().unwrap();
    let Some(t) = map.get_mut(&id) else {
        return std::ptr::null();
    };
    let c = t.title_cstr();
    if !out_len.is_null() {
        unsafe { *out_len = c.to_bytes().len() };
    }
    c.as_ptr()
}

// Keep `c_void` referenced so a header generator records the opaque handle type.
#[doc(hidden)]
pub type _UntermHandle = *mut c_void;

// ---- C-ABI Bridge for Unity Device Sharing ----

use std::sync::atomic::AtomicPtr;

use unity_native_plugin::interface::UnityInterfaces;
use unity_native_plugin::metal::{UnityGraphicsMetalV1, UnityGraphicsMetalV1Interface};
use unity_native_plugin::metal::{UnityGraphicsMetalV2, UnityGraphicsMetalV2Interface};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLDevice, MTLCommandQueue};

// ---- Original auto-loaded DLL: store Retained smart pointers ----
// Retained holds a +1 refcount; dropping it calls release automatically.

static UNITY_METAL_DEVICE: Mutex<Option<Retained<ProtocolObject<dyn MTLDevice>>>> =
    Mutex::new(None);
static UNITY_METAL_QUEUE: Mutex<Option<Retained<ProtocolObject<dyn MTLCommandQueue>>>> =
    Mutex::new(None);

unity_native_plugin::unity_native_plugin_entry_point! {
    fn unity_plugin_load(interfaces: &'static UnityInterfaces) {
        if let Some(metal_v2) = interfaces.interface::<UnityGraphicsMetalV2>() {
            // metal_device() returns Option<Retained<...>> — already retained.
            // Storing the Retained keeps our +1 refcount alive.
            *UNITY_METAL_DEVICE.lock().unwrap() = metal_v2.metal_device();
            *UNITY_METAL_QUEUE.lock().unwrap() = metal_v2.command_queue();
        } else if let Some(metal_v1) = interfaces.interface::<UnityGraphicsMetalV1>() {
            *UNITY_METAL_DEVICE.lock().unwrap() = metal_v1.metal_device();
            // V1 does not expose the command queue
        }
    }
    fn unity_plugin_unload() {
        // Setting to None drops the Retained, calling release via Drop trait
        *UNITY_METAL_DEVICE.lock().unwrap() = None;
        *UNITY_METAL_QUEUE.lock().unwrap() = None;
    }
}

// Expose raw pointers for the C# bridge to read
#[no_mangle]
pub extern "C" fn unterm_get_unity_device() -> *mut c_void {
    UNITY_METAL_DEVICE
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| Retained::as_ptr(r) as *const _ as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "C" fn unterm_get_unity_queue() -> *mut c_void {
    UNITY_METAL_QUEUE
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| Retained::as_ptr(r) as *const _ as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

// ---- Shadow-copied DLL: receive raw pointers from C# bridge ----

static SHADOW_METAL_DEVICE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static SHADOW_METAL_QUEUE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

#[no_mangle]
pub unsafe extern "C" fn unterm_set_unity_device(
    device: *mut c_void,
    queue: *mut c_void,
) {
    SHADOW_METAL_DEVICE.store(device, Ordering::Release);
    SHADOW_METAL_QUEUE.store(queue, Ordering::Release);
}

pub fn shadow_device_ptr() -> *mut c_void {
    SHADOW_METAL_DEVICE.load(Ordering::Acquire)
}

pub fn shadow_queue_ptr() -> *mut c_void {
    SHADOW_METAL_QUEUE.load(Ordering::Acquire)
}

