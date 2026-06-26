//! Offscreen wgpu renderer for the agent panel (the chat view).
//!
//! Renders the role-tagged transcript as stacked, optionally-carded message
//! blocks (Zed-like) plus a pinned row of action buttons (permission options),
//! into an IOSurface-backed `MTLTexture` handed to Unity zero-copy. Unlike the
//! terminal (a fixed grid), the panel flows wrapped text and supports mouse
//! selection and scrollback.
//!
//! It shares the one process-global wgpu device, queue, glyph cache, and font
//! database (see [`crate::gpu`]) with the terminal renderer, so opening a panel
//! alongside terminals stays cheap and the glyph atlas is warmed once. The
//! panel itself holds no durable state: the conversation lives in the agent
//! session (which survives domain reloads), so the host recreates the panel on
//! reload and re-renders from the session transcript.

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};

use crate::markdown;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::gpu::{self, FORMAT};
use crate::surface::{self, SharedSurface};
use crate::quads::{Quad, QuadRenderer};
use std::ffi::c_void;

/// Record/unit separators used to encode role-tagged blocks in `set_text`.
/// `set_text` content is `role\x1f text` blocks joined by `\x1e`. Plain text
/// with neither separator is treated as a single agent block. Mirrors the
/// transcript format produced by [`crate::acp_session`].
pub(crate) const RS: char = '\u{1e}';
pub(crate) const US: char = '\u{1f}';

/// Family for code blocks / inline code. cosmic-text maps the generic
/// `Family::Monospace` to the (usually absent) "Fira Mono", so macOS would fall back
/// to an arbitrary monospace; name Menlo to match the terminal. On Windows the
/// generic already resolves to Consolas (= the terminal), so keep it.
#[cfg(target_os = "macos")]
fn code_family() -> Family<'static> {
    Family::Name("Menlo")
}
#[cfg(windows)]
fn code_family() -> Family<'static> {
    Family::Monospace
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    User,
    Agent,
    Thought,
    Tool,
    /// A follow-up prompt queued while a turn runs (dimmed; sent on turn end).
    Queued,
    /// An ExitPlanMode plan: Markdown like an agent block, but laid out in a
    /// capped-height, internally-scrollable box so a long plan can't dominate.
    Plan,
}

impl Role {
    fn from_tag(c: char) -> Role {
        match c {
            'u' => Role::User,
            't' => Role::Thought,
            'x' => Role::Tool,
            'q' => Role::Queued,
            'p' => Role::Plan,
            _ => Role::Agent,
        }
    }
    /// Whether the block gets a card background.
    fn carded(self) -> bool {
        matches!(self, Role::User | Role::Tool | Role::Queued)
    }
}

struct Block {
    role: Role,
    /// Header text (the folded view for a tool; the whole body otherwise).
    text: String,
    /// Tool blocks only: the toolUseId (stable fold-state key), a short one-line
    /// input preview shown next to the header, and the unfolded detail (full input +
    /// result output). Empty/None for every other role.
    tool_id: Option<String>,
    tool_preview: String,
    tool_detail: String,
}

/// A shaped, measured render item: one card-able, optionally-indented buffer.
/// Non-agent blocks produce one; an agent block is expanded into several (one per
/// Markdown element).
struct Measured {
    buffer: Buffer,
    text: String, // visible text (must match the buffer, for selection)
    height: f32,
    card_alpha: f32, // 0 = no card background
    indent: f32,     // left indent in physical px (lists / quotes)
    code: bool,      // a code block: rendered unwrapped + horizontally scrollable
    natural_w: f32,  // unwrapped content width (code blocks only)
    table: Option<TableMeasured>, // a drawn grid of cells (overrides `buffer`)
    tool_key: Option<u64>, // tool blocks: fold-state key (click-to-toggle target)
    header_h: f32,         // tool blocks: height of the header line(s), for the hit rect
}

/// A measured table: positioned cell buffers plus the grid-line/header rects to
/// draw behind them (all relative to the block's top-left).
struct TableMeasured {
    cells: Vec<TableCell>,
    lines: Vec<[f32; 4]>, // grid line rects (x, y, w, h)
    header_h: f32,        // height of the header row (for its background)
    total_w: f32,
}

struct TableCell {
    buffer: Buffer,
    text: String,
    dx: f32,
    dy: f32,
}

/// A laid-out block kept after render() so mouse hit-testing/selection works
/// between frames. `tx/ty` is the text top-left in physical px.
struct LaidBlock {
    buffer: Buffer,
    text: String,
    tx: f32,
    ty: f32,
    /// Horizontal scroll applied to a code block (0 for everything else); the
    /// buffer is drawn at `tx - hscroll` and clipped to `clip`.
    hscroll: f32,
    /// Clip rect (physical px x,y,w,h) for code blocks; None = full panel.
    clip: Option<[f32; 4]>,
    /// Content-hash key + max scroll, so the wheel handler can scroll this block.
    code_key: Option<u64>,
    max_hscroll: f32,
}

/// A caret position: byte offset into block `block`'s text.
#[derive(Clone, Copy, PartialEq)]
struct TextPos {
    block: usize,
    offset: usize,
}

impl TextPos {
    fn le(self, o: TextPos) -> bool {
        (self.block, self.offset) <= (o.block, o.offset)
    }
}

fn parse_blocks(text: &str) -> Vec<Block> {
    if !text.contains(RS) && !text.contains(US) {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Block {
            role: Role::Agent,
            text: text.to_string(),
            tool_id: None,
            tool_preview: String::new(),
            tool_detail: String::new(),
        }];
    }
    text.split(RS)
        .filter(|s| !s.is_empty())
        .map(|chunk| {
            let mut it = chunk.splitn(2, US);
            let tag = it.next().unwrap_or("a").chars().next().unwrap_or('a');
            let body = it.next().unwrap_or("");
            let role = Role::from_tag(tag);
            // A tool body is `<id>{US}<header>{US}<preview>{US}<detail>` (see
            // control::Conv); other roles carry their text verbatim.
            if role == Role::Tool {
                let mut f = body.splitn(4, US);
                let id = f.next().unwrap_or("");
                let header = f.next().unwrap_or("");
                let preview = f.next().unwrap_or("");
                let detail = f.next().unwrap_or("");
                return Block {
                    role,
                    text: header.to_string(),
                    tool_id: (!id.is_empty()).then(|| id.to_string()),
                    tool_preview: preview.to_string(),
                    tool_detail: detail.to_string(),
                };
            }
            Block {
                role,
                text: body.to_string(),
                tool_id: None,
                tool_preview: String::new(),
                tool_detail: String::new(),
            }
        })
        .collect()
}

pub struct PanelRenderer {
    width: u32,
    height: u32,
    shared: SharedSurface,

