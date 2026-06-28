//! Tree-sitter syntax highlighting for the code editor surface.
//!
//! The agent panel keeps using syntect for Markdown code fences (arbitrary
//! languages); this module is editor-only and covers the few languages we edit.
//! It turns source text into per-logical-line colored spans that
//! [`crate::input::InputBox`] applies to each `cosmic_text` buffer line via an
//! `AttrsList`. Each editor buffer keeps a [`Highlighter`] that reparses
//! INCREMENTALLY: the previous syntax tree is edited by the byte delta since the
//! last call and handed back to the parser, so a keystroke re-parses only the
//! changed region instead of the whole file.

use std::ops::Range;
use std::sync::OnceLock;

use glyphon::Color;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, Tree};

/// One highlighted logical line: byte-range spans into that line's text. Ranges
/// are relative to the line start and never include the trailing `\n`.
pub struct LineSpans {
    pub spans: Vec<(Range<usize>, Color)>,
}

/// Highlight capture names we recognize. `Highlight(i)` from tree-sitter indexes
/// into this slice (and the parallel color tables below). Capture names in a
/// grammar's `highlights.scm` match by dotted-prefix, so `keyword.control` maps
/// to `keyword` here.
const HL_NAMES: &[&str] = &[
    "keyword",
    "function",
    "function.method",
    "type",
    "string",
    "comment",
    "number",
    "constant",
    "constant.builtin",
    "property",
    "variable",
    "operator",
    "punctuation",
    "attribute",
    "constructor",
    "namespace",
    "label",
    "escape",
];

/// Foreground colors for the dark theme, parallel to [`HL_NAMES`] (One Dark-ish).
const DARK: &[(u8, u8, u8)] = &[
    (198, 120, 221), // keyword
    (97, 175, 239),  // function
    (97, 175, 239),  // function.method
    (229, 192, 123), // type
    (152, 195, 121), // string
    (92, 99, 112),   // comment
    (209, 154, 102), // number
    (209, 154, 102), // constant
    (209, 154, 102), // constant.builtin
    (224, 108, 117), // property
    (224, 108, 117), // variable
    (86, 182, 194),  // operator
    (171, 178, 191), // punctuation
    (229, 192, 123), // attribute
    (229, 192, 123), // constructor
    (229, 192, 123), // namespace
    (97, 175, 239),  // label
    (86, 182, 194),  // escape
];

/// Foreground colors for the light theme, parallel to [`HL_NAMES`] (GitHub-ish).
const LIGHT: &[(u8, u8, u8)] = &[
    (207, 34, 46),  // keyword
    (130, 80, 223), // function
    (130, 80, 223), // function.method
    (149, 56, 0),   // type
    (10, 48, 105),  // string
    (110, 119, 129), // comment
    (5, 80, 174),   // number
    (5, 80, 174),   // constant
    (5, 80, 174),   // constant.builtin
    (17, 99, 41),   // property
    (31, 35, 40),   // variable
    (5, 80, 174),   // operator
    (31, 35, 40),   // punctuation
    (17, 99, 41),   // attribute
    (149, 56, 0),   // constructor
    (36, 41, 47),   // namespace
    (130, 80, 223), // label
    (5, 80, 174),   // escape
];

fn color_for(index: usize, dark: bool) -> Color {
    let table = if dark { DARK } else { LIGHT };
    let (r, g, b) = table.get(index).copied().unwrap_or((200, 200, 200));
    Color::rgb(r, g, b)
}

/// A grammar plus its compiled highlights query, built once and cached for the
/// process. `cap_color` maps each query capture index to a color-table index.
struct LangConfig {
    language: Language,
    query: Query,
    cap_color: Vec<Option<usize>>,
}

/// The [`LangConfig`] for `lang_id`, or None if we have no grammar for it (the
/// caller falls back to uniform color).
fn lang_config(lang_id: &str) -> Option<&'static LangConfig> {
    match lang_id {
        "cs" | "c_sharp" | "csharp" => {
            static CFG: OnceLock<Option<LangConfig>> = OnceLock::new();
            CFG.get_or_init(|| {
                build_config(
                    tree_sitter_c_sharp::LANGUAGE.into(),
                    tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
                )
            })
            .as_ref()
        }
        _ => None,
    }
}

