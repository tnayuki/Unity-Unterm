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
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{viewport_to_point, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

use crate::keys;
use crate::palette::{self, Theme};
use crate::pty::{self, Pty};
use crate::renderer::Renderer;
use crate::shell;

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
    /// The shell's working directory, captured from OSC 7 / OSC 9;9 in its output
    /// (the shell reports it each prompt). Used to resume in the same dir.
    cwd: Mutex<String>,
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
    /// `None` for a display-only (already-exited) terminal restored from a saved
    /// buffer — it has a grid to show but no live shell.
    pty: Option<Pty>,
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
    /// Stable storage for the selected text returned across the C ABI.
    sel_snap: CString,
    /// Stable storage for the full-buffer dump returned across the C ABI.
    dump_snap: CString,
    /// Stable storage for the working directory returned across the C ABI.
    cwd_snap: CString,
    _reader: Option<JoinHandle<()>>,
}

impl Terminal {
    /// Create a terminal sized to `width`x`height` physical pixels at `scale`,
    /// rooted at `cwd` (empty = inherit). With an empty `command` it runs an
    /// interactive shell; otherwise it launches `command` directly in the PTY so
    /// the program replaces the shell as the PTY leader (no typed-ahead input).
    /// The shell and its launch arguments are picked per-OS by [`crate::shell`].
    pub fn new(width: u32, height: u32, scale: f32, cwd: &str, command: &str) -> Self {
        Self::build(width, height, scale, cwd, command, "", true)
    }

    /// Like [`new`](Self::new) (interactive shell), but first seeds the grid with
    /// `seed` (terminal text, e.g. SGR-styled restored scrollback) so it appears
    /// above the fresh prompt. Used to restore a session across an editor restart.
    pub fn new_seeded(width: u32, height: u32, scale: f32, cwd: &str, seed: &str) -> Self {
        Self::build(width, height, scale, cwd, "", seed, true)
    }

    /// A display-only terminal: seeds the grid with `seed` but spawns no shell and
    /// is marked exited. Restores the final screen of a terminal that had already
    /// exited before an editor restart.
    pub fn new_dead(width: u32, height: u32, scale: f32, seed: &str) -> Self {
        Self::build(width, height, scale, "", "", seed, false)
    }

    /// Shared constructor. `spawn` controls whether a live shell/PTY is started;
    /// `seed` (if non-empty) is fed through the parser before any shell output so
    /// it lands above the prompt.
    fn build(
        width: u32,
        height: u32,
        scale: f32,
        cwd: &str,
        command: &str,
        seed: &str,
        spawn: bool,
    ) -> Self {
        let scale = scale.max(0.5);
        let mut renderer = Renderer::new(width.max(1), height.max(1));
        renderer.set_scale(scale);
        let (cols, rows) = renderer.cell_grid_size();

        let shared = Arc::new(Shared {
            title: Mutex::new(String::new()),
            dirty: AtomicBool::new(true),
            child_exited: AtomicBool::new(!spawn),
            cwd: Mutex::new(String::new()),
        });

        // Either a live PTY+shell, or a no-op sink so writes from a display-only
        // (exited) terminal go nowhere.
        let (writer_box, pty, reader_in): (
            Box<dyn Write + Send>,
            Option<Pty>,
            Option<Box<dyn Read + Send>>,
        ) = if spawn {
            // Per-OS shell (macOS: $SHELL + `-lic exec`; Windows: PowerShell/cmd).
            let spec = shell::resolve(command);
            let handles = pty::spawn(&spec.program, &spec.args, cwd, cols as u16, rows as u16)
                .expect("unterm: failed to spawn shell on PTY");
            (handles.writer, Some(handles.pty), Some(handles.reader))
        } else {
            (Box::new(std::io::sink()), None, None)
        };

        let writer = Arc::new(Mutex::new(writer_box));
        let proxy = EventProxy {
            writer: writer.clone(),
            shared: shared.clone(),
        };
        let config = Config::default();
        let mut term = Term::new(config, &TermSize { cols, rows }, proxy);

        // Seed the grid before the reader thread starts, so restored scrollback
        // sits above whatever the shell prints next.
        if !seed.is_empty() {
            let mut parser: Processor = Processor::new();
            parser.advance(&mut term, seed.as_bytes());
        }

        let term = Arc::new(Mutex::new(term));
        let reader = reader_in.map(|r| spawn_reader(r, term.clone(), shared.clone()));

        Terminal {
            term,
            pty,
            writer,
            shared,
            renderer,
            theme: Theme::default(),
            cols,
            rows,
            scale,
            focused: true,
            title_snap: CString::default(),
            sel_snap: CString::default(),
            dump_snap: CString::default(),
            cwd_snap: CString::default(),
            _reader: reader,
        }
    }

