//! Standalone debugger window, built with egui.
//!
//! Runs its own winit event loop + wgpu surface in a separate process, so it stays
//! alive and interactive while the Unity editor is frozen at a breakpoint. The SDB
//! session runs on a background thread that owns the socket (never blocked by the
//! managed suspend) and communicates with the egui UI via a shared `UiState` plus a
//! command channel.
//!
//! The UI is themed to match Unity's editor (Inter/RobotoMono fonts, dark palette,
//! 18px rows). Launched by the `unterm-debugger` bin.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Color32, FontId, RichText};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::editorview::EditorView;
use crate::gpu;
use crate::sdb::{self, value, wire};

// ---------------------------------------------------------------------------
// Messages + shared state
// ---------------------------------------------------------------------------

/// A command from the UI thread to the SDB session thread.
#[derive(Debug, Clone)]
enum Cmd {
    Resume,
    StepOver,
    StepInto,
    StepOut,
    /// Toggle a breakpoint at a 1-based source line in the currently-viewed file.
    ToggleBp(i32),
    /// The UI switched the displayed file (so the session reports its breakpoints).
    View(String),
    /// Break into a running program (VM suspend) and inspect the current state.
    Pause,
    /// Inspect a different (managed) thread's stack and variables.
    SelectThread(u32),
    /// Inspect a different call-stack frame (0 = innermost).
    SelectFrame(usize),
    /// Lazily fetch the children (fields or elements) of an expandable value.
    ExpandVar { id: u32, array: bool },
    /// Attach to a different target (the editor, or a discovered player).
    SelectTarget(Target),
}

/// An attachable SDB endpoint: this project's editor, or a discovered Unity player.
#[derive(Clone, Debug)]
struct Target {
    ip: String,
    port: u16,
    label: String,
}

/// Wakeups posted from the SDB thread to the winit loop.
#[derive(Debug, Clone, Copy)]
enum Wake {
    Redraw,
    /// Surface the window (un-minimize + focus), e.g. when a breakpoint is hit.
    Focus,
    /// Tuck the window just behind the editor's window (the editor came forward), so it
    /// rides above other apps without covering the editor or stealing focus.
    Raise,
    /// Close the window (the editor we were attached to went away).
    Quit,
}

/// Input the source pane collected this frame, applied to the embedded editor after
/// the egui pass (the editor lives on `App`, out of reach inside the UI closure).
struct SourcePaneOut {
    /// The pane's rect in egui points (for sizing the editor to it).
    rect: egui::Rect,
    acts: Vec<EditorAct>,
    /// Pointer position (egui points) while hovering the source, for the value tooltip.
    hover: Option<egui::Pos2>,
}

impl Default for SourcePaneOut {
    fn default() -> Self {
        Self { rect: egui::Rect::NOTHING, acts: Vec::new(), hover: None }
    }
}

/// A forwarded editor input; coordinates are physical px relative to the pane.
enum EditorAct {
    Mouse { x: f32, y: f32, kind: u8 },
    Scroll { dx: f32, dy: f32 },
    Key(String, bool, bool, bool),
}

/// Which pane has keyboard focus (drives ↑/↓ navigation). Focus follows the last click.
#[derive(Clone, Copy, PartialEq, Default)]
enum Focus {
    #[default]
    None,
    Tree,
    Source,
}

/// Shared, rendered debugger state (snapshotted by the UI each frame).
#[derive(Default, Clone)]
struct UiState {
    /// An error / special message (shown only when not normally attached).
    status: String,
    /// Whether the SDB connection is established.
    attached: bool,
    /// Runtime/protocol version detail (shown on hover).
    version: String,
    stopped: bool,
    /// Absolute path of the source file the top frame is in.
    cur_file: String,
    /// 1-based current line, or 0 if unknown.
    cur_line: i32,
    /// Managed threads at the current stop (only populated while stopped).
    threads: Vec<ThreadInfo>,
    /// The thread whose stack/variables are shown (SDB thread id).
    cur_thread: u32,
    stack: Vec<String>,
    /// Which call-stack frame's variables are shown (0 = innermost).
    cur_frame: usize,
    locals: Vec<Var>,
    members: Vec<Var>,
    this_label: String,
    /// Lazily-fetched children of expandable values, keyed by object id.
    children: std::collections::HashMap<u32, Vec<Var>>,
    /// Bumps on every new stop / frame switch so the UI can drop stale expand state.
    gen: u64,
    /// 1-based breakpoint lines in the currently-displayed file (for gutter dots).
    bp_lines: Vec<i32>,
    /// All breakpoints across files (display file, 1-based line) for the list panel.
    all_bps: Vec<(String, i32)>,
    log: Vec<String>,
    /// Debuggable Unity players found via multicast discovery (for the target picker).
    players: Vec<sdb::Player>,
    /// Label of the target we're currently attached to (for the target picker).
    cur_target: String,
}

/// A variable row in the inspector. `expand` is set when the value is a reference
/// object or array whose fields/elements can be drilled into on demand.
#[derive(Clone)]
struct Var {
    name: String,
    value: String,
    expand: Option<Expand>,
}

#[derive(Clone, Copy)]
struct Expand {
    id: u32,
    array: bool,
}

/// A managed thread shown in the Threads strip (name + where it is stopped).
#[derive(Clone)]
struct ThreadInfo {
    id: u32,
    name: String,
    location: String,
}

impl UiState {
    fn push_log(&mut self, msg: impl Into<String>) {
        self.log.push(msg.into());
        let n = self.log.len();
        if n > 8 {
            self.log.drain(0..n - 8);
        }
    }
}

/// Entry point. `breakpoints` is the (file, line) set to arm (file may be a basename).
pub fn run(breakpoints: Vec<(String, i32)>) {
    env_logger::try_init().ok();

    // Advertise our PID so the editor launcher can tell this project's debugger is
    // already running (project-scoped single instance) and signal it to come forward.
    write_pid_file();

    let mut builder = EventLoop::<Wake>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // Accessory: no Dock icon / menu-bar ownership (it's a helper for the editor);
        // its window can still become key. It follows the editor forward via the
        // activation observer below, so it needn't be in the Dock / Cmd+Tab.
        builder.with_activation_policy(ActivationPolicy::Accessory);
        builder.with_activate_ignoring_other_apps(true);
    }
    let event_loop = builder.build().expect("create event loop");
    let proxy = event_loop.create_proxy();

    // Follow the editor to the foreground: when it activates, slide this window up to
    // just behind it (above other apps, without covering the editor or taking focus).
    install_activation_observer(proxy.clone());

    let state = Arc::new(Mutex::new(UiState::default()));
    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
    {
        let state = state.clone();
        let proxy = proxy.clone();
        std::thread::spawn(move || session_thread(breakpoints, state, rx, proxy));
    }
    // Discover debuggable Unity players on the network (for the target picker).
    spawn_discovery(state.clone(), proxy.clone());

    let files = scan_cs_files();
    let tree = build_tree(&files);
    let mut app = App {
        state,
        tx,
        gfx: None,
        view_file: String::new(),
        editor: None,
        editor_tex: None,
        editor_file: String::new(),
        editor_dims: (0, 0),
        files,
        tree,
        scroll_to_cur: false,
        goto: None,
        focus: Focus::None,
        expanded: ["Assets".to_string(), "Packages".to_string()]
            .into_iter()
            .collect(),
        tree_sel: 0,
        var_open: std::collections::HashSet::new(),
        last_gen: 0,
        hover_tip: None,
    };
    event_loop.run_app(&mut app).expect("run app");
}

// ---------------------------------------------------------------------------
// egui + winit + wgpu integration
// ---------------------------------------------------------------------------

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

struct App {
    state: Arc<Mutex<UiState>>,
    tx: Sender<Cmd>,
    gfx: Option<Gfx>,
    // Which file the source pane shows (may differ from the stopped file).
    view_file: String,
    // Embedded read-only code editor (shared with the Unity code editor) rendered
    // into a texture and shown in the source pane; None until the window exists.
    editor: Option<EditorView>,
    /// egui handle for the editor's texture, and the file/size it was last set to.
    editor_tex: Option<egui::TextureId>,
    editor_file: String,
    editor_dims: (u32, u32),
    // Project .cs files (absolute path, display name) + the prebuilt folder tree.
    files: Vec<(String, String)>,
    tree: Tree,
    scroll_to_cur: bool,
    /// A line to scroll the source to next frame (e.g. from a breakpoint-list click).
    goto: Option<i32>,
    /// Which pane has keyboard focus (for ↑/↓ navigation).
    focus: Focus,
    /// Expanded folder keys in the file tree, and the selected (cursor) row index.
    expanded: std::collections::HashSet<String>,
    tree_sel: usize,
    /// Object ids the user has expanded in the variables inspector.
    var_open: std::collections::HashSet<u32>,
    /// Last-seen inspector generation; when it changes, `var_open` is dropped.
    last_gen: u64,
    /// Hover-watch tooltip (screen pos + text), computed one frame behind the pointer.
    hover_tip: Option<(egui::Pos2, String)>,
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("Unterm Debugger")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 680.0))
            // Keep the tree / source / inspector columns and the three stacked
            // inspector panes usable — below this they'd collapse into each other.
            .with_min_inner_size(winit::dpi::LogicalSize::new(720.0, 420.0));
        // Windows: a tool window (no taskbar button) — the counterpart to the macOS
        // Accessory policy. It follows the editor forward via the activation hook.
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_skip_taskbar(true);
        }
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        window.focus_window();

        let g = gpu::gpu();
        let size = window.inner_size();
        let surface = g.instance.create_surface(window.clone()).expect("surface");
        let caps = surface.get_capabilities(&g.adapter);
        // egui expects a linear (non-sRGB) framebuffer; prefer one to get correct colors.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&g.device, &config);

        let egui_ctx = egui::Context::default();
        setup_theme(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(&g.device, format, egui_wgpu::RendererOptions::default());

        let gfx = Gfx {
            window,
            surface,
            config,
            egui_ctx,
            egui_state,
            egui_renderer,
        };
        gfx.window.request_redraw(); // paint the first frame
        self.gfx = Some(gfx);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Wake) {
        if matches!(ev, Wake::Quit) {
            el.exit();
            return;
        }
        if let Some(g) = &self.gfx {
            match ev {
                Wake::Focus => {
                    // Bring a hidden/behind/minimized window forward on a breakpoint hit.
                    g.window.set_visible(true);
                    g.window.set_minimized(false);
                    g.window.focus_window();
                }
                Wake::Raise => order_behind_editor(&g.window),
                _ => {}
            }
            g.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else {
            return;
        };
        let resp = gfx.egui_state.on_window_event(&gfx.window, &event);
        if resp.repaint {
            gfx.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                gfx.config.width = size.width.max(1);
                gfx.config.height = size.height.max(1);
                gfx.surface.configure(&gpu::gpu().device, &gfx.config);
                gfx.window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.paint(),
            _ => {}
        }
    }
}

