using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Threading;
using UnityEditor;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Unterm.Editor
{
    /// <summary>
    /// Detects whether the Claude Code CLI (<c>claude</c>) is installed and gates
    /// the "Window/Unterm/Claude Code" entry on it: the item is enabled only when
    /// the CLI is found, and selecting it opens the agent panel
    /// (<see cref="UntermAgentWindow"/>).
    ///
    /// Detection resolves <c>claude</c> to an absolute path: it prefers the native
    /// installer's predictable locations (<c>~/.local/bin/claude</c>, legacy
    /// <c>~/.claude/local/claude</c>), and only falls back to asking the shell when
    /// none exist — on macOS the login+interactive shell (Unity launched from the
    /// GUI has a minimal PATH; the rc sources a node-version-manager npm install),
    /// on Windows <c>where</c> (GUI processes inherit the full user PATH). The
    /// resolved path is handed to the native agent at spawn (see <see cref="ClaudePath"/>),
    /// so "detected" is exactly "what gets launched".
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
        // The resolved absolute path, cached in SessionState so it survives domain
        // reloads (and a full restart re-resolves it synchronously, see ClaudePath).
        private const string PathKey = "Unterm.ClaudeCodePath";

        /// The resolved absolute path to the `claude` CLI, passed to the native agent
        /// at spawn (<see cref="UntermNative.AgentviewCreate"/>). Resolves synchronously
        /// from the predictable native-install locations if the async probe hasn't
        /// landed yet — so a window restored on editor restart has the path *before*
        /// it spawns, instead of racing the probe. "" if claude can't be found that way
        /// (the probe's slower shell/`where` fallback may still fill it in later).
        internal static string ClaudePath
        {
            get
            {
                string cached = SessionState.GetString(PathKey, "");
                if (!string.IsNullOrEmpty(cached)) return cached;
                foreach (var p in NativeInstallPaths())
                {
                    try { if (File.Exists(p)) { SessionState.SetString(PathKey, p); return p; } }
                    catch { /* unreadable: skip */ }
                }
                return "";
            }
        }

#if UNITY_EDITOR_OSX || UNITY_EDITOR_WIN
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
            // Open the native agent panel (it starts the in-editor MCP server
            // and wires the session to it for the unity_* tools).
            UntermAgentWindow.Open();
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
                string path = ResolveClaudePath();
                EditorApplication.delayCall += () =>
                {
                    // Cache the resolved absolute path so it's handed to the native
                    // agent at spawn (see ClaudePath); SessionState survives C# domain
                    // reloads. The driver execs this path directly — no shell.
                    if (!string.IsNullOrEmpty(path))
                        SessionState.SetString(PathKey, path);
                    SessionState.SetInt(SessionKey, string.IsNullOrEmpty(path) ? 0 : 1);
                    s_detecting = false;
                };
            })
            {
                IsBackground = true,
                Name = "UntermClaudeProbe",
            };
            thread.Start();
        }

        // Resolve `claude` to an absolute path. Prefer the predictable native-install
        // locations (instant, no subprocess, and independent of how the GUI's PATH is
        // set up); only if none exist fall back to asking the shell/`where` the way a
        // real terminal would. Returns "" when not found.
        private static string ResolveClaudePath()
        {
            foreach (var p in NativeInstallPaths())
            {
                try { if (File.Exists(p)) return p; } catch { /* unreadable: skip */ }
            }
            try
            {
                using var proc = Process.Start(BuildProbe());
                if (proc == null) return "";

                string outp = proc.StandardOutput.ReadToEnd();
                proc.StandardError.ReadToEnd();
                if (!proc.WaitForExit(5000))
                {
                    try { proc.Kill(); } catch { /* already gone */ }
                    return "";
                }
                if (proc.ExitCode != 0) return "";

                // `command -v` / `where` may print more than one line; take the first.
                foreach (var line in outp.Split('\n'))
                {
                    string t = line.Trim();
                    if (!string.IsNullOrEmpty(t)) return t;
                }
                return "";
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Unterm] Claude Code detection failed: " + e.Message);
                return "";
            }
        }

        // The native installer's predictable locations (most-preferred first):
        // the current `~/.local/bin` target and the legacy `~/.claude/local`.
        private static IEnumerable<string> NativeInstallPaths()
        {
            string home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            if (string.IsNullOrEmpty(home)) yield break;
#if UNITY_EDITOR_WIN
            yield return Path.Combine(home, ".local", "bin", "claude.exe");
            yield return Path.Combine(home, ".claude", "local", "claude.exe");
#else
            yield return Path.Combine(home, ".local", "bin", "claude");
            yield return Path.Combine(home, ".claude", "local", "claude");
#endif
        }

#if UNITY_EDITOR_WIN
        // Windows GUI processes inherit the full user PATH, so `where` resolves a
        // CLI installed via npm/winget without sourcing a shell rc. `where.exe`
        // exits 0 and prints the path when found, 1 when not.
        private static ProcessStartInfo BuildProbe() => new ProcessStartInfo
        {
            FileName = "where.exe",
            Arguments = "claude",
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
#else
        // Resolve `claude` through `$SHELL -lic 'command -v claude'`: -l (login)
        // and -i (interactive) source both profile and rc so PATH matches a real
        // terminal; -c runs the probe.
        private static ProcessStartInfo BuildProbe()
        {
            string shell = Environment.GetEnvironmentVariable("SHELL");
            if (string.IsNullOrEmpty(shell)) shell = "/bin/zsh";

            return new ProcessStartInfo
            {
                FileName = shell,
                Arguments = "-lic \"command -v claude\"",
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true,
            };
        }
#endif
    }
}
