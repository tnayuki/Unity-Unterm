//! The native session browser: the "All Sessions" list drawn in place of the
//! transcript panel. Fully Rust-rendered (same glyphon + quad primitives as the
//! panel/input box) so the list matches the app's typography instead of IMGUI's.
//!
//! It owns no data source of its own: results come from the async [`sessions`]
//! worker (kicked by the query text the host types into the composer-turned-
//! search-box), and archiving goes straight to [`sessions::set_archived`]. Row
//! activation is reported to the [`AgentView`](crate::agentview::AgentView),
//! which hands it to the host as a host command.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};

use crate::gpu::{self, FORMAT};
use crate::quads::{MeshRenderer, MeshVertex, Quad, QuadRenderer};
use crate::sessions::{self, Session};
use crate::surface::{self, SharedSurface};

/// What a mouse-down on the list resolved to.
pub enum Click {
    None,
    /// Open (resume) this session: (id, title).
    Resume(String, String),
    /// The archive icon was toggled (handled internally; just re-render).
    Toggled,
}

/// One laid-out row from the last render, for hit-testing (physical px).
struct RowRect {
    index: usize, // into the *filtered* rows of the last layout
    y: f32,
    h: f32,
    icon: [f32; 4],
}

pub struct BrowserView {
    width: u32,
    height: u32,
    shared: SharedSurface,
    scale: f32,
    clear: wgpu::Color,
    text_color: Color,
    font_family: Option<String>,

    cwd: String,
    /// The query the in-flight/last request was issued for.
    sent_query: Option<String>,
    last_gen: u64, // sessions-dir generation the current list reflects
    serial: u64, // in-flight sessions request (0 = idle)
    rows: Arc<Vec<Session>>,
    loading: bool,
    show_archived: bool,
    /// The session currently open in this window (accent highlight).
    current_id: String,
    /// Session ids driven by a live `claude` process (this window's, another
    /// Unterm window's, or an external CLI) — read from Claude Code's own session
    /// registry so two processes can't resume the same conversation. Refreshed
    /// (throttled) in [`Self::poll`]; excludes the current window via `current_id`.
    busy: HashSet<String>,
    busy_read: u64, // unix secs of the last registry read (throttle)
    /// Hovered row index into the last layout's filtered rows, + whether the
    /// pointer is over its archive icon.
    hover: Option<(usize, bool)>,
    scroll: f32, // top-anchored, physical px
    content_h: f32,
    /// Row hit rects from the last render.
    row_rects: Vec<RowRect>,
    /// The filtered (visible) rows of the last layout, as indices into `rows`.
    visible: Vec<usize>,
    dirty: bool,

    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    quads: QuadRenderer,
    mesh: MeshRenderer, // diagonals the axis-aligned quads can't do (the archive icon)
}

