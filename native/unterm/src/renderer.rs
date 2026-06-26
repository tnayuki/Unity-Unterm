//! Offscreen wgpu/glyphon renderer for a terminal grid.
//!
//! Renders an `alacritty_terminal` grid into an IOSurface-backed `MTLTexture`
//! that Unity samples zero-copy. Cell backgrounds, the cursor, and selection
//! are instanced quads; text is one shaped glyphon `Buffer` per visible row,
//! relying on a monospace font so columns line up. Everything renders at
//! physical (HiDPI) pixels so glyphs stay crisp on Retina.

use glyphon::{
    fontdb, Attrs, Buffer, Color, Family, Metrics, Resolution, Shaping, Style, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport, Weight,
};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, TermMode, Term};

use crate::gpu::{self, FORMAT};
use crate::surface::{IOSurfaceRef, SharedSurface};
use crate::palette::{self, Theme};
use crate::quads::{Quad, QuadRenderer};
use crate::term::EventProxy;
use std::ffi::c_void;
use unicode_width::UnicodeWidthChar;

/// Padding inside the window edge, in points (scaled to physical px).
const PAD_PT: f32 = 2.0;
/// Default monospace size in points.
const DEFAULT_FONT_PT: f32 = 13.0;

/// A single cell resolved for drawing.
#[derive(Clone, Copy)]
struct CellVis {
    ch: char,
    fg: [u8; 3],
    bg: [u8; 3],
    bold: bool,
    italic: bool,
    /// A double-width (CJK/emoji) lead cell — occupies two columns.
    wide: bool,
    /// The trailing half of a wide glyph — drawn by its lead cell.
    spacer: bool,
    /// Whether `bg` differs from the default and needs a fill quad.
    bg_fill: bool,
}

pub struct Renderer {
    width: u32,
    height: u32,
    /// The render target. Single-buffered and synchronous on both platforms; the
    /// renderer drives it through `begin_frame`/`view`/`finish_frame`/`present`.
    shared: SharedSurface,

    /// Cursor rect in physical px from the last render (x, y, w, h), if shown.
    /// Exposed so the host can place the IME composition/candidate window.
    cursor_px: Option<[f32; 4]>,

    scale: f32,
    font_pt: f32,
    /// Primary monospace family (loaded font name); None = generic monospace.
    font_family: Option<String>,
    /// Cached cell metrics in physical px (recomputed on font/scale change).
    cell_w: f32,
    cell_h: f32,
    metrics_dirty: bool,

    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    quads: QuadRenderer,
}