    clear: wgpu::Color,
    text_color: Color,
    /// Font family names per style (e.g. Unity's Inter Regular/SemiBold/Italic);
    /// None falls back to sans-serif. Heavier Inter weights use a distinct family
    /// name ("Inter SemiBold"), so Markdown bold/italic select the right face here
    /// rather than relying on synthesis. Missing glyphs (CJK, emoji) still resolve
    /// via cosmic-text's fallback.
    font_family: Option<String>,
    font_bold: Option<String>,
    font_italic: Option<String>,
    font_bold_italic: Option<String>,
    /// HiDPI factor: the panel renders at physical pixels and scales all sizes
    /// by this so text is crisp (no upscaling blur) on Retina displays.
    scale: f32,
    /// Vertical scroll offset in physical px (0 = bottom-anchored / latest).
    scroll: f32,
    /// Laid-out content height in physical px (for the host's scrollbar).
    content_h: f32,
    /// Action buttons (e.g. permission options) drawn pinned at the bottom.
    buttons: Vec<String>,
    /// Hit rects (physical px) for `buttons`, computed each render.
    button_rects: Vec<[f32; 4]>,
    /// Laid-out blocks from the last render, for hit-testing/selection.
    laid: Vec<LaidBlock>,
    /// Active text selection (anchor, focus), as block+offset positions.
    sel: Option<(TextPos, TextPos)>,
    /// Per-code-block horizontal scroll, keyed by the block's content hash so it
    /// survives re-layout as the transcript grows.
    hscroll: HashMap<u64, f32>,
    /// Unfolded tool blocks, keyed by the hash of their toolUseId (so the state
    /// survives the transcript growing / a tool flipping in_progress→completed).
    expanded: HashMap<u64, bool>,
    /// Hit rects (physical px) of each tool's header line + its fold key, for
    /// click-to-toggle. Computed each render (header line only, so the unfolded
    /// detail stays drag-selectable).
    tool_rects: Vec<(u64, [f32; 4])>,
    /// Internal vertical scroll of the capped ExitPlanMode plan box (physical px).
    plan_scroll: f32,
    /// Max plan_scroll (content height beyond the capped box; 0 = no scroll).
    plan_max: f32,
    /// The plan box's hit rect (physical px x,y,w,h) for wheel routing, if shown.
    plan_rect: Option<[f32; 4]>,

    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    quads: QuadRenderer,
}

impl PanelRenderer {
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

