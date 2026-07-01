//! Rust-side multi-line input box backed by [`cosmic_text::Editor`], rendered
//! with the same wgpu/glyphon/IOSurface path as the panel. The host (C#)
//! forwards typed/IME text and named keys; whether Enter sends or inserts a
//! newline is decided host-side (it simply doesn't forward a plain Enter when it
//! means "send"). cosmic-text owns the editing logic (cursor, wrapping, motion,
//! insert/delete) so this only wires it to input events and draws it.

use glyphon::cosmic_text::{Change, Cursor, Motion, Scroll, Selection};
use glyphon::{
    Action, Attrs, Buffer, Color, Edit, Editor, Family, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};

use crate::gpu::{self, FORMAT};
use crate::surface::{self, SharedSurface};
use crate::quads::{Quad, QuadRenderer};
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

/// Cached line-number gutter glyph buffers, rebuilt only when the visible rows /
/// metrics / color change (keyed by a fingerprint) rather than every frame.
#[derive(Default)]
struct GutterCache {
    key: Option<u64>,
    bufs: Vec<Buffer>,
    pos: Vec<(f32, f32)>,
}

/// A git-diff "peek" tooltip: the original (base) lines of the hunk under the
/// pointer, floated as a card near the cursor so the removed / pre-edit content is
/// readable on hover (no click needed).
struct Peek {
    /// The hunk (by index into `diff_hunks`) currently shown, so hover only rebuilds
    /// when the pointer moves onto a different hunk.
    hunk_i: usize,
    /// Pointer position (surface px) where the tooltip opened; the card's top-left is
    /// placed near it and stays fixed there (no cursor-follow).
    at: (f32, f32),
    /// The hunk as a unified diff: `(added, text)` per row — removed base lines (red,
    /// `false`) first, then the added current lines (green, `true`). A pure deletion
    /// has only removed rows; a modification shows both, so they read apart.
    lines: Vec<(bool, String)>,
}

/// Inner padding (logical px, scaled) around the text.
const PAD: f32 = 6.0;

/// Default cap on retained undo steps (see `undo_limit`). The host can override it
/// per editor via `set_undo_limit`.
const DEFAULT_UNDO_LIMIT: usize = 500;

pub struct InputBox {
    width: u32,
    height: u32,
    shared: SharedSurface,

    clear: wgpu::Color,
    text_color: Color,
    font_family: Option<String>,
    scale: f32,

    editor: Editor<'static>,
    /// Undo/redo stacks: each edit pushes to `undo` and clears `redo`; undo moves
    /// the change to `redo` (and back) so it can be replayed.
    undo: Vec<Change>,
    redo: Vec<Change>,
    /// Version marks parallel to the undo/redo stacks: a monotonic id identifying
    /// the document *state*, which undo/redo restore (like Monaco's alternative
    /// version id). The host records `cur_mark` at save and compares it to detect
    /// unsaved changes without holding a second copy of the buffer.
    undo_marks: Vec<u64>,
    redo_marks: Vec<u64>,
    cur_mark: u64,
    next_mark: u64,
    /// Cap on retained undo steps; the oldest are dropped past it (a line-op change
    /// can hold a whole replaced block, so an unbounded stack grows without limit
    /// over a long session). 0 = unlimited. Host-configurable via `set_undo_limit`.
    undo_limit: usize,
    /// Laid-out text height in physical px (host grows the strip to fit).
    content_h: f32,
    /// Caret rect in physical px (x, y, w, h) for host-side IME positioning.
    caret: [f32; 4],
    /// Vertical scroll offset (px from content top) kept between renders, so the
    /// view only scrolls when the caret would leave the box — not on every caret
    /// move.
    scroll_v: f32,
    /// Live IME preedit (marked text) inserted inline at the caret while
    /// composing — empty when not composing. Tracked so it can be replaced each
    /// composition update and removed on commit/cancel, without touching undo.
    preedit: String,
    preedit_anchor: Option<Cursor>,
    /// Send/Stop action button drawn in the strip: 0=none, 1=send (▶), 2=stop (■).
    button: u8,
    /// The button's hit rect (physical px x,y,w,h), computed each render.
    button_rect: [f32; 4],

    /// Code-editor mode: tree-sitter language id (e.g. "cs") to highlight with,
    /// or None for plain uniform color (the agent composer). When set, per-line
    /// colored spans replace the uniform attrs.
    highlight_lang: Option<String>,
    /// Whether to use the dark highlight theme.
    highlight_dark: bool,
    /// Draw a line-number gutter on the left (code-editor mode).
    gutter: bool,
    /// Gutter width in physical px from the last render (0 when off), so `mouse`
    /// hit-testing can offset by it.
    gutter_px: f32,
    /// Cached gutter number buffers (see [`GutterCache`]).
    gutter_cache: GutterCache,
    /// Whether the host window owns keyboard focus. The caret is only drawn while
    /// focused, so an unfocused input box (background editor window) shows no caret.
    focused: bool,
    /// Debugger breakpoint lines (0-based), shown as gutter dots in code mode.
    breakpoints: std::collections::HashSet<usize>,
    /// Whether the gutter reserves a breakpoint-dot column left of the line numbers
    /// (only when debugging is enabled — otherwise the gutter stays at its plain
    /// line-number width and no dots are drawn).
    bp_gutter: bool,
    /// Read-only mode: caret movement, selection, copy, scroll and find still work,
    /// but text mutations are ignored (used by the debugger's shared source view).
    read_only: bool,
    /// The debugger's current execution line (0-based), highlighted amber. Distinct
    /// from the caret's current-line highlight in `code_mode`.
    exec_line: Option<usize>,
    /// Code-editor behaviors: no word-wrap (+ horizontal scroll), auto-indent,
    /// auto-close brackets, current-line + matching-bracket highlight, smart Home.
    code_mode: bool,
    /// Horizontal scroll offset (px) for the no-wrap code view.
    scroll_h: f32,
    /// Set when the caret moved (key/mouse/edit), so render scrolls to keep it in
    /// view ONCE; cleared after. Wheel/scrollbar scrolling doesn't set it, so it
    /// isn't snapped back to the caret.
    caret_dirty: bool,
    /// Bumped on every text mutation; the highlight cache recomputes when it
    /// differs from `hl_gen` (so tree-sitter runs once per edit, not per frame).
    edit_gen: u64,
    hl_gen: u64,
    hl_cache: Vec<crate::highlight::LineSpans>,
    /// Incremental tree-sitter highlighter (None when the language has no grammar
    /// or code mode is off). Reparses only the changed region per edit.
    highlighter: Option<crate::highlight::Highlighter>,
    /// Set when the per-line `AttrsList`s need rebuilding (highlight cache, theme,
    /// text color, or font changed). Cleared after the render re-applies them, so
    /// steady-state frames skip the whole-document attrs loop.
    attrs_dirty: bool,

    /// Git HEAD and index versions of the file, fetched by the host via `set_diff`.
    /// The gutter diffs the buffer against HEAD (VS Code/Zed-style, so staged changes
    /// stay marked until committed), falling back to the index before the first
    /// commit; both together drive the staged detection (diff-of-diffs). None/None =
    /// untracked / diff off. `diff_markers` is recomputed against the live buffer
    /// once per edit (gated by `diff_gen`, mirroring `hl_gen`).
    diff_head: Option<String>,
    diff_index: Option<String>,
    /// diff(HEAD → index): the staged blocks, recomputed per fetch (not per edit).
    diff_index_hunks: Vec<crate::diff::Hunk>,
    diff_gen: u64,
    /// Per-logical-line marker bits (see [`crate::diff`]); empty when diff is off.
    diff_markers: Vec<u8>,
    /// The changed blocks for the current buffer (parallel to `diff_markers`), so a
    /// gutter-marker click can recover a hunk's original lines to peek.
    diff_hunks: Vec<crate::diff::Hunk>,
    /// Which of `diff_hunks` are already staged (parallel), for hollow markers and
    /// the host's Stage/Unstage menu choice.
    diff_staged: Vec<bool>,
    /// For staged-ONLY hunks (the change lives in the index but the buffer is back
    /// at HEAD): the index's new-range, so the peek can show the staged content
    /// (parallel to `diff_hunks`; None for ordinary buffer hunks).
    diff_index_new: Vec<Option<(usize, usize)>>,
    /// Open inline peek of a hunk's original lines (click a gutter marker to toggle).
    peek: Option<Peek>,

    /// Autocomplete popup items + selected index (code mode). The host computes
    /// the list; this draws it (on top of the text) anchored at the caret.
    completions: Vec<String>,
    compl_sel: usize,

    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    quads: QuadRenderer,
    /// Separate renderers for the popup so it draws ON TOP of the main text (own
    /// GPU buffers — re-preparing the main ones for a second draw would race).
    popup_text: TextRenderer,
    popup_quads: QuadRenderer,
    /// Own renderers for the inline diff peek, drawn on top like the popup (and
    /// independent of it, since a peek can be open with no completion list).
    peek_text: TextRenderer,
    peek_quads: QuadRenderer,
}

impl InputBox {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let g = gpu::gpu();
        let shared = create_target(&g.device, width, height);

        let swash_cache = SwashCache::new();
        let viewport = Viewport::new(&g.device, &g.cache);
        let mut atlas = TextAtlas::new(&g.device, &g.queue, &g.cache, FORMAT);
        let text_renderer =
            TextRenderer::new(&mut atlas, &g.device, wgpu::MultisampleState::default(), None);
        let quads = QuadRenderer::new(&g.device, FORMAT);
        let popup_text =
            TextRenderer::new(&mut atlas, &g.device, wgpu::MultisampleState::default(), None);
        let popup_quads = QuadRenderer::new(&g.device, FORMAT);
        let peek_text =
            TextRenderer::new(&mut atlas, &g.device, wgpu::MultisampleState::default(), None);
        let peek_quads = QuadRenderer::new(&g.device, FORMAT);

        let editor = {
            let mut fs = gpu::lock_font_system();
            let buffer = Buffer::new(&mut fs, Metrics::new(14.0, 20.0));
            Editor::new(buffer)
        };