impl BrowserView {
    pub fn new(width: u32, height: u32, cwd: String) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let g = gpu::gpu();
        let shared = surface::create_shared_target(&g.device, width, height, FORMAT);
        let swash_cache = SwashCache::new();
        let viewport = Viewport::new(&g.device, &g.cache);
        let mut atlas = TextAtlas::new(&g.device, &g.queue, &g.cache, FORMAT);
        let text_renderer =
            TextRenderer::new(&mut atlas, &g.device, wgpu::MultisampleState::default(), None);
        let quads = QuadRenderer::new(&g.device, FORMAT);
        let mesh = MeshRenderer::new(&g.device, FORMAT);
        Self {
            width,
            height,
            shared,
            scale: 1.0,
            clear: wgpu::Color { r: 0.05, g: 0.05, b: 0.05, a: 1.0 },
            text_color: Color::rgb(210, 210, 214),
            font_family: None,
            cwd,
            sent_query: None,
            last_gen: 0,
            serial: 0,
            rows: Arc::new(Vec::new()),
            loading: false,
            show_archived: false,
            current_id: String::new(),
            busy: HashSet::new(),
            busy_read: 0,
            hover: None,
            scroll: 0.0,
            content_h: 0.0,
            row_rects: Vec::new(),
            visible: Vec::new(),
            dirty: true,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            quads,
            mesh,
        }
    }

    // --- Host-mirrored setup (matches the panel) -----------------------------

    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        let g = gpu::gpu();
        self.shared = surface::create_shared_target(&g.device, width, height, FORMAT);
        self.dirty = true;
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale.max(0.5);
            self.dirty = true;
        }
    }

    pub fn set_theme(&mut self, clear: wgpu::Color, text: Color) {
        self.clear = clear;
        self.text_color = text;
        self.dirty = true;
    }

    pub fn set_font(&mut self, family: Option<String>) {
        if family != self.font_family {
            self.font_family = family;
            self.dirty = true;
        }
    }

    pub fn set_current(&mut self, id: &str) {
        if id != self.current_id {
            self.current_id = id.to_string();
            self.dirty = true;
        }
    }

    fn is_busy(&self, id: &str) -> bool {
        id != self.current_id && self.busy.contains(id)
    }

    pub fn set_scroll(&mut self, px: f32) {
        let max = (self.content_h - self.height as f32).max(0.0);
        let px = px.clamp(0.0, max);
        if (px - self.scroll).abs() > 0.25 {
            self.scroll = px;
            self.dirty = true;
        }
    }

    pub fn content_height(&self) -> f32 {
        self.content_h
    }

    pub fn raw_texture(&self) -> *mut c_void {
        self.shared.raw_texture()
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn toggle_show_archived(&mut self) {
        self.show_archived = !self.show_archived;
        self.dirty = true;
    }

    pub fn archived_count(&self) -> usize {
        self.rows.iter().filter(|s| s.archived).count()
    }

    // --- Data (async sessions worker) ----------------------------------------

    /// Keep the listing in sync with the search text: (re)issue the query when it
    /// changes, and drain a finished request. Returns true when the view changed.
    pub fn poll(&mut self, query: &str) -> bool {
        // Refresh the "open in a live process" set from Claude Code's registry at
        // most once a second — cheap (a handful of tiny JSON files) and catches an
        // external `claude --resume` that doesn't touch the transcript on disk.
        let now = crate::clock::now_secs();
        if now != self.busy_read {
            self.busy_read = now;
            let next: HashSet<String> = sessions::open_elsewhere(&self.cwd).into_iter().collect();
            if next != self.busy {
                self.busy = next;
                self.dirty = true;
            }
        }

        // Re-list on a query change OR when a session appears/vanishes on disk
        // (the generation bumps), so the open browser stays live.
        let gen = sessions::generation();
        if self.sent_query.as_deref() != Some(query) || gen != self.last_gen {
            self.last_gen = gen;
            self.sent_query = Some(query.to_string());
            self.serial = sessions::query(&self.cwd, 0, query);
            self.loading = true;
            self.dirty = true;
        }
        if self.serial != 0 {
            if let Some(rows) = sessions::poll_rows(self.serial) {
                self.rows = rows;
                self.serial = 0;
                self.loading = false;
                self.hover = None;
                self.dirty = true;
            }
        }
        self.take_dirty()
    }

    // --- Input ----------------------------------------------------------------

    /// Track the pointer for the hover highlight / icon. Returns true on change.
    pub fn hover(&mut self, x: f32, y: f32) -> bool {
        let mut hit = None;
        for r in &self.row_rects {
            if y >= r.y && y < r.y + r.h && x >= 0.0 && x <= self.width as f32 {
                // A busy (open-elsewhere) row is disabled: no hover, no icon.
                if self.visible.get(r.index).is_some_and(|&i| self.is_busy(&self.rows[i].id)) {
                    break;
                }
                let on_icon = x >= r.icon[0]
                    && x <= r.icon[0] + r.icon[2]
                    && y >= r.icon[1]
                    && y <= r.icon[1] + r.icon[3];
                hit = Some((r.index, on_icon));
                break;
            }
        }
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
            return true;
        }
        false
    }

    /// Resolve a mouse-down: the archive icon toggles (persisted via the sessions
    /// module), anywhere else on a row opens it.
    pub fn click(&mut self, x: f32, y: f32) -> Click {
        self.hover(x, y);
        let Some((index, on_icon)) = self.hover else {
            return Click::None;
        };
        let Some(&row) = self.visible.get(index) else {
            return Click::None;
        };
        let s = &self.rows[row];
        if on_icon {
            sessions::set_archived(&self.cwd, &s.id, !s.archived);
            // Update the local copy in place so the row dims/undims immediately
            // (no need to wait for a re-query round-trip).
            let mut rows = (*self.rows).clone();
            rows[row].archived = !rows[row].archived;
            self.rows = Arc::new(rows);
            self.dirty = true;
            return Click::Toggled;
        }
        Click::Resume(s.id.clone(), s.title.clone())
    }

    /// The topmost openable session (Enter in the search box opens it), skipping
    /// any that are busy in another window.
    pub fn first(&self) -> Option<(String, String)> {
        self.visible
            .iter()
            .map(|&row| &self.rows[row])
            .find(|s| !self.is_busy(&s.id))
            .map(|s| (s.id.clone(), s.title.clone()))
    }

    // --- Render ----------------------------------------------------------------

    pub fn render(&mut self) {
        self.shared.begin_frame();
        let g = gpu::gpu();
        let s = self.scale;
        let width = self.width as f32;
        let height = self.height as f32;
        let now = crate::clock::now_secs();

        let pad_h = 12.0 * s;
        let pad_v = 7.0 * s;
        let title_size = 13.0 * s;
        let meta_size = 11.0 * s;
        let title_lh = 18.0 * s;
        let meta_lh = 15.0 * s;
        let icon_w = 16.0 * s;
        let time_w = 90.0 * s;

        let dark = self.clear.r + self.clear.g + self.clear.b < 1.5;
        let t = self.text_color;
        let dim = Color::rgba(t.r(), t.g(), t.b(), 150);
        let faint = Color::rgba(t.r(), t.g(), t.b(), 90);
        let hover_bg = if dark {
            [1.0, 1.0, 1.0, 0.05]
        } else {
            [0.0, 0.0, 0.0, 0.045]
        };
        let accent_bg = if dark {
            [0.32, 0.52, 0.92, 0.20]
        } else {
            [0.28, 0.5, 0.9, 0.16]
        };
        let sep = if dark {
            [1.0, 1.0, 1.0, 0.06]
        } else {
            [0.0, 0.0, 0.0, 0.08]
        };

        let mut guard = gpu::lock_font_system();
        let fs = &mut *guard;
        let family = match self.font_family.as_deref() {
            Some(n) => Family::Name(n),
            None => Family::SansSerif,
        };

        // Filter + lay out rows top-down.
        self.visible = (0..self.rows.len())
            .filter(|&i| self.show_archived || !self.rows[i].archived)
            .collect();
        let mut quads: Vec<Quad> = Vec::new();
        let mut verts: Vec<MeshVertex> = Vec::new();
        let mut buffers: Vec<(Buffer, f32, f32, Option<[f32; 4]>, Color)> = Vec::new();
        self.row_rects.clear();

        // A single centered status line replaces the list while it's empty.
        if self.visible.is_empty() {
            let label = if self.loading {
                "Loading…"
            } else if self.sent_query.as_deref().unwrap_or("").is_empty() {
                "No sessions."
            } else {
                "No matches."
            };
            let b = shape(fs, label, family, meta_size, meta_lh, dim, width);
            let x = ((width - measure_w(&b)) / 2.0).max(0.0);
            buffers.push((b, x, height * 0.35, None, dim));
            self.content_h = height;
        } else {
            let mut y = -self.scroll;
            for (index, &row) in self.visible.clone().iter().enumerate() {
                let sess = &self.rows[row];
                let has_snippet = !sess.snippet.is_empty();
                let row_h = pad_v * 2.0 + title_lh + if has_snippet { meta_lh } else { 0.0 };
                let icon_rect = [
                    width - pad_h - icon_w,
                    y + pad_v + (title_lh - icon_w * 0.75) / 2.0,
                    icon_w,
                    icon_w * 0.75,
                ];
                self.row_rects.push(RowRect {
                    index,
                    y,
                    h: row_h,
                    icon: icon_rect,
                });

                let on_screen = y + row_h > 0.0 && y < height;
                if on_screen {
                    let is_current = !self.current_id.is_empty() && sess.id == self.current_id;
                    let busy = self.is_busy(&sess.id);
                    let hovered = !busy && self.hover.map(|(i, _)| i) == Some(index);
                    if is_current {
                        quads.push(Quad { x: 0.0, y, w: width, h: row_h, color: accent_bg, radius: 0.0 });
                    } else if hovered {
                        quads.push(Quad { x: 0.0, y, w: width, h: row_h, color: hover_bg, radius: 0.0 });
                    }
                    // Hairline separator.
                    quads.push(Quad { x: 0.0, y: y + row_h - 1.0, w: width, h: 1.0, color: sep, radius: 0.0 });

                    // Title (clipped left of the time slot) + right-aligned time.
                    // Archived rows keep full-strength text — dimming them would
                    // read as "disabled" when they're perfectly usable; the archive
                    // icon marks them instead. A busy row (open elsewhere) IS
                    // disabled, so it's the one thing that greys out.
                    let title_color = if busy { faint } else { t };
                    let meta_color = if busy { faint } else { dim };
                    let title = if sess.title.is_empty() { "(untitled)" } else { &sess.title };
                    let title_w = (width - pad_h * 2.0 - time_w - icon_w - 16.0 * s).max(1.0);
                    let b = shape(fs, title, family, title_size, title_lh, title_color, title_w);
                    let clip = [pad_h, y, title_w, row_h];
                    buffers.push((b, pad_h, y + pad_v, Some(clip), title_color));

                    if sess.updated > 0 {
                        let label = crate::clock::format_relative(sess.updated, now);
                        let b = shape(fs, &label, family, meta_size, title_lh, meta_color, time_w);
                        let tx = width - pad_h - icon_w - 8.0 * s - measure_w(&b);
                        buffers.push((b, tx, y + pad_v + (title_lh - meta_lh) / 2.0, None, meta_color));
                    }

                    if has_snippet {
                        let snippet: String = sess
                            .snippet
                            .chars()
                            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                            .collect();
                        let sw = (width - pad_h * 2.0).max(1.0);
                        let b = shape(fs, &snippet, family, meta_size, meta_lh, faint, sw);
                        let clip = [pad_h, y, sw, row_h];
                        buffers.push((b, pad_h, y + pad_v + title_lh, Some(clip), faint));
                    }

                    // Archive icon (a document box): the archived marker + the
                    // archive action. Archived rows show it always (this is how you
                    // tell they're archived); other rows show it only on hover, as
                    // the "archive" affordance. Not offered on a busy (disabled) row.
                    let show_icon = !busy && (sess.archived || hovered);
                    if show_icon {
                        let on_icon = self.hover == Some((index, true));
                        let alpha = if on_icon {
                            0.95
                        } else if sess.archived {
                            0.75
                        } else {
                            0.55
                        };
                        let c = [t.r() as f32 / 255.0, t.g() as f32 / 255.0, t.b() as f32 / 255.0, alpha];
                        push_archive_icon(&mut quads, &mut verts, icon_rect, c, s);
                    }
                }
                y += row_h;
            }
            self.content_h = y + self.scroll;
        }

        // Prepare + draw.
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        };
        let areas: Vec<TextArea> = buffers
            .iter()
            .map(|(b, x, y, clip, color)| TextArea {
                buffer: b,
                left: *x,
                top: *y,
                scale: 1.0,
                bounds: clip.map_or(bounds, |c| TextBounds {
                    left: c[0] as i32,
                    top: c[1] as i32,
                    right: (c[0] + c[2]) as i32,
                    bottom: (c[1] + c[3]) as i32,
                }),
                default_color: *color,
                custom_glyphs: &[],
            })
            .collect();

        self.viewport.update(
            &g.queue,
            Resolution { width: self.width, height: self.height },
        );
        self.quads.prepare(&g.device, &g.queue, (width, height), &quads);
        self.mesh.prepare(&g.device, &g.queue, (width, height), &verts);
        if let Err(e) = self.text_renderer.prepare(
            &g.device,
            &g.queue,
            fs,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        ) {
            log::error!("unterm: browser glyphon prepare failed: {e}");
            return;
        }

        let mut encoder = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("unterm-browser-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("unterm-browser-pass"),
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
            self.mesh.render(&mut pass);
            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                log::error!("unterm: browser glyphon render failed: {e}");
            }
        }
        self.shared.finish_frame(&mut encoder);
        g.queue.submit([encoder.finish()]);
        self.shared.present();
        self.atlas.trim();
    }
}

