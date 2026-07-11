//! Drives the user's native `claude` binary in `stream-json` mode and speaks
//! Claude Code's **control protocol** directly (no Node, no port), giving
//! in-process MCP and host-driven permission prompts.
//!
//! Wire format (reverse-engineered from `@anthropic-ai/claude-agent-sdk`,
//! verified against claude 2.1.183 / SDK 0.3.183 — internal & undocumented, so
//! re-verify on upgrades):
//! - CLI: `claude --output-format stream-json --verbose --input-format stream-json
//!   --permission-prompt-tool stdio [--resume <id>]`. The `--permission-prompt-tool
//!   stdio` flag is what makes the engine route tool permissions to us as
//!   `can_use_tool` control_requests.
//! - All messages are newline-delimited JSON on the child's stdio.
//! - Host→engine startup: `control_request{request_id, request:{subtype:"initialize",
//!   sdkMcpServers:["unity"], hooks:{}}}`. Listing the SDK MCP server name routes
//!   that server's calls back to us via `mcp_message`.
//! - Engine→host `control_request`:
//!   - `can_use_tool` `{tool_name, input, title, ...}` → reply a `PermissionResult`
//!     (`{behavior:"allow", updatedInput}` / `{behavior:"deny", message}`).
//!   - `mcp_message` `{server_name, message:<MCP JSON-RPC>}` → dispatch to our
//!     in-process server and reply `{mcp_response:<JSON-RPC response>}`.
//! - Reply envelope: `control_response{response:{subtype:"success"|"error",
//!   request_id, response|error}}`. Replies to *our* requests come back the same way.
//! - Stream (non-control) events: `system/init` (carries `session_id`, `model`),
//!   `assistant` (Anthropic message), `user` (tool results), `result` (turn end).
//! - Prompt input line: `{"type":"user","session_id":"","message":{"role":"user",
//!   "content":[{"type":"text","text":...}]},"parent_tool_use_id":null}`.
//!
//! A single reader thread pumps the child's stdout and never blocks: blocking work
//! (`mcp_message` → Unity tool call) is handed to short-lived helper threads, and
//! permission prompts are published for the UI and answered later via [`Driver::respond`].

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::mcp::McpDispatcher;
use crate::LockRecover;

/// Transcript field separators, mirrored by the C# parser: role-tagged blocks
/// `role{US}body` joined by `{RS}`. (ASCII record/unit separators.)
pub const RS: char = '\u{1e}';
pub const US: char = '\u{1f}';

static NEXT_REQ: AtomicU64 = AtomicU64::new(1);

// ===========================================================================
// Conversation model: serialized into the role-tagged transcript the renderer
// parses (`role{US}body` blocks joined by `{RS}`). Roles: 'u' user, 'a' agent,
// 't' thought, 'x' tool. Shared by the live reader and resume reconstruction.
// ===========================================================================

pub struct Conv {
    blocks: Vec<(char, String)>,
    /// Unix-seconds creation stamp per block, parallel to `blocks` (0 = unknown).
    /// `serialize` marks a lull ≥ [`TIME_GAP_SECS`] between consecutive stamps
    /// with a relative-time separator ("5 minutes ago"), chat-app style.
    stamps: Vec<u64>,
    /// While reconstructing a saved session, the record's own timestamp — so
    /// historical blocks get historical stamps. `None` = live, stamp with now.
    clock: Option<u64>,
    tools: HashMap<String, ToolEntry>, // toolUseId -> rendered tool block
    // tool_use ids we render via custom UI (ExitPlanMode / AskUserQuestion), so their
    // raw "▸ ToolName" line is suppressed in the transcript (incl. their tool_result).
    hidden_tools: HashSet<String>,
}

/// A quiet stretch this long between blocks gets a timestamp separator.
const TIME_GAP_SECS: u64 = 5 * 60;

/// While a separator is younger than this, its "N minutes ago" label changes each
/// minute, so the panel force-repaints on minute ticks to keep it live. Beyond it
/// the label moves by the hour at most, not worth waking an idle transcript for.
const RELATIVE_TICK_SECS: u64 = 60 * 60;

/// A tool call's rendered state. `idx` is the block it owns; `title` is the tool
/// name; `input`/`output` are the (sanitized) call arguments and result text, kept
/// so the panel can fold/unfold the tool's content. Serialized into the block body
/// as `<id>{US}<header>{US}<detail>` (see [`Conv::rebuild_tool`]).
struct ToolEntry {
    idx: usize,
    title: String,
    glyph: &'static str,
    input: String,
    output: String,
}

