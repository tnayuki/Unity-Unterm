//! The agent view: a single Rust object that owns the control-protocol
//! [`Driver`] (conversation state), the [`PanelRenderer`] (transcript), and the
//! [`InputBox`] (composer). It composes the displayed transcript (history +
//! pending permission + Thinking/Starting indicator), renders both surfaces,
//! draws and resolves the buttons (permission options + Send/Stop), and routes
//! input — so the C# host needs to know nothing about the agent. The host only
//! provides the window, raw input events, IME, clipboard, and repaint pacing.

use std::ffi::{c_void, CString};
use std::time::Instant;

use crate::control::{self, Conv, Driver, RS, US};
use crate::input::InputBox;
use crate::mcp::McpDispatcher;
use crate::panel::PanelRenderer;

/// `poll()` result flags.
pub const FLAG_DIRTY: u32 = 1; // something changed; host should render + repaint
pub const FLAG_ANIMATING: u32 = 2; // an indicator is animating; keep repainting

pub struct AgentView {
    driver: Option<Driver>,
    fail: String,
    panel: PanelRenderer,
    input: InputBox,

    /// Vertical transcript scroll in physical px (0 = latest/bottom).
    scroll: f32,
    /// Option ids for the currently shown permission buttons (click → respond).
    pending_ids: Vec<String>,
    /// A built-in command the host must run in a real terminal (e.g. `/login`),
    /// not the stream-json driver. Set by `send()`, drained once by the host.
    pending_host_cmd: Option<String>,
    /// Start time, for the indicator's dot animation.
    started: Instant,

    // Change detection for `poll()`. The transcript is tracked by the driver's
    // change serial, not a cloned copy — poll runs every editor tick, and cloning
    // the full transcript each tick just to compare it scaled with session length.
    last_transcript_serial: u64,
    /// Last minute (unix/60) `poll` ticked the now-relative time separators on —
    /// while one is visible, its "12 min ago" label re-renders once a minute.
    last_minute: u64,
    last_status: String,
    last_pending_title: String,
    last_dot: usize,

    // Cached NUL-terminated snapshots handed back over FFI.
    session_id_snap: CString,
    title_snap: CString,
    selected_snap: CString,
    text_snap: CString,
    copy_snap: CString,
    mode_snap: CString,
    model_snap: CString,
    models_snap: CString,
    commands_snap: CString,
    slash_snap: CString,
    host_cmd_snap: CString,
    /// Cached token under the last click, for the host to open as a file path.
    token_snap: CString,
}

impl AgentView {
    pub fn new(
        cwd: String,
        mcp: Option<McpDispatcher>,
        resume: Option<String>,
        panel_w: u32,
        panel_h: u32,
        input_w: u32,
        input_h: u32,
        effort: String,
        claude_cmd: String,
    ) -> Self {
        let (seed, resume_cwd) = match resume.as_deref() {
            Some(id) if !id.is_empty() => control::reconstruct_transcript(id),
            _ => (Conv::new(), None),
        };
        // Resume in the directory the session was created in (recorded in its
        // transcript), so claude's cwd-scoped `--resume` lookup resolves to the
        // right project dir. Falls back to the passed cwd for a fresh session, or
        // if the recorded directory is gone (a moved project).
        let cwd = resume_cwd
            .filter(|c| std::path::Path::new(c).is_dir())
            .unwrap_or(cwd);
        // The dir the agent runs in: relative file paths in the transcript resolve
        // against it, so the panel can gate the clickable-path underline on existence.
        let root = std::path::PathBuf::from(&cwd);
        let (driver, fail) = match Driver::new(cwd, mcp, resume, seed, effort, claude_cmd) {
            Ok(d) => (Some(d), String::new()),
            Err(e) => (None, e.to_string()),
        };
        let mut panel = PanelRenderer::new(panel_w, panel_h);
        panel.set_root(root);
        Self {
            driver,
            fail,
            panel,
            input: InputBox::new(input_w, input_h),
            scroll: 0.0,
            pending_ids: Vec::new(),
            pending_host_cmd: None,
            started: Instant::now(),
            last_transcript_serial: 0,
            last_minute: 0,
            last_status: String::new(),
            last_pending_title: String::new(),
            last_dot: usize::MAX,
            session_id_snap: CString::default(),
            title_snap: CString::default(),
            selected_snap: CString::default(),
            text_snap: CString::default(),
            copy_snap: CString::default(),
            mode_snap: CString::default(),
            model_snap: CString::default(),
            models_snap: CString::default(),
            commands_snap: CString::default(),
            slash_snap: CString::default(),
            host_cmd_snap: CString::default(),
            token_snap: CString::default(),
        }
    }

