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

mod agentview;
mod browser;
mod clock;
mod control;
mod diff;
mod editops;
mod editorview;
mod gpu;
mod highlight;
mod input;
#[cfg(target_os = "macos")]
mod iosurface;
mod keys;
mod markdown;
mod mcp;
mod palette;
mod panel;
#[cfg(any(target_os = "macos", windows))]
mod popup;
mod pty;
mod quads;
mod renderer;
mod sessions;
mod shell;
#[cfg(any(target_os = "macos", windows))]
mod sound;
mod surface;
mod term;
mod unity;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use agentview::AgentView;
use editorview::EditorView;
use term::Terminal;

/// Tees log output to stderr *and* a file. env_logger writes only to stderr,
/// which a GUI Unity on Windows surfaces nowhere (no console, and Unity doesn't
/// capture a native plugin's stderr there), so the file is the only place the
/// native logs are recoverable. The file lives at `<temp>/unterm.log`.
struct Tee(std::fs::File);

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        self.0.flush()
    }
}

/// The native log file path (`<temp>/unterm.log`), e.g. `%TEMP%\unterm.log` on
/// Windows. Truncated once per process so each editor session starts fresh.
fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("unterm.log")
}

/// Initialize logging once. Safe to call repeatedly.
fn init_log() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Show our own logs (the agent control protocol, claude's stderr, the D3D
        // surface status, …) without needing RUST_LOG set, so a GUI Unity — notably
        // on Windows, where there's no console — is diagnosable; dependencies stay
        // at `error` to avoid flooding. RUST_LOG still overrides.
        let env = env_logger::Env::default().default_filter_or("error,unterm=info");
        let mut builder = env_logger::Builder::from_env(env);
        if let Ok(file) = std::fs::File::create(log_path()) {
            builder.target(env_logger::Target::Pipe(Box::new(Tee(file))));
        }
        let _ = builder.try_init();
    });
}

type Registry = HashMap<u64, Box<Terminal>>;

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering a poisoned mutex instead of panicking. A panic
/// caught at the FFI boundary (see [`ffi_guard`]) can poison this lock; without
/// recovery every later call would then panic on `.unwrap()` and the terminals
/// would be wedged.
fn lock_registry() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock a mutex, recovering from poisoning instead of panicking — the same policy
/// [`lock_registry`] applies, reusable on any mutex. A panic in a worker thread
/// (the agent reader in `control`, the PTY pump, an MCP responder) while it holds
/// a lock poisons that mutex; without recovery the next `.lock().unwrap()` on the
/// FFI thread would panic in turn. That panic is contained by [`ffi_guard`] so
/// Unity survives, but it recurs on *every* later call, permanently wedging the
/// session. Recovering the poisoned guard keeps the session usable instead.
pub(crate) trait LockRecover<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for std::sync::Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Run `f`, swallowing any panic and returning `default` instead. The exported
/// `unterm_*` functions are `extern "C"`; letting a Rust panic unwind across the
/// C ABI into Unity is undefined behavior (and crashes the editor). wgpu panics
/// on uncaptured GPU errors (e.g. a lost/again device on weak drivers), so the
/// render/readback paths in particular must be guarded.
fn ffi_guard<R>(default: R, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            log::error!("unterm: recovered from a panic at the FFI boundary");
            default
        }
    }
}

/// Next free id: one past the highest id currently in the registry. There is no
/// persistent counter, so ids naturally restart from the live set each launch —
/// and a restored terminal re-claims its *own* id (see `restore_id`), so a stale
/// serialized id is never silently handed to a different, fresh terminal.
fn alloc_id(reg: &Registry) -> u64 {
    reg.keys().max().copied().unwrap_or(0) + 1
}

/// Id for a restored terminal: its original `hint` if still free, else a fresh
/// one. Keeping the original id means a re-adopting window finds its own terminal
/// and never collides with another window's.
fn restore_id(reg: &Registry, hint: u64) -> u64 {
    if hint != 0 && !reg.contains_key(&hint) {
        hint
    } else {
        alloc_id(reg)
    }
}

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

/// Run `f` against the terminal with `id`, returning `default` if absent or if
/// `f` panics (the panic is contained so it can't unwind into Unity).
fn with_term<R>(id: u64, default: R, f: impl FnOnce(&mut Terminal) -> R) -> R {
    // `None` distinguishes absent/panicked from a real result; both fall back to
    // `default` without needing it inside the guarded closure.
    let ran = ffi_guard(None, || lock_registry().get_mut(&id).map(|t| f(t)));
    ran.unwrap_or(default)
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
    ffi_guard(0, || {
        let cwd = cstr(cwd);
        let terminal = Terminal::new(width, height, scale, &cwd, "");
        let mut reg = lock_registry();
        let id = alloc_id(&reg);
        reg.insert(id, Box::new(terminal));
        id
    })
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
    ffi_guard(0, || {
        let cwd = cstr(cwd);
        let command = cstr(command);
        let terminal = Terminal::new(width, height, scale, &cwd, &command);
        let mut reg = lock_registry();
        let id = alloc_id(&reg);
        reg.insert(id, Box::new(terminal));
        id
    })
}

/// Like `unterm_create`, but first seeds the grid with `seed` (terminal text,
/// e.g. SGR-styled restored scrollback) above a fresh interactive shell. Used to
/// restore a live session across an editor restart. `id` is the terminal's id from
/// the previous run, re-claimed if free (so a re-adopting window finds its own).
/// Returns a stable id (0 on error).
#[no_mangle]
pub unsafe extern "C" fn unterm_create_seeded(
    id: u64,
    width: u32,
    height: u32,
    scale: f32,
    cwd: *const c_char,
    seed: *const c_char,
) -> u64 {
    init_log();
    ffi_guard(0, || {
        let cwd = cstr(cwd);
        let seed = cstr(seed);
        let terminal = Terminal::new_seeded(width, height, scale, &cwd, &seed);
        let mut reg = lock_registry();
        let id = restore_id(&reg, id);
        reg.insert(id, Box::new(terminal));
        id
    })
}

/// Create a display-only terminal: seeds the grid with `seed` but spawns no shell
/// and is marked exited. Restores the final screen of a terminal that had already
/// exited before an editor restart. `id` is re-claimed if free (see
/// `unterm_create_seeded`). Returns a stable id (0 on error).
#[no_mangle]
pub unsafe extern "C" fn unterm_create_dead(
    id: u64,
    width: u32,
    height: u32,
    scale: f32,
    seed: *const c_char,
) -> u64 {
    init_log();
    ffi_guard(0, || {
        let seed = cstr(seed);
        let terminal = Terminal::new_dead(width, height, scale, &seed);
        let mut reg = lock_registry();
        let id = restore_id(&reg, id);
        reg.insert(id, Box::new(terminal));
        id
    })
}

/// Whether a terminal id is still live (used to re-adopt after a reload).
#[no_mangle]
pub extern "C" fn unterm_exists(id: u64) -> bool {
    lock_registry().contains_key(&id)
}

