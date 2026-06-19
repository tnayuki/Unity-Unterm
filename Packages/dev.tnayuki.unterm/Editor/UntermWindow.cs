using System;
using System.IO;
using UnityEditor;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// A terminal window: a native wgpu-rendered VT grid (PTY-backed shell)
    /// hosted in an <see cref="EditorWindow"/> and blitted with IMGUI. The shell
    /// and grid live in the native plugin keyed by a stable id, so they survive
    /// C# domain reloads; this window re-adopts by id after a reload and only
    /// kills the shell when the window actually closes.
    ///
    /// Multiple windows are independent terminals: each is created via
    /// <see cref="CreateWindow"/> (not the singleton <c>GetWindow</c>), gets its
    /// own id, and shares one native wgpu device with the others.
    /// </summary>
    public sealed class UntermWindow : EditorWindow
    {
        private const float ToolbarHeight = 20f;
        private const float DefaultFontPt = 13f;
        // Overlay scrollbar on the grid's right edge: it stays hidden while the
        // viewport is pinned to the live bottom and appears once you scroll back.
        private const float ScrollbarWidth = 9f;
        private const float ScrollbarMinThumb = 24f;

        private UntermNative _native;
        // The terminal lives in the native registry; we hold its id and re-adopt
        // it across domain reloads (it's serialized with the window).
        [SerializeField] private long _termIdRaw;
        [SerializeField] private float _fontPt = DefaultFontPt;
        private ulong Tid => (ulong)_termIdRaw;

        private Texture2D _tex;
        private IntPtr _externalTexPtr;
        private string _status = "";
        private bool _alive = true;

        // IME: a hidden, always-focused text field is the input sink so the OS
        // IME engages. Plain typing + committed IME text land in `_imeBuffer`
        // and are flushed to the PTY each frame; the in-progress composition is
        // drawn at the cursor. `_composing` is snapshotted at Layout so key
        // handling is stable within a frame. `_composeJustEnded` marks the frame
        // a composition was committed so the Enter that committed it is swallowed
        // instead of being forwarded to the PTY ahead of the committed text.
        private const string InputControl = "UntermInput";
        private string _imeBuffer = "";
        private bool _composing;
        private bool _prevComposing;
        private bool _composeJustEnded;
        private bool _refocus;

        // Mouse selection: a drag from MouseDown extends the highlight; a plain
        // click (down+up with no drag) clears it. `_selecting` is live between
        // down and up; `_dragged` records whether the mouse actually moved.
        private bool _selecting;
        private bool _dragged;
        // The selection mode set at MouseDown (0 = char, 1 = word, 2 = line).
        // MouseUp can't read clickCount reliably, so we keep it here.
        private byte _selectMode;
        // Scrollbar thumb drag: live between MouseDown on the thumb and MouseUp;
        // `_dragGrabY` is where within the thumb the pointer grabbed it.
        private bool _draggingScroll;
        private float _dragGrabY;
        private Color32 _bg = new Color32(24, 24, 24, 255);
        private Color32 _fg = new Color32(208, 208, 212, 255);
        private GUIStyle _imeStyle;
        private Texture2D _imeBgTex;

        private static bool s_reloading;
        // A one-shot command for the next terminal created by CreateRunning; null
        // for a plain interactive shell. Consumed in LoadNative on fresh create.
        private static string s_pendingCommand;

        // The native terminal is macOS-only (IOSurface/Metal zero-copy path), so
        // only register the menu item when the Editor itself runs on macOS.
#if UNITY_EDITOR_OSX
        [MenuItem("Window/Unterm/New Terminal %#t")]
        public static void OpenNew()
        {
            var w = CreateWindow<UntermWindow>();
            w.titleContent = new GUIContent("Terminal");
            w.minSize = new Vector2(240, 120);
            w.Show();
            w.Focus();
        }

        // Open a terminal that launches `command` directly in the PTY (not typed
        // into a shell). The terminal is created synchronously in OnEnable during
        // CreateWindow, so the command is handed to LoadNative via s_pendingCommand
        // and consumed there for the fresh terminal. Used by the Claude Code menu.
        internal static UntermWindow CreateRunning(string title, string command)
        {
            s_pendingCommand = command;
            try
            {
                var w = CreateWindow<UntermWindow>();
                w.titleContent = new GUIContent(title);
                w.minSize = new Vector2(240, 120);
                w.Show();
                w.Focus();
                return w;
            }
            finally
            {
                s_pendingCommand = null;
            }
        }
#endif

        // GUID of unterm.bundle.meta. Resolving by GUID makes the loader agnostic
        // to where the plugin lives: embedded under Assets/, an embedded package
        // under Packages/, or a git/registry package cached in Library/PackageCache.
        private const string BundleGuid = "54ea61c3e6ad54b688596fae0846fc88";

        private static string BundlePath
        {
            get
            {
                var assetPath = AssetDatabase.GUIDToAssetPath(BundleGuid);
                if (string.IsNullOrEmpty(assetPath))
                {
                    // Fallback to the in-repo source layout.
                    return Path.Combine(Application.dataPath, "Unterm/Plugins/macOS/unterm.bundle");
                }

                // Map the virtual asset path to a physical one. For packages cached
                // under Library/PackageCache the "Packages/<name>" prefix is virtual,
                // so resolve it through PackageInfo.resolvedPath.
                var pkg = UnityEditor.PackageManager.PackageInfo.FindForAssetPath(assetPath);
                if (pkg != null)
                {
                    var prefix = "Packages/" + pkg.name;
                    var rel = assetPath.Substring(prefix.Length).TrimStart('/');
                    return Path.Combine(pkg.resolvedPath, rel);
                }

                return Path.GetFullPath(assetPath);
            }
        }

        private static string ProjectRoot =>
            Directory.GetParent(Application.dataPath)?.FullName ?? Application.dataPath;

        private void OnEnable()
        {
            s_reloading = false;
            wantsMouseMove = false;
            AssemblyReloadEvents.beforeAssemblyReload += OnBeforeReload;
            EditorApplication.update += OnEditorUpdate;
            LoadNative();
        }

        private void OnDisable()
        {
            AssemblyReloadEvents.beforeAssemblyReload -= OnBeforeReload;
            EditorApplication.update -= OnEditorUpdate;
            // On a domain reload keep the native terminal (and the mapped image)
            // alive so the shell + scrollback survive the recompile; only tear it
            // all down when the window is actually closing.
            Teardown(keepTerminal: s_reloading);
        }

        private static void OnBeforeReload() => s_reloading = true;

        private void LoadNative(bool freshInstance = false)
        {
            try
            {
                _native = new UntermNative();
                _native.Load(BundlePath, freshInstance);

                float ppp = EditorGUIUtility.pixelsPerPoint;
                var (w, h) = CurrentPixelSize();

                // Re-adopt the existing terminal across reload, else create one.
                // A pending command (e.g. Claude Code) launches directly in the
                // PTY; otherwise a plain interactive shell. Re-adoption skips this,
                // so a running command survives reloads without relaunching.
                if (Tid == 0 || !_native.Exists(Tid))
                {
                    _termIdRaw = string.IsNullOrEmpty(s_pendingCommand)
                        ? (long)_native.Create(w, h, ppp, ProjectRoot)
                        : (long)_native.CreateCommand(w, h, ppp, ProjectRoot, s_pendingCommand);
                    ApplyFont();
                    _native.SetFontSize(Tid, _fontPt);
                }

                ApplyTheme();
                _native.SetFocus(Tid, true);
                RenderNow();
                _refocus = true;
                _status = "ready";
            }
            catch (Exception e)
            {
                _status = "load failed: " + e.Message;
                Debug.LogError("[Unterm] " + e);
                Teardown(keepTerminal: false);
            }
        }

        // Use the editor's monospace font if we can find one, else fall back to
        // the native generic monospace family.
        private void ApplyFont()
        {
            // Menlo first: a clean "Menlo" family with full Regular/Bold/Italic
            // faces. (SF Mono registers under a private ".SF NS Mono" name that
            // doesn't resolve by name, and Monaco reports as non-monospaced.)
            string[] candidates =
            {
                "/System/Library/Fonts/Menlo.ttc",
                "/System/Library/Fonts/SFNSMono.ttf",
                "/System/Library/Fonts/Monaco.ttf",
            };
            foreach (var p in candidates)
            {
                if (File.Exists(p)) { _native.SetFont(Tid, p); break; }
            }
        }

        private void Teardown(bool keepTerminal)
        {
            if (_tex != null)
            {
                DestroyImmediate(_tex);
                _tex = null;
                _externalTexPtr = IntPtr.Zero;
            }
            if (_imeBgTex != null)
            {
                DestroyImmediate(_imeBgTex);
                _imeBgTex = null;
                _imeStyle = null;
            }

            if (!keepTerminal)
            {
                if (_native != null && Tid != 0)
                    _native.Destroy(Tid);
                _termIdRaw = 0;
                _native?.Dispose(); // dlclose on real teardown
            }
            // On reload: drop the managed wrapper WITHOUT dlclose so the native
            // image (and the terminal registry) stay mapped for re-adoption.
            _native = null;
        }

        private (uint, uint) CurrentPixelSize()
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            uint w = (uint)Mathf.Max(1, Mathf.RoundToInt(position.width * ppp));
            uint h = (uint)Mathf.Max(1, Mathf.RoundToInt((position.height - ToolbarHeight) * ppp));
            return (w, h);
        }

        // Poll native state off the editor tick; re-render only when it changed.
        private void OnEditorUpdate()
        {
            if (_native == null || Tid == 0) return;

            bool repaint = false;

            string title = _native.Title(Tid);
            _alive = _native.IsAlive(Tid);
            string want = string.IsNullOrEmpty(title) ? "Terminal" : title;
            if (!_alive) want += " (exited)";
            if (titleContent.text != want)
            {
                titleContent = new GUIContent(want);
                repaint = true;
            }

            if (_native.Dirty(Tid))
            {
                RenderNow();
                repaint = true;
            }
            if (repaint) Repaint();
        }

        private void RenderNow()
        {
            if (_native == null || Tid == 0) return;
            var (w, h) = CurrentPixelSize();
            _native.Resize(Tid, w, h, EditorGUIUtility.pixelsPerPoint);
            _native.Render(Tid);
            UploadZeroCopy((int)w, (int)h);
        }

        private void ApplyTheme()
        {
            Color32 bg, fg, cursor;
            if (EditorGUIUtility.isProSkin)
            {
                bg = new Color32(24, 24, 24, 255);
                fg = new Color32(208, 208, 212, 255);
                cursor = new Color32(220, 220, 224, 255);
            }
            else
            {
                bg = new Color32(250, 250, 250, 255);
                fg = new Color32(28, 28, 30, 255);
                cursor = new Color32(40, 40, 44, 255);
            }
            _bg = bg;
            _fg = fg;
            _imeStyle = null; // rebuild against the new colors
            _native.SetColors(Tid, fg, bg, cursor);
        }

        // Wrap the native IOSurface-backed MTLTexture directly — no CPU copy.
        // Falls back to readback only if the texture is somehow unavailable.
        private void UploadZeroCopy(int iw, int ih)
        {
            IntPtr texPtr = _native.RawTexture(Tid);
            if (texPtr == IntPtr.Zero)
            {
                UploadReadback(iw, ih);
                return;
            }
            if (_tex == null || _tex.width != iw || _tex.height != ih || _externalTexPtr != texPtr)
            {
                if (_tex != null) DestroyImmediate(_tex);
                _tex = Texture2D.CreateExternalTexture(iw, ih, TextureFormat.RGBA32, false, false, texPtr);
                _tex.filterMode = FilterMode.Bilinear;
                _tex.hideFlags = HideFlags.HideAndDontSave;
                _externalTexPtr = texPtr;
            }
            else
            {
                _tex.UpdateExternalTexture(texPtr);
            }
        }

        private void UploadReadback(int iw, int ih)
        {
            IntPtr px = _native.GetPixels(Tid, out int len);
            if (px == IntPtr.Zero || len <= 0) { _status = "no pixels"; return; }
            if (_tex == null || _tex.width != iw || _tex.height != ih || _externalTexPtr != IntPtr.Zero)
            {
                if (_tex != null) DestroyImmediate(_tex);
                _tex = new Texture2D(iw, ih, TextureFormat.RGBA32, false)
                {
                    filterMode = FilterMode.Bilinear,
                    hideFlags = HideFlags.HideAndDontSave,
                };
                _externalTexPtr = IntPtr.Zero;
            }
            _tex.LoadRawTextureData(px, len);
            _tex.Apply(false);
        }

        private void OnFocus()
        {
            _refocus = true;
            if (_native != null && Tid != 0) _native.SetFocus(Tid, true);
        }

        private void OnLostFocus()
        {
            if (_native != null && Tid != 0) _native.SetFocus(Tid, false);
        }

        private void OnGUI()
        {
            // Snapshot IME composition once per frame for stable key handling.
            // A non-empty -> empty transition means a composition was just
            // committed; the OS clears compositionString in the same event that
            // delivers the committing Enter, so flag that frame to swallow it.
            if (Event.current.type == EventType.Layout)
            {
                bool now = !string.IsNullOrEmpty(Input.compositionString);
                _composeJustEnded = _prevComposing && !now;
                _prevComposing = now;
                _composing = now;
            }

            DrawToolbar();

            var rect = new Rect(0, ToolbarHeight, position.width, position.height - ToolbarHeight);

            // Re-render when the draw area no longer matches the texture (resize).
            var (cw, ch) = CurrentPixelSize();
            if (_native != null && Tid != 0 &&
                (_tex == null || _tex.width != (int)cw || _tex.height != (int)ch))
            {
                RenderNow();
            }

            HandleInput(rect);

            if (_tex != null)
            {
                // Native frame is top-down; Texture2D samples bottom-up, so flip V.
                GUI.DrawTextureWithTexCoords(rect, _tex, new Rect(0, 1, 1, -1));
            }
            else
            {
                EditorGUI.LabelField(rect, _status, EditorStyles.centeredGreyMiniLabel);
            }

            DrawScrollbar(rect);

            // The IME sink is drawn on top at the cursor: invisible when idle,
            // opaque while composing so the OS renders the composition inline.
            DrawImeField(rect);
            FlushIme();

            if (_refocus && Event.current.type == EventType.Repaint)
            {
                EditorGUI.FocusTextInControl(InputControl);
                _refocus = false;
            }
        }

        // Pixel-cursor rect mapped to GUI points within `rect` (fallbacks to a
        // bottom-left caret when the cursor is hidden).
        private Rect CursorPointRect(Rect rect)
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            if (_native != null && Tid != 0 &&
                _native.CursorPx(Tid, out float cx, out float cy, out float cw, out float chh))
            {
                return new Rect(rect.x + cx / ppp, rect.y + cy / ppp, cw / ppp, chh / ppp);
            }
            return new Rect(rect.x + 4, rect.yMax - 18, 8, 16);
        }

        // The focused text field that drives IME + plain input. It sits at the
        // cursor: invisible (no style, empty) when idle, and opaque while the IME
        // is composing so the OS draws the composition inline at the cursor.
        private void DrawImeField(Rect rect)
        {
            if (_native == null || Tid == 0) return;
            var cr = CursorPointRect(rect);
            // Visible only while composing — plain typing is flushed the same
            // frame, so it never needs to paint.
            bool show = _composing || !string.IsNullOrEmpty(Input.compositionString);

            GUI.SetNextControlName(InputControl);
            if (show)
            {
                float w = Mathf.Max(60f, rect.xMax - cr.x);
                _imeBuffer = GUI.TextField(new Rect(cr.x, cr.y, w, cr.height), _imeBuffer, ImeStyle());
            }
            else
            {
                _imeBuffer = GUI.TextField(new Rect(cr.x, cr.y, Mathf.Max(2f, cr.width), cr.height),
                    _imeBuffer, GUIStyle.none);
            }
            // Place the OS IME/candidate window just below the caret.
            Input.compositionCursorPos = GUIUtility.GUIToScreenPoint(new Vector2(cr.x, cr.yMax));
        }

        // An opaque, terminal-colored style so the inline composition is legible
        // over the rendered grid. Rebuilt when the theme changes.
        private GUIStyle ImeStyle()
        {
            if (_imeStyle != null) return _imeStyle;
            if (_imeBgTex == null)
            {
                _imeBgTex = new Texture2D(1, 1) { hideFlags = HideFlags.HideAndDontSave };
                _imeBgTex.SetPixel(0, 0, _bg);
                _imeBgTex.Apply();
            }
            else
            {
                _imeBgTex.SetPixel(0, 0, _bg);
                _imeBgTex.Apply();
            }
            _imeStyle = new GUIStyle(EditorStyles.label)
            {
                richText = false,
                padding = new RectOffset(1, 1, 0, 0),
                alignment = TextAnchor.MiddleLeft,
            };
            _imeStyle.normal.background = _imeBgTex;
            _imeStyle.normal.textColor = _fg;
            _imeStyle.focused.background = _imeBgTex;
            _imeStyle.focused.textColor = _fg;
            return _imeStyle;
        }

        // Send committed text (plain typing or a finished IME phrase) to the PTY
        // once per frame, clearing the hidden field without dropping focus.
        private void FlushIme()
        {
            if (Event.current.type != EventType.Repaint) return;
            if (_composing || string.IsNullOrEmpty(_imeBuffer)) return;
            if (_native == null || Tid == 0) { _imeBuffer = ""; return; }

            _native.SendText(Tid, _imeBuffer);
            _imeBuffer = "";
            // Clear the focused editor's buffer in place (keeps IME engaged).
            var te = (TextEditor)GUIUtility.GetStateObject(typeof(TextEditor), GUIUtility.keyboardControl);
            if (te != null) { te.text = ""; te.cursorIndex = 0; te.selectIndex = 0; }
        }

        private void DrawToolbar()
        {
            using (new GUILayout.HorizontalScope(EditorStyles.toolbar))
            {
                if (!_alive && GUILayout.Button("Restart", EditorStyles.toolbarButton, GUILayout.Width(60)))
                    Restart();

                GUILayout.FlexibleSpace();

                // Font size controls, pinned to the right edge.
                if (GUILayout.Button("-", EditorStyles.toolbarButton, GUILayout.Width(24)))
                    ChangeFont(-1f);
                if (GUILayout.Button("+", EditorStyles.toolbarButton, GUILayout.Width(24)))
                    ChangeFont(+1f);
            }
        }

        private void ChangeFont(float delta) => SetFont(_fontPt + delta);
        private void ResetFont() => SetFont(DefaultFontPt);

        private void SetFont(float points)
        {
            _fontPt = Mathf.Clamp(points, 8f, 32f);
            if (_native != null && Tid != 0)
            {
                _native.SetFontSize(Tid, _fontPt);
                RenderNow();
                Repaint();
            }
        }

        private void Restart()
        {
            if (_native == null) return;
            if (Tid != 0) _native.Destroy(Tid);
            _termIdRaw = 0;
            var (w, h) = CurrentPixelSize();
            _termIdRaw = (long)_native.Create(w, h, EditorGUIUtility.pixelsPerPoint, ProjectRoot);
            ApplyFont();
            _native.SetFontSize(Tid, _fontPt);
            ApplyTheme();
            _native.SetFocus(Tid, true);
            RenderNow();
            Repaint();
        }

        // Overlay-scrollbar geometry within the grid `rect`. Returns false (and
        // draws nothing) while pinned to the live bottom with no active drag, so
        // the bar stays out of the way until you scroll back. `offset` is lines
        // up from the bottom, `history` the total scrollback above the screen.
        private bool ScrollbarGeometry(Rect rect, out Rect track, out Rect thumb,
            out uint history, out uint offset, out uint screen)
        {
            track = thumb = default;
            history = offset = screen = 0;
            if (_native == null || Tid == 0) return false;
            _native.ScrollState(Tid, out history, out offset, out screen);
            if (history == 0 || screen == 0) return false;
            // Hidden at the bottom; revealed once scrolled, or while dragging.
            if (offset == 0 && !_draggingScroll) return false;

            float total = history + screen;
            track = new Rect(rect.xMax - ScrollbarWidth, rect.y, ScrollbarWidth, rect.height);
            float thumbH = Mathf.Clamp(track.height * (screen / total), ScrollbarMinThumb, track.height);
            // p = 0 at the top of history (offset == history), 1 at the live
            // bottom (offset == 0); the thumb travels over the leftover track.
            float p = history > 0 ? (history - offset) / (float)history : 1f;
            float y = track.y + p * (track.height - thumbH);
            thumb = new Rect(track.x, y, track.width, thumbH);
            return true;
        }

        // Drive the scrollbar from a mouse event. Returns true when the event was
        // consumed (so HandleInput stops before selection sees it).
        private bool HandleScrollbarInput(Rect rect, Event e)
        {
            bool visible = ScrollbarGeometry(rect, out var track, out var thumb,
                out uint history, out uint offset, out uint screen);

            if (e.type == EventType.MouseDown && e.button == 0 && visible &&
                track.Contains(e.mousePosition))
            {
                if (thumb.Contains(e.mousePosition))
                {
                    _draggingScroll = true;
                    _dragGrabY = e.mousePosition.y - thumb.y;
                }
                else
                {
                    // Page toward the click (a screen's worth of lines).
                    _native.Scroll(Tid, e.mousePosition.y < thumb.y ? (int)screen : -(int)screen);
                    RenderNow();
                }
                Repaint();
                e.Use();
                return true;
            }

            if (e.type == EventType.MouseDrag && e.button == 0 && _draggingScroll)
            {
                float travel = track.height - thumb.height;
                float p = travel > 0
                    ? Mathf.Clamp01((e.mousePosition.y - _dragGrabY - track.y) / travel)
                    : 0f;
                int desired = Mathf.RoundToInt(history * (1f - p));
                _native.Scroll(Tid, desired - (int)offset);
                RenderNow();
                Repaint();
                e.Use();
                return true;
            }

            if (e.type == EventType.MouseUp && e.button == 0 && _draggingScroll)
            {
                _draggingScroll = false;
                Repaint();
                e.Use();
                return true;
            }

            return false;
        }

        private void DrawScrollbar(Rect rect)
        {
            if (Event.current.type != EventType.Repaint) return;
            if (!ScrollbarGeometry(rect, out _, out var thumb, out _, out _, out _)) return;

            // A bare overlay thumb (no track), tuned to read over either theme.
            Color thumbCol = EditorGUIUtility.isProSkin
                ? new Color(1f, 1f, 1f, _draggingScroll ? 0.42f : 0.28f)
                : new Color(0f, 0f, 0f, _draggingScroll ? 0.38f : 0.24f);
            // Inset a touch so the thumb floats off the very edge.
            var t = new Rect(thumb.x + 1f, thumb.y + 1f, thumb.width - 2f, thumb.height - 2f);
            EditorGUI.DrawRect(t, thumbCol);
        }

        private void HandleInput(Rect rect)
        {
            if (_native == null || Tid == 0) return;
            var e = Event.current;

            // Scrollbar drag takes priority over selection: grabbing the thumb
            // scrolls to an absolute position, clicking the track pages, and a
            // live drag keeps following the pointer even off the thumb.
            if (HandleScrollbarInput(rect, e)) return;

            // Mouse-wheel scroll through scrollback (in lines).
            if (e.type == EventType.ScrollWheel && rect.Contains(e.mousePosition))
            {
                int lines = Mathf.RoundToInt(Mathf.Clamp(-e.delta.y, -5f, 5f));
                if (lines == 0) lines = e.delta.y > 0 ? -1 : 1;
                _native.Scroll(Tid, lines);
                RenderNow();
                Repaint();
                e.Use();
                return;
            }

            // Right-click (or Ctrl-click) inside the grid: a Copy/Paste menu.
            // Keeps the current selection intact so Copy has something to act on.
            if (e.type == EventType.ContextClick && rect.Contains(e.mousePosition))
            {
                ShowContextMenu();
                e.Use();
                return;
            }

            // Mouse selection (left button). MouseDown takes keyboard focus and
            // anchors a selection — single click by character, double by word,
            // triple by line; MouseDrag extends it; MouseUp finalizes (a plain
            // click with no drag clears any prior selection).
            if (e.type == EventType.MouseDown && e.button == 0 && rect.Contains(e.mousePosition))
            {
                Focus();
                _refocus = true;
                _selecting = true;
                _dragged = false;
                _selectMode = e.clickCount >= 3 ? (byte)2 : e.clickCount == 2 ? (byte)1 : (byte)0;
                var (px, py) = ToTermPx(rect, e.mousePosition);
                _native.SelectionStart(Tid, px, py, _selectMode);
                RenderNow();
                Repaint();
                e.Use();
                return;
            }
            if (e.type == EventType.MouseDrag && e.button == 0 && _selecting)
            {
                _dragged = true;
                var (px, py) = ToTermPx(rect, e.mousePosition);
                _native.SelectionUpdate(Tid, px, py);
                RenderNow();
                Repaint();
                e.Use();
                return;
            }
            if (e.type == EventType.MouseUp && e.button == 0 && _selecting)
            {
                _selecting = false;
                // A plain single click (no drag) clears the selection; a word/
                // line click or a drag keeps what it selected. (clickCount isn't
                // reliable on MouseUp, so use the mode recorded at MouseDown.)
                if (!_dragged && _selectMode == 0)
                {
                    _native.SelectionClear(Tid);
                    RenderNow();
                    Repaint();
                }
                e.Use();
                return;
            }

            if (e.type != EventType.KeyDown) return;

            // While composing, let every key reach the IME field (Enter commits,
            // arrows move the candidate, Backspace edits the composition).
            if (_composing) return;

            // While the terminal is focused it claims the keyboard: handle the
            // emulator-level Cmd shortcuts and swallow every other Cmd combo so
            // Unity's global shortcuts don't fire underneath. (macOS menu-bar
            // accelerators such as Cmd-S/W/Q are taken by the OS before the
            // window, so those still reach Unity. Cmd isn't a PTY modifier, so
            // unmapped combos simply stop here rather than going to the shell.)
            if (e.command)
            {
                switch (e.keyCode)
                {
                    case KeyCode.C:
                        // Copy the current selection (no-op if nothing selected;
                        // Ctrl-C for SIGINT is handled separately below).
                        string sel = _native.SelectionText(Tid);
                        if (!string.IsNullOrEmpty(sel))
                            EditorGUIUtility.systemCopyBuffer = sel;
                        break;
                    case KeyCode.V:
                        _native.Paste(Tid, EditorGUIUtility.systemCopyBuffer);
                        break;
                    case KeyCode.K:
                        _native.Clear(Tid);
                        break;
                    case KeyCode.Equals:      // Cmd-= and Cmd-+ (shifted '=')
                    case KeyCode.Plus:
                    case KeyCode.KeypadPlus:
                        ChangeFont(+1f);
                        break;
                    case KeyCode.Minus:
                    case KeyCode.KeypadMinus:
                        ChangeFont(-1f);
                        break;
                    case KeyCode.Alpha0:
                    case KeyCode.Keypad0:
                        ResetFont();
                        break;
                }
                e.Use();
                return;
            }

            // The Enter that commits an IME composition arrives after the Layout
            // that cleared compositionString, so `_composing` no longer guards it.
            // Swallow it here: the committed phrase is in `_imeBuffer` and flushes
            // this frame, and forwarding the Enter would send a CR ahead of it.
            if (_composeJustEnded &&
                (e.keyCode == KeyCode.Return || e.keyCode == KeyCode.KeypadEnter))
            {
                e.Use();
                return;
            }

            // Named special keys first (so Enter sends CR, not '\n').
            string special = SpecialKeyName(e.keyCode);
            if (special != null)
            {
                _native.SendKey(Tid, special, e.control, e.alt, e.shift);
                e.Use();
                return;
            }

            // Ctrl-combo (Ctrl-C, Ctrl-D, ...): encode from the physical key.
            if (e.control)
            {
                string name = CtrlComboName(e.keyCode, e.character);
                if (name != null)
                {
                    _native.SendKey(Tid, name, true, e.alt, e.shift);
                    e.Use();
                }
                return;
            }

            // Plain printable input is left for the hidden IME field, which
            // accumulates it (and any committed composition) for FlushIme().
        }

        // Right-click context menu: Copy the current selection (disabled when
        // nothing is selected) and Paste the system clipboard into the shell.
        private void ShowContextMenu()
        {
            if (_native == null || Tid == 0) return;
            var menu = new GenericMenu();

            string sel = _native.SelectionText(Tid);
            if (!string.IsNullOrEmpty(sel))
                menu.AddItem(new GUIContent("Copy"), false,
                    () => EditorGUIUtility.systemCopyBuffer = sel);
            else
                menu.AddDisabledItem(new GUIContent("Copy"));

            menu.AddItem(new GUIContent("Paste"), false, () =>
            {
                if (_native != null && Tid != 0)
                    _native.Paste(Tid, EditorGUIUtility.systemCopyBuffer);
            });

            menu.ShowAsContext();
        }

        // Map a GUI-point mouse position to physical pixels relative to the
        // terminal draw area's top-left (the coordinate space the native
        // selection/cursor APIs use).
        private static (float, float) ToTermPx(Rect rect, Vector2 mouse)
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            return ((mouse.x - rect.x) * ppp, (mouse.y - rect.y) * ppp);
        }

        private static string SpecialKeyName(KeyCode k)
        {
            switch (k)
            {
                case KeyCode.Return: case KeyCode.KeypadEnter: return "Enter";
                case KeyCode.Backspace: return "Backspace";
                case KeyCode.Tab: return "Tab";
                case KeyCode.Escape: return "Escape";
                case KeyCode.UpArrow: return "Up";
                case KeyCode.DownArrow: return "Down";
                case KeyCode.LeftArrow: return "Left";
                case KeyCode.RightArrow: return "Right";
                case KeyCode.Home: return "Home";
                case KeyCode.End: return "End";
                case KeyCode.PageUp: return "PageUp";
                case KeyCode.PageDown: return "PageDown";
                case KeyCode.Insert: return "Insert";
                case KeyCode.Delete: return "Delete";
                case KeyCode.F1: return "F1";
                case KeyCode.F2: return "F2";
                case KeyCode.F3: return "F3";
                case KeyCode.F4: return "F4";
                case KeyCode.F5: return "F5";
                case KeyCode.F6: return "F6";
                case KeyCode.F7: return "F7";
                case KeyCode.F8: return "F8";
                case KeyCode.F9: return "F9";
                case KeyCode.F10: return "F10";
                case KeyCode.F11: return "F11";
                case KeyCode.F12: return "F12";
                default: return null;
            }
        }

        // The character to control-encode for a Ctrl-<key> combo.
        private static string CtrlComboName(KeyCode k, char ch)
        {
            if (k >= KeyCode.A && k <= KeyCode.Z)
                return ((char)('a' + (k - KeyCode.A))).ToString();
            if (k >= KeyCode.Alpha0 && k <= KeyCode.Alpha9)
                return ((char)('0' + (k - KeyCode.Alpha0))).ToString();
            switch (k)
            {
                case KeyCode.LeftBracket: return "[";
                case KeyCode.RightBracket: return "]";
                case KeyCode.Backslash: return "\\";
                case KeyCode.Space: return " ";
                case KeyCode.Minus: return "-";
                case KeyCode.Slash: return "/";
                default:
                    return (ch != '\0' && ch >= ' ') ? ch.ToString() : null;
            }
        }
    }
}