    fn status(&self) -> String {
        match &self.driver {
            Some(d) => d.status(),
            None => format!("spawn failed: {}", self.fail),
        }
    }

    /// Whether a turn is actively running (the user sent a prompt and the agent is
    /// thinking/replying). Distinct from idle/ready, `initializing` (session
    /// startup, incl. a resume), or a spawn failure — so the host can tell real
    /// conversation activity from merely opening/switching a session.
    pub fn is_thinking(&self) -> bool {
        self.status() == "thinking"
    }

    /// Indicator dot count (0..=3), animated off wall-clock time.
    fn dots(&self) -> usize {
        (self.started.elapsed().as_secs_f64() * 3.0) as usize % 4
    }

    /// Pull driver state, update the buttons/indicator, and report what changed.
    pub fn poll(&mut self) -> u32 {
        let mut flags = 0u32;
        let (status, transcript_serial, pending) = match &self.driver {
            Some(d) => (d.status(), d.transcript_serial(), d.pending_view()),
            None => (self.status(), 0, None),
        };

        // Permission buttons follow the pending prompt's options.
        let (title, ids, names) = match &pending {
            Some((t, opts)) => (
                t.clone(),
                opts.iter().map(|(id, _, _)| id.clone()).collect::<Vec<_>>(),
                opts.iter().map(|(_, n, _)| n.clone()).collect::<Vec<_>>(),
            ),
            None => (String::new(), Vec::new(), Vec::new()),
        };
        if ids != self.pending_ids {
            self.pending_ids = ids;
            self.panel.set_buttons(names);
            flags |= FLAG_DIRTY;
        }

        if transcript_serial != self.last_transcript_serial {
            self.last_transcript_serial = transcript_serial;
            flags |= FLAG_DIRTY;
        }
        // While a now-relative time separator is visible, repaint on minute
        // ticks so its "12 min ago" label follows the clock; otherwise the
        // tick costs one cheap stamp scan and no render.
        let now = crate::clock::now_secs();
        if now / 60 != self.last_minute {
            self.last_minute = now / 60;
            if self.driver.as_ref().is_some_and(|d| d.has_relative_stamp(now)) {
                flags |= FLAG_DIRTY;
            }
        }
        if status != self.last_status {
            self.last_status = status.clone();
            flags |= FLAG_DIRTY;
        }
        if title != self.last_pending_title {
            self.last_pending_title = title;
            flags |= FLAG_DIRTY;
        }
        if status == "thinking" || status == "initializing" {
            flags |= FLAG_ANIMATING;
            let d = self.dots();
            if d != self.last_dot {
                self.last_dot = d;
                flags |= FLAG_DIRTY;
            }
        }
        flags
    }

    /// Compose the panel text: transcript + pending note + animated indicator.
    fn compose(&self) -> String {
        let Some(d) = &self.driver else {
            return format!("spawn failed: {}", self.fail);
        };
        let mut text = d.transcript();
        if let Some((title, _)) = d.pending_view() {
            // An ExitPlanMode plan is Markdown: render it as a plan block (Markdown
            // in a capped, internally-scrollable box) before the "Ready to code?" note.
            if let Some(plan) = d.pending_plan() {
                push_plan(&mut text, &plan);
            }
            // A distinct card, not dim status text — and no "Thinking" indicator: the
            // session is blocked on the user's decision, not thinking.
            push_notice(&mut text, &title);
            return text;
        }
        let status = d.status();
        let label = match status.as_str() {
            "thinking" => Some("Thinking"),
            "initializing" => Some("Starting"),
            _ => None,
        };
        if let Some(label) = label {
            push_note(&mut text, &format!("{label}{}", ".".repeat(self.dots())));
        } else if status.starts_with("init failed") || status.starts_with("spawn failed") {
            // A dead session (most often: not signed in) renders nothing else, so
            // surface an actionable note instead of a blank panel. `/login` is
            // intercepted by `send()` and run in a real terminal.
            push_note(
                &mut text,
                "Not signed in — type /login to authenticate, then press Enter.",
            );
        }
        text
    }

