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
    // Markdown (tree-sitter-md block + inline highlight queries).
    "text.title",     // headings
    "text.literal",   // code spans / fenced code
    "text.emphasis",  // *italic*
    "text.strong",    // **bold**
    "text.uri",       // links / autolinks
    "text.reference", // link labels / references
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
    (224, 108, 117), // text.title (heading) — coral
    (152, 195, 121), // text.literal (code) — green
    (198, 120, 221), // text.emphasis (italic) — purple
    (229, 192, 123), // text.strong (bold) — gold
    (97, 175, 239),  // text.uri (link) — blue
    (86, 182, 194),  // text.reference — cyan
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
    (207, 34, 46),  // text.title (heading) — red
    (17, 99, 41),   // text.literal (code) — green
    (130, 80, 223), // text.emphasis (italic) — purple
    (149, 56, 0),   // text.strong (bold) — brown
    (5, 80, 174),   // text.uri (link) — blue
    (5, 80, 174),   // text.reference — blue
];

fn color_for(index: usize, dark: bool) -> Color {
    let table = if dark { DARK } else { LIGHT };
    let (r, g, b) = table.get(index).copied().unwrap_or((200, 200, 200));
    Color::rgb(r, g, b)
}

/// The theme color for a highlight name (e.g. "function", "type", "property"),
/// so other UI (the completion popup) can color tokens like the editor does.
pub fn color_of(name: &str, dark: bool) -> Color {
    match HL_NAMES.iter().position(|&n| n == name) {
        Some(i) => color_for(i, dark),
        None => Color::rgb(200, 200, 200),
    }
}

/// A grammar plus its compiled highlights query, built once and cached for the
/// process. `cap_color` maps each query capture index to a color-table index.
pub(crate) struct LangConfig {
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

/// Markdown needs two grammars: a block grammar (headings, fenced code, lists,
/// tables) and an inline grammar (emphasis, code spans, links) parsed over each
/// block's inline ranges. Each carries its own compiled query + capture→color map.
pub(crate) struct MdConfig {
    block_lang: Language,
    inline_lang: Language,
    block_q: Query,
    block_cap: Vec<Option<usize>>,
    inline_q: Query,
    inline_cap: Vec<Option<usize>>,
}

/// The process-cached [`MdConfig`], or None if either grammar's query fails to
/// compile (the caller then has no highlighter and falls back to uniform color).
fn md_config() -> Option<&'static MdConfig> {
    static CFG: OnceLock<Option<MdConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let block_lang: Language = tree_sitter_md::LANGUAGE.into();
        let inline_lang: Language = tree_sitter_md::INLINE_LANGUAGE.into();
        let block_q = Query::new(&block_lang, tree_sitter_md::HIGHLIGHT_QUERY_BLOCK)
            .map_err(|e| log::warn!("unterm: md block query failed: {e}"))
            .ok()?;
        let inline_q = Query::new(&inline_lang, tree_sitter_md::HIGHLIGHT_QUERY_INLINE)
            .map_err(|e| log::warn!("unterm: md inline query failed: {e}"))
            .ok()?;
        let block_cap = block_q.capture_names().iter().map(|n| name_to_index(n)).collect();
        let inline_cap = inline_q.capture_names().iter().map(|n| name_to_index(n)).collect();
        Some(MdConfig { block_lang, inline_lang, block_q, block_cap, inline_q, inline_cap })
    })
    .as_ref()
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

/// Stateful incremental highlighter for one editor buffer. Single-grammar
/// languages (C#) parse one tree; Markdown parses a block tree plus one inline
/// tree per inline node. Either way successive [`Highlighter::highlight`] calls
/// reparse only the region that changed (the previous tree is edited by the byte
/// delta and handed back to the parser).
pub(crate) enum Highlighter {
    Ts {
        cfg: &'static LangConfig,
        parser: Parser,
        tree: Option<Tree>,
        prev: String,
    },
    Md {
        cfg: &'static MdConfig,
        /// Parses the document's block structure (incrementally, like `Ts`).
        block_parser: Parser,
        /// Re-set to each inline node's byte range per call and reparsed fresh.
        inline_parser: Parser,
        block_tree: Option<Tree>,
        prev: String,
    },
}

impl Highlighter {
    /// Build a highlighter for `lang_id`, or None if we have no grammar for it.
    pub fn new(lang_id: &str) -> Option<Highlighter> {
        if matches!(lang_id, "md" | "markdown") {
            let cfg = md_config()?;
            let mut block_parser = Parser::new();
            block_parser
                .set_language(&cfg.block_lang)
                .map_err(|e| log::warn!("unterm: md block set_language failed: {e}"))
                .ok()?;
            let mut inline_parser = Parser::new();
            inline_parser
                .set_language(&cfg.inline_lang)
                .map_err(|e| log::warn!("unterm: md inline set_language failed: {e}"))
                .ok()?;
            return Some(Highlighter::Md {
                cfg,
                block_parser,
                inline_parser,
                block_tree: None,
                prev: String::new(),
            });
        }
        let cfg = lang_config(lang_id)?;
        let mut parser = Parser::new();
        parser
            .set_language(&cfg.language)
            .map_err(|e| log::warn!("unterm: set_language failed: {e}"))
            .ok()?;
        Some(Highlighter::Ts { cfg, parser, tree: None, prev: String::new() })
    }

