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

/// Inner padding (logical px, scaled) around the text.
const PAD: f32 = 6.0;

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

    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    quads: QuadRenderer,
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

        let editor = {
            let mut fs = gpu::font_system().lock().unwrap();
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
            content_h: 0.0,
            caret: [0.0; 4],
            scroll_v: 0.0,
            preedit: String::new(),
            preedit_anchor: None,
            button: 0,
            button_rect: [0.0; 4],
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            quads,
        }
    }

    pub fn set_clear_color(&mut self, r: f64, g: f64, b: f64, a: f64) {
        self.clear = wgpu::Color { r, g, b, a };
    }

    pub fn set_text_color(&mut self, r: u8, g: u8, b: u8, a: u8) {
        self.text_color = Color::rgba(r, g, b, a);
    }

    pub fn set_font(&mut self, path: &str) {
        let mut fs = gpu::font_system().lock().unwrap();
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
        let mut fs = gpu::font_system().lock().unwrap();
        let family = match family_name.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::SansSerif,
        };
        let attrs = Attrs::new().family(family).color(color);
        self.editor
            .with_buffer_mut(|b| b.set_text(&mut fs, text, attrs, Shaping::Advanced));
        self.editor.action(&mut fs, Action::Motion(Motion::BufferEnd));
        self.undo.clear(); // programmatic reset isn't an undoable edit
        self.redo.clear();
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Insert typed or IME-committed text at the caret. Shaping happens on the
    /// next `render` via `shape_as_needed`, so no font system is needed here.
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // A real insert (e.g. IME commit) replaces any in-progress composition.
        self.clear_preedit();
        self.editor.start_change();
        self.editor.insert_string(text, None);
        if let Some(c) = self.editor.finish_change() {
            self.undo.push(c);
            self.redo.clear();
        }
    }

    /// Undo the most recent edit (moves it to the redo stack).
    pub fn undo(&mut self) {
        if let Some(mut change) = self.undo.pop() {
            change.reverse();
            self.editor.apply_change(&change);
            self.redo.push(change);
        }
    }

    /// Redo the most recently undone edit.
    pub fn redo(&mut self) {
        if let Some(mut change) = self.redo.pop() {
            change.reverse();
            self.editor.apply_change(&change);
            self.undo.push(change);
        }
    }

    /// Apply a named editing key. The host decides Enter-to-send; `shift` with a
    /// motion key extends the selection (otherwise the motion clears it).
    pub fn key(&mut self, name: &str, _ctrl: bool, _alt: bool, shift: bool) {
        let mut fs = gpu::font_system().lock().unwrap();
        let motion = match name {
            "LeftArrow" => Some(Motion::Left),
            "RightArrow" => Some(Motion::Right),
            "UpArrow" => Some(Motion::Up),
            "DownArrow" => Some(Motion::Down),
            "Home" => Some(Motion::Home),
            "End" => Some(Motion::End),
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
                self.undo.push(c);
                self.redo.clear();
            }
        }
    }

    /// Mouse interaction at physical px relative to the box: kind 0 = click (place
    /// caret), 1 = drag (extend selection), 2 = double-click (word), 3 = triple
    /// (line).
    pub fn mouse(&mut self, x: f32, y: f32, kind: u8) {
        let pad = PAD * self.scale;
        let bx = (x - pad).round() as i32;
        let by = (y - pad).round() as i32;
        let mut fs = gpu::font_system().lock().unwrap();
        let action = match kind {
            1 => Action::Drag { x: bx, y: by },
            2 => Action::DoubleClick { x: bx, y: by },
            3 => Action::TripleClick { x: bx, y: by },
            _ => Action::Click { x: bx, y: by },
        };
        self.editor.action(&mut fs, action);
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
                self.undo.push(c);
                self.redo.clear();
            }
        }
        s
    }

    pub fn select_all(&mut self) {
        let mut fs = gpu::font_system().lock().unwrap();
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
        if text.is_empty() {
            return;
        }
        self.preedit_anchor = Some(self.editor.cursor());
        self.preedit = text.to_string();
        self.editor.insert_string(text, None);
    }

    /// Remove the current preedit (if any), restoring the caret to its anchor.
    fn clear_preedit(&mut self) {
        if self.preedit.is_empty() {
            return;
        }
        if let Some(anchor) = self.preedit_anchor.take() {
            self.editor.set_selection(Selection::Normal(anchor));
            self.editor.delete_selection();
        }
        self.preedit.clear();
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
        let inner_w = (width - pad * 2.0 - reserve).max(1.0);

        let family_name = self.font_family.clone();
        let color = self.text_color;
        let mut guard = gpu::font_system().lock().unwrap();
        let fs = &mut *guard;
        let family = match family_name.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::SansSerif,
        };
        let attrs = Attrs::new().family(family).color(color);

        let inner_h = (height - pad * 2.0).max(line_height);
        self.editor.with_buffer_mut(|b| {
            b.set_metrics(fs, Metrics::new(font_size, line_height));
            b.set_wrap(fs, Wrap::WordOrGlyph);
            // Re-apply attrs (color/family) to every line so theme changes take.
            for line in b.lines.iter_mut() {
                line.set_attrs_list(glyphon::AttrsList::new(attrs));
            }
        });

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
        let caret_y = self.editor.cursor_position().map(|(_, cy)| cy as f32).unwrap_or(0.0);

        // Keep the kept scroll offset, nudging it only when the caret would leave
        // the box: scroll up only once the caret reaches the top visible line, and
        // down only once it passes the bottom. (We set the scroll ourselves because
        // cosmic-text 0.12's shape_until_cursor doesn't scroll within one wrapped
        // line.)
        let max_scroll = (full_h - inner_h).max(0.0);
        if caret_y < self.scroll_v {
            self.scroll_v = caret_y;
        } else if caret_y + line_height > self.scroll_v + inner_h {
            self.scroll_v = caret_y + line_height - inner_h;
        }
        self.scroll_v = self.scroll_v.clamp(0.0, max_scroll);

        // Pass 2: bound to the box at the kept scroll offset.
        self.editor.with_buffer_mut(|b| {
            b.set_size(fs, Some(inner_w), Some(inner_h));
            b.set_scroll(Scroll { line: 0, vertical: self.scroll_v, horizontal: 0.0 });
        });
        self.editor.shape_as_needed(fs, false);

        let mut quads: Vec<Quad> = Vec::new();
        // Selection highlight (behind the text).
        if let Some((c1, c2)) = self.editor.selection_bounds() {
            self.editor.with_buffer(|buf| {
                for run in buf.layout_runs() {
                    if let Some((lx, w)) = run.highlight(c1, c2) {
                        quads.push(Quad {
                            x: pad + lx,
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
                                x: pad + lx,
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

        // Caret quad from the editor's cursor position (buffer-relative px).
        if let Some((cx, cy)) = self.editor.cursor_position() {
            let x = pad + cx as f32;
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
                Attrs::new().family(Family::SansSerif).color(tc),
                Shaping::Advanced,
            );
            b.shape_until_scroll(fs, false);
            let icon_w = b.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);
            icon_left = btn_x + ((bw - icon_w) / 2.0).max(0.0);
            icon_top = btn_y + ((bw - isize) / 2.0).max(0.0);
            icon_buf = Some(b);
        }

        let bounds = TextBounds {
            left: 0,
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
            editor.with_buffer(|buf| {
                let mut areas: Vec<TextArea> = Vec::with_capacity(2);
                areas.push(TextArea {
                    buffer: buf,
                    left: pad,
                    top: pad,
                    scale: 1.0,
                    bounds,
                    default_color: text_color,
                    custom_glyphs: &[],
                });
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
                text_renderer
                    .prepare(&g.device, &g.queue, fs, atlas, viewport, areas, swash_cache)
                    .expect("unterm: input glyphon prepare failed");
            });
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
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.quads.render(&mut pass);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .expect("unterm: input glyphon render failed");
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