    pub fn render(&mut self) {
        let text = self.compose();
        // Stop while a turn is running, Send otherwise.
        let btn = if self.status() == "thinking" { 2 } else { 1 };
        self.input.set_button(btn);
        self.panel.set_scroll(self.scroll);
        self.panel.render(&text);
        self.input.render();
    }

    // --- Sizing / theme -----------------------------------------------------

    pub fn resize(&mut self, pw: u32, ph: u32, iw: u32, ih: u32, scale: f32) {
        self.panel.resize(pw, ph);
        self.panel.set_scale(scale);
        self.input.resize(iw, ih);
        self.input.set_scale(scale);
    }

    pub fn set_theme(&mut self, br: f64, bg: f64, bb: f64, ba: f64, fr: u8, fg: u8, fb: u8) {
        self.panel.set_clear_color(br, bg, bb, ba);
        self.panel.set_text_color(fr, fg, fb, 255);
        self.input.set_clear_color(br, bg, bb, ba);
        self.input.set_text_color(fr, fg, fb, 255);
    }

    pub fn set_fonts(&mut self, regular: &str, bold: &str, italic: &str, bold_italic: &str) {
        self.panel.set_fonts(regular, bold, italic, bold_italic);
        if !regular.is_empty() {
            self.input.set_font(regular);
        }
    }

    // --- Textures / metrics -------------------------------------------------

    pub fn panel_texture(&self) -> *mut c_void {
        self.panel.raw_texture()
    }
    pub fn input_texture(&self) -> *mut c_void {
        self.input.raw_texture()
    }
    pub fn content_height(&self) -> f32 {
        self.panel.content_height()
    }
    pub fn input_height(&self) -> f32 {
        self.input.content_height()
    }
    pub fn caret_rect(&self) -> [f32; 4] {
        self.input.caret_rect()
    }
    pub fn set_focused(&mut self, focused: bool) {
        self.input.set_focused(focused);
    }
    pub fn set_scroll(&mut self, scroll: f32) {
        self.scroll = scroll.max(0.0);
    }

    // --- Actions ------------------------------------------------------------

    fn send(&mut self) {
        let text = self.input.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // `/login` and `/logout` are interactive built-in CLI commands (OAuth /
        // browser flow): they need a real TTY and can't run over stream-json.
        // Hand them to the host to launch in a real terminal instead of sending
        // them to the driver (which would hang on a turn that never finishes).
        if trimmed == "/login" || trimmed == "/logout" {
            self.pending_host_cmd = Some(trimmed.to_string());
            self.input.clear();
            self.scroll = 0.0;
            return;
        }
        if let Some(d) = &self.driver {
            d.send(&text);
        }
        self.input.clear();
        self.scroll = 0.0;
    }

    /// Drain the pending host command (e.g. `/login`), if any. Consume-once: the
    /// host runs it in a real terminal, then we forget it.
    pub fn take_host_command(&mut self) -> &CString {
        self.host_cmd_snap = clean(self.pending_host_cmd.take().unwrap_or_default());
        &self.host_cmd_snap
    }

    /// The whitespace-delimited token under (`x`, `y`) in the transcript, for the
    /// host to open if it resolves to a file (empty when the click isn't on one).
    pub fn panel_token_at(&mut self, x: f32, y: f32) -> &CString {
        self.token_snap = clean(self.panel.token_at(x, y).unwrap_or_default());
        &self.token_snap
    }

    /// The unix stamp of the time separator under (`x`, `y`) in the transcript
    /// (physical px), or 0 when not over one.
    pub fn panel_stamp_at(&self, x: f32, y: f32) -> u64 {
        self.panel.stamp_at(x, y)
    }

