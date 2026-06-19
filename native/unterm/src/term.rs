//! The terminal model: an alacritty VT grid fed by a PTY reader thread.
//!
//! One `Terminal` owns the shell (via [`crate::pty`]), the VT state machine's
//! grid (`alacritty_terminal::Term`), and the GPU [`Renderer`]. The grid lives
//! behind a `Mutex` shared with a background reader thread that pumps shell
//! output through the parser; the host thread locks the same grid to render and
//! to inject keystrokes. Everything here is durable: the registry in `lib.rs`
//! keeps `Terminal`s alive across Unity C# domain reloads.

use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

use crate::keys;
use crate::palette::Theme;
use crate::pty::{self, Pty};
use crate::renderer::Renderer;

/// Terminal grid size in cells. Scrollback is configured separately, so the
/// `Dimensions` total is just the visible screen.
#[derive(Clone, Copy)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// State the parser/host both touch without holding the (heavier) grid lock.
struct Shared {
    title: Mutex<String>,
    dirty: AtomicBool,
    child_exited: AtomicBool,
}

/// Sink for terminal events. Writes replies back to the PTY and tracks the
/// window title / child-exit state. Called from the reader thread while it
/// holds the grid lock, so it must never touch that lock.
#[derive(Clone)]
pub struct EventProxy {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    shared: Arc<Shared>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Ok(mut w) = self.writer.lock() {
                    let _ = w.write_all(text.as_bytes());
                    let _ = w.flush();
                }
            }
            Event::Title(t) => {
                if let Ok(mut g) = self.shared.title.lock() {
                    *g = t;
                }
            }
            Event::ResetTitle => {
                if let Ok(mut g) = self.shared.title.lock() {
                    g.clear();
                }
            }
            Event::ChildExit(_) | Event::Exit => {
                self.shared.child_exited.store(true, Ordering::Relaxed);
                self.shared.dirty.store(true, Ordering::Relaxed);
            }
            Event::Wakeup | Event::Bell => {
                self.shared.dirty.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct Terminal {
    term: Arc<Mutex<Term<EventProxy>>>,
    pty: Pty,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    shared: Arc<Shared>,
    renderer: Renderer,
    theme: Theme,
    cols: usize,
    rows: usize,
    scale: f32,
    focused: bool,
    /// Stable storage for the title returned across the C ABI.
    title_snap: CString,
    _reader: JoinHandle<()>,
}

impl Terminal {
    /// Create a terminal sized to `width`x`height` physical pixels at `scale`,
    /// rooted at `cwd` (empty = inherit), running `$SHELL`.
    pub fn new(width: u32, height: u32, scale: f32, cwd: &str) -> Self {
        let scale = scale.max(0.5);
        let mut renderer = Renderer::new(width.max(1), height.max(1));
        renderer.set_scale(scale);
        let (cols, rows) = renderer.cell_grid_size();

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let handles = pty::spawn(&shell, cwd, cols as u16, rows as u16)
            .expect("unterm: failed to spawn shell on PTY");

        let writer = Arc::new(Mutex::new(handles.writer));
        let shared = Arc::new(Shared {
            title: Mutex::new(String::new()),
            dirty: AtomicBool::new(true),
            child_exited: AtomicBool::new(false),
        });

        let proxy = EventProxy {
            writer: writer.clone(),
            shared: shared.clone(),
        };
        let config = Config::default();
        let term = Term::new(config, &TermSize { cols, rows }, proxy);
        let term = Arc::new(Mutex::new(term));

        let reader = spawn_reader(handles.reader, term.clone(), shared.clone());

        Terminal {
            term,
            pty: handles.pty,
            writer,
            shared,
            renderer,
            theme: Theme::default(),
            cols,
            rows,
            scale,
            focused: true,
            title_snap: CString::default(),
            _reader: reader,
        }
    }

    /// Write raw bytes to the shell (already encoded keystrokes/text).
    pub fn send(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        // Any keypress jumps back to the live prompt.
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(Scroll::Bottom);
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Resize to `width`x`height` physical px at `scale`, recomputing the cell
    /// grid and informing both the VT model and the PTY.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        let scale = scale.max(0.5);
        self.scale = scale;
        self.renderer.set_scale(scale);
        self.renderer.resize(width.max(1), height.max(1));
        let (cols, rows) = self.renderer.cell_grid_size();
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        if let Ok(mut t) = self.term.lock() {
            t.resize(TermSize { cols, rows });
        }
        self.pty.resize(cols as u16, rows as u16);
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Encode a named special key (+modifiers) and write it to the shell.
    pub fn send_key(&mut self, name: &str, ctrl: bool, alt: bool, shift: bool) {
        let app_cursor = self
            .term
            .lock()
            .map(|t| t.mode().contains(TermMode::APP_CURSOR))
            .unwrap_or(false);
        if let Some(bytes) = keys::encode(name, ctrl, alt, shift, app_cursor) {
            self.send(&bytes);
        }
    }

    /// Set the themeable default colors (the 16 ANSI colors keep their default).
    pub fn set_colors(&mut self, fg: [u8; 3], bg: [u8; 3], cursor: [u8; 3]) {
        self.theme.fg = fg;
        self.theme.bg = bg;
        self.theme.cursor = cursor;
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Refresh and return the title as a stable C string (valid until the next
    /// call on this terminal).
    pub fn title_cstr(&mut self) -> &CStr {
        let t = self.title();
        self.title_snap = CString::new(t.replace('\0', "")).unwrap_or_default();
        &self.title_snap
    }

    /// Last rendered frame as RGBA8 (readback fallback). Empty until rendered.
    pub fn read_pixels(&mut self) -> &[u8] {
        self.renderer.read_rgba()
    }

    /// Clear the scrollback and ask the shell to redraw a fresh prompt — the
    /// emulator-level "clear" (Terminal.app's Cmd-K): drop history, jump to the
    /// bottom, then send Ctrl-L so the shell repaints a clean prompt.
    pub fn clear(&mut self) {
        if let Ok(mut t) = self.term.lock() {
            t.grid_mut().clear_history();
            t.scroll_display(Scroll::Bottom);
        }
        self.send(&[0x0c]);
    }

    /// Paste text. Honors bracketed-paste mode (so editors/REPLs can tell a
    /// paste from typing and won't auto-run pasted newlines); otherwise newlines
    /// are normalized to CR so each line submits like Enter.
    pub fn paste(&mut self, text: &str) {
        let bracketed = self
            .term
            .lock()
            .map(|t| t.mode().contains(TermMode::BRACKETED_PASTE))
            .unwrap_or(false);
        if bracketed {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            self.send(&bytes);
        } else {
            let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
            self.send(normalized.as_bytes());
        }
    }

    /// Change the HiDPI scale, keeping the pixel size (re-derives the grid).
    pub fn set_scale(&mut self, scale: f32) {
        let (w, h) = self.renderer.size();
        self.resize(w, h, scale);
    }

    /// Scroll the viewport by `delta` lines (positive = into history).
    pub fn scroll(&mut self, delta: i32) {
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(Scroll::Delta(delta));
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_font(&mut self, path: &str) {
        self.renderer.set_font(path);
        // Cell metrics changed -> grid may now hold a different cols/rows.
        let (cols, rows) = self.renderer.cell_grid_size();
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            if let Ok(mut t) = self.term.lock() {
                t.resize(TermSize { cols, rows });
            }
            self.pty.resize(cols as u16, rows as u16);
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_font_size(&mut self, points: f32) {
        self.renderer.set_font_size(points);
        let (cols, rows) = self.renderer.cell_grid_size();
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            if let Ok(mut t) = self.term.lock() {
                t.resize(TermSize { cols, rows });
            }
            self.pty.resize(cols as u16, rows as u16);
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_focused(&mut self, focused: bool) {
        if focused != self.focused {
            self.focused = focused;
            self.shared.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Render the current grid into the IOSurface. Clears the dirty flag.
    pub fn render(&mut self) {
        self.shared.dirty.store(false, Ordering::Relaxed);
        if let Ok(term) = self.term.lock() {
            self.renderer.render(&term, &self.theme, self.focused);
        }
    }

    /// Whether new output (or a state change) has arrived since the last render.
    pub fn dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Relaxed)
    }

    pub fn title(&self) -> String {
        self.shared.title.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn is_alive(&mut self) -> bool {
        !self.shared.child_exited.load(Ordering::Relaxed) && self.pty.is_alive()
    }

    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }
}

/// Pump shell output through the parser into the shared grid until EOF.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term<EventProxy>>>,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut parser: Processor = Processor::new();
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut t) = term.lock() {
                        parser.advance(&mut *t, &buf[..n]);
                    }
                    shared.dirty.store(true, Ordering::Relaxed);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        shared.child_exited.store(true, Ordering::Relaxed);
        shared.dirty.store(true, Ordering::Relaxed);
    })
}