impl Conv {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            stamps: Vec::new(),
            clock: None,
            tools: HashMap::new(),
            hidden_tools: HashSet::new(),
        }
    }

    /// The creation stamp for a block appended right now: the reconstruction
    /// clock while replaying a saved session, else the wall clock.
    fn stamp_now(&self) -> u64 {
        self.clock.unwrap_or_else(crate::clock::now_secs)
    }

    /// The one place a block enters `blocks`, so `stamps` can never fall out of
    /// lockstep with it.
    fn push_block(&mut self, role: char, text: String) {
        let stamp = self.stamp_now();
        self.blocks.push((role, text));
        self.stamps.push(stamp);
    }

    pub fn push_user(&mut self, text: &str) {
        // Slash commands run through the CLI are recorded as synthetic user messages
        // wrapped in `<command-*>` / `<local-command-*>` tags (seen on resume replay;
        // live user events skip text via `apply_tool_results`, so no double-render).
        // Show them cleanly instead of as raw tags.
        let t = text.trim();
        // Boilerplate the CLI wraps output in, plus harness task-completion pings —
        // pure noise, drop them. (The caveat is also `isMeta` and already dropped in
        // `reconstruct_transcript`; kept here as a belt-and-suspenders guard.)
        if t.starts_with("<local-command-caveat>") || t.starts_with("<task-notification>") {
            return;
        }
        // A slash-command invocation → "/name args".
        if let Some(name) = tag_inner(t, "command-name") {
            let args = tag_inner(t, "command-args").unwrap_or_default();
            let (name, args) = (name.trim(), args.trim());
            let line = if args.is_empty() { name.to_string() } else { format!("{name} {args}") };
            self.push_block('u', line);
            return;
        }
        // A command's stdout → a plain result line (agent role: no user bubble).
        if let Some(out) = tag_inner(t, "local-command-stdout").or_else(|| tag_inner(t, "local-command-stderr")) {
            let out = out.trim();
            if !out.is_empty() {
                self.push_block('a', out.to_string());
            }
            return;
        }
        self.push_block('u', text.to_string());
    }

    /// A short marker standing in for a compaction summary (the verbose summary is
    /// still in the engine's real context; the transcript just notes the boundary).
    fn push_compact_marker(&mut self) {
        self.push_block('a', "[continued from a compacted conversation]".to_string());
    }

    /// Append a queued (not-yet-sent) user prompt, kept as a dimmed `'q'` block in
    /// the transcript until the running turn finishes and it is promoted/sent.
    pub fn push_queued(&mut self, text: &str) {
        self.push_block('q', text.to_string());
    }

    /// Promote the oldest queued prompt to a real user turn, returning its text
    /// (None if nothing is queued).
    fn promote_first_queued(&mut self) -> Option<String> {
        let now = self.stamp_now();
        for (i, b) in self.blocks.iter_mut().enumerate() {
            if b.0 == 'q' {
                b.0 = 'u';
                // It is *sent* now — restamp so the gap separator reflects when
                // the turn actually ran, not when it was queued.
                if let Some(s) = self.stamps.get_mut(i) {
                    *s = now;
                }
                return Some(b.1.clone());
            }
        }
        None
    }

    fn queued_count(&self) -> usize {
        self.blocks.iter().filter(|b| b.0 == 'q').count()
    }

    /// Drop the `index`-th queued prompt (0-based among queued blocks only).
    fn cancel_queued(&mut self, index: usize) {
        let mut seen = 0;
        let mut target = None;
        for (i, b) in self.blocks.iter().enumerate() {
            if b.0 == 'q' {
                if seen == index {
                    target = Some(i);
                    break;
                }
                seen += 1;
            }
        }
        if let Some(i) = target {
            self.remove_block(i);
        }
    }

    /// Remove block `i`, keeping the tool-id → block-index map consistent.
    fn remove_block(&mut self, i: usize) {
        if i >= self.blocks.len() {
            return;
        }
        self.blocks.remove(i);
        if i < self.stamps.len() {
            self.stamps.remove(i);
        }
        for e in self.tools.values_mut() {
            if e.idx > i {
                e.idx -= 1;
            }
        }
    }

    fn append_role(&mut self, role: char, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some((r, t)) = self.blocks.last_mut() {
            if *r == role {
                // Streaming continuation: the block keeps its start stamp.
                t.push_str(s);
                return;
            }
        }
        self.push_block(role, s.to_string());
    }

    /// Record a `tool_use`: set/refresh the title + input summary and mark the call
    /// in-progress (`▸`). Creates the block on first sight, else updates in place.
    fn tool_begin(&mut self, id: &str, name: &str, input: &str) {
        let input = sanitize(input);
        if let Some(e) = self.tools.get_mut(id) {
            if !name.is_empty() {
                e.title = name.to_string();
            }
            e.input = input;
            e.glyph = "▸";
        } else {
            let idx = self.blocks.len();
            self.push_block('x', String::new());
            let title = if name.is_empty() { "(tool)".to_string() } else { name.to_string() };
            self.tools.insert(
                id.to_string(),
                ToolEntry { idx, title, glyph: "▸", input, output: String::new() },
            );
        }
        self.rebuild_tool(id);
    }

    /// Record a `tool_result`: flip the status glyph (`✓`/`✗`) and store the output.
    fn tool_end(&mut self, id: &str, status: &str, output: &str) {
        let output = sanitize(output);
        let glyph = glyph_for(status);
        if let Some(e) = self.tools.get_mut(id) {
            e.glyph = glyph;
            e.output = output;
        } else {
            // A result with no preceding tool_use (shouldn't happen): stub a block.
            let idx = self.blocks.len();
            self.push_block('x', String::new());
            self.tools.insert(
                id.to_string(),
                ToolEntry { idx, title: "(tool)".to_string(), glyph, input: String::new(), output },
            );
        }
        self.rebuild_tool(id);
    }

    /// Re-encode a tool's block body as `<id>{US}<header>{US}<preview>{US}<detail>`,
    /// where the header is `<glyph> <title>`, the preview is a short one-line input
    /// summary shown next to the header, and the detail (unfolded view) is the full
    /// input followed by the result output. The panel renders the preview/detail in
    /// a smaller font, so they're kept as separate fields.
    fn rebuild_tool(&mut self, id: &str) {
        let Some(e) = self.tools.get(id) else { return };
        let header = format!("{} {}", e.glyph, e.title);
        let preview = truncate(e.input.lines().next().unwrap_or(""), 60);
        let mut detail = String::new();
        if !e.input.is_empty() {
            detail.push_str(&e.input);
        }
        if !e.output.is_empty() {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(&e.output);
        }
        let i = e.idx;
        self.blocks[i].1 = format!("{id}{US}{header}{US}{preview}{US}{detail}");
    }

    fn note_closed(&mut self) {
        self.push_block('a', "[connection closed]".to_string());
    }

    /// Apply one Anthropic message's `content` (string or block array). Used for
    /// `assistant` stream events and for resume reconstruction of both roles.
    fn apply_message(&mut self, role: &str, content: &Value) {
        match content {
            Value::String(s) => {
                if role == "user" {
                    self.push_user(s);
                } else {
                    self.append_role('a', s);
                }
            }
            Value::Array(blocks) => {
                for b in blocks {
                    self.apply_block(role, b);
                }
            }
            _ => {}
        }
    }

    /// Apply only `tool_result` blocks from a `user` stream event (flipping tool
    /// glyphs). The user's own prompt is added locally on `send`, so live `user`
    /// events must NOT create a 'u' turn — hence we guard on block type here.
    fn apply_tool_results(&mut self, content: &Value) {
        if let Some(blocks) = content.as_array() {
            for b in blocks {
                if b["type"] == "tool_result" {
                    self.apply_block("user", b);
                }
            }
        }
    }

    fn apply_block(&mut self, role: &str, b: &Value) {
        match b["type"].as_str() {
            Some("text") => {
                let t = b["text"].as_str().unwrap_or("");
                if role == "user" {
                    self.push_user(t);
                } else {
                    self.append_role('a', t);
                }
            }
            Some("thinking") => self.append_role('t', b["thinking"].as_str().unwrap_or("")),
            Some("tool_use") => {
                let id = b["id"].as_str().unwrap_or("");
                let name = b["name"].as_str().unwrap_or("");
                // Tools we surface through custom UI (plan approval / question
                // prompts) shouldn't also show a raw "▸ ToolName" line.
                if name == "ExitPlanMode" || name == "AskUserQuestion" {
                    if !id.is_empty() {
                        self.hidden_tools.insert(id.to_string());
                    }
                } else {
                    self.tool_begin(id, name, &describe_tool(&b["input"]));
                }
            }
            Some("tool_result") => {
                let id = b["tool_use_id"].as_str().unwrap_or("");
                if self.hidden_tools.contains(id) {
                    return;
                }
                let status = if b["is_error"].as_bool().unwrap_or(false) {
                    "failed"
                } else {
                    "completed"
                };
                self.tool_end(id, status, &tool_result_text(&b["content"]));
            }
            _ => {}
        }
    }

    pub fn serialize(&self) -> String {
        // Mark the transcript's opening and its conversation lulls with a
        // timestamp separator (an 's' block): the first stamped block always gets
        // one (so the top shows when the conversation started), and thereafter one
        // appears whenever consecutive blocks are ≥ TIME_GAP_SECS apart — chat-app
        // style, never per line. The block body is the RAW unix stamp; the panel
        // formats it against the clock at layout time, so a now-relative "12 min
        // ago" label can age without this memoized string going stale.
        let mut out: Vec<String> = Vec::with_capacity(self.blocks.len());
        let mut prev: Option<u64> = None;
        for (i, (r, t)) in self.blocks.iter().enumerate() {
            let stamp = self.stamps.get(i).copied().unwrap_or(0);
            if stamp != 0 {
                let gap = prev.is_none_or(|p| stamp.saturating_sub(p) >= TIME_GAP_SECS);
                if gap {
                    out.push(format!("s{US}{stamp}"));
                }
                prev = Some(stamp);
            }
            out.push(format!("{r}{US}{t}"));
        }
        out.join(&RS.to_string())
    }

    /// Whether the transcript has a separator young enough that its label changes
    /// minute-to-minute ("5 minutes ago" → "6 minutes ago") — the UI forces a
    /// repaint on minute ticks while this holds, so the label follows the clock.
    /// Past the window the label only changes by the hour (or slower), so an idle
    /// transcript stops ticking and refreshes on the next interaction instead.
    fn has_relative_stamp(&self, now: u64) -> bool {
        let window = RELATIVE_TICK_SECS;
        let mut prev: Option<u64> = None;
        for i in 0..self.blocks.len() {
            let stamp = self.stamps.get(i).copied().unwrap_or(0);
            if stamp == 0 {
                continue;
            }
            // Mirror `serialize`: the first stamped block and each ≥ gap boundary
            // carry a separator.
            let gap = prev.is_none_or(|p| stamp.saturating_sub(p) >= TIME_GAP_SECS);
            if gap && now.saturating_sub(stamp) <= window {
                return true;
            }
            prev = Some(stamp);
        }
        false
    }
}

