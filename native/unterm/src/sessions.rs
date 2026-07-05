//! Async listing / search of the current project's Claude Code sessions for the
//! agent window's session picker. Reading and (especially) full-text searching
//! ~100 `.jsonl` transcripts is too slow to do on the editor's main thread, so a
//! single background worker owns it: the host submits a [`query`] (cwd + limit +
//! search text) and [`poll`]s for the JSON result by serial, never blocking.
//!
//! Latest-wins: a new query overwrites the pending slot and supersedes any older
//! result, so search-as-you-type coalesces to just the newest keystroke's work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use serde_json::{json, Value};

use crate::LockRecover;

struct Request {
    serial: u64,
    cwd: String,
    limit: usize, // 0 = all
    query: String,
}

/// Keep at most this many completed results before evicting the oldest by serial.
const RESULT_CAP: usize = 32;

struct Worker {
    serial: AtomicU64,
    /// Latest not-yet-started request per `limit` — so the two independent callers
    /// (the recent-list dropdown at `limit`=N and the browser at `limit`=0) each
    /// get their own lane and never receive each other's result, while
    /// search-as-you-type still coalesces within its lane (a newer keystroke
    /// overwrites the older pending one before the worker picks it up).
    pending: Mutex<HashMap<usize, Request>>,
    wake: Condvar,
    /// Completed results keyed by their exact serial. `poll` matches by serial, so
    /// a lane's result is never confused with another's.
    results: Mutex<HashMap<u64, Arc<Vec<Session>>>>,
}

fn worker() -> &'static Worker {
    static W: OnceLock<Worker> = OnceLock::new();
    let w = W.get_or_init(|| Worker {
        serial: AtomicU64::new(0),
        pending: Mutex::new(HashMap::new()),
        wake: Condvar::new(),
        results: Mutex::new(HashMap::new()),
    });
    // Spawn the worker thread exactly once, on first use.
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("unterm-sessions".into())
            .spawn(|| worker_loop(worker()))
            .ok();
    });
    w
}

fn worker_loop(w: &'static Worker) {
    loop {
        let req = {
            let mut pending = w.pending.lock_recover();
            let key = loop {
                if let Some(&k) = pending.keys().next() {
                    break k;
                }
                pending = w.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
            };
            pending.remove(&key).unwrap()
        };
        let rows = compute(&req);
        let mut results = w.results.lock_recover();
        results.insert(req.serial, Arc::new(rows));
        // Bounded: drop the oldest serials once we exceed the cap.
        while results.len() > RESULT_CAP {
            if let Some(&oldest) = results.keys().min() {
                results.remove(&oldest);
            }
        }
    }
}

/// Bumped whenever a session file is created or removed in the watched project
/// directory, so the host/browser knows to re-list. Starts at 0.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current session-directory generation (see [`GENERATION`]).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Start watching `cwd`'s session directory once, bumping [`GENERATION`] when a
/// `.jsonl` is created/removed/renamed (ignoring writes to existing sessions, so
/// the active conversation's own streaming doesn't trigger constant re-lists).
/// The watcher is parked in a static so it stays alive for the process.
fn ensure_watcher(cwd: &str) {
    use notify::{EventKind, RecursiveMode, Watcher};
    static WATCHER: OnceLock<Option<notify::RecommendedWatcher>> = OnceLock::new();
    let cwd = cwd.to_string();
    WATCHER.get_or_init(move || {
        let dir = project_dir(&cwd)?;
        let mut watcher = notify::recommended_watcher(|res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let structural = matches!(
                ev.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            );
            if structural
                && ev
                    .paths
                    .iter()
                    .any(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            {
                GENERATION.fetch_add(1, Ordering::Relaxed);
            }
        })
        .ok()?;
        watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
        Some(watcher)
    });
}

/// Submit a listing/search request; returns its serial for later [`poll`].
pub fn query(cwd: &str, limit: usize, query: &str) -> u64 {
    ensure_watcher(cwd);
    let w = worker();
    let serial = w.serial.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut pending = w.pending.lock_recover();
        pending.insert(
            limit,
            Request {
                serial,
                cwd: cwd.to_string(),
                limit,
                query: query.to_string(),
            },
        );
    }
    w.wake.notify_one();
    serial
}

