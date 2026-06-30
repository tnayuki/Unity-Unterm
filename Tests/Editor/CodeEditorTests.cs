using System;
using System.IO;
using System.Text.RegularExpressions;
using NUnit.Framework;
using Unterm.Editor;

namespace Unterm.Editor.Tests
{
    /// <summary>
    /// EditMode E2E tests for the code-editor surface. They drive the SHIPPED
    /// boundary — C# marshaling (<see cref="UntermNative"/>) into the built native
    /// bundle — and assert on text state read back via the FFI, so they cover the
    /// integration layer the pure Rust <c>editops</c> unit tests can't.
    ///
    /// No PTY/subprocess/network is involved and every op is synchronous and
    /// deterministic, so these are plain [Test]s (no coroutine pumping). The only
    /// external requirement is a GPU: <c>EditorCreate</c> builds a renderer. When
    /// the bundle isn't built (CI without the build step) or no GPU is present, the
    /// affected tests <c>Assert.Ignore</c> rather than fail.
    /// </summary>
    public class CodeEditorTests
    {
        private UntermNative _n;
        private ulong _id;

        [OneTimeSetUp]
        public void LoadBundle()
        {
            var path = UntermWindow.PluginPath;
            if (string.IsNullOrEmpty(path) || !File.Exists(path))
                Assert.Ignore($"native bundle not built (run native/build-macos.sh): {path}");

            UntermWindow.EnsureNativeImageLoaded();
            _n = new UntermNative();
            _n.Load(path);
            Assert.IsTrue(_n.IsLoaded, "native bundle failed to load");
        }

        [OneTimeTearDown]
        public void Unload() => _n?.Dispose();

        [SetUp]
        public void CreateEditor()
        {
            _id = _n.EditorCreate(800, 600, 2f);
            if (_id == 0)
                Assert.Ignore("EditorCreate returned 0 (no GPU / headless renderer)");
        }

        [TearDown]
        public void DestroyEditor()
        {
            if (_id != 0) _n.EditorDestroy(_id);
            _id = 0;
        }

        [Test]
        public void SetText_RoundTrips()
        {
            _n.EditorSetText(_id, "hello world");
            Assert.AreEqual("hello world", _n.EditorText(_id));
        }

        [Test]
        public void Insert_AppendsAtLineEnd()
        {
            _n.EditorSetText(_id, "hello");
            _n.EditorKey(_id, "End", false, false, false);
            _n.EditorInsert(_id, " world");
            Assert.AreEqual("hello world", _n.EditorText(_id));
        }

        [Test]
        public void Undo_Redo_RoundTrips()
        {
            _n.EditorSetText(_id, "abc");
            ulong serial0 = _n.EditorEditSerial(_id);

            _n.EditorKey(_id, "End", false, false, false);
            _n.EditorInsert(_id, "X");
            Assert.AreEqual("abcX", _n.EditorText(_id));
            Assert.AreNotEqual(serial0, _n.EditorEditSerial(_id), "edit serial should advance on edit");

            _n.EditorUndo(_id);
            Assert.AreEqual("abc", _n.EditorText(_id));

            _n.EditorRedo(_id);
            Assert.AreEqual("abcX", _n.EditorText(_id));
        }

        [Test]
        public void ReplaceAll_ReturnsCount_AndReplaces()
        {
            _n.EditorSetText(_id, "a a a");
            uint replaced = _n.EditorReplaceAll(_id, "a", "b", true);
            Assert.AreEqual(3u, replaced);
            Assert.AreEqual("b b b", _n.EditorText(_id));
        }

        [Test]
        public void Find_LocatesAndMisses()
        {
            _n.EditorSetText(_id, "alpha beta gamma");
            Assert.IsTrue(_n.EditorFind(_id, "beta", true, true));
            Assert.IsFalse(_n.EditorFind(_id, "zzz", true, true));
        }

        [Test]
        public void Complete_ReplacesWordPrefix()
        {
            _n.EditorSetText(_id, "Console.Wr");
            _n.EditorKey(_id, "End", false, false, false);

            Assert.AreEqual("Wr", _n.EditorWordPrefix(_id));
            _n.EditorComplete(_id, 2, "WriteLine");
            Assert.AreEqual("Console.WriteLine", _n.EditorText(_id));
        }

