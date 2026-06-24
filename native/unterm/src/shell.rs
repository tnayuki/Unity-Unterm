//! Pick the shell (and its launch arguments) for the current platform.
//!
//! `term.rs` asks for a [`ShellSpec`] given an optional `command`. With an empty
//! command we launch an interactive shell; otherwise we launch `command` so it
//! replaces the shell and the terminal closes when it exits. The detection is
//! fully automatic — there is no user override — so this module is the single
//! place that knows which shell each OS prefers.

/// A resolved shell invocation: the program to spawn on the PTY plus its args.
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Resolve the shell for `command` (empty = interactive shell).
///
/// Unix: the login+interactive `$SHELL` (default `/bin/zsh`). A command is run
/// via `-lic "exec <command>"` so rc is sourced (PATH resolves) and the program
/// replaces the shell as the PTY leader — no typed-ahead input.
#[cfg(unix)]
pub fn resolve(command: &str) -> ShellSpec {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let args = if command.is_empty() {
        Vec::new()
    } else {
        vec!["-lic".to_string(), format!("exec {command}")]
    };
    ShellSpec { program, args }
}

/// Resolve the shell for `command` (empty = interactive shell).
///
/// Windows detection ladder: PowerShell 7+ (`pwsh.exe` on PATH), then Windows
/// PowerShell (`powershell.exe` at its fixed System32 path), then the legacy
/// `cmd.exe` (via `%COMSPEC%`). PowerShell variants get `-NoLogo` to skip the
/// banner; a command is passed through to the shell so it runs and then exits.
#[cfg(windows)]
pub fn resolve(command: &str) -> ShellSpec {
    let program = detect_shell();
    let lower = program.to_ascii_lowercase();
    let is_powershell = lower.ends_with("pwsh.exe") || lower.ends_with("powershell.exe");

    let mut args = Vec::new();
    if is_powershell {
        args.push("-NoLogo".to_string());
        if command.is_empty() {
            // Interactive: wrap the prompt to report the working directory via
            // OSC 9;9 every prompt (PowerShell's Set-Location doesn't change the
            // process cwd, so this is how the host learns it for resume), then stay
            // interactive with -NoExit.
            args.push("-NoExit".to_string());
            args.push("-Command".to_string());
            args.push(PS_CWD_PROMPT.to_string());
        } else {
            args.push("-Command".to_string());
            args.push(command.to_string());
        }
    } else if !command.is_empty() {
        // cmd.exe: `/c` runs the command and exits, mirroring the unix `exec`.
        args.push("/c".to_string());
        args.push(command.to_string());
    }
    ShellSpec { program, args }
}

/// PowerShell snippet (run via `-NoExit -Command`) that wraps `prompt` to emit
/// the current location as OSC 9;9 each time it renders, so the reader can
/// capture the cwd for resume. Uses `[char]27`/`[char]7` (valid in both pwsh and
/// Windows PowerShell) and preserves the user's prompt via `& $__untermPrompt`.
#[cfg(windows)]
const PS_CWD_PROMPT: &str = "$__untermPrompt = $function:prompt; function global:prompt { [Console]::Write([char]27 + ']9;9;' + $ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath + [char]7); & $__untermPrompt }";

/// Walk the detection ladder and return the absolute (or PATH-resolvable) path
/// of the first shell found.
#[cfg(windows)]
fn detect_shell() -> String {
    if let Some(pwsh) = find_in_path("pwsh.exe") {
        return pwsh;
    }
    // powershell.exe always ships at this fixed location; build the absolute
    // path so a hijacked PATH can't shadow it.
    if let Some(sysroot) = std::env::var_os("SystemRoot") {
        let ps = std::path::Path::new(&sysroot)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if ps.is_file() {
            return ps.to_string_lossy().into_owned();
        }
    }
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}

/// Find `exe` on the user's PATH, returning its full path if it's a real file.
#[cfg(windows)]
fn find_in_path(exe: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}