/// The typed result for exactly `serial` if ready, else `None` (still computing,
/// or coalesced away because a newer request in its lane superseded it — the
/// caller only ever polls its own latest serial, so that's fine).
pub fn poll_rows(serial: u64) -> Option<Arc<Vec<Session>>> {
    worker().results.lock_recover().get(&serial).cloned()
}

/// [`poll_rows`] serialized for the FFI/C# caller (the recent-list dropdown).
pub fn poll(serial: u64) -> Option<String> {
    let rows = poll_rows(serial)?;
    let arr: Vec<Value> = rows
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "updated": s.updated,
                "snippet": s.snippet,
                "archived": s.archived,
            })
        })
        .collect();
    Some(Value::Array(arr).to_string())
}

// --- Archive (Unterm-local, non-destructive hide) ---------------------------
//
// Claude Code has no archive concept and the transcripts are its storage, so
// this only records which sessions the picker hides by default — it never
// touches the `.jsonl` files (deleting one open in a live `claude` elsewhere
// would corrupt it). Persisted per project under Library/Unterm (machine-local,
// gitignored), same file the C# side used before this moved native.

fn archive_path(cwd: &str) -> PathBuf {
    PathBuf::from(cwd)
        .join("Library")
        .join("Unterm")
        .join("archived-sessions.json")
}

/// The archived session-id set for `cwd`'s project (empty when none).
pub fn archived_ids(cwd: &str) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(archive_path(cwd)) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return HashSet::new();
    };
    v["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The picker title for a single session id (Claude Code's `ai-title`, else the
/// first user line) — so the tab/header can show the same title the browser
/// does. `None` when the file is missing or has no real title yet.
pub fn title_for(cwd: &str, id: &str) -> Option<String> {
    let dir = project_dir(cwd)?;
    let text = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).ok()?;
    scan(&text, cwd, "")
        .map(|(t, _)| t)
        .filter(|t| t != "(untitled)")
}

/// Mark/unmark a session as archived for `cwd`'s project.
pub fn set_archived(cwd: &str, id: &str, archived: bool) {
    if id.is_empty() {
        return;
    }
    let mut ids = archived_ids(cwd);
    let changed = if archived {
        ids.insert(id.to_string())
    } else {
        ids.remove(id)
    };
    if !changed {
        return;
    }
    let path = archive_path(cwd);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort();
    let json = json!({ "ids": sorted }).to_string();
    if let Err(e) = std::fs::write(&path, json) {
        log::warn!("unterm: archived-sessions write failed: {e}");
    }
}

/// The user's `~/.claude` directory (`HOME` on Unix, `USERPROFILE` on Windows).
fn claude_home() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".claude"))
}

/// The `~/.claude/projects/<slug>` directory for `cwd`. Claude Code slugs a cwd
/// by replacing every non-alphanumeric character with `-` (so
/// `/a/b.c/d` -> `-a-b-c-d`); this reproduces that so we read one project's
/// sessions directly instead of scanning every project dir.
fn project_dir(cwd: &str) -> Option<PathBuf> {
    let slug: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(claude_home()?.join("projects").join(slug))
}

/// Session ids currently driven by a *live* `claude` process in `cwd`'s project,
/// read from Claude Code's own per-process registry (`~/.claude/sessions/<pid>.json`,
/// each `{pid, sessionId, cwd, ...}`). This is how Claude Code itself knows a
/// session is in use elsewhere — so the picker can grey out sessions open in an
/// external CLI or another editor, not just this process's own windows. Stale
/// entries (dead pid) are skipped, matching Claude Code's own liveness check.
pub fn open_elsewhere(cwd: &str) -> Vec<String> {
    let Some(dir) = claude_home().map(|h| h.join("sessions")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["cwd"].as_str() != Some(cwd) {
            continue;
        }
        let Some(sid) = v["sessionId"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let pid = v["pid"].as_u64().unwrap_or(0);
        if pid != 0 && pid_alive(pid as u32) {
            ids.insert(sid.to_string());
        }
    }
    ids.into_iter().collect()
}

/// Whether process `pid` is currently running. The `claude` processes are the
/// same user as us, so a plain existence check is enough.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 only probes existence; it sends nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: OpenProcess with a query-only access right; the handle is closed
    // immediately. A successful open means the pid is a live process.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// One listed session, as consumed by the native browser view (typed) and the
/// C# recent-list dropdown (serialized by [`poll`]).
#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub updated: u64, // file mtime, unix seconds
    pub snippet: String,
    pub archived: bool,
}