        Self {
            width,
            height,
            shared,
            clear: wgpu::Color {
                r: 0.08,
                g: 0.08,
                b: 0.08,
                a: 1.0,
            },
            text_color: Color::rgb(210, 210, 214),
            font_family: None,
            scale: 1.0,
            editor,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_marks: Vec::new(),
            redo_marks: Vec::new(),
            cur_mark: 0,
            next_mark: 1,
            undo_limit: DEFAULT_UNDO_LIMIT,
            content_h: 0.0,
            caret: [0.0; 4],
            scroll_v: 0.0,
            preedit: String::new(),
            preedit_anchor: None,
            button: 0,
            button_rect: [0.0; 4],
            highlight_lang: None,
            highlight_dark: true,
            gutter: false,
            gutter_px: 0.0,
            gutter_cache: GutterCache::default(),
            focused: true,
            breakpoints: std::collections::HashSet::new(),
            bp_gutter: false,
            read_only: false,
            exec_line: None,
            code_mode: false,
            scroll_h: 0.0,
            caret_dirty: true,
            edit_gen: 0,
            hl_gen: u64::MAX,
            hl_cache: Vec::new(),
            highlighter: None,
            attrs_dirty: true,
            diff_head: None,
            diff_index: None,
            diff_index_hunks: Vec::new(),
            diff_gen: u64::MAX,
            diff_markers: Vec::new(),
            diff_hunks: Vec::new(),
            diff_staged: Vec::new(),
            diff_index_new: Vec::new(),
            peek: None,
            completions: Vec::new(),
            compl_sel: 0,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            quads,
            popup_text,
            popup_quads,
            peek_text,
            peek_quads,
        }
    }

    /// Set the autocomplete popup items + selected index (empty list hides it).
    pub fn set_completions(&mut self, items: Vec<String>, selected: usize) {
        self.completions = items;
        self.compl_sel = if self.completions.is_empty() {
            0
        } else {
            selected.min(self.completions.len() - 1)
        };
    }

    pub fn set_clear_color(&mut self, r: f64, g: f64, b: f64, a: f64) {
        self.clear = wgpu::Color { r, g, b, a };
    }

    pub fn set_text_color(&mut self, r: u8, g: u8, b: u8, a: u8) {
        self.text_color = Color::rgba(r, g, b, a);
        self.attrs_dirty = true; // base color feeds every line's attrs
    }

    pub fn set_font(&mut self, path: &str) {
        self.attrs_dirty = true; // family feeds every line's attrs
        // A family name (not a file path) is already in the shared FontSystem (a
        // system UI font): address it directly, no file load.
        if !gpu::is_font_path(path) {
            self.font_family = Some(path.to_string());
            return;
        }
        let mut fs = gpu::lock_font_system();
        let db = fs.db_mut();
        if let Err(e) = db.load_font_file(path) {
            log::warn!("unterm: failed to load input font {path}: {e}");
            return;
        }
        if let Some(name) = db
            .faces()
            .last()
            .and_then(|f| f.families.first())
            .map(|(name, _)| name.clone())
        {
            self.font_family = Some(name);
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(0.5);
    }

    /// Enable code-editor highlighting with a tree-sitter language id (e.g. "cs"),
    /// or None for plain uniform color. Unknown ids fall back to uniform.
    pub fn set_language(&mut self, lang: Option<&str>) {
        let new = lang.map(|s| s.to_string());
        if new != self.highlight_lang {
            // Rebuild the incremental highlighter for the new grammar (None if we
            // don't have one, which falls back to uniform color).
            self.highlighter = new.as_deref().and_then(crate::highlight::Highlighter::new);
            self.highlight_lang = new;
            self.hl_gen = u64::MAX; // force re-highlight
        }
    }

    /// Choose the dark or light highlight theme.
    pub fn set_dark(&mut self, dark: bool) {
        if dark != self.highlight_dark {
            self.highlight_dark = dark;
            self.hl_gen = u64::MAX;
        }
    }

    /// Draw a line-number gutter (code-editor mode).
    pub fn set_gutter(&mut self, on: bool) {
        self.gutter = on;
    }

    /// Set the git HEAD and index versions of the file (both None clears the
    /// markers). Returns true when they actually changed — the host refreshes on a
    /// 1s poll, so an unchanged delivery is a no-op that must NOT drop the peek or
    /// force a recompute. The markers recompute on the next render when changed.
    pub fn set_diff(&mut self, head: Option<String>, index: Option<String>) -> bool {
        if self.diff_head == head && self.diff_index == index {
            return false;
        }
        // diff(HEAD → index) — the staged blocks — changes only per fetch, so
        // compute it here rather than per edit.
        self.diff_index_hunks = match (head.as_deref(), index.as_deref()) {
            (Some(h), Some(i)) => crate::diff::hunks(h, i),
            _ => Vec::new(),
        };
        self.diff_head = head;
        self.diff_index = index;
        self.diff_gen = u64::MAX; // force a recompute next render
        self.peek = None; // hunks may have shifted; drop any open peek
        true
    }

    /// The text the gutter diffs the buffer against: HEAD, or the index before the
    /// first commit (so a freshly-added file still gets markers).
    fn diff_base_text(&self) -> Option<&str> {
        self.diff_head.as_deref().or(self.diff_index.as_deref())
    }

    /// Whether hunk `hunk_i`'s change is already staged in the index (exact match).
    pub fn hunk_staged(&self, hunk_i: usize) -> bool {
        self.diff_staged.get(hunk_i).copied().unwrap_or(false)
    }

    /// Whether hunk `hunk_i` is staged-ONLY: the change lives in the index while the
    /// buffer is back at HEAD. Reverting the buffer is a no-op there, so the host's
    /// menu offers just Unstage.
    pub fn hunk_staged_only(&self, hunk_i: usize) -> bool {
        self.diff_index_new.get(hunk_i).copied().flatten().is_some()
    }

    /// Whether ANY staged block overlaps hunk `hunk_i`'s HEAD range. True also for a
    /// partially staged region (staged, then edited further): `hunk_staged` is false
    /// there, but an Unstage would still drop the staged version, so the host offers
    /// both Stage (update) and Unstage (drop).
    pub fn hunk_has_staged(&self, hunk_i: usize) -> bool {
        let Some(h) = self.diff_hunks.get(hunk_i) else { return false };
        crate::diff::overlaps_staged(&self.diff_index_hunks, (h.old_start, h.old_start + h.old_len))
    }

    /// Logical line at surface `y` (px), from the last render's layout. None past the
    /// end of the visible text.
    fn line_at_y(&self, y: f32) -> Option<usize> {
        let target = y - PAD * self.scale;
        self.editor.with_buffer(|b| {
            for run in b.layout_runs() {
                if target >= run.line_top && target < run.line_top + run.line_height {
                    return Some(run.line_i);
                }
            }
            None
        })
    }

    /// Index of the hunk a hover on `line` should peek — any changed span or a
    /// pure-deletion boundary (VS Code's quick-diff peek opens on every marker
    /// kind; a pure addition shows just its `+` lines). `n` is the buffer line
    /// count. The hunk's rows are padded by one line on each side so a hover near
    /// a thin marker (especially a deletion wedge) still lands.
    fn peekable_hunk_for_line(&self, line: usize, n: usize) -> Option<usize> {
        self.diff_hunks.iter().position(|h| {
            let (lo, hi) = if h.new_len > 0 {
                (h.new_start, h.new_start + h.new_len) // [lo, hi)
            } else {
                let b = if h.new_start < n { h.new_start } else { n.saturating_sub(1) };
                (b, b + 1)
            };
            // ±1 line of slack so a click near the marker still lands.
            line >= lo.saturating_sub(1) && line < hi + 1
        })
    }

    /// Index of ANY hunk at `line` (added / modified / deleted), for stage/revert —
    /// unlike `peekable_hunk_for_line` this includes pure additions (which have no
    /// removed content to peek but are still actionable). ±1 line of slack.
    fn hunk_index_for_line(&self, line: usize, n: usize) -> Option<usize> {
        self.diff_hunks.iter().position(|h| {
            let (lo, hi) = if h.new_len > 0 {
                (h.new_start, h.new_start + h.new_len)
            } else {
                let b = if h.new_start < n { h.new_start } else { n.saturating_sub(1) };
                (b, b + 1)
            };
            line >= lo.saturating_sub(1) && line < hi + 1
        })
    }

    /// The hunk a click at surface (`x`, `y`) targets, or None when the click isn't in
    /// the gutter lane or no hunk is there. Drives the gutter-click Stage/Revert menu.
    pub fn hunk_at(&self, x: f32, y: f32) -> Option<usize> {
        let pad = PAD * self.scale;
        if !self.gutter || x >= pad + self.gutter_px + 4.0 * self.scale {
            return None;
        }
        let line = self.line_at_y(y)?;
        let n = self.editor.with_buffer(|b| b.lines.len());
        self.hunk_index_for_line(line, n)
    }

    /// Revert hunk `hunk_i` to its git-base (HEAD) content, as one undoable edit
    /// (marks the buffer dirty; the base is unchanged so the marker clears on the
    /// next render).
    pub fn revert_hunk(&mut self, hunk_i: usize) {
        let Some(h) = self.diff_hunks.get(hunk_i).copied() else { return };
        let Some(base) = self.diff_base_text() else { return };
        let base_old: Vec<String> =
            base.split('\n').skip(h.old_start).take(h.old_len).map(str::to_string).collect();
        let n = self.line_count();
        if h.new_len > 0 {
            // Modified/added span: replace the current lines with the base lines
            // (empty base_old for a pure addition → deletes the added lines).
            let l0 = h.new_start.min(n.saturating_sub(1));
            let l1 = (h.new_start + h.new_len - 1).min(n.saturating_sub(1));
            self.splice_lines(l0, l1, &base_old, (l0, 0), None);
        } else if !base_old.is_empty() {
            // Pure deletion: re-insert the removed base lines at the boundary.
            if h.new_start < n {
                let mut repl = base_old;
                repl.push(self.line_text(h.new_start)); // keep the line it sat above
                self.splice_lines(h.new_start, h.new_start, &repl, (h.new_start, 0), None);
            } else {
                let last = n.saturating_sub(1);
                let mut repl = vec![self.line_text(last)];
                repl.extend(base_old);
                self.splice_lines(last, last, &repl, (last, 0), None);
            }
        }
    }

    /// The new index content (LF) that stages hunk `hunk_i`: the current index with
    /// just this hunk's buffer change applied, so hunks staged earlier are kept.
    /// None when the file isn't in the index or there's nothing to stage there.
    pub fn stage_hunk_content(&self, hunk_i: usize) -> Option<String> {
        let h = self.diff_hunks.get(hunk_i).copied()?;
        let index = self.diff_index.as_deref()?;
        let cur = self.text();
        crate::diff::stage_apply(index, &cur, (h.new_start, h.new_start + h.new_len))
    }

    /// The new index content (LF) that UNstages hunk `hunk_i`: the current index
    /// with this hunk's staged block reverted to its HEAD lines. None when there's
    /// no commit yet / nothing staged there.
    pub fn unstage_hunk_content(&self, hunk_i: usize) -> Option<String> {
        let h = self.diff_hunks.get(hunk_i).copied()?;
        let head = self.diff_head.as_deref()?;
        let index = self.diff_index.as_deref()?;
        crate::diff::unstage_apply(head, index, (h.old_start, h.old_start + h.old_len))
    }

    /// Pointer moved to surface (`x`, `y`): show the diff-peek tooltip when hovering a
    /// gutter marker whose hunk has removed content, else hide it. Returns true when
    /// the tooltip is showing or its position/target changed, so the host repaints.
    pub fn hover(&mut self, x: f32, y: f32) -> bool {
        let pad = PAD * self.scale;
        let over_gutter = self.gutter && x < pad + self.gutter_px + 4.0 * self.scale;
        let target = if over_gutter {
            self.line_at_y(y).and_then(|line| {
                let n = self.editor.with_buffer(|b| b.lines.len());
                self.peekable_hunk_for_line(line, n)
            })
        } else {
            None
        };
        match target {
            Some(hi) => {
                // Already showing this hunk: keep the card fixed where it first opened
                // (no cursor-follow) and skip the repaint.
                if self.peek.as_ref().map(|p| p.hunk_i) == Some(hi) {
                    return false;
                }
                let h = self.diff_hunks[hi];
                // Unified diff: removed base lines (red), then added current lines
                // (green). For a staged-ONLY hunk the change lives in the index, so
                // the added side comes from the index's lines instead of the buffer.
                let mut lines: Vec<(bool, String)> = match self.diff_base_text() {
                    Some(base) => base
                        .split('\n')
                        .skip(h.old_start)
                        .take(h.old_len)
                        .map(|s| (false, s.to_string()))
                        .collect(),
                    None => Vec::new(),
                };
                if let Some((is, il)) = self.diff_index_new.get(hi).copied().flatten() {
                    if let Some(index) = self.diff_index.as_deref() {
                        lines.extend(index.split('\n').skip(is).take(il).map(|s| (true, s.to_string())));
                    }
                } else if h.new_len > 0 {
                    self.editor.with_buffer(|b| {
                        for l in b.lines.iter().skip(h.new_start).take(h.new_len) {
                            lines.push((true, l.text().to_string()));
                        }
                    });
                }
                if lines.is_empty() {
                    return self.peek.take().is_some();
                }
                self.peek = Some(Peek { hunk_i: hi, at: (x, y), lines });
                true
            }
            // Left every marker: hide the tooltip (repaint once to clear it).
            None => self.peek.take().is_some(),
        }
    }

    /// Gutter width (physical px) from the last render, for hit-testing clicks.
    pub fn gutter_width(&self) -> f32 {
        self.gutter_px
    }

    /// The 0-based logical line nearest physical y (clamped to the last line; for
    /// gutter-click breakpoint toggles, which snap to a line even past text end).
    pub fn line_at_y_clamped(&self, y: f32) -> usize {
        let pad = PAD * self.scale;
        let by = (y - pad).max(0.0);
        let mut line = 0usize;
        self.editor.with_buffer(|buf| {
            for run in buf.layout_runs() {
                if run.line_top <= by {
                    line = run.line_i;
                }
            }
        });
        line
    }

    /// Toggle a breakpoint on a 0-based line.
    pub fn toggle_breakpoint(&mut self, line: usize) {
        if !self.breakpoints.remove(&line) {
            self.breakpoints.insert(line);
        }
    }

    /// Replace the breakpoint set (0-based lines).
    pub fn set_breakpoints(&mut self, lines: &[u32]) {
        self.breakpoints = lines.iter().map(|&l| l as usize).collect();
    }

    /// Current breakpoint lines (0-based), ascending.
    #[allow(dead_code)] // host (C#) is the source of truth; kept for tests/future query
    pub fn breakpoints(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.breakpoints.iter().map(|&l| l as u32).collect();
        v.sort_unstable();
        v
    }

    /// Read-only mode: caret/selection/scroll/find still work; text edits are ignored.
    pub fn set_read_only(&mut self, on: bool) {
        self.read_only = on;
    }

    /// Reserve (and draw) the breakpoint-dot column in the gutter. Off = plain
    /// line-number gutter at its usual width, with no dots.
    pub fn set_bp_gutter(&mut self, on: bool) {
        self.bp_gutter = on;
    }

    /// Highlight a 0-based execution line (the debugger's current line), or clear it.
    pub fn set_exec_line(&mut self, line: Option<usize>) {
        self.exec_line = line;
    }

    /// Scroll vertically by `dy` physical px (wheel). Clamped on the next render.
    pub fn scroll_by(&mut self, dy: f32) {
        self.scroll_v = (self.scroll_v + dy).max(0.0);
        self.peek = None; // the hover position is stale once content moves
    }

    /// Scroll horizontally by `dx` physical px (wheel/trackpad). Clamped on the
    /// next render to the longest line.
    pub fn scroll_h_by(&mut self, dx: f32) {
        self.scroll_h = (self.scroll_h + dx).max(0.0);
        self.peek = None;
    }

    /// Mark the text as changed so the highlight cache recomputes next render.
    fn bump(&mut self) {
        self.edit_gen = self.edit_gen.wrapping_add(1);
        self.caret_dirty = true;
        self.peek = None; // an edit shifts hunks; drop the peek
    }

    /// Record a new undoable edit: push the change and advance the version mark to a
    /// fresh value (the prior state's mark is remembered so undo can restore it).
    /// Clears the redo branch — a new edit makes redo unreachable.
    fn push_change(&mut self, c: Change) {
        self.undo.push(c);
        self.undo_marks.push(self.cur_mark);
        self.cur_mark = self.next_mark;
        self.next_mark = self.next_mark.wrapping_add(1);
        self.redo.clear();
        self.redo_marks.clear();
        // Bound the history: drop the oldest steps once it exceeds the cap (parallel
        // mark stack stays in sync). New edits only ever push one past the cap.
        // A zero limit means unlimited.
        if self.undo_limit != 0 && self.undo.len() > self.undo_limit {
            let excess = self.undo.len() - self.undo_limit;
            self.undo.drain(0..excess);
            self.undo_marks.drain(0..excess);
        }
    }

    /// Set the undo-history cap (0 = unlimited). Trims the existing stack at once if
    /// the new limit is smaller than what's already retained.
    pub fn set_undo_limit(&mut self, limit: usize) {
        self.undo_limit = limit;
        if limit != 0 && self.undo.len() > limit {
            let excess = self.undo.len() - limit;
            self.undo.drain(0..excess);
            self.undo_marks.drain(0..excess);
        }
    }

    /// The current document-state version (see `undo_marks`). The host compares this
    /// to the value captured at save to detect unsaved changes — undoing back to the
    /// saved state restores its mark, so the buffer reads clean again.
    pub fn edit_serial(&self) -> u64 {
        self.cur_mark
    }

    /// Enable code-editor behaviors (no word-wrap + horizontal scroll, auto-indent,
    /// auto-close brackets, current-line + matching-bracket highlight, smart Home).
    pub fn set_code_mode(&mut self, on: bool) {
        self.code_mode = on;
    }

    /// Host keyboard focus. The caret is hidden while unfocused, so a background
    /// editor window doesn't show a stray blinking-position bar.
    pub fn set_focused(&mut self, on: bool) {
        self.focused = on;
    }

    /// Set the absolute vertical scroll (px); the next render clamps it.
    pub fn set_scroll_v(&mut self, px: f32) {
        self.scroll_v = px.max(0.0);
    }

    /// The current vertical scroll offset (px) and total content height (px).
    pub fn scroll_offset(&self) -> f32 {
        self.scroll_v
    }

    // --- code-editor operations (gated by the host to editor windows) ----------

    /// Current caret as (line, character-column).
    fn cur_pos(&self) -> (usize, usize) {
        let c = self.editor.cursor();
        let col = self
            .editor
            .with_buffer(|b| b.lines.get(c.line).map(|l| byte_to_col(l.text(), c.index)).unwrap_or(0));
        (c.line, col)
    }

    /// The line range touched by the selection (or the caret line if none).
    fn sel_line_range(&self) -> (usize, usize) {
        match self.editor.selection_bounds() {
            Some((a, b)) => (a.line, b.line),
            None => {
                let l = self.editor.cursor().line;
                (l, l)
            }
        }
    }

    /// Logical line count.
    fn line_count(&self) -> usize {
        self.editor.with_buffer(|b| b.lines.len())
    }

    /// Byte length of logical line `l` (0 if out of range).
    fn line_byte_len(&self, l: usize) -> usize {
        self.editor
            .with_buffer(|b| b.lines.get(l).map(|x| x.text().len()).unwrap_or(0))
    }

    /// Owned text of logical line `l` (empty if out of range).
    fn line_text(&self, l: usize) -> String {
        self.editor
            .with_buffer(|b| b.lines.get(l).map(|x| x.text().to_string()).unwrap_or_default())
    }

    /// Owned text of logical lines `l0..=l1` (clamped to the buffer).
    fn lines_in_range(&self, l0: usize, l1: usize) -> Vec<String> {
        self.editor.with_buffer(|b| {
            let l1 = l1.min(b.lines.len().saturating_sub(1));
            (l0..=l1).map(|i| b.lines[i].text().to_string()).collect()
        })
    }

    /// Replace the whole buffer with `lines` as ONE undoable change, then place the
    /// caret at `caret` (line, char-col) with an optional selection anchor.
    fn apply_lines(&mut self, lines: Vec<String>, caret: (usize, usize), sel: Option<(usize, usize)>) {
        let text = lines.join("\n");
        {
            let mut fs = gpu::lock_font_system();
            self.editor.start_change();
            self.editor.set_selection(Selection::None);
            self.editor.action(&mut fs, Action::Motion(Motion::BufferStart));
            let start = self.editor.cursor();
            self.editor.set_selection(Selection::Normal(start));
            self.editor.action(&mut fs, Action::Motion(Motion::BufferEnd));
            self.editor.delete_selection();
            self.editor.insert_string(&text, None);
            if let Some(c) = self.editor.finish_change() {
                self.push_change(c);
            }
        }
        let last = lines.len().saturating_sub(1);
        let cl = caret.0.min(last);
        let cb = col_to_byte(&lines[cl], caret.1);
        match sel {
            Some((al, ac)) => {
                let al = al.min(last);
                let ab = col_to_byte(&lines[al], ac);
                self.editor.set_selection(Selection::Normal(Cursor::new(al, ab)));
                self.editor.set_cursor(Cursor::new(cl, cb));
            }
            None => {
                self.editor.set_selection(Selection::None);
                self.editor.set_cursor(Cursor::new(cl, cb));
            }
        }
        self.bump();
    }

    /// Replace logical lines `l0..=l1` with `new_lines` as ONE undoable change,
    /// then place the caret at `caret` (line, char-col) with an optional selection
    /// anchor. Only the affected byte range is touched (and the recorded change
    /// holds only that range), so a line op on a large file stays O(block), not
    /// O(document) — unlike `apply_lines`, which rewrites the whole buffer.
    fn splice_lines(
        &mut self,
        l0: usize,
        l1: usize,
        new_lines: &[String],
        caret: (usize, usize),
        sel: Option<(usize, usize)>,
    ) {
        let n = self.line_count();
        let l1 = l1.min(n.saturating_sub(1));
        let joined = new_lines.join("\n");
        // Pick the span + replacement so the resulting line count follows
        // `new_lines` (covers same-count ops, growth, and deletion).
        let (start, end, repl) = if l1 + 1 < n {
            // A line follows the block: include the '\n' after l1 so an empty
            // replacement removes the block cleanly (and a normal one restores it).
            let repl = if new_lines.is_empty() { String::new() } else { format!("{joined}\n") };
            (Cursor::new(l0, 0), Cursor::new(l1 + 1, 0), repl)
        } else if l0 > 0 {
            // Block runs to EOF but isn't the whole buffer: include the '\n' before
            // l0 so deletion doesn't leave a trailing empty line.
            let repl = if new_lines.is_empty() { String::new() } else { format!("\n{joined}") };
            (
                Cursor::new(l0 - 1, self.line_byte_len(l0 - 1)),
                Cursor::new(l1, self.line_byte_len(l1)),
                repl,
            )
        } else {
            // Whole buffer.
            (Cursor::new(0, 0), Cursor::new(l1, self.line_byte_len(l1)), joined)
        };
        self.editor.start_change();
        self.editor.set_selection(Selection::Normal(start));
        self.editor.set_cursor(end);
        self.editor.delete_selection();
        self.editor.insert_string(&repl, None);
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        let last = self.line_count().saturating_sub(1);
        let cl = caret.0.min(last);
        let cb = col_to_byte(&self.line_text(cl), caret.1);
        match sel {
            Some((al, ac)) => {
                let al = al.min(last);
                let ab = col_to_byte(&self.line_text(al), ac);
                self.editor.set_selection(Selection::Normal(Cursor::new(al, ab)));
                self.editor.set_cursor(Cursor::new(cl, cb));
            }
            None => {
                self.editor.set_selection(Selection::None);
                self.editor.set_cursor(Cursor::new(cl, cb));
            }
        }
        self.bump();
    }

    pub fn indent(&mut self) {
        let (l0, l1) = self.sel_line_range();
        let had_sel = self.editor.selection_bounds().is_some();
        let cur = self.cur_pos();
        let mut block = self.lines_in_range(l0, l1);
        let bi = cur.0 - l0; // no selection ⇒ single line ⇒ 0
        let old = block[bi].chars().count();
        let last = block.len().saturating_sub(1);
        crate::editops::indent(&mut block, 0, last);
        if had_sel {
            let end = block[(l1 - l0).min(block.len() - 1)].chars().count();
            self.splice_lines(l0, l1, &block, (l1, end), Some((l0, 0)));
        } else {
            let delta = block[bi].chars().count() as i64 - old as i64;
            self.splice_lines(l0, l1, &block, (cur.0, (cur.1 as i64 + delta).max(0) as usize), None);
        }
    }

    pub fn outdent(&mut self) {
        let (l0, l1) = self.sel_line_range();
        let had_sel = self.editor.selection_bounds().is_some();
        let cur = self.cur_pos();
        let mut block = self.lines_in_range(l0, l1);
        let bi = cur.0 - l0;
        let old = block[bi].chars().count();
        let last = block.len().saturating_sub(1);
        crate::editops::outdent(&mut block, 0, last);
        if had_sel {
            let end = block[(l1 - l0).min(block.len() - 1)].chars().count();
            self.splice_lines(l0, l1, &block, (l1, end), Some((l0, 0)));
        } else {
            let delta = old as i64 - block[bi].chars().count() as i64;
            self.splice_lines(l0, l1, &block, (cur.0, (cur.1 as i64 - delta).max(0) as usize), None);
        }
    }

    pub fn toggle_comment(&mut self) {
        let (l0, l1) = self.sel_line_range();
        let had_sel = self.editor.selection_bounds().is_some();
        let cur = self.cur_pos();
        let mut block = self.lines_in_range(l0, l1);
        let bi = cur.0 - l0;
        let old = block[bi].chars().count();
        let last = block.len().saturating_sub(1);
        crate::editops::toggle_comment(&mut block, 0, last, "// ");
        if had_sel {
            let end = block[(l1 - l0).min(block.len() - 1)].chars().count();
            self.splice_lines(l0, l1, &block, (l1, end), Some((l0, 0)));
        } else {
            let delta = block[bi].chars().count() as i64 - old as i64;
            self.splice_lines(l0, l1, &block, (cur.0, (cur.1 as i64 + delta).max(0) as usize), None);
        }
    }

    pub fn move_line_up(&mut self) {
        let cur = self.cur_pos();
        if cur.0 == 0 {
            return;
        }
        let mut block = self.lines_in_range(cur.0 - 1, cur.0);
        crate::editops::move_up(&mut block, 1); // swaps the two lines
        self.splice_lines(cur.0 - 1, cur.0, &block, (cur.0 - 1, cur.1), None);
    }

    pub fn move_line_down(&mut self) {
        let cur = self.cur_pos();
        if cur.0 + 1 >= self.line_count() {
            return;
        }
        let mut block = self.lines_in_range(cur.0, cur.0 + 1);
        crate::editops::move_down(&mut block, 0); // swaps the two lines
        self.splice_lines(cur.0, cur.0 + 1, &block, (cur.0 + 1, cur.1), None);
    }

    pub fn duplicate_line(&mut self) {
        let (l0, l1) = self.sel_line_range();
        let cur = self.cur_pos();
        let mut block = self.lines_in_range(l0, l1);
        let last = block.len().saturating_sub(1);
        crate::editops::duplicate(&mut block, 0, last);
        // Caret moves onto the duplicated copy (keeps typing in the new block).
        self.splice_lines(l0, l1, &block, (cur.0 + (l1 - l0 + 1), cur.1), None);
    }

    pub fn delete_line(&mut self) {
        let (l0, l1) = self.sel_line_range();
        // Removing the block: an empty replacement collapses the lines away (the
        // splice handles the surrounding newline and leaves ≥1 line).
        self.splice_lines(l0, l1, &[], (l0, 0), None);
    }

    /// Insert a newline that carries the current line's indentation (code mode).
    fn auto_indent_enter(&mut self) {
        let cur = self.editor.cursor();
        let ins = self.editor.with_buffer(|b| {
            let line = b.lines.get(cur.line).map(|l| l.text()).unwrap_or("");
            let col = byte_to_col(line, cur.index);
            crate::editops::auto_indent(line, col)
        });
        self.editor.start_change();
        self.editor.insert_string(&ins, None);
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        self.bump();
    }

    /// Smart Backspace: when only whitespace precedes the caret, delete back to the
    /// previous tab stop (matching VS Code / Sublime / Atom's `useTabStops`);
    /// otherwise a normal one-character backspace (also deletes a selection).
    fn smart_backspace(&mut self) {
        let mut fs = gpu::lock_font_system();
        if self.editor.selection_bounds().is_some() {
            self.editor.start_change();
            self.editor.action(&mut fs, Action::Backspace);
            if let Some(c) = self.editor.finish_change() {
                self.push_change(c);
            }
            drop(fs);
            self.bump();
            return;
        }
        let cur = self.editor.cursor();
        let spaces = self.editor.with_buffer(|b| {
            let line = b.lines.get(cur.line).map(|l| l.text()).unwrap_or("");
            let before = &line[..cur.index.min(line.len())];
            if !before.is_empty() && before.chars().all(|c| c == ' ') {
                before.chars().count()
            } else {
                0
            }
        });
        let remove = if spaces > 0 {
            let unit = crate::editops::INDENT.len();
            spaces - ((spaces - 1) / unit) * unit
        } else {
            1
        };
        self.editor.start_change();
        for _ in 0..remove {
            self.editor.action(&mut fs, Action::Backspace);
        }
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        drop(fs);
        self.bump();
    }

    /// Smart Home: jump to the first non-whitespace char, or to column 0 if already
    /// there. `shift` extends the selection.
    fn smart_home(&mut self, shift: bool) {
        let mut fs = gpu::lock_font_system();
        if shift {
            if matches!(self.editor.selection(), Selection::None) {
                let anchor = self.editor.cursor();
                self.editor.set_selection(Selection::Normal(anchor));
            }
        } else {
            self.editor.set_selection(Selection::None);
        }
        let cur = self.editor.cursor();
        let soft = self.editor.with_buffer(|b| {
            let line = b.lines.get(cur.line).map(|l| l.text()).unwrap_or("");
            line.char_indices()
                .find(|(_, c)| *c != ' ' && *c != '\t')
                .map(|(b, _)| b)
                .unwrap_or(line.len())
        });
        let motion = if cur.index != soft { Motion::SoftHome } else { Motion::Home };
        self.editor.action(&mut fs, Action::Motion(motion));
    }

    /// The caret's absolute character offset in the document (for Roslyn position).
    pub fn caret_offset(&self) -> usize {
        let text = self.text();
        cursor_char_off(&text, self.editor.cursor())
    }

    /// The identifier characters immediately before the caret (empty if none) —
    /// the prefix an autocomplete popup filters on.
    pub fn word_prefix(&self) -> String {
        let cur = self.editor.cursor();
        self.editor.with_buffer(|b| {
            let line = b.lines.get(cur.line).map(|l| l.text()).unwrap_or("");
            let before = &line[..cur.index.min(line.len())];
            let mut start = before.len();
            for (i, c) in before.char_indices().rev() {
                if c.is_alphanumeric() || c == '_' {
                    start = i;
                } else {
                    break;
                }
            }
            before[start..].to_string()
        })
    }

    /// If the caret sits within a leading `/command` token on the first line, return
    /// that token *including* the slash (e.g. `/co`, or `/` right after typing the
    /// slash). Unlike [`word_prefix`], this doesn't stop at hyphens, so it matches
    /// slash-command names like `code-review`. Empty when not in that context (no
    /// leading slash, caret past the first whitespace, or not on line 0).
    pub fn slash_prefix(&self) -> String {
        let cur = self.editor.cursor();
        if cur.line != 0 {
            return String::new();
        }
        self.editor.with_buffer(|b| {
            let line = b.lines.get(0).map(|l| l.text()).unwrap_or("");
            let before = &line[..cur.index.min(line.len())];
            if !before.starts_with('/') || before[1..].contains(char::is_whitespace) {
                return String::new();
            }
            before.to_string()
        })
    }

    /// Accept a completion: delete `prefix_len` characters before the caret and
    /// insert `text` in their place (one undoable change).
    pub fn complete(&mut self, prefix_len: usize, text: &str) {
        let mut fs = gpu::lock_font_system();
        self.editor.start_change();
        for _ in 0..prefix_len {
            self.editor.action(&mut fs, Action::Backspace);
        }
        self.editor.insert_string(text, None);
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        drop(fs);
        self.bump();
    }

    /// Target (line, char-col) for a code-aware word motion/deletion from the caret
    /// (handles line edges). `.` is a boundary except inside a float literal.
    fn word_target(&self, forward: bool) -> (usize, usize) {
        let cur = self.cur_pos();
        let last = self.editor.with_buffer(|b| b.lines.len().saturating_sub(1));
        let line = self
            .editor
            .with_buffer(|b| b.lines.get(cur.0).map(|l| l.text().to_string()).unwrap_or_default());
        let len = line.chars().count();
        if forward {
            if cur.1 >= len {
                if cur.0 < last { (cur.0 + 1, 0) } else { (cur.0, len) }
            } else {
                (cur.0, crate::editops::word_right(&line, cur.1))
            }
        } else if cur.1 == 0 {
            if cur.0 > 0 {
                let plen = self
                    .editor
                    .with_buffer(|b| b.lines.get(cur.0 - 1).map(|l| l.text().chars().count()).unwrap_or(0));
                (cur.0 - 1, plen)
            } else {
                (cur.0, 0)
            }
        } else {
            (cur.0, crate::editops::word_left(&line, cur.1))
        }
    }

    fn word_byte(&self, line: usize, col: usize) -> usize {
        self.editor
            .with_buffer(|b| b.lines.get(line).map(|l| col_to_byte(l.text(), col)).unwrap_or(0))
    }

    /// Move the caret one word left/right (code-aware); `shift` extends selection.
    fn word_move(&mut self, forward: bool, shift: bool) {
        if shift {
            if matches!(self.editor.selection(), Selection::None) {
                let anchor = self.editor.cursor();
                self.editor.set_selection(Selection::Normal(anchor));
            }
        } else {
            self.editor.set_selection(Selection::None);
        }
        let (tl, tc) = self.word_target(forward);
        let tb = self.word_byte(tl, tc);
        self.editor.set_cursor(Cursor::new(tl, tb));
        self.caret_dirty = true;
    }

    /// Delete one word back/forward (code-aware), or the selection if there is one.
    fn word_delete(&mut self, forward: bool) {
        if self.editor.selection_bounds().is_none() {
            let anchor = self.editor.cursor();
            let (tl, tc) = self.word_target(forward);
            self.editor.set_selection(Selection::Normal(anchor));
            self.editor.set_cursor(Cursor::new(tl, self.word_byte(tl, tc)));
        }
        self.editor.start_change();
        self.editor.delete_selection();
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        self.bump();
    }

    /// Move the caret to the start of line `line` (0-based), clearing any selection.
    pub fn goto_line(&mut self, line: usize) {
        let max = self.editor.with_buffer(|b| b.lines.len().saturating_sub(1));
        self.editor.set_selection(Selection::None);
        self.editor.set_cursor(Cursor::new(line.min(max), 0));
        self.caret_dirty = true;
    }

    /// Find `query` and select the match (search wraps). Returns true if found.
    pub fn find(&mut self, query: &str, forward: bool, case_sensitive: bool) -> bool {
        let text = self.text();
        let start = match self.editor.selection_bounds() {
            Some((a, b)) => {
                if forward {
                    cursor_char_off(&text, b)
                } else {
                    cursor_char_off(&text, a)
                }
            }
            None => cursor_char_off(&text, self.editor.cursor()),
        };
        if let Some((s, e)) = crate::editops::find(&text, query, start, forward, case_sensitive) {
            let cs = char_off_to_cursor(&text, s);
            let ce = char_off_to_cursor(&text, e);
            self.editor.set_selection(Selection::Normal(cs));
            self.editor.set_cursor(ce);
            self.caret_dirty = true;
            true
        } else {
            false
        }
    }

    /// Replace the current selection with `repl` (no-op without a selection).
    pub fn replace_selection(&mut self, repl: &str) {
        if self.editor.selection_bounds().is_none() {
            return;
        }
        self.clear_preedit();
        self.editor.start_change();
        self.editor.delete_selection();
        if !repl.is_empty() {
            self.editor.insert_string(repl, None);
        }
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        self.bump();
    }

    /// Replace every occurrence of `query` with `repl`. Returns the count.
    pub fn replace_all(&mut self, query: &str, repl: &str, case_sensitive: bool) -> u32 {
        let text = self.text();
        let (new, n) = crate::editops::replace_all(&text, query, repl, case_sensitive);
        if n > 0 {
            self.apply_lines(crate::editops::to_lines(&new), (0, 0), None);
        }
        n
    }

    pub fn content_height(&self) -> f32 {
        self.content_h
    }

    pub fn caret_rect(&self) -> [f32; 4] {
        self.caret
    }

    /// Set the action button: 0=none, 1=send (▶), 2=stop (■).
    pub fn set_button(&mut self, kind: u8) {
        self.button = kind;
    }

    /// True if physical-px (x, y) is inside the action button.
    pub fn hit_button(&self, x: f32, y: f32) -> bool {
        let r = self.button_rect;
        self.button != 0 && x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]
    }


    pub fn raw_texture(&self) -> *mut c_void {
        self.shared.raw_texture()
    }

    /// The wgpu texture view the editor renders into (for in-process compositing,
    /// e.g. the debugger showing this as an egui image via `register_native_texture`).
    pub fn texture_view(&self) -> &wgpu::TextureView {
        self.shared.view()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        let shared = create_target(&gpu::gpu().device, width, height);
        self.shared = shared;
    }

    /// The current text (lines joined by '\n').
    pub fn text(&self) -> String {
        self.editor.with_buffer(|b| {
            let mut s = String::new();
            for (i, line) in b.lines.iter().enumerate() {
                if i > 0 {
                    s.push('\n');
                }
                s.push_str(line.text());
            }
            s
        })
    }

    /// Replace the whole text and move the caret to the end.
    pub fn set_text(&mut self, text: &str) {
        let family_name = self.font_family.clone();
        let color = self.text_color;
        let mut fs = gpu::lock_font_system();
        let family = match family_name.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::Monospace,
        };
        let attrs = Attrs::new().family(family).color(color);
        self.editor
            .with_buffer_mut(|b| b.set_text(&mut fs, text, &attrs, Shaping::Advanced, None));
        self.editor.action(&mut fs, Action::Motion(Motion::BufferEnd));
        // A programmatic buffer replacement invalidates any in-progress IME
        // composition; drop it so a later clear_preedit can't delete against a stale
        // anchor that now points past the new buffer (cosmic-text split_off panic).
        self.preedit.clear();
        self.preedit_anchor = None;
        self.undo.clear(); // programmatic reset isn't an undoable edit
        self.redo.clear();
        self.undo_marks.clear();
        self.redo_marks.clear();
        // Give the freshly-loaded buffer a distinct version so the host's saved-mark
        // comparison is well-defined from the start.
        self.cur_mark = self.next_mark;
        self.next_mark = self.next_mark.wrapping_add(1);
        drop(fs);
        self.bump();
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Insert typed or IME-committed text at the caret. Shaping happens on the
    /// next `render` via `shape_as_needed`, so no font system is needed here.
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() || self.read_only {
            return;
        }
        // A real insert (e.g. IME commit) replaces any in-progress composition.
        self.clear_preedit();
        // Code mode: typing an opening bracket/quote also inserts its closer, with
        // the caret left between the pair.
        if self.code_mode {
            if let Some(close) = matching_close(text) {
                self.editor.start_change();
                self.editor.insert_string(text, None);
                let mid = self.editor.cursor();
                self.editor.insert_string(close, None);
                self.editor.set_cursor(mid);
                if let Some(c) = self.editor.finish_change() {
                    self.push_change(c);
                }
                self.bump();
                return;
            }
        }
        self.editor.start_change();
        self.editor.insert_string(text, None);
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        self.bump();
    }

    /// Insert `using <ns>;` near the top of the file (after the leading run of
    /// `using`/blank/comment lines), as one undoable change — unless it's already
    /// imported. The caret stays on its line (shifted down when the import lands
    /// above it). Used by completion's auto-import of an unimported type.
    pub fn add_using(&mut self, ns: &str) {
        let ns = ns.trim();
        if ns.is_empty() {
            return;
        }
        let want = format!("using {ns};");
        let lines: Vec<String> = self
            .editor
            .with_buffer(|b| b.lines.iter().map(|l| l.text().to_string()).collect());
        if lines.iter().any(|l| l.trim() == want) {
            return; // already imported
        }
        // Insert right after the last leading `using` (so it groups with them, before
        // any trailing blank line or comment); if there are none, just before the first
        // real code line (keeping a file-header comment block above it).
        let mut last_using_end: Option<usize> = None;
        let mut first_code = lines.len();
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            if t.starts_with("using ") {
                last_using_end = Some(i + 1);
            } else if t.is_empty()
                || t.starts_with("//")
                || t.starts_with("/*")
                || t.starts_with('*')
                || t.starts_with('#')
            {
                // file-header comment / blank / preprocessor directive — keep scanning
            } else {
                first_code = i;
                break;
            }
        }
        let target = last_using_end
            .unwrap_or(first_code)
            .min(lines.len().saturating_sub(1));
        let cur = self.editor.cursor();
        self.editor.start_change();
        self.editor.set_cursor(Cursor::new(target, 0));
        self.editor.insert_string(&format!("{want}\n"), None);
        if let Some(c) = self.editor.finish_change() {
            self.push_change(c);
        }
        // Restore the caret, shifted down a line if the import was inserted above it.
        let line = if cur.line >= target { cur.line + 1 } else { cur.line };
        self.editor.set_cursor(Cursor::new(line, cur.index));
        self.bump();
    }

    /// Undo the most recent edit (moves it to the redo stack).
    pub fn undo(&mut self) {
        if let Some(mut change) = self.undo.pop() {
            change.reverse();
            self.editor.apply_change(&change);
            self.redo.push(change);
            // Restore the prior state's version mark (remembering the current one so
            // redo can return to it).
            self.redo_marks.push(self.cur_mark);
            self.cur_mark = self.undo_marks.pop().unwrap_or(0);
            self.bump();
        }
    }

    /// Redo the most recently undone edit.
    pub fn redo(&mut self) {
        if let Some(mut change) = self.redo.pop() {
            change.reverse();
            self.editor.apply_change(&change);
            self.undo.push(change);
            self.undo_marks.push(self.cur_mark);
            self.cur_mark = self.redo_marks.pop().unwrap_or(0);
            self.bump();
        }
    }

    /// Apply a named editing key. The host decides Enter-to-send; `shift` with a
    /// motion key extends the selection (otherwise the motion clears it).
    pub fn key(&mut self, name: &str, _ctrl: bool, _alt: bool, shift: bool) {
        self.caret_dirty = true; // a key moves the caret → scroll to keep it visible

        // Read-only (debugger source view): drop text-mutating keys, keep navigation.
        if self.read_only
            && matches!(
                name,
                "DeleteWordBack"
                    | "DeleteWordForward"
                    | "Return"
                    | "KeypadEnter"
                    | "Backspace"
                    | "Delete"
                    | "DeleteToLineStart"
            )
        {
            return;
        }

        // Code-aware word motion/deletion (`.` is a boundary except in floats).
        match name {
            "WordLeft" => {
                self.word_move(false, shift);
                return;
            }
            "WordRight" => {
                self.word_move(true, shift);
                return;
            }
            "DeleteWordBack" => {
                self.word_delete(false);
                return;
            }
            "DeleteWordForward" => {
                self.word_delete(true);
                return;
            }
            _ => {}
        }

        // Code-mode keys that need their own buffer access (they lock the font
        // system themselves, so handle them before locking it here).
        if self.code_mode {
            match name {
                "Return" | "KeypadEnter" => {
                    self.auto_indent_enter();
                    return;
                }
                "LineStart" => {
                    self.smart_home(shift);
                    return;
                }
                "Backspace" => {
                    self.smart_backspace();
                    return;
                }
                _ => {}
            }
        }

        let mut fs = gpu::lock_font_system();
        let motion = match name {
            "LeftArrow" => Some(Motion::Left),
            "RightArrow" => Some(Motion::Right),
            "UpArrow" => Some(Motion::Up),
            "DownArrow" => Some(Motion::Down),
            "Home" | "LineStart" => Some(Motion::Home),
            "End" | "LineEnd" => Some(Motion::End),
            "DocStart" => Some(Motion::BufferStart),
            "DocEnd" => Some(Motion::BufferEnd),
            "PageUp" => Some(Motion::PageUp),
            "PageDown" => Some(Motion::PageDown),
            _ => None,
        };
        if let Some(m) = motion {
            if shift {
                // Start a selection at the caret if there isn't one, then move.
                if matches!(self.editor.selection(), Selection::None) {
                    let anchor = self.editor.cursor();
                    self.editor.set_selection(Selection::Normal(anchor));
                }
            } else {
                self.editor.set_selection(Selection::None);
            }
            self.editor.action(&mut fs, Action::Motion(m));
            return;
        }

        // Delete to line start (Cmd+Backspace). Word deletion is handled above
        // (code-aware); this only covers the line-start case.
        let del_to = match name {
            "DeleteToLineStart" => Some(Motion::Home),
            _ => None,
        };
        if let Some(m) = del_to {
            if matches!(self.editor.selection(), Selection::None) {
                let anchor = self.editor.cursor();
                self.editor.set_selection(Selection::Normal(anchor));
            }
            self.editor.action(&mut fs, Action::Motion(m));
            self.editor.start_change();
            self.editor.delete_selection();
            if let Some(c) = self.editor.finish_change() {
                self.push_change(c);
            }
            drop(fs);
            self.bump();
            return;
        }

        let action = match name {
            "Return" | "KeypadEnter" => Some(Action::Enter),
            "Backspace" => Some(Action::Backspace),
            "Delete" => Some(Action::Delete),
            _ => None,
        };
        if let Some(a) = action {
            self.editor.start_change();
            self.editor.action(&mut fs, a);
            if let Some(c) = self.editor.finish_change() {
                self.push_change(c);
            }
            drop(fs);
            self.bump();
        }
    }

    /// The (line text, character column) at a physical-px point, or None over the
    /// gutter. Non-destructive: it hit-tests via a click then restores the caret and
    /// selection, so it can drive hover tooltips without moving the cursor.
    pub fn pos_at_pixel(&mut self, x: f32, y: f32) -> Option<(String, usize)> {
        if x < self.gutter_px {
            return None;
        }
        let pad = PAD * self.scale;
        let bx = (x - pad - self.gutter_px).round() as i32;
        let by = (y - pad).round() as i32;
        let saved_cursor = self.editor.cursor();
        let saved_sel = self.editor.selection();
        {
            let mut fs = gpu::lock_font_system();
            self.editor.action(&mut fs, Action::Click { x: bx, y: by });
        }
        let cur = self.editor.cursor();
        self.editor.set_cursor(saved_cursor);
        self.editor.set_selection(saved_sel);
        let line = self.line_text(cur.line);
        let col = byte_to_col(&line, cur.index);
        Some((line, col))
    }

    /// Mouse interaction at physical px relative to the box: kind 0 = click (place
    /// caret), 1 = drag (extend selection), 2 = double-click (word), 3 = triple
    /// (line).
    pub fn mouse(&mut self, x: f32, y: f32, kind: u8) {
        let pad = PAD * self.scale;
        let bx = (x - pad - self.gutter_px).round() as i32;
        let by = (y - pad).round() as i32;
        let mut fs = gpu::lock_font_system();
        if kind == 2 {
            // Double-click: place the caret, then select the code-aware token under
            // it (cosmic-text's word select uses UAX#29, which keeps `foo.bar`
            // whole). `.` is a boundary except inside a float literal.
            self.editor.action(&mut fs, Action::Click { x: bx, y: by });
            let cur = self.editor.cursor();
            let line = self
                .editor
                .with_buffer(|b| b.lines.get(cur.line).map(|l| l.text().to_string()).unwrap_or_default());
            let col = byte_to_col(&line, cur.index);
            let (s, e) = crate::editops::word_at(&line, col);
            self.editor
                .set_selection(Selection::Normal(Cursor::new(cur.line, col_to_byte(&line, s))));
            self.editor.set_cursor(Cursor::new(cur.line, col_to_byte(&line, e)));
        } else {
            let action = match kind {
                1 => Action::Drag { x: bx, y: by },
                3 => Action::TripleClick { x: bx, y: by },
                _ => Action::Click { x: bx, y: by },
            };
            self.editor.action(&mut fs, action);
        }
        drop(fs);
        self.caret_dirty = true;
    }


    /// The current selection text, or None if nothing is selected.
    pub fn copy(&self) -> Option<String> {
        self.editor.copy_selection()
    }

    /// Copy then delete the selection (returns the copied text).
    pub fn cut(&mut self) -> Option<String> {
        let s = self.editor.copy_selection();
        if s.is_some() {
            self.editor.start_change();
            self.editor.delete_selection();
            if let Some(c) = self.editor.finish_change() {
                self.push_change(c);
            }
            self.bump();
        }
        s
    }

    pub fn select_all(&mut self) {
        let mut fs = gpu::lock_font_system();
        self.editor.action(&mut fs, Action::Motion(Motion::BufferStart));
        let start = self.editor.cursor();
        self.editor.set_selection(Selection::Normal(start));
        self.editor.action(&mut fs, Action::Motion(Motion::BufferEnd));
    }

    /// Drop any selection (used when focus moves to the transcript).
    pub fn clear_selection(&mut self) {
        self.editor.set_selection(Selection::None);
    }

    /// Show `text` as inline IME preedit (marked text) at the caret. It's inserted
    /// into the buffer so it wraps and grows the box exactly like real text, but
    /// is not committed (no undo) — passing it again replaces it, and an empty
    /// string removes it (on commit/cancel). The caret sits at its end.
    pub fn set_preedit(&mut self, text: &str) {
        self.clear_preedit();
        if text.is_empty() || self.read_only {
            return;
        }
        self.preedit_anchor = Some(self.editor.cursor());
        self.preedit = text.to_string();
        self.editor.insert_string(text, None);
        self.bump();
    }

    /// Clamp a cursor to the current buffer's bounds, so a stale position can't
    /// drive an out-of-range delete (which panics cosmic-text's `split_off`).
    fn clamp_cursor(&self, c: Cursor) -> Cursor {
        self.editor.with_buffer(|b| {
            let last = b.lines.len().saturating_sub(1);
            let line = c.line.min(last);
            let len = b.lines.get(line).map(|l| l.text().len()).unwrap_or(0);
            Cursor::new(line, c.index.min(len))
        })
    }

    /// Remove the current preedit (if any), restoring the caret to its anchor.
    fn clear_preedit(&mut self) {
        if self.preedit.is_empty() {
            return;
        }
        if let Some(anchor) = self.preedit_anchor.take() {
            // Clamp both ends defensively: the buffer may have changed under the
            // composition (send/clear), leaving the anchor past the current text.
            let anchor = self.clamp_cursor(anchor);
            let cur = self.clamp_cursor(self.editor.cursor());
            self.editor.set_cursor(cur);
            self.editor.set_selection(Selection::Normal(anchor));
            self.editor.delete_selection();
        }
        self.preedit.clear();
        self.bump();
    }

    pub fn render(&mut self) {
        // Self-heal a placeholder surface once Unity's device is available (no-op on
        // macOS, and after the first real frame).
        self.shared.begin_frame();
        let g = gpu::gpu();
        let s = self.scale;
        let font_size = 14.0 * s;
        let line_height = 20.0 * s;
        let pad = PAD * s;
        let width = self.width as f32;
        let height = self.height as f32;
        // Reserve a square on the right for the Send/Stop button, if any.
        let bw = if self.button != 0 { (28.0 * s).min(height - 2.0).max(0.0) } else { 0.0 };
        let btn_x = width - pad - bw;
        let btn_y = ((height - bw) / 2.0).max(0.0);
        self.button_rect = if bw > 0.0 { [btn_x, btn_y, bw, bw] } else { [0.0; 4] };
        let reserve = if bw > 0.0 { bw + pad } else { 0.0 };

        // Code-editor gutter: width from the logical line count, shifting the text
        // area right by `gutter_w`. Zero (and no inset) when the gutter is off.
        // With the breakpoint column enabled, a dot-sized strip is reserved LEFT of
        // the numbers so dots and digits never overlap; without it the gutter stays
        // at its plain line-number width.
        let line_count = self.editor.with_buffer(|b| b.lines.len()).max(1);
        let bp_dot = (line_height * 0.52).min(font_size);
        let bp_col = if self.bp_gutter { bp_dot + pad * 0.45 } else { 0.0 };
        let gutter_w = if self.gutter {
            let digits = ((line_count as f32).log10().floor() as usize + 1).max(2);
            // digits + a right gap for the numbers + a left lane for the diff markers
            // (+ a breakpoint-dot column between the markers and the numbers, when
            // debugging is enabled). The whole gutter is the click target for the
            // peek, so the marker lane also gives that a comfortable width.
            (digits as f32 * font_size * 0.6 + pad * 2.0 + 8.0 * s + bp_col).ceil()
        } else {
            0.0
        };
        self.gutter_px = gutter_w;
        let text_left = pad + gutter_w;
        let inner_w = (width - text_left - pad - reserve).max(1.0);

        // Recompute the tree-sitter highlight cache and git-diff markers only when
        // the text changed (once per edit, not per frame). Both need the full buffer
        // text, so build it once and share it. The highlight recompute reparses only
        // the changed region via the incremental highlighter.
        if self.hl_gen != self.edit_gen || self.diff_gen != self.edit_gen {
            let text = self.text();
            if self.hl_gen != self.edit_gen {
                let dark = self.highlight_dark;
                self.hl_cache = match self.highlighter.as_mut() {
                    Some(hl) => hl.highlight(&text, dark),
                    None => Vec::new(),
                };
                self.hl_gen = self.edit_gen;
                self.attrs_dirty = true; // spans (or the edited lines) changed
            }
            if self.diff_gen != self.edit_gen {
                // Buffer-vs-HEAD hunks with staged flags, plus synthesized hunks for
                // changes that exist only in the index (staged then reverted /
                // unsaved) — so the editor never disagrees with `git diff --cached`.
                let display = crate::diff::display_hunks(
                    self.diff_head.as_deref(),
                    self.diff_index.as_deref(),
                    &self.diff_index_hunks,
                    &text,
                );
                self.diff_hunks = display.iter().map(|d| d.hunk).collect();
                self.diff_staged = display.iter().map(|d| d.staged).collect();
                self.diff_index_new = display.iter().map(|d| d.index_new).collect();
                let n = text.split('\n').count();
                self.diff_markers = crate::diff::markers_from_hunks(&self.diff_hunks, n);
                crate::diff::apply_staged_bits(&mut self.diff_markers, &self.diff_hunks, &self.diff_staged);
                self.diff_gen = self.edit_gen;
            }
        }
        let use_hl = self.highlighter.is_some() && !self.hl_cache.is_empty();

        let color = self.text_color;
        let mut guard = gpu::lock_font_system();
        let fs = &mut *guard;
        // Borrow the family name directly (no per-frame clone): `attrs` is a field-
        // disjoint shared borrow of `self.font_family` that lives only across the
        // re-apply loop below, alongside the mutable borrow of `self.editor`.
        let family = match self.font_family.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::Monospace,
        };
        let attrs = Attrs::new().family(family).color(color);

        let inner_h = (height - pad * 2.0).max(line_height);
        let hl_cache = &self.hl_cache;
        let apply_attrs = self.attrs_dirty;
        self.editor.with_buffer_mut(|b| {
            b.set_metrics(fs, Metrics::new(font_size, line_height));
            // Code mode doesn't wrap (long lines scroll horizontally instead).
            b.set_wrap(fs, if self.code_mode { Wrap::None } else { Wrap::WordOrGlyph });
            // Re-apply per-line attrs only when something that feeds them changed
            // (highlight cache, theme, text color, or font), tracked by `attrs_dirty`.
            // set_attrs_list itself is cheap (it diffs and skips reshaping), but
            // allocating an AttrsList for every line every frame is needless churn —
            // a steady-state frame skips this loop entirely. The lines keep the attrs
            // set on the last applied frame.
            if apply_attrs {
                for (i, line) in b.lines.iter_mut().enumerate() {
                    if use_hl {
                        let mut al = glyphon::AttrsList::new(&attrs);
                        if let Some(ls) = hl_cache.get(i) {
                            let n = line.text().len();
                            for (r, c) in &ls.spans {
                                let end = r.end.min(n);
                                if r.start < end {
                                    al.add_span(r.start..end, &attrs.clone().color(*c));
                                }
                            }
                        }
                        line.set_attrs_list(al);
                    } else {
                        line.set_attrs_list(glyphon::AttrsList::new(&attrs));
                    }
                }
            }
        });
        self.attrs_dirty = false;

        // Pass 1: unbounded height (scroll reset to the top) to measure the FULL
        // content and the caret's absolute Y. `layout_runs` is visible-only when
        // bounded, so this is the only way to see the whole thing.
        self.editor.with_buffer_mut(|b| {
            b.set_size(fs, Some(inner_w), None);
            b.set_scroll(Scroll { line: 0, vertical: 0.0, horizontal: 0.0 });
        });
        self.editor.shape_as_needed(fs, false);
        let full_h = self.editor.with_buffer(measure_height);
        self.content_h = full_h + pad * 2.0;
        // Longest laid-out line width (no-wrap), to clamp horizontal scroll.
        let max_line_w = self
            .editor
            .with_buffer(|b| b.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max));
        let (caret_x, caret_y) = self
            .editor
            .cursor_position()
            .map(|(cx, cy)| (cx as f32, cy as f32))
            .unwrap_or((0.0, 0.0));

        // Scroll to keep the caret in view ONLY when it just moved (a key/edit/
        // click set `caret_dirty`). Wheel/scrollbar/trackpad scrolling leaves it
        // unset, so the view isn't snapped back to the caret. Always clamp.
        let max_scroll = (full_h - inner_h).max(0.0);
        let max_scroll_h = (max_line_w - inner_w).max(0.0);
        if self.caret_dirty {
            if caret_y < self.scroll_v {
                self.scroll_v = caret_y;
            } else if caret_y + line_height > self.scroll_v + inner_h {
                self.scroll_v = caret_y + line_height - inner_h;
            }
            // Horizontal caret-follow (code mode, no-wrap).
            if self.code_mode {
                let margin = font_size; // keep a little room past the caret
                if caret_x < self.scroll_h {
                    self.scroll_h = caret_x;
                } else if caret_x + margin > self.scroll_h + inner_w {
                    self.scroll_h = caret_x + margin - inner_w;
                }
            }
        }
        self.scroll_v = self.scroll_v.clamp(0.0, max_scroll);
        self.scroll_h = if self.code_mode { self.scroll_h.clamp(0.0, max_scroll_h) } else { 0.0 };
        self.caret_dirty = false;

        // Pass 2: bound to the box at the kept scroll offset.
        let scroll_h = self.scroll_h;
        self.editor.with_buffer_mut(|b| {
            b.set_size(fs, Some(inner_w), Some(inner_h));
            b.set_scroll(Scroll { line: 0, vertical: self.scroll_v, horizontal: scroll_h });
        });
        self.editor.shape_as_needed(fs, false);

        // Horizontal render offset (cosmic-text doesn't shift glyphs by
        // scroll.horizontal — the renderer must), so text + caret + selection are
        // drawn shifted left by the code-mode horizontal scroll. The gutter stays
        // fixed and the text is clipped at its left edge (see `text_bounds`).
        let hoff = self.scroll_h;

        let mut quads: Vec<Quad> = Vec::new();

        // Current-line highlight (code mode), behind everything else. Only when
        // there's no selection, so it doesn't fight the selection highlight.
        if self.code_mode && self.editor.selection_bounds().is_none() {
            if let Some((_, cy)) = self.editor.cursor_position() {
                let a = if self.highlight_dark { 0.05 } else { 0.05 };
                let c = if self.highlight_dark { 1.0 } else { 0.0 };
                quads.push(Quad {
                    x: 0.0,
                    y: pad + cy as f32,
                    w: width,
                    h: line_height,
                    color: [c, c, c, a],
                    radius: 0.0,
                });
            }
        }

        // Debugger execution line: a full-width amber band behind the text.
        if let Some(el) = self.exec_line {
            let mut top: Option<f32> = None;
            self.editor.with_buffer(|buf| {
                for run in buf.layout_runs() {
                    if run.line_i == el {
                        top = Some(run.line_top);
                        break;
                    }
                }
            });
            if let Some(t) = top {
                quads.push(Quad {
                    x: 0.0,
                    y: pad + t,
                    w: width,
                    h: line_height,
                    color: [0.902, 0.706, 0.251, 0.20],
                    radius: 0.0,
                });
            }
        }

        // Selection highlight (behind the text).
        if let Some((c1, c2)) = self.editor.selection_bounds() {
            self.editor.with_buffer(|buf| {
                for run in buf.layout_runs() {
                    if let Some((lx, w)) = run.highlight(c1, c2) {
                        quads.push(Quad {
                            x: text_left + lx - hoff,
                            y: pad + run.line_top,
                            w: w.max(2.0),
                            h: run.line_height,
                            color: [0.30, 0.50, 0.90, 0.35],
                            radius: 0.0,
                        });
                    }
                }
            });
        }

        // IME preedit: underline the marked text so composition reads as such
        // (it wraps and grows the box because it lives in the buffer).
        if !self.preedit.is_empty() {
            if let Some(anchor) = self.preedit_anchor {
                let caret = self.editor.cursor();
                let (c1, c2) = if (anchor.line, anchor.index) <= (caret.line, caret.index) {
                    (anchor, caret)
                } else {
                    (caret, anchor)
                };
                self.editor.with_buffer(|buf| {
                    for run in buf.layout_runs() {
                        if let Some((lx, w)) = run.highlight(c1, c2) {
                            quads.push(Quad {
                                x: text_left + lx - hoff,
                                y: pad + run.line_top + run.line_height - 2.0 * s,
                                w: w.max(2.0),
                                h: (1.5 * s).max(1.0),
                                color: [
                                    self.text_color.r() as f32 / 255.0,
                                    self.text_color.g() as f32 / 255.0,
                                    self.text_color.b() as f32 / 255.0,
                                    0.8,
                                ],
                                radius: 0.0,
                            });
                        }
                    }
                });
            }
        }

        // Caret quad from the editor's cursor position (buffer-relative px). Only
        // while the host window has focus — an unfocused input box shows no caret.
        if let Some((cx, cy)) = self.editor.cursor_position().filter(|_| self.focused) {
            let x = text_left + cx as f32 - hoff;
            let y = pad + cy as f32;
            self.caret = [x, y, 2.0 * s, line_height];
            quads.push(Quad {
                x,
                y,
                w: (2.0 * s).max(1.0),
                h: line_height,
                color: [self.text_color.r() as f32 / 255.0, self.text_color.g() as f32 / 255.0, self.text_color.b() as f32 / 255.0, 0.9],
                radius: 0.0,
            });
        }

        // Send/Stop button: a rounded chip with a ▶ / ■ glyph centered in it.
        let mut icon_buf: Option<Buffer> = None;
        let mut icon_left = 0.0_f32;
        let mut icon_top = 0.0_f32;
        if bw > 0.0 {
            let tc = self.text_color;
            quads.push(Quad {
                x: btn_x,
                y: btn_y,
                w: bw,
                h: bw,
                color: [
                    tc.r() as f32 / 255.0,
                    tc.g() as f32 / 255.0,
                    tc.b() as f32 / 255.0,
                    if self.button == 2 { 0.22 } else { 0.16 },
                ],
                radius: 6.0 * s,
            });
            let isize = bw * 0.5;
            let mut b = Buffer::new(fs, Metrics::new(isize, isize));
            b.set_size(fs, Some(bw), Some(bw));
            let ch = if self.button == 2 { "\u{25A0}" } else { "\u{25B6}" };
            b.set_text(
                fs,
                ch,
                &Attrs::new().family(Family::Monospace).color(tc),
                Shaping::Advanced,
                None,
            );
            b.shape_until_scroll(fs, false);
            let icon_w = b.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);
            icon_left = btn_x + ((bw - icon_w) / 2.0).max(0.0);
            icon_top = btn_y + ((bw - isize) / 2.0).max(0.0);
            icon_buf = Some(b);
        }

        // Line-number gutter: a background strip plus right-aligned numbers
        // aligned to each visible logical line's first row. The number glyph
        // buffers are cached and only rebuilt when the visible rows, gutter width,
        // font size, or color change (fingerprinted below) — not every frame.
        if self.gutter && gutter_w > 0.0 {
            let c = self.clear;
            let shade = 0.06_f32;
            quads.push(Quad {
                x: 0.0,
                y: 0.0,
                w: gutter_w,
                h: height,
                color: [
                    (c.r as f32 + shade).min(1.0),
                    (c.g as f32 + shade).min(1.0),
                    (c.b as f32 + shade).min(1.0),
                    1.0,
                ],
                radius: 0.0,
            });
            let mut tops: Vec<(usize, f32)> = Vec::new();
            let mut last = usize::MAX;
            self.editor.with_buffer(|buf| {
                for run in buf.layout_runs() {
                    if run.line_i != last {
                        last = run.line_i;
                        tops.push((run.line_i, run.line_top));
                    }
                }
            });

            // Git-diff markers: a colored bar at the gutter's left edge per changed
            // line (added/modified), plus a short wedge at a deletion boundary. Keyed
            // by logical line, so it lines up with `diff_markers` (both LF line-space).
            // Staged hunks draw HOLLOW (an inner cut of the gutter background), the
            // Zed idiom, so what's already staged reads apart from working changes.
            if !self.diff_markers.is_empty() {
                use crate::diff::{ADDED, DELETED_ABOVE, DELETED_BELOW, MODIFIED, STAGED, STAGED_DEL};
                let dark = self.highlight_dark;
                // Opaque (alpha 1) + a small radius: the quad SDF leaves a radius-0
                // interior at half alpha, which washed these out. Bars overlap by the
                // radius so consecutive changed lines read as one continuous mark.
                let added = if dark { [0.24, 0.64, 0.36, 1.0] } else { [0.18, 0.56, 0.30, 1.0] };
                let modified = if dark { [0.13, 0.54, 0.72, 1.0] } else { [0.10, 0.50, 0.80, 1.0] };
                let deleted = if dark { [0.86, 0.22, 0.22, 1.0] } else { [0.82, 0.12, 0.12, 1.0] };
                // The gutter strip's own color, for the hollow inner cut.
                let gbg = [
                    (c.r as f32 + shade).min(1.0),
                    (c.g as f32 + shade).min(1.0),
                    (c.b as f32 + shade).min(1.0),
                    1.0,
                ];
                let bar_w = (4.0 * s).max(3.0);
                let wedge_w = (8.0 * s).max(4.0);
                let wedge_h = (3.0 * s).max(2.0);
                let rr = 1.5 * s;
                let inset = (1.2 * s).max(1.0);
                for (li, top) in &tops {
                    let m = self.diff_markers.get(*li).copied().unwrap_or(0);
                    if m == 0 {
                        continue;
                    }
                    let y = pad + *top;
                    if m & (ADDED | MODIFIED) != 0 {
                        let color = if m & MODIFIED != 0 { modified } else { added };
                        quads.push(Quad { x: 0.0, y: y - rr, w: bar_w, h: line_height + rr * 2.0, color, radius: rr });
                        if m & STAGED != 0 {
                            // Hollow: cut the bar's interior back to the gutter color,
                            // leaving a frame (per-line cuts, so a staged line next to
                            // an unstaged one still reads correctly).
                            quads.push(Quad {
                                x: inset,
                                y: y - rr + inset,
                                w: (bar_w - inset * 2.0).max(1.0),
                                h: line_height + rr * 2.0 - inset * 2.0,
                                color: gbg,
                                radius: (rr - inset * 0.5).max(0.0),
                            });
                        }
                    }
                    for (bit, wy) in [
                        (DELETED_ABOVE, y - wedge_h * 0.5),
                        (DELETED_BELOW, y + line_height - wedge_h * 0.5),
                    ] {
                        if m & bit != 0 {
                            quads.push(Quad { x: 0.0, y: wy, w: wedge_w, h: wedge_h, color: deleted, radius: wedge_h * 0.5 });
                            if m & STAGED_DEL != 0 {
                                quads.push(Quad {
                                    x: inset,
                                    y: wy + inset * 0.5,
                                    w: (wedge_w - inset * 2.0).max(1.0),
                                    h: (wedge_h - inset).max(1.0),
                                    color: gbg,
                                    radius: (wedge_h - inset) * 0.5,
                                });
                            }
                        }
                    }
                }
            }

            // Breakpoint dots: always drawn when set (the caller only sets breakpoints
            // when debugging is on). Placed to the RIGHT of the diff-marker lane —
            // `bp_gutter` reserves their own column so dots never sit on the numbers,
            // but even without it a set breakpoint stays visible.
            if !self.breakpoints.is_empty() {
                let d = bp_dot;
                for (li, top) in &tops {
                    if self.breakpoints.contains(li) {
                        quads.push(Quad {
                            x: 8.0 * s + pad * 0.45,
                            y: pad + *top + (line_height - d) * 0.5,
                            w: d,
                            h: d,
                            color: [0.85, 0.16, 0.16, 1.0],
                            radius: d * 0.5,
                        });
                    }
                }
            }
            let nc = self.text_color;
            let key = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                tops.len().hash(&mut h);
                for (li, top) in &tops {
                    li.hash(&mut h);
                    top.to_bits().hash(&mut h);
                }
                gutter_w.to_bits().hash(&mut h);
                font_size.to_bits().hash(&mut h);
                line_height.to_bits().hash(&mut h);
                [nc.r(), nc.g(), nc.b()].hash(&mut h);
                h.finish()
            };
            if self.gutter_cache.key != Some(key) {
                let num_attrs = Attrs::new()
                    .family(Family::Monospace)
                    .color(Color::rgba(nc.r(), nc.g(), nc.b(), 120));
                let mut bufs: Vec<Buffer> = Vec::with_capacity(tops.len());
                let mut pos: Vec<(f32, f32)> = Vec::with_capacity(tops.len());
                for (li, top) in &tops {
                    let mut b = Buffer::new(fs, Metrics::new(font_size, line_height));
                    b.set_size(fs, Some(gutter_w), Some(line_height));
                    let label = (li + 1).to_string();
                    b.set_text(fs, &label, &num_attrs, Shaping::Advanced, None);
                    b.shape_until_scroll(fs, false);
                    let tw = b.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);
                    let left = (gutter_w - pad * 0.5 - tw).max(2.0);
                    bufs.push(b);
                    pos.push((left, pad + *top));
                }
                self.gutter_cache = GutterCache { key: Some(key), bufs, pos };
            }
        } else if self.gutter_cache.key.is_some() {
            self.gutter_cache = GutterCache::default();
        }

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        };
        // The main text clips at the gutter's right edge so horizontally-scrolled
        // content never draws over the line numbers / left padding.
        let text_bounds = TextBounds {
            left: text_left as i32,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        };
        let text_color = self.text_color;

        self.viewport.update(
            &g.queue,
            Resolution {
                width: self.width,
                height: self.height,
            },
        );
        self.quads
            .prepare(&g.device, &g.queue, (width, height), &quads);

        // Explicit disjoint borrows of distinct fields so the editor buffer can
        // be borrowed (shared) while the text renderer/atlas are borrowed mutably.
        {
            let editor = &self.editor;
            let text_renderer = &mut self.text_renderer;
            let atlas = &mut self.atlas;
            let swash_cache = &mut self.swash_cache;
            let viewport = &self.viewport;
            let icon_ref = icon_buf.as_ref();
            let gutter_ref = &self.gutter_cache.bufs;
            let gutter_pos_ref = &self.gutter_cache.pos;
            editor.with_buffer(|buf| {
                let mut areas: Vec<TextArea> = Vec::with_capacity(2 + gutter_ref.len());
                areas.push(TextArea {
                    buffer: buf,
                    left: text_left - hoff,
                    top: pad,
                    scale: 1.0,
                    bounds: text_bounds,
                    default_color: text_color,
                    custom_glyphs: &[],
                });
                for (gb, (gleft, gtop)) in gutter_ref.iter().zip(gutter_pos_ref.iter()) {
                    areas.push(TextArea {
                        buffer: gb,
                        left: *gleft,
                        top: *gtop,
                        scale: 1.0,
                        bounds,
                        default_color: text_color,
                        custom_glyphs: &[],
                    });
                }
                if let Some(ib) = icon_ref {
                    areas.push(TextArea {
                        buffer: ib,
                        left: icon_left,
                        top: icon_top,
                        scale: 1.0,
                        bounds,
                        default_color: text_color,
                        custom_glyphs: &[],
                    });
                }
                if let Err(e) =
                    text_renderer.prepare(&g.device, &g.queue, fs, atlas, viewport, areas, swash_cache)
                {
                    // Full atlas / transient device error: log and let the frame
                    // draw without fresh text rather than panic across the C ABI.
                    log::error!("unterm: input glyphon prepare failed: {e}");
                }
            });
        }

        // Autocomplete popup, drawn ON TOP of the text via its own renderers
        // (re-preparing the main ones for a second draw in the same submit would
        // race on their GPU buffers). Anchored at the caret; flips above on
        // overflow. The host supplies the items + selection.
        let popup = !self.completions.is_empty();
        let mut popup_buf: Option<Buffer> = None;
        let mut popup_text_pos = (0.0_f32, 0.0_f32);
        let mut popup_bounds = bounds;
        if popup {
            let n = self.completions.len();
            let row_h = line_height;
            let (cx, cy) = (self.caret[0], self.caret[1]);

            // Width fits the longest item (monospace estimate), capped to the surface
            // so the popup never needs to extend past the editor's bounds.
            let max_chars = self
                .completions
                .iter()
                .take(n.min(200))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(8);
            let pw = ((max_chars as f32) * font_size * 0.6 + pad * 2.0)
                .clamp(80.0, (width - 8.0).max(80.0));

            // Pick the side (below / above the caret) with more room and cap the row
            // count to what fits there — the popup is clipped to the editor surface.
            let below = (height - (cy + row_h) - pad).max(0.0);
            let above = (cy - pad).max(0.0);
            let use_below = below >= above;
            let room = if use_below { below } else { above };
            let fit_rows = ((room / row_h).floor() as usize).max(1);
            let visible = n.min(10).min(fit_rows);
            let ph = visible as f32 * row_h + pad;

            let px = cx.min((width - pw).max(0.0)).max(0.0);
            let py = if use_below { cy + row_h } else { (cy - ph).max(0.0) };
            let top = if self.compl_sel >= visible { self.compl_sel + 1 - visible } else { 0 };

            let bg = self.clear;
            let shade = if self.highlight_dark { 0.10_f32 } else { -0.06_f32 };
            let mut pquads: Vec<Quad> = Vec::with_capacity(2);
            pquads.push(Quad {
                x: px,
                y: py,
                w: pw,
                h: ph,
                color: [
                    (bg.r as f32 + shade).clamp(0.0, 1.0),
                    (bg.g as f32 + shade).clamp(0.0, 1.0),
                    (bg.b as f32 + shade).clamp(0.0, 1.0),
                    0.98,
                ],
                radius: 4.0 * s,
            });
            let sel_row = self.compl_sel.saturating_sub(top);
            pquads.push(Quad {
                x: px,
                y: py + pad * 0.5 + sel_row as f32 * row_h,
                w: pw,
                h: row_h,
                color: [0.30, 0.50, 0.90, 0.55],
                radius: 0.0,
            });
            self.popup_quads.prepare(&g.device, &g.queue, (width, height), &pquads);

            // Each item is a 1-char kind tag + the display label. Strip the tag for
            // display, keep it to color the row like the editor.
            let mut joined = String::new();
            let mut kinds: Vec<char> = Vec::with_capacity(visible);
            for i in 0..visible {
                let item = &self.completions[top + i];
                let mut chars = item.chars();
                kinds.push(chars.next().unwrap_or(' '));
                if i > 0 {
                    joined.push('\n');
                }
                joined.push_str(chars.as_str()); // the rest, after the kind tag
            }
            let mut b = Buffer::new(fs, Metrics::new(font_size, row_h));
            b.set_size(fs, Some(pw - pad), Some(ph));
            b.set_wrap(fs, Wrap::None); // labels never wrap — clip at the popup edge
            let base = Attrs::new().family(Family::Monospace).color(self.text_color);
            b.set_text(fs, &joined, &base, Shaping::Advanced, None);
            // Color each row like the editor: the name by its kind (function/type/
            // property…) and the signature/type part in the type color.
            let dark = self.highlight_dark;
            for (line, &kind) in b.lines.iter_mut().zip(kinds.iter()) {
                let label = line.text().to_string();
                line.set_attrs_list(popup_label_attrs(&label, kind, &base, dark));
            }
            b.shape_until_scroll(fs, false);
            popup_text_pos = (px + pad * 0.5, py + pad * 0.5);
            // Clip the popup text to its own box so long labels don't draw over the
            // surrounding editor.
            popup_bounds = TextBounds {
                left: px as i32,
                top: py as i32,
                right: (px + pw) as i32,
                bottom: (py + ph) as i32,
            };
            popup_buf = Some(b);
        }
        if let Some(pb) = popup_buf.as_ref() {
            let popup_text = &mut self.popup_text;
            let atlas = &mut self.atlas;
            let swash_cache = &mut self.swash_cache;
            let viewport = &self.viewport;
            let area = TextArea {
                buffer: pb,
                left: popup_text_pos.0,
                top: popup_text_pos.1,
                scale: 1.0,
                bounds: popup_bounds,
                default_color: text_color,
                custom_glyphs: &[],
            };
            if let Err(e) =
                popup_text.prepare(&g.device, &g.queue, fs, atlas, viewport, [area], swash_cache)
            {
                // Skip the popup's text this frame rather than panic; retried next frame.
                log::error!("unterm: popup glyphon prepare failed: {e}");
            }
        }

        // Diff peek tooltip: the hovered hunk as a unified diff — removed base lines
        // (red, `−`) then added current lines (green, `+`) — floated in a bordered,
        // drop-shadowed card near the pointer (own renderers, on top of the editor).
        // Per-row tints + signs make deletions (red-only) read apart from modifications
        // (red + green).
        let mut peek_buf: Option<Buffer> = None;
        let mut peek_text_pos = (0.0_f32, 0.0_f32);
        let mut peek_bounds = bounds;
        let mut has_peek = false;
        if let Some(pk) = self.peek.as_ref() {
            {
                let dark = self.highlight_dark;
                let vpad = pad * 0.5;
                let sign_w = font_size * 0.6 * 2.0; // "− " / "+ " column (monospace)

                // Card fits the content (monospace estimate), capped to what fits.
                let max_rows = (((height - pad * 2.0) / line_height).floor() as usize).max(1);
                let visible = pk.lines.len().min(max_rows);
                let longest = pk.lines[..visible]
                    .iter()
                    .map(|(_, t)| t.chars().count())
                    .max()
                    .unwrap_or(0);
                let text_w = (longest as f32 * font_size * 0.6).ceil();
                let card_w = (text_w + sign_w + pad * 2.0).clamp(60.0, (width - pad * 2.0).max(60.0));
                let card_h = visible as f32 * line_height + vpad * 2.0;

                // Top-left just below-right of the pointer — snug, like an OS tooltip
                // (just enough to clear the arrow cursor) — flipping / clamping so the
                // whole card stays on the surface.
                let (ptr_x, ptr_y) = pk.at;
                let ox = 4.0 * s;
                let oy = 8.0 * s;
                let mut card_x = ptr_x + ox;
                if card_x + card_w > width - pad {
                    card_x = ptr_x - ox - card_w; // flip to the pointer's left
                }
                card_x = card_x.clamp(pad, (width - pad - card_w).max(pad));
                let mut card_y = ptr_y + oy;
                if card_y + card_h > height - pad {
                    card_y = ptr_y - oy - card_h; // flip above the pointer
                }
                card_y = card_y.clamp(pad, (height - pad - card_h).max(pad));

                let shadow = [0.0, 0.0, 0.0, if dark { 0.5 } else { 0.25 }];
                let border = if dark { [0.34, 0.34, 0.36, 1.0] } else { [0.72, 0.72, 0.74, 1.0] };
                let bg = if dark { [0.15, 0.15, 0.16, 1.0] } else { [0.99, 0.99, 0.99, 1.0] };
                // Per-row diff tints (opaque, near-pure so red/green read vividly on the
                // sRGB target — keep the off-channels low so it doesn't wash to rose/mint).
                let row_del = if dark { [0.44, 0.05, 0.05, 1.0] } else { [1.0, 0.62, 0.62, 1.0] };
                let row_add = if dark { [0.05, 0.38, 0.10, 1.0] } else { [0.55, 0.88, 0.58, 1.0] };
                let bw = (1.0 * s).max(1.0); // border thickness
                let sh = (3.0 * s).max(2.0); // shadow offset
                let mut pquads: Vec<Quad> = Vec::with_capacity(3 + visible);
                // drop shadow, then border, then the neutral card fill.
                pquads.push(Quad { x: card_x - bw + sh, y: card_y - bw + sh, w: card_w + bw * 2.0, h: card_h + bw * 2.0, color: shadow, radius: 5.0 * s });
                pquads.push(Quad { x: card_x - bw, y: card_y - bw, w: card_w + bw * 2.0, h: card_h + bw * 2.0, color: border, radius: 5.0 * s });
                pquads.push(Quad { x: card_x, y: card_y, w: card_w, h: card_h, color: bg, radius: 4.0 * s });
                // one tinted row per diff line. A small radius is required for FULL
                // opacity: the quad shader's SDF leaves a radius-0 interior at alpha
                // 0.5 (it only measures the exterior distance). Rows overlap by the
                // radius so the rounding never opens seams between them.
                let rr = 2.0 * s;
                for (i, (added, _)) in pk.lines[..visible].iter().enumerate() {
                    let ry = card_y + vpad + i as f32 * line_height;
                    pquads.push(Quad { x: card_x, y: ry - rr, w: card_w, h: line_height + rr * 2.0, color: if *added { row_add } else { row_del }, radius: rr });
                }
                self.peek_quads.prepare(&g.device, &g.queue, (width, height), &pquads);

                // Prefix each row with its diff sign; color the whole row's text by kind.
                let joined: String = pk.lines[..visible]
                    .iter()
                    .map(|(added, t)| format!("{} {}", if *added { '+' } else { '−' }, t))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut b = Buffer::new(fs, Metrics::new(font_size, line_height));
                b.set_size(fs, Some((card_w - pad * 0.5).max(1.0)), Some(card_h));
                b.set_wrap(fs, Wrap::None); // code never wraps — clip at the card edge
                let del_c = if dark { Color::rgb(255, 210, 210) } else { Color::rgb(140, 20, 20) };
                let add_c = if dark { Color::rgb(205, 250, 210) } else { Color::rgb(15, 100, 30) };
                let base = Attrs::new().family(Family::Monospace).color(text_color);
                b.set_text(fs, &joined, &base, Shaping::Advanced, None);
                for (line, (added, _)) in b.lines.iter_mut().zip(pk.lines[..visible].iter()) {
                    let c = if *added { add_c } else { del_c };
                    line.set_attrs_list(glyphon::AttrsList::new(&base.clone().color(c)));
                }
                b.shape_until_scroll(fs, false);
                peek_text_pos = (card_x + pad * 0.5, card_y + vpad);
                peek_bounds = TextBounds {
                    left: card_x as i32,
                    top: card_y as i32,
                    right: (card_x + card_w) as i32,
                    bottom: (card_y + card_h) as i32,
                };
                peek_buf = Some(b);
                has_peek = true;
            }
        }
        if let Some(pb) = peek_buf.as_ref() {
            let peek_text = &mut self.peek_text;
            let atlas = &mut self.atlas;
            let swash_cache = &mut self.swash_cache;
            let viewport = &self.viewport;
            let area = TextArea {
                buffer: pb,
                left: peek_text_pos.0,
                top: peek_text_pos.1,
                scale: 1.0,
                bounds: peek_bounds,
                default_color: text_color,
                custom_glyphs: &[],
            };
            if let Err(e) =
                peek_text.prepare(&g.device, &g.queue, fs, atlas, viewport, [area], swash_cache)
            {
                log::error!("unterm: peek glyphon prepare failed: {e}");
            }
        }

        let mut encoder = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("unterm-input-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("unterm-input-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.shared.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.quads.render(&mut pass);
            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                // Draw the frame without text rather than abort; next frame retries.
                log::error!("unterm: input glyphon render failed: {e}");
            }
            if popup {
                self.popup_quads.render(&mut pass);
                if let Err(e) = self.popup_text.render(&self.atlas, &self.viewport, &mut pass) {
                    log::error!("unterm: popup glyphon render failed: {e}");
                }
            }
            if has_peek {
                self.peek_quads.render(&mut pass);
                if let Err(e) = self.peek_text.render(&self.atlas, &self.viewport, &mut pass) {
                    log::error!("unterm: peek glyphon render failed: {e}");
                }
            }
        }
        // Blit the freshly rendered frame into the surface's presented texture:
        // no-op on macOS (the IOSurface is the render target); on Windows it copies
        // the private target into the shared D3D texture Unity samples.
        self.shared.finish_frame(&mut encoder);
        g.queue.submit([encoder.finish()]);
        // Block until the GPU finishes (render + copy) so Unity samples a complete
        // texture (the zero-copy path has no readback to force completion).
        self.shared.present();
    }

}