/// Rebuild a [`Conv`] from a prior session's on-disk transcript, since
/// `claude --resume` retains context but does not replay turns as stream events.
/// Globs `~/.claude/projects/*/<session-id>.jsonl` (robust vs computing the
/// encoded cwd dir) and reads it best-effort (the file may not exist yet).
///
/// Also returns the `cwd` the session recorded, so the resume can run claude in
/// that exact directory: `claude --resume` only finds a session under the project
/// dir derived from the *current* cwd, so a mismatch (e.g. Unity's forward-slash
/// project path vs the saved one on Windows) makes claude report "No conversation
/// found" and exit. The recorded cwd round-trips to the right project dir.
pub fn reconstruct_transcript(session_id: &str) -> (Conv, Option<String>) {
    let mut conv = Conv::new();
    let mut cwd: Option<String> = None;
    if session_id.is_empty() {
        return (conv, cwd);
    }
    // `HOME` on Unix; Windows GUI processes set `USERPROFILE` instead.
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default();
    let base = PathBuf::from(home).join(".claude").join("projects");
    let file_name = format!("{session_id}.jsonl");
    let Ok(dirs) = std::fs::read_dir(&base) else {
        return (conv, cwd);
    };
    for entry in dirs.flatten() {
        let path = entry.path().join(&file_name);
        if !path.is_file() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                // Records carry the cwd claude ran in; keep the first we see.
                if cwd.is_none() {
                    if let Some(c) = v["cwd"].as_str().filter(|s| !s.is_empty()) {
                        cwd = Some(c.to_string());
                    }
                }
                // Only conversational records carry a message; skip metadata
                // (queue-operation, ai-title, last-prompt, attachment, ...).
                let role = v["type"].as_str().unwrap_or("");
                if role == "user" || role == "assistant" {
                    // Replay under the record's own timestamp so historical blocks
                    // get historical stamps (0/absent = unknown, no separator).
                    let ts = v["timestamp"]
                        .as_str()
                        .map(crate::clock::parse_iso8601_secs)
                        .unwrap_or(0);
                    conv.clock = (ts != 0).then_some(ts);
                    // Live sessions render user *text* only via `send()` (incoming user
                    // events apply tool_results only), so the CLI's synthetic user turns
                    // never show live. On replay they would — filter them to match:
                    // `isMeta` marks harness-injected turns (auto "Continue…" nudges,
                    // system reminders, the caveat); a compact summary becomes a marker.
                    if role == "user" && v["isMeta"].as_bool().unwrap_or(false) {
                        continue;
                    }
                    if role == "user" && v["isCompactSummary"].as_bool().unwrap_or(false) {
                        conv.push_compact_marker();
                        continue;
                    }
                    conv.apply_message(role, &v["message"]["content"]);
                }
            }
        }
        break;
    }
    // Back to live stamping — blocks appended after the replay are "now" again.
    conv.clock = None;
    (conv, cwd)
}

// ===========================================================================
// Pending permission, shared driver state, and the driver itself.
// ===========================================================================

/// Something awaiting the user, surfaced through the same C# button UI: either a
/// tool-permission prompt (allow/deny) or an `AskUserQuestion` (the agent's own
/// question with its options). Both render as a title + a row of option buttons;
/// [`Driver::respond`] routes the click back appropriately.
enum Pending {
    /// A `can_use_tool` permission request (synthesized allow/deny options).
    Permission {
        request_id: String,
        tool_name: String,
        input: Value,
        title: String,
    },
    /// An `AskUserQuestion` tool call: the questions are presented one at a time,
    /// answers accumulate, and the whole set is returned at once. stdio mode never
    /// emits the real `request_user_dialog`, so we answer via the `can_use_tool`
    /// result — `deny` carrying the user's choices as a message the model reads.
    Question {
        request_id: String,
        questions: Vec<Question>,
        answers: Vec<(String, String)>, // (header, chosen label)
        index: usize,
    },
    /// An `ExitPlanMode` tool call: the agent presents a plan and asks to leave
    /// plan mode. Rendered as a "Ready to code?" approval (accept + pick the next
    /// permission mode, or keep planning) rather than a generic allow/deny.
    Plan {
        request_id: String,
        input: Value, // echoed back as updatedInput on approval
    },
}

struct Question {
    header: String,
    question: String,
    options: Vec<QOption>,
}

struct QOption {
    label: String,
    description: String,
}

/// State shared between the reader thread, the MCP helper threads, and the host
/// (via [`Driver`]). Everything the reader mutates lives behind a `Mutex` so the
/// UI can snapshot it from another thread without coordination.
struct State {
    /// The child's stdin. `None` after [`State::close_stdin`] signals EOF (the
    /// stream-json engine exits cleanly on stdin close); writes then no-op.
    writer: Mutex<Option<ChildStdin>>,
    /// Bumped whenever [`Conv`] changes. The UI polls this (cheap) instead of
    /// cloning + comparing the full transcript every editor tick, and the
    /// serialized text is rebuilt lazily in [`Driver::transcript`] — so a
    /// streaming turn no longer re-serializes the whole conversation on every
    /// delta, only when the UI actually reads it.
    transcript_serial: AtomicU64,
    /// Memoized serialization of `conv`, tagged with the serial it was built at.
    transcript_cache: Mutex<(u64, String)>,
    status: Mutex<String>,
    pending: Mutex<Option<Pending>>,
    session_id: Mutex<String>,
    conv: Mutex<Conv>,
    remembered: Mutex<HashMap<String, bool>>, // tool_name -> allow (session "always")
    /// Set by a user interrupt while a turn is running: the `result` that ends
    /// the aborted turn must NOT auto-send the next queued prompt (the user just
    /// said "stop" — firing a queued follow-up right after would undo that).
    /// Consumed by that `result`; cleared early when the user re-engages (`send`).
    queue_hold: AtomicBool,
    ready: AtomicBool,
    outbox: Mutex<Vec<String>>, // prompts buffered until `initialize` completes
    mcp: Option<McpDispatcher>,
    init_id: String,
    // Runtime settings driven from the UI: `permission_mode`/`model` are pushed to
    // the engine via control_requests (and re-applied once `initialize` completes,
    // for values chosen before the engine was ready). Reasoning effort is a
    // spawn-time CLI flag (see `Driver::new`), not stored here.
    permission_mode: Mutex<String>,
    model: Mutex<String>,
    /// The model roster the engine advertises in its `initialize` reply (a JSON
    /// array of `{value, displayName, description, ...}`), captured verbatim so the
    /// UI can build its picker from what the account is actually entitled to — Fable,
    /// 1M variants, etc. — instead of a hardcoded list. Empty until initialized.
    models: Mutex<String>,
    /// The slash-command roster the engine advertises in its `initialize` reply (a
    /// JSON array of `{name, description, argumentHint, aliases?}`), captured verbatim
    /// so the composer can offer `/` completion. Empty until initialized.
    commands: Mutex<String>,
}