impl App {
    fn paint(&mut self) {
        let snap = self.state.lock().map(|s| s.clone()).unwrap_or_default();

        // `gen` bumps on every stop, call-stack frame select, and thread select — the
        // exact moments the source should jump. Follow the (resolved) current location
        // and drop stale object-expansion state. Keying on `gen` (not the line number)
        // means selecting a frame in another file at the same line still jumps, and a
        // stop at the same line after browsing away still returns to it.
        if snap.gen != self.last_gen {
            self.last_gen = snap.gen;
            self.var_open.clear();
            if snap.stopped {
                self.view_file = self.resolve_local(&snap.cur_file);
                self.scroll_to_cur = true;
            }
        }
        if self.view_file.is_empty() {
            self.view_file = self.resolve_local(&snap.cur_file);
        }

        // What source line to scroll the editor to this frame (a fresh stop, or a
        // breakpoint-list jump).
        let scroll_line = if self.scroll_to_cur && snap.stopped {
            self.scroll_to_cur = false;
            Some(snap.cur_line)
        } else {
            self.goto.take()
        };

        let g = gpu::gpu();
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut new_view: Option<String> = None;
        let mut goto: Option<i32> = None;
        let mut focus = self.focus;
        let mut src_out = SourcePaneOut::default();
        // Taken out (not borrowed) so build_ui can mutate them without conflicting
        // with ui_ctx's immutable borrows of other App fields.
        let mut expanded = std::mem::take(&mut self.expanded);
        let mut tree_sel = self.tree_sel;
        let mut var_open = std::mem::take(&mut self.var_open);

        let gfx = self.gfx.as_mut().unwrap();
        let raw = gfx.egui_state.take_egui_input(&gfx.window);
        let ctx = gfx.egui_ctx.clone();
        let ui_ctx = UiCtx {
            s: &snap,
            tree: &self.tree,
            files: &self.files,
            editor_tex: self.editor_tex,
            // Shown a frame behind the pointer (it needs the editor, updated below).
            hover_tip: self.hover_tip.clone(),
        };
        let full = ctx.run_ui(raw, |ui| {
            build_ui(
                ui,
                &ui_ctx,
                &mut cmds,
                &mut new_view,
                &mut goto,
                &mut focus,
                &mut expanded,
                &mut tree_sel,
                &mut var_open,
                &mut src_out,
            );
        });
        drop(ui_ctx);
        self.focus = focus;
        self.expanded = expanded;
        self.tree_sel = tree_sel;
        self.var_open = var_open;

        for cmd in cmds {
            self.tx.send(cmd).ok();
        }
        if let Some(v) = new_view {
            if v != self.view_file {
                self.view_file = v.clone();
                self.scroll_to_cur = false;
                self.tx.send(Cmd::View(v)).ok();
            }
            // A jump from the breakpoint list also scrolls the source next frame.
            self.goto = goto;
        }

        // Drive the embedded editor (size, content, input, breakpoints, render).
        let needs_reg = self.sync_editor(&snap, &src_out, full.pixels_per_point, scroll_line);
        // Recompute the hover-watch tooltip for next frame (needs the live editor).
        self.hover_tip = self.compute_hover(&snap, &src_out, full.pixels_per_point);

        let gfx = self.gfx.as_mut().unwrap();
        // (Re)bind the editor's texture so the source pane's image samples it.
        if let Some(ed) = &self.editor {
            let view = ed.texture_view();
            match self.editor_tex {
                Some(_) if !needs_reg => {}
                Some(id) => gfx.egui_renderer.update_egui_texture_from_wgpu_texture(
                    &g.device,
                    view,
                    wgpu::FilterMode::Nearest,
                    id,
                ),
                None => {
                    self.editor_tex = Some(gfx.egui_renderer.register_native_texture(
                        &g.device,
                        view,
                        wgpu::FilterMode::Nearest,
                    ))
                }
            }
        }
        gfx.egui_state.handle_platform_output(&gfx.window, full.platform_output);
        let tris = gfx.egui_ctx.tessellate(full.shapes, full.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gfx.config.width, gfx.config.height],
            pixels_per_point: full.pixels_per_point,
        };
        for (id, delta) in &full.textures_delta.set {
            gfx.egui_renderer.update_texture(&g.device, &g.queue, *id, delta);
        }
        let mut encoder = g
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("egui") });
        let user_cmds =
            gfx.egui_renderer.update_buffers(&g.device, &g.queue, &mut encoder, &tris, &screen);

        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            _ => {
                gfx.surface.configure(&g.device, &gfx.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.13, g: 0.13, b: 0.13, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            gfx.egui_renderer.render(&mut pass, &tris, &screen);
        }
        g.queue.submit(user_cmds.into_iter().chain([encoder.finish()]));
        frame.present();
        for id in &full.textures_delta.free {
            gfx.egui_renderer.free_texture(id);
        }
        // Keep animating while egui wants it (panel drag, hover, etc.).
        if gfx.egui_ctx.has_requested_repaint() {
            gfx.window.request_redraw();
        }
    }

    /// Map a source path from the target's debug info to a local project file. Debug
    /// info records the path as compiled — the local editor's absolute path (which
    /// exists as-is), or a remote build machine's path (which does not). We match by
    /// basename, disambiguating same-named files by the longest common path suffix, so
    /// the developer's local checkout of the same project resolves regardless of where
    /// it ran. Falls back to the original path when nothing matches.
    fn resolve_local(&self, dbg_path: &str) -> String {
        if dbg_path.is_empty() {
            return String::new();
        }
        // Fast path: the recorded path exists here (local editor debugging).
        if std::path::Path::new(dbg_path).is_file() {
            return dbg_path.to_string();
        }
        let base = basename(dbg_path);
        let want: Vec<&str> = dbg_path.rsplit(['/', '\\']).collect(); // trailing-first
        let mut best: Option<(usize, &str)> = None;
        for (abs, name) in &self.files {
            if name != base {
                continue;
            }
            let have = abs.rsplit(['/', '\\']);
            let score = want.iter().zip(have).take_while(|(a, b)| *a == b).count();
            if best.map(|(s, _)| score > s).unwrap_or(true) {
                best = Some((score, abs));
            }
        }
        best.map(|(_, abs)| abs.to_string())
            .unwrap_or_else(|| dbg_path.to_string())
    }

    /// Hover-watch: resolve the identifier under the pointer to its current value
    /// (in-scope locals / `this` members). Returns the screen pos + `name = value`.
    fn compute_hover(
        &mut self,
        snap: &UiState,
        src_out: &SourcePaneOut,
        ppp: f32,
    ) -> Option<(egui::Pos2, String)> {
        if !snap.stopped {
            return None;
        }
        let screen = src_out.hover?;
        let ed = self.editor.as_mut()?;
        let x = (screen.x - src_out.rect.min.x) * ppp;
        let y = (screen.y - src_out.rect.min.y) * ppp;
        let (line, col) = ed.pos_at_pixel(x, y)?;
        let word = word_at(&line, col)?;
        let val = lookup_var(snap, &word)?;
        Some((screen, format!("{word} = {val}")))
    }

    /// Create/size the embedded editor, load the viewed file, apply forwarded input,
    /// push breakpoints + the current execution line, and render it. Returns whether
    /// its texture needs (re)registering with egui (first use or after a resize).
    fn sync_editor(
        &mut self,
        snap: &UiState,
        src_out: &SourcePaneOut,
        ppp: f32,
        scroll_line: Option<i32>,
    ) -> bool {
        let rect = src_out.rect;
        if !rect.is_finite() || rect.width() < 1.0 || rect.height() < 1.0 {
            return false;
        }
        let w = (rect.width() * ppp).round().max(1.0) as u32;
        let h = (rect.height() * ppp).round().max(1.0) as u32;

        let mut needs_register = false;
        if self.editor.is_none() {
            let mut ed = EditorView::new(w, h, ppp);
            ed.set_read_only(true);
            ed.set_bp_gutter(true); // the debugger always shows/toggles breakpoints
            ed.set_language("cs");
            // Match the Unity code editor's look (dark theme + RobotoMono).
            ed.set_theme(0.156, 0.156, 0.156, 1.0, 0xd2, 0xd2, 0xd2, true);
            if let Some(p) = unity_font_path("RobotoMono-Regular.ttf") {
                ed.set_font(&p);
            }
            self.editor = Some(ed);
            self.editor_dims = (w, h);
            needs_register = true;
        } else if self.editor_dims != (w, h) {
            self.editor.as_mut().unwrap().resize(w, h, ppp);
            self.editor_dims = (w, h);
            needs_register = true; // the shared surface (and its view) was recreated
        }

        // Load the viewed file when it changes.
        if self.view_file != self.editor_file {
            self.editor_file = self.view_file.clone();
            let text = std::fs::read_to_string(&self.view_file).unwrap_or_default();
            self.editor.as_mut().unwrap().set_text(&text);
        }

        let ed = self.editor.as_mut().unwrap();

        // Forward collected input. A press in the gutter toggles a breakpoint (routed
        // through the session, the source of truth) instead of moving the caret.
        let gutter = ed.gutter_width();
        for act in &src_out.acts {
            match act {
                EditorAct::Mouse { x, y, kind } => {
                    if *kind == 0 && *x < gutter {
                        let line = ed.line_at_y(*y);
                        self.tx.send(Cmd::ToggleBp(line as i32 + 1)).ok();
                    } else {
                        ed.mouse(*x, *y, *kind);
                    }
                }
                EditorAct::Scroll { dx, dy } => {
                    ed.scroll(*dy);
                    ed.scroll_h(*dx);
                }
                EditorAct::Key(name, ctrl, alt, shift) => ed.key(name, *ctrl, *alt, *shift),
            }
        }

        // Breakpoints (1-based store → 0-based editor) + the execution line.
        let bp0: Vec<u32> = snap
            .bp_lines
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| (l - 1) as u32)
            .collect();
        ed.set_breakpoints(&bp0);
        let viewing_stopped =
            snap.stopped && basename(&self.view_file) == basename(&snap.cur_file);
        ed.set_exec_line(if viewing_stopped && snap.cur_line > 0 {
            (snap.cur_line - 1) as usize
        } else {
            usize::MAX
        });

        if let Some(line) = scroll_line {
            if line > 0 {
                ed.goto_line((line - 1) as usize);
            }
        }

        ed.render();
        needs_register
    }
}

