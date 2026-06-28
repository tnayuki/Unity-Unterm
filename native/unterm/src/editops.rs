//! Pure text-editing operations for the code editor, working on a line vector so
//! they can be unit-tested without a GPU/buffer. [`crate::input::InputBox`] glues
//! these to the `cosmic_text` editor (applying the result as one undoable change).
//!
//! Columns are CHARACTER offsets within a line (the caller converts to byte
//! indices for cosmic-text). Line endings are normalized to '\n' upstream.

/// Indentation unit (spaces). Tabs are inserted as spaces, matching the host.
pub const INDENT: &str = "    ";

/// Split a document into owned lines (no trailing '\n').
pub fn to_lines(text: &str) -> Vec<String> {
    text.split('\n').map(|s| s.to_string()).collect()
}

/// Leading whitespace (spaces/tabs) of a line.
fn leading_ws(line: &str) -> String {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// The newline-plus-indent string to insert for auto-indent on Enter: carries the
/// current line's indentation, plus one extra level if the caret follows an
/// opening brace. `caret_col` is a character offset into `line`.
pub fn auto_indent(line: &str, caret_col: usize) -> String {
    let ws = leading_ws(line);
    let before: String = line.chars().take(caret_col).collect();
    let opens = before.trim_end().ends_with('{');
    let mut s = String::with_capacity(1 + ws.len() + INDENT.len());
    s.push('\n');
    s.push_str(&ws);
    if opens {
        s.push_str(INDENT);
    }
    s
}

/// Indent lines `l0..=l1` by one level, in place. The caller passes only the
/// affected slice, so no whole-document copy happens.
pub fn indent(lines: &mut [String], l0: usize, l1: usize) {
    for i in l0..=l1.min(lines.len().saturating_sub(1)) {
        if !lines[i].is_empty() || l0 == l1 {
            lines[i] = format!("{INDENT}{}", lines[i]);
        }
    }
}

/// Remove up to one indent level (4 spaces, or a leading tab) from `l0..=l1`, in place.
pub fn outdent(lines: &mut [String], l0: usize, l1: usize) {
    for i in l0..=l1.min(lines.len().saturating_sub(1)) {
        let line = &lines[i];
        if let Some(rest) = line.strip_prefix('\t') {
            lines[i] = rest.to_string();
        } else {
            let spaces = line.chars().take_while(|c| *c == ' ').count().min(INDENT.len());
            lines[i] = line.chars().skip(spaces).collect();
        }
    }
}

/// Toggle a line comment (`prefix`, e.g. "// ") on `l0..=l1`, in place. If every
/// non-blank line is already commented, uncomment; otherwise comment all (at the
/// minimum indentation so they stay aligned).
pub fn toggle_comment(lines: &mut [String], l0: usize, l1: usize, prefix: &str) {
    let l1 = l1.min(lines.len().saturating_sub(1));
    let trimmed = prefix.trim_end();
    let non_blank: Vec<usize> = (l0..=l1).filter(|&i| !lines[i].trim().is_empty()).collect();
    let all_commented = !non_blank.is_empty()
        && non_blank.iter().all(|&i| lines[i].trim_start().starts_with(trimmed));

    if all_commented {
        for &i in &non_blank {
            let indent = leading_ws(&lines[i]);
            let body = lines[i].trim_start();
            let body = body.strip_prefix(prefix).or_else(|| body.strip_prefix(trimmed)).unwrap_or(body);
            lines[i] = format!("{indent}{body}");
        }
    } else {
        // Comment at the shallowest indentation among non-blank lines.
        let min_indent = non_blank
            .iter()
            .map(|&i| leading_ws(&lines[i]).chars().count())
            .min()
            .unwrap_or(0);
        for &i in &non_blank {
            let mut chars: Vec<char> = lines[i].chars().collect();
            let at = min_indent.min(chars.len());
            for (k, c) in prefix.chars().enumerate() {
                chars.insert(at + k, c);
            }
            lines[i] = chars.into_iter().collect();
        }
    }
}

/// Swap line `line` with the one above it, in place. No-op at the top.
pub fn move_up(lines: &mut [String], line: usize) {
    if line > 0 && line < lines.len() {
        lines.swap(line, line - 1);
    }
}

/// Swap line `line` with the one below it, in place. No-op at the bottom.
pub fn move_down(lines: &mut [String], line: usize) {
    if line + 1 < lines.len() {
        lines.swap(line, line + 1);
    }
}

/// Duplicate lines `l0..=l1` in place (the copy is inserted directly below).
pub fn duplicate(lines: &mut Vec<String>, l0: usize, l1: usize) {
    let l1 = l1.min(lines.len().saturating_sub(1));
    let block: Vec<String> = lines[l0..=l1].to_vec();
    for (k, s) in block.into_iter().enumerate() {
        lines.insert(l1 + 1 + k, s);
    }
}

/// Find `query` in `text` starting from character offset `from` (the search wraps
/// around). Returns the matched character range [start, end). Empty query → None.
pub fn find(text: &str, query: &str, from: usize, forward: bool, case_sensitive: bool) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let hay: Vec<char> = text.chars().collect();
    let ned: Vec<char> = query.chars().collect();
    if ned.len() > hay.len() {
        return None;
    }
    let eq = |a: char, b: char| if case_sensitive { a == b } else { a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase()) };
    let matches_at = |i: usize| (0..ned.len()).all(|k| eq(hay[i + k], ned[k]));
    let last = hay.len() - ned.len();

    if forward {
        let start = from.min(last + 1);
        for i in start..=last {
            if matches_at(i) {
                return Some((i, i + ned.len()));
            }
        }
        for i in 0..start {
            if matches_at(i) {
                return Some((i, i + ned.len()));
            }
        }
    } else {
        let start = from.min(last + 1);
        for i in (0..start).rev() {
            if matches_at(i) {
                return Some((i, i + ned.len()));
            }
        }
        for i in (start..=last).rev() {
            if matches_at(i) {
                return Some((i, i + ned.len()));
            }
        }
    }
    None
}