fn build_config(language: Language, highlights: &str) -> Option<LangConfig> {
    let query = Query::new(&language, highlights)
        .map_err(|e| log::warn!("unterm: highlight query failed: {e}"))
        .ok()?;
    let cap_color = query.capture_names().iter().map(|n| name_to_index(n)).collect();
    Some(LangConfig { language, query, cap_color })
}

/// Map a query capture name to a color-table index by longest dotted-prefix match
/// against [`HL_NAMES`] (so "keyword.control" → "keyword", "function.method" →
/// "function.method"). None for names we don't color.
fn name_to_index(name: &str) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, &n) in HL_NAMES.iter().enumerate() {
        let hit = name == n || name.strip_prefix(n).is_some_and(|r| r.starts_with('.'));
        if hit && best.is_none_or(|(_, bl)| n.len() > bl) {
            best = Some((i, n.len()));
        }
    }
    best.map(|(i, _)| i)
}

/// Stateful incremental highlighter for one editor buffer. Holds the parser and
/// last syntax tree so successive [`Highlighter::highlight`] calls reparse only
/// the region that changed.
pub struct Highlighter {
    cfg: &'static LangConfig,
    parser: Parser,
    tree: Option<Tree>,
    prev: String,
}

impl Highlighter {
    /// Build a highlighter for `lang_id`, or None if we have no grammar for it.
    pub fn new(lang_id: &str) -> Option<Highlighter> {
        let cfg = lang_config(lang_id)?;
        let mut parser = Parser::new();
        parser
            .set_language(&cfg.language)
            .map_err(|e| log::warn!("unterm: set_language failed: {e}"))
            .ok()?;
        Some(Highlighter { cfg, parser, tree: None, prev: String::new() })
    }

    /// (Re)highlight `text` into per-logical-line colored spans (split on `\n`,
    /// aligned 1:1 with the lines `cosmic_text` holds). Reuses the previous parse
    /// tree via an incremental edit when the text changed since the last call.
    pub fn highlight(&mut self, text: &str, dark: bool) -> Vec<LineSpans> {
        if let Some(tree) = self.tree.as_mut() {
            if let Some(edit) = text_edit(&self.prev, text) {
                tree.edit(&edit);
            }
        }
        self.tree = self.parser.parse(text, self.tree.as_ref());
        self.prev.clear();
        self.prev.push_str(text);
        match self.tree.as_ref() {
            Some(tree) => spans_from_tree(self.cfg, tree, text, dark),
            None => Vec::new(),
        }
    }
}

/// Run the highlights query over `tree` and clip each capture onto the logical
/// lines it touches. Later captures (more specific patterns) overwrite earlier
/// ones in any overlap (the caller applies the spans in order).
fn spans_from_tree(cfg: &LangConfig, tree: &Tree, text: &str, dark: bool) -> Vec<LineSpans> {
    // Byte offset of each logical line start (and an implicit end = text.len()).
    let mut line_start = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_start.push(i + 1);
        }
    }
    let n_lines = line_start.len();
    let mut out: Vec<LineSpans> = (0..n_lines).map(|_| LineSpans { spans: Vec::new() }).collect();
    let content_end = |line: usize| -> usize {
        if line + 1 < n_lines {
            line_start[line + 1] - 1
        } else {
            text.len()
        }
    };

    let mut cursor = QueryCursor::new();
    let mut caps = cursor.captures(&cfg.query, tree.root_node(), text.as_bytes());
    while let Some((m, ci)) = caps.next() {
        let cap = m.captures[*ci];
        let Some(Some(idx)) = cfg.cap_color.get(cap.index as usize) else { continue };
        let color = color_for(*idx, dark);
        let (start, end) = (cap.node.start_byte(), cap.node.end_byte());
        let mut line = match line_start.binary_search(&start) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        };
        while line < n_lines && line_start[line] < end {
            let ls = line_start[line];
            let ce = content_end(line);
            let s = start.max(ls);
            let e = end.min(ce);
            if s < e {
                out[line].spans.push((s - ls..e - ls, color));
            }
            line += 1;
        }
    }
    out
}

