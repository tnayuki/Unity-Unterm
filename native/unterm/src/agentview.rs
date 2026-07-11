//! The agent view: a single Rust object that owns the control-protocol
//! [`Driver`] (conversation state), the [`PanelRenderer`] (transcript), and the
//! [`InputBox`] (composer). It composes the displayed transcript (history +
//! pending permission + Thinking/Starting indicator), renders both surfaces,
//! draws and resolves the buttons (permission options + Send/Stop), and routes
//! input — so the C# host needs to know nothing about the agent. The host only
//! provides the window, raw input events, IME, clipboard, and repaint pacing.

use std::ffi::{c_void, CString};
use std::time::Instant;

use crate::browser::{BrowserView, Click};
use crate::control::{self, Conv, Driver, RS, US};
use crate::input::InputBox;
use crate::mcp::McpDispatcher;
use crate::panel::PanelRenderer;

/// `poll()` result flags.
pub const FLAG_DIRTY: u32 = 1; // something changed; host should render + repaint
pub const FLAG_ANIMATING: u32 = 2; // an indicator is animating; keep repainting
pub const FLAG_HOST_CMD: u32 = 4; // a host command is pending; drain take_host_command
pub const FLAG_META: u32 = 8; // permission mode / session id changed; re-read them

pub struct AgentView {
    driver: Option<Driver>,
    fail: String,
    panel: PanelRenderer,
    input: InputBox,