/// Whether the character at `chars[i]` is part of a code "word": identifier chars
/// (alphanumeric or `_`), plus `.` ONLY when between two digits (so a float literal
/// like `1.0f` stays one word while `foo.bar` splits at the dot).
fn word_char_at(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    if c.is_alphanumeric() || c == '_' {
        return true;
    }
    if c == '.' {
        let prev_digit = i > 0 && chars[i - 1].is_ascii_digit();
        let next_digit = i + 1 < chars.len() && chars[i + 1].is_ascii_digit();
        return prev_digit && next_digit;
    }
    false
}

/// Character column to the LEFT of `col` at the previous word start (skips trailing
/// whitespace, then a run of word chars or a run of punctuation).
pub fn word_left(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = col.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    if word_char_at(&chars, i - 1) {
        while i > 0 && word_char_at(&chars, i - 1) {
            i -= 1;
        }
    } else {
        while i > 0 && !word_char_at(&chars, i - 1) && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
    }
    i
}

/// Character column to the RIGHT of `col` at the next word end (skips leading
/// whitespace, then a run of word chars or a run of punctuation).
pub fn word_right(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = col.min(n);
    while i < n && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= n {
        return n;
    }
    if word_char_at(&chars, i) {
        while i < n && word_char_at(&chars, i) {
            i += 1;
        }
    } else {
        while i < n && !word_char_at(&chars, i) && !chars[i].is_whitespace() {
            i += 1;
        }
    }
    i
}

/// The character range [start, end) of the token at column `col` (for double-click
/// selection), using the same code-aware classes: a run of word chars, of
/// whitespace, or of punctuation.
pub fn word_at(line: &str, col: usize) -> (usize, usize) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 {
        return (0, 0);
    }
    let i = col.min(n - 1);
    let is_word = word_char_at(&chars, i);
    let is_ws = chars[i].is_whitespace();
    let same = |j: usize| {
        if is_word {
            word_char_at(&chars, j)
        } else if is_ws {
            chars[j].is_whitespace()
        } else {
            !word_char_at(&chars, j) && !chars[j].is_whitespace()
        }
    };
    let mut s = i;
    while s > 0 && same(s - 1) {
        s -= 1;
    }
    let mut e = i + 1;
    while e < n && same(e) {
        e += 1;
    }
    (s, e)
}

