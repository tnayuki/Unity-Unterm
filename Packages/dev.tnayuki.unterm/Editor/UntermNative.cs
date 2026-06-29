using System;
using System.IO;
using System.Runtime.InteropServices;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// Dynamic-library loader for the Unterm native terminal plugin (macOS +
    /// Windows).
    ///
    /// Terminals live in process globals on the native side (the registry keyed
    /// by a stable u64 id), so they survive Unity C# domain reloads. To keep
    /// them mapped we load the library via a *stable* shadow copy and never
    /// unload it on reload. Every editor window loads the same shadow path, so
    /// they all share one native image and one registry; each window owns one
    /// terminal id it serializes and re-adopts after a reload.
    ///
    /// The OS dynamic loader is used directly (dlopen on macOS, LoadLibrary on
    /// Windows) rather than Unity's native-plugin import system, so we control
    /// when the image loads/unloads across reloads.
    /// </summary>
    internal sealed class UntermNative : IDisposable
    {
        // --- platform dynamic-loader shim -------------------------------------
#if UNITY_EDITOR_WIN
        [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)]
        private static extern IntPtr LoadLibrary(string path);
        // GetProcAddress takes an ANSI symbol name regardless of the wide module API.
        [DllImport("kernel32", SetLastError = true)]
        private static extern IntPtr GetProcAddress(IntPtr handle, [MarshalAs(UnmanagedType.LPStr)] string symbol);
        [DllImport("kernel32", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool FreeLibrary(IntPtr handle);

        private static IntPtr NativeOpen(string path) => LoadLibrary(path);
        private static IntPtr NativeSym(IntPtr handle, string symbol) => GetProcAddress(handle, symbol);
        private static void NativeClose(IntPtr handle) => FreeLibrary(handle);
        private static string NativeError() =>
            new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error()).Message;
#else
        // Shadow copies keep a .dylib extension on macOS.
        private const string ShadowExt = ".dylib";

        private const int RTLD_NOW = 2;
        private const int RTLD_LOCAL = 4;

        [DllImport("/usr/lib/libSystem.B.dylib")]
        private static extern IntPtr dlopen(string path, int mode);
        [DllImport("/usr/lib/libSystem.B.dylib")]
        private static extern IntPtr dlsym(IntPtr handle, string symbol);
        [DllImport("/usr/lib/libSystem.B.dylib")]
        private static extern int dlclose(IntPtr handle);
        [DllImport("/usr/lib/libSystem.B.dylib")]
        private static extern IntPtr dlerror();

        private static IntPtr NativeOpen(string path) => dlopen(path, RTLD_NOW | RTLD_LOCAL);
        private static IntPtr NativeSym(IntPtr handle, string symbol) => dlsym(handle, symbol);
        private static void NativeClose(IntPtr handle) => dlclose(handle);
        private static string NativeError()
        {
            var p = dlerror();
            return p == IntPtr.Zero ? "(no error)" : Marshal.PtrToStringAnsi(p);
        }
#endif

        // --- terminal registry C ABI (id-based; survives reload) ---
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong CreateSeededFn(ulong id, uint w, uint h, float scale, [MarshalAs(UnmanagedType.LPUTF8Str)] string cwd, [MarshalAs(UnmanagedType.LPUTF8Str)] string seed);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong CreateDeadFn(ulong id, uint w, uint h, float scale, [MarshalAs(UnmanagedType.LPUTF8Str)] string seed);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong CreateFn(uint w, uint h, float scale, [MarshalAs(UnmanagedType.LPUTF8Str)] string cwd);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong CreateCommandFn(uint w, uint h, float scale, [MarshalAs(UnmanagedType.LPUTF8Str)] string cwd, [MarshalAs(UnmanagedType.LPUTF8Str)] string command);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool ExistsFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void IdFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void ResizeFn(ulong id, uint w, uint h, float scale);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SetScaleFn(ulong id, float scale);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SetFontFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string path);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SetFontSizeFn(ulong id, float points);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SetColorsFn(ulong id, uint fg, uint bg, uint cursor);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SetFocusFn(ulong id, [MarshalAs(UnmanagedType.I1)] bool focused);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SendTextFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string text);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SendKeyFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string name, [MarshalAs(UnmanagedType.I1)] bool ctrl, [MarshalAs(UnmanagedType.I1)] bool alt, [MarshalAs(UnmanagedType.I1)] bool shift);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void ScrollFn(ulong id, int delta);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SelStartFn(ulong id, float x, float y, byte mode);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SelUpdateFn(ulong id, float x, float y);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool BoolFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr PtrFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SizeFn(ulong id, out uint a, out uint b);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void ScrollStateFn(ulong id, out uint history, out uint offset, out uint screen);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool CursorPxFn(ulong id, out float x, out float y, out float w, out float h);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr TitleFn(ulong id, out UIntPtr len);

        // --- shared MCP server bridge (editor-global; no terminal id) ---
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr McpBufFn(out UIntPtr len);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void McpSetToolsFn([MarshalAs(UnmanagedType.LPUTF8Str)] string json);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void McpRespondFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string resultJson);

        // --- agent view (id-based; owns session + transcript panel + input box;
        // survives reload via a process-global registry). All symbols prefixed
        // `unterm_agentview_`. ---
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong AvCreateFn([MarshalAs(UnmanagedType.LPUTF8Str)] string cwd, uint pw, uint ph, uint iw, uint ih, [MarshalAs(UnmanagedType.LPUTF8Str)] string effort, [MarshalAs(UnmanagedType.LPUTF8Str)] string claudeCmd);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong AvLoadFn([MarshalAs(UnmanagedType.LPUTF8Str)] string cwd, [MarshalAs(UnmanagedType.LPUTF8Str)] string resume, uint pw, uint ph, uint iw, uint ih, [MarshalAs(UnmanagedType.LPUTF8Str)] string effort, [MarshalAs(UnmanagedType.LPUTF8Str)] string claudeCmd);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool AvExistsFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvVoidFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate uint AvPollFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvResizeFn(ulong id, uint pw, uint ph, uint iw, uint ih, float scale);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvThemeFn(ulong id, double br, double bg, double bb, double ba, byte fr, byte fg, byte fb);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvFontsFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string regular, [MarshalAs(UnmanagedType.LPUTF8Str)] string bold, [MarshalAs(UnmanagedType.LPUTF8Str)] string italic, [MarshalAs(UnmanagedType.LPUTF8Str)] string boldItalic);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr AvPtrFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr AvBufFn(ulong id, out UIntPtr len);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate float AvFloatFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvF1Fn(ulong id, float v);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvCaretFn(ulong id, out float x, out float y, out float w, out float h);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate byte AvDownFn(ulong id, float x, float y);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvDragFn(ulong id, float x, float y);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate byte AvScrollHFn(ulong id, float x, float y, float dx);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool AvBoolFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate byte AvInputDownFn(ulong id, float x, float y, byte kind);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvInputKeyFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string name, [MarshalAs(UnmanagedType.I1)] bool ctrl, [MarshalAs(UnmanagedType.I1)] bool alt, [MarshalAs(UnmanagedType.I1)] bool shift);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvStrFn(ulong id, [MarshalAs(UnmanagedType.LPUTF8Str)] string text);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate uint AvUintGetFn(ulong id);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void AvUintSetFn(ulong id, uint v);