/// Absolute path to a Unity-bundled font (`Contents/Resources/Fonts/<name>`), if present.
fn unity_font_path(name: &str) -> Option<String> {
    let inst = sdb::find_editor_instance(&std::env::current_dir().ok()?)?;
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(inst).ok()?).ok()?;
    let contents = json.get("app_contents_path")?.as_str()?;
    let p = std::path::Path::new(contents)
        .join("Resources")
        .join("Fonts")
        .join(name);
    p.exists().then(|| p.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Theme (Unity dark editor)
// ---------------------------------------------------------------------------

fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn setup_theme(ctx: &egui::Context) {
    // Fonts: Unity's Inter (UI) + RobotoMono (code), if available.
    let mut fonts = egui::FontDefinitions::default();
    if let Some(b) = unity_font_bytes("Inter-Regular.ttf") {
        fonts.font_data.insert("inter".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "inter".into());
    }
    if let Some(b) = unity_font_bytes("RobotoMono-Regular.ttf") {
        fonts.font_data.insert("robotomono".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "robotomono".into());
    }
    // Japanese fallback (Inter/RobotoMono have no CJK; Unity bundles no CJK font),
    // appended last so kanji/kana render instead of tofu. Try each platform's system
    // fonts; non-existent paths are simply skipped.
    for jp in [
        // macOS
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        // Windows
        "C:\\Windows\\Fonts\\YuGothM.ttc",
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        // Linux (Noto CJK, common install paths)
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ] {
        if let Ok(b) = std::fs::read(jp) {
            fonts.font_data.insert("jp".into(), Arc::new(egui::FontData::from_owned(b)));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push("jp".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);

    use egui::{FontFamily, TextStyle};
    let text_styles: std::collections::BTreeMap<TextStyle, FontId> = [
        (TextStyle::Small, FontId::new(10.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into();

    let mut v = egui::Visuals::dark();
    v.window_fill = c(0x38, 0x38, 0x38);
    v.panel_fill = c(0x38, 0x38, 0x38);
    v.extreme_bg_color = c(0x28, 0x28, 0x28);
    v.faint_bg_color = c(0x3f, 0x3f, 0x3f);
    v.code_bg_color = c(0x28, 0x28, 0x28);
    v.override_text_color = Some(c(0xc4, 0xc4, 0xc4));
    v.hyperlink_color = c(0x4c, 0x7e, 0xff);
    v.selection.bg_fill = c(0x2c, 0x5d, 0x87);
    v.selection.stroke = egui::Stroke::new(1.0, c(0x4c, 0x7e, 0xff));
    v.widgets.noninteractive.bg_fill = c(0x38, 0x38, 0x38);
    v.widgets.noninteractive.fg_stroke.color = c(0xc4, 0xc4, 0xc4);
    v.widgets.inactive.bg_fill = c(0x58, 0x58, 0x58);
    v.widgets.inactive.weak_bg_fill = c(0x58, 0x58, 0x58);
    v.widgets.inactive.fg_stroke.color = c(0xc4, 0xc4, 0xc4);
    v.widgets.hovered.bg_fill = c(0x67, 0x67, 0x67);
    v.widgets.hovered.weak_bg_fill = c(0x67, 0x67, 0x67);
    v.widgets.hovered.fg_stroke.color = c(0xe0, 0xe0, 0xe0);
    v.widgets.active.bg_fill = c(0x4c, 0x7e, 0xff);
    v.widgets.active.weak_bg_fill = c(0x4c, 0x7e, 0xff);
    v.widgets.open.bg_fill = c(0x45, 0x45, 0x45);
    // Visible panel separators / borders (#232323), brighter on hover for resizing.
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, c(0x20, 0x20, 0x20));
    v.widgets.hovered.bg_stroke = egui::Stroke::new(2.0, c(0x4c, 0x7e, 0xff));
    v.widgets.active.bg_stroke = egui::Stroke::new(2.0, c(0x4c, 0x7e, 0xff));
    ctx.all_styles_mut(move |style| {
        style.text_styles = text_styles.clone();
        style.visuals = v.clone();
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.interaction.resize_grab_radius_side = 8.0;
        // Only the source code + variable values opt into selection (below); keep
        // titles, the file tree, the stack and the breakpoint list non-selectable.
        style.interaction.selectable_labels = false;
    });
}

fn unity_font_bytes(name: &str) -> Option<Vec<u8>> {
    let inst = sdb::find_editor_instance(&std::env::current_dir().ok()?)?;
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(inst).ok()?).ok()?;
    let contents = json.get("app_contents_path")?.as_str()?;
    let p = std::path::Path::new(contents)
        .join("Resources")
        .join("Fonts")
        .join(name);
    std::fs::read(p).ok()
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// Borrowed data the UI reads each frame (avoids capturing `&mut App` in nested
/// egui closures, which would be a borrow conflict).
struct UiCtx<'a> {
    s: &'a UiState,
    tree: &'a Tree,
    files: &'a [(String, String)],
    /// egui texture handle for the embedded editor (None until its first render).
    editor_tex: Option<egui::TextureId>,
    /// Hover-watch tooltip to draw over the source (screen pos + text).
    hover_tip: Option<(egui::Pos2, String)>,
}

fn build_ui(
    ui: &mut egui::Ui,
    ctx: &UiCtx,
    cmds: &mut Vec<Cmd>,
    new_view: &mut Option<String>,
    goto: &mut Option<i32>,
    focus: &mut Focus,
    expanded: &mut std::collections::HashSet<String>,
    tree_sel: &mut usize,
    var_open: &mut std::collections::HashSet<u32>,
    src_out: &mut SourcePaneOut,
) {
    let s = ctx.s;
    let accent = c(0x4c, 0x7e, 0xff);
    let amber = c(0xe6, 0xb4, 0x40);

    // --- toolbar ---
    egui::Panel::top("toolbar").show(ui, |ui| {
        ui.horizontal(|ui| {
            let enabled = s.stopped;
            ui.add_enabled_ui(enabled, |ui| {
                if ui.button("▶").on_hover_text("Continue (c)").clicked() {
                    cmds.push(Cmd::Resume);
                }
                if ui.button("⤼").on_hover_text("Step Over (n)").clicked() {
                    cmds.push(Cmd::StepOver);
                }
                if ui.button("↓").on_hover_text("Step Into (i)").clicked() {
                    cmds.push(Cmd::StepInto);
                }
                if ui.button("↑").on_hover_text("Step Out (o)").clicked() {
                    cmds.push(Cmd::StepOut);
                }
            });
            ui.add_enabled_ui(s.attached && !s.stopped, |ui| {
                if ui.button("⏸").on_hover_text("Pause / Break All (p)").clicked() {
                    cmds.push(Cmd::Pause);
                }
            });
            ui.separator();
            let (word, col) = if !s.attached {
                if s.status.is_empty() {
                    ("Connecting…", c(0x9a, 0x9a, 0x9a))
                } else {
                    (s.status.as_str(), c(0xd6, 0x5a, 0x5a))
                }
            } else if s.stopped {
                ("Paused", amber)
            } else {
                ("Running", c(0x88, 0xc0, 0x70))
            };
            ui.label(RichText::new("●").color(col));
            let resp = ui.label(RichText::new(word).color(col));
            if s.attached {
                let tip = if s.version.is_empty() {
                    "attached".to_string()
                } else {
                    format!("attached — {}", s.version)
                };
                resp.on_hover_text(tip);
            }
            if s.stopped && !s.cur_file.is_empty() {
                ui.label(RichText::new(format!("{}:{}", basename(&s.cur_file), s.cur_line)).weak());
            }

            // Target picker: the editor, plus any debuggable players found on the network.
            ui.separator();
            let cur = if s.cur_target.is_empty() { "Editor" } else { s.cur_target.as_str() };
            egui::ComboBox::from_id_salt("target")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(cur == "Editor", "Editor (this project)").clicked() {
                        cmds.push(Cmd::SelectTarget(Target {
                            ip: String::new(),
                            port: 0,
                            label: "Editor".to_string(),
                        }));
                    }
                    for p in &s.players {
                        let label = format!("{} ({})", p.label, p.ip);
                        let text = if p.project.is_empty() {
                            label.clone()
                        } else {
                            format!("{} · {}", p.project, label)
                        };
                        if ui.selectable_label(cur == label, text).clicked() {
                            cmds.push(Cmd::SelectTarget(Target {
                                ip: p.ip.clone(),
                                port: p.port,
                                label,
                            }));
                        }
                    }
                    if s.players.is_empty() {
                        ui.label(RichText::new("(no players found)").weak());
                    }
                });
        });
    });

    // keyboard shortcuts
    ui.input(|i| {
        if s.stopped {
            if i.key_pressed(egui::Key::C) {
                cmds.push(Cmd::Resume);
            }
            if i.key_pressed(egui::Key::N) {
                cmds.push(Cmd::StepOver);
            }
            if i.key_pressed(egui::Key::I) {
                cmds.push(Cmd::StepInto);
            }
            if i.key_pressed(egui::Key::O) {
                cmds.push(Cmd::StepOut);
            }
        } else if s.attached && i.key_pressed(egui::Key::P) {
            cmds.push(Cmd::Pause);
        }
    });

    // --- left: file tree (no title; full rows; scrollable) ---
    let files_resp = egui::Panel::left("files")
        .resizable(true)
        .default_size(210.0)
        .show(ui, |ui| {
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                tree_panel(ui, ctx.tree, *focus == Focus::Tree, expanded, tree_sel, new_view);
            });
        });

    // --- right: three vertically-stacked, resizable panes ---
    egui::Panel::right("inspector")
        .resizable(true)
        .default_size(330.0)
        .show(ui, |ui| {
            // Cap each pane's max height so the next pane's title (>= ~46px) always
            // stays visible — egui doesn't auto-negotiate sizes between stacked panels.
            let min = 46.0_f32;
            let total = ui.available_height();
            let stack_max = (total - 2.0 * min).max(min);
            egui::Panel::top("p_stack").resizable(true).default_size(150.0).size_range(egui::Rangef::new(min, stack_max)).show(ui, |ui| {
                // Threads strip: only when the stop involves more than one managed thread.
                if s.threads.len() > 1 {
                    ui.label(RichText::new("Threads").strong());
                    egui::ScrollArea::both().id_salt("sa_threads").max_height(72.0).auto_shrink([false, false]).show(ui, |ui| {
                        for t in &s.threads {
                            let selected = t.id == s.cur_thread;
                            let col = if selected { accent } else { c(0xc4, 0xc4, 0xc4) };
                            let resp = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{}  ·  {}", t.name, t.location)).monospace().color(col),
                                )
                                .selectable(false)
                                .truncate()
                                .sense(egui::Sense::click()),
                            );
                            if selected {
                                ui.painter().rect_filled(
                                    resp.rect.expand2(egui::vec2(4.0, 1.0)),
                                    2.0,
                                    accent.gamma_multiply(0.18),
                                );
                            }
                            if resp.clicked() && !selected {
                                cmds.push(Cmd::SelectThread(t.id));
                            }
                        }
                    });
                    ui.separator();
                }
                ui.label(RichText::new("Call Stack").strong());
                egui::ScrollArea::both().id_salt("sa_stack").auto_shrink([false, false]).show(ui, |ui| {
                    for (i, f) in s.stack.iter().enumerate() {
                        let selected = i == s.cur_frame;
                        let col = if selected { accent } else { c(0xc4, 0xc4, 0xc4) };
                        let resp = ui.add(
                            egui::Label::new(RichText::new(f).monospace().color(col))
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        );
                        if selected {
                            ui.painter().rect_filled(
                                resp.rect.expand2(egui::vec2(4.0, 1.0)),
                                2.0,
                                accent.gamma_multiply(0.18),
                            );
                        }
                        if resp.clicked() && i != s.cur_frame {
                            cmds.push(Cmd::SelectFrame(i));
                        }
                    }
                });
            });
            let vars_max = (ui.available_height() - min).max(min);
            egui::Panel::top("p_vars").resizable(true).default_size(240.0).size_range(egui::Rangef::new(min, vars_max)).show(ui, |ui| {
                ui.label(RichText::new("Variables").strong());
                egui::ScrollArea::both().id_salt("sa_vars").auto_shrink([false, false]).show(ui, |ui| {
                    egui::CollapsingHeader::new("Local").default_open(true).show(ui, |ui| {
                        if s.locals.is_empty() {
                            ui.label(RichText::new("(none)").weak());
                        }
                        var_tree(ui, &s.locals, &s.children, var_open, cmds, 0, &mut Vec::new());
                    });
                    if !s.this_label.is_empty() {
                        egui::CollapsingHeader::new(format!("this : {}", s.this_label))
                            .default_open(true)
                            .show(ui, |ui| {
                                var_tree(ui, &s.members, &s.children, var_open, cmds, 0, &mut Vec::new());
                            });
                    }
                });
            });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label(RichText::new("Breakpoints").strong());
                egui::ScrollArea::both().id_salt("sa_bps").auto_shrink([false, false]).show(ui, |ui| {
                    if s.all_bps.is_empty() {
                        ui.label(RichText::new("(none)").weak());
                    }
                    for (file, line) in &s.all_bps {
                        let resp = ui
                            .horizontal(|ui| {
                                ui.label(RichText::new("●").color(c(0xc8, 0x3a, 0x3a)));
                                ui.add(
                                    egui::Label::new(RichText::new(format!("{file}:{line}")).monospace())
                                        .sense(egui::Sense::click()),
                                )
                            })
                            .inner;
                        if resp.clicked() {
                            if let Some((abs, _)) = ctx.files.iter().find(|(_, n)| n == file) {
                                *new_view = Some(abs.clone());
                                *goto = Some(*line);
                            }
                        }
                    }
                });
            });
        });

    // --- center: source ---
    let src_resp = egui::CentralPanel::default().show(ui, |ui| {
        source_view(ui, ctx, *focus == Focus::Source, src_out);
    });

    // Focus follows the last click (updates next frame's arrow-key target).
    if ui.ctx().input(|i| i.pointer.any_pressed()) {
        if let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos()) {
            *focus = if files_resp.response.rect.contains(p) {
                Focus::Tree
            } else if src_resp.response.rect.contains(p) {
                Focus::Source
            } else {
                Focus::None
            };
        }
    }
}