impl State {
    fn write_value(&self, v: &Value) {
        // Serialize before locking; keep the critical section to one line + flush.
        let line = format!("{v}\n");
        if let Some(w) = self.writer.lock_recover().as_mut() {
            let _ = w.write_all(line.as_bytes()).and_then(|_| w.flush());
        }
    }

    fn write_line(&self, line: &str) {
        if let Some(w) = self.writer.lock_recover().as_mut() {
            let _ = w
                .write_all(line.as_bytes())
                .and_then(|_| w.write_all(b"\n"))
                .and_then(|_| w.flush());
        }
    }

    /// Close the child's stdin (drop it) so the stream-json engine sees EOF and
    /// shuts down cleanly — the protocol-level "done", used before any signal.
    fn close_stdin(&self) {
        let _ = self.writer.lock_recover().take();
    }

    /// Mark the transcript changed. Serialization is deferred to
    /// [`Driver::transcript`], so a burst of streaming deltas costs one bump each
    /// rather than a full re-serialization of the conversation.
    fn sync_transcript(&self) {
        self.transcript_serial.fetch_add(1, Ordering::Release);
    }

    fn set_status(&self, s: &str) {
        *self.status.lock_recover() = s.to_string();
    }

    /// Reply to a `can_use_tool` request with the user's allow/deny decision.
    fn write_permission(&self, request_id: &str, allow: bool, input: &Value) {
        let result = if allow {
            json!({ "behavior": "allow", "updatedInput": input })
        } else {
            json!({ "behavior": "deny", "message": "User denied the request." })
        };
        self.write_value(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": result }
        }));
    }

    fn write_control_error(&self, request_id: &str, message: &str) {
        self.write_value(&json!({
            "type": "control_response",
            "response": { "subtype": "error", "request_id": request_id, "error": message }
        }));
    }

    /// Send a host→engine `control_request` carrying a single `{key: value}` field
    /// (e.g. `set_model`/`set_permission_mode`). Fire-and-forget: the engine's
    /// success reply is ignored, like `interrupt`.
    fn send_control(&self, subtype: &str, key: &str, value: &str) {
        let id = format!("unterm-ctl-{}", NEXT_REQ.fetch_add(1, Ordering::Relaxed));
        self.write_value(&json!({
            "type": "control_request",
            "request_id": id,
            "request": { "subtype": subtype, key: value }
        }));
    }

}

/// A live control-protocol session: the spawned `claude` child plus its reader
/// thread. The host calls [`Driver::send`]/[`Driver::respond`] and polls the
/// snapshot accessors; all streaming happens on the reader thread.
pub struct Driver {
    state: Arc<State>,
    child: Option<Child>,
}

