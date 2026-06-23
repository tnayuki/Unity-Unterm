//! Pseudo-terminal: spawn the user's shell on a PTY and expose its master end.
//!
//! `term.rs` owns the parser/grid and drives a reader thread that pumps the
//! shell's output into it; this module just opens the PTY, launches the child,
//! and hands back the master (for resize), a reader, and a writer.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

pub struct Pty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

/// The split halves handed out at spawn: master+child kept by the terminal,
/// reader consumed by the reader thread, writer used by the input path.
pub struct PtyHandles {
    pub pty: Pty,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

/// Spawn `shell` (with `args`) on a fresh PTY of `cols`x`rows`, rooted at `cwd`.
/// `args` is empty for a plain interactive shell; the command-launch path passes
/// `-lic "exec <command>"` so the shell sources rc (resolving PATH) then execs
/// the program directly as the PTY leader.
pub fn spawn(
    shell: &str,
    args: &[String],
    cwd: &str,
    cols: u16,
    rows: u16,
) -> Result<PtyHandles, Box<dyn std::error::Error>> {
    let pair = native_pty_system().openpty(PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(shell);
    for a in args {
        cmd.arg(a);
    }
    // Only set the cwd if it's a real directory: a stale/deleted path (e.g. a
    // resumed session whose dir is gone, or a TOCTOU after the host's check) would
    // otherwise fail the spawn. Falling back to inherit keeps the terminal alive.
    if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
        cmd.cwd(cwd);
    }
    // Advertise a capable terminal so programs emit colors/cursor sequences.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    // macOS ships /etc/zshrc_Apple_Terminal (and a bash equivalent) that reports
    // the shell's working directory via OSC 7 — but only when TERM_PROGRAM marks
    // an Apple terminal. Set it so the shell emits OSC 7 on every prompt; the
    // reader captures it for cwd-on-resume (no sysinfo, no rc injection).
    #[cfg(target_os = "macos")]
    cmd.env("TERM_PROGRAM", "Apple_Terminal");

    // Ensure a UTF-8 locale so the shell's line editor handles multibyte input
    // (e.g. Japanese) instead of garbling it. GUI hosts like Unity often launch
    // without LANG set, which leaves zsh/readline in a single-byte C locale.
    // Windows has no locale env of this kind (ConPTY is UTF-16/UTF-8 natively),
    // so this is unix-only.
    #[cfg(unix)]
    {
        let has_utf8 = ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|k| {
            std::env::var(k)
                .map(|v| v.to_ascii_uppercase().replace('-', "").contains("UTF8"))
                .unwrap_or(false)
        });
        if !has_utf8 {
            cmd.env("LANG", "en_US.UTF-8");
        }
    }

    let child = pair.slave.spawn_command(cmd)?;
    // Dropping the slave after spawn lets the child own the only slave fd, so
    // reads return EOF once it exits (otherwise the reader thread would hang).
    drop(pair.slave);

    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    Ok(PtyHandles {
        pty: Pty {
            master: pair.master,
            child,
        },
        reader,
        writer,
    })
}

impl Pty {
    /// Resize the PTY window (informs the child via SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Whether the child shell is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Best-effort: ensure the shell goes away with the terminal.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