/// The whole buffer (scrollback + screen) as truecolor-SGR text. Writes the byte
/// length; the pointer is valid until the next `unterm_dump` on this terminal.
/// Used to save a session for restore across an editor restart.
#[no_mangle]
pub unsafe extern "C" fn unterm_dump(id: u64, out_len: *mut usize) -> *const u8 {
    with_term(id, std::ptr::null(), |t| {
        let bytes = t.dump_cstr().to_bytes();
        if !out_len.is_null() {
            unsafe { *out_len = bytes.len() };
        }
        bytes.as_ptr()
    })
}

/// The shell's current working directory (UTF-8). Writes the byte length; the
/// pointer is valid until the next `unterm_cwd` on this terminal. Empty if there's
/// no live shell. Used to restore the cwd on resume across an editor restart.
#[no_mangle]
pub unsafe extern "C" fn unterm_cwd(id: u64, out_len: *mut usize) -> *const u8 {
    with_term(id, std::ptr::null(), |t| {
        let bytes = t.cwd_cstr().to_bytes();
        if !out_len.is_null() {
            unsafe { *out_len = bytes.len() };
        }
        bytes.as_ptr()
    })
}

/// Destroy a terminal (kills its shell and frees the GPU surface).
#[no_mangle]
pub extern "C" fn unterm_destroy(id: u64) {
    lock_registry().remove(&id);
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

/// Set the in-progress IME composition drawn at the cursor (null/empty clears).
/// This is display-only — committed text still arrives via `unterm_send_text`.
#[no_mangle]
pub unsafe extern "C" fn unterm_set_preedit(id: u64, text: *const c_char) {
    let s = if text.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    };
    with_term(id, (), |t| t.set_preedit(&s));
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
    let mut map = lock_registry();
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

/// Advance the render-target swapchain on a host tick: promotes a finished frame
/// to the front even while idle (double-buffered zero-copy). Returns true if the
/// displayed buffer changed, so the host can repaint.
#[no_mangle]
pub extern "C" fn unterm_present(id: u64) -> bool {
    with_term(id, false, |t| t.advance())
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

/// Write the current pixel size into `width`/`height` (either may be null).
#[no_mangle]
pub unsafe extern "C" fn unterm_size(id: u64, width: *mut u32, height: *mut u32) {
    let map = lock_registry();
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
    let map = lock_registry();
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
    let map = lock_registry();
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
    let map = lock_registry();
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
    let mut map = lock_registry();
    let Some(t) = map.get_mut(&id) else {
        return std::ptr::null();
    };
    let c = t.title_cstr();
    if !out_len.is_null() {
        unsafe { *out_len = c.to_bytes().len() };
    }
    c.as_ptr()
}

// ===========================================================================
// Shared MCP server, held in native globals so it survives C# domain reloads:
// the tool catalog and the queued calls stay alive across recompiles. There is
// one server per editor; the Unity side supplies the tool catalog and executes
// queued calls on its main thread. The agent connects in-process over the
// control protocol, so there is no socket and no URL — `mcp_message` requests
// are dispatched straight into the queue.
// ===========================================================================

fn mcp_call_snap() -> &'static Mutex<CString> {
    static C: OnceLock<Mutex<CString>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(CString::default()))
}

/// A transport-free handle onto the in-process MCP server's queue for the
/// control-protocol driver. Dispatches land in the same queue the Unity side
/// drains over the FFI below. There is no socket to fail to bind, so this never
/// fails — the `Option` only keeps the driver's signature uniform.
fn ensure_mcp_dispatcher() -> Option<crate::mcp::McpDispatcher> {
    Some(crate::mcp::dispatcher())
}

/// Publish the tool list (JSON array of {name,description,inputSchema}).
#[no_mangle]
pub unsafe extern "C" fn unterm_mcp_set_tools(json: *const c_char) {
    let json = cstr(json);
    crate::mcp::dispatcher().set_tools(&json);
}

/// Pop the next queued tool call as `{id,name,args}` JSON, or null if none.
#[no_mangle]
pub unsafe extern "C" fn unterm_mcp_next_call(out_len: *mut usize) -> *const c_char {
    let Some(call) = crate::mcp::dispatcher().next_call() else {
        return std::ptr::null();
    };
    let mut snap = mcp_call_snap().lock_recover();
    *snap = CString::new(call).unwrap_or_default();
    if !out_len.is_null() {
        unsafe { *out_len = snap.as_bytes().len() };
    }
    let ptr = snap.as_ptr();
    drop(snap);
    ptr
}

/// Answer a tool call (id from next_call) with the MCP tool result JSON.
#[no_mangle]
pub unsafe extern "C" fn unterm_mcp_respond(id: u64, result_json: *const c_char) {
    let result = cstr(result_json);
    crate::mcp::dispatcher().respond(id, &result);
}
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);













// ===========================================================================
// Agent view: a single id-handled object owning the conversation, the transcript
// renderer, and the composer. It composes the transcript (history + pending +
// indicator), renders both surfaces, draws/resolves all buttons, and routes
// input — so the host knows nothing about the agent. It lives in a process-global
// registry keyed by a stable id, so it survives C# domain reloads (the host
// re-adopts it by id and re-applies size/theme/fonts).
// ===========================================================================

type ViewMap = HashMap<u64, Box<AgentView>>;

fn views() -> &'static Mutex<ViewMap> {
    static V: OnceLock<Mutex<ViewMap>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the view map, recovering a poisoned mutex instead of panicking — a panic
