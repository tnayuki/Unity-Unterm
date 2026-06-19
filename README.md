# Unity-Unterm

A native terminal window for the Unity Editor on macOS — a real PTY-backed
shell rendered by a Rust/wgpu engine (zero-copy via IOSurface/Metal) and
hosted inside an `EditorWindow`.

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

Pin a version: `...git#v0.1.0`.

## Usage

Open **Window ▸ Unterm ▸ New Terminal** (`Cmd+Shift+T`). Each invocation
opens an independent terminal; open as many as you like. New terminals
start in the project root.

- **IME** — full composition input with wide-character alignment.
- **Selection** — drag to select; double/triple-click for word/line.
- **Copy / Paste** — right-click menu, or the usual editor shortcuts;
  bracketed paste is supported.
- **Domain reloads** — the shell and scrollback live in the native plugin,
  so they survive C# recompiles. The window re-adopts its terminal after a
  reload instead of restarting the shell.

## Platform

macOS only. The renderer uses the IOSurface/Metal zero-copy path, so the
menu item is only registered when the Editor itself runs on macOS. On other
platforms the package contributes nothing.

## Repository layout

- `Packages/dev.tnayuki.unterm/` — the distributable UPM package (the
  prebuilt universal `unterm.bundle` is committed here). A GitHub Action
  subtree-splits it to the `upm` branch that the install URL points at.
- `native/` — the Rust source for the terminal engine. Run
  `native/build-macos.sh` to rebuild the universal `unterm.bundle` into the
  package. Not part of the published package.

## License

MIT