/// Render variable rows, recursing into expanded objects/arrays (children fetched
/// lazily via `Cmd::ExpandVar` the first time a node is opened).
///
/// `path` is the chain of object ids currently descended through. Because open state
/// is keyed by object id, a cyclic graph (e.g. Unity's `transform ↔ gameObject`) would
/// otherwise recurse forever — expanding both ends makes every occurrence auto-open —
/// exhausting memory. We refuse to descend into an id already on the path.
fn var_tree(
    ui: &mut egui::Ui,
    vars: &[Var],
    children: &std::collections::HashMap<u32, Vec<Var>>,
    var_open: &mut std::collections::HashSet<u32>,
    cmds: &mut Vec<Cmd>,
    depth: usize,
    path: &mut Vec<u32>,
) {
    for v in vars {
        let open = v.expand.map(|e| var_open.contains(&e.id)).unwrap_or(false);
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 14.0);
            // Expand triangle (or a spacer, to keep names aligned).
            if let Some(e) = v.expand {
                let tri = if open { "▾" } else { "▸" };
                let hit = ui.add(
                    egui::Label::new(RichText::new(tri).monospace().color(c(0x9a, 0x9a, 0x9a)))
                        .sense(egui::Sense::click()),
                );
                let name = ui.add(
                    egui::Label::new(RichText::new(&v.name).monospace().color(c(0xa0, 0xa0, 0xa0)))
                        .selectable(false)
                        .sense(egui::Sense::click()),
                );
                if hit.clicked() || name.clicked() {
                    if open {
                        var_open.remove(&e.id);
                    } else {
                        var_open.insert(e.id);
                        if !children.contains_key(&e.id) {
                            cmds.push(Cmd::ExpandVar { id: e.id, array: e.array });
                        }
                    }
                }
            } else {
                ui.add_space(12.0);
                ui.label(RichText::new(&v.name).monospace().color(c(0xa0, 0xa0, 0xa0)));
            }
            if !v.value.is_empty() {
                ui.label(RichText::new("=").weak());
                // The value is selectable (copy it); names/triangles are not.
                ui.add(
                    egui::Label::new(RichText::new(&v.value).monospace().color(c(0xd2, 0xd2, 0xd2)))
                        .selectable(true),
                );
            }
        });
        if open {
            if let Some(e) = v.expand {
                if path.contains(&e.id) || depth >= 64 {
                    // Cycle (or pathologically deep): stop descending to bound memory.
                    leaf_note(ui, depth + 1, "…");
                } else {
                    match children.get(&e.id) {
                        Some(kids) if !kids.is_empty() => {
                            path.push(e.id);
                            var_tree(ui, kids, children, var_open, cmds, depth + 1, path);
                            path.pop();
                        }
                        Some(_) => leaf_note(ui, depth + 1, "(empty)"),
                        None => leaf_note(ui, depth + 1, "loading…"),
                    }
                }
            }
        }
    }
}

fn leaf_note(ui: &mut egui::Ui, depth: usize, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0 + 12.0);
        ui.label(RichText::new(text).weak());
    });
}

/// The source pane: draws the embedded editor's texture and forwards pointer/keys
/// to it (applied to the editor on `App` after the egui pass). The editor renders
/// the code, gutter, breakpoints, caret and current line — this is just its host.
fn source_view(ui: &mut egui::Ui, ctx: &UiCtx, focused: bool, out: &mut SourcePaneOut) {
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    out.rect = rect;

    if let Some(tex) = ctx.editor_tex {
        ui.painter().image(
            tex,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter().rect_filled(rect, 0.0, c(0x28, 0x28, 0x28));
    }

    // egui points → physical px relative to the pane's top-left (matching the code
    // editor window's `(mouse - rect) * ppp` convention).
    let ppp = ui.ctx().pixels_per_point();
    let phys = |p: egui::Pos2| ((p.x - rect.min.x) * ppp, (p.y - rect.min.y) * ppp);

    // Mouse (kinds: 0 click, 1 drag, 2 double-click).
    if resp.double_clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let (x, y) = phys(p);
            out.acts.push(EditorAct::Mouse { x, y, kind: 2 });
        }
    } else if resp.drag_started() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let (x, y) = phys(p);
            out.acts.push(EditorAct::Mouse { x, y, kind: 0 });
        }
    }
    if resp.dragged() {
        if let Some(p) = resp.interact_pointer_pos() {
            let (x, y) = phys(p);
            out.acts.push(EditorAct::Mouse { x, y, kind: 1 });
        }
    }

    // Wheel scroll + hover position (for the value tooltip) while over the pane.
    if resp.hovered() {
        let d = ui.input(|i| i.smooth_scroll_delta);
        if d.x != 0.0 || d.y != 0.0 {
            out.acts.push(EditorAct::Scroll { dx: -d.x * ppp, dy: -d.y * ppp });
        }
        out.hover = resp.hover_pos();
    }

    // Hover-watch tooltip (computed a frame behind, so it needs the live editor).
    if let Some((pos, text)) = &ctx.hover_tip {
        egui::Area::new(egui::Id::new("unterm_hover_watch"))
            .fixed_pos(*pos + egui::vec2(12.0, 16.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(text).monospace());
                });
            });
    }

    // Keyboard navigation when the pane has focus (read-only ⇒ only motions apply).
    if focused {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                if let Some(name) = map_editor_key(key, &modifiers) {
                    out.acts
                        .push(EditorAct::Key(name, modifiers.ctrl, modifiers.alt, modifiers.shift));
                }
            }
        }
    }
}

/// Translate an egui key press into the editor's key-name convention (navigation
/// only; shift extends the selection, alt/ctrl = word, cmd = line/document ends).
fn map_editor_key(key: egui::Key, m: &egui::Modifiers) -> Option<String> {
    use egui::Key;
    let cmd = m.command || m.mac_cmd;
    let word = m.alt || m.ctrl;
    let name = match key {
        Key::ArrowLeft => {
            if word {
                "WordLeft"
            } else if cmd {
                "Home"
            } else {
                "LeftArrow"
            }
        }
        Key::ArrowRight => {
            if word {
                "WordRight"
            } else if cmd {
                "End"
            } else {
                "RightArrow"
            }
        }
        Key::ArrowUp => {
            if cmd {
                "DocStart"
            } else {
                "UpArrow"
            }
        }
        Key::ArrowDown => {
            if cmd {
                "DocEnd"
            } else {
                "DownArrow"
            }
        }
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        _ => return None,
    };
    Some(name.to_string())
}

// --- file tree ---

#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    files: Vec<(String, String)>, // (name, abspath)
}

fn build_tree(files: &[(String, String)]) -> Tree {
    let root = project_root();
    let mut tree = Tree::default();
    for (abs, name) in files {
        let rel = std::path::Path::new(abs)
            .strip_prefix(&root)
            .ok()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(name));
        let mut comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // Show cached packages (Library/PackageCache/…) under the "Packages" node.
        if comps.len() >= 2 && comps[0] == "Library" && comps[1] == "PackageCache" {
            comps.splice(0..2, ["Packages".to_string()]);
        }
        let mut node = &mut tree;
        for (k, comp) in comps.iter().enumerate() {
            if k + 1 == comps.len() {
                node.files.push((comp.clone(), abs.clone()));
            } else {
                node = node.dirs.entry(comp.clone()).or_default();
            }
        }
    }
    tree
}

/// One visible row of the file tree, in display order (respecting `expanded`).
struct TreeRow {
    depth: usize,
    is_dir: bool,
    label: String,
    /// Folder path (tracked in `expanded`) for dirs, or the file abspath for files.
    key: String,
}

/// Flatten the tree into ordered visible rows, descending into open folders only.
fn flatten_tree(
    tree: &Tree,
    prefix: &str,
    depth: usize,
    expanded: &std::collections::HashSet<String>,
    out: &mut Vec<TreeRow>,
) {
    for (dname, child) in &tree.dirs {
        let key = if prefix.is_empty() {
            dname.clone()
        } else {
            format!("{prefix}/{dname}")
        };
        out.push(TreeRow {
            depth,
            is_dir: true,
            label: dname.clone(),
            key: key.clone(),
        });
        if expanded.contains(&key) {
            flatten_tree(child, &key, depth + 1, expanded, out);
        }
    }
    for (name, abs) in &tree.files {
        out.push(TreeRow {
            depth,
            is_dir: false,
            label: name.clone(),
            key: abs.clone(),
        });
    }
}