/// caught at the FFI boundary can poison it, and `.unwrap()` would then abort Unity.
fn lock_views() -> std::sync::MutexGuard<'static, ViewMap> {
    views().lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f` against the agent view with `id`, returning `default` if absent or if `f`
/// panics — cosmic-text can panic on a malformed edit (e.g. an IME composition over
/// a buffer that changed underneath), and letting that unwind across the C ABI would
/// abort the editor. Mirrors [`with_term`]/`with_editor`.
fn with_view<R>(id: u64, default: R, f: impl FnOnce(&mut AgentView) -> R) -> R {
    ffi_guard(None, || lock_views().get_mut(&id).map(|v| f(v))).unwrap_or(default)
}

/// Create a fresh agent view rooted at `cwd`, wired to the shared MCP server.
/// `pw/ph` and `iw/ih` are the transcript and composer surface sizes (physical
/// px). Returns a stable id the host persists to re-adopt across reloads.
///
/// # Safety
/// `cwd`/`claude_cmd` must be valid C strings or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_create(
    cwd: *const c_char,
    pw: u32,
    ph: u32,
    iw: u32,
    ih: u32,
    effort: *const c_char,
    claude_cmd: *const c_char,
) -> u64 {
    init_log();
    let mcp = ensure_mcp_dispatcher();
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let v = AgentView::new(cstr(cwd), mcp, None, pw.max(1), ph.max(1), iw.max(1), ih.max(1), cstr(effort), cstr(claude_cmd));
    lock_views().insert(id, Box::new(v));
    id
}

/// Like `unterm_agentview_create`, but resumes the prior conversation `resume`.
///
/// # Safety
/// `cwd`/`resume`/`claude_cmd` must be valid C strings or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_load(
    cwd: *const c_char,
    resume: *const c_char,
    pw: u32,
    ph: u32,
    iw: u32,
    ih: u32,
    effort: *const c_char,
    claude_cmd: *const c_char,
) -> u64 {
    init_log();
    let mcp = ensure_mcp_dispatcher();
    let resume = {
        let s = cstr(resume);
        (!s.is_empty()).then_some(s)
    };
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let v = AgentView::new(cstr(cwd), mcp, resume, pw.max(1), ph.max(1), iw.max(1), ih.max(1), cstr(effort), cstr(claude_cmd));
    lock_views().insert(id, Box::new(v));
    id
}

/// Whether a view id is still live (to re-adopt after a reload).
#[no_mangle]
pub extern "C" fn unterm_agentview_exists(id: u64) -> bool {
    lock_views().contains_key(&id)
}

/// Destroy a view (ends its worker, detaches the subprocess, frees surfaces).
#[no_mangle]
pub extern "C" fn unterm_agentview_destroy(id: u64) {
    lock_views().remove(&id);
}

/// Pull driver state and report what changed: bit0 = dirty (render+repaint),
/// bit1 = animating (keep repainting).
#[no_mangle]
pub extern "C" fn unterm_agentview_poll(id: u64) -> u32 {
    // Guard like `unterm_render`/`with_view`: polling advances the driver and can
    // touch cosmic-text, so a panic must not unwind across the C ABI into Unity.
    ffi_guard(0, || match lock_views().get_mut(&id) {
        Some(v) => v.poll(),
        None => 0,
    })
}

/// Compose + render both surfaces.
#[no_mangle]
pub extern "C" fn unterm_agentview_render(id: u64) {
    // wgpu can panic on a lost/again device and glyphon on a full atlas; contain
    // it here so the render path matches the guarantee `unterm_render` gives the
    // terminal (letting it unwind across the C ABI would abort the editor).
    ffi_guard((), || {
        if let Some(v) = lock_views().get_mut(&id) {
            v.render();
        }
    });
}

/// Resize both surfaces and set the HiDPI scale.
#[no_mangle]
pub extern "C" fn unterm_agentview_resize(id: u64, pw: u32, ph: u32, iw: u32, ih: u32, scale: f32) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.resize(pw.max(1), ph.max(1), iw.max(1), ih.max(1), scale);
    }
}

/// Apply the editor theme (background rgba + foreground rgb) to both surfaces.
#[no_mangle]
pub extern "C" fn unterm_agentview_set_theme(
    id: u64,
    br: f64,
    bg: f64,
    bb: f64,
    ba: f64,
    fr: u8,
    fg: u8,
    fb: u8,
) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.set_theme(br, bg, bb, ba, fr, fg, fb);
    }
}

/// Load the Regular/Bold/Italic/BoldItalic faces (empty = skip).
///
/// # Safety
/// Each path must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_set_fonts(
    id: u64,
    regular: *const c_char,
    bold: *const c_char,
    italic: *const c_char,
    bold_italic: *const c_char,
) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.set_fonts(&cstr(regular), &cstr(bold), &cstr(italic), &cstr(bold_italic));
    }
}

/// Raw `id<MTLTexture>` of the transcript surface (for Unity zero-copy).
#[no_mangle]
pub extern "C" fn unterm_agentview_panel_texture(id: u64) -> *mut c_void {
    match lock_views().get(&id) {
        Some(v) => v.panel_texture(),
        None => std::ptr::null_mut(),
    }
}

/// Raw `id<MTLTexture>` of the composer surface.
#[no_mangle]
pub extern "C" fn unterm_agentview_input_texture(id: u64) -> *mut c_void {
    match lock_views().get(&id) {
        Some(v) => v.input_texture(),
        None => std::ptr::null_mut(),
    }
}


/// Transcript content height in physical px (for the host scrollbar).
#[no_mangle]
pub extern "C" fn unterm_agentview_content_height(id: u64) -> f32 {
    lock_views().get(&id).map_or(0.0, |v| v.content_height())
}

/// Composer content height in physical px (for host auto-grow).
#[no_mangle]
pub extern "C" fn unterm_agentview_input_height(id: u64) -> f32 {
    lock_views().get(&id).map_or(0.0, |v| v.input_height())
}

/// Set the vertical transcript scroll (physical px, 0 = latest).
#[no_mangle]
pub extern "C" fn unterm_agentview_set_scroll(id: u64, scroll: f32) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.set_scroll(scroll);
    }
}

/// Composer caret rect in physical px (for host IME positioning).
///
/// # Safety
/// Out pointers must be writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_caret(
    id: u64,
    x: *mut f32,
    y: *mut f32,
    w: *mut f32,
    h: *mut f32,
) {
    if let Some(v) = lock_views().get(&id) {
        let r = v.caret_rect();
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
    }
}

/// Interrupt the in-flight turn (no-op if idle).
#[no_mangle]
pub extern "C" fn unterm_agentview_interrupt(id: u64) {
    if let Some(v) = lock_views().get(&id) {
        v.interrupt();
    }
}

/// Escape pressed: deny a pending tool-permission request, else interrupt the
/// in-flight turn.
#[no_mangle]
pub extern "C" fn unterm_agentview_escape(id: u64) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.escape();
    }
}

// --- session browser ("All Sessions", drawn in place of the transcript) -----

/// Enter/leave the native session browser. While on, the panel texture shows
/// the list and the composer becomes its search box.
#[no_mangle]
pub extern "C" fn unterm_agentview_set_browsing(id: u64, on: u8) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.set_browsing(on != 0);
    }
}

#[no_mangle]
pub extern "C" fn unterm_agentview_browsing(id: u64) -> u8 {
    lock_views().get(&id).map_or(0, |v| v.browsing() as u8)
}

/// Pointer motion over the browser list (physical px). Returns 1 when the
/// hover state changed (host should re-render + repaint).
#[no_mangle]
pub extern "C" fn unterm_agentview_browse_hover(id: u64, x: f32, y: f32) -> u8 {
    lock_views().get_mut(&id).map_or(0, |v| v.browse_hover(x, y) as u8)
}

/// Toggle listing archived sessions in the browser.
#[no_mangle]
pub extern "C" fn unterm_agentview_browse_toggle_archived(id: u64) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.browse_toggle_archived();
    }
}


/// How many of the browser's listed sessions are archived.
#[no_mangle]
pub extern "C" fn unterm_agentview_browse_archived_count(id: u64) -> u64 {
    lock_views().get(&id).map_or(0, |v| v.browse_archived_count())
}

/// Set the permission mode (`default`/`plan`/`acceptEdits`/`bypassPermissions`).
///
/// # Safety
/// `mode` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_set_permission_mode(id: u64, mode: *const c_char) {
    let mode = cstr(mode);
    if let Some(v) = lock_views().get(&id) {
        v.set_permission_mode(&mode);
    }
}

/// The current permission mode. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_permission_mode(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.permission_mode())
}

/// Set the model (alias like `opus`/`sonnet`/`haiku`, empty/`default` = engine default).
///
/// # Safety
/// `model` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_set_model(id: u64, model: *const c_char) {
    let model = cstr(model);
    if let Some(v) = lock_views().get(&id) {
        v.set_model(&model);
    }
}

/// The active model (user choice, else resolved from init). Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_model(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.model())
}

/// The model roster advertised by the engine's `initialize` reply, as a JSON array
/// of `{value, displayName, description, ...}` (empty until initialized). Lets the
/// host build its model picker from what the account is entitled to. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_models(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.models())
}

/// The slash-command roster from the `initialize` reply, as a JSON array of
/// `{name, description, argumentHint, aliases?}` (empty until initialized). Drives
/// the composer's `/` completion. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_commands(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.commands())
}

/// The leading `/command` token under the composer caret (with the slash), or empty
/// when not in slash-command context. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_slash_prefix(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.input_slash_prefix())
}

/// Accept a completion in the composer: delete `prefix_len` chars before the caret
/// and insert `text` in their place.
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_complete(id: u64, prefix_len: u32, text: *const c_char) {
    let text = cstr(text);
    with_view(id, (), |v| v.input_complete(prefix_len as usize, &text));
}

/// Number of follow-up prompts waiting in the queue.
#[no_mangle]
pub extern "C" fn unterm_agentview_queue_len(id: u64) -> u32 {
    lock_views().get(&id).map(|v| v.queue_len()).unwrap_or(0)
}

/// Cancel the `index`-th queued follow-up prompt (0-based).
#[no_mangle]
pub extern "C" fn unterm_agentview_cancel_queued(id: u64, index: u32) {
    if let Some(v) = lock_views().get(&id) {
        v.cancel_queued(index);
    }
}

/// The live Claude session id (empty until established). Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_session_id(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.session_id())
}

/// The session's title (first user line). Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_title(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.title())
}

/// A pending built-in command for the host to run in a real terminal (`/login`,
/// `/logout`), or empty if none. Consume-once: drained when read. Writes the
/// byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_take_host_command(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.take_host_command())
}

/// The whitespace-delimited token under (`x`, `y`) in the transcript (physical px),
/// for the host to open if it resolves to a file. Empty when not on a token.
/// `out_len` writable or null; pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_panel_token_at(id: u64, x: f32, y: f32, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.panel_token_at(x, y))
}

/// The unix stamp of the time separator under (`x`, `y`) in the transcript
/// (physical px), or 0 when not over one — the label is relative ("5 minutes
/// ago"), so the host uses this to show the exact time on hover.
#[no_mangle]
pub extern "C" fn unterm_agentview_panel_stamp_at(id: u64, x: f32, y: f32) -> u64 {
    with_view(id, 0, |v| v.panel_stamp_at(x, y))
}

/// Snapshot buffer for [`unterm_format_relative`] (not per-view; one global).
fn relative_snap() -> &'static Mutex<CString> {
    static C: OnceLock<Mutex<CString>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(CString::default()))
}

/// Format a unix-seconds stamp as the same localized relative label the
/// transcript separators use ("5 minutes ago"), for host UI like the session
/// picker. Writes the byte length; the pointer is valid until the next call.
///
/// # Safety
/// `out_len` must be writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_format_relative(unix_secs: u64, out_len: *mut usize) -> *const c_char {
    ffi_guard(std::ptr::null(), || {
        let label = clock::format_relative(unix_secs, clock::now_secs());
        let mut snap = relative_snap().lock_recover();
        *snap = CString::new(label).unwrap_or_default();
        if !out_len.is_null() {
            unsafe { *out_len = snap.as_bytes().len() };
        }
        snap.as_ptr()
    })
}

// --- session picker (async listing / search) ---

/// Submit an async request to list/search this project's Claude sessions:
/// `cwd` is the project root, `limit` caps the result count (0 = all), and
/// `query` (empty = list only) matches session titles and transcript bodies.
/// Returns a serial to [`unterm_sessions_poll`]; the worker never blocks the
/// caller. `cwd`/`query` must be valid C strings or null.
///
/// # Safety
/// `cwd` and `query` must be null or point to valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn unterm_sessions_query(
    cwd: *const c_char,
    limit: usize,
    query: *const c_char,
) -> u64 {
    ffi_guard(0, || sessions::query(&cstr(cwd), limit, &cstr(query)))
}

/// The session-directory generation: bumped when a session `.jsonl` is created
/// or removed in the background, so the host re-lists (the recent dropdown). The
/// browser watches it internally.
#[no_mangle]
pub extern "C" fn unterm_sessions_generation() -> u64 {
    ffi_guard(0, sessions::generation)
}

/// Newline-joined session ids currently driven by a live `claude` process in
/// `cwd`'s project (from Claude Code's own `~/.claude/sessions` registry), so the
/// host can grey out sessions open in another process. Valid until the next call.
///
/// # Safety
/// `cwd` must be null or a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn unterm_sessions_open_elsewhere(
    cwd: *const c_char,
    out_len: *mut usize,
) -> *const c_char {
    ffi_guard(std::ptr::null(), || {
        let joined = sessions::open_elsewhere(&cstr(cwd)).join("\n");
        let mut snap = open_elsewhere_snap().lock_recover();
        *snap = CString::new(joined).unwrap_or_default();
        if !out_len.is_null() {
            unsafe { *out_len = snap.as_bytes().len() };
        }
        snap.as_ptr()
    })
}

/// Snapshot buffer for [`unterm_sessions_open_elsewhere`] (main-thread only).
fn open_elsewhere_snap() -> &'static Mutex<CString> {
    static C: OnceLock<Mutex<CString>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(CString::default()))
}

/// Snapshot buffer for [`unterm_sessions_poll`] (one global; polled on the main thread).
fn sessions_snap() -> &'static Mutex<CString> {
    static C: OnceLock<Mutex<CString>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(CString::default()))
}

/// The JSON result for `serial` if ready, else null (still computing). The
/// pointer is valid until the next call. Writes the byte length to `out_len`.
///
/// # Safety
/// `out_len` must be writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_sessions_poll(serial: u64, out_len: *mut usize) -> *const c_char {
    ffi_guard(std::ptr::null(), || {
        let Some(json) = sessions::poll(serial) else {
            return std::ptr::null();
        };
        let mut snap = sessions_snap().lock_recover();
        *snap = CString::new(json).unwrap_or_default();
        if !out_len.is_null() {
            unsafe { *out_len = snap.as_bytes().len() };
        }
        snap.as_ptr()
    })
}

// --- transcript (panel) input ---

/// Mouse-down in the transcript. Returns 1 if consumed.
#[no_mangle]
pub extern "C" fn unterm_agentview_panel_down(id: u64, x: f32, y: f32) -> u8 {
    match lock_views().get_mut(&id) {
        Some(v) => v.panel_down(x, y) as u8,
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn unterm_agentview_panel_drag(id: u64, x: f32, y: f32) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.panel_drag(x, y);
    }
}

/// Horizontal scroll of the code block under (x, y). Returns 1 if consumed.
#[no_mangle]
pub extern "C" fn unterm_agentview_panel_scroll_h(id: u64, x: f32, y: f32, dx: f32) -> u8 {
    match lock_views().get_mut(&id) {
        Some(v) => v.panel_scroll_h(x, y, dx) as u8,
        None => 0,
    }
}

/// Vertical scroll of the capped plan box under (x, y). Returns 1 if consumed.
#[no_mangle]
pub extern "C" fn unterm_agentview_panel_scroll_v(id: u64, x: f32, y: f32, dy: f32) -> u8 {
    match lock_views().get_mut(&id) {
        Some(v) => v.panel_scroll_v(x, y, dy) as u8,
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn unterm_agentview_panel_select_all(id: u64) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.panel_select_all();
    }
}

#[no_mangle]
pub extern "C" fn unterm_agentview_panel_select_clear(id: u64) {
    if let Some(v) = lock_views().get_mut(&id) {
        v.panel_select_clear();
    }
}

#[no_mangle]
pub extern "C" fn unterm_agentview_panel_has_selection(id: u64) -> bool {
    matches!(lock_views().get(&id), Some(v) if v.panel_has_selection())
}

/// Whether a turn is actively running (sent a prompt, agent thinking/replying) —
/// not idle, initializing, or just resumed. Lets the host record real conversation
/// activity (vs merely opening/switching a session).
#[no_mangle]
pub extern "C" fn unterm_agentview_thinking(id: u64) -> bool {
    matches!(lock_views().get(&id), Some(v) if v.is_thinking())
}

/// Drain the one-shot "needs the user" signal: 0 none, 1 turn finished, 2 waiting
/// on a permission/decision. Consume-once — the host polls it each tick and, when
/// non-zero and the editor is backgrounded, chimes + notifies.
#[no_mangle]
pub extern "C" fn unterm_agentview_take_attention(id: u64) -> u32 {
    with_view(id, 0, |v| v.take_attention())
}

/// Play the bundled "agent done" chime. The host calls this when a turn finishes
/// while the window is in the background (it owns focus/visibility); no-op on
/// platforms without an audio backend.
#[no_mangle]
pub extern "C" fn unterm_play_agent_done() {
    ffi_guard((), || {
        #[cfg(any(target_os = "macos", windows))]
        sound::play_agent_done();
    });
}

/// Selected transcript text. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_panel_selected_text(
    id: u64,
    out_len: *mut usize,
) -> *const c_char {
    view_string(id, out_len, |v| v.panel_selected_text())
}

// --- composer (input) input ---

/// Mouse-down in the composer. `kind`: 0 click, 2 double, 3 triple. Returns 1
/// if the Send/Stop button was hit (action performed; host should not drag).
#[no_mangle]
pub extern "C" fn unterm_agentview_input_down(id: u64, x: f32, y: f32, kind: u8) -> u8 {
    with_view(id, 0, |v| v.input_down(x, y, kind) as u8)
}

#[no_mangle]
pub extern "C" fn unterm_agentview_input_drag(id: u64, x: f32, y: f32) {
    with_view(id, (), |v| v.input_drag(x, y));
}

/// A composer key (Enter sends, Shift+Enter newlines, rest edits).
///
/// # Safety
/// `name` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_key(
    id: u64,
    name: *const c_char,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
    let name = cstr(name);
    with_view(id, (), |v| v.input_key(&name, ctrl, alt, shift));
}

/// Host keyboard focus for the composer; hides the caret while unfocused.
#[no_mangle]
pub extern "C" fn unterm_agentview_set_focus(id: u64, focused: bool) {
    with_view(id, (), |v| v.set_focused(focused));
}

/// Insert text into the composer (paste / IME commit).
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_insert(id: u64, text: *const c_char) {
    let text = cstr(text);
    with_view(id, (), |v| v.input_insert(&text));
}

/// Set the live IME composition shown inline as marked text (empty clears it).
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_set_preedit(id: u64, text: *const c_char) {
    let text = cstr(text);
    with_view(id, (), |v| v.input_set_preedit(&text));
}

#[no_mangle]
pub extern "C" fn unterm_agentview_input_undo(id: u64) {
    with_view(id, (), |v| v.input_undo());
}

#[no_mangle]
pub extern "C" fn unterm_agentview_input_redo(id: u64) {
    with_view(id, (), |v| v.input_redo());
}

#[no_mangle]
pub extern "C" fn unterm_agentview_input_select_all(id: u64) {
    with_view(id, (), |v| v.input_select_all());
}

/// Copy the composer selection to a snapshot (host writes the OS clipboard).
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_copy(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.input_copy())
}

/// Cut the composer selection to a snapshot (host writes the OS clipboard).
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_cut(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.input_cut())
}

/// The composer's current text. Writes the byte length.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this view.
#[no_mangle]
pub unsafe extern "C" fn unterm_agentview_input_text(id: u64, out_len: *mut usize) -> *const c_char {
    view_string(id, out_len, |v| v.input_text())
}

/// Shared helper for the cached-CString accessors.
unsafe fn view_string(
    id: u64,
    out_len: *mut usize,
    f: impl FnOnce(&mut AgentView) -> &CString,
) -> *const c_char {
    let mut map = lock_views();
    let Some(v) = map.get_mut(&id) else {
        return std::ptr::null();
    };
    let c = f(v);
    if !out_len.is_null() {
        unsafe { *out_len = c.as_bytes().len() };
    }
    let ptr = c.as_ptr();
    drop(map);
    ptr
}





















































// ===========================================================================
// Code editor view: an id-handled editing surface (tree-sitter highlighting +
// line-number gutter) the Unity side blits and drives. Lives in its own
// process-global registry keyed by a stable id so unsaved edits survive C#
// domain reloads (the host re-adopts by id). The file path / dirty state are
// C#-side; this owns only the surface, language, and theme.
// ===========================================================================

type EditorMap = HashMap<u64, Box<EditorView>>;

fn editors() -> &'static Mutex<EditorMap> {
    static E: OnceLock<Mutex<EditorMap>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the editor registry, recovering a poisoned mutex instead of panicking —
/// the same reasoning as [`lock_registry`]: a panic caught at the FFI boundary
/// (see [`with_editor`]) must not wedge every later call.
fn lock_editors() -> std::sync::MutexGuard<'static, EditorMap> {
    editors().lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f` against the editor with `id`, contained by [`ffi_guard`] so a panic in
