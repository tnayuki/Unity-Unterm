# Unity-Unterm

A native terminal window for the Unity Editor on macOS and Windows — a real
PTY-backed shell rendered by a Rust/wgpu engine (zero-copy: IOSurface/Metal on
macOS, a shared D3D12 texture on Windows) and hosted inside an `EditorWindow`.

![Unterm running inside the Unity Editor](docs/demo.gif)

## Why

Editor work constantly bounces out to a terminal — `git`, build scripts,
tailing logs. Unterm puts a genuine terminal *inside* the editor: not a
log capture or a command runner, but a full VT emulator running your login
shell, so `vim`, `tmux`, REPLs, and TUIs all work.

## Install

Add to `Packages/manifest.json`:

```json
{
  "dependencies": {
    "dev.tnayuki.unterm": "https://github.com/tnayuki/Unity-Unterm.git#upm"
  }
}
```

Pin a version: `...git#upm/v0.2.0` (CI tags the `upm` branch as `upm/<tag>`).

## Usage

Open **Window ▸ Unterm ▸ New Terminal** (`Cmd+Shift+T`). Each invocation
opens an independent terminal; open as many as you like. New terminals
start in the project root.

- **IME** — full composition input with wide-character alignment.
- **Selection** — drag to select; double/triple-click for word/line.
- **Copy / Paste** — right-click menu, or the usual editor shortcuts;
  bracketed paste is supported.
- **Scrollback** — scroll the wheel to page back through history; an overlay
  scrollbar appears on the right edge and can be dragged to any position.
- **Domain reloads** — the shell and scrollback live in the native plugin,
  so they survive C# recompiles. The window re-adopts its terminal after a
  reload instead of restarting the shell.

## Claude Code

Unterm has an in-Editor Claude Code agent panel — a transcript and composer that
drive Anthropic's standalone Claude Code engine in-process, no Node required.

1. Open **Preferences ▸ Unterm** and click **Download Claude Code**. The engine
   (~214 MB) is fetched from Anthropic's official npm registry into a per-user
   folder shared by all your projects.
2. Sign in with your own Anthropic account: run `claude login` (or type `/login`
   in the panel, which opens a terminal for the browser sign-in).
3. Open the panel from **Window ▸ Unterm ▸ Claude Code**. The menu item stays
   disabled until the engine has been downloaded.

## Code editor

Unterm can be your script editor too — an in-Editor code editor with tree-sitter
highlighting and in-process Roslyn C# completion, no external application or
solution files. Select it under **Preferences ▸ External Tools ▸ External Script
Editor ▸ Unterm Code Editor**. Afterwards, double-clicking a script, jumping to a
compile error, **Open C# Project**, and file paths clicked in the Claude Code
transcript all open there.

## Platform

macOS and Windows, Unity 6.3 (6000.3) or newer. The renderer hands the editor a
GPU texture with no CPU copy — an IOSurface (Metal) on macOS, a shared D3D12
texture on Windows — so the menu item is registered only on those editors; on
any other platform the package contributes nothing.

## Repository layout

- `Packages/dev.tnayuki.unterm/` — the distributable UPM package. The native
  `unterm.dylib` is a build artifact and is **not** tracked here; only its
  `.meta` is. On each release (a `v*` tag) a GitHub Action builds the native
  binaries — a universal `unterm.dylib` on a macOS runner and `unterm.dll` on a
  Windows runner — and publishes the package, binaries included, to the `upm`
  branch that the install URL points at, plus a matching `upm/<tag>`.
- `native/` — the Rust source for the terminal engine. Run
  `native/build-macos.sh` (or `native/build-windows.ps1`) to build the native
  binary into the package for in-editor development. Not part of the published source.

## License

MIT