        Self {
            width,
            height,
            shared,
            // Themed by the host (Unity editor colors); these are fallbacks.
            clear: wgpu::Color {
                r: 0.051,
                g: 0.051,
                b: 0.051,
                a: 1.0,
            },
            text_color: Color::rgb(210, 210, 214),
            font_family: None,
            font_bold: None,
            font_italic: None,
            font_bold_italic: None,
            scale: 1.0,
            scroll: 0.0,
            content_h: 0.0,
            plan_scroll: 0.0,
            plan_max: 0.0,
            plan_rect: None,
            buttons: Vec::new(),
            button_rects: Vec::new(),
            laid: Vec::new(),
            sel: None,
            hscroll: HashMap::new(),
            expanded: HashMap::new(),
            tool_rects: Vec::new(),
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            quads,
        }
    }

    /// Background clear color, in linear space (sRGB target encodes on store).
    pub fn set_clear_color(&mut self, r: f64, g: f64, b: f64, a: f64) {
        self.clear = wgpu::Color { r, g, b, a };
    }

    /// Default text color, as sRGB bytes (glyphon color space).
    pub fn set_text_color(&mut self, r: u8, g: u8, b: u8, a: u8) {
        self.text_color = Color::rgba(r, g, b, a);
    }

    /// Load Regular/Bold/Italic/BoldItalic faces (empty path = skip). Each is
    /// recorded by its own family name so Markdown selects the real face.
    pub fn set_fonts(&mut self, regular: &str, bold: &str, italic: &str, bold_italic: &str) {
        let mut fs = gpu::font_system().lock().unwrap();
        if !regular.is_empty() {
            self.font_family = load_face(&mut fs, regular);
        }
        if !bold.is_empty() {
            self.font_bold = load_face(&mut fs, bold);
        }
        if !italic.is_empty() {
            self.font_italic = load_face(&mut fs, italic);
        }
        if !bold_italic.is_empty() {
            self.font_bold_italic = load_face(&mut fs, bold_italic);
        }
    }

    /// HiDPI scale (pixels per point). Layout and font sizes scale by this.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(0.5);
    }

    /// Scroll offset in physical px (0 = bottom). Clamped during layout.
    pub fn set_scroll(&mut self, scroll: f32) {
        self.scroll = scroll.max(0.0);
    }

    /// Total laid-out content height in physical px (from the last render).
    pub fn content_height(&self) -> f32 {
        self.content_h
    }

    /// Set the action buttons drawn pinned at the bottom (empty = none).
    pub fn set_buttons(&mut self, labels: Vec<String>) {
        self.buttons = labels;
    }

    /// Begin a selection at physical-px (x, y).
    pub fn selection_begin(&mut self, x: f32, y: f32) {
        if let Some(p) = self.hit_text(x, y) {
            self.sel = Some((p, p));
        } else {
            self.sel = None;
        }
    }

    /// Extend the active selection to (x, y).
    pub fn selection_update(&mut self, x: f32, y: f32) {
        if let (Some((a, _)), Some(p)) = (self.sel, self.hit_text(x, y)) {
            self.sel = Some((a, p));
        }
    }

    pub fn selection_clear(&mut self) {
        self.sel = None;
    }

    pub fn has_selection(&self) -> bool {
        matches!(self.sel, Some((a, b)) if a != b)
    }

    /// Select everything.
    pub fn select_all(&mut self) {
        if self.laid.is_empty() {
            self.sel = None;
            return;
        }
        let last = self.laid.len() - 1;
        self.sel = Some((
            TextPos { block: 0, offset: 0 },
            TextPos { block: last, offset: self.laid[last].text.len() },
        ));
    }

    /// The selected text (joining across blocks with newlines), or empty.
    pub fn selected_text(&self) -> String {
        let Some((a, b)) = self.sel else {
            return String::new();
        };
        let (lo, hi) = if a.le(b) { (a, b) } else { (b, a) };
        if lo == hi {
            return String::new();
        }
        let mut out = String::new();
        for bi in lo.block..=hi.block.min(self.laid.len().saturating_sub(1)) {
            let t = &self.laid[bi].text;
            let start = if bi == lo.block { lo.offset } else { 0 };
            let end = if bi == hi.block { hi.offset } else { t.len() };
            let start = clamp_boundary(t, start);
            let end = clamp_boundary(t, end.max(start));
            if bi != lo.block {
                out.push('\n');
            }
            out.push_str(&t[start..end]);
        }
        out
    }

    /// Map a physical-px point to a caret position.
    fn hit_text(&self, x: f32, y: f32) -> Option<TextPos> {
        if self.laid.is_empty() {
            return None;
        }
        // Pick the block whose vertical band contains y (clamp to ends).
        let mut block = 0usize;
        for (i, b) in self.laid.iter().enumerate() {
            if y >= b.ty {
                block = i;
            }
        }
        if y < self.laid[0].ty {
            block = 0;
        }
        let b = &self.laid[block];
        // Code blocks are drawn at `tx - hscroll`, so map back into buffer space.
        let cursor = b.buffer.hit(x - b.tx + b.hscroll, (y - b.ty).max(0.0));
        let offset = match cursor {
            Some(c) => cursor_to_offset(&b.buffer, c),
            None => {
                if x < b.tx - b.hscroll {
                    0
                } else {
                    b.text.len()
                }
            }
        };
        Some(TextPos {
            block,
            offset: offset.min(b.text.len()),
        })
    }

    /// Highlight quads for the current selection (physical px, overlay color).
    fn selection_quads(&self, overlay: f32) -> Vec<Quad> {
        let Some((a, b)) = self.sel else {
            return Vec::new();
        };
        let (lo, hi) = if a.le(b) { (a, b) } else { (b, a) };
        if lo == hi {
            return Vec::new();
        }
        let mut quads = Vec::new();
        for bi in lo.block..=hi.block.min(self.laid.len().saturating_sub(1)) {
            let blk = &self.laid[bi];
            let sel_start = if bi == lo.block { lo.offset } else { 0 };
            let sel_end = if bi == hi.block { hi.offset } else { blk.text.len() };
            let line_starts = line_starts(&blk.buffer);
            for run in blk.buffer.layout_runs() {
                let line_off = line_starts.get(run.line_i).copied().unwrap_or(0);
                let mut min_x = f32::MAX;
                let mut max_x = f32::MIN;
                for g in run.glyphs.iter() {
                    let gs = line_off + g.start;
                    let ge = line_off + g.end;
                    if ge > sel_start && gs < sel_end {
                        min_x = min_x.min(g.x);
                        max_x = max_x.max(g.x + g.w);
                    }
                }
                if max_x > min_x {
                    quads.push(Quad {
                        x: blk.tx - blk.hscroll + min_x,
                        y: blk.ty + run.line_top,
                        w: max_x - min_x,
                        h: run.line_height,
                        color: [overlay, overlay, overlay, 0.28],
                        radius: 0.0,
                    });
                }
            }
        }
        quads
    }

    /// Scroll the code block under (x, y) horizontally by `dx` physical px.
    /// Returns true if a code block consumed it (so the host can keep the event).
    pub fn scroll_h(&mut self, x: f32, y: f32, dx: f32) -> bool {
        for l in &self.laid {
            let (Some(key), Some(c)) = (l.code_key, l.clip) else {
                continue;
            };
            if x >= c[0] && x <= c[0] + c[2] && y >= c[1] && y <= c[1] + c[3] && l.max_hscroll > 0.5 {
                let cur = self.hscroll.get(&key).copied().unwrap_or(0.0);
                self.hscroll.insert(key, (cur + dx).clamp(0.0, l.max_hscroll));
                return true;
            }
        }
        false
    }

    /// Scroll the capped plan box under (x, y) vertically by `dy` physical px.
    /// Returns true if the plan box consumed it (so the host keeps the event and
    /// doesn't scroll the whole transcript).
    pub fn scroll_v(&mut self, x: f32, y: f32, dy: f32) -> bool {
        if let Some(r) = self.plan_rect {
            if self.plan_max > 0.5
                && x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]
            {
                self.plan_scroll = (self.plan_scroll + dy).clamp(0.0, self.plan_max);
                return true;
            }
        }
        false
    }

    /// The fold key of the tool whose header line is at physical-px (x, y), or None.
    /// Only the header line is hit-tested, so an unfolded tool's detail text stays
    /// selectable.
    pub fn hit_tool(&self, x: f32, y: f32) -> Option<u64> {
        for (key, r) in &self.tool_rects {
            if x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3] {
                return Some(*key);
            }
        }
        None
    }

    /// Toggle a tool block's folded/unfolded state.
    pub fn toggle_tool(&mut self, key: u64) {
        let e = self.expanded.entry(key).or_insert(false);
        *e = !*e;
    }

    /// Index of the button at physical-px point (x, y), or -1.
    pub fn hit_button(&self, x: f32, y: f32) -> i32 {
        for (i, r) in self.button_rects.iter().enumerate() {
            if x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3] {
                return i as i32;
            }
        }
        -1
    }

    /// Raw `id<MTLTexture>` of the current render target.
    pub fn raw_texture(&self) -> *mut c_void {
        self.shared.raw_texture()
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

    /// Render the role-tagged transcript as stacked, optionally-carded blocks
    /// (Zed-like). Newest content is bottom-anchored so it stays in view.
    pub fn render(&mut self, text: &str) {
        // Self-heal a placeholder surface once Unity's device is available (no-op on
        // macOS, and after the first real frame).
        self.shared.begin_frame();
        let g = gpu::gpu();
        let mut fs = gpu::font_system().lock().unwrap();

        let s = self.scale;
        let pad = 14.0 * s;
        let gap = 8.0 * s;
        let card_pad = 10.0 * s;
        let radius = 8.0 * s;
        let font_size = 14.0 * s;
        let line_height = 20.0 * s;

        let width = self.width as f32;
        let height = self.height as f32;
        let content_w = (width - pad * 2.0).max(1.0);
        let text_color = self.text_color;

        // Card overlay adapts to the theme: light wash on dark bg, dark on light.
        let lum = 0.2126 * self.clear.r + 0.7152 * self.clear.g + 0.0722 * self.clear.b;
        let overlay = if lum < 0.5 { 1.0_f32 } else { 0.0_f32 };

        // No editor font loaded → generic monospace, which (unlike `SansSerif`)
        // reliably resolves a face in the embedded FontSystem on Windows too
        // (Consolas), matching the terminal renderer's fallback.
        let regular = self
            .font_family
            .as_deref()
            .map(Family::Name)
            .unwrap_or(Family::Monospace);
        let bold = self.font_bold.as_deref().map(Family::Name).unwrap_or(regular);
        let italic = self.font_italic.as_deref().map(Family::Name).unwrap_or(regular);
        let bold_italic = self
            .font_bold_italic
            .as_deref()
            .map(Family::Name)
            .unwrap_or(bold);
        let faces = Faces {
            regular,
            bold,
            italic,
            bold_italic,
        };

        let blocks = parse_blocks(text);

        // First pass: shape + measure each block. Non-agent blocks render plain;
        // agent blocks are parsed as Markdown and expanded into styled items.
        let mut measured: Vec<Measured> = Vec::new();
        // Tool fold keys seen this frame, to GC `expanded` for tools that scrolled
        // out of the (capped) transcript reconstruction.
        let mut live_tool_keys: Vec<u64> = Vec::new();
        // The contiguous run of measured items produced by the plan block, if any.
        let mut plan_range: Option<(usize, usize)> = None;
        // The plan box reserves `card_pad` of inner padding on each side, so its
        // Markdown is measured (wrapped) at a narrower width.
        let plan_w = (content_w - card_pad * 2.0).max(1.0);
        for b in &blocks {
            if (b.role == Role::Agent || b.role == Role::Plan) && !b.text.is_empty() {
                let start = measured.len();
                let w = if b.role == Role::Plan { plan_w } else { content_w };
                for mb in markdown::parse(&b.text) {
                    if let Some(m) = build_md(
                        &mut fs, &mb, w, font_size, line_height, card_pad, faces, text_color,
                        lum < 0.5,
                    ) {
                        measured.push(m);
                    }
                }
                if b.role == Role::Plan {
                    plan_range = Some((start, measured.len()));
                }
            } else if b.role == Role::Tool {
                // A tool block folds: the header line always shows; the detail (full
                // input + result output) shows only when unfolded. The whole thing is
                // one card that grows, keyed for click-to-toggle by the toolUseId.
                let has_detail = !b.tool_detail.is_empty();
                // Only a tool with content to reveal is foldable/clickable.
                let key = b.tool_id.as_deref().filter(|_| has_detail).map(hash_str);
                if let Some(k) = key {
                    live_tool_keys.push(k);
                }
                let unfolded = key
                    .map(|k| self.expanded.get(&k).copied().unwrap_or(false))
                    .unwrap_or(false);
                // The fold state shows as a disclosure triangle pinned to the header's
                // right edge (drawn in the placement pass), so reserve space for it and
                // keep it out of the flowed text.
                let reserve = if has_detail { font_size * 1.6 } else { 0.0 };
                // The header renders at the base size; the input preview and (when
                // unfolded) the detail render smaller. Build the folded form first for
                // the click-target height, then re-build with the detail if open.
                let mut m = build_tool(
                    &mut fs, &b.text, &b.tool_preview, None, content_w, reserve, font_size,
                    line_height, card_pad, faces.regular, text_color,
                );
                let header_h = (m.height - card_pad * 2.0).max(0.0);
                if unfolded {
                    m = build_tool(
                        &mut fs, &b.text, &b.tool_preview, Some(&b.tool_detail), content_w, reserve,
                        font_size, line_height, card_pad, faces.regular, text_color,
                    );
                }
                m.tool_key = key;
                m.header_h = header_h;
                measured.push(m);
            } else {
                measured.push(build_plain(
                    &mut fs, b, content_w, font_size, line_height, card_pad, faces.regular, text_color,
                ));
            }
        }
        self.expanded.retain(|k, _| live_tool_keys.contains(k));

        // Measure the action-button labels and pack them into rows that fit the
        // width, so a narrow panel wraps the buttons instead of overflowing; the
        // reserved bottom strip grows with the number of rows.
        let btn_pad_x = 12.0 * s;
        let gap_b = 8.0 * s;
        let btn_h = line_height + card_pad;
        let avail = (width - pad * 2.0).max(1.0);
        let mut btn_buffers: Vec<Buffer> = Vec::new();
        let mut btn_w: Vec<f32> = Vec::new(); // full button width incl. h-padding
        for label in &self.buttons {
            let mut buf = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
            buf.set_size(&mut fs, None, None);
            buf.set_text(
                &mut fs,
                label,
                &Attrs::new().family(faces.regular).color(text_color),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut fs, false);
            btn_w.push(measure_width(&buf) + btn_pad_x * 2.0);
            btn_buffers.push(buf);
        }
        let mut rows: Vec<Vec<usize>> = Vec::new();
        {
            let mut cur: Vec<usize> = Vec::new();
            let mut cur_w = 0.0_f32;
            for (i, &w) in btn_w.iter().enumerate() {
                let add = if cur.is_empty() { w } else { w + gap_b };
                if !cur.is_empty() && cur_w + add > avail {
                    rows.push(std::mem::take(&mut cur));
                    cur.push(i);
                    cur_w = w;
                } else {
                    cur.push(i);
                    cur_w += add;
                }
            }
            if !cur.is_empty() {
                rows.push(cur);
            }
        }
        let button_block_h = if rows.is_empty() {
            0.0
        } else {
            rows.len() as f32 * btn_h + (rows.len() as f32 - 1.0) * gap_b
        };
        // Buttons scroll inline with the transcript (not pinned to the bottom), so
        // no bottom strip is reserved; they're added to the content total below and
        // placed right after the last block, `gap` beneath it.
        let buttons_h = if rows.is_empty() { 0.0 } else { gap + button_block_h };
        let content_bottom = height - pad;

        // The plan box is capped: it contributes at most `plan_region_h` to the
        // laid-out height (the overflow scrolls internally), so factor that out of
        // the totals below. `plan_region_h`/`plan_total` are 0 when no plan shows.
        let plan_cap = (height * 0.5).min(280.0 * s);
        let (plan_total, plan_inner_h) = match plan_range {
            Some((a, b)) => {
                let items = &measured[a..b];
                let pt = items.iter().map(|m| m.height).sum::<f32>()
                    + gap * items.len().saturating_sub(1) as f32;
                (pt, pt.min(plan_cap))
            }
            None => (0.0, 0.0),
        };
        // The drawn box adds `card_pad` of inner padding above and below the content.
        let plan_box_h = if plan_range.is_some() { plan_inner_h + card_pad * 2.0 } else { 0.0 };
        self.plan_max = (plan_total - plan_inner_h).max(0.0);
        self.plan_scroll = self.plan_scroll.clamp(0.0, self.plan_max);

        // Bottom-anchor when the transcript overflows; `scroll` reveals older
        // content (clamped so 0 = latest, max = top). The plan box counts as its
        // capped box height, not its full content height.
        let total: f32 = measured.iter().map(|m| m.height).sum::<f32>()
            + gap * measured.len().saturating_sub(1) as f32
            - (plan_total - plan_box_h)
            + buttons_h;
        self.content_h = total + pad * 2.0;
        let viewport_h = content_bottom - pad;
        let mut y = if total <= viewport_h {
            pad
        } else {
            let max_scroll = total - viewport_h;
            let scroll = self.scroll.min(max_scroll);
            content_bottom - total + scroll
        };

        // Second pass: place each block, emitting a card quad where needed.
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        };
        let mut quads: Vec<Quad> = Vec::new();
        // Disclosure triangles (tool fold markers) drawn at the header's right edge,
        // outside the selectable text. Shaped here, blitted after the laid blocks.
        let mut deco: Vec<(Buffer, f32, f32)> = Vec::new();
        self.laid.clear();
        self.tool_rects.clear();
        let mut live_keys: Vec<u64> = Vec::new();
        self.plan_rect = None;
        let mut plan_box_top = 0.0_f32; // y of the plan box's top (set at its first item)
        let mut plan_y = 0.0_f32; // running y inside the plan box (offset by plan_scroll)
        for (idx, m) in measured.into_iter().enumerate() {
            // Plan items: lay out inside a capped box that scrolls internally, so a
            // long plan can't push the rest off-screen. Clip every item to the box
            // and offset by `plan_scroll`; the box advances `y` by its capped height.
            if let Some((ps, pe)) = plan_range {
                if idx >= ps && idx < pe {
                    if idx == ps {
                        plan_box_top = y;
                        self.plan_rect = Some([pad, y, content_w, plan_box_h]);
                        quads.push(Quad {
                            x: pad,
                            y,
                            w: content_w,
                            h: plan_box_h,
                            color: [overlay, overlay, overlay, 0.06],
                            radius,
                        });
                        // Content starts card_pad below the box top, offset by scroll.
                        plan_y = y + card_pad - self.plan_scroll;
                    }
                    // Clip to the padded inner area so content never paints over the
                    // box's padding (top/bottom/left/right).
                    let top = (plan_box_top + card_pad).max(0.0);
                    let bottom = (plan_box_top + plan_box_h - card_pad).min(self.height as f32);
                    let clip = [pad + card_pad, top, plan_w, (bottom - top).max(0.0)];
                    let tx = pad + card_pad + m.indent;
                    if let Some(tbl) = m.table {
                        for c in tbl.cells {
                            self.laid.push(LaidBlock {
                                buffer: c.buffer, text: c.text, tx: tx + c.dx, ty: plan_y + c.dy,
                                hscroll: 0.0, clip: Some(clip), code_key: None, max_hscroll: 0.0,
                            });
                        }
                    } else {
                        self.laid.push(LaidBlock {
                            buffer: m.buffer, text: m.text, tx, ty: plan_y,
                            hscroll: 0.0, clip: Some(clip), code_key: None, max_hscroll: 0.0,
                        });
                    }
                    plan_y += m.height + gap;
                    if idx == pe - 1 {
                        // Scroll thumb on the box's right edge when it overflows.
                        if self.plan_max > 0.5 && plan_total > 0.0 {
                            let track = plan_inner_h;
                            let thumb_h = (track * plan_inner_h / plan_total).max(20.0 * s);
                            let thumb_y = plan_box_top + card_pad
                                + (self.plan_scroll / self.plan_max) * (track - thumb_h);
                            quads.push(Quad {
                                x: pad + content_w - 4.0 * s, y: thumb_y, w: 3.0 * s, h: thumb_h,
                                color: [overlay, overlay, overlay, 0.45], radius: 1.5 * s,
                            });
                        }
                        y = plan_box_top + plan_box_h + gap;
                    }
                    continue;
                }
            }
            // A table draws its own grid + header background, then places each
            // cell buffer as its own laid block (so selection still works).
            if let Some(tbl) = m.table {
                let x0 = pad + m.indent;
                quads.push(Quad {
                    x: x0,
                    y,
                    w: tbl.total_w,
                    h: tbl.header_h,
                    color: [overlay, overlay, overlay, 0.07],
                    radius: 0.0,
                });
                for ln in &tbl.lines {
                    quads.push(Quad {
                        x: x0 + ln[0],
                        y: y + ln[1],
                        w: ln[2],
                        h: ln[3],
                        color: [overlay, overlay, overlay, 0.22],
                        radius: 0.0,
                    });
                }
                for c in tbl.cells {
                    self.laid.push(LaidBlock {
                        buffer: c.buffer,
                        text: c.text,
                        tx: x0 + c.dx,
                        ty: y + c.dy,
                        hscroll: 0.0,
                        clip: None,
                        code_key: None,
                        max_hscroll: 0.0,
                    });
                }
                y += m.height + gap;
                continue;
            }
            let card = m.card_alpha > 0.0;
            let x0 = pad + m.indent;
            if card {
                quads.push(Quad {
                    x: x0,
                    y,
                    w: (content_w - m.indent).max(1.0),
                    h: m.height,
                    color: [overlay, overlay, overlay, m.card_alpha],
                    radius,
                });
            }
            let (tx, ty) = if card {
                (x0 + card_pad, y + card_pad)
            } else {
                (x0, y)
            };
            // A tool's header line is the click-to-toggle target (only the header, so
            // an unfolded tool's detail below stays drag-selectable). A disclosure
            // triangle (▶ folded / ▼ open) is pinned to the header's right edge.
            if let Some(key) = m.tool_key {
                let hit_h = (card_pad * 2.0 + m.header_h).min(m.height);
                self.tool_rects.push((key, [x0, y, (content_w - m.indent).max(1.0), hit_h]));
                let glyph = if self.expanded.get(&key).copied().unwrap_or(false) {
                    "▼"
                } else {
                    "▶"
                };
                let mut gb = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
                gb.set_size(&mut fs, None, None);
                gb.set_text(
                    &mut fs,
                    glyph,
                    &Attrs::new().family(faces.regular).color(dim(text_color, 150)),
                    Shaping::Advanced,
                    None,
                );
                gb.shape_until_scroll(&mut fs, false);
                let gw = measure_width(&gb);
                let gx = x0 + (content_w - m.indent).max(1.0) - card_pad - gw;
                deco.push((gb, gx, y + card_pad));
            }
            // Code blocks: render unwrapped, clipped to the card, with a per-block
            // horizontal scroll (clamped to how far the longest line overflows).
            let (hscroll, clip, code_key, max_hscroll) = if m.code {
                let inner_w = (content_w - card_pad * 2.0).max(1.0);
                let max_h = (m.natural_w - inner_w).max(0.0);
                let key = hash_str(&m.text);
                let cur = self.hscroll.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_h);
                self.hscroll.insert(key, cur);
                live_keys.push(key);
                let left = tx.max(0.0);
                let top = y.max(0.0);
                let right = (tx + inner_w).min(self.width as f32);
                let bottom = (y + m.height).min(self.height as f32);
                let clip = [left, top, (right - left).max(0.0), (bottom - top).max(0.0)];
                (cur, Some(clip), Some(key), max_h)
            } else {
                (0.0, None, None, 0.0)
            };
            self.laid.push(LaidBlock {
                buffer: m.buffer,
                text: m.text,
                tx,
                ty,
                hscroll,
                clip,
                code_key,
                max_hscroll,
            });
            y += m.height + gap;
        }
        self.hscroll.retain(|k, _| live_keys.contains(k));

        // Selection highlight (above the cards, below the text).
        quads.extend(self.selection_quads(overlay));

        // Action buttons: placed right after the last block (scrolling with the
        // transcript, not pinned), wrapped into rows (each row right-aligned).
        // Positions are indexed by button so hit-testing maps a click to its option.
        self.button_rects.clear();
        let mut btn_pos: Vec<(f32, f32)> = vec![(0.0, 0.0); self.buttons.len()];
        if !rows.is_empty() {
            let mut rects = vec![[0.0_f32; 4]; self.buttons.len()];
            let block_top = y;
            for (ri, row) in rows.iter().enumerate() {
                let row_w: f32 = row.iter().map(|&i| btn_w[i]).sum::<f32>()
                    + gap_b * row.len().saturating_sub(1) as f32;
                let mut bx = (width - pad - row_w).max(pad);
                let row_y = block_top + ri as f32 * (btn_h + gap_b);
                for &i in row {
                    let bw = btn_w[i];
                    quads.push(Quad {
                        x: bx,
                        y: row_y,
                        w: bw,
                        h: btn_h,
                        color: [overlay, overlay, overlay, 0.18],
                        radius,
                    });
                    btn_pos[i] = (bx + btn_pad_x, row_y + (btn_h - line_height) / 2.0);
                    rects[i] = [bx, row_y, bw, btn_h];
                    bx += bw + gap_b;
                }
            }
            self.button_rects = rects;
        }

        // Text areas: blocks (from self.laid) then button labels.
        let mut areas: Vec<TextArea> = Vec::with_capacity(self.laid.len() + btn_buffers.len());
        for l in &self.laid {
            let b = l.clip.map_or(bounds, |c| TextBounds {
                left: c[0] as i32,
                top: c[1] as i32,
                right: (c[0] + c[2]) as i32,
                bottom: (c[1] + c[3]) as i32,
            });
            areas.push(TextArea {
                buffer: &l.buffer,
                left: l.tx - l.hscroll,
                top: l.ty,
                scale: 1.0,
                bounds: b,
                default_color: text_color,
                custom_glyphs: &[],
            });
        }
        for (i, buf) in btn_buffers.iter().enumerate() {
            let (lx, ty) = btn_pos[i];
            areas.push(TextArea {
                buffer: buf,
                left: lx,
                top: ty,
                scale: 1.0,
                bounds,
                default_color: text_color,
                custom_glyphs: &[],
            });
        }
        for (buf, gx, gy) in &deco {
            areas.push(TextArea {
                buffer: buf,
                left: *gx,
                top: *gy,
                scale: 1.0,
                bounds,
                default_color: text_color,
                custom_glyphs: &[],
            });
        }

        self.viewport.update(
            &g.queue,
            Resolution {
                width: self.width,
                height: self.height,
            },
        );
        self.quads
            .prepare(&g.device, &g.queue, (width, height), &quads);
        self.text_renderer
            .prepare(
                &g.device,
                &g.queue,
                &mut fs,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            )
            .expect("unterm: panel glyphon prepare failed");

        let mut encoder = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("unterm-panel-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("unterm-panel-pass"),
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
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .expect("unterm: panel glyphon render failed");
        }
        // Blit the freshly rendered frame into the surface's presented texture:
        // no-op on macOS (the IOSurface is the render target); on Windows it copies
        // the private target into the shared D3D texture Unity samples.
        self.shared.finish_frame(&mut encoder);
        g.queue.submit([encoder.finish()]);
        // Block until the GPU finishes (render + copy) so Unity samples a complete
        // texture (the zero-copy path has no readback to force completion).
        self.shared.present();
        self.atlas.trim();
    }

}