/// cosmic-text (e.g. an out-of-range selection during an edit) or wgpu can't
/// unwind across the C ABI and abort Unity.
fn with_editor<R>(id: u64, default: R, f: impl FnOnce(&mut EditorView) -> R) -> R {
    ffi_guard(None, || lock_editors().get_mut(&id).map(|e| f(e))).unwrap_or(default)
}

static NEXT_EDITOR_ID: AtomicU64 = AtomicU64::new(1);

/// Shared helper for the editor's cached-CString accessors.
unsafe fn editor_string(
    id: u64,
    out_len: *mut usize,
    f: impl FnOnce(&mut EditorView) -> &CString,
) -> *const c_char {
    // Guard like the other editor entry points: `cut` mutates (deletes the
    // selection) and could panic in cosmic-text; that must not abort Unity. The
    // returned pointer is into the editor's cached CString, valid until the next
    // call on this editor (so it outlives the dropped lock).
    ffi_guard(std::ptr::null(), || {
        let mut map = lock_editors();
        let Some(e) = map.get_mut(&id) else {
            return std::ptr::null();
        };
        let c = f(e);
        if !out_len.is_null() {
            unsafe { *out_len = c.as_bytes().len() };
        }
        c.as_ptr()
    })
}

/// Create a code-editor surface sized `width`x`height` physical px at `scale`.
/// Returns a stable id the host persists to re-adopt across reloads (0 on error).
#[no_mangle]
pub extern "C" fn unterm_editor_create(width: u32, height: u32, scale: f32) -> u64 {
    init_log();
    ffi_guard(0, || {
        let v = EditorView::new(width.max(1), height.max(1), scale);
        let id = NEXT_EDITOR_ID.fetch_add(1, Ordering::Relaxed);
        lock_editors().insert(id, Box::new(v));
        id
    })
}

