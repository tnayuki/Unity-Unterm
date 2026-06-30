# Unterm

A native terminal window for the Unity Editor on macOS and Windows. A real
PTY-backed shell rendered by a Rust/wgpu engine (zero-copy: IOSurface/Metal on
macOS, a shared D3D12 texture on Windows) and hosted inside an `EditorWindow`.

## Why

Editor work constantly bounces out to a terminal — `git`, build scripts,
tailing logs. Unterm puts a genuine terminal *inside* the editor: not a
log capture or a command runner, but a full VT emulator running your login
shell, so `vim`, `tmux`, REPLs, and TUIs all work.

## Usage

Open **Window ▸ Unterm ▸ New Terminal** (`Cmd/Ctrl+Shift+T`). Each invocation
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

## Platform

macOS and Windows — the zero-copy display platforms. The renderer hands the
editor a GPU texture with no CPU copy: an IOSurface (Metal) on macOS, a shared
D3D12 texture on Windows. The menu item is registered only on those editors; on
any other platform the package contributes nothing.

The package ships prebuilt native binaries — a universal (arm64 + x86_64)
`unterm.dylib` for macOS and an `unterm.dll` for Windows (x86_64). To rebuild
from the Rust source, run `native/build-macos.sh` or `native/build-windows.ps1`
in the [development repository](https://github.com/tnayuki/Unity-Unterm).

## License

MIT