/// Shape one single-style text run into a fresh buffer.
fn shape(
    fs: &mut FontSystem,
    text: &str,
    family: Family,
    size: f32,
    line_h: f32,
    color: Color,
    width: f32,
) -> Buffer {
    let mut b = Buffer::new(fs, Metrics::new(size, line_h));
    b.set_size(fs, Some(width.max(1.0)), None);
    b.set_wrap(fs, Wrap::None);
    b.set_text(fs, text, &Attrs::new().family(family).color(color), Shaping::Advanced, None);
    b.shape_until_scroll(fs, false);
    b
}

/// Laid-out width of a shaped buffer (single line).
fn measure_w(b: &Buffer) -> f32 {
    b.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max)
}

/// The archive icon — a lidded box with a handle line, the outlined style icon
/// sets (Lucide / Zed's `archive`) use. Modelled on a 16-unit SVG viewBox scaled
/// to `rect`: straight strokes are rounded-cap quads, and the box's rounded
/// bottom corners are quarter-annulus arcs from the mesh renderer (which the
/// axis-aligned quads can't express).
fn push_archive_icon(
    quads: &mut Vec<Quad>,
    verts: &mut Vec<MeshVertex>,
    rect: [f32; 4],
    color: [f32; 4],
    _s: f32,
) {
    let [x, y, w, h] = rect;
    let ux = w / 16.0;
    let uy = h / 16.0;
    let t = (1.2 * ux).max(1.0); // stroke width (Lucide uses 1.2 on a 16 grid)
    let vx = |v: f32| x + v * ux;
    let vy = |v: f32| y + v * uy;
    let hbar = |q: &mut Vec<Quad>, x0: f32, x1: f32, yc: f32| {
        q.push(Quad { x: vx(x0) - t / 2.0, y: vy(yc) - t / 2.0, w: (x1 - x0) * ux + t, h: t, color, radius: t / 2.0 });
    };
    let vbar = |q: &mut Vec<Quad>, xc: f32, y0: f32, y1: f32| {
        q.push(Quad { x: vx(xc) - t / 2.0, y: vy(y0), w: t, h: (y1 - y0) * uy, color, radius: t / 2.0 });
    };

    // Lid: a hollow rounded rectangle across the top (design: x3..13, y3..5.6).
    hbar(quads, 3.0, 13.0, 3.0);
    hbar(quads, 3.0, 13.0, 5.6);
    vbar(quads, 3.0, 3.0, 5.6);
    vbar(quads, 13.0, 3.0, 5.6);

    // Body: an open-topped box with rounded bottom corners (design: x3.6..12.4,
    // top y5.6 open, bottom y13). Sides + bottom are strokes; the corners are arcs.
    let cr = 1.3; // corner radius (design units)
    vbar(quads, 3.6, 5.6, 13.0 - cr);
    vbar(quads, 12.4, 5.6, 13.0 - cr);
    hbar(quads, 3.6 + cr, 12.4 - cr, 13.0);
    // Bottom-left / bottom-right rounded corners (y is down: 90°=down, 180°=left).
    let r_px = cr * ux;
    push_arc(verts, vx(3.6 + cr), vy(13.0 - cr), r_px, t, 90.0, 180.0, color);
    push_arc(verts, vx(12.4 - cr), vy(13.0 - cr), r_px, t, 0.0, 90.0, color);

    // Handle: a short horizontal stroke centred in the box (design: x6..10, y8.3).
    hbar(quads, 6.0, 10.0, 8.3);
}