/// Custom file tree. Keyboard (when the pane is focused): ↑/↓ move the cursor over
/// the visible rows (folders included), → expands a folder (or steps into its first
/// child), ← collapses it (or steps out to the parent), and ↑/↓/Enter open a file
/// row. Clicking toggles a folder or opens a file. Full-width Unity-style selection.
fn tree_panel(
    ui: &mut egui::Ui,
    tree: &Tree,
    focused: bool,
    expanded: &mut std::collections::HashSet<String>,
    tree_sel: &mut usize,
    new_view: &mut Option<String>,
) {
    let mut rows = Vec::new();
    flatten_tree(tree, "", 0, expanded, &mut rows);
    if rows.is_empty() {
        return;
    }
    *tree_sel = (*tree_sel).min(rows.len() - 1);

    let mut scroll_sel = false;
    if focused {
        let (up, down, left, right, enter) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Enter),
            )
        });
        scroll_sel = up || down || left || right || enter;
        if down {
            *tree_sel = (*tree_sel + 1).min(rows.len() - 1);
        }
        if up {
            *tree_sel = tree_sel.saturating_sub(1);
        }
        if right {
            let r = &rows[*tree_sel];
            if r.is_dir {
                if !expanded.contains(&r.key) {
                    expanded.insert(r.key.clone());
                } else if *tree_sel + 1 < rows.len() && rows[*tree_sel + 1].depth > r.depth {
                    *tree_sel += 1; // already open: step into first child
                }
            }
        }
        if left {
            let (is_dir, key, depth) = (
                rows[*tree_sel].is_dir,
                rows[*tree_sel].key.clone(),
                rows[*tree_sel].depth,
            );
            if is_dir && expanded.contains(&key) {
                expanded.remove(&key); // collapse
            } else if depth > 0 {
                for k in (0..*tree_sel).rev() {
                    if rows[k].depth < depth {
                        *tree_sel = k; // step out to parent
                        break;
                    }
                }
            }
        }
        if up || down || enter {
            let r = &rows[*tree_sel];
            if !r.is_dir {
                *new_view = Some(r.key.clone());
            } else if enter {
                if expanded.contains(&r.key) {
                    expanded.remove(&r.key);
                } else {
                    expanded.insert(r.key.clone());
                }
            }
        }
    }

    let row_h = 18.0;
    let font = FontId::proportional(12.0);
    for (idx, r) in rows.iter().enumerate() {
        let selected = idx == *tree_sel;
        let text_col = if selected {
            c(0xf0, 0xf0, 0xf2)
        } else {
            c(0xc4, 0xc4, 0xc4)
        };
        let galley = ui
            .painter()
            .layout_no_wrap(r.label.clone(), font.clone(), text_col);
        let indent = 6.0 + r.depth as f32 * 14.0;
        let row_w = (indent + 16.0 + galley.size().x).max(ui.available_width());
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(row_w, row_h), egui::Sense::click());
        if selected {
            ui.painter().rect_filled(rect, 0.0, c(0x2c, 0x5d, 0x87));
            if scroll_sel {
                ui.scroll_to_rect(rect, None);
            }
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, c(0x45, 0x45, 0x45));
        }
        let x = rect.left() + indent;
        let cy = rect.center().y;
        if r.is_dir {
            // Draw the disclosure triangle as a vector shape: the loaded UI font has no
            // reliable ▸/▾ glyph, so a painted triangle renders crisply everywhere.
            let col = c(0x9a, 0x9a, 0x9a);
            let pts = if expanded.contains(&r.key) {
                vec![
                    egui::pos2(x + 1.0, cy - 2.0),
                    egui::pos2(x + 9.0, cy - 2.0),
                    egui::pos2(x + 5.0, cy + 3.0),
                ]
            } else {
                vec![
                    egui::pos2(x + 2.0, cy - 4.0),
                    egui::pos2(x + 2.0, cy + 4.0),
                    egui::pos2(x + 7.0, cy),
                ]
            };
            ui.painter()
                .add(egui::Shape::convex_polygon(pts, col, egui::Stroke::NONE));
        }
        ui.painter().galley(
            egui::pos2(x + 16.0, cy - galley.size().y / 2.0),
            galley,
            text_col,
        );
        if resp.clicked() {
            *tree_sel = idx;
            if r.is_dir {
                if expanded.contains(&r.key) {
                    expanded.remove(&r.key);
                } else {
                    expanded.insert(r.key.clone());
                }
            } else {
                *new_view = Some(r.key.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SDB session thread (unchanged from the glyphon version)
// ---------------------------------------------------------------------------

struct Stop {
    stack: Vec<String>,
    frames: Vec<sdb::Frame>,
    locals: Vec<Var>,
    members: Vec<Var>,
    this_label: String,
    file: String,
    line: i32,
}

struct Bp {
    file: String,
    line: i32,
    armed: bool,
    req: Option<i32>,
}

/// Listen for Unity player-discovery multicast and keep `s.players` in sync (deduped
/// by Guid, expiring players not heard from for a few seconds). Runs for the process's
/// lifetime; a bind failure (locked-down env) just leaves the player list empty.
fn spawn_discovery(state: Arc<Mutex<UiState>>, proxy: EventLoopProxy<Wake>) {
    std::thread::spawn(move || {
        let Ok(sock) = sdb::discovery_socket() else {
            return;
        };
        let mut seen: std::collections::HashMap<u32, (sdb::Player, std::time::Instant)> =
            std::collections::HashMap::new();
        let mut last_sig: Vec<u32> = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Some(p) = sdb::parse_player(&buf[..n]) {
                        seen.insert(p.guid, (p, std::time::Instant::now()));
                    }
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {}
            }
            seen.retain(|_, (_, t)| t.elapsed() < std::time::Duration::from_secs(4));
            // Only publish (and wake the UI) when the set of players actually changes.
            let mut sig: Vec<u32> = seen.keys().copied().collect();
            sig.sort_unstable();
            if sig != last_sig {
                last_sig = sig;
                let mut players: Vec<sdb::Player> = seen.values().map(|(p, _)| p.clone()).collect();
                players.sort_by(|a, b| a.project.cmp(&b.project).then(a.label.cmp(&b.label)));
                set(&state, &proxy, |s| s.players = players);
            }
        }
    });
}

fn session_thread(
    breakpoints: Vec<(String, i32)>,
    state: Arc<Mutex<UiState>>,
    rx: Receiver<Cmd>,
    proxy: EventLoopProxy<Wake>,
) {
    let root = std::env::current_dir().unwrap_or_default();
    // Breakpoints seeded on every (re)connect — the store is the source of truth; CLI
    // pairs, when given, override it for a manual run.
    let seed = if breakpoints.is_empty() {
        load_breakpoints().unwrap_or_default()
    } else {
        breakpoints
    };
    // Default target: this project's editor; the user can switch to a discovered player.
    // An empty ip is the "Editor" sentinel — resolved fresh (its pid can change) below.
    let mut target = Target { ip: String::new(), port: 0, label: "Editor".to_string() };
    loop {
        if target.ip.is_empty() {
            match sdb::editor_endpoint(&root) {
                Ok((ip, port)) => {
                    target.ip = ip;
                    target.port = port;
                }
                Err(_) => {
                    target.ip = "127.0.0.1".to_string();
                }
            }
        }
        set(&state, &proxy, |s| {
            s.attached = false;
            s.stopped = false;
            s.status = format!("Connecting to {}…", target.label);
        });
        let conn = match sdb::connect(&target.ip, target.port) {
            Ok(c) => c,
            Err(e) => {
                set(&state, &proxy, |s| s.status = format!("{}: connect failed: {e}", target.label));
                // Nothing to do until the user picks a (different) target.
                match wait_for_target(&rx) {
                    Some(t) => {
                        target = t;
                        continue;
                    }
                    None => return, // UI channel closed → shutting down
                }
            }
        };
        match run_connection(conn, &seed, state.clone(), &rx, proxy.clone(), &target.label) {
            Some(t) => target = t, // switch targets and reconnect
            None => return,        // disconnected / died / channel closed
        }
    }
}

/// Block until the UI asks to attach to a target (or the channel closes → `None`).
fn wait_for_target(rx: &Receiver<Cmd>) -> Option<Target> {
    loop {
        match rx.recv() {
            Ok(Cmd::SelectTarget(t)) => return Some(t),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// One attached session. Returns `Some(next)` to switch targets, or `None` to exit.
fn run_connection(
    mut conn: sdb::Connection,
    seed: &[(String, i32)],
    state: Arc<Mutex<UiState>>,
    rx: &Receiver<Cmd>,
    proxy: EventLoopProxy<Wake>,
    target_label: &str,
) -> Option<Target> {
    let v = conn.version().clone();
    while conn.take_queued().is_some() {}

    let mut bps: Vec<Bp> = seed
        .iter()
        .cloned()
        .map(|(file, line)| Bp { file, line, armed: false, req: None })
        .collect();
    set(&state, &proxy, |s| {
        s.attached = true;
        s.cur_target = target_label.to_string();
        s.status.clear();
        s.version = format!("{} (protocol {}.{})", v.vm, v.major, v.minor);
        s.push_log(format!("attached: {target_label} ({} bp)", bps.len()));
    });

    // Watch for a new play-mode AppDomain: the Play domain reload invalidates any
    // breakpoint armed on an edit-mode method, so we re-sync + re-arm on the fresh types.
    conn.set_event(wire::kind::APPDOMAIN_CREATE, wire::suspend::NONE, &[]).ok();
    let mut watch_req: Option<i32> = None;
    rewatch(&mut conn, &bps, &mut watch_req);
    arm_loaded(&mut conn, &mut bps, &state, &proxy);
    sync_bps(&state, &proxy, &bps, "");

    conn.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut cur_thread: u32 = 0;
    let mut cur_frames: Vec<sdb::Frame> = Vec::new();
    let mut step_req: Option<i32> = None;
    let mut ui_view = String::new();

    // The editor menu, re-opened while this debugger is already running, touches
    // `focus.request` to ask us to come forward instead of spawning a second window.
    let focus_path = unterm_state_path("focus.request");
    let mut last_focus = std::fs::metadata(&focus_path).ok().and_then(|m| m.modified().ok());

    // Live breakpoint sync: watch the shared store for edits the editor makes while we
    // run, and reconcile. `last_bp` is bumped after our own writes so we don't re-read
    // them back.
    let bp_path = unterm_state_path("breakpoints.json");
    let bp_mtime = || std::fs::metadata(&bp_path).ok().and_then(|m| m.modified().ok());
    let mut last_bp = bp_mtime();

    // Retry arming: when we attach mid-play the agent may not answer type queries yet,
    // so the initial `arm_loaded` finds nothing; keep retrying un-armed breakpoints
    // (against already-loaded types) so they take without a manual toggle.
    let mut last_arm = std::time::Instant::now();

    loop {
        let f = std::fs::metadata(&focus_path).ok().and_then(|m| m.modified().ok());
        if f != last_focus {
            last_focus = f;
            proxy.send_event(Wake::Focus).ok();
        }

        // Editor edited the breakpoint store while we run → reconcile live.
        let cur_bp = bp_mtime();
        if cur_bp != last_bp {
            last_bp = cur_bp;
            reconcile_breakpoints(&mut conn, &mut bps, &state, &proxy, &ui_view);
        }

        if bps.iter().any(|b| !b.armed)
            && last_arm.elapsed() >= Duration::from_millis(500)
        {
            last_arm = std::time::Instant::now();
            arm_loaded(&mut conn, &mut bps, &state, &proxy);
            sync_bps(&state, &proxy, &bps, &ui_view);
        }

        while let Ok(cmd) = rx.try_recv() {
            match &cmd {
                // Attach elsewhere: drop this connection (the agent detaches/resumes on
                // disconnect) and let the outer loop reconnect to the chosen target.
                Cmd::SelectTarget(t) => return Some(t.clone()),
                // View + breakpoint edits work any time (even while running).
                Cmd::View(file) => {
                    ui_view = file.clone();
                    sync_bps(&state, &proxy, &bps, &ui_view);
                }
                Cmd::ToggleBp(line) => {
                    let line = *line;
                    let base = basename(&ui_view).to_string();
                    if base.is_empty() {
                        continue;
                    }
                    if let Some(pos) = bps.iter().position(|b| basename(&b.file) == base && b.line == line) {
                        let req = bps[pos].req;
                        req.map(|q| conn.clear_event(wire::kind::BREAKPOINT, q));
                        bps.remove(pos);
                    } else {
                        let mut bp = Bp { file: base.clone(), line, armed: false, req: None };
                        if let Ok(types) = conn.types_for_source_file(&bp.file, true) {
                            if !types.is_empty() {
                                if let Some(req) = arm(&mut conn, &types, &bp.file, bp.line, &state, &proxy) {
                                    bp.armed = true;
                                    bp.req = Some(req);
                                }
                            }
                        }
                        bps.push(bp);
                    }
                    sync_bps(&state, &proxy, &bps, &ui_view);
                    persist_breakpoints(&ui_view, &bps);
                    last_bp = bp_mtime(); // our own write; don't reconcile it back
                }
                // Break into a running program: suspend, then inspect a thread that
                // has a resolvable managed stack (Unity spawns many native threads).
                Cmd::Pause if cur_thread == 0 => {
                    if conn.suspend().is_ok() {
                        let threads = conn.all_threads().unwrap_or_default();
                        let chosen = threads
                            .iter()
                            .copied()
                            .find(|&t| {
                                conn.frames(t)
                                    .ok()
                                    .and_then(|f| f.into_iter().next())
                                    .and_then(|f| conn.debug_info(f.method).ok().map(|i| (i, f.il_offset)))
                                    .and_then(|(i, il)| il_to_source(&i, il))
                                    .map(|(_, ln)| ln > 0)
                                    .unwrap_or(false)
                            })
                            .or_else(|| threads.first().copied())
                            .unwrap_or(0);
                        if chosen != 0 {
                            cur_thread = chosen;
                            let stop = dump(&mut conn, chosen);
                            cur_frames = stop.frames;
                            let threads = collect_threads(&mut conn);
                            ui_view = stop.file.clone();
                            set(&state, &proxy, |s| {
                                s.stopped = true;
                                s.threads = threads;
                                s.cur_thread = chosen;
                                s.cur_frame = 0;
                                s.cur_file = stop.file;
                                s.cur_line = stop.line;
                                s.stack = stop.stack;
                                s.locals = stop.locals;
                                s.members = stop.members;
                                s.this_label = stop.this_label;
                                s.children.clear();
                                s.gen = s.gen.wrapping_add(1);
                            });
                            sync_bps(&state, &proxy, &bps, &ui_view);
                            proxy.send_event(Wake::Focus).ok();
                        } else {
                            conn.resume().ok();
                        }
                    }
                }
                // Inspect a different thread: switch the active thread and dump its
                // innermost frame. Subsequent steps then act on this thread.
                Cmd::SelectThread(id) if cur_thread != 0 => {
                    let id = *id;
                    cur_thread = id;
                    let stop = dump(&mut conn, id);
                    cur_frames = stop.frames;
                    ui_view = stop.file.clone();
                    set(&state, &proxy, |s| {
                        s.cur_thread = id;
                        s.cur_frame = 0;
                        s.cur_file = stop.file;
                        s.cur_line = stop.line;
                        s.stack = stop.stack;
                        s.locals = stop.locals;
                        s.members = stop.members;
                        s.this_label = stop.this_label;
                        s.children.clear();
                        s.gen = s.gen.wrapping_add(1);
                    });
                    sync_bps(&state, &proxy, &bps, &ui_view);
                }
                // Inspect a different call-stack frame: recompute its variables and
                // jump the source view to its location.
                Cmd::SelectFrame(idx) if cur_thread != 0 => {
                    if let Some(frame) = cur_frames.get(*idx).cloned() {
                        let (locals, members, this_label) = frame_vars(&mut conn, cur_thread, &frame);
                        let (file, line) = conn
                            .debug_info(frame.method)
                            .ok()
                            .and_then(|info| il_to_source(&info, frame.il_offset))
                            .unwrap_or_default();
                        ui_view = file.clone();
                        let idx = *idx;
                        set(&state, &proxy, |s| {
                            s.cur_frame = idx;
                            s.cur_file = file;
                            s.cur_line = line;
                            s.locals = locals;
                            s.members = members;
                            s.this_label = this_label;
                            s.children.clear();
                            s.gen = s.gen.wrapping_add(1);
                        });
                        sync_bps(&state, &proxy, &bps, &ui_view);
                    }
                }
                // Lazily fetch an expandable value's children.
                Cmd::ExpandVar { id, array } if cur_thread != 0 => {
                    let (id, array) = (*id, *array);
                    let children = if array {
                        fetch_array(&mut conn, id)
                    } else {
                        fetch_object_fields(&mut conn, id)
                    };
                    set(&state, &proxy, |s| {
                        s.children.insert(id, children);
                    });
                }
                // Resume/step only make sense while stopped.
                Cmd::Resume if cur_thread != 0 => {
                    running(&state, &proxy);
                    conn.resume().ok();
                    cur_thread = 0;
                    cur_frames.clear();
                }
                step @ (Cmd::StepOver | Cmd::StepInto | Cmd::StepOut) if cur_thread != 0 => {
                    let depth = match step {
                        Cmd::StepInto => wire::step::DEPTH_INTO,
                        Cmd::StepOut => wire::step::DEPTH_OUT,
                        _ => wire::step::DEPTH_OVER,
                    };
                    if let Ok(req) = conn.step(cur_thread, wire::step::SIZE_LINE, depth) {
                        step_req = Some(req);
                    }
                    running(&state, &proxy);
                    conn.resume().ok();
                    cur_thread = 0;
                    cur_frames.clear();
                }
                _ => {}
            }
        }

        match conn.next_event() {
            Ok(comp) => {
                // Whether a breakpoint/step actually stopped us in this batch — if so we
                // stay suspended; otherwise a suspended batch (e.g. the type-load watch)
                // must be resumed once we've handled it.
                let mut stopped_here = false;
                for ev in &comp.events {
                    match ev.kind {
                        wire::kind::APPDOMAIN_CREATE => {
                            // Fresh (play-mode) domain: clear the old event requests (so
                            // they can't linger as duplicates), re-sync breakpoints from
                            // the shared store (so editor-side edits apply on this run),
                            // then re-watch + re-arm on TYPE_LOAD.
                            for bp in &mut bps {
                                if let Some(req) = bp.req.take() {
                                    conn.clear_event(wire::kind::BREAKPOINT, req).ok();
                                }
                                bp.armed = false;
                            }
                            if let Some(fresh) = load_breakpoints() {
                                bps = fresh
                                    .into_iter()
                                    .map(|(file, line)| Bp { file, line, armed: false, req: None })
                                    .collect();
                            }
                            rewatch(&mut conn, &bps, &mut watch_req);
                            sync_bps(&state, &proxy, &bps, &ui_view);
                        }
                        wire::kind::TYPE_LOAD => {
                            for bp in &mut bps {
                                if !bp.armed {
                                    if let Some(req) =
                                        arm(&mut conn, &[ev.type_id], &bp.file, bp.line, &state, &proxy)
                                    {
                                        bp.armed = true;
                                        bp.req = Some(req);
                                    }
                                }
                            }
                        }
                        wire::kind::BREAKPOINT | wire::kind::STEP => {
                            // A landed step keeps focus where it is; a real breakpoint
                            // hit surfaces the window (the user is over in the editor).
                            let was_step = step_req.is_some();
                            if let Some(req) = step_req.take() {
                                conn.clear_event(wire::kind::STEP, req).ok();
                            }
                            stopped_here = true;
                            cur_thread = ev.thread;
                            let stop = dump(&mut conn, ev.thread);
                            cur_frames = stop.frames;
                            let threads = collect_threads(&mut conn);
                            ui_view = stop.file.clone();
                            set(&state, &proxy, |s| {
                                s.stopped = true;
                                s.threads = threads;
                                s.cur_thread = ev.thread;
                                s.cur_frame = 0;
                                s.cur_file = stop.file;
                                s.cur_line = stop.line;
                                s.stack = stop.stack;
                                s.locals = stop.locals;
                                s.members = stop.members;
                                s.this_label = stop.this_label;
                                s.children.clear();
                                s.gen = s.gen.wrapping_add(1);
                            });
                            sync_bps(&state, &proxy, &bps, &ui_view);
                            if !was_step {
                                proxy.send_event(Wake::Focus).ok();
                            }
                        }
                        wire::kind::VM_DEATH => {
                            set(&state, &proxy, |s| {
                                s.attached = false;
                                s.status = "target exited".into();
                            });
                            proxy.send_event(Wake::Quit).ok();
                            return None;
                        }
                        _ => {}
                    }
                }
                // The type-load watch suspends the VM so we can arm a breakpoint on the
                // freshly-loaded type BEFORE its methods (Awake/Start) run — winning the
                // launch-vs-run race. Resume once armed (unless we actually stopped).
                if comp.suspend_policy != wire::suspend::NONE && !stopped_here {
                    conn.resume().ok();
                }
            }
            Err(wire::SdbError::Io(ref e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(wire::SdbError::Eof) => {
                set(&state, &proxy, |s| {
                    s.attached = false;
                    s.status = format!("{target_label} disconnected");
                });
                proxy.send_event(Wake::Quit).ok();
                return None;
            }
            Err(e) => set(&state, &proxy, |s| s.push_log(format!("event error: {e}"))),
        }
    }
}

/// Publish bp_lines (current file) + all_bps (everything) for the UI.
fn sync_bps(state: &Arc<Mutex<UiState>>, proxy: &EventLoopProxy<Wake>, bps: &[Bp], cur_file: &str) {
    let base = basename(cur_file);
    let lines: Vec<i32> = bps
        .iter()
        .filter(|b| basename(&b.file) == base)
        .map(|b| b.line)
        .collect();
    let all: Vec<(String, i32)> = bps
        .iter()
        .map(|b| (basename(&b.file).to_string(), b.line))
        .collect();
    set(state, proxy, |s| {
        s.bp_lines = lines;
        s.all_bps = all;
    });
}

fn persist_breakpoints(ui_view: &str, bps: &[Bp]) {
    if ui_view.is_empty() {
        return;
    }
    let dir = project_root().join("Library").join("Unterm");
    let path = dir.join("breakpoints.json");
    let mut files: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("files").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    let base = basename(ui_view);
    let lines: Vec<i64> = bps
        .iter()
        .filter(|b| basename(&b.file) == base)
        .map(|b| (b.line - 1) as i64)
        .collect();
    files.retain(|e| e.get("path").and_then(|p| p.as_str()) != Some(ui_view));
    if !lines.is_empty() {
        files.push(serde_json::json!({ "path": ui_view, "lines": lines }));
    }
    let out = serde_json::json!({ "files": files });
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, serde_json::to_string(&out).unwrap_or_default());
}

fn running(state: &Arc<Mutex<UiState>>, proxy: &EventLoopProxy<Wake>) {
    set(state, proxy, |s| {
        s.stopped = false;
        s.threads.clear();
        s.cur_thread = 0;
        s.cur_frame = 0;
        s.stack.clear();
        s.locals.clear();
        s.members.clear();
        s.this_label.clear();
        s.children.clear();
    });
}

/// (Re)subscribe to TYPE_LOAD for the files that currently hold breakpoints, clearing
/// any previous subscription so a newly-added breakpoint's file gets watched too.
fn rewatch(conn: &mut sdb::Connection, bps: &[Bp], watch_req: &mut Option<i32>) {
    if let Some(req) = watch_req.take() {
        conn.clear_event(wire::kind::TYPE_LOAD, req).ok();
    }
    let mut files: Vec<String> = bps.iter().map(|b| b.file.clone()).collect();
    files.sort();
    files.dedup();
    if !files.is_empty() {
        *watch_req = conn.watch_source_files(&files).ok();
    }
}

/// Arm every not-yet-armed breakpoint against the types already loaded for its file.
fn arm_loaded(
    conn: &mut sdb::Connection,
    bps: &mut [Bp],
    state: &Arc<Mutex<UiState>>,
    proxy: &EventLoopProxy<Wake>,
) {
    for bp in bps.iter_mut() {
        if bp.armed {
            continue;
        }
        if let Ok(types) = conn.types_for_source_file(&bp.file, true) {
            if !types.is_empty() {
                if let Some(req) = arm(conn, &types, &bp.file, bp.line, state, proxy) {
                    bp.armed = true;
                    bp.req = Some(req);
                }
            }
        }
    }
}

/// Reconcile our live breakpoints with the shared store (edited by the editor while
/// we run): clear breakpoints dropped from the store and arm ones newly added, leaving
/// unchanged ones (and their armed state) intact.
fn reconcile_breakpoints(
    conn: &mut sdb::Connection,
    bps: &mut Vec<Bp>,
    state: &Arc<Mutex<UiState>>,
    proxy: &EventLoopProxy<Wake>,
    ui_view: &str,
) {
    let Some(fresh) = load_breakpoints() else {
        return; // unreadable store: keep what we have rather than wiping
    };
    // Drop breakpoints no longer in the store (clearing their agent request).
    let mut i = 0;
    while i < bps.len() {
        if fresh.iter().any(|(f, l)| *f == bps[i].file && *l == bps[i].line) {
            i += 1;
        } else {
            if let Some(req) = bps[i].req.take() {
                conn.clear_event(wire::kind::BREAKPOINT, req).ok();
            }
            bps.remove(i);
        }
    }
    // Arm breakpoints newly added to the store (against already-loaded types; ones
    // whose type isn't loaded yet are picked up by the arm-retry / TYPE_LOAD watch).
    for (file, line) in fresh {
        if bps.iter().any(|b| b.file == file && b.line == line) {
            continue;
        }
        let mut bp = Bp { file: file.clone(), line, armed: false, req: None };
        if let Ok(types) = conn.types_for_source_file(&bp.file, true) {
            if !types.is_empty() {
                if let Some(req) = arm(conn, &types, &bp.file, bp.line, state, proxy) {
                    bp.armed = true;
                    bp.req = Some(req);
                }
            }
        }
        bps.push(bp);
    }
    sync_bps(state, proxy, bps, ui_view);
}

/// Read the shared breakpoint store as (basename, 1-based line) pairs. Returns `None`
/// on a missing/unreadable file so a transient error can't wipe live breakpoints.
fn load_breakpoints() -> Option<Vec<(String, i32)>> {
    let path = project_root().join("Library").join("Unterm").join("breakpoints.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut out = Vec::new();
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        for e in files {
            let Some(p) = e.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let base = basename(p).to_string();
            if let Some(lines) = e.get("lines").and_then(|l| l.as_array()) {
                for ln in lines.iter().filter_map(|l| l.as_i64()) {
                    out.push((base.clone(), ln as i32 + 1)); // store is 0-based
                }
            }
        }
    }
    Some(out)
}

fn arm(
    conn: &mut sdb::Connection,
    types: &[u32],
    file: &str,
    line: i32,
    state: &Arc<Mutex<UiState>>,
    proxy: &EventLoopProxy<Wake>,
) -> Option<i32> {
    let mut candidates = Vec::new();
    for &t in types {
        let Ok(methods) = conn.type_methods(t) else {
            continue;
        };
        for m in methods {
            if let Ok(info) = conn.debug_info(m) {
                candidates.push((m, info));
            }
        }
    }
    let (method, il) = sdb::resolve_line(&candidates, file, line)?;
    match conn.set_breakpoint(method, il) {
        Ok(req) => {
            let name = conn.method_name(method).unwrap_or_default();
            set(state, proxy, |s| s.push_log(format!("breakpoint: {name}+0x{il:x}")));
            Some(req)
        }
        Err(_) => None,
    }
}

/// Enumerate the managed threads (those with a resolvable source frame) for the
/// Threads strip, labelling each with its name and topmost source location.
fn collect_threads(conn: &mut sdb::Connection) -> Vec<ThreadInfo> {
    let mut out = Vec::new();
    for id in conn.all_threads().unwrap_or_default() {
        let frames = conn.frames(id).unwrap_or_default();
        let top = frames.iter().find_map(|f| {
            conn.debug_info(f.method)
                .ok()
                .and_then(|info| il_to_source(&info, f.il_offset))
                .filter(|(_, ln)| *ln > 0)
                .map(|(src, ln)| (f.method, src, ln))
        });
        let Some((method, src, ln)) = top else {
            continue; // native/threadpool thread with no managed frame: skip
        };
        let raw = conn.thread_name(id).unwrap_or_default();
        let name = if raw.is_empty() { format!("Thread {id}") } else { raw };
        let mname = conn.method_name(method).unwrap_or_default();
        out.push(ThreadInfo {
            id,
            name,
            location: format!("{mname}  {}:{ln}", basename(&src)),
        });
    }
    out
}

fn dump(conn: &mut sdb::Connection, thread: u32) -> Stop {
    let frames = conn.frames(thread).unwrap_or_default();
    let mut stack = Vec::new();
    let mut file = String::new();
    let mut line = 0;
    for (i, f) in frames.iter().enumerate().take(20) {
        let name = conn.method_name(f.method).unwrap_or_default();
        let (src, ln) = conn
            .debug_info(f.method)
            .ok()
            .and_then(|info| il_to_source(&info, f.il_offset))
            .unwrap_or_default();
        if i == 0 {
            file = src.clone();
            line = ln;
        }
        let loc = if ln > 0 { format!("{}:{ln}", basename(&src)) } else { String::new() };
        stack.push(format!("#{i} {name}  {loc}"));
    }

    let (locals, members, this_label) = match frames.first() {
        Some(top) => frame_vars(conn, thread, top),
        None => (Vec::new(), Vec::new(), String::new()),
    };
    Stop { stack, frames, locals, members, this_label, file, line }
}

/// Locals, `this` members, and the `this` type label for a single call-stack frame.
fn frame_vars(
    conn: &mut sdb::Connection,
    thread: u32,
    frame: &sdb::Frame,
) -> (Vec<Var>, Vec<Var>, String) {
    let mut locals = Vec::new();
    if let Ok(infos) = conn.locals_info(frame.method) {
        let in_scope: Vec<(usize, sdb::Local)> = infos
            .into_iter()
            .enumerate()
            .filter(|(_, l)| frame.il_offset >= l.live_start && frame.il_offset < l.live_end)
            .collect();
        let positions: Vec<i32> = in_scope.iter().map(|(i, _)| *i as i32).collect();
        if let Ok(values) = conn.frame_values(thread, frame.id, &positions) {
            for ((_, l), val) in in_scope.iter().zip(values) {
                locals.push(make_var(conn, l.name.clone(), &val));
            }
        }
    }

    let mut members = Vec::new();
    let mut this_label = String::new();
    if let Ok(value::Value::Object { id, .. }) = conn.frame_this(thread, frame.id) {
        if id != 0 {
            if let Ok(ty) = conn.object_type(id) {
                this_label = conn.type_name(ty).unwrap_or_default();
            }
            members = fetch_object_fields(conn, id);
        }
    }
    (locals, members, this_label)
}

/// Build a variable row, tagging it as expandable when it's a non-null object/array.
fn make_var(conn: &mut sdb::Connection, name: String, val: &value::Value) -> Var {
    let expand = match val {
        value::Value::Object { tag, id } if *id != 0 => Some(Expand {
            id: *id,
            array: *tag == value::tag::ARRAY || *tag == value::tag::SZARRAY,
        }),
        _ => None,
    };
    Var {
        name,
        value: render_value(conn, val),
        expand,
    }
}

/// The instance fields of an object as variable rows.
fn fetch_object_fields(conn: &mut sdb::Connection, id: u32) -> Vec<Var> {
    let mut out = Vec::new();
    let Ok(mut ty) = conn.object_type(id) else {
        return out;
    };
    // Walk the type hierarchy so inherited (base-class) instance fields show too —
    // `TYPE.GET_FIELDS` returns only the fields declared on each type. Derived fields
    // come first, then each base class up to (but not including) System.Object.
    for _ in 0..32 {
        if let Ok(fields) = conn.type_fields(ty) {
            let inst: Vec<sdb::Field> = fields.into_iter().filter(|f| !f.is_static()).collect();
            let ids: Vec<u32> = inst.iter().map(|f| f.id).collect();
            if !ids.is_empty() {
                if let Ok(vals) = conn.object_values(id, &ids) {
                    for (f, val) in inst.iter().zip(vals) {
                        out.push(make_var(conn, f.name.clone(), &val));
                    }
                }
            }
        }
        match conn.type_base(ty) {
            Ok(base) if base != 0 => ty = base,
            _ => break,
        }
    }
    out
}

/// The elements of an array as variable rows (capped, with an ellipsis overflow row).
fn fetch_array(conn: &mut sdb::Connection, id: u32) -> Vec<Var> {
    const CAP: i32 = 500;
    let len = conn.array_length(id).unwrap_or(0).max(0);
    let n = len.min(CAP);
    let mut out = Vec::new();
    if n > 0 {
        if let Ok(vals) = conn.array_values(id, 0, n) {
            for (i, val) in vals.iter().enumerate() {
                out.push(make_var(conn, format!("[{i}]"), val));
            }
        }
    }
    if len > n {
        out.push(Var {
            name: format!("… {} more", len - n),
            value: String::new(),
            expand: None,
        });
    }
    out
}

fn render_value(conn: &mut sdb::Connection, v: &value::Value) -> String {
    use value::Value;
    match v {
        Value::String(0) => "null".into(),
        Value::String(id) => format!("\"{}\"", conn.string_value(*id).unwrap_or_default()),
        Value::Object { id, .. } if *id == 0 => "null".into(),
        Value::Object { id, .. } => conn
            .object_type(*id)
            .ok()
            .and_then(|t| conn.type_name(t).ok())
            .map(|n| format!("{{{n}}}"))
            .unwrap_or_else(|| format!("obj#{id}")),
        Value::ValueType { fields, .. } => format!(
            "({})",
            fields.iter().map(|f| render_value(conn, f)).collect::<Vec<_>>().join(", ")
        ),
        other => other.summary(),
    }
}

fn il_to_source(info: &sdb::DebugInfo, il: i32) -> Option<(String, i32)> {
    let sp = info
        .seq_points
        .iter()
        .filter(|s| !s.is_hidden() && s.il_offset <= il)
        .max_by_key(|s| s.il_offset)?;
    let src = info.sources.first().cloned().unwrap_or_default();
    Some((src, sp.line))
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The C# identifier covering character column `col` in `line`, if any.
fn word_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    if !is_ident(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && is_ident(chars[end + 1]) {
        end += 1;
    }
    if chars[start].is_ascii_digit() {
        return None;
    }
    Some(chars[start..=end].iter().collect())
}

/// Look up a variable name among the in-scope locals and `this` members.
fn lookup_var<'a>(s: &'a UiState, name: &str) -> Option<&'a str> {
    s.locals
        .iter()
        .chain(s.members.iter())
        .find(|v| v.name == name)
        .map(|v| v.value.as_str())
}

/// The Unity project root (the dir containing `Library/EditorInstance.json`), so the
/// file tree and breakpoint store work regardless of the process's launch directory.
fn project_root() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(inst) = sdb::find_editor_instance(&cwd) {
        if let Some(root) = inst.parent().and_then(|p| p.parent()) {
            return root.to_path_buf();
        }
    }
    cwd
}

/// `Library/Unterm/<name>` in this project (the editor and debugger's shared dir).
fn unterm_state_path(name: &str) -> std::path::PathBuf {
    project_root().join("Library").join("Unterm").join(name)
}

/// Publish our PID so the editor launcher can detect (per project, via the path of
/// this file) that this project's debugger is already running.
fn write_pid_file() {
    let p = unterm_state_path("debugger.pid");
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, std::process::id().to_string());
}

/// The editor's process id (from `Library/EditorInstance.json`), to recognise it among
/// activating apps and among the on-screen window list.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn editor_pid() -> Option<i32> {
    let inst = sdb::find_editor_instance(&std::env::current_dir().ok()?)?;
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(inst).ok()?).ok()?;
    json.get("process_id").and_then(|v| v.as_i64()).map(|v| v as i32)
}

