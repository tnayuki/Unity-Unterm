//! Relative-time helpers for the transcript's time-gap separators.
//!
//! The conversation model stamps each block in unix seconds; `serialize` marks a
//! lull between consecutive blocks with a separator whose label is that block's
//! age relative to *now* — "5 minutes ago", "2 hours ago", "3 days ago". The
//! wording comes from the `timeago` crate (the Rust counterpart to moment.js's
//! `fromNow`), localized to the editor's locale, so we don't hand-roll thresholds
//! or translations. Everything is relative, so no timezone/DST handling is
//! needed; the only absolute-time code is parsing the session file's stamps on
//! resume.

use std::sync::OnceLock;
use std::time::Duration;

use timeago::{BoxedLanguage, Formatter, TimeUnit};

/// Current unix time in seconds (0 if the system clock is before the epoch).
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `timeago` formatter for the editor's locale, built once. `Minutes` is the
/// floor unit — separators only appear for multi-minute gaps, so sub-minute noise
/// ("30 seconds ago") never helps — and one item keeps it to a single unit
/// ("2 hours ago", not "2 hours 5 minutes ago").
fn formatter() -> &'static Formatter<BoxedLanguage> {
    static F: OnceLock<Formatter<BoxedLanguage>> = OnceLock::new();
    F.get_or_init(|| {
        let mut f = Formatter::with_language(detect_language());
        f.min_unit(TimeUnit::Minutes).num_items(1);
        f
    })
}

/// Resolve the editor's locale (e.g. `ja-JP`) to a `timeago` language, falling
/// back to English for an undetectable or unsupported locale.
fn detect_language() -> BoxedLanguage {
    sys_locale::get_locale()
        .as_deref()
        .and_then(|loc| loc.split(['-', '_']).next())
        .map(str::to_ascii_lowercase)
        .and_then(|code| isolang::Language::from_639_1(&code))
        .and_then(timeago::from_isolang)
        .unwrap_or_else(|| Box::new(timeago::English))
}

/// Label for a block stamped `stamp` viewed at `now` (unix seconds): its age as a
/// localized relative phrase. Clock skew (`now < stamp`) collapses to the
/// formatter's "now" string.
pub fn format_relative(stamp: u64, now: u64) -> String {
    formatter().convert(Duration::from_secs(now.saturating_sub(stamp)))
}

/// Parse an RFC 3339 timestamp ("2026-07-02T08:12:34.567Z", as written in Claude
/// Code's session files) to unix seconds. 0 on any parse failure — callers treat
/// 0 as "unknown" and skip gap logic for that block. Hand-rolled so the crate
/// needs no date library just for this one field.
pub fn parse_iso8601_secs(s: &str) -> u64 {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':'
    {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (Some(y), Some(m), Some(d), Some(hh), Some(mm), Some(ss)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return 0;
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return 0;
    }
    // Days from the unix epoch to this civil date (Howard Hinnant, public domain),
    // then add the intraday seconds. UTC — the source stamps carry a `Z`.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    secs.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_session_stamps_and_rejects_malformed() {
        assert_eq!(parse_iso8601_secs("1970-01-01T00:00:00Z"), 0);
        // Intraday fields add onto midnight; the sub-second suffix is ignored.
        let midnight = parse_iso8601_secs("2026-07-02T00:00:00Z");
        assert_ne!(midnight, 0);
        let intraday = 8 * 3600 + 12 * 60 + 34;
        assert_eq!(parse_iso8601_secs("2026-07-02T08:12:34Z"), midnight + intraday);
        assert_eq!(parse_iso8601_secs("2026-07-02T08:12:34.567Z"), midnight + intraday);
        // Consecutive civil days are exactly 86_400 s apart, across a leap day too.
        let next = parse_iso8601_secs("2026-07-03T00:00:00Z");
        assert_eq!(next - midnight, 86_400);
        let feb29 = parse_iso8601_secs("2024-02-29T00:00:00Z");
        let mar1 = parse_iso8601_secs("2024-03-01T00:00:00Z");
        assert_eq!(mar1 - feb29, 86_400);
        // Malformed / out-of-range → 0 (unknown).
        assert_eq!(parse_iso8601_secs(""), 0);
        assert_eq!(parse_iso8601_secs("not a date"), 0);
        assert_eq!(parse_iso8601_secs("2026-13-02T08:12:34Z"), 0);
        assert_eq!(parse_iso8601_secs("2026-07-02 08:12"), 0);
    }

    #[test]
    fn format_relative_scales_and_survives_skew() {
        // Locale-independent (the wording follows the machine's locale): assert the
        // label grows distinct as the age grows, and that skew can't panic.
        let s = 1_700_000_000;
        let five_min = format_relative(s, s + 5 * 60);
        let three_h = format_relative(s, s + 3 * 3600);
        let four_d = format_relative(s, s + 4 * 86_400);
        assert!(!five_min.is_empty());
        assert_ne!(five_min, three_h);
        assert_ne!(three_h, four_d);
        // now == stamp and now < stamp both collapse to the "too low" label.
        assert_eq!(format_relative(s, s), format_relative(s, s - 100));
    }

    #[test]
    fn japanese_has_no_word_spacing() {
        // Regression guard for the upstream fix (timeago 0.6.1, vi/timeago#41):
        // Japanese must render without the default inter-word spaces.
        use timeago::languages::japanese::Japanese;
        let mut f = Formatter::with_language(Box::new(Japanese) as BoxedLanguage);
        f.min_unit(TimeUnit::Minutes).num_items(1);
        assert_eq!(f.convert(Duration::from_secs(7 * 86_400)), "1週間前");
    }
}