    /// Write raw bytes to the shell (already encoded keystrokes/text).
    pub fn send(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        // Any keypress jumps back to the live prompt and drops the selection.
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(Scroll::Bottom);
            t.selection = None;
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
        if let Some(pty) = &self.pty {
            pty.resize(cols as u16, rows as u16);
        }
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
        self.theme.selection = crate::palette::selection_bg(bg);
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Refresh and return the title as a stable C string (valid until the next
    /// call on this terminal).
    pub fn title_cstr(&mut self) -> &CStr {
        let t = self.title();
        self.title_snap = CString::new(t.replace('\0', "")).unwrap_or_default();
        &self.title_snap
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

    /// Map a physical-pixel point to the grid `Point` it falls on, plus which
    /// half (left/right) of the cell, anchored to the current scrollback offset.
    fn px_to_point(&mut self, x: f32, y: f32) -> (GridPoint, Side) {
        let (line, col, left) = self.renderer.px_to_cell(x, y, self.cols, self.rows);
        let side = if left { Side::Left } else { Side::Right };
        let display_offset = self
            .term
            .lock()
            .map(|t| t.grid().display_offset())
            .unwrap_or(0);
        let point = viewport_to_point(display_offset, GridPoint::new(line, Column(col)));
        (point, side)
    }

    /// Begin a selection at a physical-pixel point. `mode`: 0 = by character,
    /// 1 = by word (semantic, for double-click), 2 = by line (triple-click).
    pub fn selection_start(&mut self, x: f32, y: f32, mode: u8) {
        let (point, side) = self.px_to_point(x, y);
        let ty = match mode {
            1 => SelectionType::Semantic,
            2 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        if let Ok(mut t) = self.term.lock() {
            t.selection = Some(Selection::new(ty, point, side));
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Extend the active selection to a physical-pixel point (mouse drag).
    pub fn selection_update(&mut self, x: f32, y: f32) {
        let (point, side) = self.px_to_point(x, y);
        if let Ok(mut t) = self.term.lock() {
            if let Some(sel) = t.selection.as_mut() {
                sel.update(point, side);
            }
        }
        self.shared.dirty.store(true, Ordering::Relaxed);
    }

    /// Drop any active selection (clears the highlight).
    pub fn selection_clear(&mut self) {
        if let Ok(mut t) = self.term.lock() {
            if t.selection.take().is_some() {
                self.shared.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    /// The selected text as a stable C string (empty if nothing is selected).
    /// Valid until the next call on this terminal.
    pub fn selection_text_cstr(&mut self) -> &CStr {
        let s = self
            .term
            .lock()
            .ok()
            .and_then(|t| t.selection_to_string())
            .unwrap_or_default();
        self.sel_snap = CString::new(s.replace('\0', "")).unwrap_or_default();
        &self.sel_snap
    }

    /// The whole grid (scrollback + screen) as text with truecolor SGR, capped to
    /// the most recent rows. Fed back through the parser by [`new_seeded`] /
    /// [`new_dead`] to restore a colored buffer across an editor restart.
    pub fn dump_styled(&mut self) -> String {
        /// Cap so a restored layout file stays bounded.
        const MAX_LINES: i32 = 5000;

        let t = match self.term.lock() {
            Ok(t) => t,
            Err(_) => return String::new(),
        };
        let grid = t.grid();
        let cols = grid.columns();
        let theme = &self.theme;
        let bottom = grid.bottommost_line().0;
        let start = grid.topmost_line().0.max(bottom - MAX_LINES + 1);

        let mut out = String::new();
        for line in start..=bottom {
            let row = &grid[Line(line)];
            // Trim trailing blank cells (space on default bg, no flags).
            let mut last = 0usize;
            for col in 0..cols {
                let cell = &row[Column(col)];
                let blank = cell.c == ' '
                    && matches!(cell.bg, Color::Named(NamedColor::Background))
                    && cell.flags.is_empty();
                if !blank {
                    last = col + 1;
                }
            }
            let (mut cf, mut cb) = (None, None);
            let (mut cbold, mut cital) = (false, false);
            for col in 0..last {
                let cell = &row[Column(col)];
                // Skip the placeholder cell after (or before, at a wrap) a wide
                // glyph: the wide char re-creates its own spacer when the dump is
                // re-parsed, so emitting this cell's space would add an extra column
                // — widening every CJK glyph's gap on restore.
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let fg = palette::resolve(cell.fg, theme);
                let bg = palette::resolve(cell.bg, theme);
                let bold = cell.flags.contains(Flags::BOLD);
                let ital = cell.flags.contains(Flags::ITALIC);
                if Some(fg) != cf || Some(bg) != cb || bold != cbold || ital != cital {
                    out.push_str("\x1b[0");
                    if bold {
                        out.push_str(";1");
                    }
                    if ital {
                        out.push_str(";3");
                    }
                    out.push_str(&format!(
                        ";38;2;{};{};{};48;2;{};{};{}m",
                        fg[0], fg[1], fg[2], bg[0], bg[1], bg[2]
                    ));
                    cf = Some(fg);
                    cb = Some(bg);
                    cbold = bold;
                    cital = ital;
                }
                out.push(cell.c);
            }
            out.push_str("\x1b[0m\r\n");
        }
        out
    }

    /// [`dump_styled`](Self::dump_styled) as a stable C string (valid until the
    /// next call on this terminal).
    pub fn dump_cstr(&mut self) -> &CStr {
        let s = self.dump_styled();
        self.dump_snap = CString::new(s.replace('\0', "")).unwrap_or_default();
        &self.dump_snap
    }

    /// The shell's current working directory, queried from the OS by the child
    /// pid (so it doesn't depend on the shell emitting OSC 7). Empty if there's no
    /// live shell or the cwd can't be read. Used to restore the cwd on resume.
    pub fn cwd(&mut self) -> String {
        self.shared.cwd.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// [`cwd`](Self::cwd) as a stable C string (valid until the next call).
    pub fn cwd_cstr(&mut self) -> &CStr {
        let s = self.cwd();
        self.cwd_snap = CString::new(s.replace('\0', "")).unwrap_or_default();
        &self.cwd_snap
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
            if let Some(pty) = &self.pty {
            pty.resize(cols as u16, rows as u16);
        }
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
            if let Some(pty) = &self.pty {
            pty.resize(cols as u16, rows as u16);
        }
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

    /// Advance the render-target swapchain (promotes a finished frame to the front
    /// on idle ticks). Returns true if the displayed buffer changed.
    pub fn advance(&mut self) -> bool {
        self.renderer.advance()
    }

    /// Whether new output (or a state change) has arrived since the last render.
    pub fn dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Relaxed)
    }

    pub fn title(&self) -> String {
        self.shared.title.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn is_alive(&mut self) -> bool {
        !self.shared.child_exited.load(Ordering::Relaxed)
            && self.pty.as_mut().map_or(false, |p| p.is_alive())
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

    /// Cursor rect (x, y, w, h) in physical px from the last render, if shown.
    pub fn cursor_px(&self) -> Option<[f32; 4]> {
        self.renderer.cursor_px()
    }

    /// Scrollback geometry for a scrollbar: total scrollback lines above the
    /// screen (`history`), how far up from the live bottom the viewport
    /// currently sits (`offset`, 0 = pinned to the bottom), and the visible
    /// row count (`screen`).
    pub fn scroll_state(&self) -> (usize, usize, usize) {
        let (history, offset) = self
            .term
            .lock()
            .map(|t| (t.grid().history_size(), t.grid().display_offset()))
            .unwrap_or((0, 0));
        (history, offset, self.rows)
    }
}

/// Pump shell output through the parser into the shared grid until EOF.
/// Extracts the shell's working directory from OSC escape sequences in its
/// output: OSC 7 (`ESC ] 7 ; file://host/path ST`, what the shells emit on
/// macOS/unix) and OSC 9;9 (`ESC ] 9 ; 9 ; path ST`, what we have Windows
/// PowerShell emit). Stateful so a sequence split across reads still parses.
#[derive(Default)]
struct CwdScanner {
    collecting: bool,
    payload: Vec<u8>,
    esc: bool,
}

impl CwdScanner {
    /// Feed a chunk; returns the latest cwd if an OSC 7/9;9 completed in it.
    fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut out = None;
        for &b in bytes {
            if self.collecting {
                if b == 0x07 {
                    if let Some(c) = parse_osc_cwd(&self.payload) {
                        out = Some(c);
                    }
                    self.collecting = false;
                    self.payload.clear();
                    self.esc = false;
                } else if self.esc {
                    if b == b'\\' {
                        if let Some(c) = parse_osc_cwd(&self.payload) {
                            out = Some(c);
                        }
                        self.collecting = false;
                        self.payload.clear();
                    } else {
                        self.payload.push(0x1b);
                        self.payload.push(b);
                    }
                    self.esc = false;
                } else if b == 0x1b {
                    self.esc = true;
                } else {
                    self.payload.push(b);
                    if self.payload.len() > 4096 {
                        self.collecting = false;
                        self.payload.clear();
                    }
                }
            } else if self.esc {
                if b == b']' {
                    self.collecting = true;
                    self.payload.clear();
                }
                self.esc = false;
            } else if b == 0x1b {
                self.esc = true;
            }
        }
        out
    }
}

/// Parse an OSC payload (without the `ESC ]` / terminator) into a cwd.
fn parse_osc_cwd(payload: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(payload).ok()?;
    if let Some(rest) = s.strip_prefix("7;") {
        // `file://<host>/<percent-encoded-path>`; host may be empty (file:///path).
        let rest = rest.strip_prefix("file://")?;
        let slash = rest.find('/')?;
        Some(percent_decode(&rest[slash..]))
    } else if let Some(rest) = s.strip_prefix("9;9;") {
        Some(rest.to_string())
    } else {
        None
    }
}

/// Minimal percent-decoder for OSC 7 file:// paths (`%20` → space, etc.).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term<EventProxy>>>,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut parser: Processor = Processor::new();
        let mut scanner = CwdScanner::default();
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Capture the shell's reported working directory (OSC 7 / 9;9).
                    if let Some(dir) = scanner.feed(&buf[..n]) {
                        if let Ok(mut c) = shared.cwd.lock() {
                            *c = dir;
                        }
                    }
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