impl Driver {
    /// Spawn `claude` rooted at `cwd`, send `initialize`, and start the reader.
    /// `seed` pre-populates the transcript (resume reconstruction); pass an empty
    /// [`Conv`] for a fresh session.
    pub fn new(
        cwd: String,
        mcp: Option<McpDispatcher>,
        resume: Option<String>,
        seed: Conv,
        effort: String,
        claude_cmd: String,
    ) -> std::io::Result<Self> {
        // The host (see ClaudeCode) resolves `claude` to its managed install and
        // passes that absolute path in. We deliberately never fall back to a bare
        // `claude` off the user's PATH: Unterm drives only the engine it manages, so
        // a missing command is a hard error, not a silent system-`claude` spawn.
        if claude_cmd.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no managed claude binary — download it in Preferences > Unterm",
            ));
        }
        let cmd = claude_cmd;
        let mut args: Vec<String> = "--output-format stream-json --verbose \
             --input-format stream-json --permission-prompt-tool stdio"
            .split_whitespace()
            .map(String::from)
            .collect();
        if let Some(id) = resume.as_deref().filter(|s| !s.is_empty()) {
            args.push("--resume".into());
            args.push(id.to_string());
        }
        // Reasoning effort (none/low/medium/high/max). Empty/"default" = don't
        // override, so the model's own default applies (e.g. high on 4.x). This is
        // a spawn-time flag (no runtime control), so the host respawns to change it.
        if !effort.is_empty() && effort != "default" {
            args.push("--effort".into());
            args.push(effort);
        }
        let workdir: std::path::PathBuf = if cwd.is_empty() { ".".into() } else { cwd.into() };
        // Ground the agent in its host up front: it runs embedded in the Unity
        // Editor, so name the editor version / project and point it at the
        // unterm-unity MCP tools. Spawn-time only — live editor state (scene,
        // play mode, selection) is pulled via the unity_editor tool instead.
        if let Some(ctx) = unity_context(&workdir) {
            args.push("--append-system-prompt".into());
            args.push(ctx);
        }
        // The exact cwd matters on resume: `claude --resume` only finds a session
        // under the project dir derived from this directory, so log it to verify.
        log::info!(
            "claude spawn: cmd={cmd:?} resume={:?} cwd={}",
            resume.as_deref().unwrap_or(""),
            workdir.display()
        );

        // Unity launched from the GUI inherits a minimal environment, so resolve
        // `claude` the way it can be found on each OS.
        let mut command = if std::path::Path::new(&cmd).is_absolute() {
            // A known absolute path (e.g. a native install at ~/.local/bin/claude):
            // exec it directly — no shell, no rc, no PATH lookup, same on every OS.
            let mut c = Command::new(&cmd);
            c.args(&args);
            c
        } else {
            // Bare command name: resolve it on PATH the way a real terminal would.
            #[cfg(windows)]
            {
                // Windows GUI processes inherit the full user PATH; go through
                // cmd.exe so an npm `.cmd` shim resolves too, and suppress the
                // console window.
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                // Quote whitespace-carrying args (the system prompt) so cmd.exe
                // keeps each one a single argument.
                let joined = args
                    .iter()
                    .map(|a| {
                        if a.chars().any(char::is_whitespace) {
                            format!("\"{a}\"")
                        } else {
                            a.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut c = Command::new("cmd");
                c.args(["/c", &format!("{cmd} {joined}")])
                    .creation_flags(CREATE_NO_WINDOW);
                c
            }
            #[cfg(not(windows))]
            {
                // The login+interactive shell sources the user's rc so `claude`
                // resolves despite the minimal GUI PATH. Single-quote
                // whitespace-carrying args (the system prompt) so the shell
                // keeps each one a single argument.
                let joined = args
                    .iter()
                    .map(|a| {
                        if a.chars().any(char::is_whitespace) {
                            format!("'{}'", a.replace('\'', r"'\''"))
                        } else {
                            a.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
                let mut c = Command::new(shell);
                c.args(["-lic", &format!("exec {cmd} {joined}")]);
                c
            }
        };
        let mut child = command
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Capture (don't inherit) stderr: a GUI Unity on Windows has no console
            // for an inherited handle, so claude's own errors — "No conversation
            // found", MCP/auth failures, crashes — would vanish. Surface each line
            // through the log (which the editor mirrors) so they're diagnosable.
            .stderr(Stdio::piped())
            .spawn()?;

        let writer = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        log::warn!("claude stderr: {line}");
                    }
                }
            });
        }
        let init_id = format!("unterm-init-{}", NEXT_REQ.fetch_add(1, Ordering::Relaxed));

        let state = Arc::new(State {
            writer: Mutex::new(Some(writer)),
            // Serial 1 with a serial-0 cache: the first transcript() read
            // serializes the seed lazily.
            transcript_serial: AtomicU64::new(1),
            transcript_cache: Mutex::new((0, String::new())),
            status: Mutex::new("initializing".to_string()),
            pending: Mutex::new(None),
            session_id: Mutex::new(String::new()),
            conv: Mutex::new(seed),
            remembered: Mutex::new(HashMap::new()),
            queue_hold: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            outbox: Mutex::new(Vec::new()),
            mcp,
            init_id: init_id.clone(),
            permission_mode: Mutex::new("default".to_string()),
            model: Mutex::new(String::new()),
            models: Mutex::new(String::new()),
            commands: Mutex::new(String::new()),
        });

        // Declare our in-process MCP server so the engine routes its calls to us.
        state.write_value(&json!({
            "type": "control_request",
            "request_id": init_id,
            "request": { "subtype": "initialize", "sdkMcpServers": ["unity"], "hooks": {} }
        }));

        let reader_state = state.clone();
        std::thread::spawn(move || reader_main(reader_state, stdout));

        Ok(Self {
            state,
            child: Some(child),
        })
    }

    /// Send a user prompt, or queue it if a turn is already running.
    ///
    /// - A turn is in flight (`ready` and status `thinking`): hold it as a dimmed
    ///   queued block; the reader sends it as its own turn when the current one
    ///   ends (see the `result` handler). This is the follow-up input queue.
    /// - Otherwise write immediately once `initialize` has completed, else buffer
    ///   in `outbox` until then (the engine must be initialized first).
    pub fn send(&self, prompt: &str) {
        // A dead session can't accept a turn: `init failed` (most often: not
        // signed in) is terminal, and `closed` means the child is gone. Don't
        // flip to "thinking" and buffer a line that would never be flushed (the
        // outbox only drains on a *successful* init). The panel surfaces a
        // "not signed in" note, and `/login` is intercepted by the host.
        // (`initializing` is transient and still buffers — handled below.)
        {
            let status = self.state.status.lock_recover();
            if status.starts_with("init failed") || *status == "closed" {
                return;
            }
        }
        // Sending anything is re-engaging: a queue parked by an interrupt may
        // drain again once the next turn ends.
        self.state.queue_hold.store(false, Ordering::Relaxed);
        if self.state.ready.load(Ordering::Relaxed)
            && *self.state.status.lock_recover() == "thinking"
        {
            self.state.conv.lock_recover().push_queued(prompt);
            self.state.sync_transcript();
            return;
        }
        {
            let mut c = self.state.conv.lock_recover();
            c.push_user(prompt);
        }
        self.state.sync_transcript();
        self.state.set_status("thinking");
        let line = user_line(prompt);
        if self.state.ready.load(Ordering::Relaxed) {
            self.state.write_line(&line);
        } else {
            self.state.outbox.lock_recover().push(line);
        }
    }

    /// Answer the pending prompt with the clicked option id. For a permission
    /// prompt that's `allow_once`/`allow_always`/`reject_once`/`reject_always`;
    /// for a question it's the chosen option label (or `__skip__`).
    pub fn respond(&self, option_id: &str) {
        let Some(p) = self.state.pending.lock_recover().take() else {
            return;
        };
        match p {
            Pending::Permission {
                request_id,
                tool_name,
                input,
                ..
            } => {
                let allow = option_id.starts_with("allow");
                if option_id.ends_with("always") {
                    self.state.remembered.lock_recover().insert(tool_name, allow);
                }
                self.state.write_permission(&request_id, allow, &input);
            }
            Pending::Question {
                request_id,
                questions,
                mut answers,
                index,
            } => {
                let header = questions
                    .get(index)
                    .map(|q| q.header.clone())
                    .unwrap_or_default();
                let label = if option_id == "__skip__" {
                    "(skipped)".to_string()
                } else {
                    option_id.to_string()
                };
                answers.push((header, label));
                let next = index + 1;
                if next < questions.len() {
                    // More questions: re-arm the prompt with the next one.
                    *self.state.pending.lock_recover() = Some(Pending::Question {
                        request_id,
                        questions,
                        answers,
                        index: next,
                    });
                } else {
                    // All answered: return the choices to the model. stdio gives no
                    // dialog channel, so we deny the tool with the answers as the
                    // message the model reads and acts on.
                    let mut msg = String::from("The user answered your question(s):\n");
                    for (h, a) in &answers {
                        if h.is_empty() {
                            msg.push_str(&format!("- {a}\n"));
                        } else {
                            msg.push_str(&format!("- {h}: {a}\n"));
                        }
                    }
                    msg.push_str("Proceed with these answers.");
                    self.state.write_value(&json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": { "behavior": "deny", "message": msg }
                        }
                    }));
                }
            }
            Pending::Plan { request_id, input } => match option_id {
                // Approve: let ExitPlanMode run (engine leaves plan mode), then set
                // the permission mode the user picked for coding.
                "plan_accept" => {
                    self.state.write_permission(&request_id, true, &input);
                    self.set_permission_mode("acceptEdits");
                }
                "plan_default" => {
                    self.state.write_permission(&request_id, true, &input);
                    self.set_permission_mode("default");
                }
                // Keep planning: deny so the agent revises and re-presents the plan.
                _ => self.state.write_value(&json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {
                            "behavior": "deny",
                            "message": "The user wants to keep planning. Revise the plan based on any feedback and call ExitPlanMode again when ready."
                        }
                    }
                })),
            },
        }
    }

    /// Interrupt the in-flight turn (control_request `interrupt`). Fire-and-forget:
    /// the engine aborts and emits a `result`, which flips status back to ready.
    pub fn interrupt(&self) {
        if !self.state.ready.load(Ordering::Relaxed) {
            return;
        }
        // Park the follow-up queue: the user asked the agent to stop, so the
        // aborted turn's `result` must not immediately fire the next queued
        // prompt. Only when a turn is actually running — an idle interrupt has
        // no matching `result` to consume the flag.
        if *self.state.status.lock_recover() == "thinking" {
            self.state.queue_hold.store(true, Ordering::Relaxed);
        }
        let id = format!("unterm-int-{}", NEXT_REQ.fetch_add(1, Ordering::Relaxed));
        self.state.write_value(&json!({
            "type": "control_request",
            "request_id": id,
            "request": { "subtype": "interrupt" }
        }));
    }

    /// Set the permission mode (`default`/`plan`/`acceptEdits`/`bypassPermissions`).
    /// Stored, and pushed to the engine now if ready (else applied on init).
    pub fn set_permission_mode(&self, mode: &str) {
        *self.state.permission_mode.lock_recover() = mode.to_string();
        if self.state.ready.load(Ordering::Relaxed) {
            self.state.send_control("set_permission_mode", "mode", mode);
        }
    }
    pub fn permission_mode(&self) -> String {
        self.state.permission_mode.lock_recover().clone()
    }

    /// Set the model (alias like `opus`/`sonnet`/`haiku`, or empty/`default` to
    /// keep the engine default). Stored, and pushed now if ready (else on init).
    pub fn set_model(&self, model: &str) {
        *self.state.model.lock_recover() = model.to_string();
        // The engine rejects an empty model ("String should have at least 1
        // character"); empty/`default` just means "keep the engine default".
        if self.state.ready.load(Ordering::Relaxed) && !model.is_empty() && model != "default" {
            self.state.send_control("set_model", "model", model);
        }
    }
    /// The active model: a user choice, else the resolved model from `system/init`.
    pub fn model(&self) -> String {
        self.state.model.lock_recover().clone()
    }
    /// The model roster from the `initialize` reply (JSON array string; empty until
    /// the engine is ready). Each entry: `{value, displayName, description, ...}`.
    pub fn models(&self) -> String {
        self.state.models.lock_recover().clone()
    }
    /// The slash-command roster from the `initialize` reply (JSON array string; empty
    /// until ready). Each entry: `{name, description, argumentHint, aliases?}`.
    pub fn commands(&self) -> String {
        self.state.commands.lock_recover().clone()
    }

    /// Number of prompts waiting in the follow-up queue.
    pub fn queue_len(&self) -> u32 {
        self.state.conv.lock_recover().queued_count() as u32
    }
    /// Cancel the `index`-th queued prompt (0-based among queued blocks).
    pub fn cancel_queued(&self, index: u32) {
        {
            self.state.conv.lock_recover().cancel_queued(index as usize);
        }
        self.state.sync_transcript();
    }

    /// Send the oldest queued prompt now, if idle. The manual resume for a queue
    /// parked by an interrupt (Enter on the empty composer / the Send button).
    /// Returns false when a turn is running or nothing is queued.
    pub fn send_next_queued(&self) -> bool {
        if !self.state.ready.load(Ordering::Relaxed)
            || *self.state.status.lock_recover() != "ready"
        {
            return false;
        }
        let Some(text) = self.state.conv.lock_recover().promote_first_queued() else {
            return false;
        };
        self.state.sync_transcript();
        self.state.set_status("thinking");
        self.state.write_line(&user_line(&text));
        true
    }

    /// The serialized transcript, rebuilt from [`Conv`] only when it changed
    /// since the last read (memoized against [`State::transcript_serial`]).
    pub fn transcript(&self) -> String {
        let serial = self.state.transcript_serial.load(Ordering::Acquire);
        let mut cache = self.state.transcript_cache.lock_recover();
        if cache.0 != serial {
            // A bump between the load and the serialize below just means the next
            // read re-serializes once more — never a stale result.
            cache.1 = self.state.conv.lock_recover().serialize();
            cache.0 = serial;
        }
        cache.1.clone()
    }

    /// Monotonic change counter for the transcript — lets the UI detect "did
    /// anything change?" per tick without cloning the text (see `AgentView::poll`).
    pub fn transcript_serial(&self) -> u64 {
        self.state.transcript_serial.load(Ordering::Acquire)
    }

    /// Whether a now-relative time separator is on screen (see
    /// [`Conv::has_relative_stamp`]); drives the panel's minute-tick repaint.
    pub fn has_relative_stamp(&self, now: u64) -> bool {
        self.state.conv.lock_recover().has_relative_stamp(now)
    }


    pub fn status(&self) -> String {
        self.state.status.lock_recover().clone()
    }

    pub fn session_id(&self) -> String {
        self.state.session_id.lock_recover().clone()
    }

    /// The pending prompt as `(title, options)` where each option is
    /// `(id, name, kind)`, or None. The C# UI renders the title as a note and the
    /// options as buttons; a click calls back into [`Driver::respond`] with the id.
    pub fn pending_view(&self) -> Option<(String, Vec<(String, String, String)>)> {
        let guard = self.state.pending.lock_recover();
        match guard.as_ref()? {
            Pending::Permission { title, .. } => {
                // Just the tool name: the command is already shown (and expandable) in
                // the tool block right above, so repeating it here reads as a duplicate.
                let body = format!("Permission requested: {title}");
                let opts = [
                    ("allow_once", "Allow"),
                    ("allow_always", "Always allow"),
                    ("reject_once", "Deny"),
                    ("reject_always", "Always deny"),
                ]
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string(), id.to_string()))
                .collect();
                Some((body, opts))
            }
            Pending::Question {
                questions, index, ..
            } => {
                let q = &questions[*index];
                let mut title = String::new();
                if questions.len() > 1 {
                    title.push_str(&format!("Question {}/{}", index + 1, questions.len()));
                    if !q.header.is_empty() {
                        title.push_str(&format!(" — {}", q.header));
                    }
                    title.push('\n');
                } else if !q.header.is_empty() {
                    title.push_str(&format!("{}\n", q.header));
                }
                title.push_str(&q.question);
                for o in &q.options {
                    if o.description.is_empty() {
                        title.push_str(&format!("\n• {}", o.label));
                    } else {
                        title.push_str(&format!("\n• {} — {}", o.label, o.description));
                    }
                }
                let mut opts: Vec<(String, String, String)> = q
                    .options
                    .iter()
                    .map(|o| (o.label.clone(), o.label.clone(), "answer".to_string()))
                    .collect();
                opts.push(("__skip__".to_string(), "Skip".to_string(), "skip".to_string()));
                Some((title, opts))
            }
            Pending::Plan { .. } => {
                // The plan itself is rendered as a Markdown agent block (see
                // `AgentView::compose` + `pending_plan`); this note is just the prompt.
                let body = "Ready to code?".to_string();
                let opts = [
                    ("plan_accept", "Yes, and auto-accept edits"),
                    ("plan_default", "Yes, and manually approve edits"),
                    ("plan_reject", "No, keep planning"),
                ]
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string(), id.to_string()))
                .collect();
                Some((body, opts))
            }
        }
    }

    /// The pending plan's Markdown text, if the current prompt is an `ExitPlanMode`
    /// approval — so the host can render it as a Markdown block (not a plain note).
    pub fn pending_plan(&self) -> Option<String> {
        match self.state.pending.lock_recover().as_ref()? {
            Pending::Plan { input, .. } => {
                let p = input["plan"].as_str().unwrap_or("");
                (!p.is_empty()).then(|| p.to_string())
            }
            _ => None,
        }
    }

    /// Drop a pending permission without answering (used on teardown so a waiting
    /// turn doesn't keep a stale prompt up).
    pub fn clear_pending(&self) {
        *self.state.pending.lock_recover() = None;
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.clear_pending();
        // Close stdin now (EOF) so the engine begins its own clean shutdown, then
        // reap off the calling thread so a reload/switch never blocks Unity's main
        // thread. The child exiting closes stdout, so the reader exits on its own.
        self.state.close_stdin();
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || shutdown_child(&mut child));
        }
    }
}

