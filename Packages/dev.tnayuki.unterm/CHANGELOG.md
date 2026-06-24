# Changelog

## [Unreleased]

### Added

- **Session restore** — terminals restore their scrollback across a full editor restart (not just a C# domain reload); a window whose shell had already exited is restored read-only.
- **Working-directory restore** — a resumed terminal reopens in the shell's last working directory, falling back to the project root if that directory is gone.

### Changed

- The terminal grid now fills the whole window: the toolbar has been removed, and font size +/- moved into the right-click menu.

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
