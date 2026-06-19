using System;
using System.IO;
using System.Runtime.InteropServices;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// dlopen loader for the Unterm native terminal plugin (macOS).
    ///
    /// Terminals live in process globals on the native side (the registry keyed
    /// by a stable u64 id), so they survive Unity C# domain reloads. To keep
    /// them mapped we load the bundle via a *stable* shadow copy and never
    /// dlclose on reload. Every editor window loads the same shadow path, so
    /// they all share one native image and one registry; each window owns one
    /// terminal id it serializes and re-adopts after a reload.
    /// </summary>
    internal sealed class UntermNative : IDisposable
    {
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

        // --- terminal registry C ABI (id-based; survives reload) ---
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate ulong CreateFn(uint w, uint h, float scale, [MarshalAs(UnmanagedType.LPUTF8Str)] string cwd);
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
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr PixelsFn(ulong id, out UIntPtr len);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate void SizeFn(ulong id, out uint a, out uint b);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] [return: MarshalAs(UnmanagedType.I1)] private delegate bool CursorPxFn(ulong id, out float x, out float y, out float w, out float h);
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate IntPtr TitleFn(ulong id, out UIntPtr len);

        private IntPtr _handle;
        private string _shadowPath;
        private bool _stable;

        private CreateFn _create; private ExistsFn _exists; private IdFn _destroy; private ResizeFn _resize;
        private SetScaleFn _setScale; private SetFontFn _setFont; private SetFontSizeFn _setFontSize;
        private SetColorsFn _setColors; private SetFocusFn _setFocus; private SendTextFn _sendText;
        private SendKeyFn _sendKey; private SendTextFn _paste; private IdFn _clear;
        private ScrollFn _scroll; private IdFn _render; private BoolFn _dirty;
        private SelStartFn _selStart; private SelUpdateFn _selUpdate; private IdFn _selClear; private TitleFn _selText;
        private BoolFn _isAlive; private PtrFn _iosurface; private PtrFn _rawTexture; private PixelsFn _getPixels;
        private SizeFn _size; private SizeFn _gridSize; private CursorPxFn _cursorPx; private TitleFn _title;

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
            if (!File.Exists(bundlePath))
                throw new FileNotFoundException($"Unterm native bundle not found: {bundlePath}");

            _stable = !freshInstance;
            var info = new FileInfo(bundlePath);
            _shadowPath = freshInstance
                ? Path.Combine(Path.GetTempPath(), $"unterm_{Guid.NewGuid():N}.dylib")
                : Path.Combine(Path.GetTempPath(), $"unterm_{info.Length}_{info.LastWriteTimeUtc.Ticks}.dylib");

            if (freshInstance || !File.Exists(_shadowPath))
                File.Copy(bundlePath, _shadowPath, overwrite: freshInstance);

            _handle = dlopen(_shadowPath, RTLD_NOW | RTLD_LOCAL);
            if (_handle == IntPtr.Zero)
                throw new Exception($"dlopen failed: {ReadDlError()}");

            _create = Sym<CreateFn>("unterm_create");
            _exists = Sym<ExistsFn>("unterm_exists");
            _destroy = Sym<IdFn>("unterm_destroy");
            _resize = Sym<ResizeFn>("unterm_resize");
            _setScale = Sym<SetScaleFn>("unterm_set_scale");
            _setFont = Sym<SetFontFn>("unterm_set_font");
            _setFontSize = Sym<SetFontSizeFn>("unterm_set_font_size");
            _setColors = Sym<SetColorsFn>("unterm_set_colors");
            _setFocus = Sym<SetFocusFn>("unterm_set_focus");
            _sendText = Sym<SendTextFn>("unterm_send_text");
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
            _isAlive = Sym<BoolFn>("unterm_is_alive");
            _iosurface = Sym<PtrFn>("unterm_iosurface");
            _rawTexture = Sym<PtrFn>("unterm_raw_texture");
            _getPixels = Sym<PixelsFn>("unterm_get_pixels");
            _size = Sym<SizeFn>("unterm_size");
            _gridSize = Sym<SizeFn>("unterm_grid_size");
            _cursorPx = Sym<CursorPxFn>("unterm_cursor_px");
            _title = Sym<TitleFn>("unterm_title");
        }

        private T Sym<T>(string name) where T : Delegate
        {
            var addr = dlsym(_handle, name);
            if (addr == IntPtr.Zero)
                throw new Exception($"dlsym('{name}') failed: {ReadDlError()}");
            return Marshal.GetDelegateForFunctionPointer<T>(addr);
        }

        private static string ReadDlError()
        {
            var p = dlerror();
            return p == IntPtr.Zero ? "(no error)" : Marshal.PtrToStringAnsi(p);
        }

        private static string Utf8(IntPtr p, UIntPtr len) =>
            p == IntPtr.Zero ? string.Empty : Marshal.PtrToStringUTF8(p, (int)len.ToUInt64());

        public ulong Create(uint w, uint h, float scale, string cwd) => _create(w, h, scale, cwd ?? string.Empty);
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
        public bool IsAlive(ulong id) => _isAlive(id);
        public IntPtr IOSurface(ulong id) => _iosurface(id);
        public IntPtr RawTexture(ulong id) => _rawTexture(id);
        public IntPtr GetPixels(ulong id, out int length)
        {
            var ptr = _getPixels(id, out UIntPtr len);
            length = (int)len.ToUInt64();
            return ptr;
        }
        public void Size(ulong id, out uint w, out uint h) => _size(id, out w, out h);
        public void GridSize(ulong id, out uint cols, out uint rows) => _gridSize(id, out cols, out rows);
        public bool CursorPx(ulong id, out float x, out float y, out float w, out float h) =>
            _cursorPx(id, out x, out y, out w, out h);
        public string Title(ulong id)
        {
            var p = _title(id, out UIntPtr len);
            return Utf8(p, len);
        }

        private static uint Pack(Color32 c) => (uint)((c.r << 16) | (c.g << 8) | c.b);

        public void Dispose()
        {
            if (_handle != IntPtr.Zero)
            {
                // Best-effort: the OS keeps the image mapped while other (leaked)
                // refs from prior reloads or sibling windows remain — intended,
                // the native globals must outlive any single managed wrapper.
                dlclose(_handle);
                _handle = IntPtr.Zero;
            }
            _create = null; _exists = null; _destroy = null; _resize = null; _setScale = null;
            _setFont = null; _setFontSize = null; _setColors = null; _setFocus = null;
            _sendText = null; _sendKey = null; _scroll = null; _render = null; _dirty = null;
            _selStart = null; _selUpdate = null; _selClear = null; _selText = null;
            _isAlive = null; _iosurface = null; _rawTexture = null; _getPixels = null;
            _paste = null; _clear = null;
            _size = null; _gridSize = null; _cursorPx = null; _title = null;

            if (!_stable && !string.IsNullOrEmpty(_shadowPath) && File.Exists(_shadowPath))
            {
                try { File.Delete(_shadowPath); } catch { /* best effort */ }
            }
            _shadowPath = null;
        }
    }
}
