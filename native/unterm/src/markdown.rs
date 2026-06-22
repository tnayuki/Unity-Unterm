//! Minimal Markdown → styled-block model for the agent panel. Parsing is done by
//! `pulldown-cmark`; the actual rendering (glyphon) lives in [`crate::panel`].
//!
//! We only model what a chat transcript needs: paragraphs, headings, fenced/
//! indented code blocks (flagged when the language is `diff`), list items,
//! block quotes, and horizontal rules, with inline bold / italic / code / link
//! styling carried per text span.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// A run of text with uniform inline styling.
#[derive(Clone, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
}

/// A block-level element.
pub enum Block {
    Paragraph(Vec<Span>),
    Heading { level: u8, spans: Vec<Span> },
    Code {
        text: String,
        lang: Option<String>,
        diff: bool,
    },
    ListItem { depth: u8, marker: String, spans: Vec<Span> },
    Quote(Vec<Span>),
    /// A table: a header row then body rows; each cell is a run of spans.
    Table {
        headers: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
}

/// Parse Markdown into a flat list of styled blocks.
pub fn parse(md: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();

    // Inline style nesting counts (so `**a _b_**` keeps both).
    let mut bold = 0u32;
    let mut italic = 0u32;
    let mut link = 0u32;

    // List nesting: (ordered, next number) per level; plus the current item marker.
    let mut lists: Vec<(bool, u64)> = Vec::new();
    let mut marker = String::new();

    let mut heading: Option<u8> = None;
    let mut quote = 0u32;

    let mut in_code = false;
    let mut code_text = String::new();
    let mut code_lang: Option<String> = None;
    let mut code_diff = false;

    // GFM tables: collected as rows of styled-span cells (header row first).
    let mut in_cell = false;
    let mut table_rows: Vec<Vec<Vec<Span>>> = Vec::new();
    let mut cur_row: Vec<Vec<Span>> = Vec::new();
    let mut cur_cell: Vec<Span> = Vec::new();

    let push_span = |spans: &mut Vec<Span>, text: String, b: u32, i: u32, c: bool, l: u32| {
        if text.is_empty() {
            return;
        }
        spans.push(Span {
            text,
            bold: b > 0,
            italic: i > 0,
            code: c,
            link: l > 0,
        });
    };

    for ev in Parser::new_ext(md, Options::ENABLE_TABLES) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Strong => bold += 1,
                Tag::Emphasis => italic += 1,
                Tag::Link { .. } => link += 1,
                Tag::Heading { level, .. } => {
                    heading = Some(level as u8);
                    spans.clear();
                }
                Tag::CodeBlock(kind) => {
                    in_code = true;
                    code_text.clear();
                    code_lang = match &kind {
                        CodeBlockKind::Fenced(l) => {
                            let t = l.split_whitespace().next().unwrap_or("").to_lowercase();
                            (!t.is_empty()).then_some(t)
                        }
                        CodeBlockKind::Indented => None,
                    };
                    code_diff = matches!(&code_lang, Some(l) if l == "diff");
                }
                Tag::List(start) => lists.push((start.is_some(), start.unwrap_or(1))),
                Tag::Item => {
                    spans.clear();
                    marker = match lists.last_mut() {
                        Some((true, n)) => {
                            let m = format!("{n}.");
                            *n += 1;
                            m
                        }
                        _ => "•".to_string(),
                    };
                }
                Tag::BlockQuote(_) => {
                    quote += 1;
                    spans.clear();
                }
                Tag::Table(_) => {
                    table_rows.clear();
                }
                Tag::TableHead | Tag::TableRow => cur_row.clear(),
                Tag::TableCell => {
                    in_cell = true;
                    cur_cell = Vec::new();
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Strong => bold = bold.saturating_sub(1),
                TagEnd::Emphasis => italic = italic.saturating_sub(1),
                TagEnd::Link => link = link.saturating_sub(1),
                TagEnd::Heading(_) => out.push(Block::Heading {
                    level: heading.take().unwrap_or(1),
                    spans: std::mem::take(&mut spans),
                }),
                TagEnd::CodeBlock => {
                    in_code = false;
                    let text = code_text.trim_end_matches('\n').to_string();
                    out.push(Block::Code {
                        text,
                        lang: code_lang.take(),
                        diff: code_diff,
                    });
                    code_text.clear();
                }
                TagEnd::Item => out.push(Block::ListItem {
                    depth: lists.len().max(1) as u8,
                    marker: std::mem::take(&mut marker),
                    spans: std::mem::take(&mut spans),
                }),
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::BlockQuote(_) => {
                    quote = quote.saturating_sub(1);
                    out.push(Block::Quote(std::mem::take(&mut spans)));
                }
                TagEnd::TableCell => {
                    in_cell = false;
                    cur_row.push(std::mem::take(&mut cur_cell));
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    table_rows.push(std::mem::take(&mut cur_row));
                }
                TagEnd::Table => {
                    let mut all = std::mem::take(&mut table_rows);
                    if !all.is_empty() {
                        let headers = all.remove(0);
                        out.push(Block::Table { headers, rows: all });
                    }
                }
                TagEnd::Paragraph => {
                    // A paragraph inside a list item or quote keeps its spans for
                    // that container's End to flush; a top-level one is its own block.
                    if heading.is_none() && lists.is_empty() && quote == 0 {
                        out.push(Block::Paragraph(std::mem::take(&mut spans)));
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_cell {
                    push_span(&mut cur_cell, t.to_string(), bold, italic, false, link);
                } else if in_code {
                    code_text.push_str(&t);
                } else {
                    push_span(&mut spans, t.to_string(), bold, italic, false, link);
                }
            }
            Event::Code(t) => {
                if in_cell {
                    push_span(&mut cur_cell, t.to_string(), bold, italic, true, link);
                } else if !in_code {
                    push_span(&mut spans, t.to_string(), bold, italic, true, link);
                }
            }
            Event::SoftBreak => {
                if in_cell {
                    push_span(&mut cur_cell, " ".to_string(), bold, italic, false, link);
                } else if in_code {
                    code_text.push('\n');
                } else {
                    push_span(&mut spans, " ".to_string(), bold, italic, false, link);
                }
            }
            Event::HardBreak => {
                if in_cell {
                    push_span(&mut cur_cell, " ".to_string(), bold, italic, false, link);
                } else if in_code {
                    code_text.push('\n');
                } else {
                    push_span(&mut spans, "\n".to_string(), bold, italic, false, link);
                }
            }
            Event::Rule => out.push(Block::Rule),
            _ => {}
        }
    }

    out
}

