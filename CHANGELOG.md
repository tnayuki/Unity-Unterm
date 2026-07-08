# Changelog

## [0.6.0] - 2026-07-08

### Added

- The code editor shows git-diff markers in the gutter, colored bars for added/modified lines and a red wedge where lines were deleted, diffed against HEAD and updating live as you type. Hovering a marker pops up the hunk's unified diff (removed lines in red, added in green); clicking it opens a menu to stage, unstage, or revert that hunk. Staged hunks draw hollow so they stay visible until committed.
- The Claude Code agent now tells you when it needs you while you're working elsewhere: it plays a soft chime and shows a notification card in the top-right of the screen — naming the project and session — when a turn finishes or a permission prompt is raised, but only while the Unity Editor is in the background, so it never interrupts you when you're already in the editor. The card is a native floating window (the same wgpu-rendered panel the editor's completion popup uses), so it looks and works the same on macOS and Windows: clicking it brings the editor to the front, and it dismisses itself once you're back. On macOS it also follows you across Spaces and over full-screen apps like a system notification. The chime is a bundled sound. It can all be turned off under Preferences ▸ Unterm ▸ Agent.
- The Claude Code agent's in-Editor Unity tools gained several capabilities: `unity_capture` renders a Game camera or the Scene view to a screenshot the agent can actually see (URP/HDRP via render requests, built-in via `Camera.Render`); `unity_package` drives the Package Manager (list / info / add / remove); `unity_menu` can now search menu-item paths instead of only executing a path you already know; `unity_script` can validate C# syntax through the bundled Roslyn without waiting for a domain reload; and `unity_editor`'s state read now reports the Unity version, platform, play-mode transition, active scene path, current selection, active tool, tags, and layers instead of just play/pause/compiling and the scene name.
- The Claude Code agent is now told, up front, that it is running embedded inside the Unity Editor: each session is spawned with an appended system prompt naming the Unity version and the project's product name and pointing the agent at the in-process `unterm-unity` MCP tools, so it reaches for them over shell commands when a task touches the running editor.
- Switching to another session or starting a new conversation while a turn is still running now asks for confirmation first, so an in-flight response isn't stopped by accident.
- The Claude Code agent has a project-wide session picker. The header dropdown lists this project's recent conversations straight from Claude Code's own storage — so `claude` CLI sessions in the same project appear too — each titled with Claude Code's generated `ai-title`, and an "All Sessions…" view replaces the transcript with a native, searchable list (by title and transcript text) over the full set. Sessions open in another live `claude` process are greyed out (from Claude Code's session registry), any session can be archived to hide it from the picker, and the list updates live as sessions come and go on disk.

### Fixed

- Ending a conversation's `claude` engine (on switching sessions, starting a new one, or closing the window) now shuts it down gracefully — closing its stdin so the stream-json engine exits on its own and finalizes its transcript, escalating to SIGTERM then SIGKILL only if it lingers — instead of an immediate SIGKILL.
- The Claude Code agent composer now honours the same caret-navigation shortcuts as the code editor — word-wise motion (⌥←/→, Ctrl+←/→ on Windows/Linux), line and document ends (⌘←/→, ⌘↑/↓, Home/End), and word / line deletion (⌥/⌘+Backspace, ⌥+Delete) — which were already wired up in the editor but had been missed in the composer.
- The text caret in the agent composer and the code editor is now hidden while the window lacks keyboard focus, instead of leaving a blinking insertion bar (and a stray thin IME text-cursor line) on a background window.
- The native quad and mesh renderers (terminal cell backgrounds, cursor, selection, popup chrome and icons) now write each frame's vertices into one persistent, grow-only GPU buffer instead of allocating a fresh buffer — and discarding the old one — on every rendered frame.
- The terminal now caches each row's shaped glyph layout across frames by content, so streaming output re-shapes only the rows that actually changed instead of every visible row on every frame (scrolled rows stay cache hits, as does a selection sweeping over them), and its glyph rasterization cache is persisted across frames instead of being rebuilt per frame.
- The code editor no longer re-sends its size and theme to the native view — nor re-resolves Unity's internal background-color method via reflection — on every keystroke, drag, and scroll; both are pushed only when they actually change.
- The Claude Code agent and terminal windows no longer allocate fresh managed strings on every idle editor tick: the agent's permission mode, session id, and pending host command are read only on ticks the native poll flags them changed, and the terminal's title only on dirty ticks (a title-only OSC change now marks the surface dirty so none are missed).

## [0.5.1] - 2026-07-04

### Fixed

- Double-clicking a scene (or any non-code asset) in the Project window no longer opens it as text in the Unterm code editor when Unterm is selected as the External Script Editor. Unterm now only claims the extensions Unity treats as project code — its C# project-generation set plus whatever you add under Project Settings ▸ Editor, and the `json`/`asmdef`/`asmref`/`log` formats Unity's own VSCode/Rider packages force-add — and declines everything else so Unity's native handler opens it. The same filter now governs which transcript path-clicks open in the editor.

