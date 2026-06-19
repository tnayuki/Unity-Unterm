using System;
using System.Diagnostics;
using System.Threading;
using UnityEditor;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Unterm.Editor
{
    /// <summary>
    /// Detects whether the Claude Code CLI (<c>claude</c>) is installed and gates
    /// the "Window/Unterm/Claude Code" entry on it: the item is enabled only when
    /// the CLI is found, and selecting it opens a terminal already running it.
    ///
    /// Unity launched from the GUI inherits a minimal PATH, so <c>claude</c>
    /// (usually installed through a node version manager that's sourced from the
    /// shell rc) isn't on the bare PATH. Detection therefore asks the user's
    /// login + interactive shell — the same environment Unterm's PTY shell runs
    /// in — to resolve it, so "detected" matches "would actually run".
    ///
    /// Unity has no supported API to add/remove a menu item at runtime, so the
    /// entry is a static <c>[MenuItem]</c> whose validate callback greys it out
    /// until detection succeeds.
    /// </summary>
    internal static class ClaudeCode
    {
        private const string MenuPath = "Window/Unterm/Claude Code";

        // Per-session cache so the shell is probed at most once per editor session
        // (the value survives domain reloads). -1 unknown, 0 absent, 1 present.
        private const string SessionKey = "Unterm.ClaudeCodeAvailable";

#if UNITY_EDITOR_OSX
        // Probe ahead of time so the menu is usually already resolved (enabled or
        // not) by the first time the user opens it, instead of greyed on first
        // look and only enabled on the next.
        [InitializeOnLoadMethod]
        private static void WarmUp()
        {
            if (SessionState.GetInt(SessionKey, -1) == -1) BeginDetect();
        }

        [MenuItem(MenuPath, priority = 1)]
        public static void OpenClaudeCode()
        {
            UntermWindow.CreateRunning("Claude Code", "claude");
        }

        [MenuItem(MenuPath, validate = true)]
        public static bool OpenClaudeCodeValidate()
        {
            switch (SessionState.GetInt(SessionKey, -1))
            {
                case 1: return true;
                case 0: return false;
                default:
                    BeginDetect(); // still unknown: probe once, greyed until it lands
                    return false;
            }
        }
#endif

        // Probe the shell off the main thread, then publish the result back on the
        // main thread. A guard flag keeps concurrent validate calls to one probe.
        private static bool s_detecting;

        private static void BeginDetect()
        {
            if (s_detecting) return;
            s_detecting = true;

            var thread = new Thread(() =>
            {
                bool found = ResolveClaude();
                EditorApplication.delayCall += () =>
                {
                    SessionState.SetInt(SessionKey, found ? 1 : 0);
                    s_detecting = false;
                };
            })
            {
                IsBackground = true,
                Name = "UntermClaudeProbe",
            };
            thread.Start();
        }

        // Resolve `claude` through `$SHELL -lic 'command -v claude'`: -l (login) and
        // -i (interactive) source both profile and rc so PATH matches a real
        // terminal; -c runs the probe. Returns true only on a clean exit with a
        // non-empty path.
        private static bool ResolveClaude()
        {
            try
            {
                string shell = Environment.GetEnvironmentVariable("SHELL");
                if (string.IsNullOrEmpty(shell)) shell = "/bin/zsh";

                var psi = new ProcessStartInfo
                {
                    FileName = shell,
                    Arguments = "-lic \"command -v claude\"",
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    UseShellExecute = false,
                    CreateNoWindow = true,
                };

                using var p = Process.Start(psi);
                if (p == null) return false;

                string outp = p.StandardOutput.ReadToEnd();
                p.StandardError.ReadToEnd();
                if (!p.WaitForExit(5000))
                {
                    try { p.Kill(); } catch { /* already gone */ }
                    return false;
                }

                return p.ExitCode == 0 && !string.IsNullOrWhiteSpace(outp);
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Unterm] Claude Code detection failed: " + e.Message);
                return false;
            }
        }
    }
}