/// Whether an editor id is still live (to re-adopt after a reload).
#[no_mangle]
pub extern "C" fn unterm_editor_exists(id: u64) -> bool {
    lock_editors().contains_key(&id)
}

/// Destroy an editor surface (frees its GPU target; unsaved text is lost).
#[no_mangle]
pub extern "C" fn unterm_editor_destroy(id: u64) {
    lock_editors().remove(&id);
}

/// Resize to `width`x`height` physical px at `scale`.
#[no_mangle]
pub extern "C" fn unterm_editor_resize(id: u64, width: u32, height: u32, scale: f32) {
    if let Some(e) = lock_editors().get_mut(&id) {
        e.resize(width.max(1), height.max(1), scale);
    }
}

/// Set the HiDPI scale (pixels per point) without changing the pixel size.
#[no_mangle]
pub extern "C" fn unterm_editor_set_scale(id: u64, scale: f32) {
    if let Some(e) = lock_editors().get_mut(&id) {
        e.set_scale(scale);
    }
}

/// Set the undo-history cap (number of retained steps; 0 = unlimited).
#[no_mangle]
pub extern "C" fn unterm_editor_set_undo_limit(id: u64, limit: u32) {
    if let Some(e) = lock_editors().get_mut(&id) {
        e.set_undo_limit(limit as usize);
    }
}