        [Test]
        public void AddUsing_InsertsImportOnce()
        {
            _n.EditorSetText(_id, "class C {}");
            _n.EditorAddUsing(_id, "System.Linq");
            _n.EditorAddUsing(_id, "System.Linq"); // already imported -> no-op

            var text = _n.EditorText(_id);
            int count = Regex.Matches(text, @"using\s+System\.Linq\s*;").Count;
            Assert.AreEqual(1, count, $"expected exactly one import; got:\n{text}");
        }

        [Test]
        public void Render_ProducesTexture()
        {
            _n.EditorSetText(_id, "int x = 1;");
            _n.EditorSetLanguage(_id, "cs");
            _n.EditorRender(_id);

            if (_n.EditorRawTexture(_id) == IntPtr.Zero)
                Assert.Ignore("no render target (headless / no GPU)");

            Assert.Greater(_n.EditorContentHeight(_id), 0f);
        }

        [Test]
        public void Indent_Outdent_RoundTrip()
        {
            _n.EditorSetText(_id, "a");
            _n.EditorGotoLine(_id, 0);
            _n.EditorIndent(_id);
            Assert.AreEqual("    a", _n.EditorText(_id));
            _n.EditorOutdent(_id);
            Assert.AreEqual("a", _n.EditorText(_id));
        }

        [Test]
        public void ToggleComment_RoundTrip()
        {
            _n.EditorSetLanguage(_id, "cs");
            _n.EditorSetText(_id, "a");
            _n.EditorGotoLine(_id, 0);
            _n.EditorToggleComment(_id);
            StringAssert.StartsWith("//", _n.EditorText(_id));
            _n.EditorToggleComment(_id);
            Assert.AreEqual("a", _n.EditorText(_id));
        }

        [Test]
        public void DuplicateLine_CopiesCaretLine()
        {
            _n.EditorSetText(_id, "a");
            _n.EditorGotoLine(_id, 0);
            _n.EditorDuplicateLine(_id);
            Assert.AreEqual("a\na", _n.EditorText(_id));
        }

        [Test]
        public void MoveLineDown_SwapsWithNext()
        {
            _n.EditorSetText(_id, "a\nb");
            _n.EditorGotoLine(_id, 0);
            _n.EditorMoveLineDown(_id);
            Assert.AreEqual("b\na", _n.EditorText(_id));
        }

        [Test]
        public void DeleteLine_RemovesCaretLine()
        {
            _n.EditorSetText(_id, "a\nb");
            _n.EditorGotoLine(_id, 0);
            _n.EditorDeleteLine(_id);
            Assert.AreEqual("b", _n.EditorText(_id));
        }

        [Test]
        public void GotoLine_MovesCaretToLineStart()
        {
            _n.EditorSetText(_id, "a\nbb\nc");
            _n.EditorGotoLine(_id, 1);
            Assert.AreEqual(2, _n.EditorCaretOffset(_id)); // offset of 'b' (start of line 1)
        }

        [Test]
        public void SelectAll_Copy_ReturnsWholeBuffer()
        {
            _n.EditorSetText(_id, "hello");
            _n.EditorSelectAll(_id);
            Assert.AreEqual("hello", _n.EditorCopy(_id));
        }

        [Test]
        public void Cut_RemovesSelectionFromBuffer()
        {
            _n.EditorSetText(_id, "hello");
            _n.EditorSelectAll(_id);
            Assert.AreEqual("hello", _n.EditorCut(_id));
            Assert.AreEqual("", _n.EditorText(_id));
        }

        [Test]
        public void SetScroll_ClampsAndReportsOffset()
        {
            var sb = new System.Text.StringBuilder();
            for (int i = 0; i < 300; i++) sb.Append(i).Append('\n');
            _n.EditorSetText(_id, sb.ToString());
            _n.EditorRender(_id); // compute layout / content height
            if (_n.EditorRawTexture(_id) == IntPtr.Zero)
                Assert.Ignore("no render target (headless / no GPU)");

            _n.EditorSetScroll(_id, 150f);
            Assert.Greater(_n.EditorScrollOffset(_id), 0f); // long doc -> 150px is valid
            _n.EditorSetScroll(_id, 0f);
            Assert.AreEqual(0f, _n.EditorScrollOffset(_id));
        }
    }
}
