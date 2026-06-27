# Changelog

## [0.3.0] - 2026-06-27

### Added

- **Claude Code agent panel** — a native in-Editor chat view (transcript + composer) that drives the `claude` CLI in-process over its stream-json control protocol (no Node), with the in-editor MCP server wired in. Renders Markdown (code & diff fences with syntax highlighting, tables), resumes past conversations via a session picker, queues follow-up prompts, and exposes permission-mode / model / thinking-level controls, plan approval, and collapsible tool calls.
- **Windows support** — the terminal now runs in the Windows Editor too (PowerShell/cmd over a ConPTY), rendered zero-copy via a shared D3D12 texture handed to Unity's own device.
- **Session restore** — terminals restore their scrollback across a full editor restart (not just a C# domain reload); a window whose shell had already exited is restored read-only.
- **Working-directory restore** — a resumed terminal reopens in the shell's last working directory, falling back to the project root if that directory is gone.

### Changed

- The terminal grid now fills the whole window: the toolbar has been removed, and font size +/- moved into the right-click menu.
- A new terminal window opens offset from the active one instead of stacking exactly on top of it.
- The in-progress IME composition is now drawn natively at the cursor in the terminal font (with an underline and caret) instead of via an IMGUI text-field overlay, so it matches the grid on both macOS and Windows.
- Upgraded the native renderer to wgpu 29 (glyphon 0.11 / cosmic-text 0.18), rewriting the zero-copy paths onto the binding crates wgpu-hal now uses (objc2-metal on macOS, windows-rs on Windows).

### Fixed

- A terminal window wider or taller than 2048 px no longer fails to render: the renderer now requests the GPU's real maximum texture size and clamps its target to it, instead of the 2048-px downlevel default.
- CJK ideographs (kanji) now render in the correct regional font for the system locale (e.g. Japanese on a `ja-JP` machine) instead of falling back to a Chinese font.

## [0.2.2] - 2026-06-20

### Changed

- Lowered the minimum supported Unity version to 6.3 (6000.3).

## [0.2.1] - 2026-06-20

### Added

- **Claude Code launcher** — a menu item that opens a terminal already running
  `claude` in the PTY, so you can start a Claude Code session in one step.

## [0.2.0] - 2026-06-19

### Added

- **Scrollback** — scroll the wheel to page back through history, with an
  overlay scrollbar on the right edge that appears while scrolled back and can
  be dragged to any position.

### Fixed

- The Enter key that commits an IME composition is no longer also sent to the
  shell, so confirming a conversion no longer runs a stray command.

## [0.1.0] - 2026-06-19

### Added

- Native Rust/wgpu terminal window for the Unity Editor, backed by a real PTY
  shell, that survives C# domain reloads.
- IME composition input with wide-character alignment and a UTF-8 shell locale.
- Mouse selection with a right-click Copy/Paste menu.
- Terminal shortcuts: focused-terminal key priority, clear, and bracketed paste.