/// Load a monospace font file (TTF/OTF/TTC) or address a system family by name.
///
/// # Safety
/// `path` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_font(id: u64, path: *const c_char) {
    let path = cstr(path);
    if let Some(e) = lock_editors().get_mut(&id) {
        e.set_font(&path);
    }
}

/// Background rgba + foreground rgb, plus whether to use the dark highlight theme.
#[no_mangle]
pub extern "C" fn unterm_editor_set_theme(
    id: u64,
    br: f64,
    bg: f64,
    bb: f64,
    ba: f64,
    fr: u8,
    fg: u8,
    fb: u8,
    dark: bool,
) {
    if let Some(e) = lock_editors().get_mut(&id) {
        e.set_theme(br, bg, bb, ba, fr, fg, fb, dark);
    }
}

/// Set the tree-sitter language token (e.g. "cs"); empty/unknown = plain.
///
/// # Safety
/// `token` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_language(id: u64, token: *const c_char) {
    let token = cstr(token);
    with_editor(id, (), |e| e.set_language(&token));
}

/// Set the open file's path so the editor can read its git-index version and draw
/// diff gutter markers (empty/null = clear). Kicks a background fetch.
///
/// # Safety
/// `path` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_path(id: u64, path: *const c_char) {
    let path = cstr(path);
    with_editor(id, (), |e| e.set_path(&path));
}

/// Re-fetch the git base for the diff markers (call on focus / after save).
#[no_mangle]
pub extern "C" fn unterm_editor_refresh_diff(id: u64) {
    with_editor(id, (), |e| e.refresh_diff());
}

/// Toggle Markdown-preview mode: while on, `render`/`raw_texture`/scroll/mouse/
/// copy operate on a rendered-Markdown view of the live buffer (read-only).
#[no_mangle]
pub extern "C" fn unterm_editor_set_preview(id: u64, on: bool) {
    with_editor(id, (), |e| e.set_preview(on));
}

/// Whether Markdown-preview mode is currently on.
#[no_mangle]
pub extern "C" fn unterm_editor_preview_active(id: u64) -> bool {
    with_editor(id, false, |e| e.preview_active())
}

/// The existing-file path token under (x, y) in preview mode (for click-to-open),
/// or an empty string. The pointer is valid until the next call on this editor.
///
/// # Safety
/// `out_len` must be a valid pointer or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_preview_token_at(
    id: u64,
    x: f32,
    y: f32,
    out_len: *mut usize,
) -> *const c_char {
    editor_string(id, out_len, |e| e.preview_token_at(x, y))
}

/// Apply any finished background git fetch; returns true when new diff markers were
/// applied (the host should re-render). Cheap to poll each editor tick.
#[no_mangle]
pub extern "C" fn unterm_editor_poll_diff(id: u64) -> bool {
    with_editor(id, false, |e| e.poll_diff())
}

/// Render the editor surface into its IOSurface/shared texture.
#[no_mangle]
pub extern "C" fn unterm_editor_render(id: u64) {
    with_editor(id, (), |e| e.render());
}

/// Raw `id<MTLTexture>` of the editor surface (for Unity zero-copy).
#[no_mangle]
pub extern "C" fn unterm_editor_raw_texture(id: u64) -> *mut c_void {
    match lock_editors().get(&id) {
        Some(e) => e.raw_texture(),
        None => std::ptr::null_mut(),
    }
}

/// Laid-out content height in physical px (for the host scrollbar).
#[no_mangle]
pub extern "C" fn unterm_editor_content_height(id: u64) -> f32 {
    lock_editors().get(&id).map_or(0.0, |e| e.content_height())
}

/// Document-state version: a monotonic id that undo/redo restore. The host records
/// it at save and compares to detect unsaved changes (no second buffer copy).
#[no_mangle]
pub extern "C" fn unterm_editor_edit_serial(id: u64) -> u64 {
    lock_editors().get(&id).map_or(0, |e| e.edit_serial())
}

/// Caret rect in physical px into x/y/w/h (any pointer may be null).
///
/// # Safety
/// Out pointers must be writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_caret(
    id: u64,
    x: *mut f32,
    y: *mut f32,
    w: *mut f32,
    h: *mut f32,
) {
    if let Some(e) = lock_editors().get(&id) {
        let r = e.caret_rect();
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
    }
}

/// Apply a named editing key (arrows/Home/End/Return/Backspace/Delete).
///
/// # Safety
/// `name` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_key(
    id: u64,
    name: *const c_char,
    ctrl: bool,
    alt: bool,
    shift: bool,
) {
    let name = cstr(name);
    with_editor(id, (), |e| e.key(&name, ctrl, alt, shift));
}

/// Host keyboard focus for the code editor; hides the caret while unfocused.
#[no_mangle]
pub extern "C" fn unterm_editor_set_focus(id: u64, focused: bool) {
    with_editor(id, (), |e| e.set_focused(focused));
}

