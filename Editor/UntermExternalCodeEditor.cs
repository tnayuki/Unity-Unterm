using System.IO;
using Unity.CodeEditor;
using UnityEditor;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// Registers the Unterm code editor as a selectable "External Script Editor"
    /// (Preferences &gt; External Tools). When Unterm is chosen, every script open
    /// Unity routes through the configured editor lands in
    /// <see cref="UntermCodeEditorWindow"/> — double-click, compile-error jump,
    /// "Open C# Project", and the agent transcript's path clicks (which go through
    /// <c>CodeEditor.Editor.CurrentCodeEditor.OpenProject</c>). This replaces the old
    /// OnOpenAsset hijack + preference toggle with Unity's standard mechanism.
    /// </summary>
    [InitializeOnLoad]
    internal sealed class UntermExternalCodeEditor : IExternalCodeEditor
    {
        // The "installation path" Unity stores as the selected editor. Unterm is
        // in-editor (no executable), but Unity's dropdown only lists installations
        // whose path exists on disk — so we key off the package's own package.json,
        // a real file unique to this package.
        private static readonly string EditorKey =
            Path.GetFullPath("Packages/dev.tnayuki.unterm/package.json");

        static UntermExternalCodeEditor()
        {
            try
            {
                CodeEditor.Register(new UntermExternalCodeEditor());
            }
            catch (System.Exception e)
            {
                Debug.LogError("[Unterm] External editor registration failed: " + e);
            }
        }

        public CodeEditor.Installation[] Installations { get; } =
        {
            new CodeEditor.Installation { Name = "Unterm Code Editor", Path = EditorKey },
        };

        public bool TryGetInstallationForPath(string editorPath, out CodeEditor.Installation installation)
        {
            if (editorPath == EditorKey)
            {
                installation = Installations[0];
                return true;
            }
            installation = default;
            return false;
        }

        public void Initialize(string editorInstallationPath) { }

        // Unterm highlights with tree-sitter and completes with in-process Roslyn, so
        // it needs no generated .sln/.csproj — syncing is a no-op.
        public void SyncIfNeeded(string[] addedFiles, string[] deletedFiles, string[] movedFiles,
            string[] movedFromFiles, string[] importedFiles) { }

        public void SyncAll() { }

        public void OnGUI()
        {
            EditorGUILayout.HelpBox(
                "Unterm opens scripts in its in-editor code editor (tree-sitter highlighting, " +
                "Roslyn completion). No external application or solution files are used.",
                MessageType.None);
        }

        public bool OpenProject(string filePath = "", int line = -1, int column = -1)
        {
            if (string.IsNullOrEmpty(filePath))
            {
                // "Open C# Project" with no specific file: just surface the editor.
                UntermCodeEditorWindow.OpenEmpty();
                return true;
            }
            if (!File.Exists(filePath)) return false;
            UntermCodeEditorWindow.OpenPath(filePath, line);
            return true;
        }
    }
}