impl Renderer {
    pub fn new(width: u32, height: u32) -> Self {
        let g = gpu::gpu();
        // Clamp to the device's max texture size so an oversized window can't fail
        // target creation (which used to panic the whole terminal create — restored
        // windows are built straight at their saved size, so a wide one died there).
        let max = g.device.limits().max_texture_dimension_2d;
        let width = width.clamp(1, max);
        let height = height.clamp(1, max);
        let shared = crate::surface::create_shared_target(&g.device, width, height, FORMAT);
        let viewport = Viewport::new(&g.device, &g.cache);
        let mut atlas = TextAtlas::new(&g.device, &g.queue, &g.cache, FORMAT);
        let text_renderer =
            TextRenderer::new(&mut atlas, &g.device, wgpu::MultisampleState::default(), None);
        let quads = QuadRenderer::new(&g.device, FORMAT);

        Renderer {
            width,
            height,
            shared,
            cursor_px: None,
            scale: 1.0,
            font_pt: DEFAULT_FONT_PT,
            font_family: None,
            cell_w: 8.0,
            cell_h: 16.0,
            metrics_dirty: true,
            viewport,
            atlas,
            text_renderer,
            quads,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn set_scale(&mut self, scale: f32) {
        let s = scale.max(0.5);
        if s != self.scale {
            self.scale = s;
            self.metrics_dirty = true;
        }
    }

    pub fn set_font_size(&mut self, points: f32) {
        let p = points.clamp(6.0, 64.0);
        if p != self.font_pt {
            self.font_pt = p;
            self.metrics_dirty = true;
        }
    }

    /// Load a font file and make its best monospace family the primary one.
    pub fn set_font(&mut self, path: &str) {
        let mut fs = gpu::font_system().lock().unwrap();
        let before = fs.db().faces().count();
        if let Err(e) = fs.db_mut().load_font_file(path) {
            log::warn!("unterm: failed to load font {path}: {e}");
            return;
        }
        // Among the file's faces, pick the best family to address by name:
        // prefer monospaced, upright, weight closest to Regular (400). Skip
        // private ('.'-prefixed) family names — they don't resolve by name and
        // cause a silent fall back to a proportional default.
        let chosen = fs
            .db()
            .faces()
            .skip(before)
            .filter_map(|f| {
                let (name, _) = f.families.first()?;
                if name.starts_with('.') {
                    return None;
                }
                Some((
                    (!f.monospaced) as i32,
                    (f.style != fontdb::Style::Normal) as i32,
                    (f.weight.0 as i32 - 400).abs(),
                    name.clone(),
                ))
            })
            .min()
            .map(|(_, _, _, name)| name);
        match chosen {
            Some(name) => self.font_family = Some(name),
            None => log::warn!("unterm: no addressable monospace family in {path}"),
        }
        self.metrics_dirty = true;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let g = gpu::gpu();
        let max = g.device.limits().max_texture_dimension_2d;
        let width = width.clamp(1, max);
        let height = height.clamp(1, max);
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.shared = crate::surface::create_shared_target(&g.device, width, height, FORMAT);
    }

    /// Map a physical-pixel coordinate to the (line, column) viewport cell it
    /// falls on plus whether it's on the cell's left half. Clamped to the grid,
    /// so points outside the padding/edges snap to the nearest cell.
    pub fn px_to_cell(&mut self, x: f32, y: f32, cols: usize, rows: usize) -> (usize, usize, bool) {
        self.ensure_metrics();
        let pad = PAD_PT * self.scale;
        let fx = (x - pad) / self.cell_w;
        let fy = (y - pad) / self.cell_h;
        let col = (fx.floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let line = (fy.floor().max(0.0) as usize).min(rows.saturating_sub(1));
        // Side from the clamped column so points past the row end read as the
        // right half (extend to end) and points left of the grid as the left.
        let left = (fx - col as f32) < 0.5;
        (line, col, left)
    }

    /// The current cell grid size derived from the pixel size and metrics.
    pub fn cell_grid_size(&mut self) -> (usize, usize) {
        self.ensure_metrics();
        let pad = PAD_PT * self.scale;
        let avail_w = (self.width as f32 - pad * 2.0).max(self.cell_w);
        let avail_h = (self.height as f32 - pad * 2.0).max(self.cell_h);
        let cols = (avail_w / self.cell_w).floor().max(1.0) as usize;
        let rows = (avail_h / self.cell_h).floor().max(1.0) as usize;
        (cols, rows)
    }

    fn font_px(&self) -> f32 {
        (self.font_pt * self.scale).max(4.0)
    }

    fn line_height(&self) -> f32 {
        (self.font_px() * 1.25).round().max(1.0)
    }

    fn family(&self) -> Family<'_> {
        match self.font_family.as_deref() {
            Some(name) => Family::Name(name),
            None => Family::Monospace,
        }
    }

    /// Recompute cell width/height from the font by measuring an 'M' advance.
    fn ensure_metrics(&mut self) {
        if !self.metrics_dirty {
            return;
        }
        let font_px = self.font_px();
        let line_h = self.line_height();
        let family = self.family();
        let mut fs = gpu::font_system().lock().unwrap();
        let mut buf = Buffer::new(&mut fs, Metrics::new(font_px, line_h));
        buf.set_size(&mut fs, None, None);
        let sample = "MMMMMMMMMMMMMMMMMMMM"; // 20 cells, averaged for stability
        buf.set_text(
            &mut fs,
            sample,
            &Attrs::new().family(family),
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut fs, false);
        let line_w = buf
            .layout_runs()
            .next()
            .map(|r| r.line_w)
            .unwrap_or(font_px * 0.6 * sample.len() as f32);
        drop(fs);
        self.cell_w = (line_w / sample.len() as f32).max(1.0);
        self.cell_h = line_h;
        self.metrics_dirty = false;
    }

    pub fn iosurface(&self) -> IOSurfaceRef {
        self.shared.surface()
    }

    pub fn raw_texture(&self) -> *mut c_void {
        self.shared.raw_texture()
    }

    /// Idle-tick hook for a swapchain to promote a finished frame; always false on
    /// the single-buffered targets (kept for the host's polling interface).
    pub fn advance(&mut self) -> bool {
        self.shared.advance()
    }

    /// Cursor rect (x, y, w, h) in physical px from the last render, if shown.
    pub fn cursor_px(&self) -> Option<[f32; 4]> {
        self.cursor_px
    }

    /// Render `term`'s visible grid into the IOSurface target. `preedit` is the
    /// in-progress IME composition drawn as an underlined overlay at the cursor
    /// (empty = nothing to draw).
    pub fn render(&mut self, term: &Term<EventProxy>, theme: &Theme, focused: bool, preedit: &str) {
        self.ensure_metrics();
        // No-op on the single-buffered targets; kept for the surface interface.
        self.shared.begin_frame();

        let grid = term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let display_offset = grid.display_offset();
        let show_cursor = term.mode().contains(TermMode::SHOW_CURSOR) && display_offset == 0;

        let pad = PAD_PT * self.scale;
        let cell_w = self.cell_w;
        let cell_h = self.cell_h;

        // --- Resolve every visible cell. ---
        let blank = CellVis {
            ch: ' ',
            fg: theme.fg,
            bg: theme.bg,
            bold: false,
            italic: false,
            wide: false,
            spacer: false,
            bg_fill: false,
        };
        // Active selection range (grid coords), used to tint selected cells.
        let selection = term.selection.as_ref().and_then(|s| s.to_range(term));

        let mut cells = vec![blank; cols * rows];
        for indexed in grid.display_iter() {
            let Some(vp) = point_to_viewport(display_offset, indexed.point) else {
                continue;
            };
            if vp.line >= rows || vp.column.0 >= cols {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            let mut fg = palette::resolve(cell.fg, theme);
            let mut bg = palette::resolve(cell.bg, theme);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) {
                fg = [fg[0] / 2 + fg[0] / 4, fg[1] / 2 + fg[1] / 4, fg[2] / 2 + fg[2] / 4];
            }
            // Selected cells take the highlight background (text color is kept).
            if selection.map_or(false, |r| r.contains(indexed.point)) {
                bg = theme.selection;
            }
            let hidden = flags.contains(Flags::HIDDEN);
            let spacer = flags.contains(Flags::WIDE_CHAR_SPACER);
            cells[vp.line * cols + vp.column.0] = CellVis {
                ch: if hidden { ' ' } else { cell.c },
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                wide: flags.contains(Flags::WIDE_CHAR),
                spacer,
                bg_fill: bg != theme.bg,
            };
        }

        // --- Cursor. ---
        let cursor_vp = point_to_viewport(display_offset, grid.cursor.point)
            .filter(|p| p.line < rows && p.column.0 < cols);
        if show_cursor && focused {
            if let Some(p) = cursor_vp {
                // Invert the glyph under a focused block cursor for contrast.
                let idx = p.line * cols + p.column.0;
                cells[idx].fg = theme.bg;
                cells[idx].bold = false;
            }
        }

        // --- Background + cursor quads (drawn under the text). ---
        let mut quads: Vec<Quad> = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                let cv = cells[row * cols + col];
                if cv.bg_fill {
                    quads.push(Quad {
                        x: pad + col as f32 * cell_w,
                        y: pad + row as f32 * cell_h,
                        w: cell_w,
                        h: cell_h,
                        color: linear(cv.bg, 1.0),
                        radius: 0.0,
                    });
                }
            }
        }
        // The cursor spans two columns when it sits on a wide (CJK) glyph.
        let cursor_cells = |p: &alacritty_terminal::index::Point<usize>| -> f32 {
            if cells[p.line * cols + p.column.0].wide { cell_w * 2.0 } else { cell_w }
        };
        if show_cursor {
            if let Some(p) = cursor_vp {
                let x = pad + p.column.0 as f32 * cell_w;
                let y = pad + p.line as f32 * cell_h;
                let cw = cursor_cells(&p);
                let col = linear(theme.cursor, 1.0);
                if focused {
                    quads.push(Quad { x, y, w: cw, h: cell_h, color: col, radius: 0.0 });
                } else {
                    // Hollow outline when the window isn't focused.
                    let t = (1.0 * self.scale).max(1.0);
                    quads.push(Quad { x, y, w: cw, h: t, color: col, radius: 0.0 });
                    quads.push(Quad { x, y: y + cell_h - t, w: cw, h: t, color: col, radius: 0.0 });
                    quads.push(Quad { x, y, w: t, h: cell_h, color: col, radius: 0.0 });
                    quads.push(Quad { x: x + cw - t, y, w: t, h: cell_h, color: col, radius: 0.0 });
                }
            }
        }

        // Remember the cursor rect (physical px) for the host's IME placement.
        self.cursor_px = if show_cursor {
            cursor_vp
                .map(|p| [pad + p.column.0 as f32 * cell_w, pad + p.line as f32 * cell_h, cursor_cells(&p), cell_h])
        } else {
            None
        };

        // --- One shaped buffer per non-empty row. ---
        let font_px = self.font_px();
        let line_h = self.line_height();
        let font_family = self.font_family.clone();
        let family = match font_family.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::Monospace,
        };