#if UNITY_EDITOR_OSX
        // --- macOS Unity device sharing (see lib.rs `unity_metal`) ---
        // The *original* bundle Unity auto-loads exposes the editor's MTLDevice /
        // MTLCommandQueue pointers; resolved by bare name so these P/Invokes bind
        // to that image (not our RTLD_LOCAL shadow copy). Forwarded into the shadow
        // image via the `unterm_set_unity_device` delegate below.
        private static class UntermOriginal
        {
            [DllImport("unterm")]
            public static extern IntPtr unterm_get_unity_device();
            [DllImport("unterm")]
            public static extern IntPtr unterm_get_unity_queue();
        }

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        private delegate void SetUnityDeviceFn(IntPtr device, IntPtr queue);
#endif

        private IntPtr _handle;
        private string _shadowPath;
        private bool _stable;

        private CreateFn _create; private CreateCommandFn _createCommand; private ExistsFn _exists; private IdFn _destroy; private ResizeFn _resize;
        // Restore-across-restart: seed+shell, seed-only (display-only/exited), the buffer dump, the cwd.
        private CreateSeededFn _createSeeded; private CreateDeadFn _createDead; private TitleFn _dump; private TitleFn _cwd;
        private SetScaleFn _setScale; private SetFontFn _setFont; private SetFontSizeFn _setFontSize;
        private SetColorsFn _setColors; private SetFocusFn _setFocus; private SendTextFn _sendText;
        private SendTextFn _setPreedit;
        private SendKeyFn _sendKey; private SendTextFn _paste; private IdFn _clear;
        private ScrollFn _scroll; private IdFn _render; private BoolFn _dirty; private BoolFn _present;
        private SelStartFn _selStart; private SelUpdateFn _selUpdate; private IdFn _selClear; private TitleFn _selText;
        private BoolFn _isAlive; private PtrFn _iosurface; private PtrFn _rawTexture;
        private SizeFn _size; private SizeFn _gridSize; private ScrollStateFn _scrollState; private CursorPxFn _cursorPx; private TitleFn _title;
        private McpSetToolsFn _mcpSetTools; private McpBufFn _mcpNextCall; private McpRespondFn _mcpRespond;
        private AvCreateFn _avCreate; private AvLoadFn _avLoad; private AvExistsFn _avExists; private AvVoidFn _avDestroy;
        private AvPollFn _avPoll; private AvVoidFn _avRender; private AvResizeFn _avResize; private AvThemeFn _avSetTheme; private AvFontsFn _avSetFonts;
        private AvPtrFn _avPanelTexture; private AvPtrFn _avInputTexture;
        private AvFloatFn _avContentHeight; private AvFloatFn _avInputHeight; private AvF1Fn _avSetScroll; private AvCaretFn _avCaret;
        private AvVoidFn _avInterrupt; private AvBufFn _avSessionId; private AvBufFn _avTitle;
        private AvBufFn _avTakeHostCommand;
        private AvStrFn _avSetPermissionMode; private AvBufFn _avPermissionMode;
        private AvStrFn _avSetModel; private AvBufFn _avModel;
        private AvUintGetFn _avQueueLen; private AvUintSetFn _avCancelQueued;
        private AvDownFn _avPanelDown; private AvDragFn _avPanelDrag; private AvScrollHFn _avPanelScrollH; private AvScrollHFn _avPanelScrollV;
        private AvVoidFn _avPanelSelectAll; private AvVoidFn _avPanelSelectClear; private AvBoolFn _avPanelHasSelection; private AvBufFn _avPanelSelectedText;
        private AvBoolFn _avThinking;
        private AvInputDownFn _avInputDown; private AvDragFn _avInputDrag; private AvInputKeyFn _avInputKey;
        private AvStrFn _avInputInsert; private AvStrFn _avInputSetPreedit; private AvVoidFn _avInputUndo; private AvVoidFn _avInputRedo; private AvVoidFn _avInputSelectAll;
        private AvBufFn _avInputCopy; private AvBufFn _avInputCut; private AvBufFn _avInputText;
