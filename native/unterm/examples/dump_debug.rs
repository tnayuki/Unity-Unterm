//! Headless SDB debug session against a running Unity editor.
//!
//! Attaches to the editor's Mono agent, arms a source breakpoint, and — once you
//! press Play with the target script on a GameObject — resolves it via sequence
//! points, stops on the hit, and dumps the call stack + locals.
//!
//!   cargo run -p unterm --example dump_debug                 # Rotator.cs:37
//!   cargo run -p unterm --example dump_debug -- Rotator.cs 43
//!
//! The project root is discovered from the cwd (walks up to Library/EditorInstance.json).

use std::time::{Duration, Instant};

use unterm::sdb::{self, kind_name, value::Value, wire, DebugInfo};

fn main() {
    env_logger::try_init().ok();

    let mut args = std::env::args().skip(1);
    let file = args.next().unwrap_or_else(|| "Rotator.cs".to_string());
    let line: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(37);

    let root = std::env::current_dir().unwrap();
    let mut conn = match sdb::connect_editor(&root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e} (is the Unity editor open under {}?)", root.display());
            std::process::exit(1);
        }
    };
    let v = conn.version().clone();
    println!("attached: {} (protocol {}.{})", v.vm, v.major, v.minor);
    while conn.take_queued().is_some() {} // drain VM_START

    validate_debug_info_framing(&mut conn);

    // Arm: subscribe to TYPE_LOAD for the target file. Types already loaded resolve
    // immediately; everything else resolves when the play-mode domain loads them.
    let watch = conn.watch_source_files(&[file.clone()]).expect("watch source");
    println!("watching TYPE_LOAD for {file} (request {watch})");
    let mut armed = false;
    if let Ok(types) = conn.types_for_source_file(&file, true) {
        if !types.is_empty() {
            println!("{} type(s) already loaded for {file}", types.len());
            armed |= try_arm(&mut conn, &types, &file, line);
        }
    }

    println!("\n>>> Press Play in Unity (attach `{file}` to a GameObject). Waiting for the breakpoint...\n");
    conn.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let deadline = Instant::now() + Duration::from_secs(300);

    while Instant::now() < deadline {
        let composite = match conn.next_event() {
            Ok(c) => c,
            Err(wire::SdbError::Io(ref e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(wire::SdbError::Eof) => {
                println!("editor disconnected");
                return;
            }
            Err(e) => {
                println!("event error: {e}");
                continue;
            }
        };

        for ev in &composite.events {
            match ev.kind {
                wire::kind::TYPE_LOAD => {
                    if !armed {
                        armed = try_arm(&mut conn, &[ev.type_id], &file, line);
                    }
                }
                wire::kind::BREAKPOINT => {
                    println!("\n=== BREAKPOINT hit on thread {} ===", ev.thread);
                    dump_stop(&mut conn, ev.thread);
                    println!("\nresuming...");
                    conn.resume().ok();
                    return;
                }
                other => {
                    println!("event: {} ({})", kind_name(other), other);
                }
            }
        }
    }
    println!("timed out waiting for the breakpoint");
}

/// Get type methods, read their sequence points, resolve the line, and set the BP.
fn try_arm(conn: &mut sdb::Connection, types: &[u32], file: &str, line: i32) -> bool {
    let mut candidates = Vec::new();
    for &t in types {
        let methods = match conn.type_methods(t) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for m in methods {
            if let Ok(info) = conn.debug_info(m) {
                candidates.push((m, info));
            }
        }
    }
    match sdb::resolve_line(&candidates, file, line) {
        Some((method, il)) => {
            let name = conn.method_name(method).unwrap_or_default();
            match conn.set_breakpoint(method, il) {
                Ok(req) => {
                    println!("armed breakpoint at {file}:{line} -> {name}+0x{il:x} (request {req})");
                    true
                }
                Err(e) => {
                    println!("set_breakpoint failed: {e}");
                    false
                }
            }
        }
        None => {
            println!("could not resolve {file}:{line} in {} method(s) yet", candidates.len());
            false
        }
    }
}

/// Dump the call stack and the top frame's `this` + in-scope locals.
fn dump_stop(conn: &mut sdb::Connection, thread: u32) {
    let frames = match conn.frames(thread) {
        Ok(f) => f,
        Err(e) => {
            println!("frames error: {e}");
            return;
        }
    };
    println!("call stack ({} frames):", frames.len());
    for (i, f) in frames.iter().enumerate().take(12) {
        let name = conn.method_name(f.method).unwrap_or_default();
        let loc = conn
            .debug_info(f.method)
            .ok()
            .and_then(|info| il_to_source(&info, f.il_offset))
            .unwrap_or_default();
        println!("  #{i:<2} {name}  (il 0x{:x}) {loc}", f.il_offset);
    }

    let Some(top) = frames.first() else { return };

    match conn.frame_this(thread, top.id) {
        Ok(Value::Object { id, .. }) if id != 0 => println!("\nthis = obj#{id}"),
        Ok(v) => println!("\nthis = {}", v.summary()),
        Err(e) => println!("\nthis: <error {e}>"),
    }

    let locals = match conn.locals_info(top.method) {
        Ok(l) => l,
        Err(e) => {
            println!("locals_info error: {e}");
            return;
        }
    };
    // Only request locals in scope at the current IL offset (others may be invalid).
    let in_scope: Vec<(usize, &sdb::Local)> = locals
        .iter()
        .enumerate()
        .filter(|(_, l)| top.il_offset >= l.live_start && top.il_offset < l.live_end)
        .collect();
    println!("locals ({}/{} in scope at il 0x{:x}):", in_scope.len(), locals.len(), top.il_offset);
    if in_scope.is_empty() {
        return;
    }
    let positions: Vec<i32> = in_scope.iter().map(|(i, _)| *i as i32).collect();
    match conn.frame_values(thread, top.id, &positions) {
        Ok(values) => {
            for ((_, l), v) in in_scope.iter().zip(values) {
                println!("  {} = {}", l.name, v.summary());
            }
        }
        Err(e) => println!("  <frame_values error: {e}>"),
    }
}

/// Map an IL offset back to a source line: the nearest non-hidden seq point at or
/// before it.
fn il_to_source(info: &DebugInfo, il: i32) -> Option<String> {
    let sp = info
        .seq_points
        .iter()
        .filter(|s| !s.is_hidden() && s.il_offset <= il)
        .max_by_key(|s| s.il_offset)?;
    let src = info.sources.get(sp.source_idx as usize).cloned().unwrap_or_default();
    Some(format!("{}:{}", src.rsplit(['/', '\\']).next().unwrap_or(&src), sp.line))
}

/// Sanity-check the GET_DEBUG_INFO decoder against a loaded method (mscorlib is
/// always present) so a layout error surfaces before we depend on it for breakpoints.
fn validate_debug_info_framing(conn: &mut sdb::Connection) {
    let probe = (|| -> Option<()> {
        let dom = conn.root_domain().ok()?;
        let asm = *conn.domain_assemblies(dom).ok()?.first()?;
        // ASSEMBLY.GET_TYPE for a ubiquitous type.
        let type_id = conn.assembly_type(asm, "System.Object").ok()?;
        if type_id == 0 {
            return None;
        }
        let methods = conn.type_methods(type_id).ok()?;
        let m = *methods.first()?;
        let info = conn.debug_info(m).ok()?;
        println!(
            "debug_info framing ok: mscorlib method maxil={} sources={} seq_points={}",
            info.max_il,
            info.sources.len(),
            info.seq_points.len()
        );
        Some(())
    })();
    if probe.is_none() {
        println!("debug_info framing probe skipped (no symbols on mscorlib, expected)");
    }
}
