using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using UnityEditor;
using UnityEditor.ShortcutManagement;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// User-configurable code-editor settings, persisted in <see cref="EditorPrefs"/>
    /// (shared across projects, like the editor's other preferences). Surfaced in
    /// "Preferences ▸ Unterm" and applied to each native editor as it opens.
    /// </summary>
    internal static class UntermCodeEditorPrefs
    {
        private const string UndoLimitKey = "Unterm.CodeEditor.UndoLimit";

        /// Default retained undo steps (mirrors the native default).
        public const int DefaultUndoLimit = 500;

        /// Cap on retained undo steps (0 = unlimited). A line operation can hold a
        /// whole replaced block, so this bounds the editor's memory over a long
        /// session.
        public static int UndoLimit
        {
            get => Mathf.Max(0, EditorPrefs.GetInt(UndoLimitKey, DefaultUndoLimit));
            set => EditorPrefs.SetInt(UndoLimitKey, Mathf.Max(0, value));
        }
    }

    /// <summary>
    /// A syntax-highlighted code editor window. A single native "EditorView" (the
    /// `editorview` module — an InputBox in code-editor mode: tree-sitter
    /// highlighting + a line-number gutter) owns the editing surface; this
    /// <see cref="EditorWindow"/> is a thin host that lays it out, blits its
    /// texture, forwards raw input, drives the OS clipboard + hidden IME field,
    /// and reads/writes the file on disk.
    ///
    /// The view lives in a process-global native registry, so unsaved edits
    /// survive C# domain reloads (re-adopted by a serialized id). It is opened by
    /// right-clicking a script asset, from Window ▸ Unterm ▸ Code Editor, or — when
    /// enabled in Preferences — by double-clicking a file.
    /// </summary>
    public sealed class UntermCodeEditorWindow : EditorWindow, IHasCustomMenu
    {
        private const string InputControl = "UntermCodeEditorInput";

        private UntermNative _native;
        private string _status = "";

        // The EditorView lives in the native plugin (survives reloads), referenced
        // by a stable id we serialize and re-adopt.
        [SerializeField] private long _editorId;
        private static bool s_reloading;
        private ulong Eid => (ulong)_editorId;

        // The file being edited and its on-disk line ending. Buffer text is kept LF
        // internally; we restore the original ending on save.
        [SerializeField] private string _filePath = "";
        [SerializeField] private bool _crlf;
        [SerializeField] private string _langToken = "";
        [SerializeField] private bool _dirty;
        // The native editor's document version (see unterm_editor_edit_serial) as of
        // the last load/save. `_dirty` is recomputed by comparing the live version to
        // this — undo/redo restore versions, so undoing back to the saved state reads
        // clean, with no second copy of the buffer (just this u64).
        [SerializeField] private ulong _savedSerial;

        // Editing surface texture (zero-copy wrap of the native MTLTexture).
        private Texture2D _tex;
        private IntPtr _externalTexPtr;

        // IME: a hidden IMGUI field at the caret drives composition + plain typing;
        // committed text is flushed into the native editor each Repaint.
        private string _imeBuffer = "";
        private string _lastPreedit = "";
        private bool _composing;
        private bool _prevComposing;
        private bool _composeJustEnded;
        private bool _refocus;
        private GUIStyle _imeHidden;
        private bool _mouseDragging;

        // Find / replace / goto bar overlay.
        private enum BarMode { None, Find, Replace, Goto }
        private BarMode _bar = BarMode.None;
        private string _findText = "", _replaceText = "", _gotoText = "";
        private bool _findCase;
        private string _findStatus = "";
        private bool _focusBar;
        private const string FindField = "UntermFindField";
        private const float ScrollbarWidth = 13f;

        // Shortcut hints for menu labels, with a space between each modifier and key.
#if UNITY_EDITOR_OSX
        private const string KCmd = "⌘", KAlt = "⌥", KShift = "⇧", KSep = " ";
#else
        private const string KCmd = "Ctrl", KAlt = "Alt", KShift = "Shift", KSep = " + ";
#endif

        // Last on-disk modification time, to detect external edits.
        [SerializeField] private long _fileTicks;

        // --- opening -----------------------------------------------------------

        [MenuItem("Window/Unterm/Code Editor")]
        public static void OpenEmpty()
        {
            var w = CreateWindow<UntermCodeEditorWindow>();
            w.titleContent = new GUIContent("Code Editor");
            w.minSize = new Vector2(320, 200);
            w.Show();
            w.Focus();
        }

        [MenuItem("Assets/Open in Unterm Code Editor", false, 19)]
        private static void OpenSelected()
        {
            var path = AssetDatabase.GetAssetPath(Selection.activeObject);
            if (!string.IsNullOrEmpty(path)) OpenPath(path);
        }

        [MenuItem("Assets/Open in Unterm Code Editor", true)]
        private static bool OpenSelectedValidate()
        {
            var path = AssetDatabase.GetAssetPath(Selection.activeObject);
            return !string.IsNullOrEmpty(path) && File.Exists(path) && !Directory.Exists(path);
        }

        // Double-click hijack: only when enabled in Preferences and the asset is an
        // editable text file. Returning true suppresses the default external editor.
        [UnityEditor.Callbacks.OnOpenAsset(0)]
        private static bool OnOpen(int instanceID, int line)
        {
            if (!UntermCodeEditorPrefs.HijackDoubleClick) return false;
            string path = AssetDatabase.GetAssetPath(instanceID);
            if (string.IsNullOrEmpty(path) || Directory.Exists(path) || !File.Exists(path))
                return false;
            if (!IsEditable(path)) return false;
            OpenPath(path);
            return true;
        }

        // Text files we'll open on double-click (don't hijack binary assets).
        private static readonly string[] s_textExt =
        {
            ".cs", ".txt", ".json", ".xml", ".uxml", ".uss", ".shader", ".cginc",
            ".hlsl", ".compute", ".md", ".markdown", ".yml", ".yaml", ".js", ".ts",
            ".py", ".rs", ".toml", ".csv", ".log", ".asmdef", ".asmref", ".cs.txt",
        };

        private static bool IsEditable(string path) =>
            Array.IndexOf(s_textExt, Path.GetExtension(path).ToLowerInvariant()) >= 0;

        // Reuse an already-open window for the same file; otherwise a new one.
        private static void OpenPath(string path)
        {
            string full = Path.GetFullPath(path);
            foreach (var w in Resources.FindObjectsOfTypeAll<UntermCodeEditorWindow>())
            {
                if (!string.IsNullOrEmpty(w._filePath) &&
                    Path.GetFullPath(w._filePath) == full)
                {
                    w.Focus();
                    return;
                }
            }
            // Open as a TAB in the central editing area, not a floating window.
            // CreateWindow(desiredDockNextTo) only "attempts" docking and is ignored
            // for arbitrary types, so dock explicitly via the internal DockArea.
            var win = CreateInstance<UntermCodeEditorWindow>();
            win.minSize = new Vector2(320, 200);
            DockIntoCenter(win);
            win.LoadFile(path);
            win.Focus();
        }

        // Dock `win` as a tab into the central editing area. Unity 6.3: the target's
        // EditorWindow.m_Parent is a HostView (a DockArea when docked); call
        // DockArea.AddTab(EditorWindow, bool). Distinguishing the dock LOCATION uses
        // on-screen position (EditorWindow.position is in screen coords).
        private static readonly FieldInfo ParentField =
            typeof(EditorWindow).GetField("m_Parent", BindingFlags.Instance | BindingFlags.NonPublic);

        private static void DockIntoCenter(EditorWindow win)
        {
            object dock = CenterDockArea();
            if (dock == null) { win.Show(); return; } // no dock area at all: float
            var addTab = dock.GetType().GetMethod(
                "AddTab", new[] { typeof(EditorWindow), typeof(bool) });
            addTab.Invoke(dock, new object[] { win, true });
        }

        // The DockArea at the center of the layout — the main editing area — chosen
        // by screen position so a file docks there, not in a side panel
        // (Hierarchy/Inspector) or a right-docked Claude Code window. Only windows
        // ACTUALLY docked (m_Parent is a DockArea) are considered;
        // Resources.FindObjectsOfTypeAll also returns unparented instances.
        private static object CenterDockArea()
        {
            var cands = new List<(EditorWindow w, object dock)>();
            Rect union = default;
            bool any = false;
            foreach (var w in Resources.FindObjectsOfTypeAll<EditorWindow>())
            {
                if (w == null || w is UntermCodeEditorWindow) continue;
                Rect r = w.position;
                if (r.width < 1f || r.height < 1f) continue;
                object dock = ParentField.GetValue(w);
                if (dock == null || dock.GetType().Name != "DockArea") continue;
                cands.Add((w, dock));
                union = any ? RectUnion(union, r) : r;
                any = true;
            }
            if (cands.Count == 0) return null;

            Vector2 center = union.center;
            object centerDock = null, largestDock = null;
            float bestArea = 0f, centerArea = 0f;
            foreach (var (w, dock) in cands)
            {
                Rect r = w.position;
                float a = r.width * r.height;
                if (a > bestArea) { bestArea = a; largestDock = dock; }
                if (r.Contains(center) && a > centerArea) { centerArea = a; centerDock = dock; }
            }
            return centerDock ?? largestDock;
        }

        private static Rect RectUnion(Rect a, Rect b)
        {
            float x = Mathf.Min(a.xMin, b.xMin);
            float y = Mathf.Min(a.yMin, b.yMin);
            float xMax = Mathf.Max(a.xMax, b.xMax);
            float yMax = Mathf.Max(a.yMax, b.yMax);
            return new Rect(x, y, xMax - x, yMax - y);
        }

        // Extension -> tree-sitter language token (null = no grammar / plain). The
        // first milestone only bundles C#; others fall through to plain.
        private static string LangTokenFor(string path)
        {
            switch (Path.GetExtension(path).ToLowerInvariant())
            {
                case ".cs": return "cs";
                default: return null;
            }
        }

        // Decide the line ending to preserve: keep the file's if it has one
        // (CRLF if any \r\n present, else LF), otherwise the OS default.
        private static bool DetectCrlf(string text)
        {
            if (text.Contains("\r\n")) return true;
            if (text.IndexOf('\n') >= 0) return false;
            return Environment.NewLine == "\r\n";
        }

        private void LoadFile(string path)
        {
            _filePath = path;
            string text = "";
            try { text = File.ReadAllText(path); }
            catch (Exception e) { _status = "read failed: " + e.Message; }
            _crlf = DetectCrlf(text);
            text = text.Replace("\r\n", "\n").Replace("\r", "\n");
            _langToken = LangTokenFor(path) ?? "";
            // (line ending decided above; _crlf maintains the file's, else OS default)
            _fileTicks = File.Exists(path) ? File.GetLastWriteTimeUtc(path).Ticks : 0;
            _dirty = false;
            UpdateTitle();
            if (_native != null && _editorId != 0)
            {
                _native.EditorSetLanguage(Eid, _langToken);
                _native.EditorSetText(Eid, text);
                _savedSerial = _native.EditorEditSerial(Eid); // baseline: just-loaded = clean
                RenderView();
                Repaint();
            }
        }

        // --- lifecycle ---------------------------------------------------------

        private static string ProjectRoot =>
            Directory.GetParent(Application.dataPath)?.FullName ?? Application.dataPath;

        private void OnEnable()
        {
            s_reloading = false;
            AssemblyReloadEvents.beforeAssemblyReload += OnBeforeReload;
            wantsMouseMove = false;
            // New/untitled buffer: default to the OS line ending until a file is
            // loaded (which then maintains that file's ending).
            if (string.IsNullOrEmpty(_filePath)) _crlf = Environment.NewLine == "\r\n";
            LoadNative();
            // Re-derive dirty from the native version now the view is adopted (the
            // serialized flag is just the pre-reload display value).
            MarkDirty();
            UpdateTitle();
        }

        private void OnBeforeReload()
        {
            // A domain reload (recompile) re-adopts the native view by id, so live
            // edits survive without a snapshot; only `s_reloading` needs to carry that
            // intent to OnDisable. We deliberately persist NO buffer content: an editor
            // restart reopens the file fresh from disk, carrying over neither clean nor
            // unsaved edits (Unity's unsaved-changes prompt handles saving on quit).
            s_reloading = true;
        }

        private void OnDisable()
        {
            AssemblyReloadEvents.beforeAssemblyReload -= OnBeforeReload;
            Teardown(keepView: s_reloading);
        }

        private void OnFocus()
        {
            _refocus = true;
#if UNITY_EDITOR_WIN
            Input.imeCompositionMode = IMECompositionMode.On;
#endif
            // Returning to the window: pick up edits made to the file externally.
            if (_native != null && _editorId != 0) CheckExternalChange();
        }

        private void OnLostFocus()
        {
#if UNITY_EDITOR_WIN
            Input.imeCompositionMode = IMECompositionMode.Auto;
#endif
        }

        private void LoadNative()
        {
            try
            {
                UntermWindow.EnsureNativeImageLoaded();
                _native = new UntermNative();
                _native.Load(UntermWindow.PluginPath);

                var (w, h) = CurrentSize();
                float ppp = EditorGUIUtility.pixelsPerPoint;

                // Adopt our own native view only if no OTHER live window already
                // owns this id. Native editor ids are recycled (alloc = max id + 1, so
                // closing the top one frees its number), so a window restored with a
                // stale serialized `_editorId` can land on an id now owned by a
                // DIFFERENT file's editor — driving the same view, its buffer shows the
                // other file. On a collision we build a fresh view seeded from THIS
                // window's own snapshot/file (never the sibling's buffer).
                ulong oldEid = Eid;
                bool collision = _editorId != 0 && AnyOtherWindowOwns(oldEid);
                bool reAdopt = _editorId != 0 && _native.EditorExists(oldEid) && !collision;
                if (!reAdopt)
                {
                    _editorId = (long)_native.EditorCreate(w, h, ppp);
                    ApplyTheme();
                    _native.EditorSetUndoLimit(Eid, UntermCodeEditorPrefs.UndoLimit);
                    _native.EditorSetLanguage(Eid, _langToken ?? "");
                    // Restart / collision: the native view is gone, so reopen this
                    // window's own file fresh from disk. No buffer content is carried
                    // across a restart (clean or unsaved alike), so there's never a
                    // stale snapshot to diff against on the next external-change poll.
                    if (!string.IsNullOrEmpty(_filePath) && File.Exists(_filePath))
                        LoadFile(_filePath); // fresh from disk; sets _savedSerial clean + _fileTicks now
                    else
                        _savedSerial = _native.EditorEditSerial(Eid); // untitled / file gone → empty, clean
                }

                _refocus = true;
                RenderView();
                _status = "ready";
            }
            catch (Exception e)
            {
                _status = "load failed: " + e.Message;
                Debug.LogError("[Unterm] " + e);
                Teardown(keepView: false);
            }
        }

        private void Teardown(bool keepView)
        {
            if (_tex != null) { DestroyImmediate(_tex); _tex = null; }

            var native = _native;
            _native = null;
            if (!keepView)
            {
                ulong eid = Eid;
                bool ownedElsewhere = AnyOtherWindowOwns(eid);
                if (native != null && eid != 0 && !ownedElsewhere)
                    native.EditorDestroy(eid);
                _editorId = 0;
                native?.Dispose();
            }
        }

        private bool AnyOtherWindowOwns(ulong eid)
        {
            if (eid == 0) return false;
            foreach (var w in Resources.FindObjectsOfTypeAll<UntermCodeEditorWindow>())
                if (w != this && (ulong)w._editorId == eid) return true;
            return false;
        }

        // --- sizing / rendering ------------------------------------------------

        private (uint, uint) CurrentSize()
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            uint w = (uint)Mathf.Max(1, Mathf.RoundToInt(position.width * ppp));
            uint h = (uint)Mathf.Max(1, Mathf.RoundToInt(position.height * ppp));
            return (w, h);
        }

        private void ApplyTheme()
        {
            if (_native == null || _editorId == 0) return;
            Color bg = GetEditorBackground();
            bool dark = EditorGUIUtility.isProSkin;
            Color32 fg = dark
                ? new Color32(210, 210, 214, 255)
                : new Color32(32, 32, 32, 255);
            // Native clears in linear space; the sRGB target re-encodes on store.
            _native.EditorSetTheme(Eid, bg.linear, fg, dark);
        }

        private void RenderView()
        {
            if (_native == null || _editorId == 0) return;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            var (w, h) = CurrentSize();
            _native.EditorResize(Eid, w, h, ppp);
            ApplyTheme();
            _native.EditorRender(Eid);
            UploadSurface((int)w, (int)h);
        }

        private void UploadSurface(int iw, int ih)
        {
            IntPtr texPtr = _native.EditorRawTexture(Eid);
            if (texPtr == IntPtr.Zero) return;

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

        private static Color GetEditorBackground()
        {
            var m = typeof(EditorGUIUtility).GetMethod(
                "GetDefaultBackgroundColor",
                System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Static);
            if (m != null && m.ReturnType == typeof(Color))
                return (Color)m.Invoke(null, null);
            return EditorGUIUtility.isProSkin
                ? (Color)new Color32(40, 40, 40, 255)
                : (Color)new Color32(220, 220, 220, 255);
        }

        // --- GUI ---------------------------------------------------------------

        private void OnGUI()
        {
            if (Event.current.type == EventType.Layout)
            {
                bool now = !string.IsNullOrEmpty(Input.compositionString);
                _composeJustEnded = _prevComposing && !now;
                _prevComposing = now;
                _composing = now;
            }

            HandleKeys();

            // Re-render on resize.
            var (cw, ch) = CurrentSize();
            if (_native != null && _editorId != 0 &&
                (_tex == null || _tex.width != (int)cw || _tex.height != (int)ch))
            {
                RenderView();
            }

            var rect = new Rect(0, 0, position.width, position.height);

            if (Event.current.type == EventType.ScrollWheel && rect.Contains(Event.current.mousePosition)
                && _native != null && _editorId != 0)
            {
                var we = Event.current;
                float ppp = EditorGUIUtility.pixelsPerPoint;
                if (Mathf.Abs(we.delta.x) > 0.01f) _native.EditorScrollH(Eid, we.delta.x * 24f * ppp);
                if (Mathf.Abs(we.delta.y) > 0.01f) _native.EditorScroll(Eid, we.delta.y * 24f * ppp);
                RenderView();
                Repaint();
                we.Use();
            }

            HandleMouse(rect);

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

            DrawBar();

            DrawImeField(rect);
            SyncIme();
            // Don't steal focus from the find bar's field while it's open.
            if (_bar == BarMode.None && _refocus && Event.current.type == EventType.Repaint)
            {
                EditorGUI.FocusTextInControl(InputControl);
                _refocus = false;
            }
        }

        private void HandleMouse(Rect rect)
        {
            if (_native == null || _editorId == 0) return;
            var e = Event.current;
            // Leave clicks on the find bar / scrollbar to their own controls.
            if (_bar != BarMode.None && e.mousePosition.y < BarHeight()) return;
            if (e.type == EventType.MouseDown && e.mousePosition.x >= rect.xMax - ScrollbarWidth
                && HasOverflow(rect)) return;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            float lx = (e.mousePosition.x - rect.x) * ppp;
            float ly = (e.mousePosition.y - rect.y) * ppp;

            switch (e.type)
            {
                case EventType.MouseDown when e.button == 0 && rect.Contains(e.mousePosition):
                    byte kind = e.clickCount >= 3 ? (byte)3 : e.clickCount == 2 ? (byte)2 : (byte)0;
                    _native.EditorMouse(Eid, lx, ly, kind);
                    _mouseDragging = true;
                    _refocus = true;
                    RenderView(); Repaint(); e.Use();
                    break;
                case EventType.MouseDrag when _mouseDragging:
                    _native.EditorMouse(Eid, lx, ly, 1);
                    RenderView(); Repaint(); e.Use();
                    break;
                case EventType.MouseUp when _mouseDragging:
                    _mouseDragging = false; e.Use();
                    break;
                case EventType.ContextClick when rect.Contains(e.mousePosition):
                    ShowContextMenu();
                    e.Use();
                    break;
            }
        }

        private void ShowContextMenu()
        {
            var menu = new GenericMenu();
            menu.AddItem(new GUIContent("Cut"), false, () =>
            {
                string s = _native.EditorCut(Eid);
                if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                MarkDirty(); RenderView(); Repaint();
            });
            menu.AddItem(new GUIContent("Copy"), false, () =>
            {
                string s = _native.EditorCopy(Eid);
                if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
            });
            menu.AddItem(new GUIContent("Paste"), false, () =>
            {
                _native.EditorInsert(Eid, EditorGUIUtility.systemCopyBuffer);
                MarkDirty(); RenderView(); Repaint();
            });
            menu.AddItem(new GUIContent("Select All"), false, () =>
            {
                _native.EditorSelectAll(Eid); RenderView(); Repaint();
            });
            menu.AddSeparator("");
            menu.AddItem(new GUIContent($"Find…   {KCmd}{KSep}F"), false, () => OpenBar(BarMode.Find));
            menu.AddItem(new GUIContent($"Replace…   {KCmd}{KSep}{KAlt}{KSep}F"), false, () => OpenBar(BarMode.Replace));
            menu.AddItem(new GUIContent($"Go to Line…   {KCmd}{KSep}L"), false, () => OpenBar(BarMode.Goto));
            menu.AddItem(new GUIContent($"Toggle Comment   {KCmd}{KSep}∕"), false, () =>
            {
                _native.EditorToggleComment(Eid); MarkDirty(); RenderView(); Repaint();
            });
            menu.AddSeparator("");
            menu.AddItem(new GUIContent($"Save   {KCmd}{KSep}S"), false, Save);
            menu.ShowAsContext();
        }

        // The window tab's "⋮" dropdown — so Find / Replace / Go to Line and the
        // shortcuts are discoverable without knowing the key combos.
        public void AddItemsToMenu(GenericMenu menu)
        {
            if (_native == null || _editorId == 0) return;
            menu.AddItem(new GUIContent($"Find…   {KCmd}{KSep}F"), false, () => OpenBar(BarMode.Find));
            menu.AddItem(new GUIContent($"Replace…   {KCmd}{KSep}{KAlt}{KSep}F"), false, () => OpenBar(BarMode.Replace));
            menu.AddItem(new GUIContent($"Go to Line…   {KCmd}{KSep}L"), false, () => OpenBar(BarMode.Goto));
            menu.AddItem(new GUIContent($"Toggle Comment   {KCmd}{KSep}∕"), false, () =>
            {
                _native.EditorToggleComment(Eid); MarkDirty(); RenderView(); Repaint();
            });
            menu.AddItem(new GUIContent($"Save   {KCmd}{KSep}S"), false, Save);
        }

        private void HandleKeys()
        {
            if (_native == null || _editorId == 0) return;
            var e = Event.current;
            if (e.type != EventType.KeyDown) return;

            // Find / replace / goto bar: while open, Esc closes and Enter triggers;
            // every other key is left for the bar's text field.
            if (_bar != BarMode.None)
            {
                if (e.keyCode == KeyCode.Escape) { CloseBar(); e.Use(); return; }
                if (e.keyCode == KeyCode.Return || e.keyCode == KeyCode.KeypadEnter)
                {
                    if (_bar == BarMode.Goto) DoGoto();
                    else DoFind(!e.shift);
                    e.Use();
                    return;
                }
                if ((e.command || e.control) && e.keyCode == KeyCode.G)
                {
                    DoFind(!e.shift); e.Use(); return;
                }
                return;
            }

            // While composing, let every key reach the IME field.
            if (_composing) return;

            // Swallow the Enter that committed an IME composition.
            if (_composeJustEnded &&
                (e.keyCode == KeyCode.Return || e.keyCode == KeyCode.KeypadEnter))
            {
                e.Use();
                return;
            }

            // Move the current line up/down (Alt/Option + Up/Down).
            if (e.alt && !e.command && !e.control &&
                (e.keyCode == KeyCode.UpArrow || e.keyCode == KeyCode.DownArrow))
            {
                if (e.keyCode == KeyCode.UpArrow) _native.EditorMoveLineUp(Eid);
                else _native.EditorMoveLineDown(Eid);
                MarkDirty(); RenderView(); Repaint(); e.Use();
                return;
            }

            // Caret motion: arrows / Home / End, plus modifier combos (word, line
            // start/end, document start/end). Resolved per-platform, then forwarded
            // as a semantic name; shift extends the selection.
            string motion = ResolveMotion(e);
            if (motion != null)
            {
                _native.EditorKey(Eid, motion, e.control, e.alt, e.shift);
                RenderView(); Repaint(); e.Use();
                return;
            }

            // Word / line deletion. macOS: Option+Backspace = delete word, Cmd+
            // Backspace = delete to line start, Option+Delete = delete word forward.
            // Windows/Linux: Ctrl+Backspace/Delete = delete word.
            if (e.keyCode == KeyCode.Backspace && (e.alt || e.command || e.control))
            {
#if UNITY_EDITOR_OSX
                string n = e.command ? "DeleteToLineStart" : "DeleteWordBack";
#else
                string n = "DeleteWordBack";
#endif
                _native.EditorKey(Eid, n, e.control, e.alt, e.shift);
                MarkDirty();
                RenderView(); Repaint(); e.Use();
                return;
            }
            if (e.keyCode == KeyCode.Delete && (e.alt || e.control))
            {
                _native.EditorKey(Eid, "DeleteWordForward", e.control, e.alt, e.shift);
                MarkDirty();
                RenderView(); Repaint(); e.Use();
                return;
            }

            // Cmd/Ctrl shortcuts. Cmd+S is also bound as a Shortcut (context), but
            // handle it here too as a best-effort fallback.
            if (e.command || e.control)
            {
                switch (e.keyCode)
                {
                    case KeyCode.S:
                        Save(); e.Use(); return;
                    case KeyCode.V:
                        _native.EditorInsert(Eid, EditorGUIUtility.systemCopyBuffer);
                        MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.C:
                    {
                        string s = _native.EditorCopy(Eid);
                        if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                        e.Use(); return;
                    }
                    case KeyCode.X:
                    {
                        string s = _native.EditorCut(Eid);
                        if (!string.IsNullOrEmpty(s)) EditorGUIUtility.systemCopyBuffer = s;
                        MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    }
                    case KeyCode.A:
                        _native.EditorSelectAll(Eid); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.Z when !e.shift:
                        _native.EditorUndo(Eid); MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.Z when e.shift:
                        _native.EditorRedo(Eid); MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.Slash:
                        _native.EditorToggleComment(Eid); MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.D:
                        _native.EditorDuplicateLine(Eid); MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.K when e.shift:
                        _native.EditorDeleteLine(Eid); MarkDirty(); RenderView(); Repaint(); e.Use(); return;
                    case KeyCode.L:
                        OpenBar(BarMode.Goto); e.Use(); return;
                    case KeyCode.F:
                        OpenBar(e.alt ? BarMode.Replace : BarMode.Find); e.Use(); return;
                    case KeyCode.G when !e.shift:
                        DoFind(true); e.Use(); return;
                    case KeyCode.G when e.shift:
                        DoFind(false); e.Use(); return;
                }
            }

            // Tab: indent a selection, outdent with Shift, else insert spaces.
            if (e.keyCode == KeyCode.Tab && !e.control && !e.command)
            {
                bool hasSel = !string.IsNullOrEmpty(_native.EditorCopy(Eid));
                if (e.shift) _native.EditorOutdent(Eid);
                else if (hasSel) _native.EditorIndent(Eid);
                else _native.EditorInsert(Eid, "    ");
                MarkDirty(); RenderView(); Repaint(); e.Use();
                return;
            }

            // Editing keys (motion is handled above).
            string name = e.keyCode switch
            {
                KeyCode.Return => "Return",
                KeyCode.KeypadEnter => "Return",
                KeyCode.Backspace => "Backspace",
                KeyCode.Delete => "Delete",
                _ => null,
            };
            if (name != null)
            {
                _native.EditorKey(Eid, name, e.control, e.alt, e.shift);
                MarkDirty();
                RenderView();
                Repaint();
                e.Use();
            }
        }

        // Map an arrow/Home/End keystroke (with modifiers) to a semantic motion
        // name the native editor understands. Returns null for non-motion keys.
        // macOS: Cmd+←/→ = line start/end, Cmd+↑/↓ = document start/end,
        // Option+←/→ = word. Windows/Linux: Ctrl+←/→ = word, Ctrl+Home/End = doc.
        private static string ResolveMotion(Event e)
        {
#if UNITY_EDITOR_OSX
            bool word = e.alt;
            bool ends = e.command;
            switch (e.keyCode)
            {
                case KeyCode.LeftArrow:  return word ? "WordLeft"  : ends ? "LineStart" : "LeftArrow";
                case KeyCode.RightArrow: return word ? "WordRight" : ends ? "LineEnd"   : "RightArrow";
                case KeyCode.UpArrow:    return ends ? "DocStart" : "UpArrow";
                case KeyCode.DownArrow:  return ends ? "DocEnd"   : "DownArrow";
                case KeyCode.Home:       return "LineStart";
                case KeyCode.End:        return "LineEnd";
                case KeyCode.PageUp:     return "PageUp";
                case KeyCode.PageDown:   return "PageDown";
                default: return null;
            }
#else
            bool word = e.control;
            switch (e.keyCode)
            {
                case KeyCode.LeftArrow:  return word ? "WordLeft"  : "LeftArrow";
                case KeyCode.RightArrow: return word ? "WordRight" : "RightArrow";
                case KeyCode.UpArrow:    return "UpArrow";
                case KeyCode.DownArrow:  return "DownArrow";
                case KeyCode.Home:       return e.control ? "DocStart" : "LineStart";
                case KeyCode.End:        return e.control ? "DocEnd"   : "LineEnd";
                case KeyCode.PageUp:     return "PageUp";
                case KeyCode.PageDown:   return "PageDown";
                default: return null;
            }
#endif
        }

        // --- find / replace / goto bar + scrollbar -----------------------------

        private float BarHeight() => _bar == BarMode.Replace ? 44f : (_bar == BarMode.None ? 0f : 22f);

        private bool HasOverflow(Rect rect)
        {
            float ppp = EditorGUIUtility.pixelsPerPoint;
            return _native.EditorContentHeight(Eid) - rect.height * ppp > 0.5f;
        }

        private void OpenBar(BarMode mode)
        {
            _bar = mode;
            _focusBar = true;
            _findStatus = "";
            Repaint();
        }

        private void CloseBar()
        {
            _bar = BarMode.None;
            _refocus = true;
            Repaint();
        }

        private void DoFind(bool forward)
        {
            if (string.IsNullOrEmpty(_findText)) return;
            bool ok = _native.EditorFind(Eid, _findText, forward, _findCase);
            _findStatus = ok ? "" : "No match";
            RenderView(); Repaint();
        }

        private void DoGoto()
        {
            if (int.TryParse(_gotoText, out int n))
            {
                _native.EditorGotoLine(Eid, (uint)Mathf.Max(0, n - 1));
                RenderView(); Repaint();
            }
            CloseBar();
        }

        private void DoReplace()
        {
            // Replace the current match (if any) then advance to the next.
            _native.EditorReplaceSelection(Eid, _replaceText ?? "");
            _native.EditorFind(Eid, _findText, true, _findCase);
            MarkDirty(); RenderView(); Repaint();
        }

        private void DoReplaceAll()
        {
            uint n = _native.EditorReplaceAll(Eid, _findText, _replaceText ?? "", _findCase);
            _findStatus = n + " replaced";
            if (n > 0) MarkDirty();
            RenderView(); Repaint();
        }

        private void DrawBar()
        {
            if (_bar == BarMode.None) return;
            float h = BarHeight();
            var area = new Rect(0, 0, position.width, h);
            EditorGUI.DrawRect(area, EditorGUIUtility.isProSkin
                ? new Color(0.18f, 0.18f, 0.18f, 0.98f)
                : new Color(0.85f, 0.85f, 0.85f, 0.98f));

            GUILayout.BeginArea(area);
            if (_bar == BarMode.Goto)
            {
                GUILayout.BeginHorizontal();
                GUILayout.Label("Go to line:", GUILayout.Width(64));
                GUI.SetNextControlName(FindField);
                _gotoText = GUILayout.TextField(_gotoText, GUILayout.Width(80));
                if (GUILayout.Button("Go", EditorStyles.miniButton, GUILayout.Width(34))) DoGoto();
                GUILayout.FlexibleSpace();
                if (GUILayout.Button("✕", EditorStyles.miniButton, GUILayout.Width(22))) CloseBar();
                GUILayout.EndHorizontal();
            }
            else
            {
                GUILayout.BeginHorizontal();
                GUI.SetNextControlName(FindField);
                _findText = GUILayout.TextField(_findText, GUILayout.MinWidth(120));
                _findCase = GUILayout.Toggle(_findCase, "Aa", EditorStyles.miniButton, GUILayout.Width(28));
                if (GUILayout.Button("◀", EditorStyles.miniButton, GUILayout.Width(22))) DoFind(false);
                if (GUILayout.Button("▶", EditorStyles.miniButton, GUILayout.Width(22))) DoFind(true);
                GUILayout.Label(_findStatus, GUILayout.Width(64));
                GUILayout.FlexibleSpace();
                if (GUILayout.Button("✕", EditorStyles.miniButton, GUILayout.Width(22))) CloseBar();
                GUILayout.EndHorizontal();
                if (_bar == BarMode.Replace)
                {
                    GUILayout.BeginHorizontal();
                    _replaceText = GUILayout.TextField(_replaceText, GUILayout.MinWidth(120));
                    if (GUILayout.Button("Replace", EditorStyles.miniButton, GUILayout.Width(60))) DoReplace();
                    if (GUILayout.Button("All", EditorStyles.miniButton, GUILayout.Width(34))) DoReplaceAll();
                    GUILayout.FlexibleSpace();
                    GUILayout.EndHorizontal();
                }
            }
            GUILayout.EndArea();

            if (_focusBar && Event.current.type == EventType.Repaint)
            {
                EditorGUI.FocusTextInControl(FindField);
                _focusBar = false;
            }
        }

        private void DrawScrollbar(Rect rect)
        {
            if (_native == null || _editorId == 0) return;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            float content = _native.EditorContentHeight(Eid);
            float viewport = rect.height * ppp;
            float max = Mathf.Max(0f, content - viewport);
            if (max <= 0.5f) return;

            float cur = _native.EditorScrollOffset(Eid);
            var sb = new Rect(rect.xMax - ScrollbarWidth, rect.y, ScrollbarWidth, rect.height);
            EditorGUI.BeginChangeCheck();
            float nv = GUI.VerticalScrollbar(sb, cur, viewport, 0f, content);
            if (EditorGUI.EndChangeCheck())
            {
                _native.EditorSetScroll(Eid, Mathf.Clamp(nv, 0f, max));
                RenderView(); Repaint();
            }
        }

        // Reload from disk if the file changed externally (kept edits win if dirty).
        private void CheckExternalChange()
        {
            if (string.IsNullOrEmpty(_filePath) || !File.Exists(_filePath)) return;
            long t = File.GetLastWriteTimeUtc(_filePath).Ticks;
            if (_fileTicks == 0 || t == _fileTicks) return;
            if (!_dirty) { LoadFile(_filePath); return; } // no local edits → just reload

            // Conflict: unsaved local edits AND the file changed on disk. Let the user
            // pick, then sync `_fileTicks` either way so we don't re-prompt every poll.
            bool reload = EditorUtility.DisplayDialog(
                "File changed on disk",
                $"{Path.GetFileName(_filePath)} was modified outside the editor, but you have " +
                "unsaved changes here.\n\nReload from disk (discard your edits), or keep your " +
                "edits (a later save overwrites the disk version)?",
                "Reload", "Keep my edits");
            if (reload) LoadFile(_filePath);
            else { _fileTicks = t; _status = "Kept your edits — saving will overwrite the external change"; }
        }

        // --- IME (hidden field that drives composition + plain typing) ---------

        private void DrawImeField(Rect rect)
        {
            if (_native == null || _editorId == 0) return;
            float ppp = EditorGUIUtility.pixelsPerPoint;
            _native.EditorCaret(Eid, out float cx, out float cy, out float _, out float chh);
            float gx = rect.x + cx / ppp;
            float gy = rect.y + cy / ppp;
            float gh = Mathf.Max(14f, chh / ppp);

            // Clamp the focused field's cached TextEditor caret to the current buffer:
            // Unity doesn't re-clamp it when `_imeBuffer` is reset out from under it,
            // so a stale index throws ArgumentOutOfRange in ReplaceSelection on the
            // next keystroke.
            if (GUIUtility.keyboardControl != 0
                && GUIUtility.GetStateObject(typeof(TextEditor), GUIUtility.keyboardControl) is TextEditor kte)
            {
                kte.cursorIndex = Mathf.Min(kte.cursorIndex, _imeBuffer.Length);
                kte.selectIndex = Mathf.Min(kte.selectIndex, _imeBuffer.Length);
            }

            GUI.SetNextControlName(InputControl);
            var style = ImeHidden();
            bool composing = _composing || !string.IsNullOrEmpty(Input.compositionString);
            if (composing)
            {
                style.alignment = TextAnchor.UpperRight;
                float w = Mathf.Max(120f, gx);
                _imeBuffer = GUI.TextField(new Rect(gx - w, gy, w, gh), _imeBuffer, style);
            }
            else
            {
                style.alignment = TextAnchor.UpperLeft;
                _imeBuffer = GUI.TextField(new Rect(gx, gy, 2f, gh), _imeBuffer, style);
            }
            if (focusedWindow == this)
                Input.compositionCursorPos = new Vector2(gx, gy + gh * 1.5f);
        }

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

        private void SyncIme()
        {
            if (_native == null || _editorId == 0) return;

            if (focusedWindow != this)
            {
                if (!string.IsNullOrEmpty(_lastPreedit))
                {
                    _native.EditorSetPreedit(Eid, "");
                    _lastPreedit = "";
                    RenderView(); Repaint();
                }
                return;
            }

            if (_composing)
            {
                string marked = _imeBuffer + Input.compositionString;
                if (marked != _lastPreedit)
                {
                    _lastPreedit = marked;
                    _native.EditorSetPreedit(Eid, marked);
                    RenderView(); Repaint();
                }
                return;
            }

            if (Event.current.type != EventType.Repaint) return;

            if (!string.IsNullOrEmpty(_lastPreedit))
            {
                _native.EditorSetPreedit(Eid, "");
                _lastPreedit = "";
            }
            if (string.IsNullOrEmpty(_imeBuffer)) return;

            _native.EditorInsert(Eid, _imeBuffer);
            _imeBuffer = "";
            var te = (TextEditor)GUIUtility.GetStateObject(typeof(TextEditor), GUIUtility.keyboardControl);
            if (te != null) { te.text = ""; te.cursorIndex = 0; te.selectIndex = 0; }
            MarkDirty();
            RenderView(); Repaint();
        }

        // --- save / dirty ------------------------------------------------------

        private void MarkDirty()
        {
            // Recompute against the saved version (cheap u64), so editing then undoing
            // back to the saved content clears the flag rather than latching it on.
            if (_native == null || _editorId == 0) return;
            bool dirty = _native.EditorEditSerial(Eid) != _savedSerial;
            _dirty = dirty;
            // Unity shows the unsaved indicator on the tab from hasUnsavedChanges
            // and prompts on close via saveChangesMessage — no manual title marker.
            hasUnsavedChanges = dirty;
            if (dirty) saveChangesMessage = $"{Path.GetFileName(_filePath)} has unsaved changes.";
        }

        public override void SaveChanges()
        {
            Save();
            base.SaveChanges();
        }

        private void Save()
        {
            if (_native == null || _editorId == 0) return;
            if (string.IsNullOrEmpty(_filePath))
            {
                string path = EditorUtility.SaveFilePanel("Save", Application.dataPath, "Untitled", "cs");
                if (string.IsNullOrEmpty(path)) return;
                _filePath = path;
                _langToken = LangTokenFor(path) ?? "";
                _native.EditorSetLanguage(Eid, _langToken);
            }

            // Guard against clobbering an external change that landed since we loaded
            // or last synced (e.g. between on-disk-change polls): confirm the overwrite.
            if (_fileTicks != 0 && File.Exists(_filePath)
                && File.GetLastWriteTimeUtc(_filePath).Ticks != _fileTicks
                && !EditorUtility.DisplayDialog(
                    "Overwrite external changes?",
                    $"{Path.GetFileName(_filePath)} was changed on disk since you opened it. " +
                    "Overwrite those changes with your version?",
                    "Overwrite", "Cancel"))
            {
                _status = "Save canceled (file changed on disk)";
                return;
            }

            string text = _native.EditorText(Eid);
            if (_crlf) text = text.Replace("\n", "\r\n");
            try
            {
                File.WriteAllText(_filePath, text);
            }
            catch (Exception e)
            {
                _status = "save failed: " + e.Message;
                Debug.LogError("[Unterm] " + e);
                return;
            }

            // Record our own write time so it isn't seen as an external change.
            _fileTicks = File.Exists(_filePath) ? File.GetLastWriteTimeUtc(_filePath).Ticks : 0;
            // Baseline the dirty check to the just-saved state.
            _savedSerial = _native.EditorEditSerial(Eid);

            // Reimport if the file lives under the project so Unity recompiles.
            string rel = ToProjectRelative(_filePath);
            if (rel != null) AssetDatabase.ImportAsset(rel, ImportAssetOptions.ForceUpdate);

            _dirty = false;
            hasUnsavedChanges = false;
            UpdateTitle();
            RenderView(); Repaint();
        }

        // A path under the project's Assets/ or Packages/ as a project-relative
        // path (for AssetDatabase), or null if it's outside the project.
        private static string ToProjectRelative(string path)
        {
            string full = Path.GetFullPath(path).Replace('\\', '/');
            string root = ProjectRoot.Replace('\\', '/').TrimEnd('/') + "/";
            if (!full.StartsWith(root, StringComparison.OrdinalIgnoreCase)) return null;
            string rel = full.Substring(root.Length);
            return (rel.StartsWith("Assets/") || rel.StartsWith("Packages/")) ? rel : null;
        }

        private void UpdateTitle()
        {
            // No manual dirty marker — Unity overlays the unsaved indicator from
            // hasUnsavedChanges.
            titleContent = new GUIContent(string.IsNullOrEmpty(_filePath) ? "Untitled" : Path.GetFileName(_filePath));
        }

        // Cmd/Ctrl+S while this window is focused saves the file (a window-context
        // shortcut takes priority over the global File ▸ Save when focused, and
        // restores it elsewhere).
        [Shortcut("Unterm/Save Code File", typeof(UntermCodeEditorWindow), KeyCode.S, ShortcutModifiers.Action)]
        private static void SaveShortcut(ShortcutArguments args)
        {
            var w = args.context as UntermCodeEditorWindow ?? focusedWindow as UntermCodeEditorWindow;
            w?.Save();
        }
    }

    /// <summary>
    /// Preferences for the Unterm code editor (stored in EditorPrefs).
    /// </summary>
    internal static class UntermCodeEditorPrefs
    {
        private const string HijackKey = "Unterm.CodeEditor.HijackDoubleClick";

        public static bool HijackDoubleClick
        {
            get => EditorPrefs.GetBool(HijackKey, false);
            set => EditorPrefs.SetBool(HijackKey, value);
        }

        [SettingsProvider]
        public static SettingsProvider CreateProvider()
        {
            return new SettingsProvider("Preferences/Unterm", SettingsScope.User)
            {
                label = "Unterm",
                guiHandler = _ =>
                {
                    EditorGUILayout.Space();
                    bool v = EditorGUILayout.ToggleLeft(
                        new GUIContent("Open files in Unterm Code Editor on double-click",
                            "When on, double-clicking a file in the Project window opens it in the " +
                            "Unterm code editor instead of the external script editor."),
                        HijackDoubleClick);
                    if (v != HijackDoubleClick) HijackDoubleClick = v;
                },
                keywords = new[] { "Unterm", "code", "editor", "double", "click" },
            };
        }
    }
}