/// Enumerate (and optionally search) the project's sessions, newest first.
/// The capped lane (`limit` > 0, the recent dropdown) excludes archived
/// sessions entirely; the full lane (`limit` = 0, the browser) includes them
/// flagged, so the browser's "Archived" toggle can reveal them.
fn compute(req: &Request) -> Vec<Session> {
    let Some(dir) = project_dir(&req.cwd) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let archived = archived_ids(&req.cwd);
    let needle = req.query.trim().to_lowercase();
    let mut sessions: Vec<Session> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let is_archived = archived.contains(&id);
        if is_archived && req.limit != 0 {
            continue; // the recent dropdown never shows archived sessions
        }
        let updated = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((title, snippet)) = scan(&text, &req.cwd, &needle) else {
            continue; // wrong project cwd, or search miss
        };
        sessions.push(Session {
            id,
            title,
            updated,
            snippet,
            archived: is_archived,
        });
    }

    sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
    if req.limit != 0 {
        sessions.truncate(req.limit);
    }
    sessions
}

/// Derive a session's title and verify it belongs to `cwd`; when `needle` is
/// non-empty, also require a title/body match and return a match snippet. Title
/// preference: Claude Code's own generated title (the last `ai-title` record's
/// `aiTitle`, i.e. exactly what its `--resume` picker shows), else the first real
/// user message.
fn scan(text: &str, cwd: &str, needle: &str) -> Option<(String, String)> {
    let mut title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut cwd_ok = false;
    let mut snippet = String::new();
    let mut matched = needle.is_empty();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !cwd_ok {
            if let Some(c) = v["cwd"].as_str().filter(|s| !s.is_empty()) {
                if c != cwd {
                    return None; // a different project's session sharing the slug
                }
                cwd_ok = true;
            }
        }
        match v["type"].as_str().unwrap_or("") {
            // Claude Code regenerates this as the conversation grows; the last one
            // is the current title.
            "ai-title" => {
                if let Some(s) = v["aiTitle"].as_str().filter(|s| !s.is_empty()) {
                    ai_title = Some(s.to_string());
                }
            }
            role @ ("user" | "assistant") => {
                let is_meta = v["isMeta"].as_bool().unwrap_or(false)
                    || v["isCompactSummary"].as_bool().unwrap_or(false);
                let body = message_text(&v["message"]["content"]);
                if role == "user" && title.is_none() && !is_meta {
                    if let Some(t) = user_title(&body) {
                        title = Some(t);
                    }
                }
                if !matched && !body.is_empty() && body.to_lowercase().contains(needle) {
                    matched = true;
                    snippet = excerpt(&body, needle);
                }
            }
            _ => {}
        }
    }

    let title = ai_title
        .or(title)
        .unwrap_or_else(|| "(untitled)".to_string());
    if !matched && !title.to_lowercase().contains(needle) {
        return None;
    }
    Some((title, snippet))
}

/// Flatten a message `content` field (a plain string, or an array of blocks) to
/// its text, ignoring non-text blocks (tool calls, images).
fn message_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if let Some(t) = block["text"].as_str() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(t);
        }
    }
    out
}

/// A display title from a user message body, or `None` if this turn shouldn't
/// title the session — so the title skips slash-command invocations
/// (`<command-name>/model…`) and the harness's local-command caveat/output, and
/// falls through to the first real user prose (which is what a title should be).
fn user_title(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() || body.starts_with("<command-") || body.starts_with("<local-command-") {
        return None;
    }
    Some(first_line(body))
}