/// Replace every occurrence of `query` with `repl`. Returns (new text, count).
pub fn replace_all(text: &str, query: &str, repl: &str, case_sensitive: bool) -> (String, u32) {
    if query.is_empty() {
        return (text.to_string(), 0);
    }
    let hay: Vec<char> = text.chars().collect();
    let ned: Vec<char> = query.chars().collect();
    let eq = |a: char, b: char| if case_sensitive { a == b } else { a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase()) };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut n = 0u32;
    while i < hay.len() {
        if i + ned.len() <= hay.len() && (0..ned.len()).all(|k| eq(hay[i + k], ned[k])) {
            out.push_str(repl);
            i += ned.len();
            n += 1;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundaries_are_code_aware() {
        // '.' splits member access...
        assert_eq!(word_left("foo.bar", 7), 4); // delete "bar"
        assert_eq!(word_left("foo.bar", 4), 3); // delete "."
        assert_eq!(word_left("a.b.c", 5), 4);
        // ...but a float literal stays one word.
        assert_eq!(word_left("1.0f", 4), 0);
        assert_eq!(word_left("x = 1.0f", 8), 4);
        // forward
        assert_eq!(word_right("foo.bar", 0), 3);
        assert_eq!(word_right("1.0f;", 0), 4);
        // whitespace skipping
        assert_eq!(word_left("foo   ", 6), 0);
    }

    #[test]
    fn word_at_selects_code_token() {
        assert_eq!(word_at("foo.bar", 5), (4, 7)); // "bar"
        assert_eq!(word_at("foo.bar", 0), (0, 3)); // "foo"
        assert_eq!(word_at("foo.bar", 3), (3, 4)); // "."
        assert_eq!(word_at("x = 1.0f;", 6), (4, 8)); // "1.0f"
    }

    #[test]
    fn replace_all_counts() {
        assert_eq!(replace_all("a.a.a", "a", "X", true), ("X.X.X".to_string(), 3));
        assert_eq!(replace_all("Foo foo", "foo", "bar", false), ("bar bar".to_string(), 2));
        assert_eq!(replace_all("abc", "x", "y", true), ("abc".to_string(), 0));
    }

    #[test]
    fn auto_indent_carries_and_opens() {
        assert_eq!(auto_indent("    foo", 7), "\n    ");
        assert_eq!(auto_indent("    if (x) {", 12), "\n        "); // +1 level after {
        assert_eq!(auto_indent("noindent", 8), "\n");
    }

    #[test]
    fn indent_outdent_roundtrip() {
        let mut l = to_lines("a\n  b\n");
        indent(&mut l, 0, 2);
        // Blank lines aren't indented (no trailing whitespace).
        assert_eq!(l, vec!["    a", "      b", ""]);
        outdent(&mut l, 0, 2);
        assert_eq!(l, vec!["a", "  b", ""]);
    }

    #[test]
    fn outdent_handles_tab_and_short() {
        let mut tab = to_lines("\tx");
        outdent(&mut tab, 0, 0);
        assert_eq!(tab, vec!["x"]);
        let mut short = to_lines("  y");
        outdent(&mut short, 0, 0); // 2 spaces < 4
        assert_eq!(short, vec!["y"]);
    }

    #[test]
    fn comment_toggle() {
        let mut l = to_lines("  a\n  b");
        toggle_comment(&mut l, 0, 1, "// ");
        assert_eq!(l, vec!["  // a", "  // b"]);
        toggle_comment(&mut l, 0, 1, "// ");
        assert_eq!(l, vec!["  a", "  b"]);
    }

    #[test]
    fn line_ops() {
        let mut up = to_lines("a\nb\nc");
        move_up(&mut up, 2);
        assert_eq!(up, vec!["a", "c", "b"]);
        let mut down = to_lines("a\nb\nc");
        move_down(&mut down, 0);
        assert_eq!(down, vec!["b", "a", "c"]);
        let mut dup = to_lines("a\nb\nc");
        duplicate(&mut dup, 1, 1);
        assert_eq!(dup, vec!["a", "b", "b", "c"]);
    }

    #[test]
    fn find_wraps_and_cases() {
        let t = "abc ABC abc";
        assert_eq!(find(t, "abc", 0, true, true), Some((0, 3)));
        assert_eq!(find(t, "abc", 1, true, true), Some((8, 11)));
        assert_eq!(find(t, "abc", 9, true, true), Some((0, 3))); // wrap
        assert_eq!(find(t, "abc", 0, true, false), Some((0, 3)));
        assert_eq!(find(t, "ABC", 1, false, true), Some((4, 7)));
        assert_eq!(find(t, "zzz", 0, true, true), None);
    }
}
