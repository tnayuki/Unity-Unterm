using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;

namespace Unterm.Editor
{
    /// <summary>
    /// Per-project index of agent sessions — each a session id, a derived
    /// title, and a last-used time — persisted under the project's Library so the
    /// panel can list past conversations to resume. The conversation content
    /// itself lives in Claude Code's own storage (keyed by the session id); this
    /// is only a lightweight index that drives the session picker.
    /// </summary>
    internal static class UntermAgentSessions
    {
        [Serializable]
        public struct Entry
        {
            public string id;
            public string title;
            public long updated; // DateTime.UtcNow.Ticks
        }

        [Serializable]
        private class Index
        {
            public List<Entry> entries = new List<Entry>();
        }

        // Library is per-project and machine-local (gitignored), so the index is
        // naturally scoped to this project without hashing the path into the name.
        private static string IndexPath
        {
            get
            {
                string root = Directory.GetParent(Application.dataPath)?.FullName ?? Application.dataPath;
                return Path.Combine(root, "Library", "Unterm", "agent-sessions.json");
            }
        }

        private static Index LoadIndex()
        {
            try
            {
                string p = IndexPath;
                if (File.Exists(p))
                    return JsonUtility.FromJson<Index>(File.ReadAllText(p)) ?? new Index();
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Unterm] session index read failed: " + e.Message);
            }
            return new Index();
        }

        private static void SaveIndex(Index idx)
        {
            try
            {
                string p = IndexPath;
                Directory.CreateDirectory(Path.GetDirectoryName(p));
                File.WriteAllText(p, JsonUtility.ToJson(idx));
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Unterm] session index write failed: " + e.Message);
            }
        }

        /// All known sessions, most-recently-used first.
        public static List<Entry> All()
        {
            var idx = LoadIndex();
            idx.entries.Sort((a, b) => b.updated.CompareTo(a.updated));
            return idx.entries;
        }

        /// Record or refresh a session. The first non-empty title sticks (so a
        /// later empty render doesn't wipe it); `updated` always bumps.
        public static void Touch(string id, string title)
        {
            if (string.IsNullOrEmpty(id)) return;
            var idx = LoadIndex();
            long now = DateTime.UtcNow.Ticks;
            for (int i = 0; i < idx.entries.Count; i++)
            {
                if (idx.entries[i].id != id) continue;
                var e = idx.entries[i];
                if (string.IsNullOrEmpty(e.title) && !string.IsNullOrEmpty(title)) e.title = title;
                e.updated = now;
                idx.entries[i] = e;
                SaveIndex(idx);
                return;
            }
            idx.entries.Add(new Entry { id = id, title = title ?? "", updated = now });
            SaveIndex(idx);
        }
    }
}