    /// The "All Sessions" browser, drawn in place of the transcript while
    /// `browsing`. Created on first entry, kept across exits (cheap re-entry);
    /// while browsing the composer doubles as its search box (the draft
    /// composer text is stashed and restored on exit).
    browser: Option<BrowserView>,
    browsing: bool,
    /// Composer draft stashed while the input box is the browser's search field.
    stashed_draft: String,
    /// The project root, for the browser's session store + archive file.
    cwd: String,
    /// Current panel size/scale/theme/font (mirrored from the host's setup calls)
    /// so a browser created later starts in sync with the panel.
    panel_size: (u32, u32),
    panel_scale: f32,
    theme_bg: [f64; 4],
    theme_fg: [u8; 3],
    ui_font: String,

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
    /// Whether a permission/decision prompt was up last `poll` (edge-detect a new
    /// one, which — unlike a finished turn — doesn't change the status string).
    had_pending: bool,
    /// Last permission mode / session id reported via `FLAG_META`, so the host
    /// only marshals those strings across the FFI on ticks they changed.
    last_mode: String,
    last_sid: String,
    /// One-shot "the session now needs the user" signal for the host to act on
    /// (chime + notification): 0 none, 1 turn finished, 2 waiting on a decision.
    /// Drained once by `take_attention`.
    attention: u32,
    /// Claude Code's generated title (`ai-title`) for this session, read from its
    /// jsonl so the tab/header matches the session picker. Empty until known;
    /// refreshed (throttled) as the conversation grows.
    ai_title: String,
    ai_title_read: u64, // unix secs of the last jsonl read (throttle)
    /// The id passed to resume this view, if any — the effective session id until
    /// the driver re-learns it from the init event (so the title reads its jsonl
    /// immediately on resume instead of waiting).
    resume_id: String,

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
        // The id we're resuming (empty for a fresh session): usable for the title
        // read straight away, before the driver re-learns it from the init event.
        let resume_id = resume.as_deref().unwrap_or_default().to_string();
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
        panel.set_root(root.clone());
        Self {
            driver,
            fail,
            panel,
            input: InputBox::new(input_w, input_h),
            browser: None,
            browsing: false,
            stashed_draft: String::new(),
            cwd: root.to_string_lossy().into_owned(),
            panel_size: (panel_w.max(1), panel_h.max(1)),
            panel_scale: 1.0,
            theme_bg: [0.05, 0.05, 0.05, 1.0],
            theme_fg: [210, 210, 214],
            ui_font: String::new(),
            scroll: 0.0,
            pending_ids: Vec::new(),
            pending_host_cmd: None,
            started: Instant::now(),
            last_transcript_serial: 0,
            last_minute: 0,
            last_status: String::new(),
            last_pending_title: String::new(),
            last_dot: usize::MAX,
            had_pending: false,
            last_mode: String::new(),
            last_sid: String::new(),
            attention: 0,
            ai_title: String::new(),
            ai_title_read: 0,
            resume_id,
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

    /// The session id to use now: the driver's once it's learned it from the init
    /// event, otherwise the id we resumed with (so the title/current-highlight are
    /// right immediately on resume instead of after init lands).
    fn effective_id(&self) -> String {
        let live = self.driver.as_ref().map(|d| d.session_id()).unwrap_or_default();
        if live.is_empty() {
            self.resume_id.clone()
        } else {
            live
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

    // --- Session browser ("All Sessions") -----------------------------------

    /// Enter/leave the browser. While on, the panel texture shows the session
    /// list and the composer is its search box (the draft is stashed/restored).
    pub fn set_browsing(&mut self, on: bool) {
        if on == self.browsing {
            return;
        }
        self.browsing = on;
        if on {
            // Snapshot the fields the browser needs before the mutable borrow.
            let (w, h) = self.panel_size;
            let scale = self.panel_scale;
            let bg = self.theme_bg;
            let clear = wgpu::Color { r: bg[0], g: bg[1], b: bg[2], a: bg[3] };
            let fg = glyphon::Color::rgb(self.theme_fg[0], self.theme_fg[1], self.theme_fg[2]);
            let family = resolve_family(&self.ui_font);
            let current = self.effective_id();
            let cwd = self.cwd.clone();
            let b = self
                .browser
                .get_or_insert_with(|| BrowserView::new(w, h, cwd));
            b.resize(w, h);
            b.set_scale(scale);
            b.set_theme(clear, fg);
            b.set_font(family);
            b.set_current(&current);
            b.set_scroll(0.0);
            self.stashed_draft = self.input.text();
            self.input.clear();
        } else {
            let draft = std::mem::take(&mut self.stashed_draft);
            self.input.set_text(&draft);
        }
    }

    pub fn browsing(&self) -> bool {
        self.browsing
    }

    /// Pointer motion over the browser list (hover highlight + icon). Returns
    /// true when the hover state changed (host should re-render + repaint).
    pub fn browse_hover(&mut self, x: f32, y: f32) -> bool {
        match (self.browsing, &mut self.browser) {
            (true, Some(b)) => b.hover(x, y),
            _ => false,
        }
    }

    /// Toggle showing archived sessions in the browser list.
    pub fn browse_toggle_archived(&mut self) {
        if let Some(b) = &mut self.browser {
            b.toggle_show_archived();
        }
    }

    /// How many of the browser's listed sessions are archived (for the host's
    /// "Archived" toggle visibility).
    pub fn browse_archived_count(&self) -> u64 {
        self.browser.as_ref().map(|b| b.archived_count() as u64).unwrap_or(0)
    }

    /// Open a browser row as a host command: the host owns view lifetimes (it
    /// must destroy this view and load the picked session), so hand it up.
    /// Using an archived session makes it current again — unarchive it here
    /// (there is no explicit unarchive anywhere else).
    fn browse_open(&mut self, id: String, title: String) {
        crate::sessions::set_archived(&self.cwd, &id, false);
        self.pending_host_cmd = Some(format!("resume{US}{id}{US}{title}"));
    }

    /// Pull driver state, update the buttons/indicator, and report what changed.
    pub fn poll(&mut self) -> u32 {
        // Reported in every mode — the session browser raises host commands too
        // (opening a session from it routes through one). The host drains the
        // string only on ticks this bit is set.
        let host_flag = if self.pending_host_cmd.is_some() { FLAG_HOST_CMD } else { 0 };
        // Browser mode: the composer text is the live search query.
        if self.browsing {
            let query = self.input.text();
            let dirty = match &mut self.browser {
                Some(b) => b.poll(query.trim()),
                None => false,
            };
            return host_flag | if dirty { FLAG_DIRTY } else { 0 };
        }
        let mut flags = host_flag;
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

        let transcript_changed = transcript_serial != self.last_transcript_serial;
        if transcript_changed {
            self.last_transcript_serial = transcript_serial;
            flags |= FLAG_DIRTY;
        }
        // Keep the tab/header title following Claude Code's generated ai-title (what
        // the session picker shows), read from this session's jsonl. Throttled: only
        // when the transcript changed and ≥3s since the last read, plus a first read
        // once the id is known.
        let sid = self.effective_id();
        if !sid.is_empty() {
            let now = crate::clock::now_secs();
            let first = self.ai_title.is_empty() && self.ai_title_read == 0;
            if first || (transcript_changed && now.saturating_sub(self.ai_title_read) >= 3) {
                self.ai_title_read = now.max(1);
                let next = crate::sessions::title_for(&self.cwd, &sid).unwrap_or_default();
                if next != self.ai_title {
                    self.ai_title = next;
                    flags |= FLAG_DIRTY;
                }
            }
        }
        // Session id / permission mode are cheap to compare here, but marshaling
        // them over the FFI allocates host-side — report changes as FLAG_META so
        // the host re-reads them only then.
        if sid != self.last_sid {
            self.last_sid = sid;
            flags |= FLAG_META;
        }
        let mode = self.driver.as_ref().map(|d| d.permission_mode()).unwrap_or_default();
        if mode != self.last_mode {
            self.last_mode = mode;
            flags |= FLAG_META;
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
        // Fire a one-shot attention signal when the session starts needing the
        // user: a finished turn (thinking→ready), or a freshly raised decision
        // prompt — which keeps the status at "thinking", so it needs its own edge.
        // The host chimes / notifies on it when the window is in the background.
        // Checked before `last_status` is updated below (it reads the prior value).
        let now_pending = pending.is_some();
        if self.last_status == "thinking" && status == "ready" {
            self.attention = 1;
        } else if now_pending && !self.had_pending {
            self.attention = 2;
        }
        self.had_pending = now_pending;
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

    /// Drain the one-shot attention signal (see the `attention` field): 0 none,
    /// 1 turn finished, 2 waiting on a decision. Consume-once.
    pub fn take_attention(&mut self) -> u32 {
        std::mem::replace(&mut self.attention, 0)
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
        if self.browsing {
            // The browser replaces the transcript; the composer stays rendered
            // as the search field (no Send/Stop button while browsing).
            self.input.set_button(0);
            if let Some(b) = &mut self.browser {
                b.set_scroll(self.scroll);
                b.render();
            }
            self.input.render();
            return;
        }
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
        self.panel_size = (pw.max(1), ph.max(1));
        self.panel_scale = scale;
        if let Some(b) = &mut self.browser {
            b.resize(pw, ph);
            b.set_scale(scale);
        }
    }

    pub fn set_theme(&mut self, br: f64, bg: f64, bb: f64, ba: f64, fr: u8, fg: u8, fb: u8) {
        self.panel.set_clear_color(br, bg, bb, ba);
        self.panel.set_text_color(fr, fg, fb, 255);
        self.input.set_clear_color(br, bg, bb, ba);
        self.input.set_text_color(fr, fg, fb, 255);
        self.theme_bg = [br, bg, bb, ba];
        self.theme_fg = [fr, fg, fb];
        if let Some(b) = &mut self.browser {
            b.set_theme(
                wgpu::Color { r: br, g: bg, b: bb, a: ba },
                glyphon::Color::rgb(fr, fg, fb),
            );
        }
    }

    pub fn set_fonts(&mut self, regular: &str, bold: &str, italic: &str, bold_italic: &str) {
        self.panel.set_fonts(regular, bold, italic, bold_italic);
        if !regular.is_empty() {
            self.input.set_font(regular);
            self.ui_font = regular.to_string();
            if let Some(b) = &mut self.browser {
                b.set_font(resolve_family(regular));
            }
        }
    }

    // --- Textures / metrics -------------------------------------------------

    pub fn panel_texture(&self) -> *mut c_void {
        if self.browsing {
            if let Some(b) = &self.browser {
                return b.raw_texture();
            }
        }
        self.panel.raw_texture()
    }
    pub fn input_texture(&self) -> *mut c_void {
        self.input.raw_texture()
    }
    pub fn content_height(&self) -> f32 {
        if self.browsing {
            if let Some(b) = &self.browser {
                return b.content_height();
            }
        }
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
            // Enter (or Send) with nothing typed: fire the next queued prompt if
            // one is waiting — the manual resume for a queue parked by an interrupt.
            if let Some(d) = &self.driver {
                if d.send_next_queued() {
                    self.scroll = 0.0;
                }
            }
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

    /// Escape: deny a pending tool-permission request, else interrupt the turn.
    /// Keeps Escape's "back out" meaning without killing a whole turn just to
    /// refuse one tool call.
    pub fn escape(&mut self) {
        if self.permission_pending() {
            self.respond_pending("reject_once");
        } else {
            self.interrupt();
        }
    }

    /// Answer the pending prompt with option `id` and drop its buttons.
    fn respond_pending(&mut self, id: &str) {
        if let Some(d) = &self.driver {
            d.respond(id);
        }
        self.pending_ids.clear();
        self.panel.set_buttons(Vec::new());
    }

    /// True when the pending prompt is a tool-permission request — recognized by
    /// its synthesized `allow_once` first option, so Question/Plan prompts (whose
    /// options aren't a symmetric allow/deny) never get answered by a bare key.
    fn permission_pending(&self) -> bool {
        self.pending_ids.first().is_some_and(|id| id == "allow_once")
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
        if self.browsing {
            let click = match &mut self.browser {
                Some(b) => b.click(x, y),
                None => Click::None,
            };
            match click {
                Click::Resume(id, title) => self.browse_open(id, title),
                Click::Toggled | Click::None => {}
            }
            return true;
        }
        if !self.pending_ids.is_empty() {
            let idx = self.panel.hit_button(x, y);
            if idx >= 0 && (idx as usize) < self.pending_ids.len() {
                let id = self.pending_ids[idx as usize].clone();
                self.respond_pending(&id);
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
        if self.browsing {
            return; // no text selection in the browser list
        }
        self.panel.selection_update(x, y);
    }
    pub fn panel_scroll_h(&mut self, x: f32, y: f32, dx: f32) -> bool {
        if self.browsing {
            return false;
        }
        self.panel.scroll_h(x, y, dx)
    }
    pub fn panel_scroll_v(&mut self, x: f32, y: f32, dy: f32) -> bool {
        if self.browsing {
            return false; // the host scrolls the whole list via set_scroll
        }
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
    /// goes to the editor. While browsing, Enter opens the top listed session
    /// instead of sending anything.
    pub fn input_key(&mut self, name: &str, ctrl: bool, alt: bool, shift: bool) {
        if name == "Return" && !shift {
            if self.browsing {
                if let Some((id, title)) = self.browser.as_ref().and_then(|b| b.first()) {
                    self.browse_open(id, title);
                }
                return;
            }
            // Enter on an empty composer answers a pending permission (Allow);
            // with text it keeps its send/queue meaning, so a follow-up typed
            // while the prompt is up can't be swallowed as an approval.
            if self.permission_pending() && self.input.text().trim().is_empty() {
                self.respond_pending("allow_once");
                return;
            }
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
    /// The tab/header title: Claude Code's generated title (matching the session
    /// picker), falling back to the first user line until that's known.
    pub fn title(&mut self) -> &CString {
        let title = if !self.ai_title.is_empty() {
            self.ai_title.clone()
        } else {
            let t = self.driver.as_ref().map(|d| d.transcript()).unwrap_or_default();
            first_user_line(&t)
        };
        self.title_snap = clean(title);
        &self.title_snap
    }
}

/// Resolve the host's font string (a file path or a family name) to the family
/// name the browser's shaper can address; loads the file into the shared
/// FontSystem when needed (same policy as the panel's font loading).
fn resolve_family(font: &str) -> Option<String> {
    if font.is_empty() {
        return None;
    }
    if !crate::gpu::is_font_path(font) {
        return Some(font.to_string());
    }
    let mut guard = crate::gpu::lock_font_system();
    let db = guard.db_mut();
    if db.load_font_file(font).is_err() {
        return None;
    }
    db.faces()
        .last()
        .and_then(|f| f.families.first())
        .map(|(name, _)| name.clone())
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