/// Observe `NSWorkspace` app-activation; when our editor becomes frontmost, post
/// `Wake::Raise`. Runs in the debugger's own run loop, so it fires even while the
/// editor is frozen (suspended at a breakpoint).
#[cfg(target_os = "macos")]
fn install_activation_observer(proxy: EventLoopProxy<Wake>) {
    use objc2::{class, msg_send, runtime::AnyObject};
    use objc2_app_kit::NSWorkspaceDidActivateApplicationNotification;
    let Some(epid) = editor_pid() else {
        return; // no editor to follow
    };
    unsafe {
        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if ws.is_null() {
            return;
        }
        let center: *mut AnyObject = msg_send![ws, notificationCenter];
        if center.is_null() {
            return;
        }
        let block = block2::RcBlock::new(move |_notif: *mut AnyObject| {
            let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            if ws.is_null() {
                return;
            }
            let front: *mut AnyObject = msg_send![ws, frontmostApplication];
            if front.is_null() {
                return;
            }
            let pid: i32 = msg_send![front, processIdentifier];
            if pid == epid {
                proxy.send_event(Wake::Raise).ok();
            }
        });
        let null = std::ptr::null::<AnyObject>();
        // The center copies the block, so it may outlive this scope's `block`.
        let _: *mut AnyObject = msg_send![
            center,
            addObserverForName: NSWorkspaceDidActivateApplicationNotification,
            object: null,
            queue: null,
            usingBlock: &*block,
        ];
    }
}