/// Byte index of character column `col` within `line` (clamped to its end).
fn col_to_byte(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map(|(b, _)| b).unwrap_or(line.len())
}

/// Character column of byte index `byte` within `line`.
fn byte_to_col(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count()
}

/// Convert a document character offset to a `cosmic_text` cursor (line, byte index).
fn char_off_to_cursor(text: &str, off: usize) -> Cursor {
    let mut line = 0usize;
    let mut line_start = 0usize; // byte offset of the current line's start
    let mut count = 0usize;
    for (b, ch) in text.char_indices() {
        if count == off {
            return Cursor::new(line, b - line_start);
        }
        count += 1;
        if ch == '\n' {
            line += 1;
            line_start = b + ch.len_utf8();
        }
    }
    Cursor::new(line, text.len() - line_start)
}

/// Convert a cursor (line, byte index) to a document character offset.
fn cursor_char_off(text: &str, cur: Cursor) -> usize {
    let mut off = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if i == cur.line {
            return off + l[..cur.index.min(l.len())].chars().count();
        }
        off += l.chars().count() + 1; // +1 for the '\n'
    }
    off
}

/// Build colored spans for a completion popup label (`name : type`, `Foo(T) : R`):
/// the name is colored by the symbol's KIND (from the host) using the editor's
/// theme captures, and the rest (params, `:`, type) in the type color.
pub(crate) fn popup_label_attrs(label: &str, kind: char, base: &Attrs, dark: bool) -> glyphon::AttrsList {
    let mut al = glyphon::AttrsList::new(base);
    let paren = label.find('(');
    let colon = label.find(" : ");
    let name_end = match (paren, colon) {
        (Some(p), Some(c)) => p.min(c),
        (Some(p), None) => p,
        (None, Some(c)) => c,
        (None, None) => label.len(),
    };
    // Kind tag → editor highlight capture (matches the in-editor token colors).
    let capture = match kind {
        'M' | 'X' => "function",
        'P' | 'V' => "property",
        'F' | 'L' | 'A' => "variable",
        'T' | 'E' | 'U' => "type",
        'N' => "namespace",
        'C' => "constant",
        'K' => "keyword",
        'S' => "keyword", // slash-command "skill" — accent the user's own commands
        _ => "",
    };
    if name_end > 0 && !capture.is_empty() {
        al.add_span(0..name_end, &base.clone().color(crate::highlight::color_of(capture, dark)));
    }
    if name_end < label.len() {
        al.add_span(name_end..label.len(), &base.clone().color(crate::highlight::color_of("type", dark)));
    }
    al
}

/// If `text` is a single opening bracket/quote, the matching closer to auto-insert.
fn matching_close(text: &str) -> Option<&'static str> {
    match text {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        "\"" => Some("\""),
        "'" => Some("'"),
        _ => None,
    }
}

/// Laid-out pixel height of a shaped buffer.
fn measure_height(buffer: &Buffer) -> f32 {
    // Total content height, independent of the editor's scroll offset: cosmic-text
    // reports `line_top` relative to the scroll position, so taking just the max
    // bottom would shrink as you scroll down. Use (max bottom − min top) so the
    // measured height (which drives auto-grow) reflects content, not scroll.
    let mut min_top = f32::MAX;
    let mut max_bottom = 0.0_f32;
    for run in buffer.layout_runs() {
        min_top = min_top.min(run.line_top);
        max_bottom = max_bottom.max(run.line_top + run.line_height);
    }
    if min_top == f32::MAX {
        20.0
    } else {
        (max_bottom - min_top).max(20.0)
    }
}

fn create_target(device: &wgpu::Device, width: u32, height: u32) -> SharedSurface {
    surface::create_shared_target(device, width, height, FORMAT)
}