/// Append a filled triangle (three vertices, one colour) to the mesh.
fn push_tri(verts: &mut Vec<MeshVertex>, a: [f32; 2], b: [f32; 2], c: [f32; 2], color: [f32; 4]) {
    verts.push(MeshVertex { pos: a, color });
    verts.push(MeshVertex { pos: b, color });
    verts.push(MeshVertex { pos: c, color });
}

/// Append a stroked circular arc (a quarter-annulus of mid-radius `r`, stroke
/// width `thick`) from `a0` to `a1` degrees, centred at (`cx`, `cy`). Angles use
/// the screen's y-down convention: 0°=right, 90°=down, 180°=left.
fn push_arc(verts: &mut Vec<MeshVertex>, cx: f32, cy: f32, r: f32, thick: f32, a0: f32, a1: f32, color: [f32; 4]) {
    let ro = r + thick / 2.0;
    let ri = (r - thick / 2.0).max(0.0);
    let steps = 8;
    let d2r = std::f32::consts::PI / 180.0;
    let pt = |rad: f32, ang: f32| [cx + rad * (ang * d2r).cos(), cy + rad * (ang * d2r).sin()];
    for i in 0..steps {
        let t0 = a0 + (a1 - a0) * (i as f32) / steps as f32;
        let t1 = a0 + (a1 - a0) * (i as f32 + 1.0) / steps as f32;
        let (oi, oo) = (pt(ri, t0), pt(ro, t0));
        let (ni, no) = (pt(ri, t1), pt(ro, t1));
        push_tri(verts, oi, oo, no, color);
        push_tri(verts, oi, no, ni, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the archive icon to a PNG for eyeballing (no Editor). Run with:
    /// `cargo test -p unterm --lib browser::tests::dump -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dump_archive_icon() {
        let g = gpu::gpu();
        // A row of the icon at a few sizes on a dark strip.
        let (w, h) = (200u32, 56u32);
        let tex = g.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("icon-test"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());

        let mut quads = Vec::new();
        let mut verts = Vec::new();
        let col = [0.92, 0.92, 0.93, 1.0];
        // Three sizes: 16, 20, 28 px, spaced out, vertically centered.
        for (i, sz) in [16.0f32, 20.0, 28.0].iter().enumerate() {
            let x = 16.0 + i as f32 * 60.0;
            let y = (h as f32 - sz) / 2.0;
            push_archive_icon(&mut quads, &mut verts, [x, y, *sz, *sz], col, sz / 16.0);
        }
        let mut qr = QuadRenderer::new(&g.device, FORMAT);
        qr.prepare(&g.device, &g.queue, (w as f32, h as f32), &quads);
        let mut mr = MeshRenderer::new(&g.device, FORMAT);
        mr.prepare(&g.device, &g.queue, (w as f32, h as f32), &verts);

        // bytes_per_row must be 256-aligned for the copy.
        let bpr = ((w * 4 + 255) / 256) * 256;
        let buf = g.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("icon-readback"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = g.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("icon-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.14, g: 0.14, b: 0.15, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            qr.render(&mut pass);
            mr.render(&mut pass);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        g.queue.submit([enc.finish()]);

        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = g.device.poll(wgpu::PollType::wait_indefinitely());
        let data = buf.slice(..).get_mapped_range();
        let mut img = image::RgbaImage::new(w, h);
        for row in 0..h {
            for col_ in 0..w {
                let o = (row * bpr + col_ * 4) as usize;
                img.put_pixel(col_, row, image::Rgba([data[o], data[o + 1], data[o + 2], data[o + 3]]));
            }
        }
        let path = std::env::temp_dir().join("unterm-archive-icon.png");
        img.save(&path).unwrap();
        println!("wrote {}", path.display());
    }
}