/// Stable content hash, used to key a code block's horizontal scroll.
fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// The bundled syntax definitions (loaded once; ~360KB embedded, all languages).
fn syntax_set() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// A bundled theme matching the panel's light/dark background.
fn theme(dark: bool) -> &'static Theme {
    static T: OnceLock<ThemeSet> = OnceLock::new();
    let ts = T.get_or_init(ThemeSet::load_defaults);
    let name = if dark { "base16-ocean.dark" } else { "InspiredGitHub" };
    &ts.themes[name]
}

/// Syntax-highlight a code block into colored pieces, or None for an unknown
/// language (caller falls back to plain monospace).
fn highlight_code(text: &str, lang: &str, dark: bool) -> Option<Vec<(String, Color)>> {
    let ss = syntax_set();
    let syntax = ss.find_syntax_by_token(lang)?;
    let mut h = HighlightLines::new(syntax, theme(dark));
    let mut out = Vec::new();
    for line in LinesWithEndings::from(text) {
        let ranges = h.highlight_line(line, ss).ok()?;
        for (st, piece) in ranges {
            let c = st.foreground;
            out.push((piece.to_string(), Color::rgb(c.r, c.g, c.b)));
        }
    }
    Some(out)
}

/// Load a font file and return its first family name (None on failure).
fn load_face(fs: &mut FontSystem, path: &str) -> Option<String> {
    // A family name (not a file path) is already in the shared FontSystem (a system
    // UI font): address it directly, no file load.
    if !gpu::is_font_path(path) {
        return Some(path.to_string());
    }
    let db = fs.db_mut();
    if let Err(e) = db.load_font_file(path) {
        log::warn!("unterm: failed to load font {path}: {e}");
        return None;
    }
    db.faces()
        .last()
        .and_then(|f| f.families.first())
        .map(|(name, _)| name.clone())
}