    /// The Send/Stop action: interrupt a running turn, else send the composer.
    fn action(&mut self) {
        if self.status() == "thinking" {
            if let Some(d) = &self.driver {
                d.interrupt();
            }
        } else {
            self.send();
        }
    }

    pub fn interrupt(&self) {
        if let Some(d) = &self.driver {
            d.interrupt();
        }
    }

    // --- Runtime settings (mode / model / thinking) + follow-up queue ----------

    pub fn set_permission_mode(&self, mode: &str) {
        if let Some(d) = &self.driver {
            d.set_permission_mode(mode);
        }
    }
    pub fn permission_mode(&mut self) -> &CString {
        let s = self.driver.as_ref().map(|d| d.permission_mode()).unwrap_or_default();
        self.mode_snap = clean(s);
        &self.mode_snap
    }
    pub fn set_model(&self, model: &str) {
        if let Some(d) = &self.driver {
            d.set_model(model);
        }
    }
    pub fn model(&mut self) -> &CString {
        let s = self.driver.as_ref().map(|d| d.model()).unwrap_or_default();
        self.model_snap = clean(s);
        &self.model_snap
    }
    pub fn models(&mut self) -> &CString {
        let s = self.driver.as_ref().map(|d| d.models()).unwrap_or_default();
        self.models_snap = clean(s);
        &self.models_snap
    }
    pub fn commands(&mut self) -> &CString {
        let s = self.driver.as_ref().map(|d| d.commands()).unwrap_or_default();
        self.commands_snap = clean(s);
        &self.commands_snap
    }
    pub fn queue_len(&self) -> u32 {
        self.driver.as_ref().map(|d| d.queue_len()).unwrap_or(0)
    }
    pub fn cancel_queued(&self, index: u32) {
        if let Some(d) = &self.driver {
            d.cancel_queued(index);
        }
    }

    // --- Panel (transcript) input ------------------------------------------

    /// Mouse-down in the transcript: resolve a permission button (→ respond) or
    /// start a selection. Returns true (the host should consume the event).
    pub fn panel_down(&mut self, x: f32, y: f32) -> bool {
        if !self.pending_ids.is_empty() {
            let idx = self.panel.hit_button(x, y);
            if idx >= 0 && (idx as usize) < self.pending_ids.len() {
                let id = self.pending_ids[idx as usize].clone();
                if let Some(d) = &self.driver {
                    d.respond(&id);
                }
                self.pending_ids.clear();
                self.panel.set_buttons(Vec::new());
                return true;
            }
        }
        // A click on a tool's header line folds/unfolds its content instead of
        // starting a selection.
        if let Some(key) = self.panel.hit_tool(x, y) {
            self.panel.toggle_tool(key);
            self.panel.selection_clear();
            return true;
        }
        self.panel.selection_begin(x, y);
        self.input.clear_selection();
        true
    }
    pub fn panel_drag(&mut self, x: f32, y: f32) {
        self.panel.selection_update(x, y);
    }
    pub fn panel_scroll_h(&mut self, x: f32, y: f32, dx: f32) -> bool {
        self.panel.scroll_h(x, y, dx)
    }
    pub fn panel_scroll_v(&mut self, x: f32, y: f32, dy: f32) -> bool {
        self.panel.scroll_v(x, y, dy)
    }
    pub fn panel_select_all(&mut self) {
        self.panel.select_all();
    }
    pub fn panel_select_clear(&mut self) {
        self.panel.selection_clear();
    }
    pub fn panel_has_selection(&self) -> bool {
        self.panel.has_selection()
    }

    // --- Composer (input box) input ----------------------------------------