        let attrs_of = |fg: [u8; 3], bold: bool, italic: bool| {
            Attrs::new()
                .family(family)
                .color(Color::rgb(fg[0], fg[1], fg[2]))
                .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
                .style(if italic { Style::Italic } else { Style::Normal })
        };

        // (buffer, left_px, top_px). Segments are anchored at their starting
        // column rather than continuously shaped, so a wide (CJK) glyph whose
        // advance differs from the cell width can't shift the rest of the row.
        let mut row_buffers: Vec<(Buffer, f32, f32)> = Vec::new();
        {
            let mut fs = gpu::font_system().lock().unwrap();
            for row in 0..rows {
                let base = row * cols;
                let top = pad + row as f32 * cell_h;
                let mut col = 0usize;
                while col < cols {
                    let cv = cells[base + col];
                    if cv.spacer {
                        col += 1;
                        continue;
                    }
                    // A double-width glyph: its own buffer, anchored at its column.
                    if cv.wide {
                        if cv.ch != ' ' {
                            let mut buf = Buffer::new(&mut fs, Metrics::new(font_px, line_h));
                            buf.set_size(&mut fs, None, Some(line_h));
                            buf.set_text(
                                &mut fs,
                                cv.ch.encode_utf8(&mut [0u8; 4]),
                                &attrs_of(cv.fg, cv.bold, cv.italic),
                                Shaping::Advanced,
                                None,
                            );
                            buf.shape_until_scroll(&mut fs, false);
                            row_buffers.push((buf, pad + col as f32 * cell_w, top));
                        }
                        col += 1; // the trailing spacer cell is skipped above
                        continue;
                    }
                    // A maximal run of single-width cells, anchored at its start.
                    let seg_start = col;
                    let mut text = String::new();
                    let mut runs: Vec<(usize, usize, [u8; 3], bool, bool)> = Vec::new();
                    let mut cur: Option<(usize, [u8; 3], bool, bool)> = None;
                    while col < cols {
                        let cv = cells[base + col];
                        if cv.spacer || cv.wide {
                            break;
                        }
                        let key = (cv.fg, cv.bold, cv.italic);
                        match cur {
                            Some((_, fg, b, i)) if (fg, b, i) == key => {}
                            _ => {
                                if let Some((s, fg, b, i)) = cur.take() {
                                    runs.push((s, text.len(), fg, b, i));
                                }
                                cur = Some((text.len(), key.0, key.1, key.2));
                            }
                        }
                        text.push(cv.ch);
                        col += 1;
                    }
                    if let Some((s, fg, b, i)) = cur.take() {
                        runs.push((s, text.len(), fg, b, i));
                    }
                    // Trim trailing blanks (no ink) to keep the atlas small.
                    let keep = text.trim_end_matches(' ').len();
                    if keep == 0 {
                        continue;
                    }
                    let mut buf = Buffer::new(&mut fs, Metrics::new(font_px, line_h));
                    buf.set_size(&mut fs, None, Some(line_h));
                    let spans = runs.iter().filter_map(|&(s, e, fg, bold, italic)| {
                        let e = e.min(keep);
                        if s >= e {
                            return None;
                        }
                        Some((&text[s..e], attrs_of(fg, bold, italic)))
                    });
                    buf.set_rich_text(&mut fs, spans, &Attrs::new().family(family), Shaping::Advanced, None);
                    buf.shape_until_scroll(&mut fs, false);
                    row_buffers.push((buf, pad + seg_start as f32 * cell_w, top));
                }
            }

            // --- IME preedit overlay: the in-progress composition at the cursor. ---
            // Laid out cell-by-cell like typed text, starting at the cursor and
            // wrapping to the next row at the right edge so it never runs off the
            // side. Each visual segment gets an opaque background (so the grid
            // underneath doesn't show through), an underline marking it composing,
            // and the glyphs in the terminal font; a thin caret follows the end.
            if !preedit.is_empty() {
                if let Some(cur) = cursor_vp {
                    // Break the composition into per-row segments by display column.
                    let mut line = cur.line;
                    let mut col = cur.column.0;
                    let mut seg_start = col;
                    let mut seg = String::new();
                    // (line, start_col, text, width_in_cols)
                    let mut segments: Vec<(usize, usize, String, usize)> = Vec::new();
                    for ch in preedit.chars() {
                        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if col + w.max(1) > cols && col > 0 {
                            segments.push((line, seg_start, std::mem::take(&mut seg), col - seg_start));
                            line += 1;
                            col = 0;
                            seg_start = 0;
                        }
                        seg.push(ch);
                        col += w;
                    }
                    segments.push((line, seg_start, seg, col.saturating_sub(seg_start)));

                    let ut = (1.5 * self.scale).max(1.0);
                    for (ln, sc, text, wc) in &segments {
                        if *wc == 0 {
                            continue;
                        }
                        let y = pad + *ln as f32 * cell_h;
                        // Opaque background + underline over the whole segment.
                        let bx = pad + *sc as f32 * cell_w;
                        let bw = *wc as f32 * cell_w;
                        quads.push(Quad { x: bx, y, w: bw, h: cell_h, color: linear(theme.bg, 1.0), radius: 0.0 });
                        quads.push(Quad { x: bx, y: y + cell_h - ut, w: bw, h: ut, color: linear(theme.fg, 1.0), radius: 0.0 });
                        // Glyphs are placed exactly like the grid: narrow runs are
                        // shaped together and anchored at their start column, while a
                        // wide (CJK) glyph gets its own buffer anchored at its column
                        // (its font advance differs from 2 cells, so shaping it inside
                        // a run would drift the rest of the segment — the cause of the
                        // pre-/post-commit width mismatch).
                        let chs: Vec<char> = text.chars().collect();
                        let mut c = *sc;
                        let mut i = 0;
                        while i < chs.len() {
                            let cw = UnicodeWidthChar::width(chs[i]).unwrap_or(0);
                            if cw >= 2 {
                                let mut buf = Buffer::new(&mut fs, Metrics::new(font_px, line_h));
                                buf.set_size(&mut fs, None, Some(line_h));
                                buf.set_text(
                                    &mut fs,
                                    chs[i].encode_utf8(&mut [0u8; 4]),
                                    &attrs_of(theme.fg, false, false),
                                    Shaping::Advanced,
                                    None,
                                );
                                buf.shape_until_scroll(&mut fs, false);
                                row_buffers.push((buf, pad + c as f32 * cell_w, y));
                                c += cw;
                                i += 1;
                            } else {
                                let run_col = c;
                                let mut run = String::new();
                                while i < chs.len() {
                                    let w2 = UnicodeWidthChar::width(chs[i]).unwrap_or(0);
                                    if w2 >= 2 {
                                        break;
                                    }
                                    run.push(chs[i]);
                                    c += w2;
                                    i += 1;
                                }
                                if !run.is_empty() {
                                    let mut buf = Buffer::new(&mut fs, Metrics::new(font_px, line_h));
                                    buf.set_size(&mut fs, None, Some(line_h));
                                    buf.set_text(&mut fs, &run, &attrs_of(theme.fg, false, false), Shaping::Advanced, None);
                                    buf.shape_until_scroll(&mut fs, false);
                                    row_buffers.push((buf, pad + run_col as f32 * cell_w, y));
                                }
                            }
                        }
                    }
                    // Caret at the composition's end (wrapped to the next row if it
                    // lands exactly on the right edge).
                    if col >= cols {
                        line += 1;
                        col = 0;
                    }
                    let ct = (1.0 * self.scale).max(1.0);
                    quads.push(Quad {
                        x: pad + col as f32 * cell_w,
                        y: pad + line as f32 * cell_h,
                        w: ct,
                        h: cell_h,
                        color: linear(theme.cursor, 1.0),
                        radius: 0.0,
                    });
                }
            }

            // --- Prepare GPU passes. ---
            let bounds = TextBounds {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            let default_color = Color::rgb(theme.fg[0], theme.fg[1], theme.fg[2]);
            let areas: Vec<TextArea> = row_buffers
                .iter()
                .map(|(buf, left, top)| TextArea {
                    buffer: buf,
                    left: *left,
                    top: *top,
                    scale: 1.0,
                    bounds,
                    default_color,
                    custom_glyphs: &[],
                })
                .collect();

            let g = gpu::gpu();
            self.viewport.update(
                &g.queue,
                Resolution {
                    width: self.width,
                    height: self.height,
                },
            );
            self.quads
                .prepare(&g.device, &g.queue, (self.width as f32, self.height as f32), &quads);
            self.text_renderer
                .prepare(
                    &g.device,
                    &g.queue,
                    &mut fs,
                    &mut self.atlas,
                    &self.viewport,
                    areas,
                    &mut glyphon::SwashCache::new(),
                )
                .expect("unterm: glyphon prepare failed");
        }

        // --- Encode + submit. ---
        let g = gpu::gpu();
        let clear = {
            let c = linear(theme.bg, 1.0);
            wgpu::Color { r: c[0] as f64, g: c[1] as f64, b: c[2] as f64, a: 1.0 }
        };
        let mut encoder = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("unterm-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("unterm-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.shared.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
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
                .expect("unterm: glyphon render failed");
        }
        // Blit the freshly rendered frame into the surface's presented texture.
        // No-op on macOS (the IOSurface is the render target); on Windows it
        // copies into the shared D3D texture so Unity sees a clean full frame.
        self.shared.finish_frame(&mut encoder);
        g.queue.submit([encoder.finish()]);
        // Block until the GPU finishes this frame so Unity samples a complete
        // texture (synchronous on both platforms).
        self.shared.present();
        self.atlas.trim();
    }
}

/// Convert an sRGB byte color to linear floats for the sRGB-encoding target.
fn linear(c: [u8; 3], a: f32) -> [f32; 4] {
    fn ch(b: u8) -> f32 {
        let s = b as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    [ch(c[0]), ch(c[1]), ch(c[2]), a]
}