/// A single [`InputEdit`] describing the net change between `old` and `new` as a
/// common-prefix/suffix diff (None if identical). Byte offsets are snapped to
/// char boundaries so tree-sitter never sees a split codepoint.
fn text_edit(old: &str, new: &str) -> Option<InputEdit> {
    if old == new {
        return None;
    }
    let (ob, nb) = (old.as_bytes(), new.as_bytes());
    let max = ob.len().min(nb.len());
    let mut start = 0;
    while start < max && ob[start] == nb[start] {
        start += 1;
    }
    while start > 0 && !old.is_char_boundary(start) {
        start -= 1;
    }
    let mut oe = ob.len();
    let mut ne = nb.len();
    while oe > start && ne > start && ob[oe - 1] == nb[ne - 1] {
        oe -= 1;
        ne -= 1;
    }
    while oe < ob.len() && !old.is_char_boundary(oe) {
        oe += 1;
    }
    while ne < nb.len() && !new.is_char_boundary(ne) {
        ne += 1;
    }
    Some(InputEdit {
        start_byte: start,
        old_end_byte: oe,
        new_end_byte: ne,
        start_position: point_at(old, start),
        old_end_position: point_at(old, oe),
        new_end_position: point_at(new, ne),
    })
}

/// Tree-sitter [`Point`] (row, byte-column) at byte offset `byte` in `s`.
fn point_at(s: &str, byte: usize) -> Point {
    let pre = &s.as_bytes()[..byte];
    let row = pre.iter().filter(|&&b| b == b'\n').count();
    let col = byte - pre.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    Point::new(row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_well_formed(src: &str, lines: &[LineSpans]) {
        let line_lens: Vec<usize> = src.split('\n').map(|l| l.len()).collect();
        assert_eq!(lines.len(), line_lens.len());
        for (i, ls) in lines.iter().enumerate() {
            for (r, _) in &ls.spans {
                assert!(r.end <= line_lens[i], "line {i} span {:?} > {}", r, line_lens[i]);
                assert!(r.start < r.end);
            }
        }
    }

    #[test]
    fn csharp_highlights_keywords_and_strings() {
        let src = "public class Foo {\n    void Bar() { var s = \"hi\"; }\n}\n";
        let mut hl = Highlighter::new("cs").expect("c# should highlight");
        let lines = hl.highlight(src, true);
        // The first line ("public class Foo {") must get at least a couple of
        // colored spans (keyword `public`/`class`, type `Foo`).
        assert!(lines[0].spans.len() >= 2, "spans: {:?}", lines[0].spans.len());
        assert_well_formed(src, &lines);
    }

    #[test]
    fn incremental_reparse_matches_fresh() {
        let src0 = "class Foo {\n    int x;\n}\n";
        let src1 = "class Foo {\n    int xy;\n}\n"; // edit line 1
        let mut inc = Highlighter::new("cs").unwrap();
        let _ = inc.highlight(src0, true); // seed the tree
        let after_edit = inc.highlight(src1, true); // incremental reparse
        let fresh = Highlighter::new("cs").unwrap().highlight(src1, true);
        // Incremental reparse must produce the same spans as a from-scratch parse.
        assert_eq!(after_edit.len(), fresh.len());
        for (a, f) in after_edit.iter().zip(fresh.iter()) {
            assert_eq!(a.spans.len(), f.spans.len());
            for ((ar, _), (fr, _)) in a.spans.iter().zip(f.spans.iter()) {
                assert_eq!(ar, fr);
            }
        }
        assert_well_formed(src1, &after_edit);
    }

    #[test]
    fn unknown_language_is_none() {
        assert!(Highlighter::new("no-such-lang").is_none());
    }
}