## [0.5.0] - 2026-07-03

### Added

- The Claude Code agent now offers the **Auto** permission mode (a model classifier approves or denies each permission prompt), in the mode dropdown and the Shift+Tab cycle. It's offered only on models that advertise support for it (per the engine's roster — e.g. not Haiku), and switching to an unsupported model drops the session back to Default.
- The Claude Code agent composer now offers native `/` slash-command completion: typing `/` opens a floating list of the engine's advertised commands (built-ins plus your skills) above the input, filtered as you type — by name and alias — with arrow-key navigation and Tab/Enter to accept. Your own skills/commands are grouped first and lightly colour-coded apart from the built-ins, each group sorted by name. The roster comes from the session's `initialize` reply, so it reflects exactly the commands that session exposes.
- The Claude Code agent's model picker is now built from the roster the engine advertises in its `initialize` reply, so it lists exactly the models your account is entitled to — Fable, 1M-context variants, and so on — under the engine's own display names, instead of a fixed Opus/Sonnet/Haiku list.
- The Claude Code agent transcript now marks pauses between messages with a small, right-aligned relative-time separator ("5 minutes ago", localized to the editor's locale via `timeago`), merging consecutive separators that resolve to the same time. Hovering a separator shows the exact local time, and the session picker lists each conversation's last activity with the same relative labels.

### Changed

- The terminal renderer now reuses its per-frame cell scratch buffer instead of reallocating it every repaint, cutting steady allocator churn while a busy shell is streaming output.
- The Claude Code agent transcript is now serialized lazily and change-tracked by a counter: polling no longer clones the full transcript every editor tick, and a streaming turn no longer re-serializes the whole conversation on every delta — both used to scale with session length.
- The agent panel now skips its full Markdown re-parse and re-layout when nothing it renders from has changed, instead of rebuilding the entire transcript's layout on every repaint request (focus changes, ignored keys, and other no-op events included). When something does change, each block's shaped layout is cached and reused, so a streaming reply re-lays-out only the block that grew instead of the whole conversation.

### Fixed

- The Claude Code agent's pending-permission prompt now renders as a full-colour card so it reads as an actionable request instead of dim status text, and the animated "Thinking" indicator no longer shows while the session is blocked waiting for your allow/deny decision (it isn't thinking — it's waiting on you).
- A resumed Claude Code conversation no longer shows the CLI's synthetic messages as raw user bubbles: slash-command invocations render as `/name args`, their output as a plain result line, and harness-injected turns (the local-command caveat, auto "Continue…" nudges, system reminders, task-completion pings) are dropped, while a compaction summary collapses to a short boundary marker. These never appeared during a live session, so a reopened transcript now matches what you saw live.
- The Claude Code agent panel can no longer crash the Editor on a rendering error: its render and poll entry points are now contained at the native boundary the way the terminal's already were, a full glyph atlas skips the frame instead of aborting, and a background-worker panic no longer wedges a session by leaving a mutex poisoned.
- Background C# autocomplete and signature-help failures are now surfaced once in the Console instead of being silently swallowed, so a broken completion is diagnosable without flooding the log.
- The autocomplete and signature-help worker threads now shut down cleanly when the Editor reloads its assemblies, instead of being aborted mid-analysis and leaking an OS event handle on every reload.

## [0.4.1] - 2026-07-01

### Documentation

- The README now covers downloading the Claude Code engine (**Preferences ▸ Unterm**) and selecting **Unterm Code Editor** as the External Script Editor.

### Fixed

- The Windows Editor plugin now statically links the MSVC C runtime, so `unterm.dll` loads on machines without the Visual C++ redistributable installed (which otherwise failed with a `126 ERROR_MOD_NOT_FOUND` native load error).

## [0.4.0] - 2026-06-30

### Added

- **Code editor** — a native in-Editor code editor window with tree-sitter syntax highlighting, Roslyn-powered C# autocomplete and signature help, find/replace and line operations, registered as a selectable External Script Editor so Unity file opens (and file paths clicked in the Claude Code transcript) route to it.
- **Built-in Claude Code engine download** — a button in **Preferences > Unterm** fetches Anthropic's standalone Claude Code engine from the npm registry on demand, so the agent panel works without a separately installed `claude` (and without Node); the binary is integrity-checked, tracked to the latest release, and shared across your Unity projects.

### Changed

- On macOS, the native plugin now binds Unity's own Metal device in-process — the shadow-copy loader is gone and device/command-queue capture is unified into one cross-platform module shared with Windows.

### Fixed

- IME composition no longer crashes: the terminal IME field clamps its cached caret so a shrunk buffer can't throw out of range, and the agent composer guards its pre-edit input FFI against a reset buffer.

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