/// Slide our window to just below the editor's frontmost window — above other apps, but
/// not covering the editor (so no covering-flicker) and without stealing focus.
#[cfg(target_os = "macos")]
fn order_behind_editor(window: &Window) {
    use objc2::{msg_send, runtime::AnyObject};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Some(pid) = editor_pid() else {
        return;
    };
    let Some(editor_win) = cgwindow::frontmost_window(pid) else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    if let RawWindowHandle::AppKit(a) = handle.as_raw() {
        unsafe {
            let ns_view = a.ns_view.as_ptr() as *mut AnyObject;
            let ns_window: *mut AnyObject = msg_send![ns_view, window];
            if !ns_window.is_null() {
                // orderWindow:NSWindowBelow(-1) relativeTo:<editor window number>.
                let below: isize = -1;
                let _: () =
                    msg_send![ns_window, orderWindow: below, relativeTo: editor_win as isize];
            }
        }
    }
}

/// Minimal CoreGraphics window-list FFI: find a process's frontmost on-screen window
/// number. Window number + owner pid + layer need no screen-recording permission.
#[cfg(target_os = "macos")]
mod cgwindow {
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFRelease, CFTypeRef};
    use core_foundation_sys::dictionary::{CFDictionaryGetValueIfPresent, CFDictionaryRef};
    use core_foundation_sys::number::{kCFNumberSInt64Type, CFNumberGetValue, CFNumberRef};
    use core_foundation_sys::string::CFStringRef;
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
        static kCGWindowOwnerPID: CFStringRef;
        static kCGWindowNumber: CFStringRef;
        static kCGWindowLayer: CFStringRef;
    }
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const NULL_WINDOW: u32 = 0;

    unsafe fn dict_i64(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
        let mut val: *const c_void = std::ptr::null();
        if CFDictionaryGetValueIfPresent(dict, key as *const c_void, &mut val) == 0 || val.is_null()
        {
            return None;
        }
        let mut out: i64 = 0;
        if CFNumberGetValue(
            val as CFNumberRef,
            kCFNumberSInt64Type,
            &mut out as *mut i64 as *mut c_void,
        ) {
            Some(out)
        } else {
            None
        }
    }

    pub fn frontmost_window(pid: i32) -> Option<i64> {
        unsafe {
            let arr = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, NULL_WINDOW);
            if arr.is_null() {
                return None;
            }
            let mut found = None;
            // The list is front-to-back, so the first match is the frontmost window.
            for i in 0..CFArrayGetCount(arr) {
                let dict = CFArrayGetValueAtIndex(arr, i) as CFDictionaryRef;
                if dict.is_null() {
                    continue;
                }
                // Only normal-level windows (layer 0); skip menus/overlays/etc.
                if dict_i64(dict, kCGWindowOwnerPID) == Some(pid as i64)
                    && dict_i64(dict, kCGWindowLayer) == Some(0)
                {
                    found = dict_i64(dict, kCGWindowNumber);
                    break;
                }
            }
            CFRelease(arr as CFTypeRef);
            found
        }
    }
}

