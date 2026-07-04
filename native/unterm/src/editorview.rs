//! Code-editor view: a single id-handled editing surface — an [`InputBox`] in
//! code-editor mode (tree-sitter highlighting + line-number gutter, no Send
//! button) — that the Unity side blits and drives over the FFI. The file path
//! and dirty state live on the C# side; this owns only the surface, its language,
//! and the cached strings handed back across the boundary.

use std::ffi::CString;
use std::os::raw::c_void;

use crate::input::InputBox;

/// Strip interior NULs so the text round-trips through a C string.
fn clean(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

pub struct EditorView {
    edit: InputBox,
    text_snap: CString,
    copy_snap: CString,
    cut_snap: CString,
    word_snap: CString,
}

impl EditorView {
    pub fn new(width: u32, height: u32, scale: f32) -> Self {
        let mut edit = InputBox::new(width, height);
        edit.set_scale(scale);
        edit.set_button(0); // editor has no Send/Stop button
        edit.set_gutter(true);
        edit.set_code_mode(true); // no-wrap, auto-indent, auto-close, etc.
        Self {
            edit,
            text_snap: CString::default(),
            copy_snap: CString::default(),
            cut_snap: CString::default(),
            word_snap: CString::default(),
        }
    }

    pub fn resize(&mut self, w: u32, h: u32, scale: f32) {
        self.edit.set_scale(scale);
        self.edit.resize(w, h);
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.edit.set_scale(scale);
    }

    pub fn set_undo_limit(&mut self, limit: usize) {
        self.edit.set_undo_limit(limit);
    }

    pub fn set_font(&mut self, path: &str) {
        self.edit.set_font(path);
    }

    /// Background rgba + foreground rgb, plus the syntect-vs-dark highlight theme.
    pub fn set_theme(&mut self, br: f64, bg: f64, bb: f64, ba: f64, fr: u8, fg: u8, fb: u8, dark: bool) {
        self.edit.set_clear_color(br, bg, bb, ba);
        self.edit.set_text_color(fr, fg, fb, 255);
        self.edit.set_dark(dark);
    }

    /// Tree-sitter language token (e.g. "cs"); empty = plain.
    pub fn set_language(&mut self, token: &str) {
        let t = token.trim();
        self.edit.set_language(if t.is_empty() { None } else { Some(t) });
    }

    pub fn render(&mut self) {
        self.edit.render();
    }

    pub fn raw_texture(&self) -> *mut c_void {
        self.edit.raw_texture()
    }

    pub fn content_height(&self) -> f32 {
        self.edit.content_height()
    }

    pub fn edit_serial(&self) -> u64 {
        self.edit.edit_serial()
    }

    pub fn caret_rect(&self) -> [f32; 4] {
        self.edit.caret_rect()
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.edit.set_focused(focused);
    }

    pub fn key(&mut self, name: &str, ctrl: bool, alt: bool, shift: bool) {
        self.edit.key(name, ctrl, alt, shift);
    }

    pub fn insert(&mut self, text: &str) {
        self.edit.insert(text);
    }

    pub fn set_preedit(&mut self, text: &str) {
        self.edit.set_preedit(text);
    }

    pub fn set_text(&mut self, text: &str) {
        self.edit.set_text(text);
    }

    pub fn undo(&mut self) {
        self.edit.undo();
    }

    pub fn redo(&mut self) {
        self.edit.redo();
    }

    pub fn select_all(&mut self) {
        self.edit.select_all();
    }

    pub fn mouse(&mut self, x: f32, y: f32, kind: u8) {
        self.edit.mouse(x, y, kind);
    }

    pub fn scroll(&mut self, dy: f32) {
        self.edit.scroll_by(dy);
    }

    pub fn scroll_h(&mut self, dx: f32) {
        self.edit.scroll_h_by(dx);
    }

    pub fn set_scroll(&mut self, px: f32) {
        self.edit.set_scroll_v(px);
    }

    pub fn scroll_offset(&self) -> f32 {
        self.edit.scroll_offset()
    }

    pub fn indent(&mut self) {
        self.edit.indent();
    }

    pub fn outdent(&mut self) {
        self.edit.outdent();
    }

    pub fn toggle_comment(&mut self) {
        self.edit.toggle_comment();
    }

    pub fn move_line_up(&mut self) {
        self.edit.move_line_up();
    }

    pub fn move_line_down(&mut self) {
        self.edit.move_line_down();
    }

    pub fn duplicate_line(&mut self) {
        self.edit.duplicate_line();
    }

    pub fn delete_line(&mut self) {
        self.edit.delete_line();
    }

    pub fn goto_line(&mut self, line: usize) {
        self.edit.goto_line(line);
    }

    pub fn add_using(&mut self, ns: &str) {
        self.edit.add_using(ns);
    }

    pub fn find(&mut self, query: &str, forward: bool, case_sensitive: bool) -> bool {
        self.edit.find(query, forward, case_sensitive)
    }

    pub fn replace_selection(&mut self, repl: &str) {
        self.edit.replace_selection(repl);
    }

    pub fn replace_all(&mut self, query: &str, repl: &str, case_sensitive: bool) -> u32 {
        self.edit.replace_all(query, repl, case_sensitive)
    }

    pub fn caret_offset(&self) -> usize {
        self.edit.caret_offset()
    }

    pub fn word_prefix(&mut self) -> &CString {
        self.word_snap = clean(&self.edit.word_prefix());
        &self.word_snap
    }

    pub fn complete(&mut self, prefix_len: usize, text: &str) {
        self.edit.complete(prefix_len, text);
    }

    /// Set the autocomplete popup items ('\n'-joined; empty hides it) + selection.
    pub fn set_completions(&mut self, joined: &str, selected: usize) {
        let items: Vec<String> = if joined.is_empty() {
            Vec::new()
        } else {
            joined.split('\n').map(|s| s.to_string()).collect()
        };
        self.edit.set_completions(items, selected);
    }

    pub fn text(&mut self) -> &CString {
        self.text_snap = clean(&self.edit.text());
        &self.text_snap
    }

    pub fn copy(&mut self) -> &CString {
        self.copy_snap = clean(&self.edit.copy().unwrap_or_default());
        &self.copy_snap
    }

    pub fn cut(&mut self) -> &CString {
        self.cut_snap = clean(&self.edit.cut().unwrap_or_default());
        &self.cut_snap
    }
}