#if UNITY_EDITOR_OSX
        private SetUnityDeviceFn _setUnityDevice;
#endif

        public bool IsLoaded => _handle != IntPtr.Zero;

        /// <summary>
        /// Load the bundle via a shadow copy. With <paramref name="freshInstance"/>
        /// false (default) a stable shadow path keyed on the bundle's identity is
        /// reused, so re-loading after a domain reload returns the same mapped
        /// image (the terminal registry persists). Pass true to force a brand-new
        /// image (picks up a rebuilt Rust bundle).
        /// </summary>
        public void Load(string bundlePath, bool freshInstance = false)
        {
            if (IsLoaded) return;
#if UNITY_EDITOR_WIN
            // Bind to the SAME image Unity already loaded as an Editor/Windows
            // native plugin: a bare-name LoadLibrary resolves to the in-process
            // module by base name, so UnityPluginLoad — which captured the editor's
            // D3D device — ran in this very image, and the zero-copy surface uses
            // that device directly (no shadow copy, no cross-image device bridge).
            // Unity keeps editor plugins mapped across domain reloads, so the
            // terminal registry survives without our own shadow-copy trick.
            _stable = true; // not a temp file we own, so never delete on Dispose
            _handle = NativeOpen("unterm");
            if (_handle == IntPtr.Zero)
                throw new Exception(
                    $"native load failed — is unterm.dll imported as an Editor/Windows plugin? {NativeError()}");
#else
            if (!File.Exists(bundlePath))
                throw new FileNotFoundException($"Unterm native bundle not found: {bundlePath}");

            _stable = !freshInstance;
            var info = new FileInfo(bundlePath);
            _shadowPath = freshInstance
                ? Path.Combine(Path.GetTempPath(), $"unterm_{Guid.NewGuid():N}{ShadowExt}")
                : Path.Combine(Path.GetTempPath(), $"unterm_{info.Length}_{info.LastWriteTimeUtc.Ticks}{ShadowExt}");

            if (freshInstance || !File.Exists(_shadowPath))
                File.Copy(bundlePath, _shadowPath, overwrite: freshInstance);

            _handle = NativeOpen(_shadowPath);
            if (_handle == IntPtr.Zero)
                throw new Exception($"native load failed: {NativeError()}");
#endif

            _create = Sym<CreateFn>("unterm_create");
            _createCommand = Sym<CreateCommandFn>("unterm_create_command");
            _createSeeded = Sym<CreateSeededFn>("unterm_create_seeded");
            _createDead = Sym<CreateDeadFn>("unterm_create_dead");
            _dump = Sym<TitleFn>("unterm_dump");
            _cwd = Sym<TitleFn>("unterm_cwd");
            _exists = Sym<ExistsFn>("unterm_exists");
            _destroy = Sym<IdFn>("unterm_destroy");
            _resize = Sym<ResizeFn>("unterm_resize");
            _setScale = Sym<SetScaleFn>("unterm_set_scale");
            _setFont = Sym<SetFontFn>("unterm_set_font");
            _setFontSize = Sym<SetFontSizeFn>("unterm_set_font_size");
            _setColors = Sym<SetColorsFn>("unterm_set_colors");
            _setFocus = Sym<SetFocusFn>("unterm_set_focus");
            _sendText = Sym<SendTextFn>("unterm_send_text");
            _setPreedit = Sym<SendTextFn>("unterm_set_preedit");
            _sendKey = Sym<SendKeyFn>("unterm_send_key");
            _paste = Sym<SendTextFn>("unterm_paste");
            _clear = Sym<IdFn>("unterm_clear");
            _scroll = Sym<ScrollFn>("unterm_scroll");
            _selStart = Sym<SelStartFn>("unterm_selection_start");
            _selUpdate = Sym<SelUpdateFn>("unterm_selection_update");
            _selClear = Sym<IdFn>("unterm_selection_clear");
            _selText = Sym<TitleFn>("unterm_selection_text");
            _render = Sym<IdFn>("unterm_render");
            _dirty = Sym<BoolFn>("unterm_dirty");
            _present = Sym<BoolFn>("unterm_present");
            _isAlive = Sym<BoolFn>("unterm_is_alive");
            _iosurface = Sym<PtrFn>("unterm_iosurface");
            _rawTexture = Sym<PtrFn>("unterm_raw_texture");
            _size = Sym<SizeFn>("unterm_size");
            _gridSize = Sym<SizeFn>("unterm_grid_size");
            _scrollState = Sym<ScrollStateFn>("unterm_scroll_state");
            _cursorPx = Sym<CursorPxFn>("unterm_cursor_px");
            _title = Sym<TitleFn>("unterm_title");

            _mcpSetTools = Sym<McpSetToolsFn>("unterm_mcp_set_tools");
            _mcpNextCall = Sym<McpBufFn>("unterm_mcp_next_call");
            _mcpRespond = Sym<McpRespondFn>("unterm_mcp_respond");

            _avCreate = Sym<AvCreateFn>("unterm_agentview_create");
            _avLoad = Sym<AvLoadFn>("unterm_agentview_load");
            _avExists = Sym<AvExistsFn>("unterm_agentview_exists");
            _avDestroy = Sym<AvVoidFn>("unterm_agentview_destroy");
            _avPoll = Sym<AvPollFn>("unterm_agentview_poll");
            _avRender = Sym<AvVoidFn>("unterm_agentview_render");
            _avResize = Sym<AvResizeFn>("unterm_agentview_resize");
            _avSetTheme = Sym<AvThemeFn>("unterm_agentview_set_theme");
            _avSetFonts = Sym<AvFontsFn>("unterm_agentview_set_fonts");
            _avPanelTexture = Sym<AvPtrFn>("unterm_agentview_panel_texture");
            _avInputTexture = Sym<AvPtrFn>("unterm_agentview_input_texture");
            _avContentHeight = Sym<AvFloatFn>("unterm_agentview_content_height");
            _avInputHeight = Sym<AvFloatFn>("unterm_agentview_input_height");
            _avSetScroll = Sym<AvF1Fn>("unterm_agentview_set_scroll");
            _avCaret = Sym<AvCaretFn>("unterm_agentview_caret");
            _avInterrupt = Sym<AvVoidFn>("unterm_agentview_interrupt");
            _avSetPermissionMode = Sym<AvStrFn>("unterm_agentview_set_permission_mode");
            _avPermissionMode = Sym<AvBufFn>("unterm_agentview_permission_mode");
            _avSetModel = Sym<AvStrFn>("unterm_agentview_set_model");
            _avModel = Sym<AvBufFn>("unterm_agentview_model");
            _avQueueLen = Sym<AvUintGetFn>("unterm_agentview_queue_len");
            _avCancelQueued = Sym<AvUintSetFn>("unterm_agentview_cancel_queued");
            _avSessionId = Sym<AvBufFn>("unterm_agentview_session_id");
            _avTitle = Sym<AvBufFn>("unterm_agentview_title");
            _avTakeHostCommand = Sym<AvBufFn>("unterm_agentview_take_host_command");
            _avPanelDown = Sym<AvDownFn>("unterm_agentview_panel_down");
            _avPanelDrag = Sym<AvDragFn>("unterm_agentview_panel_drag");
            _avPanelScrollH = Sym<AvScrollHFn>("unterm_agentview_panel_scroll_h");
            _avPanelScrollV = Sym<AvScrollHFn>("unterm_agentview_panel_scroll_v");
            _avPanelSelectAll = Sym<AvVoidFn>("unterm_agentview_panel_select_all");
            _avPanelSelectClear = Sym<AvVoidFn>("unterm_agentview_panel_select_clear");
            _avPanelHasSelection = Sym<AvBoolFn>("unterm_agentview_panel_has_selection");
            _avThinking = Sym<AvBoolFn>("unterm_agentview_thinking");
            _avPanelSelectedText = Sym<AvBufFn>("unterm_agentview_panel_selected_text");
            _avInputDown = Sym<AvInputDownFn>("unterm_agentview_input_down");
            _avInputDrag = Sym<AvDragFn>("unterm_agentview_input_drag");
            _avInputKey = Sym<AvInputKeyFn>("unterm_agentview_input_key");
            _avInputInsert = Sym<AvStrFn>("unterm_agentview_input_insert");
            _avInputSetPreedit = Sym<AvStrFn>("unterm_agentview_input_set_preedit");
            _avInputUndo = Sym<AvVoidFn>("unterm_agentview_input_undo");
            _avInputRedo = Sym<AvVoidFn>("unterm_agentview_input_redo");
            _avInputSelectAll = Sym<AvVoidFn>("unterm_agentview_input_select_all");
            _avInputCopy = Sym<AvBufFn>("unterm_agentview_input_copy");
            _avInputCut = Sym<AvBufFn>("unterm_agentview_input_cut");
            _avInputText = Sym<AvBufFn>("unterm_agentview_input_text");

#if UNITY_EDITOR_OSX
            // macOS shadow-copy split: forward the editor's MTLDevice + command
            // queue from the original auto-loaded bundle into this image so the
            // renderer builds wgpu on the same GPU (see lib.rs `unity_metal`).
            // Windows binds to Unity's own image, so it needs no bridge.
            _setUnityDevice = Sym<SetUnityDeviceFn>("unterm_set_unity_device");
            IntPtr unityDevice = IntPtr.Zero, unityQueue = IntPtr.Zero;
            try
            {
                unityDevice = UntermOriginal.unterm_get_unity_device();
                unityQueue = UntermOriginal.unterm_get_unity_queue();
            }
            catch (Exception e)
            {
                // Normal if Unity hasn't auto-loaded the original bundle yet (the
                // renderer then falls back to the default adapter).
                Debug.LogWarning(
                    "unterm: could not read Unity's device from the original bundle: " + e.Message);
            }
            _setUnityDevice(unityDevice, unityQueue);
#endif
        }

        private T Sym<T>(string name) where T : Delegate
        {
            var addr = NativeSym(_handle, name);
            if (addr == IntPtr.Zero)
                throw new Exception($"symbol '{name}' not found: {NativeError()}");
            return Marshal.GetDelegateForFunctionPointer<T>(addr);
        }

        private static string Utf8(IntPtr p, UIntPtr len) =>
            p == IntPtr.Zero ? string.Empty : Marshal.PtrToStringUTF8(p, (int)len.ToUInt64());

        public ulong Create(uint w, uint h, float scale, string cwd) => _create(w, h, scale, cwd ?? string.Empty);
        /// Create a terminal that launches `command` directly in the PTY (no shell prompt / typed input).
        public ulong CreateCommand(uint w, uint h, float scale, string cwd, string command) =>
            _createCommand(w, h, scale, cwd ?? string.Empty, command ?? string.Empty);
        /// Restore an interactive shell with the grid pre-seeded; re-claims terminal id `id` if free.
        public ulong CreateSeeded(ulong id, uint w, uint h, float scale, string cwd, string seed) =>
            _createSeeded(id, w, h, scale, cwd ?? string.Empty, seed ?? string.Empty);
        /// Restore a display-only terminal (no shell, marked exited); re-claims terminal id `id` if free.
        public ulong CreateDead(ulong id, uint w, uint h, float scale, string seed) =>
            _createDead(id, w, h, scale, seed ?? string.Empty);
        /// The full buffer (scrollback + screen) as truecolor-SGR text, for saving across a restart.
        public string Dump(ulong id)
        {
            var p = _dump(id, out UIntPtr len);
            return Utf8(p, len);
        }
        /// The shell's current working directory (empty if no live shell), for restoring cwd on resume.
        public string Cwd(ulong id)
        {
            var p = _cwd(id, out UIntPtr len);
            return Utf8(p, len);
        }
        public bool Exists(ulong id) => id != 0 && _exists(id);
        public void Destroy(ulong id) { if (id != 0) _destroy(id); }
        public void Resize(ulong id, uint w, uint h, float scale) => _resize(id, w, h, scale);
        public void SetScale(ulong id, float scale) => _setScale(id, scale);
        public void SetFont(ulong id, string path) => _setFont(id, path ?? string.Empty);
        public void SetFontSize(ulong id, float points) => _setFontSize(id, points);
        public void SetColors(ulong id, Color32 fg, Color32 bg, Color32 cursor) =>
            _setColors(id, Pack(fg), Pack(bg), Pack(cursor));
        public void SetFocus(ulong id, bool focused) => _setFocus(id, focused);
        public void SendText(ulong id, string text) { if (!string.IsNullOrEmpty(text)) _sendText(id, text); }
        // Empty string clears the composition, so always forward (don't short-circuit).
        public void SetPreedit(ulong id, string text) => _setPreedit(id, text ?? "");
        public void SendKey(ulong id, string name, bool ctrl, bool alt, bool shift) => _sendKey(id, name, ctrl, alt, shift);
        public void Paste(ulong id, string text) { if (!string.IsNullOrEmpty(text)) _paste(id, text); }
        public void Clear(ulong id) => _clear(id);
        public void Scroll(ulong id, int delta) => _scroll(id, delta);
        // mode: 0 = by character, 1 = by word (double-click), 2 = by line.
        public void SelectionStart(ulong id, float x, float y, byte mode) => _selStart(id, x, y, mode);
        public void SelectionUpdate(ulong id, float x, float y) => _selUpdate(id, x, y);
        public void SelectionClear(ulong id) => _selClear(id);
        public string SelectionText(ulong id)
        {
            var p = _selText(id, out UIntPtr len);
            return Utf8(p, len);
        }
        public void Render(ulong id) => _render(id);
        public bool Dirty(ulong id) => _dirty(id);
        // Advance the render-target swapchain; true if the displayed frame changed.
        public bool Present(ulong id) => _present(id);
        public bool IsAlive(ulong id) => _isAlive(id);
        public IntPtr IOSurface(ulong id) => _iosurface(id);
        public IntPtr RawTexture(ulong id) => _rawTexture(id);
        public void Size(ulong id, out uint w, out uint h) => _size(id, out w, out h);
        public void GridSize(ulong id, out uint cols, out uint rows) => _gridSize(id, out cols, out rows);
        // history = scrollback lines above the screen; offset = lines scrolled up
        // from the live bottom (0 = pinned); screen = visible row count.
        public void ScrollState(ulong id, out uint history, out uint offset, out uint screen) =>
            _scrollState(id, out history, out offset, out screen);
        public bool CursorPx(ulong id, out float x, out float y, out float w, out float h) =>
            _cursorPx(id, out x, out y, out w, out h);
        public string Title(ulong id)
        {
            var p = _title(id, out UIntPtr len);
            return Utf8(p, len);
        }

        // --- shared MCP server (editor-global, survives reloads) ---
        public void McpSetTools(string json) => _mcpSetTools(json ?? "[]");
        /// The next queued tool call as `{id,name,args}` JSON, or "" if none.
        public string McpNextCall()
        {
            var p = _mcpNextCall(out UIntPtr len);
            return Utf8(p, len);
        }
        public void McpRespond(ulong id, string resultJson) => _mcpRespond(id, resultJson ?? "{}");

        // --- agent view (id-based; owns session + transcript panel + input box) ---
        /// Start a new conversation rooted at `cwd`; returns the view id. Sizes are
        /// physical px: (panel w/h, input w/h).
        public ulong AgentviewCreate(string cwd, uint pw, uint ph, uint iw, uint ih, string effort, string claudeCmd) =>
            _avCreate(cwd ?? string.Empty, pw, ph, iw, ih, effort ?? string.Empty, claudeCmd ?? string.Empty);
        /// Resume a prior conversation `resume` (empty -> fresh); returns the view id.
        /// `effort` is the reasoning effort (none/low/medium/high/max; ""/default = model default).
        /// `claudeCmd` is the resolved absolute path to the `claude` CLI ("" -> bare `claude`).
        public ulong AgentviewLoad(string cwd, string resume, uint pw, uint ph, uint iw, uint ih, string effort, string claudeCmd) =>
            _avLoad(cwd ?? string.Empty, resume ?? string.Empty, pw, ph, iw, ih, effort ?? string.Empty, claudeCmd ?? string.Empty);
        public bool AgentviewExists(ulong id) => id != 0 && _avExists(id);
        public void AgentviewDestroy(ulong id) { if (id != 0) _avDestroy(id); }
        /// bit0 = dirty (re-render + repaint), bit1 = animating (repaint only).
        public uint AgentviewPoll(ulong id) => _avPoll(id);
        public void AgentviewRender(ulong id) => _avRender(id);
        public void AgentviewResize(ulong id, uint pw, uint ph, uint iw, uint ih, float scale) =>
            _avResize(id, pw, ph, iw, ih, scale);
        public void AgentviewSetTheme(ulong id, Color bg, Color32 fg) =>
            _avSetTheme(id, bg.r, bg.g, bg.b, bg.a, fg.r, fg.g, fg.b);
        public void AgentviewSetFonts(ulong id, string regular, string bold, string italic, string boldItalic) =>
            _avSetFonts(id, regular ?? string.Empty, bold ?? string.Empty, italic ?? string.Empty, boldItalic ?? string.Empty);
        public IntPtr AgentviewPanelTexture(ulong id) => _avPanelTexture(id);
        public IntPtr AgentviewInputTexture(ulong id) => _avInputTexture(id);
        public float AgentviewContentHeight(ulong id) => _avContentHeight(id);
        public float AgentviewInputHeight(ulong id) => _avInputHeight(id);
        public void AgentviewSetScroll(ulong id, float scroll) => _avSetScroll(id, scroll);
        public void AgentviewCaret(ulong id, out float x, out float y, out float w, out float h) =>
            _avCaret(id, out x, out y, out w, out h);
        public void AgentviewInterrupt(ulong id) { if (id != 0) _avInterrupt(id); }
        /// Permission mode: "default" / "plan" / "acceptEdits" / "bypassPermissions".
        public void AgentviewSetPermissionMode(ulong id, string mode) { if (id != 0) _avSetPermissionMode(id, mode ?? "default"); }
        public string AgentviewPermissionMode(ulong id) { var p = _avPermissionMode(id, out UIntPtr len); return Utf8(p, len); }
        /// Model alias ("opus"/"sonnet"/"haiku"), or "" / "default" for the engine default.
        public void AgentviewSetModel(ulong id, string model) { if (id != 0) _avSetModel(id, model ?? string.Empty); }
        /// Active model: the user's choice, else the resolved model from init.
        public string AgentviewModel(ulong id) { var p = _avModel(id, out UIntPtr len); return Utf8(p, len); }
        /// Number of follow-up prompts waiting in the queue.
        public uint AgentviewQueueLen(ulong id) => id != 0 ? _avQueueLen(id) : 0u;
        /// Cancel the index-th queued follow-up prompt (0-based).
        public void AgentviewCancelQueued(ulong id, uint index) { if (id != 0) _avCancelQueued(id, index); }
        public string AgentviewSessionId(ulong id) { var p = _avSessionId(id, out UIntPtr len); return Utf8(p, len); }
        public string AgentviewTitle(ulong id) { var p = _avTitle(id, out UIntPtr len); return Utf8(p, len); }
        public string AgentviewTakeHostCommand(ulong id) { var p = _avTakeHostCommand(id, out UIntPtr len); return Utf8(p, len); }
        /// Transcript mouse-down: resolves permission buttons AND begins selection
        /// internally. Returns 1 if consumed.
        public byte AgentviewPanelDown(ulong id, float x, float y) => _avPanelDown(id, x, y);
        public void AgentviewPanelDrag(ulong id, float x, float y) => _avPanelDrag(id, x, y);
        /// Horizontal scroll over a code block; returns 1 if consumed.
        public byte AgentviewPanelScrollH(ulong id, float x, float y, float dx) => _avPanelScrollH(id, x, y, dx);
        /// Vertical scroll over the capped plan box; returns 1 if consumed.
        public byte AgentviewPanelScrollV(ulong id, float x, float y, float dy) => _avPanelScrollV(id, x, y, dy);
        public void AgentviewPanelSelectAll(ulong id) => _avPanelSelectAll(id);
        public void AgentviewPanelSelectClear(ulong id) => _avPanelSelectClear(id);
        public bool AgentviewPanelHasSelection(ulong id) => _avPanelHasSelection(id);
        public bool AgentviewThinking(ulong id) => _avThinking != null && _avThinking(id);
        public string AgentviewPanelSelectedText(ulong id) { var s = _avPanelSelectedText(id, out UIntPtr len); return Utf8(s, len); }
        /// Input mouse-down. kind: 0 click, 2 double, 3 triple. Returns 1 if the
        /// Send/Stop button was hit (action done; do NOT start a drag).
        public byte AgentviewInputDown(ulong id, float x, float y, byte kind) => _avInputDown(id, x, y, kind);
        public void AgentviewInputDrag(ulong id, float x, float y) => _avInputDrag(id, x, y);
        /// Enter sends, Shift+Enter newlines, the rest edits — all handled in Rust.
        public void AgentviewInputKey(ulong id, string name, bool ctrl, bool alt, bool shift) =>
            _avInputKey(id, name ?? string.Empty, ctrl, alt, shift);
        public void AgentviewInputInsert(ulong id, string text) { if (!string.IsNullOrEmpty(text)) _avInputInsert(id, text); }
        public void AgentviewInputSetPreedit(ulong id, string text) => _avInputSetPreedit(id, text ?? "");
        public void AgentviewInputUndo(ulong id) => _avInputUndo(id);
        public void AgentviewInputRedo(ulong id) => _avInputRedo(id);
        public void AgentviewInputSelectAll(ulong id) => _avInputSelectAll(id);
        public string AgentviewInputCopy(ulong id) { var s = _avInputCopy(id, out UIntPtr len); return Utf8(s, len); }
        public string AgentviewInputCut(ulong id) { var s = _avInputCut(id, out UIntPtr len); return Utf8(s, len); }
        public string AgentviewInputText(ulong id) { var s = _avInputText(id, out UIntPtr len); return Utf8(s, len); }

        private static uint Pack(Color32 c) => (uint)((c.r << 16) | (c.g << 8) | c.b);

        public void Dispose()
        {
            if (_handle != IntPtr.Zero)
            {
                // Best-effort: the OS keeps the image mapped while other (leaked)
                // refs from prior reloads or sibling windows remain — intended,
                // the native globals must outlive any single managed wrapper.
                NativeClose(_handle);
                _handle = IntPtr.Zero;
            }
            _create = null; _createCommand = null; _createSeeded = null; _createDead = null; _dump = null; _cwd = null;
            _exists = null; _destroy = null; _resize = null; _setScale = null;
            _setFont = null; _setFontSize = null; _setColors = null; _setFocus = null;
            _sendText = null; _sendKey = null; _scroll = null; _render = null; _dirty = null; _present = null;
            _selStart = null; _selUpdate = null; _selClear = null; _selText = null;
            _isAlive = null; _iosurface = null; _rawTexture = null;
            _paste = null; _clear = null;
            _size = null; _gridSize = null; _scrollState = null; _cursorPx = null; _title = null;
            _mcpSetTools = null; _mcpNextCall = null; _mcpRespond = null;
            _avCreate = null; _avLoad = null; _avExists = null; _avDestroy = null;
            _avPoll = null; _avRender = null; _avResize = null; _avSetTheme = null; _avSetFonts = null;
            _avPanelTexture = null; _avInputTexture = null;
            _avContentHeight = null; _avInputHeight = null; _avSetScroll = null; _avCaret = null;
            _avInterrupt = null; _avSessionId = null; _avTitle = null; _avTakeHostCommand = null;
            _avSetPermissionMode = null; _avPermissionMode = null; _avSetModel = null; _avModel = null;
            _avQueueLen = null; _avCancelQueued = null;
            _avPanelDown = null; _avPanelDrag = null; _avPanelScrollH = null; _avPanelScrollV = null;
            _avPanelSelectAll = null; _avPanelSelectClear = null; _avPanelHasSelection = null; _avPanelSelectedText = null;
            _avThinking = null;
            _avInputDown = null; _avInputDrag = null; _avInputKey = null;
            _avInputInsert = null; _avInputSetPreedit = null; _avInputUndo = null; _avInputRedo = null; _avInputSelectAll = null;
            _avInputCopy = null; _avInputCut = null; _avInputText = null;
#if UNITY_EDITOR_OSX
            _setUnityDevice = null;
#endif

            if (!_stable && !string.IsNullOrEmpty(_shadowPath) && File.Exists(_shadowPath))
            {
                try { File.Delete(_shadowPath); } catch { /* best effort */ }
            }
            _shadowPath = null;
        }
    }
}