/// Insert text at the caret (typing / paste / IME commit).
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_insert(id: u64, text: *const c_char) {
    let text = cstr(text);
    with_editor(id, (), |e| e.insert(&text));
}

/// Set the live IME composition shown inline as marked text (empty clears it).
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_preedit(id: u64, text: *const c_char) {
    let text = cstr(text);
    with_editor(id, (), |e| e.set_preedit(&text));
}

/// Replace the whole buffer (e.g. on opening a file).
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_text(id: u64, text: *const c_char) {
    let text = cstr(text);
    with_editor(id, (), |e| e.set_text(&text));
}

/// Add `using <ns>;` near the top of the file if not already imported (one undoable
/// edit; caret preserved). For completion's auto-import of an unimported type.
///
/// # Safety
/// `ns` is a valid NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_add_using(id: u64, ns: *const c_char) {
    let ns = cstr(ns);
    with_editor(id, (), |e| e.add_using(&ns));
}

/// The editor's current text. Writes the byte length; the pointer is valid until
/// the next call on this editor.
///
/// # Safety
/// `out_len` writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_text(id: u64, out_len: *mut usize) -> *const c_char {
    editor_string(id, out_len, |e| e.text())
}

#[no_mangle]
pub extern "C" fn unterm_editor_undo(id: u64) {
    with_editor(id, (), |e| e.undo());
}

#[no_mangle]
pub extern "C" fn unterm_editor_redo(id: u64) {
    with_editor(id, (), |e| e.redo());
}

#[no_mangle]
pub extern "C" fn unterm_editor_select_all(id: u64) {
    with_editor(id, (), |e| e.select_all());
}

/// Copy the selection to a snapshot (host writes the OS clipboard). Writes len.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this editor.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_copy(id: u64, out_len: *mut usize) -> *const c_char {
    editor_string(id, out_len, |e| e.copy())
}

/// Cut the selection to a snapshot (host writes the OS clipboard). Writes len.
///
/// # Safety
/// `out_len` writable or null. Pointer valid until the next call on this editor.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_cut(id: u64, out_len: *mut usize) -> *const c_char {
    editor_string(id, out_len, |e| e.cut())
}

/// Mouse at physical px: kind 0 click, 1 drag, 2 double-click, 3 triple-click.
#[no_mangle]
pub extern "C" fn unterm_editor_mouse(id: u64, x: f32, y: f32, kind: u8) {
    with_editor(id, (), |e| e.mouse(x, y, kind));
}

/// Pointer moved (no button) at physical px: show/hide the diff-peek tooltip.
/// Returns true when the host should re-render.
#[no_mangle]
pub extern "C" fn unterm_editor_hover(id: u64, x: f32, y: f32) -> bool {
    with_editor(id, false, |e| e.hover(x, y))
}

/// The git-diff hunk index a click at physical px (`x`, `y`) targets, or -1 when
/// the click isn't on a gutter marker. The host opens a Stage/Unstage/Revert menu
/// for that hunk.
#[no_mangle]
pub extern "C" fn unterm_editor_hunk_at(id: u64, x: f32, y: f32) -> i32 {
    with_editor(id, -1, |e| e.hunk_at(x, y))
}

/// Whether hunk `hunk_i` is already staged in the git index (the marker draws
/// hollow); drives the host menu's Stage vs Unstage item.
#[no_mangle]
pub extern "C" fn unterm_editor_hunk_staged(id: u64, hunk_i: u32) -> bool {
    with_editor(id, false, |e| e.hunk_staged(hunk_i as usize))
}

/// Whether any staged content overlaps hunk `hunk_i` — also true for a partially
/// staged region (staged then edited further), where the menu offers Unstage
/// alongside Stage.
#[no_mangle]
pub extern "C" fn unterm_editor_hunk_has_staged(id: u64, hunk_i: u32) -> bool {
    with_editor(id, false, |e| e.hunk_has_staged(hunk_i as usize))
}

/// Whether hunk `hunk_i` is staged-only (the change lives in the index; the buffer
/// is back at HEAD). Reverting the buffer is a no-op there, so the host menu hides
/// Revert and offers just Unstage.
#[no_mangle]
pub extern "C" fn unterm_editor_hunk_staged_only(id: u64, hunk_i: u32) -> bool {
    with_editor(id, false, |e| e.hunk_staged_only(hunk_i as usize))
}

/// Stage hunk `hunk_i` to the git index (like `git add -p` for that hunk). Returns
/// true on success; refreshes the git texts so the marker then redraws hollow.
#[no_mangle]
pub extern "C" fn unterm_editor_stage_hunk(id: u64, hunk_i: u32) -> bool {
    with_editor(id, false, |e| e.stage_hunk(hunk_i as usize))
}

/// Unstage hunk `hunk_i` (like `git restore --staged -p` for that hunk; the buffer
/// is untouched). Returns true on success.
#[no_mangle]
pub extern "C" fn unterm_editor_unstage_hunk(id: u64, hunk_i: u32) -> bool {
    with_editor(id, false, |e| e.unstage_hunk(hunk_i as usize))
}

/// Revert hunk `hunk_i` to its git-base content (one undoable buffer edit; leaves the
/// document dirty until saved).
#[no_mangle]
pub extern "C" fn unterm_editor_revert_hunk(id: u64, hunk_i: u32) {
    with_editor(id, (), |e| e.revert_hunk(hunk_i as usize));
}

/// Scroll vertically by `dy` physical px (mouse wheel).
#[no_mangle]
pub extern "C" fn unterm_editor_scroll(id: u64, dy: f32) {
    with_editor(id, (), |e| e.scroll(dy));
}

/// Scroll horizontally by `dx` physical px (wheel/trackpad).
#[no_mangle]
pub extern "C" fn unterm_editor_scroll_h(id: u64, dx: f32) {
    with_editor(id, (), |e| e.scroll_h(dx));
}

/// Set the absolute vertical scroll offset (physical px), e.g. from a scrollbar.
#[no_mangle]
pub extern "C" fn unterm_editor_set_scroll(id: u64, px: f32) {
    with_editor(id, (), |e| e.set_scroll(px));
}

/// The current vertical scroll offset (physical px).
#[no_mangle]
pub extern "C" fn unterm_editor_scroll_offset(id: u64) -> f32 {
    lock_editors().get(&id).map_or(0.0, |e| e.scroll_offset())
}

/// Indent the selected lines (or the caret line) by one level.
#[no_mangle]
pub extern "C" fn unterm_editor_indent(id: u64) {
    with_editor(id, (), |e| e.indent());
}

/// Outdent the selected lines (or the caret line) by one level.
#[no_mangle]
pub extern "C" fn unterm_editor_outdent(id: u64) {
    with_editor(id, (), |e| e.outdent());
}

/// Toggle a line comment on the selected lines (or the caret line).
#[no_mangle]
pub extern "C" fn unterm_editor_toggle_comment(id: u64) {
    with_editor(id, (), |e| e.toggle_comment());
}

/// Move the caret line up one (swaps with the line above).
#[no_mangle]
pub extern "C" fn unterm_editor_move_line_up(id: u64) {
    with_editor(id, (), |e| e.move_line_up());
}