    /// (Re)highlight `text` into per-logical-line colored spans (split on `\n`,
    /// aligned 1:1 with the lines `cosmic_text` holds). Reuses the previous parse
    /// tree via an incremental edit when the text changed since the last call.
    pub fn highlight(&mut self, text: &str, dark: bool) -> Vec<LineSpans> {
        match self {
            Highlighter::Ts { cfg, parser, tree, prev } => {
                if let Some(t) = tree.as_mut() {
                    if let Some(edit) = text_edit(prev, text) {
                        t.edit(&edit);
                    }
                }
                *tree = parser.parse(text, tree.as_ref());
                prev.clear();
                prev.push_str(text);
                let line_start = build_line_starts(text);
                let mut out = empty_line_spans(line_start.len());
                if let Some(t) = tree.as_ref() {
                    run_query_into(&cfg.query, &cfg.cap_color, t, text, &line_start, dark, &mut out);
                }
                out
            }
            Highlighter::Md { cfg, block_parser, inline_parser, block_tree, prev } => {
                if let Some(t) = block_tree.as_mut() {
                    if let Some(edit) = text_edit(prev, text) {
                        t.edit(&edit);
                    }
                }
                *block_tree = block_parser.parse(text, block_tree.as_ref());
                prev.clear();
                prev.push_str(text);
                let line_start = build_line_starts(text);
                let mut out = empty_line_spans(line_start.len());
                if let Some(bt) = block_tree.as_ref() {
                    // Block structure first (headings, fenced code, list markers, link
                    // labels), then the inline grammar over each `inline` node's byte
                    // range so the more specific inline spans (emphasis, code, links)
                    // win any overlap. Inline nodes carry raw text the block grammar
                    // leaves unparsed; we reparse each range via included-ranges (the
                    // resulting node offsets stay in document coordinates).
                    run_query_into(&cfg.block_q, &cfg.block_cap, bt, text, &line_start, dark, &mut out);
                    let mut ranges = Vec::new();
                    collect_inline_ranges(bt.root_node(), &mut ranges);
                    for r in ranges {
                        if inline_parser.set_included_ranges(&[r]).is_err() {
                            continue;
                        }
                        if let Some(it) = inline_parser.parse(text, None) {
                            run_query_into(&cfg.inline_q, &cfg.inline_cap, &it, text, &line_start, dark, &mut out);
                        }
                    }
                }
                out
            }
        }
    }
}

/// Depth-first collect the byte ranges of every `inline` node in a Markdown block
/// tree. An `inline` node is a leaf of the block grammar carrying raw text that
/// the inline grammar re-parses (emphasis, code spans, links), so recursion stops
/// at one.
fn collect_inline_ranges(node: tree_sitter::Node, out: &mut Vec<tree_sitter::Range>) {
    if node.kind() == "inline" {
        out.push(node.range());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_inline_ranges(child, out);
    }
}

/// Byte offset of each logical line start (line 0 begins at 0); the implicit end
/// of the last line is `text.len()`.
fn build_line_starts(text: &str) -> Vec<usize> {
    let mut line_start = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_start.push(i + 1);
        }
    }
    line_start
}

/// One empty [`LineSpans`] per logical line.
fn empty_line_spans(n_lines: usize) -> Vec<LineSpans> {
    (0..n_lines).map(|_| LineSpans { spans: Vec::new() }).collect()
}

/// Run `query` over `tree` and clip each capture onto the logical lines it
/// touches, appending into `out` (pre-sized to the line count). Later captures
/// (more specific patterns, and later trees) overwrite earlier ones in any
/// overlap, since the caller applies the spans in order.
fn run_query_into(
    query: &Query,
    cap_color: &[Option<usize>],
    tree: &Tree,
    text: &str,
    line_start: &[usize],
    dark: bool,
    out: &mut [LineSpans],
) {
    let n_lines = line_start.len();
    let content_end = |line: usize| -> usize {
        if line + 1 < n_lines {
            line_start[line + 1] - 1
        } else {
            text.len()
        }
    };

    let mut cursor = QueryCursor::new();
    let mut caps = cursor.captures(query, tree.root_node(), text.as_bytes());
    while let Some((m, ci)) = caps.next() {
        let cap = m.captures[*ci];
        let Some(Some(idx)) = cap_color.get(cap.index as usize) else { continue };
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

    #[test]
    fn markdown_highlights_heading_and_inline() {
        // Heading (block grammar) on line 0; **bold** + `code` (inline grammar) on
        // line 2 — exercises the block+inline coordination.
        let src = "# Title\n\nSome **bold** and `code` here.\n";
        let mut hl = Highlighter::new("md").expect("markdown should highlight");
        let lines = hl.highlight(src, true);
        assert_well_formed(src, &lines);
        // The heading line gets colored spans (the `# ` marker and/or the title text).
        assert!(!lines[0].spans.is_empty(), "heading line had no spans");
        // The inline line gets spans from the inline grammar (bold + code span).
        assert!(lines[2].spans.len() >= 2, "inline spans: {}", lines[2].spans.len());
        // An incremental edit must match a from-scratch parse.
        let src2 = "# Title!\n\nSome **bold** and `code` here.\n";
        let inc = hl.highlight(src2, true);
        let fresh = Highlighter::new("md").unwrap().highlight(src2, true);
        assert_eq!(inc.len(), fresh.len());
        for (a, f) in inc.iter().zip(fresh.iter()) {
            assert_eq!(a.spans.len(), f.spans.len(), "line span-count mismatch after edit");
        }
    }
}