fn first_line(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    truncate_chars(line, 80)
}

/// A short window of `body` around the first occurrence of `needle` (lowercased).
fn excerpt(body: &str, needle: &str) -> String {
    let lower = body.to_lowercase();
    let Some(pos) = lower.find(needle) else {
        return truncate_chars(body.trim(), 100);
    };
    // Back up ~30 chars on a char boundary for context.
    let start = body[..pos]
        .char_indices()
        .rev()
        .nth(30)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let slice = body[start..].trim();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&truncate_chars(slice, 100));
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_claude_encoding() {
        let dir = project_dir("/Users/x/Developer/github.com/x/Unity-Unterm").unwrap();
        assert!(dir
            .to_string_lossy()
            .ends_with("/.claude/projects/-Users-x-Developer-github-com-x-Unity-Unterm"));
    }

    fn line(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn title_prefers_ai_title_then_first_user() {
        let cwd = "/proj";
        // ai-title present -> wins (and the last one, as Claude regenerates it).
        let text = [
            line(json!({"type":"user","cwd":cwd,"message":{"content":"hello there"}})),
            line(json!({"type":"ai-title","aiTitle":"Fix the build","sessionId":"x"})),
            line(json!({"type":"ai-title","aiTitle":"Fix the release build","sessionId":"x"})),
        ]
        .join("\n");
        assert_eq!(scan(&text, cwd, "").unwrap().0, "Fix the release build");

        // no ai-title -> first non-meta user message's first line.
        let text = [
            line(json!({"type":"user","cwd":cwd,"isMeta":true,"message":{"content":"<reminder>"}})),
            line(json!({"type":"user","cwd":cwd,"message":{"content":"real question\nmore"}})),
        ]
        .join("\n");
        assert_eq!(scan(&text, cwd, "").unwrap().0, "real question");
    }

    #[test]
    fn wrong_cwd_is_rejected() {
        let text = line(json!({"type":"user","cwd":"/other","message":{"content":"hi"}}));
        assert!(scan(&text, "/proj", "").is_none());
    }

    #[test]
    fn search_matches_body_and_title_else_none() {
        let cwd = "/proj";
        let text = [
            line(json!({"type":"user","cwd":cwd,"message":{"content":"talk about widgets"}})),
            line(json!({"type":"assistant","message":{"content":[{"type":"text","text":"the FrobNicator is ready"}]}})),
        ]
        .join("\n");
        // body-only term matches and yields a snippet.
        let (_, snip) = scan(&text, cwd, "frobnicator").unwrap();
        assert!(snip.to_lowercase().contains("frobnicator"), "snippet={snip:?}");
        // title term matches.
        assert!(scan(&text, cwd, "widgets").is_some());
        // miss -> None.
        assert!(scan(&text, cwd, "nonexistent").is_none());
    }

    #[test]
    fn commands_and_caveats_are_not_titles() {
        assert_eq!(user_title("<command-name>/model</command-name>"), None);
        assert_eq!(user_title("<local-command-caveat>whatever</local-command-caveat>"), None);
        assert_eq!(user_title("   "), None);
        assert_eq!(user_title("real prose\nsecond").as_deref(), Some("real prose"));
    }

    #[test]
    fn title_skips_commands_to_first_real_prose() {
        let cwd = "/proj";
        let text = [
            line(json!({"type":"user","cwd":cwd,"message":{"content":"<local-command-caveat>x</local-command-caveat>"}})),
            line(json!({"type":"user","cwd":cwd,"message":{"content":"<command-name>/model</command-name>"}})),
            line(json!({"type":"user","cwd":cwd,"message":{"content":"build me a thing"}})),
        ]
        .join("\n");
        assert_eq!(scan(&text, cwd, "").unwrap().0, "build me a thing");
    }

    #[test]
    fn array_content_is_flattened() {
        let cwd = "/proj";
        let text = line(json!({"type":"user","cwd":cwd,
            "message":{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}));
        assert_eq!(scan(&text, cwd, "").unwrap().0, "first second");
    }
}
