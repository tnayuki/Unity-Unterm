using System;
using System.Collections.Generic;
using System.IO;
using UnityEditor;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// The Claude Code agent panel. A single native "AgentView" object (see the
    /// `agentview` module) owns the agent session, the transcript panel, and the
    /// input composer; this <see cref="EditorWindow"/> is a thin host that only
    /// lays the view out, paces per-frame rendering, blits its textures, forwards
    /// raw input, and drives the OS clipboard + hidden IME field.
    ///
    /// The view lives in a process-global registry on the native side, so it
    /// (together with the loaded image) survives C# domain reloads: this window
    /// re-adopts the view by id after a reload and only tears it down when the
    /// window actually closes.
    /// </summary>
    public sealed class UntermAgentWindow : EditorWindow
    {
        private const float HeaderHeight = 22f;
        private const float InputHeight = 30f;
        private const float ScrollbarWidth = 13f;
        private const string InputControl = "UntermAgentInput";

        // IME: a hidden IMGUI field at the caret drives composition + plain typing;
        // committed text is flushed into the native input box each Repaint.
        private string _imeBuffer = "";
        private string _lastPreedit = ""; // last composition pushed to the editor
        private bool _composing;
        private bool _prevComposing;
        private bool _composeJustEnded;
        private bool _refocus;
        private GUIStyle _imeStyle;
        private GUIStyle _imeHidden; // transparent style so the IME field stays at the caret unseen
        private Texture2D _imeBgTex;

        private UntermNative _native;
        private string _status = "";

        // The AgentView lives in the native plugin (survives domain reloads),
        // referenced by a stable id we serialize and re-adopt.
        [SerializeField] private long _viewId;
        private static bool s_reloading;
        private ulong Vid => (ulong)_viewId;

        // Transcript panel texture (zero-copy wrap of the native MTLTexture).
        private Texture2D _tex;
        private IntPtr _externalTexPtr;

        // Input strip texture (the native input box renders the field AND the
        // Send/Stop button into this one surface).
        private Texture2D _inputTex;
        private IntPtr _inputExternalTexPtr;
        private float _inputHeight = InputHeight; // logical px, grows with content

        private float _scroll; // physical px, 0 = latest
        private bool _selecting;       // dragging a transcript selection
        private bool _inputDragging;   // dragging an input-box selection

        // Opened from the "Window/Unterm/Claude Code" menu (registered, and gated
        // on the CLI being installed, by ClaudeCode).
        public static void Open()
        {
            // A fresh window each time (CreateWindow, not the singleton GetWindow)
            // so several Claude Code conversations can run side by side; each gets
            // its own native AgentView and session (a session already open in
            // another window is skipped by the resume picker). Cascade off the
            // focused one so new windows don't stack exactly on top.
            var from = focusedWindow as UntermAgentWindow;
            var w = CreateWindow<UntermAgentWindow>();
            w.titleContent = new GUIContent("Claude Code");
            w.minSize = new Vector2(320, 200);

            Vector2 size = from != null ? from.position.size : new Vector2(420f, 520f);
            Vector2 origin = from != null
                ? from.position.position + new Vector2(30f, 30f)
                : new Vector2(140f, 140f);
            if (origin.x > 900f || origin.y > 600f) origin = new Vector2(140f, 140f);
            w.position = new Rect(origin, size);

            w.Show();
            w.Focus();
        }

        private static string ProjectRoot =>
            Directory.GetParent(Application.dataPath)?.FullName ?? Application.dataPath;

        private void OnEnable()
        {
            s_reloading = false;
            AssemblyReloadEvents.beforeAssemblyReload += OnBeforeReload;
            EditorApplication.update += OnEditorUpdate;
            LoadNative();
        }

        private void OnDisable()
        {
            AssemblyReloadEvents.beforeAssemblyReload -= OnBeforeReload;
            EditorApplication.update -= OnEditorUpdate;
            // On a domain reload keep the native view (and the loaded image) alive
            // so the conversation survives the recompile; only tear it all down
            // when the window is actually closing.
            Teardown(keepView: s_reloading);
        }

        private static void OnBeforeReload() => s_reloading = true;

        // Poll the view off the editor tick; the native side owns transcript /
        // status / animation, so we just react to its dirty/animating flags and
        // mirror the session-id into the picker index. (MCP tool calls are drained
        // globally by UntermMcp, not here.)
        private void OnEditorUpdate()
        {
            if (_native == null || _viewId == 0) return;

            uint f = _native.AgentviewPoll(Vid);
            bool dirty = (f & 1) != 0;
            if (dirty) { RenderView(measureInput: false); Repaint(); }
            else if ((f & 2) != 0) Repaint();

            // The native side owns the tab title (first user line).
            string agent = _native.AgentviewTitle(Vid);
            if (!string.IsNullOrEmpty(agent) && titleContent.text != agent)
                titleContent = new GUIContent(agent);
        }

        private void LoadNative()
        {
            try
            {
                // On Windows this maps the plugin into Unity's image and captures
                // its D3D device (for the zero-copy textures) before we bind below.
                UntermWindow.EnsureNativeImageLoaded();
                _native = new UntermNative();
                _native.Load(UntermWindow.PluginPath);

                var (pw, ph) = CurrentPanelSize();
                var (iw, ih) = CurrentInputSize();

                // Ensure the editor-global in-process MCP server is up (and its
                // tools published) before the session wires the agent to it.
                UntermMcp.EnsureStarted();

                // Re-adopt the existing view across reload, else start a fresh one.
                if (_viewId == 0 || !_native.AgentviewExists(Vid))
                {
                    _viewId = (long)_native.AgentviewCreate(ProjectRoot, pw, ph, iw, ih, ClaudeCode.ClaudePath);
                }

                ApplyFonts();
                _refocus = true; // park the IME field for typing
                RenderView();
            }
            catch (Exception e)
            {
                _status = "load failed: " + e.Message;
                Debug.LogError("[Unterm] " + e);
                Teardown(keepView: false);
            }
        }

        // Use the Unity editor's own bundled Inter faces (Regular + the
        // SemiBold/Italic variants) so Markdown bold/italic render real faces, not
        // synthesized ones. (Unity ships SemiBold, no "Bold".)
        private void ApplyFonts()
        {
            if (_native == null || _viewId == 0) return;
            string fontsDir = Path.Combine(EditorApplication.applicationContentsPath, "Resources/Fonts");
            string FontOrEmpty(string name)
            {
                string p = Path.Combine(fontsDir, name);
                return File.Exists(p) ? p : "";
            }
            _native.AgentviewSetFonts(
                Vid,
                FontOrEmpty("Inter-Regular.ttf"),
                FontOrEmpty("Inter-SemiBold.ttf"),
                FontOrEmpty("Inter-Italic.ttf"),
                FontOrEmpty("Inter-SemiBoldItalic.ttf"));
        }

        /// Tear down host-side resources. When <paramref name="keepView"/> is true
        /// (domain reload), leave the native view + loaded image alive so the
        /// conversation persists; otherwise destroy the view and unload.
        private void Teardown(bool keepView)
        {
            if (_tex != null)
            {
                DestroyImmediate(_tex);
                _tex = null;
            }
            if (_inputTex != null)
            {
                DestroyImmediate(_inputTex);
                _inputTex = null;
            }
            if (_imeBgTex != null)
            {
                DestroyImmediate(_imeBgTex);
                _imeBgTex = null;
                _imeStyle = null;
            }

            // Drop the field before disposing so a re-entrant update tick bails on
            // the `_native == null` guard instead of calling through nulled delegates.
            var native = _native;
            _native = null;
            if (!keepView)
            {
                ulong vid = Vid;
                // Don't destroy the view if another live window still holds the same
                // id: maximizing a tab makes Unity spin up a transient duplicate
                // UntermAgentWindow sharing this view, and destroying that duplicate
                // (on un-maximize) would otherwise kill the conversation the original
                // window is still showing.
                bool ownedElsewhere = AnyOtherWindowOwns(vid);
                if (native != null && vid != 0 && !ownedElsewhere)
                    native.AgentviewDestroy(vid);
                _viewId = 0;
                native?.Dispose(); // dlclose on real teardown
            }
            // On reload: drop the managed wrapper WITHOUT dlclose so the native
            // image (and its view globals) stay mapped for re-adoption.
        }

        // True if a UntermAgentWindow other than this one holds view id `vid` — i.e.
        // a sibling (e.g. the duplicate created while a tab is maximized) still owns
        // the view, so this window's teardown must not destroy it.
        private bool AnyOtherWindowOwns(ulong vid)
        {
            if (vid == 0) return false;
            foreach (var w in Resources.FindObjectsOfTypeAll<UntermAgentWindow>())
                if (w != this && (ulong)w._viewId == vid) return true;
            return false;
        }

        private const float InputPad = 6f;
        private const float InputMaxHeight = 100f; // grow up to ~4 lines, then scroll
        // Logical width reserved on the right of the strip for the Rust-drawn
        // Send/Stop button (≈28px button + padding), so the opaque IME field is
        // never laid over it.
        private const float SendButtonReserve = 40f;

        // Physical (HiDPI) pixel size of the transcript panel surface.
        private (uint, uint) CurrentPanelSize()
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            uint w = (uint)Mathf.Max(1, Mathf.RoundToInt(position.width * ppp));
            uint h = (uint)Mathf.Max(
                1, Mathf.RoundToInt((position.height - HeaderHeight - _inputHeight) * ppp));
            return (w, h);
        }

        // Logical width of the input strip (the native surface spans field +
        // button area, i.e. the whole bottom strip minus side padding).
        private float InputStripWidth() => Mathf.Max(1f, position.width - InputPad * 2f);

        // Physical (HiDPI) pixel size of the input strip surface (includes the
        // Send/Stop button drawn by Rust on the right).
        private (uint, uint) CurrentInputSize()
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            float fieldW = InputStripWidth();
            float fieldH = Mathf.Max(1f, _inputHeight - InputPad);
            uint w = (uint)Mathf.Max(1, Mathf.RoundToInt(fieldW * ppp));
            uint h = (uint)Mathf.Max(1, Mathf.RoundToInt(fieldH * ppp));
            return (w, h);
        }

        // Apply theme + scroll + size, re-render natively, then refresh both
        // external textures. `measureInput` re-fits the input strip to its content
        // (auto-grow); pass false for panel-only redraws (scroll, agent output) so
        // they don't disturb the input height.
        private void RenderView(bool measureInput = true)
        {
            if (_native == null || _viewId == 0) return;

            float ppp = EditorGUIUtility.pixelsPerPoint;
            var (pw, ph) = CurrentPanelSize();
            var (iw, ih) = CurrentInputSize();
            _native.AgentviewResize(Vid, pw, ph, iw, ih, ppp);

            Color bg = GetEditorBackground();
            Color32 fg = EditorGUIUtility.isProSkin
                ? new Color32(210, 210, 214, 255)
                : new Color32(32, 32, 32, 255);
            // Native clears in linear space; the sRGB target re-encodes on store.
            _native.AgentviewSetTheme(Vid, bg.linear, fg);

            _native.AgentviewSetScroll(Vid, _scroll);
            _native.AgentviewRender(Vid);

            UploadPanel((int)pw, (int)ph);
            UploadInput((int)iw, (int)ih);

            // Auto-grow the input strip to fit its content (capped). Only when the
            // input may have changed — never on a panel-only redraw, so scrolling
            // the transcript can't jiggle the input height.
            if (measureInput)
            {
                float contentLogical = _native.AgentviewInputHeight(Vid) / Mathf.Max(0.01f, ppp);
                float target = Mathf.Clamp(contentLogical + InputPad, InputHeight, InputMaxHeight);
                if (Mathf.Abs(target - _inputHeight) > 0.5f)
                {
                    _inputHeight = target;
                    Repaint();
                }
            }
        }

        private static Color GetEditorBackground()
        {
            // Prefer Unity's exact themed background; fall back to standard grays.
            var m = typeof(EditorGUIUtility).GetMethod(
                "GetDefaultBackgroundColor",
                System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Static);
            if (m != null && m.ReturnType == typeof(Color))
                return (Color)m.Invoke(null, null);

            return EditorGUIUtility.isProSkin
                ? (Color)new Color32(56, 56, 56, 255)
                : (Color)new Color32(194, 194, 194, 255);
        }

        // Wrap the native transcript texture directly — no CPU copy (zero-copy
        // only, like the terminal). The pointer alternates on Windows' double
        // buffer, so re-wrap whenever it changes; null means the frame isn't ready
        // yet (e.g. Windows D3D device not captured) — skip this tick.
        private void UploadPanel(int iw, int ih)
        {
            IntPtr texPtr = _native.AgentviewPanelTexture(Vid);
            if (texPtr == IntPtr.Zero) return;

            if (_tex == null || _tex.width != iw || _tex.height != ih || _externalTexPtr != texPtr)
            {
                if (_tex != null) DestroyImmediate(_tex);
                _tex = Texture2D.CreateExternalTexture(
                    iw, ih, TextureFormat.RGBA32, false, false, texPtr);
                _tex.filterMode = FilterMode.Bilinear;
                _tex.hideFlags = HideFlags.HideAndDontSave;
                _externalTexPtr = texPtr;
            }
            else
            {
                _tex.UpdateExternalTexture(texPtr);
            }
        }

        // Wrap the native input strip texture (zero-copy only); re-wrap when the
        // pointer changes (Windows double buffer), skip a null frame.
        private void UploadInput(int iw, int ih)
        {
            IntPtr texPtr = _native.AgentviewInputTexture(Vid);
            if (texPtr == IntPtr.Zero) return;

            if (_inputTex == null || _inputTex.width != iw || _inputTex.height != ih
                || _inputExternalTexPtr != texPtr)
            {
                if (_inputTex != null) DestroyImmediate(_inputTex);
                _inputTex = Texture2D.CreateExternalTexture(
                    iw, ih, TextureFormat.RGBA32, false, false, texPtr);
                _inputTex.filterMode = FilterMode.Bilinear;
                _inputTex.hideFlags = HideFlags.HideAndDontSave;
                _inputExternalTexPtr = texPtr;
            }
            else
            {
                _inputTex.UpdateExternalTexture(texPtr);
            }
        }

        private void OnGUI()
        {
            // Snapshot IME composition at Layout so key handling is stable; a
            // non-empty -> empty transition flags the frame that committed a phrase.
            if (Event.current.type == EventType.Layout)
            {
                bool now = !string.IsNullOrEmpty(Input.compositionString);
                _composeJustEnded = _prevComposing && !now;
                _prevComposing = now;
                _composing = now;
            }
            // Preedit/commit is synced in SyncIme (after DrawImeField), so it can
            // include any segments the IME has already committed into the field.

            DrawHeader();

            // Keystrokes go to the native input box; do this before the panel/IMGUI
            // controls so Enter/arrows aren't eaten by them.
            HandleInputKeys();

            // Follow window resizing / input auto-grow: re-render whenever the draw
            // area no longer matches the current textures.
            var (cw, ch) = CurrentPanelSize();
            var (ciw, cih) = CurrentInputSize();
            if (_native != null && _viewId != 0 &&
                (_tex == null || _tex.width != (int)cw || _tex.height != (int)ch ||
                 _inputTex == null || _inputTex.width != (int)ciw || _inputTex.height != (int)cih))
            {
                RenderView();
            }

            var rect = new Rect(0, HeaderHeight, position.width,
                position.height - HeaderHeight - _inputHeight);

            // Mouse-wheel scroll through history (offset is in physical px).
            // Horizontal wheel/swipe over a code block scrolls that block instead.
            if (Event.current.type == EventType.ScrollWheel && rect.Contains(Event.current.mousePosition))
            {
                var we = Event.current;
                float ppp = EditorGUIUtility.pixelsPerPoint;
                float step = 24f * ppp;
                bool used = false;
                if (Mathf.Abs(we.delta.x) > 0.01f && _native != null && _viewId != 0)
                {
                    float lx = (we.mousePosition.x - rect.x) * ppp;
                    float ly = (we.mousePosition.y - rect.y) * ppp;
                    used = _native.AgentviewPanelScrollH(Vid, lx, ly, we.delta.x * step) == 1;
                }
                if (Mathf.Abs(we.delta.y) > 0.01f)
                {
                    _scroll = Mathf.Clamp(_scroll - we.delta.y * step, 0f, MaxScroll());
                    used = true;
                }
                if (used)
                {
                    RenderView(measureInput: false);
                    Repaint();
                    we.Use();
                }
            }

            HandlePanelMouse(rect);

            if (_tex != null)
            {
                // Native frame is top-down; Texture2D samples bottom-up, so flip V.
                GUI.DrawTextureWithTexCoords(rect, _tex, new Rect(0, 1, 1, -1));
                DrawScrollbar(rect);
            }
            else
            {
                EditorGUI.LabelField(rect, _status, EditorStyles.centeredGreyMiniLabel);
            }

            DrawInput();

            // The hidden IME field overlays the caret; flush committed text into
            // the native input box, then keep it focused for the next keystroke.
            DrawImeField(InputStripRect());
            SyncIme();
            if (_refocus && Event.current.type == EventType.Repaint)
            {
                EditorGUI.FocusTextInControl(InputControl);
                _refocus = false;
            }
        }

        // Transcript mouse: down resolves permission buttons AND begins selection
        // internally (Rust); drag extends the selection; right-click opens a menu.
        private void HandlePanelMouse(Rect rect)
        {
            if (_native == null || _viewId == 0) return;
            var e = Event.current;
            if (!rect.Contains(e.mousePosition) && e.type != EventType.MouseDrag && e.type != EventType.MouseUp)
                return;

            // The scrollbar strip belongs to DrawScrollbar's GUI.VerticalScrollbar;
            // don't swallow a click there or the bar can never be grabbed.
            if (e.type == EventType.MouseDown && MaxScroll() > 0.5f
                && e.mousePosition.x >= rect.xMax - ScrollbarWidth)
                return;

            float ppp = EditorGUIUtility.pixelsPerPoint;
            float lx = (e.mousePosition.x - rect.x) * ppp;
            float ly = (e.mousePosition.y - rect.y) * ppp;

            switch (e.type)
            {
                case EventType.MouseDown when e.button == 0:
                    _native.AgentviewPanelDown(Vid, lx, ly);
                    _selecting = true;
                    RenderView(measureInput: false); Repaint(); e.Use();
                    break;

                case EventType.MouseDrag when _selecting:
                    _native.AgentviewPanelDrag(Vid, lx, ly);
                    RenderView(measureInput: false); Repaint(); e.Use();
                    break;

                case EventType.MouseUp when _selecting:
                    _selecting = false;
                    e.Use();
                    break;

                case EventType.ContextClick:
                    ShowContextMenu();
                    e.Use();
                    break;
            }
        }

        private void ShowContextMenu()
        {
            var menu = new GenericMenu();
            if (_native != null && _viewId != 0 && _native.AgentviewPanelHasSelection(Vid))
                menu.AddItem(new GUIContent("Copy"), false,
                    () => CopyToClipboard(_native.AgentviewPanelSelectedText(Vid)));
            else
                menu.AddDisabledItem(new GUIContent("Copy"));
            menu.AddItem(new GUIContent("Select All"), false, () =>
            {
                _native.AgentviewPanelSelectAll(Vid); RenderView(measureInput: false); Repaint();
            });
            menu.ShowAsContext();
        }

        private static void CopyToClipboard(string text)
        {
            if (!string.IsNullOrEmpty(text)) EditorGUIUtility.systemCopyBuffer = text;
        }

        // Max scroll offset (physical px): content beyond the viewport.
        private float MaxScroll()
        {
            if (_native == null || _viewId == 0) return 0f;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            float viewportH = (position.height - HeaderHeight - _inputHeight) * ppp;
            return Mathf.Max(0f, _native.AgentviewContentHeight(Vid) - viewportH);
        }

        // Draggable scrollbar on the right edge (physical px; 0 = top, max = latest).
        private void DrawScrollbar(Rect panelRect)
        {
            if (_native == null || _viewId == 0) return;

            float ppp = EditorGUIUtility.pixelsPerPoint;
            float viewportH = panelRect.height * ppp; // physical
            float totalH = _native.AgentviewContentHeight(Vid);
            float maxScroll = Mathf.Max(0f, totalH - viewportH);
            if (maxScroll <= 0.5f)
            {
                _scroll = 0f;
                return;
            }

            _scroll = Mathf.Clamp(_scroll, 0f, maxScroll);
            var sbRect = new Rect(panelRect.xMax - ScrollbarWidth, panelRect.y, ScrollbarWidth, panelRect.height);
            float value = maxScroll - _scroll; // bottom -> value at max

            EditorGUI.BeginChangeCheck();
            float nv = GUI.VerticalScrollbar(sbRect, value, viewportH, 0f, totalH);
            if (EditorGUI.EndChangeCheck())
            {
                _scroll = Mathf.Clamp(maxScroll - nv, 0f, maxScroll);
                RenderView(measureInput: false);
                Repaint();
            }
        }

        // Forward keystrokes to the native input box. Enter sends, Shift+Enter
        // newlines (both resolved in Rust); Esc interrupts a running turn; clipboard
        // shortcuts route through the native box.
        private void HandleInputKeys()
        {
            if (_native == null || _viewId == 0) return;
            var e = Event.current;
            if (e.type != EventType.KeyDown) return;

            // While composing, let every key reach the IME field (Enter commits,
            // arrows move the candidate, Backspace edits the composition).
            if (_composing) return;

            // The Enter that commits a composition arrives the frame after
            // compositionString cleared; swallow it so it doesn't also send.
            if (_composeJustEnded &&
                (e.keyCode == KeyCode.Return || e.keyCode == KeyCode.KeypadEnter))
            {
                e.Use();
                return;
            }

            // Cmd/Ctrl+C with a transcript selection copies that (transcript takes
            // precedence over the input box); leave the selection untouched.
            if ((e.command || e.control) && e.keyCode == KeyCode.C
                && _native.AgentviewPanelHasSelection(Vid))
            {
                CopyToClipboard(_native.AgentviewPanelSelectedText(Vid));
                e.Use();
                return;
            }

            // Any other key targets the input box, so it takes selection focus:
            // drop the transcript selection so only one highlight is active.
            FocusInput();

            if (e.keyCode == KeyCode.Escape)
            {
                _native.AgentviewInterrupt(Vid);
                e.Use();
                return;
            }

            // Clipboard shortcuts (Cmd/Ctrl + V/C/X/A/Z).
            if (e.command || e.control)
            {
                switch (e.keyCode)
                {
                    case KeyCode.V:
                        _native.AgentviewInputInsert(Vid, EditorGUIUtility.systemCopyBuffer);
                        RenderView(); Repaint(); e.Use();
                        return;
                    case KeyCode.C:
                    {
                        string s = _native.AgentviewInputCopy(Vid);
                        if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                        e.Use();
                        return;
                    }
                    case KeyCode.X:
                    {
                        string s = _native.AgentviewInputCut(Vid);
                        if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                        RenderView(); Repaint(); e.Use();
                        return;
                    }
                    case KeyCode.A:
                        _native.AgentviewInputSelectAll(Vid);
                        RenderView(); Repaint(); e.Use();
                        return;
                    case KeyCode.Z when !e.shift:
                        _native.AgentviewInputUndo(Vid);
                        RenderView(); Repaint(); e.Use();
                        return;
                    case KeyCode.Z when e.shift:
                        _native.AgentviewInputRedo(Vid);
                        RenderView(); Repaint(); e.Use();
                        return;
                }
            }

            // Enter/editing keys: hand them to Rust (Enter=send, Shift+Enter=newline,
            // the rest are caret/edit operations). Plain printable input is left to
            // the hidden IME field.
            string name = e.keyCode switch
            {
                KeyCode.Return => "Return",
                KeyCode.KeypadEnter => "Return",
                KeyCode.Backspace => "Backspace",
                KeyCode.Delete => "Delete",
                KeyCode.LeftArrow => "LeftArrow",
                KeyCode.RightArrow => "RightArrow",
                KeyCode.UpArrow => "UpArrow",
                KeyCode.DownArrow => "DownArrow",
                KeyCode.Home => "Home",
                KeyCode.End => "End",
                _ => null,
            };
            if (name != null)
            {
                _native.AgentviewInputKey(Vid, name, e.control, e.alt, e.shift);
                if (name == "Return" && !e.shift) _scroll = 0f; // jump to latest on send
                RenderView();
                Repaint();
                e.Use();
                return;
            }
        }

        // Mouse on the input strip: click places the caret (or hits the Send/Stop
        // button, resolved in Rust), drag selects, double/triple click selects
        // word/line, wheel scrolls, right-click opens a menu.
        private void HandleInputMouse(Rect stripRect)
        {
            if (_native == null || _viewId == 0) return;
            var e = Event.current;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            float lx = (e.mousePosition.x - stripRect.x) * ppp;
            float ly = (e.mousePosition.y - stripRect.y) * ppp;

            switch (e.type)
            {
                case EventType.MouseDown when e.button == 0 && stripRect.Contains(e.mousePosition):
                    byte kind = e.clickCount >= 3 ? (byte)3 : e.clickCount == 2 ? (byte)2 : (byte)0;
                    byte hit = _native.AgentviewInputDown(Vid, lx, ly, kind);
                    if (hit == 1)
                    {
                        // The Send/Stop action already ran in Rust; do not drag.
                        _scroll = 0f; // a send jumps to latest
                        RenderView(); Repaint(); e.Use();
                        break;
                    }
                    _inputDragging = true;
                    GUIUtility.keyboardControl = 0;
                    Focus();
                    _refocus = true; // re-park the IME field for typing
                    FocusInput();    // clicking the input drops the transcript selection
                    RenderView(); Repaint(); e.Use();
                    break;
                case EventType.MouseDrag when _inputDragging:
                    _native.AgentviewInputDrag(Vid, lx, ly);
                    RenderView(); Repaint(); e.Use();
                    break;
                case EventType.MouseUp when _inputDragging:
                    _inputDragging = false; e.Use();
                    break;
                // The composer doesn't free-scroll: it's capped (~4 lines) and the
                // editor auto-follows the caret while typing. A wheel over it does
                // nothing (the transcript is the scrollable area).
                case EventType.ContextClick when stripRect.Contains(e.mousePosition):
                    ShowInputContextMenu();
                    e.Use();
                    break;
            }
        }

        private void ShowInputContextMenu()
        {
            var menu = new GenericMenu();
            // Probe selection via copy (returns "" when there's no selection).
            bool hasSel = !string.IsNullOrEmpty(_native.AgentviewInputCopy(Vid));
            if (hasSel)
            {
                menu.AddItem(new GUIContent("Copy"), false, () =>
                {
                    string s = _native.AgentviewInputCopy(Vid);
                    if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                });
                menu.AddItem(new GUIContent("Cut"), false, () =>
                {
                    string s = _native.AgentviewInputCut(Vid);
                    if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                    RenderView(); Repaint();
                });
            }
            else
            {
                menu.AddDisabledItem(new GUIContent("Copy"));
                menu.AddDisabledItem(new GUIContent("Cut"));
            }
            menu.AddItem(new GUIContent("Paste"), false, () =>
            {
                _native.AgentviewInputInsert(Vid, EditorGUIUtility.systemCopyBuffer);
                RenderView(); Repaint();
            });
            menu.AddItem(new GUIContent("Select All"), false, () =>
            {
                _native.AgentviewInputSelectAll(Vid);
                RenderView(); Repaint();
            });
            menu.ShowAsContext();
        }

        // Selection focus moves to the input: drop any transcript selection so a
        // single highlight (and a single Cmd+C target) is active at a time.
        private void FocusInput()
        {
            if (_native != null && _viewId != 0 && _native.AgentviewPanelHasSelection(Vid))
            {
                _native.AgentviewPanelSelectClear(Vid);
                RenderView();
            }
        }

        // The input strip rect (where the native input texture is blitted), shared
        // by DrawInput, the mouse handler, and the IME field. Spans the whole
        // bottom strip width (field + Send/Stop button) minus side padding.
        private Rect InputStripRect()
        {
            var strip = new Rect(0, position.height - _inputHeight, position.width, _inputHeight);
            return new Rect(strip.x + InputPad, strip.y + InputPad / 2f,
                InputStripWidth(), strip.height - InputPad);
        }

        private void DrawInput()
        {
            var strip = new Rect(0, position.height - _inputHeight, position.width, _inputHeight);
            EditorGUI.DrawRect(new Rect(strip.x, strip.y, strip.width, 1f),
                EditorGUIUtility.isProSkin ? new Color(0, 0, 0, 0.4f) : new Color(0, 0, 0, 0.15f));

            var stripRect = InputStripRect();
            if (_inputTex != null)
                GUI.DrawTextureWithTexCoords(stripRect, _inputTex, new Rect(0, 1, 1, -1));

            // The Send/Stop button is drawn inside the input texture by Rust; the
            // host only forwards the click via AgentviewInputDown.
            HandleInputMouse(stripRect);
        }

        // The hidden IMGUI field that drives IME + plain typing. The composition
        // itself is shown inline by the editor (as preedit/marked text), so this
        // field is always invisible and — while composing — parked offscreen so
        // the OS doesn't also draw the composition. It still receives the OS
        // composition + committed text; SyncIme reads both and commits the latter.
        private void DrawImeField(Rect stripRect)
        {
            if (_native == null || _viewId == 0) return;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            _native.AgentviewCaret(Vid, out float cx, out float cy, out float _, out float chh);
            float gx = stripRect.x + cx / ppp;
            float gy = stripRect.y + cy / ppp;
            float gh = Mathf.Max(14f, chh / ppp);

            GUI.SetNextControlName(InputControl);
            // The editor IME anchors the candidate window to this field's caret (not
            // to compositionCursorPos), so the field must sit AT the caret, never
            // parked off-screen. Its text is fully transparent (ImeHidden) so the OS
            // inline composition stays hidden behind the natively drawn preedit.
            bool composing = _composing || !string.IsNullOrEmpty(Input.compositionString);
            var style = ImeHidden();
            if (composing)
            {
                // Right-align so the (invisible) marked text ENDS at the caret,
                // pinning the candidate anchor at the composition start regardless of
                // text length or font — otherwise it drifts right as you type. A tiny
                // field misplaces the anchor, so it must span toward the window edge.
                style.alignment = TextAnchor.UpperRight;
                float w = Mathf.Max(120f, gx);
                _imeBuffer = GUI.TextField(new Rect(gx - w, gy, w, gh), _imeBuffer, style);
            }
            else
            {
                // Idle it's 2px so it doesn't intercept composer clicks.
                style.alignment = TextAnchor.UpperLeft;
                _imeBuffer = GUI.TextField(new Rect(gx, gy, 2f, gh), _imeBuffer, style);
            }
            // compositionCursorPos is in the focused window's LOCAL GUI space, not
            // screen space — GUIToScreenPoint added the window's desktop offset, so the
            // candidate was only correct with the window at the screen's top-left. Pass
            // the window-local point, a line below the caret so it clears the preedit.
            // Only the focused window may move it (the position is process-global).
            if (focusedWindow == this)
                Input.compositionCursorPos = new Vector2(gx, gy + gh * 1.5f);
        }

        // A style whose text is fully transparent in every state, so the IME field
        // (and the inline marked text Unity draws into it) is invisible while the
        // field still occupies the caret position the candidate window anchors to.
        private GUIStyle ImeHidden()
        {
            if (_imeHidden == null)
            {
                _imeHidden = new GUIStyle(GUIStyle.none);
                var clear = new Color(0f, 0f, 0f, 0f);
                _imeHidden.normal.textColor = clear;
                _imeHidden.focused.textColor = clear;
                _imeHidden.hover.textColor = clear;
                _imeHidden.active.textColor = clear;
            }
            return _imeHidden;
        }

        // Sync IME state into the native input box each frame.
        //
        // While composing, show the segments the IME has already committed into the
        // field PLUS the active composition together as inline preedit (marked
        // text). The field text (`_imeBuffer`) holds segments confirmed mid-phrase
        // (e.g. after a 漢字 conversion) while `Input.compositionString` holds the
        // segment still being edited; showing only the latter made a converted
        // segment appear to vanish as you kept typing. Nothing is inserted for real
        // until the phrase commits, and the field is never mutated mid-composition
        // (which would disturb the OS IME).
        //
        // Once composition ends (or for plain typing), drop the marked text and
        // commit the field for real, once, without dropping focus.
        private void SyncIme()
        {
            if (_native == null || _viewId == 0) return;

            // IME state (Input.compositionString) is process-global, so an
            // unfocused window must NOT mirror it — otherwise the 変換中 preedit
            // appears in every open agent window. Clear any preedit this window
            // left and bail; only the focused window drives composition.
            if (focusedWindow != this)
            {
                if (!string.IsNullOrEmpty(_lastPreedit))
                {
                    _native.AgentviewInputSetPreedit(Vid, "");
                    _lastPreedit = "";
                    RenderView();
                    Repaint();
                }
                return;
            }

            if (_composing)
            {
                string marked = _imeBuffer + Input.compositionString;
                if (marked != _lastPreedit)
                {
                    _lastPreedit = marked;
                    _native.AgentviewInputSetPreedit(Vid, marked);
                    RenderView();
                    Repaint();
                }
                return;
            }

            if (Event.current.type != EventType.Repaint) return;

            if (!string.IsNullOrEmpty(_lastPreedit))
            {
                _native.AgentviewInputSetPreedit(Vid, "");
                _lastPreedit = "";
            }
            if (string.IsNullOrEmpty(_imeBuffer)) return;

            _native.AgentviewInputInsert(Vid, _imeBuffer);
            _imeBuffer = "";
            var te = (TextEditor)GUIUtility.GetStateObject(typeof(TextEditor), GUIUtility.keyboardControl);
            if (te != null) { te.text = ""; te.cursorIndex = 0; te.selectIndex = 0; }
            FocusInput(); // typing drops the transcript selection
            RenderView();
            Repaint();
        }

        // Opaque, themed style so the inline composition is legible over the box.
        private GUIStyle ImeStyle()
        {
            Color bg = GetEditorBackground();
            Color32 fg = EditorGUIUtility.isProSkin
                ? new Color32(210, 210, 214, 255)
                : new Color32(32, 32, 32, 255);
            if (_imeBgTex == null)
                _imeBgTex = new Texture2D(1, 1) { hideFlags = HideFlags.HideAndDontSave };
            _imeBgTex.SetPixel(0, 0, bg);
            _imeBgTex.Apply();
            if (_imeStyle == null)
            {
                _imeStyle = new GUIStyle(EditorStyles.label)
                {
                    richText = false,
                    padding = new RectOffset(1, 1, 0, 0),
                    alignment = TextAnchor.MiddleLeft,
                };
            }
            _imeStyle.normal.background = _imeBgTex;
            _imeStyle.normal.textColor = fg;
            _imeStyle.focused.background = _imeBgTex;
            _imeStyle.focused.textColor = fg;
            return _imeStyle;
        }

        // A thin header with the session picker: the current conversation's title
        // and a dropdown to start a new one or switch to another (sessions already
        // open in another window are greyed out).
        private void DrawHeader()
        {
            using (new GUILayout.HorizontalScope(EditorStyles.toolbar))
            {
                GUILayout.Label(CurrentTitle(), EditorStyles.toolbarButton);
                GUILayout.FlexibleSpace();
            }
        }

        private string CurrentTitle()
        {
            string t = (_native != null && _viewId != 0) ? _native.AgentviewTitle(Vid) : "";
            return string.IsNullOrEmpty(t) ? "New conversation" : t;
        }
    }
}