/// Move the caret line down one (swaps with the line below).
#[no_mangle]
pub extern "C" fn unterm_editor_move_line_down(id: u64) {
    with_editor(id, (), |e| e.move_line_down());
}

/// Duplicate the selected lines (or the caret line) below.
#[no_mangle]
pub extern "C" fn unterm_editor_duplicate_line(id: u64) {
    with_editor(id, (), |e| e.duplicate_line());
}

/// Delete the selected lines (or the caret line).
#[no_mangle]
pub extern "C" fn unterm_editor_delete_line(id: u64) {
    with_editor(id, (), |e| e.delete_line());
}

/// Move the caret to the start of line `line` (0-based).
#[no_mangle]
pub extern "C" fn unterm_editor_goto_line(id: u64, line: u32) {
    with_editor(id, (), |e| e.goto_line(line as usize));
}

/// Find `query` and select the match (search wraps). Returns true if found.
///
/// # Safety
/// `query` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_find(
    id: u64,
    query: *const c_char,
    forward: bool,
    case_sensitive: bool,
) -> bool {
    let query = cstr(query);
    with_editor(id, false, |e| e.find(&query, forward, case_sensitive))
}

/// Replace the current selection with `repl` (no-op without a selection).
///
/// # Safety
/// `repl` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_replace_selection(id: u64, repl: *const c_char) {
    let repl = cstr(repl);
    with_editor(id, (), |e| e.replace_selection(&repl));
}

/// Replace every occurrence of `query` with `repl`. Returns the count.
///
/// # Safety
/// `query`/`repl` must be valid C strings or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_replace_all(
    id: u64,
    query: *const c_char,
    repl: *const c_char,
    case_sensitive: bool,
) -> u32 {
    let query = cstr(query);
    let repl = cstr(repl);
    with_editor(id, 0, |e| e.replace_all(&query, &repl, case_sensitive))
}

/// The identifier prefix immediately before the caret (for autocomplete). Writes
/// the byte length; the pointer is valid until the next call on this editor.
///
/// # Safety
/// `out_len` writable or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_word_prefix(id: u64, out_len: *mut usize) -> *const c_char {
    editor_string(id, out_len, |e| e.word_prefix())
}

/// Set the autocomplete popup items (`\n`-joined; empty hides it) + selected index.
/// The Rust side renders the popup over the editor at the caret.
///
/// # Safety
/// `items` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_set_completions(id: u64, items: *const c_char, selected: u32) {
    let items = cstr(items);
    with_editor(id, (), |e| e.set_completions(&items, selected as usize));
}

/// The caret's absolute character offset in the document (for semantic completion).
#[no_mangle]
pub extern "C" fn unterm_editor_caret_offset(id: u64) -> u32 {
    lock_editors().get(&id).map_or(0, |e| e.caret_offset() as u32)
}

/// Show/refresh the native completion popup (a non-activating NSPanel) at screen
/// position (`x`,`y`) in physical px, top-left origin. `items` are '\n'-joined
/// `kind+label` lines (same encoding as `unterm_editor_set_completions`); `scale`
/// is pixels-per-point; background is (br,bg,bb) in 0..1 and text is (fr,fg,fb) in
/// 0..255. macOS only — the host must only call it there.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
pub extern "C" fn unterm_popup_show(
    items: *const c_char,
    selected: u32,
    scroll: u32,
    x: f32,
    y: f32,
    scale: f32,
    br: f32,
    bg: f32,
    bb: f32,
    fr: u8,
    fg: u8,
    fb: u8,
    dark: u8,
) {
    let items = cstr(items);
    let clear = wgpu::Color { r: br as f64, g: bg as f64, b: bb as f64, a: 1.0 };
    let text = glyphon::Color::rgb(fr, fg, fb);
    popup::show(&items, selected as usize, scroll as usize, x, y, scale, clear, text, dark != 0);
}

/// Like `unterm_popup_show`, but anchored ABOVE the caret (`x`,`y` is the caret TOP
/// in screen points). For a composer docked at the window bottom. macOS/Windows only.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn unterm_popup_show_above(
    items: *const c_char,
    selected: u32,
    scroll: u32,
    x: f32,
    y: f32,
    scale: f32,
    br: f32,
    bg: f32,
    bb: f32,
    fr: u8,
    fg: u8,
    fb: u8,
    dark: u8,
) {
    let items = cstr(items);
    let clear = wgpu::Color { r: br as f64, g: bg as f64, b: bb as f64, a: 1.0 };
    let text = glyphon::Color::rgb(fr, fg, fb);
    popup::show_above(&items, selected as usize, scroll as usize, x, y, scale, clear, text, dark != 0);
}

/// Hide the native completion popup. macOS only.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
pub extern "C" fn unterm_popup_hide() {
    popup::hide();
}

/// Show the native signature-help hint (anchored ABOVE the caret). `line` is the
/// full signature; `active_start`/`active_len` are CHAR offsets of the active
/// parameter within `line`. `x`/`y` are the caret TOP in screen points; `scale` is
/// pixels-per-point; background is (br,bg,bb) and text is (fr,fg,fb). macOS only.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn unterm_popup_sig_show(
    line: *const c_char,
    active_start: u32,
    active_len: u32,
    x: f32,
    y: f32,
    scale: f32,
    br: f32,
    bg: f32,
    bb: f32,
    fr: u8,
    fg: u8,
    fb: u8,
    dark: u8,
) {
    let line = cstr(line);
    let clear = wgpu::Color { r: br as f64, g: bg as f64, b: bb as f64, a: 1.0 };
    let text = glyphon::Color::rgb(fr, fg, fb);
    popup::show_sig(&line, active_start as usize, active_len as usize, x, y, scale, clear, text, dark != 0);
}

/// Hide the native signature-help hint. macOS only.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
pub extern "C" fn unterm_popup_sig_hide() {
    popup::hide_sig();
}

/// Show the agent notification card (top-right of the screen): `title` (the
/// session) over `body`. `scale` is pixels-per-point; `dark` picks the theme.
/// The host calls this when a turn finishes / a permission is raised while the
/// editor is backgrounded, and `unterm_notify_hide` once it's foregrounded again.
///
/// # Safety
/// `title` and `body` must be valid C strings or null.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
pub unsafe extern "C" fn unterm_notify_show(title: *const c_char, body: *const c_char, scale: f32, dark: u8) {
    let title = cstr(title);
    let body = cstr(body);
    popup::show_notify(&title, &body, scale, dark != 0);
}

/// Hide the agent notification card.
#[cfg(any(target_os = "macos", windows))]
#[no_mangle]
pub extern "C" fn unterm_notify_hide() {
    popup::hide_notify();
}

/// Accept a completion: delete `prefix_len` characters before the caret and insert
/// `text` in their place.
///
/// # Safety
/// `text` must be a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn unterm_editor_complete(id: u64, prefix_len: u32, text: *const c_char) {
    let text = cstr(text);
    with_editor(id, (), |e| e.complete(prefix_len as usize, &text));
}

// Keep `c_void` referenced so a header generator records the opaque handle type.
#[doc(hidden)]
pub type _UntermHandle = *mut c_void;