/// Reap a `claude` child that's already been asked to stop (its stdin was closed,
/// so a stream-json engine exits on its own). Wait up to ~2s for that clean exit;
/// if it lingers, SIGTERM (Unix) then, failing that, SIGKILL. Blocking is fine —
/// this always runs on a throwaway thread.
fn shutdown_child(child: &mut Child) {
    // Grace period for the stdin-EOF clean exit.
    for _ in 0..40 {
        match child.try_wait() {
            Ok(Some(_)) => return, // exited cleanly on EOF
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    // Still alive: escalate. SIGTERM first on Unix (lets atexit handlers run),
    // then a hard kill.
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with a pid we own; it can't be reused before our
        // `wait` below reaps it.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A short, human-readable description of what a tool call will do, for the
/// permission prompt — the salient argument (Bash command, edited/read path,
/// fetched URL, …), falling back to the compact JSON of the input.
fn describe_tool(input: &Value) -> String {
    for key in ["command", "file_path", "path", "url", "pattern", "query"] {
        if let Some(s) = input[key].as_str() {
            if !s.is_empty() {
                return truncate(s, 400);
            }
        }
    }
    match input {
        Value::Null | Value::Object(_) if input.as_object().map(|m| m.is_empty()).unwrap_or(true) => {
            String::new()
        }
        _ => truncate(&input.to_string(), 400),
    }
}

/// The status glyph shown at the head of a tool block.
fn glyph_for(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "failed" => "✗",
        "in_progress" => "▸",
        _ => "·",
    }
}

/// Collapse the transcript's block/field separators (which would corrupt the
/// encoding) to spaces. Newlines are kept — the panel splits only on RS/US.
fn sanitize(s: &str) -> String {
    s.replace([RS, US], " ")
}

/// Concatenate the text of a `tool_result` `content` (a string, or an array of
/// content blocks), ignoring non-text parts (images, etc.), capped so a huge
/// result can't bloat the transcript.
fn tool_result_text(content: &Value) -> String {
    let mut out = String::new();
    match content {
        Value::String(s) => out.push_str(s),
        Value::Array(parts) => {
            for p in parts {
                if let Some(t) = p["text"].as_str() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        _ => {}
    }
    truncate(&out, 6000)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// The text between the first `<tag>` and its matching `</tag>` (None if absent).
/// Used to unwrap the CLI's synthetic `<command-*>` / `<local-command-*>` messages.
fn tag_inner(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let close = format!("</{tag}>");
    let rel = text[start..].find(&close)?;
    Some(text[start..start + rel].to_string())
}

/// Parse an `AskUserQuestion` tool input into its questions.
fn parse_questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|q| Question {
                    header: q["header"].as_str().unwrap_or("").to_string(),
                    question: q["question"].as_str().unwrap_or("").to_string(),
                    options: q["options"]
                        .as_array()
                        .map(|os| {
                            os.iter()
                                .map(|o| QOption {
                                    label: o["label"].as_str().unwrap_or("").to_string(),
                                    description: o["description"].as_str().unwrap_or("").to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Static "you are inside Unity" grounding for `--append-system-prompt`: the
/// editor version read from ProjectSettings/ProjectVersion.txt plus the project
/// folder name. `None` when `cwd` is not a Unity project root. Kept to a single
/// line without quote characters so it survives the shell-joined spawn paths
/// verbatim on every OS. Live editor state deliberately stays out — it goes
/// stale the moment the user hits Play; the agent pulls it via unity_editor.
fn unity_context(project: &std::path::Path) -> Option<String> {
    let text =
        std::fs::read_to_string(project.join("ProjectSettings").join("ProjectVersion.txt")).ok()?;
    let version = text
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .map(str::trim)
        .filter(|v| !v.is_empty())?
        .to_string();
    // The real project name is PlayerSettings.productName, not the folder name
    // (they diverge once the repo is cloned/renamed); the agent already knows the
    // folder from its cwd, so name the product. Fall back to "this project".
    let name =
        unity_product_name(project).unwrap_or_else(|| "this project".to_string());
    Some(format!(
        "You are running embedded inside the Unity Editor (Unity {version}) in the Unity project {name}; \
         the working directory is the project root. Editor operations and live editor state \
         (scenes, GameObjects, components, console logs, screenshots, in-memory C# execution) \
         are available as unterm-unity MCP tools; prefer them over shell commands for anything \
         that touches the running editor."
    ))
}

/// PlayerSettings.productName from ProjectSettings/ProjectSettings.asset — the
/// Unity project's display name (line-scanned; the field is a top-level scalar
/// that appears once). `None` when the file is missing or the name is empty.
fn unity_product_name(project: &std::path::Path) -> Option<String> {
    let text =
        std::fs::read_to_string(project.join("ProjectSettings").join("ProjectSettings.asset"))
            .ok()?;
    text.lines()
        .find_map(|l| l.trim_start().strip_prefix("productName:"))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The stream-json input line for a user prompt.
fn user_line(text: &str) -> String {
    json!({
        "type": "user",
        "session_id": "",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        "parent_tool_use_id": null
    })
    .to_string()
}

fn reader_main(state: Arc<State>, stdout: ChildStdout) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => handle_message(&state, v),
            Err(e) => log::warn!("control: bad json line: {e}"),
        }
    }
    // Child exited / stdout closed.
    log::warn!("claude stdout closed — child exited; session -> closed");
    {
        let mut c = state.conv.lock_recover();
        c.note_closed();
    }
    state.sync_transcript();
    state.set_status("closed");
}

fn handle_message(state: &Arc<State>, v: Value) {
    match v["type"].as_str() {
        Some("control_response") => {
            // The only request we send is `initialize`; once it succeeds we are
            // ready and can flush any prompts the user queued meanwhile.
            let resp = &v["response"];
            if resp["request_id"].as_str() == Some(&state.init_id) {
                if resp["subtype"].as_str() == Some("error") {
                    let msg = resp["error"].as_str().unwrap_or("initialize failed");
                    log::warn!("initialize failed: {msg}");
                    state.set_status(&format!("init failed: {msg}"));
                    return;
                }
                log::info!("initialize ok (engine ready)");
                state.ready.store(true, Ordering::Relaxed);
                // The engine advertises its model roster here — entitlement- and
                // provider-aware (Fable, 1M variants, …). Keep it verbatim so the UI
                // picker mirrors what's actually available instead of a fixed list.
                let models = &resp["response"]["models"];
                if models.is_array() {
                    *state.models.lock_recover() = models.to_string();
                }
                // Same for the slash-command roster — used for `/` completion.
                let commands = &resp["response"]["commands"];
                if commands.is_array() {
                    *state.commands.lock_recover() = commands.to_string();
                }
                // Apply settings chosen before the engine was ready (persisted
                // mode/model the host pushed onto a not-yet-initialized session).
                {
                    let mode = state.permission_mode.lock_recover().clone();
                    if !mode.is_empty() && mode != "default" {
                        state.send_control("set_permission_mode", "mode", &mode);
                    }
                    let model = state.model.lock_recover().clone();
                    if !model.is_empty() && model != "default" {
                        state.send_control("set_model", "model", &model);
                    }
                }
                let buffered = std::mem::take(&mut *state.outbox.lock_recover());
                let had_prompts = !buffered.is_empty();
                for line in buffered {
                    state.write_line(&line);
                }
                state.set_status(if had_prompts { "thinking" } else { "ready" });
            }
        }
        Some("control_request") => handle_control_request(state, &v),
        Some("system") => {
            if v["subtype"] == "init" {
                // What claude reports as its connected MCP servers — tells us at a
                // glance whether the "unity" server came up on this platform.
                log::info!("system/init mcp_servers={}", v["mcp_servers"]);
                if let Some(sid) = v["session_id"].as_str() {
                    if !sid.is_empty() {
                        *state.session_id.lock_recover() = sid.to_string();
                    }
                }
                // Record the resolved model for display, unless the user already
                // chose one (don't clobber an explicit selection).
                if let Some(m) = v["model"].as_str() {
                    if !m.is_empty() {
                        let mut cur = state.model.lock_recover();
                        if cur.is_empty() {
                            *cur = m.to_string();
                        }
                    }
                }
            }
        }
        Some("assistant") => {
            {
                let mut c = state.conv.lock_recover();
                c.apply_message("assistant", &v["message"]["content"]);
            }
            // Capture the resolved model for display (every assistant message
            // carries it), unless the user pinned one (don't clobber a choice).
            if let Some(m) = v["message"]["model"].as_str() {
                if !m.is_empty() {
                    let mut cur = state.model.lock_recover();
                    if cur.is_empty() {
                        *cur = m.to_string();
                    }
                }
            }
            state.sync_transcript();
        }
        Some("user") => {
            {
                let mut c = state.conv.lock_recover();
                c.apply_tool_results(&v["message"]["content"]);
            }
            state.sync_transcript();
        }
        Some("result") => {
            // Turn finished. Send the next queued follow-up prompt as its own turn,
            // else go idle — except when this `result` ends a turn the user just
            // interrupted (`queue_hold`): then the queue stays parked, dimmed in
            // the transcript, until the user sends something or fires it with
            // Enter on the empty composer (`Driver::send_next_queued`).
            let held = state.queue_hold.swap(false, Ordering::Relaxed);
            let next = if held {
                None
            } else {
                state.conv.lock_recover().promote_first_queued()
            };
            if let Some(text) = next {
                state.sync_transcript();
                let line = user_line(&text);
                state.write_line(&line);
                state.set_status("thinking");
            } else {
                state.set_status("ready");
            }
        }
        // keep_alive, control_cancel_request, transcript_mirror, etc.: ignore.
        _ => {}
    }
}

fn handle_control_request(state: &Arc<State>, v: &Value) {
    let request_id = v["request_id"].as_str().unwrap_or("").to_string();
    let req = &v["request"];
    match req["subtype"].as_str() {
        Some("can_use_tool") => {
            let tool_name = req["tool_name"].as_str().unwrap_or("").to_string();
            let input = req["input"].clone();

            // AskUserQuestion is the agent asking *us*: present the questions and
            // answer via the result, rather than a yes/no permission.
            if tool_name == "AskUserQuestion" {
                let questions = parse_questions(&input);
                if questions.is_empty() {
                    state.write_permission(&request_id, true, &input);
                } else {
                    *state.pending.lock_recover() = Some(Pending::Question {
                        request_id,
                        questions,
                        answers: Vec::new(),
                        index: 0,
                    });
                }
                return;
            }

            // ExitPlanMode presents a plan and asks to leave plan mode: show a
            // "Ready to code?" approval (accept → also set the next permission mode)
            // rather than a generic allow/deny.
            if tool_name == "ExitPlanMode" {
                *state.pending.lock_recover() = Some(Pending::Plan {
                    request_id,
                    input,
                });
                return;
            }

            let title = req["title"]
                .as_str()
                .or_else(|| req["display_name"].as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&tool_name)
                .to_string();
            // Auto-honor a remembered ("always") decision without bothering the UI.
            let remembered = state.remembered.lock_recover().get(&tool_name).copied();
            if let Some(allow) = remembered {
                state.write_permission(&request_id, allow, &input);
            } else {
                *state.pending.lock_recover() = Some(Pending::Permission {
                    request_id,
                    tool_name,
                    input,
                    title,
                });
            }
        }
        Some("mcp_message") => {
            // Whether claude actually engages our in-process SDK MCP server
            // ("unity") is the key signal when tools don't show up — log each call.
            log::info!(
                "mcp_message server={:?} method={:?}",
                req["server_name"].as_str().unwrap_or(""),
                req["message"]["method"].as_str().unwrap_or("")
            );
            if req["server_name"].as_str() != Some("unity") {
                state.write_control_error(&request_id, "unknown MCP server");
                return;
            }
            let Some(mcp) = state.mcp.clone() else {
                state.write_control_error(&request_id, "MCP unavailable");
                return;
            };
            let message = req["message"].clone();
            let st = state.clone();
            // `tools/call` blocks on the Unity side — keep it off the reader thread.
            std::thread::spawn(move || {
                let mcp_response = mcp
                    .dispatch(&message)
                    .unwrap_or_else(|| json!({ "jsonrpc": "2.0", "result": {}, "id": 0 }));
                st.write_value(&json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": { "mcp_response": mcp_response }
                    }
                }));
            });
        }
        Some("request_user_dialog") => {
            // We answer AskUserQuestion via the `can_use_tool` result, and stdio
            // mode doesn't emit other dialogs; decline cleanly so nothing hangs.
            state.write_value(&json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": { "behavior": "cancelled" }
                }
            }));
        }
        _ => state.write_control_error(&request_id, "unsupported control request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_marks_time_gaps_only() {
        let mut c = Conv::new();
        c.clock = Some(1_000_000);
        c.push_user("first");
        c.clock = Some(1_000_000 + 60); // 1 min later: no separator
        c.append_role('a', "quick reply");
        c.clock = Some(1_000_000 + 60 + TIME_GAP_SECS); // long lull: separator
        c.push_user("after lunch");
        let s = c.serialize();
        let blocks: Vec<&str> = s.split(RS).collect();
        // One separator opens the transcript; another marks the lull. The 1-min
        // reply in between carries none. → [s, first, reply, s, after lunch].
        assert_eq!(blocks.len(), 5, "opening + lull separators: {s:?}");
        // Separators carry the raw stamp; the panel formats it at layout time.
        assert_eq!(blocks[0], &format!("s{US}{}", 1_000_000), "opening: {:?}", blocks[0]);
        let lull = format!("s{US}{}", 1_000_000 + 60 + TIME_GAP_SECS);
        assert_eq!(blocks[3], &lull, "lull separator: {:?}", blocks[3]);
        // The lull is fresh relative to "now" just after it → minute ticks on;
        // past the tick window → off.
        let stamp = 1_000_000 + 60 + TIME_GAP_SECS;
        assert!(c.has_relative_stamp(stamp + 60));
        assert!(!c.has_relative_stamp(stamp + RELATIVE_TICK_SECS + 60));
    }

    #[test]
    fn serialize_skips_unknown_stamps() {
        let mut c = Conv::new();
        c.clock = Some(1_000_000);
        c.push_user("stamped");
        c.push_block('a', "unstamped".to_string());
        c.stamps[1] = 0; // unknown: never participates in gap logic
        c.clock = Some(1_000_000 + 2 * TIME_GAP_SECS);
        c.push_user("much later");
        let s = c.serialize();
        // Opening separator + one for the gap measured stamped→stamped across the
        // unknown block. → [s, stamped, unstamped, s, much later].
        assert_eq!(s.split(RS).count(), 5, "{s:?}");
    }
}