    /// Mouse-down in the composer. Returns true if the Send/Stop button was hit
    /// (action performed); false for a normal click (host begins drag/focus).
    pub fn input_down(&mut self, x: f32, y: f32, kind: u8) -> bool {
        if self.input.hit_button(x, y) {
            self.action();
            return true;
        }
        self.input.mouse(x, y, kind);
        self.panel.selection_clear();
        false
    }
    pub fn input_drag(&mut self, x: f32, y: f32) {
        self.input.mouse(x, y, 1);
    }
    /// A key for the composer: Enter sends, Shift+Enter (and everything else)
    /// goes to the editor.
    pub fn input_key(&mut self, name: &str, ctrl: bool, alt: bool, shift: bool) {
        if name == "Return" && !shift {
            self.send();
        } else {
            self.input.key(name, ctrl, alt, shift);
        }
    }
    pub fn input_insert(&mut self, text: &str) {
        self.input.insert(text);
        self.panel.selection_clear();
    }
    /// The leading `/command` token under the composer caret (with the slash), or
    /// empty when not in slash-command context. Drives `/` completion.
    pub fn input_slash_prefix(&mut self) -> &CString {
        self.slash_snap = clean(self.input.slash_prefix());
        &self.slash_snap
    }
    /// Accept a completion: delete `prefix_len` chars before the caret, insert `text`.
    pub fn input_complete(&mut self, prefix_len: usize, text: &str) {
        self.input.complete(prefix_len, text);
    }
    /// Live IME composition shown inline (marked text); empty clears it.
    pub fn input_set_preedit(&mut self, text: &str) {
        self.input.set_preedit(text);
    }
    pub fn input_undo(&mut self) {
        self.input.undo();
    }
    pub fn input_redo(&mut self) {
        self.input.redo();
    }
    pub fn input_select_all(&mut self) {
        self.input.select_all();
    }

    // --- Clipboard bridges (host owns the OS clipboard) ---------------------

    pub fn input_copy(&mut self) -> &CString {
        let s = self.input.copy().unwrap_or_default();
        self.copy_snap = clean(s);
        &self.copy_snap
    }
    pub fn input_cut(&mut self) -> &CString {
        let s = self.input.cut().unwrap_or_default();
        self.copy_snap = clean(s);
        &self.copy_snap
    }
    pub fn input_text(&mut self) -> &CString {
        self.text_snap = clean(self.input.text());
        &self.text_snap
    }
    pub fn panel_selected_text(&mut self) -> &CString {
        self.selected_snap = clean(self.panel.selected_text());
        &self.selected_snap
    }

    // --- Identity (host owns the picker / persistence) ----------------------

    pub fn session_id(&mut self) -> &CString {
        let s = self.driver.as_ref().map(|d| d.session_id()).unwrap_or_default();
        self.session_id_snap = clean(s);
        &self.session_id_snap
    }
    /// First user line, for the host's session index title (capped at 48 chars).
    pub fn title(&mut self) -> &CString {
        let t = self
            .driver
            .as_ref()
            .map(|d| d.transcript())
            .unwrap_or_default();
        let title = first_user_line(&t);
        self.title_snap = clean(title);
        &self.title_snap
    }
}

/// Append a role-`t` note block to a role-tagged transcript string.
fn push_note(text: &mut String, body: &str) {
    text.push(RS);
    text.push('t');
    text.push(US);
    text.push_str(body);
}

/// A pending-permission request: a full-colour `Notice` card (see [`Role::Notice`]),
/// so it stands out as an actionable prompt rather than dim status text.
fn push_notice(text: &mut String, body: &str) {
    text.push(RS);
    text.push('n');
    text.push(US);
    text.push_str(body);
}

/// Append a role-`p` (plan) block — Markdown in a capped, scrollable box.
fn push_plan(text: &mut String, body: &str) {
    text.push(RS);
    text.push('p');
    text.push(US);
    text.push_str(body);
}

/// First line of the first user (`u`) block, trimmed and capped at 48 chars.
fn first_user_line(transcript: &str) -> String {
    for chunk in transcript.split(RS) {
        let mut it = chunk.splitn(2, US);
        let role = it.next().unwrap_or("");
        if role.starts_with('u') {
            let body = it.next().unwrap_or("").trim();
            let line = body.lines().next().unwrap_or("").trim();
            if line.chars().count() > 48 {
                let s: String = line.chars().take(48).collect();
                return format!("{s}…");
            }
            return line.to_string();
        }
    }
    String::new()
}

fn clean(s: String) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}
