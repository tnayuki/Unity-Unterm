using System;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Net;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using Newtonsoft.Json.Linq;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Unterm.Editor
{
    /// <summary>
    /// Downloads Anthropic's official standalone Claude Code engine binary from the
    /// npm registry into a per-user (not per-project) managed directory, so Unterm
    /// works even when the user hasn't installed <c>claude</c> themselves.
    ///
    /// We download rather than bundle on purpose: claude-code is "Copyright Anthropic
    /// PBC. All rights reserved." with no redistribution grant, so shipping it inside
    /// this package would be redistribution. Fetching it from Anthropic's official
    /// registry at the user's request is the same path <c>npm install</c> (and Zed's
    /// Claude Code integration) take — no redistribution happens.
    ///
    /// The binary is the platform package <c>@anthropic-ai/claude-agent-sdk-&lt;rid&gt;</c>
    /// — a Bun-compiled, self-contained native executable (~214MB) that needs no Node.
    /// Unterm tracks the registry's latest release rather than pinning a version: the
    /// native driver speaks claude's internal, undocumented control protocol (see
    /// <c>control.rs</c>), so a future claude release could break the panel until a new
    /// Unterm handles it — accepted by design (breakage gets reported and fixed).
    /// </summary>
    internal static class UntermClaudeInstaller
    {
        private const string Scope = "@anthropic-ai";
        private const string BasePackage = "claude-agent-sdk";

        // WebClient (System.dll, always referenced — unlike System.Net.Http, whose
        // availability depends on the API compatibility level) follows redirects to
        // the registry CDN and lets us stream OpenRead() ourselves for progress.
        private static WebClient NewClient()
        {
            var wc = new WebClient();
            wc.Headers.Add(HttpRequestHeader.UserAgent, "Unterm");
            // npm's abbreviated metadata: dist-tags + per-version dist, much smaller
            // than the full document (which carries every version's full manifest).
            wc.Headers.Add(HttpRequestHeader.Accept, "application/vnd.npm.install-v1+json");
            return wc;
        }

        /// The version that is actually usable right now — i.e. what the agent panel
        /// will launch — or "" if nothing is installed. This is the newest downloaded
        /// version (normally the only one: each install cleans up the previous).
        internal static string InstalledVersion()
        {
            try
            {
                string root = ManagedRoot();
                if (!Directory.Exists(root)) return "";
                string best = "";
                foreach (var dir in Directory.GetDirectories(root))
                {
                    string ver = Path.GetFileName(dir);
                    if (ver.StartsWith(".")) continue; // in-flight temp dirs
                    if (File.Exists(Path.Combine(dir, BinaryName)) && CompareVersions(ver, best) > 0)
                        best = ver;
                }
                return best;
            }
            catch { return ""; }
        }

        /// Absolute path to the installed binary the agent panel will launch, or "".
        internal static string InstalledBinaryPath()
        {
            string v = InstalledVersion();
            return string.IsNullOrEmpty(v) ? "" : BinaryPath(v);
        }

        /// The "latest" dist-tag for this platform's package, fetched from the registry
        /// (a network call — run it off the main thread). "" on any failure. Unterm
        /// always tracks latest: it does NOT pin a version, so a claude release that
        /// changes the control protocol can break the panel until a new Unterm handles
        /// it — by design (breakage gets reported and fixed).
        internal static string LatestVersion()
        {
            try { return (string)FetchPackageDoc()?["dist-tags"]?["latest"] ?? ""; }
            catch { return ""; }
        }

        private static JObject FetchPackageDoc()
        {
            string url = "https://registry.npmjs.org/" + Scope + "/" + BasePackage + "-" + Rid();
            string json;
            using (var wc = NewClient()) json = wc.DownloadString(url);
            return JObject.Parse(json);
        }

        // Numeric-aware version compare so 0.3.10 > 0.3.9 (plain string compare gets
        // that backwards). Only matters when two installs coexist — e.g. an old dir
        // whose deletion failed because a running claude held it open on Windows.
        private static int CompareVersions(string a, string b)
        {
            if (a == b) return 0;
            if (string.IsNullOrEmpty(a)) return -1;
            if (string.IsNullOrEmpty(b)) return 1;
            string[] pa = a.Split('.'), pb = b.Split('.');
            int n = Math.Max(pa.Length, pb.Length);
            for (int i = 0; i < n; i++)
            {
                int x = i < pa.Length ? ParseLeadingInt(pa[i]) : 0;
                int y = i < pb.Length ? ParseLeadingInt(pb[i]) : 0;
                if (x != y) return x < y ? -1 : 1;
            }
            return string.CompareOrdinal(a, b);
        }

        private static int ParseLeadingInt(string s)
        {
            int v = 0;
            foreach (char c in s) { if (c < '0' || c > '9') break; v = v * 10 + (c - '0'); }
            return v;
        }

        // Managed install root, keyed only on the OS user (NOT the project): one
        // download is shared across every Unity project. Deliberately not
        // Application.persistentDataPath, which is Company/Product (i.e. per-project).
        internal static string ManagedRoot()
        {
            string home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
#if UNITY_EDITOR_WIN
            string baseDir = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
            if (string.IsNullOrEmpty(baseDir)) baseDir = Path.Combine(home, "AppData", "Local");
            return Path.Combine(baseDir, "dev.tnayuki.unterm", "claude");
#elif UNITY_EDITOR_OSX
            return Path.Combine(home, "Library", "Application Support", "dev.tnayuki.unterm", "claude");
#else
            string xdg = Environment.GetEnvironmentVariable("XDG_DATA_HOME");
            string baseDir = string.IsNullOrEmpty(xdg) ? Path.Combine(home, ".local", "share") : xdg;
            return Path.Combine(baseDir, "dev.tnayuki.unterm", "claude");
#endif
        }

        internal static string BinaryName =>
#if UNITY_EDITOR_WIN
            "claude.exe";
#else
            "claude";
#endif

        // Absolute path the binary would live at for a given version (may not exist).
        private static string BinaryPath(string version) =>
            Path.Combine(ManagedRoot(), version, BinaryName);

        // The npm RID for the platform package: <os>-<cpu>, matching Anthropic's
        // optionalDependencies (darwin-arm64, win32-x64, linux-arm64, ...). Unity
        // Editor on linux is glibc, so the non-musl variant is correct.
        private static string Rid()
        {
            string cpu = RuntimeInformation.OSArchitecture == Architecture.Arm64 ? "arm64" : "x64";
#if UNITY_EDITOR_WIN
            return "win32-" + cpu;
#elif UNITY_EDITOR_OSX
            return "darwin-" + cpu;
#else
            return "linux-" + cpu;
#endif
        }

        /// Download and install the latest claude binary. Runs on a caller's background
        /// thread; <paramref name="onProgress"/> is invoked with (bytesDownloaded,
        /// totalBytes); totalBytes is 0 when the server sends no Content-Length.
        /// Returns null on success, or an error message on failure.
        internal static string Download(Action<long, long> onProgress)
        {
            string tmpDir = null;
            try
            {
                string rid = Rid();
                string pkgName = BasePackage + "-" + rid;     // claude-agent-sdk-darwin-arm64

                // 1. Registry metadata → latest version + its tarball URL + integrity.
                var doc = FetchPackageDoc();
                string version = (string)doc?["dist-tags"]?["latest"];
                if (string.IsNullOrEmpty(version))
                    return $"registry has no latest version for {Scope}/{pkgName}";
                var dist = doc["versions"]?[version]?["dist"];
                string tarball = (string)dist?["tarball"];
                string integrity = (string)dist?["integrity"];
                if (string.IsNullOrEmpty(tarball))
                    return $"registry has no tarball for {pkgName}@{version}";

                // 2. Download to a temp dir, hashing the bytes as they stream in.
                tmpDir = Path.Combine(ManagedRoot(), ".tmp-" + version + "-" + Guid.NewGuid().ToString("N"));
                Directory.CreateDirectory(tmpDir);
                string tgzPath = Path.Combine(tmpDir, "pkg.tgz");
                string sha512 = DownloadFile(tarball, tgzPath, onProgress);

                // 3. Verify integrity ("sha512-<base64>") when the registry provides it.
                if (!string.IsNullOrEmpty(integrity) && integrity.StartsWith("sha512-"))
                {
                    string want = integrity.Substring("sha512-".Length);
                    if (!string.Equals(want, sha512, StringComparison.Ordinal))
                        return $"integrity check failed for {pkgName}@{version}";
                }

                // 4. Extract package/<binary> from the gzip'd tar.
                string staged = Path.Combine(tmpDir, BinaryName);
                if (!ExtractBinary(tgzPath, staged))
                    return $"could not find {BinaryName} inside {pkgName}@{version}";

#if !UNITY_EDITOR_WIN
                Chmod755(staged);
#endif
                // 5. Move into <root>/<version>/ (replace any partial install there).
                string destDir = Path.Combine(ManagedRoot(), version);
                Directory.CreateDirectory(destDir);
                string destBin = Path.Combine(destDir, BinaryName);
                try { if (File.Exists(destBin)) File.Delete(destBin); } catch { /* in use: best effort */ }
                File.Move(staged, destBin);

                CleanupOtherVersions(version);
                return null;
            }
            catch (Exception e)
            {
                return e.Message;
            }
            finally
            {
                try { if (tmpDir != null && Directory.Exists(tmpDir)) Directory.Delete(tmpDir, true); }
                catch { /* leftover temp: harmless, cleaned on next install */ }
            }
        }

        // Stream a URL to disk, reporting (bytesRead, totalBytes) and returning the
        // base64 SHA-512 of the bytes (to match npm's dist.integrity).
        private static string DownloadFile(string url, string dest, Action<long, long> onProgress)
        {
            using var wc = NewClient();
            using var src = wc.OpenRead(url);
            long.TryParse(wc.ResponseHeaders?[HttpResponseHeader.ContentLength], out long total);

            using var dst = new FileStream(dest, FileMode.Create, FileAccess.Write, FileShare.None);
            using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA512);

            var buf = new byte[1 << 16];
            long read = 0;
            int n;
            while ((n = src.Read(buf, 0, buf.Length)) > 0)
            {
                dst.Write(buf, 0, n);
                hash.AppendData(buf, 0, n);
                read += n;
                onProgress?.Invoke(read, total);
            }
            return Convert.ToBase64String(hash.GetHashAndReset());
        }

        // Minimal tar-over-gzip reader: walk 512-byte headers and extract only the
        // regular file whose basename is BinaryName (npm tarballs are plain ustar
        // with short paths under "package/", so no GNU long-name handling needed).
        private static bool ExtractBinary(string tgzPath, string destBin)
        {
            using var fs = new FileStream(tgzPath, FileMode.Open, FileAccess.Read);
            using var gz = new GZipStream(fs, CompressionMode.Decompress);

            var header = new byte[512];
            while (ReadExact(gz, header, 512))
            {
                if (IsAllZero(header)) break; // end-of-archive marker
                string name = ParseString(header, 0, 100);
                long size = ParseOctal(header, 124, 12);
                char typeflag = (char)header[156];
                bool regular = typeflag == '0' || typeflag == '\0';

                string slash = name.Replace('\\', '/');
                string baseName = slash.Substring(slash.LastIndexOf('/') + 1);
                long padded = size + ((512 - (size % 512)) % 512);

                if (regular && baseName == BinaryName)
                {
                    using (var outFs = new FileStream(destBin, FileMode.Create, FileAccess.Write))
                        CopyN(gz, outFs, size);
                    Skip(gz, padded - size);
                    return true;
                }
                Skip(gz, padded);
            }
            return false;
        }

        // Delete sibling version dirs so an Update reclaims the ~214MB of the old one.
        // A version still in use (a running claude holds the file open) only blocks
        // deletion on Windows, where it is silently skipped.
        private static void CleanupOtherVersions(string keep)
        {
            try
            {
                foreach (var dir in Directory.GetDirectories(ManagedRoot()))
                {
                    string name = Path.GetFileName(dir);
                    if (name == keep || name.StartsWith(".")) continue;
                    try { Directory.Delete(dir, true); } catch { /* in use: leave it */ }
                }
            }
            catch { /* root vanished: nothing to clean */ }
        }

#if !UNITY_EDITOR_WIN
        private static void Chmod755(string path)
        {
            try
            {
                using var p = Process.Start(new ProcessStartInfo
                {
                    FileName = "/bin/chmod",
                    Arguments = "755 \"" + path + "\"",
                    UseShellExecute = false,
                    CreateNoWindow = true,
                });
                p?.WaitForExit(5000);
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Unterm] chmod on claude binary failed: " + e.Message);
            }
        }