/// The font families to use per inline style (each may be a distinct face, e.g.
/// "Inter SemiBold" for bold). Missing variants fall back toward Regular.
#[derive(Clone, Copy)]
struct Faces<'a> {
    regular: Family<'a>,
    bold: Family<'a>,
    italic: Family<'a>,
    bold_italic: Family<'a>,
}

impl<'a> Faces<'a> {
    fn pick(&self, bold: bool, italic: bool) -> Family<'a> {
        match (bold, italic) {
            (true, true) => self.bold_italic,
            (true, false) => self.bold,
            (false, true) => self.italic,
            (false, false) => self.regular,
        }
    }
}

/// Build one plain (non-Markdown) block: user prompts, thoughts, tool lines.
fn build_plain(
    fs: &mut FontSystem,
    b: &Block,
    content_w: f32,
    font_size: f32,
    line_height: f32,
    card_pad: f32,
    family: Family<'_>,
    text_color: Color,
) -> Measured {
    let carded = b.role.carded();
    let inner_w = if carded { content_w - card_pad * 2.0 } else { content_w };
    let color = match b.role {
        Role::Thought => dim(text_color, 150),
        Role::Tool => dim(text_color, 205),
        Role::Queued => dim(text_color, 120),
        _ => text_color,
    };
    let mut buffer = Buffer::new(fs, Metrics::new(font_size, line_height));
    buffer.set_size(fs, Some(inner_w.max(1.0)), None);
    buffer.set_wrap(fs, Wrap::WordOrGlyph);
    buffer.set_text(
        fs,
        &b.text,
        &Attrs::new().family(family).color(color),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(fs, false);
    let text_h = measure_height(&buffer);
    let card_alpha = match b.role {
        Role::User => 0.10,
        Role::Tool => 0.06,
        Role::Queued => 0.05,
        _ => 0.0,
    };
    let height = if carded { text_h + card_pad * 2.0 } else { text_h };
    Measured {
        buffer,
        text: b.text.clone(),
        height,
        card_alpha,
        indent: 0.0,
        code: false,
        natural_w: 0.0,
        table: None,
        tool_key: None,
        header_h: 0.0,
    }
}

/// Build a tool block into one card: the `header` (disclosure + status + name) at
/// the base font size, then the input `preview` and (when unfolded) the `detail`
/// at a smaller, dimmer size — all in a single buffer via per-span metrics, so the
/// whole tool stays one growable card. `detail = None` is the folded form.
#[allow(clippy::too_many_arguments)]
fn build_tool(
    fs: &mut FontSystem,
    header: &str,
    preview: &str,
    detail: Option<&str>,
    content_w: f32,
    reserve_right: f32,
    font_size: f32,
    line_height: f32,
    card_pad: f32,
    family: Family<'_>,
    text_color: Color,
) -> Measured {
    let inner_w = content_w - card_pad * 2.0 - reserve_right;
    let small = Metrics::new(font_size * 0.84, line_height * 0.84);
    let head_attrs = Attrs::new().family(family).color(dim(text_color, 205));
    let small_attrs = Attrs::new().family(family).color(dim(text_color, 165)).metrics(small);

    // Owned span texts (kept alive for the borrowed slices set_rich_text takes).
    let mut texts: Vec<String> = vec![header.to_string()];
    let mut attrs: Vec<Attrs> = vec![head_attrs.clone()];
    if !preview.is_empty() {
        texts.push(format!("  {preview}"));
        attrs.push(small_attrs.clone());
    }
    if let Some(d) = detail {
        texts.push(format!("\n{d}"));
        attrs.push(small_attrs.clone());
    }

    let mut buffer = Buffer::new(fs, Metrics::new(font_size, line_height));
    buffer.set_size(fs, Some(inner_w.max(1.0)), None);
    buffer.set_wrap(fs, Wrap::WordOrGlyph);
    let spans: Vec<(&str, Attrs)> = texts
        .iter()
        .map(|s| s.as_str())
        .zip(attrs.iter().cloned())
        .collect();
    buffer.set_rich_text(fs, spans, &head_attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(fs, false);

    let text: String = texts.concat();
    let height = measure_height(&buffer) + card_pad * 2.0;
    Measured {
        buffer,
        text,
        height,
        card_alpha: 0.06,
        indent: 0.0,
        code: false,
        natural_w: 0.0,
        table: None,
        tool_key: None,
        header_h: 0.0,
    }
}

/// Shape a run of inline spans into one buffer (bold/italic/inline-code/link),
/// returning the buffer and its concatenated visible text (for selection).
fn shape_spans(
    fs: &mut FontSystem,
    spans: &[markdown::Span],
    width: f32,
    font_size: f32,
    line_height: f32,
    force_bold: bool,
    base_color: Color,
    faces: Faces,
) -> (Buffer, String) {
    let mut buf = Buffer::new(fs, Metrics::new(font_size, line_height));
    buf.set_size(fs, Some(width.max(1.0)), None);
    buf.set_wrap(fs, Wrap::WordOrGlyph);
    let parts: Vec<(&str, Attrs)> = spans
        .iter()
        .map(|sp| {
            let bold = sp.bold || force_bold;
            let fam = if sp.code {
                code_family()
            } else {
                faces.pick(bold, sp.italic)
            };
            let col = if sp.link {
                Color::rgb(90, 160, 250)
            } else if sp.code {
                dim(base_color, 235)
            } else {
                base_color
            };
            let mut a = Attrs::new().family(fam).color(col);
            if bold {
                a = a.weight(Weight::BOLD);
            }
            if sp.italic {
                a = a.style(Style::Italic);
            }
            (sp.text.as_str(), a)
        })
        .collect();
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    buf.set_rich_text(
        fs,
        parts,
        &Attrs::new().family(faces.regular).color(base_color),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(fs, false);
    (buf, text)
}

/// Build one Markdown block into a measured render item (None for a rule).
fn build_md(
    fs: &mut FontSystem,
    mb: &markdown::Block,
    content_w: f32,
    font_size: f32,
    line_height: f32,
    card_pad: f32,
    faces: Faces,
    text_color: Color,
    dark: bool,
) -> Option<Measured> {
    use markdown::Block as MB;
    match mb {
        MB::Paragraph(spans) => {
            let (buffer, text) =
                shape_spans(fs, spans, content_w, font_size, line_height, false, text_color, faces);
            let height = measure_height(&buffer);
            Some(Measured { buffer, text, height, card_alpha: 0.0, indent: 0.0, code: false, natural_w: 0.0, table: None, tool_key: None, header_h: 0.0 })
        }
        MB::Heading { level, spans } => {
            let scale = match level {
                1 => 1.5,
                2 => 1.3,
                3 => 1.15,
                _ => 1.05,
            };
            let (buffer, text) = shape_spans(
                fs,
                spans,
                content_w,
                font_size * scale,
                line_height * scale,
                true,
                text_color,
                faces,
            );
            let height = measure_height(&buffer);
            Some(Measured { buffer, text, height, card_alpha: 0.0, indent: 0.0, code: false, natural_w: 0.0, table: None, tool_key: None, header_h: 0.0 })
        }
        MB::Code { text, lang, diff } => {
            // Code is rendered unwrapped and clipped to the card; the panel
            // scrolls it horizontally, so lay it out at its natural width.
            let mut buf = Buffer::new(fs, Metrics::new(font_size, line_height));
            buf.set_size(fs, None, None);
            buf.set_wrap(fs, Wrap::None);
            let highlighted = if *diff {
                None
            } else {
                lang.as_deref().and_then(|l| highlight_code(text, l, dark))
            };
            if *diff {
                // Color +/- lines like a diff (kept whole, including newlines).
                let lines: Vec<String> = text.split_inclusive('\n').map(|l| l.to_string()).collect();
                let parts: Vec<(&str, Attrs)> = lines
                    .iter()
                    .map(|l| {
                        let c = if l.starts_with('+') {
                            Color::rgb(120, 200, 120)
                        } else if l.starts_with('-') {
                            Color::rgb(220, 120, 120)
                        } else {
                            text_color
                        };
                        (l.as_str(), Attrs::new().family(code_family()).color(c))
                    })
                    .collect();
                buf.set_rich_text(
                    fs,
                    parts,
                    &Attrs::new().family(code_family()).color(text_color),
                    Shaping::Advanced,
                    None,
                );
            } else if let Some(pieces) = &highlighted {
                // Syntax-highlighted: one colored span per token.
                let parts: Vec<(&str, Attrs)> = pieces
                    .iter()
                    .map(|(t, c)| (t.as_str(), Attrs::new().family(code_family()).color(*c)))
                    .collect();
                buf.set_rich_text(
                    fs,
                    parts,
                    &Attrs::new().family(code_family()).color(text_color),
                    Shaping::Advanced,
                    None,
                );
            } else {
                buf.set_text(
                    fs,
                    text,
                    &Attrs::new().family(code_family()).color(text_color),
                    Shaping::Advanced,
                    None,
                );
            }
            buf.shape_until_scroll(fs, false);
            let height = measure_height(&buf) + card_pad * 2.0;
            let natural_w = measure_width(&buf);
            Some(Measured {
                buffer: buf,
                text: text.clone(),
                height,
                card_alpha: 0.08,
                indent: 0.0,
                code: true,
                natural_w,
                table: None,
                tool_key: None,
                header_h: 0.0,
            })
        }
        MB::ListItem { depth, marker, spans } => {
            let indent = (*depth as f32) * (font_size * 1.2);
            let mut all: Vec<markdown::Span> = Vec::with_capacity(spans.len() + 1);
            all.push(markdown::Span {
                text: format!("{marker} "),
                ..Default::default()
            });
            all.extend(spans.iter().cloned());
            let (buffer, text) = shape_spans(
                fs,
                &all,
                content_w - indent,
                font_size,
                line_height,
                false,
                text_color,
                faces,
            );
            let height = measure_height(&buffer);
            Some(Measured { buffer, text, height, card_alpha: 0.0, indent, code: false, natural_w: 0.0, table: None, tool_key: None, header_h: 0.0 })
        }
        MB::Quote(spans) => {
            let indent = font_size;
            let (buffer, text) = shape_spans(
                fs,
                spans,
                content_w - indent,
                font_size,
                line_height,
                false,
                dim(text_color, 180),
                faces,
            );
            let height = measure_height(&buffer);
            Some(Measured { buffer, text, height, card_alpha: 0.0, indent, code: false, natural_w: 0.0, table: None, tool_key: None, header_h: 0.0 })
        }
        MB::Table { headers, rows } => {
            build_table(fs, headers, rows, content_w, font_size, line_height, faces, text_color)
        }
        MB::Rule => None,
    }
}

/// Lay out a Markdown table into positioned cell buffers plus grid-line rects.
/// Columns size to their content, scaled down to fit `content_w` (cells then
/// wrap). Returns None for an empty table.
fn build_table(
    fs: &mut FontSystem,
    headers: &[Vec<markdown::Span>],
    rows: &[Vec<Vec<markdown::Span>>],
    content_w: f32,
    font_size: f32,
    line_height: f32,
    faces: Faces,
    text_color: Color,
) -> Option<Measured> {
    let cols = headers
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if cols == 0 {
        return None;
    }
    let border = (font_size / 14.0).round().max(1.0);
    let pad_x = font_size * 0.6;
    let pad_y = line_height * 0.3;

    // All rows (header first) as cell-span slices, padded to `cols`.
    let empty: Vec<markdown::Span> = Vec::new();
    let all: Vec<Vec<&Vec<markdown::Span>>> = std::iter::once(headers)
        .chain(rows.iter().map(|r| r.as_slice()))
        .map(|r: &[Vec<markdown::Span>]| (0..cols).map(|i| r.get(i).unwrap_or(&empty)).collect())
        .collect();

    // Natural column widths (unwrapped cell content), then fit to content_w.
    let mut col_w = vec![0.0_f32; cols];
    for row in &all {
        for (i, cell) in row.iter().enumerate() {
            let (buf, _) = shape_spans(fs, cell, 1.0e6, font_size, line_height, false, text_color, faces);
            col_w[i] = col_w[i].max(measure_width(&buf) + pad_x * 2.0);
        }
    }
    let min_col = font_size * 2.0 + pad_x * 2.0;
    let avail = (content_w - border * (cols as f32 + 1.0)).max(min_col * cols as f32);
    let sum: f32 = col_w.iter().sum();
    if sum > avail {
        let factor = avail / sum;
        for w in &mut col_w {
            *w = (*w * factor).max(min_col);
        }
    }

    // Shape each cell wrapped to its column, tracking per-row heights.
    let n_rows = all.len();
    let mut cells_grid: Vec<Vec<(Buffer, String, f32)>> = Vec::with_capacity(n_rows);
    let mut row_h = vec![0.0_f32; n_rows];
    for (r, row) in all.iter().enumerate() {
        let mut out_row = Vec::with_capacity(cols);
        for (i, cell) in row.iter().enumerate() {
            let inner = (col_w[i] - pad_x * 2.0).max(1.0);
            let header = r == 0;
            let (buf, text) =
                shape_spans(fs, cell, inner, font_size, line_height, header, text_color, faces);
            let h = measure_height(&buf);
            row_h[r] = row_h[r].max(h);
            out_row.push((buf, text, h));
        }
        row_h[r] += pad_y * 2.0;
        cells_grid.push(out_row);
    }

    // Place cells + build grid lines (all relative to the table's top-left).
    let total_w = border * (cols as f32 + 1.0) + col_w.iter().sum::<f32>();
    let total_h = border * (n_rows as f32 + 1.0) + row_h.iter().sum::<f32>();
    let mut cells = Vec::new();
    let mut lines = Vec::new();

    // Vertical lines.
    let mut vx = 0.0;
    for i in 0..=cols {
        lines.push([vx, 0.0, border, total_h]);
        if i < cols {
            vx += border + col_w[i];
        }
    }
    // Horizontal lines + cell placement.
    let mut hy = 0.0;
    for r in 0..n_rows {
        lines.push([0.0, hy, total_w, border]);
        let cy = hy + border + pad_y;
        let mut cx = border;
        for (i, (buf, text, _)) in cells_grid[r].drain(..).enumerate() {
            cells.push(TableCell {
                buffer: buf,
                text,
                dx: cx + pad_x,
                dy: cy,
            });
            cx += col_w[i] + border;
        }
        hy += border + row_h[r];
    }
    lines.push([0.0, hy, total_w, border]); // bottom

    Some(Measured {
        buffer: Buffer::new(fs, Metrics::new(font_size, line_height)),
        text: String::new(),
        height: total_h,
        card_alpha: 0.0,
        indent: 0.0,
        code: false,
        natural_w: 0.0,
        table: Some(TableMeasured {
            cells,
            lines,
            header_h: border + row_h[0],
            total_w,
        }),
        tool_key: None,
        header_h: 0.0,
    })
}

/// Scale a color's alpha (for dimmed thoughts / tool text).
fn dim(c: Color, alpha: u8) -> Color {
    Color::rgba(c.r(), c.g(), c.b(), alpha)
}

/// Byte offset in the buffer's full text where each BufferLine starts.
fn line_starts(buffer: &Buffer) -> Vec<usize> {
    let mut starts = Vec::with_capacity(buffer.lines.len());
    let mut off = 0;
    for line in &buffer.lines {
        starts.push(off);
        off += line.text().len() + 1; // +1 for the '\n' separator
    }
    starts
}

/// Convert a cosmic-text cursor to a byte offset in the buffer's full text.
fn cursor_to_offset(buffer: &Buffer, cursor: glyphon::cosmic_text::Cursor) -> usize {
    let starts = line_starts(buffer);
    starts.get(cursor.line).copied().unwrap_or(0) + cursor.index
}

/// Clamp a byte index to the nearest char boundary at or below it.
fn clamp_boundary(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        i = s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Widest laid-out line of a shaped buffer (physical px).
fn measure_width(buffer: &Buffer) -> f32 {
    buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
}

/// Laid-out pixel height of a shaped buffer.
fn measure_height(buffer: &Buffer) -> f32 {
    let mut h = 0.0_f32;
    for run in buffer.layout_runs() {
        h = h.max(run.line_top + run.line_height);
    }
    if h <= 0.0 {
        20.0
    } else {
        h
    }
}

fn create_target(device: &wgpu::Device, width: u32, height: u32) -> SharedSurface {
    surface::create_shared_target(device, width, height, FORMAT)
}
