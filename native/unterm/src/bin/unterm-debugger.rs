//! The standalone Unterm debugger window.
//!
//! Thin launcher: all logic lives in `unterm::debugger`. Usage:
//!   unterm-debugger [file.cs:line ...]
//! Each argument is a breakpoint as `SourceFile.cs:line`. The working directory must
//! be the Unity project root (so `Library/EditorInstance.json` is found). Defaults to
//! Rotator.cs:45 when no breakpoints are given.

fn main() {
    let mut bps: Vec<(String, i32)> = Vec::new();
    for arg in std::env::args().skip(1) {
        if let Some((file, line)) = arg.rsplit_once(':') {
            if let Ok(n) = line.parse::<i32>() {
                bps.push((file.to_string(), n));
                continue;
            }
        }
        eprintln!("ignoring malformed breakpoint arg: {arg:?} (expected file.cs:line)");
    }
    if bps.is_empty() {
        bps.push(("Rotator.cs".to_string(), 45));
    }
    unterm::debugger::run(bps);
}