#endif

        // --- tar primitives ----------------------------------------------------

        private static bool ReadExact(Stream s, byte[] buf, int count)
        {
            int off = 0;
            while (off < count)
            {
                int n = s.Read(buf, off, count - off);
                if (n <= 0) return off == 0 ? false : throw new EndOfStreamException("truncated tar");
                off += n;
            }
            return true;
        }

        private static bool IsAllZero(byte[] buf)
        {
            foreach (var b in buf) if (b != 0) return false;
            return true;
        }

        private static string ParseString(byte[] buf, int off, int len)
        {
            int end = off;
            while (end < off + len && buf[end] != 0) end++;
            return System.Text.Encoding.ASCII.GetString(buf, off, end - off);
        }

        private static long ParseOctal(byte[] buf, int off, int len)
        {
            // GNU base-256 encoding for large sizes sets the high bit of the first byte.
            if ((buf[off] & 0x80) != 0)
            {
                long v = buf[off] & 0x7f;
                for (int i = 1; i < len; i++) v = (v << 8) | buf[off + i];
                return v;
            }
            int p = off, e = off + len;
            while (p < e && (buf[p] == ' ' || buf[p] == 0)) p++;
            long val = 0;
            while (p < e && buf[p] >= '0' && buf[p] <= '7') { val = val * 8 + (buf[p] - '0'); p++; }
            return val;
        }

        private static void CopyN(Stream src, Stream dst, long count)
        {
            var buf = new byte[1 << 16];
            while (count > 0)
            {
                int want = (int)Math.Min(buf.Length, count);
                int n = src.Read(buf, 0, want);
                if (n <= 0) throw new EndOfStreamException("truncated tar entry");
                dst.Write(buf, 0, n);
                count -= n;
            }
        }

        private static void Skip(Stream src, long count)
        {
            var buf = new byte[1 << 16];
            while (count > 0)
            {
                int want = (int)Math.Min(buf.Length, count);
                int n = src.Read(buf, 0, want);
                if (n <= 0) break;
                count -= n;
            }
        }
    }
}
