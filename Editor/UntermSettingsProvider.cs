using System.Collections.Generic;
using System.Threading;
using UnityEditor;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// "Preferences &gt; Unterm" page. Its job is to download Anthropic's standalone
    /// engine binary with a button (see <see cref="UntermClaudeInstaller"/>) and show
    /// the active and latest versions, the resolved binary path, and live download progress.
    /// Once the binary lands, the "Window/Unterm/Claude Code" menu enables on its own —
    /// its validate callback checks <c>File.Exists</c> live (see <see cref="ClaudeCode"/>).
    ///
    /// The download runs on a background thread; the page polls a few progress fields
    /// and repaints itself while it is in flight.
    /// </summary>
    internal static class UntermSettingsProvider
    {
        private static volatile bool s_busy;
        private static long s_downloaded;         // bytes pulled so far (this run)
        private static long s_total;              // total bytes, or 0 if unknown
        private static string s_message;          // last success / error line
        private static bool s_failed;
        private static EditorWindow s_repaintTarget;

        // The registry's "latest" dist-tag, fetched once per page open in the
        // background (network), so we can show "update available".
        private static string s_latest;
        private static volatile bool s_latestChecking;
        private static bool s_latestChecked;

        [SettingsProvider]
        public static SettingsProvider Create()
        {
            return new SettingsProvider("Preferences/Unterm", SettingsScope.User)
            {
                label = "Unterm",
                guiHandler = _ => OnGui(),
                keywords = new HashSet<string>
                {
                    "unterm", "claude", "claude code", "agent", "terminal", "download",
                    "code editor", "undo", "history", "sound", "notify", "notification", "chime",
                },
            };
        }

        private static void OnGui()
        {
            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Claude Code", EditorStyles.boldLabel);
            EditorGUILayout.HelpBox(
                "Unterm's agent panel drives Anthropic's standalone Claude Code engine — no Node " +
                "required. If you haven't installed `claude` yourself, download it here. The binary " +
                "(~214 MB) is fetched from Anthropic's official npm registry into a per-user folder " +
                "shared by all your Unity projects, and you sign in with your own `claude login`.",
                MessageType.Info);

            EnsureLatestChecked();

            string active = UntermClaudeInstaller.InstalledVersion();
            string resolved = ClaudeCode.ClaudePath;
            EditorGUILayout.LabelField("Active version",
                string.IsNullOrEmpty(active) ? "(none — download required)" : active);
            EditorGUILayout.LabelField("Latest version",
                !string.IsNullOrEmpty(s_latest) ? s_latest : (s_latestChecking ? "checking…" : "(unknown)"));
            using (new EditorGUI.DisabledScope(true))
                EditorGUILayout.TextField("Binary path", string.IsNullOrEmpty(resolved) ? "(not found)" : resolved);

            EditorGUILayout.Space();

            if (s_busy)
            {
                long got = s_downloaded, total = s_total;
                float frac = total > 0 ? (float)((double)got / total) : 0f;
                string label = total > 0
                    ? $"Downloading… {Mb(got):0.0} / {Mb(total):0.0} MB ({Mathf.RoundToInt(frac * 100f)}%)"
                    : $"Downloading… {Mb(got):0.0} MB";
                var rect = EditorGUILayout.GetControlRect(false, 20f);
                EditorGUI.ProgressBar(rect, frac, label);
            }
            else
            {
                DrawAction();
            }

            if (!string.IsNullOrEmpty(s_message))
                EditorGUILayout.HelpBox(s_message, s_failed ? MessageType.Error : MessageType.Info);

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Code Editor", EditorStyles.boldLabel);
            int curLimit = UntermCodeEditorPrefs.UndoLimit;
            int nextLimit = EditorGUILayout.IntField(
                new GUIContent("Undo history limit",
                    "Maximum retained undo steps per editor buffer (0 = unlimited). Bounds memory " +
                    "over a long session; takes effect for editors opened afterward."),
                curLimit);
            if (nextLimit != curLimit)
                UntermCodeEditorPrefs.UndoLimit = nextLimit;

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Agent", EditorStyles.boldLabel);
            bool notify = UntermAgentPrefs.NotifySoundEnabled;
            bool nextNotify = EditorGUILayout.Toggle(
                new GUIContent("Notify when idle",
                    "Play a chime and show an OS notification when the agent finishes a turn " +
                    "or needs a permission — but only while the Unity Editor is in the " +
                    "background, so you're only interrupted when you're away."),
                notify);
            if (nextNotify != notify)
                UntermAgentPrefs.NotifySoundEnabled = nextNotify;
        }

        private static void DrawAction()
        {
            string installed = UntermClaudeInstaller.InstalledVersion();
            bool updateAvailable = !string.IsNullOrEmpty(s_latest) &&
                                   !string.IsNullOrEmpty(installed) && s_latest != installed;

            if (string.IsNullOrEmpty(installed))
            {
                if (GUILayout.Button("Download Claude Code")) StartDownload();
            }
            else if (updateAvailable)
            {
                EditorGUILayout.LabelField("Status", $"Installed {installed} — update available ({s_latest})");
                if (GUILayout.Button($"Update to {s_latest}")) StartDownload();
            }
            else
            {
                EditorGUILayout.LabelField("Status", $"Installed ({installed})");
                // Always fetches the current latest, so this doubles as "update" when
                // the latest check couldn't run.
                if (GUILayout.Button("Reinstall latest")) StartDownload();
            }
        }

        private static void StartDownload()
        {
            if (s_busy) return;
            s_busy = true;
            s_downloaded = 0;
            s_total = 0;
            s_message = null;
            s_failed = false;
            s_repaintTarget = EditorWindow.focusedWindow; // the Preferences window
            EditorApplication.update += RepaintWhileBusy;

            var thread = new Thread(() =>
            {
                string err = UntermClaudeInstaller.Download((got, total) =>
                {
                    s_downloaded = got;
                    s_total = total;
                });
                EditorApplication.delayCall += () =>
                {
                    s_busy = false;
                    EditorApplication.update -= RepaintWhileBusy;
                    if (err == null)
                    {
                        s_failed = false;
                        string v = UntermClaudeInstaller.InstalledVersion();
                        s_latest = v; // we just fetched and installed the latest
                        s_message = $"Installed Claude Code {v}. The menu is now enabled.";
                        // The menu's validate checks File.Exists live, so it enables on
                        // its own; nothing else to refresh.
                    }
                    else
                    {
                        s_failed = true;
                        s_message = "Download failed: " + err;
                    }
                    s_repaintTarget?.Repaint();
                };
            })
            {
                IsBackground = true,
                Name = "UntermClaudeDownload",
            };
            thread.Start();
        }

        // Fetch the registry's latest version once per page open, off the main thread.
        private static void EnsureLatestChecked()
        {
            if (s_latestChecked || s_latestChecking) return;
            s_latestChecking = true;
            if (s_repaintTarget == null) s_repaintTarget = EditorWindow.focusedWindow;

            var thread = new Thread(() =>
            {
                string v = UntermClaudeInstaller.LatestVersion();
                EditorApplication.delayCall += () =>
                {
                    s_latest = v;
                    s_latestChecking = false;
                    s_latestChecked = true;
                    s_repaintTarget?.Repaint();
                };
            })
            {
                IsBackground = true,
                Name = "UntermLatestCheck",
            };
            thread.Start();
        }

        private static void RepaintWhileBusy() => s_repaintTarget?.Repaint();

        private static double Mb(long bytes) => bytes / (1024.0 * 1024.0);
    }
}