// --- Windows: same "follow the editor forward" via a foreground WinEvent hook ---

#[cfg(windows)]
static WIN_EDITOR_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static WIN_PROXY: Mutex<Option<EventLoopProxy<Wake>>> = Mutex::new(None);

#[cfg(windows)]
unsafe extern "system" fn win_event_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    use std::sync::atomic::Ordering;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != 0 && pid == WIN_EDITOR_PID.load(Ordering::Relaxed) {
        if let Ok(g) = WIN_PROXY.lock() {
            if let Some(p) = g.as_ref() {
                p.send_event(Wake::Raise).ok();
            }
        }
    }
}

#[cfg(windows)]
fn install_activation_observer(proxy: EventLoopProxy<Wake>) {
    use std::sync::atomic::Ordering;
    use windows::Win32::UI::Accessibility::SetWinEventHook;
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    };
    let Some(pid) = editor_pid() else {
        return;
    };
    WIN_EDITOR_PID.store(pid as u32, Ordering::Relaxed);
    *WIN_PROXY.lock().unwrap() = Some(proxy);
    unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            // Out-of-context hook: no DLL to load into other processes.
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
    }
}

#[cfg(windows)]
fn order_behind_editor(window: &Window) {
    use std::sync::atomic::Ordering;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE,
    };
    let pid = WIN_EDITOR_PID.load(Ordering::Relaxed);
    if pid == 0 {
        return;
    }
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let ours = HWND(h.hwnd.get() as *mut core::ffi::c_void);
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return;
        }
        let mut fgpid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut fgpid));
        if fgpid != pid {
            return; // the editor isn't actually the foreground window
        }
        // Insert our window directly below the editor's in z-order (no move/size/focus).
        let _ = SetWindowPos(ours, Some(fg), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_activation_observer(_proxy: EventLoopProxy<Wake>) {}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn order_behind_editor(_window: &Window) {}

fn scan_cs_files() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.') || matches!(name, "Library" | "Temp" | "Logs" | "obj") {
                    continue;
                }
                walk(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("cs") {
                let disp = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                out.push((p.to_string_lossy().into_owned(), disp));
            }
        }
    }
    let root = project_root();
    let mut out = Vec::new();
    for d in [
        root.join("Assets"),
        root.join("Packages"),
        // Registry/cached packages (read-only) — needed to set breakpoints in them.
        root.join("Library").join("PackageCache"),
    ] {
        if d.is_dir() {
            walk(&d, &mut out);
        }
    }
    if out.is_empty() {
        walk(&root, &mut out);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.truncate(12000);
    out
}

fn set(state: &Arc<Mutex<UiState>>, proxy: &EventLoopProxy<Wake>, f: impl FnOnce(&mut UiState)) {
    if let Ok(mut s) = state.lock() {
        f(&mut s);
    }
    proxy.send_event(Wake::Redraw).ok();
}
