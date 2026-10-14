//! The built-in editor overlay: key handling and drawing around [`EditBuf`].
use crate::editbuf::EditBuf;
use crate::highlight;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use std::io::Write;
use unicode_width::UnicodeWidthChar;

const INDENT: &str = "  ";

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode { Edit, Find, Goto, ConfirmClose }

pub struct EditSession {
    pub note_id: String,
    pub title: String,
    pub mime: String,
    pub buf: EditBuf,
    mode: Mode,
    input: String,
    last_find: String,
    clip: String,
    pub msg: String,
    /// what the database held when the editor opened or last saved; a different value at save time means a sync (or another client) changed the note meanwhile
    pub base: String,
    force: bool,
    rows: usize,
    cols: usize,
    followed: (usize, usize, usize, usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome { None, Save, Close, SaveAndClose, ToRich }

pub(crate) fn system_copy(text: &str) {
    for (prog, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        if let Ok(mut c) = std::process::Command::new(prog).args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            if let Some(mut si) = c.stdin.take() { let _ = si.write_all(text.as_bytes()); }
            // wl-copy forks to keep serving the selection; don't wait on it forever
            std::thread::spawn(move || { let _ = c.wait(); });
            return;
        }
    }
}

pub(crate) fn system_paste() -> Option<String> {
    for (prog, args) in [("wl-paste", &["-n"][..]), ("xclip", &["-selection", "clipboard", "-o"][..])] {
        if let Ok(o) = std::process::Command::new(prog).args(args).stderr(std::process::Stdio::null()).output() {
            if o.status.success() { return String::from_utf8(o.stdout).ok(); }
        }
    }
    None
}

impl EditSession {
    pub fn new(note_id: &str, title: &str, mime: &str, text: &str) -> Self {
        EditSession { note_id: note_id.into(), title: title.into(), mime: mime.into(), buf: EditBuf::new(text), mode: Mode::Edit, input: String::new(), last_find: String::new(), clip: String::new(), msg: String::new(), base: text.to_string(), force: false, rows: 20, cols: 80, followed: (usize::MAX, 0, 0, 0) }
    }

    /// `stored` is what the database now holds (it normalises some links), the baseline for the next overwrite check.
    pub fn saved(&mut self, stored: String) { self.buf.mark_saved(); self.base = stored; self.force = false; self.msg = "saved".into(); }

    /// True when the stored note no longer matches what this editor started from and the user hasn't yet confirmed overwriting it.
    pub fn needs_overwrite_confirmation(&mut self, stored: &str) -> bool {
        if stored == self.base || self.force { return false; }
        self.force = true;
        self.mode = Mode::Edit;
        self.msg = "⚠ this note changed elsewhere (sync?) since you opened it — Ctrl+S again to overwrite it".into();
        true
    }

    pub fn paste(&mut self, text: &str) {
        match self.mode {
            Mode::Edit => self.buf.insert_str(text),
            Mode::Find | Mode::Goto => self.input.push_str(text.lines().next().unwrap_or("")),
            Mode::ConfirmClose => {}
        }
    }

    pub fn key(&mut self, ev: KeyEvent) -> Outcome {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        // Alt/Super/Meta chords are the window manager's or terminal's business, never text
        let other = ev.modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::META | KeyModifiers::HYPER);
        self.msg.clear();
        match self.mode {
            Mode::ConfirmClose => {
                if ctrl || alt || other { self.mode = Mode::Edit; return Outcome::None; }
                return match ev.code {
                    KeyCode::Char('y') | KeyCode::Char('s') => Outcome::SaveAndClose,
                    KeyCode::Char('d') | KeyCode::Char('n') => Outcome::Close,
                    _ => { self.mode = Mode::Edit; Outcome::None }
                };
            }
            Mode::Find => {
                match ev.code {
                    KeyCode::Esc => self.mode = Mode::Edit,
                    KeyCode::Enter => { self.last_find = self.input.clone(); self.find(); self.mode = Mode::Edit; }
                    KeyCode::Backspace => { self.input.pop(); }
                    KeyCode::Char(c) if !ctrl && !alt && !other => self.input.push(c),
                    _ => {}
                }
                return Outcome::None;
            }
            Mode::Goto => {
                match ev.code {
                    KeyCode::Esc => self.mode = Mode::Edit,
                    KeyCode::Enter => { if let Ok(n) = self.input.trim().parse::<usize>() { self.buf.goto_line(n); } self.mode = Mode::Edit; }
                    KeyCode::Backspace => { self.input.pop(); }
                    KeyCode::Char(c) if c.is_ascii_digit() => self.input.push(c),
                    _ => {}
                }
                return Outcome::None;
            }
            Mode::Edit => {}
        }
        let b = &mut self.buf;
        match (ev.code, ctrl) {
            (KeyCode::Char('s'), true) => return Outcome::Save,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), true) => {
                if b.changed() { self.mode = Mode::ConfirmClose; return Outcome::None; }
                return Outcome::Close;
            }
            (KeyCode::Char('z'), true) if !shift => b.undo(),
            (KeyCode::Char('y'), true) | (KeyCode::Char('z'), true) | (KeyCode::Char('Z'), true) => b.redo(),
            (KeyCode::Char('a'), true) => b.select_all(),
            (KeyCode::Char('c'), true) => self.copy(false),
            (KeyCode::Char('x'), true) => self.copy(true),
            (KeyCode::Char('v'), true) => self.paste_clip(),
            (KeyCode::Insert, _) if shift => self.paste_clip(),
            (KeyCode::Char('f'), true) => { self.mode = Mode::Find; self.input = self.buf.selected_text().filter(|s| !s.contains('\n')).unwrap_or_else(|| self.last_find.clone()); }
            (KeyCode::Char('g'), true) => { self.mode = Mode::Goto; self.input.clear(); }
            (KeyCode::F(3), _) => self.find(),
            (KeyCode::F(2), _) if self.mime == "text/html" => return Outcome::ToRich,
            (KeyCode::Char('k'), true) => b.delete_line(),
            (KeyCode::Char('d'), true) => b.duplicate_line(),
            (KeyCode::Char('w'), true) | (KeyCode::Backspace, true) => b.delete_word_back(),
            (KeyCode::Left, true) => b.word_left(shift),
            (KeyCode::Right, true) => b.word_right(shift),
            (KeyCode::Left, false) if alt => b.word_left(shift),
            (KeyCode::Right, false) if alt => b.word_right(shift),
            (KeyCode::Left, _) => b.left(shift),
            (KeyCode::Right, _) => b.right(shift),
            (KeyCode::Up, _) => b.up(1, shift),
            (KeyCode::Down, _) => b.down(1, shift),
            (KeyCode::PageUp, _) => { let n = self.rows.saturating_sub(1).max(1); self.buf.up(n, shift) }
            (KeyCode::PageDown, _) => { let n = self.rows.saturating_sub(1).max(1); self.buf.down(n, shift) }
            (KeyCode::Home, true) => b.doc_start(shift),
            (KeyCode::End, true) => b.doc_end(shift),
            (KeyCode::Home, _) => b.home(shift),
            (KeyCode::End, _) => b.end(shift),
            (KeyCode::Enter, _) => b.newline(),
            (KeyCode::Backspace, _) => b.backspace(),
            (KeyCode::Delete, _) => b.delete(),
            (KeyCode::BackTab, _) => b.dedent(INDENT),
            (KeyCode::Tab, _) => b.indent(INDENT),
            (KeyCode::Char(c), false) if !alt && !other => b.insert_char(c),
            _ => {}
        }
        Outcome::None
    }

    fn copy(&mut self, cut: bool) {
        if let Some(t) = self.buf.selected_text() {
            system_copy(&t);
            self.clip = t;
            if cut { self.buf.delete_selection(); }
            self.msg = if cut { "cut".into() } else { "copied".into() };
        }
    }

    fn paste_clip(&mut self) {
        let t = system_paste().filter(|t| !t.is_empty()).unwrap_or_else(|| self.clip.clone());
        self.buf.insert_str(&t);
    }

    fn find(&mut self) {
        if self.last_find.is_empty() { self.msg = "nothing to find".into(); return; }
        let q = self.last_find.clone();
        if !self.buf.find_next(&q) { self.msg = format!("'{q}' not found"); }
    }

    /// Click inside the text area: place the cursor.
    pub fn click(&mut self, text_area: Rect, x: u16, y: u16) {
        if y < text_area.y || y >= text_area.y + text_area.height || x < text_area.x { return; }
        let row = (self.buf.scroll + (y - text_area.y) as usize).min(self.buf.lines.len() - 1);
        let gutter = gutter_width(self.buf.lines.len()) as u16;
        let want = (x - text_area.x).saturating_sub(gutter) as usize + self.buf.hscroll;
        let mut col = 0usize;
        let mut acc = 0usize;
        for ch in self.buf.lines[row].chars().take(self.buf.line_len(row)) {
            let w = cell_width(ch);
            if want < acc + w.div_ceil(2) { break; }
            acc += w; col += 1;
        }
        self.buf.anchor = None;
        self.buf.place_cursor((row, col.min(self.buf.line_len(row))));
        self.mode = Mode::Edit;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.buf.lines.len().saturating_sub(1) as isize;
        self.buf.scroll = (self.buf.scroll as isize + delta).clamp(0, max) as usize;
    }
}

fn cell_width(c: char) -> usize { if c == '\t' { 4 } else if c.is_control() { 1 } else { c.width().unwrap_or(0) } }
fn gutter_width(lines: usize) -> usize { lines.to_string().len().max(2) + 2 }

fn display_col(line: &str, col: usize) -> usize { line.chars().take(col).map(cell_width).sum() }

pub fn text_area(area: Rect) -> Rect {
    let inner = Block::bordered().inner(area);
    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas::<2>(inner)[0]
}

pub fn draw(f: &mut Frame, s: &mut EditSession, area: Rect) {
    f.render_widget(Clear, area);
    let accent = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);
    let modified = if s.buf.changed() { Span::styled(" ● ", Style::default().fg(Color::Yellow)) } else { Span::raw(" ") };
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(accent)
        .title(Line::from(vec![Span::styled(" ✎ ", accent), Span::styled(s.title.clone(), Style::default().add_modifier(Modifier::BOLD)), modified]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [text_r, foot_r] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let gw = gutter_width(s.buf.lines.len());
    let cols = (text_r.width as usize).saturating_sub(gw);
    s.rows = text_r.height as usize;
    s.cols = cols;
    let cur_col = display_col(&s.buf.lines[s.buf.cur.0], s.buf.cur.1);
    // keep the cursor in view only when it (or the window) moved, so the mouse wheel can scroll away from it
    let marker = (s.buf.cur.0, cur_col, s.rows, cols);
    if marker != s.followed { s.buf.follow(s.rows, cols, cur_col); s.followed = marker; }
    s.buf.scroll = s.buf.scroll.min(s.buf.lines.len() - 1);

    // highlight only what is on screen; constructs that start above the window lose their colour, which beats a re-lex per keypress
    let top = s.buf.scroll;
    let bottom = (top + s.rows).min(s.buf.lines.len());
    let window: String = s.buf.lines[top..bottom].join("\n");
    // very long lines (CKEditor writes a note as one) cost far more to colour than they help
    let styled = if s.buf.lines[top..bottom].iter().any(|l| l.len() > 10_000) { vec![] } else { highlight::highlight(&window, &s.mime) };
    let sel = s.buf.selection();
    let mut out: Vec<Line> = Vec::with_capacity(s.rows);
    for (i, row) in (top..bottom).enumerate() {
        let is_cur = row == s.buf.cur.0;
        let num = format!("{:>w$} ", row + 1, w = gw - 2);
        let mut spans: Vec<Span<'static>> = vec![Span::styled(num, if is_cur { accent } else { dim }), Span::styled("│", dim)];
        // flatten to (char, style) cells so selection & horizontal scroll are exact
        let cells: Vec<(char, Style)> = match styled.get(i) {
            Some(l) => l.spans.iter().flat_map(|sp| sp.content.chars().map(move |c| (c, sp.style))).collect(),
            None => vec![],
        };
        let raw: Vec<char> = s.buf.lines[row].chars().take(s.buf.line_len(row)).collect();
        // map each raw char to its highlighted style (tabs became 4 spaces in `cells`)
        let mut ci = 0usize;
        let mut x = 0usize;
        let mut run = String::new();
        let mut run_style = Style::default();
        let flush = |run: &mut String, st: Style, spans: &mut Vec<Span<'static>>| { if !run.is_empty() { spans.push(Span::styled(std::mem::take(run), st)); } };
        let mut spans_static: Vec<Span<'static>> = vec![];
        for (k, ch) in raw.iter().enumerate() {
            let w = cell_width(*ch);
            let base = cells.get(ci).map(|c| c.1).unwrap_or_default();
            ci += if *ch == '\t' { 4 } else { 1 };
            let mut st = base;
            if sel.is_some_and(|(a, b)| (row, k) >= a && (row, k) < b) { st = st.add_modifier(Modifier::REVERSED); }
            // the part of this char's cells that falls inside the window; a char cut by the left edge shows as blanks
            let (lo, hi) = (x.max(s.buf.hscroll), (x + w).min(s.buf.hscroll + cols));
            if hi > lo {
                let text = if hi - lo < w || *ch == '\t' { " ".repeat(hi - lo) } else if ch.is_control() { "·".to_string() } else { ch.to_string() };
                if st != run_style { flush(&mut run, run_style, &mut spans_static); run_style = st; }
                run.push_str(&text);
            }
            x += w;
        }
        flush(&mut run, run_style, &mut spans_static);
        // a selection that spans the line break shows as a trailing highlighted cell
        if sel.is_some_and(|(a, b)| (row, raw.len()) >= a && (row, raw.len()) < b) && x >= s.buf.hscroll && x < s.buf.hscroll + cols {
            spans_static.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
        }
        spans.extend(spans_static);
        out.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(out), text_r);

    // footer
    let pos = format!("Ln {}, Col {}", s.buf.cur.0 + 1, s.buf.cur.1 + 1);
    let foot = match s.mode {
        Mode::Find => Line::from(vec![Span::styled(" Find: ", accent), Span::raw(s.input.clone()), Span::styled("▏", accent), Span::styled("   Enter next · Esc cancel", dim)]),
        Mode::Goto => Line::from(vec![Span::styled(" Go to line: ", accent), Span::raw(s.input.clone()), Span::styled("▏", accent)]),
        Mode::ConfirmClose => Line::from(Span::styled(" Unsaved changes — y save & close · d discard · any other key keeps editing", Style::default().fg(Color::Yellow))),
        Mode::Edit if !s.msg.is_empty() => Line::from(vec![Span::styled(format!(" {}", s.msg), accent)]),
        Mode::Edit => Line::from(vec![Span::styled(format!(" {pos}  "), dim), Span::styled("^S", accent), Span::styled(" save  ", dim), Span::styled("Esc", accent), Span::styled(" close  ", dim), Span::styled("^F", accent), Span::styled(" find  ", dim), Span::styled("^G", accent), Span::styled(" line  ", dim), Span::styled("^Z", accent), Span::styled(" undo  ", dim), Span::styled("^K", accent), Span::styled(" cut line", dim)].into_iter().chain(if s.mime == "text/html" { vec![Span::styled("  F2", accent), Span::styled(" in-place", dim)] } else { vec![] }).collect::<Vec<Span>>()),
    };
    f.render_widget(Paragraph::new(foot), foot_r);

    // terminal cursor
    if s.mode == Mode::Edit {
        let x = cur_col.saturating_sub(s.buf.hscroll);
        let y = s.buf.cur.0.saturating_sub(s.buf.scroll);
        if y < s.rows && x < cols { f.set_cursor_position((text_r.x + (gw + x) as u16, text_r.y + y as u16)); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent { KeyEvent { code, modifiers: m, kind: KeyEventKind::Press, state: KeyEventState::NONE } }
    fn type_str(s: &mut EditSession, t: &str) { for c in t.chars() { s.key(k(KeyCode::Char(c), KeyModifiers::NONE)); } }

    #[test]
    fn typing_save_and_confirm_close() {
        let mut s = EditSession::new("n1", "T", "text/html", "<p>hi</p>");
        assert_eq!(s.key(k(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Close, "clean buffer closes at once");
        let mut s = EditSession::new("n1", "T", "text/html", "<p>hi</p>");
        s.key(k(KeyCode::End, KeyModifiers::NONE));
        type_str(&mut s, "!");
        assert_eq!(s.buf.text(), "<p>hi</p>!");
        assert_eq!(s.key(k(KeyCode::Char('s'), KeyModifiers::CONTROL)), Outcome::Save);
        s.saved("<p>hi</p>!".into());
        assert!(!s.buf.changed());
        type_str(&mut s, "?");
        assert_eq!(s.key(k(KeyCode::Esc, KeyModifiers::NONE)), Outcome::None, "dirty buffer asks first");
        assert_eq!(s.key(k(KeyCode::Char('x'), KeyModifiers::NONE)), Outcome::None);
        assert_eq!(s.buf.text(), "<p>hi</p>!?", "other key cancels the prompt without typing");
        s.key(k(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(s.key(k(KeyCode::Char('d'), KeyModifiers::NONE)), Outcome::Close);
    }

    #[test]
    fn find_goto_and_indent_keys() {
        let mut s = EditSession::new("n", "T", "text/plain", "a\nfoo\nbar\nfoo");
        s.key(k(KeyCode::Char('f'), KeyModifiers::CONTROL));
        type_str(&mut s, "foo");
        s.key(k(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(s.buf.cur, (1, 3));
        s.key(k(KeyCode::F(3), KeyModifiers::NONE));
        assert_eq!(s.buf.cur, (3, 3));
        s.key(k(KeyCode::Char('g'), KeyModifiers::CONTROL));
        type_str(&mut s, "3");
        s.key(k(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(s.buf.cur.0, 2);
        s.key(k(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.buf.lines[2], "  bar");
        s.key(k(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(s.buf.lines[2], "bar");
    }

    #[test]
    fn alt_and_super_chords_are_not_text_and_tiny_windows_do_not_panic() {
        let mut s = EditSession::new("n", "T", "text/plain", &"line\n".repeat(200));
        s.key(k(KeyCode::Char('b'), KeyModifiers::ALT));
        s.key(k(KeyCode::Char('f'), KeyModifiers::SUPER));
        assert_eq!(s.buf.text(), "line\n".repeat(200));
        s.buf.doc_end(false);
        use ratatui::{Terminal, backend::TestBackend};
        for (w, h) in [(1u16, 1u16), (3, 2), (10, 2), (40, 3), (80, 24)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &mut s, f.area())).unwrap();
            s.key(k(KeyCode::Char('a'), KeyModifiers::CONTROL));
            s.key(k(KeyCode::Backspace, KeyModifiers::NONE));
            t.draw(|f| draw(f, &mut s, f.area())).unwrap();
        }
    }

    #[test]
    fn mouse_wheel_scroll_survives_the_next_draw() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut s = EditSession::new("n", "T", "text/plain", &"line\n".repeat(200));
        let mut t = Terminal::new(TestBackend::new(60, 12)).unwrap();
        t.draw(|f| draw(f, &mut s, f.area())).unwrap();
        s.scroll_by(30);
        t.draw(|f| draw(f, &mut s, f.area())).unwrap();
        assert_eq!(s.buf.scroll, 30);
    }

    #[test]
    fn ctrl_chords_never_type_letters() {
        let mut s = EditSession::new("n", "T", "text/plain", "x");
        for c in ['b', 'e', 'h', 'j', 'l', 'n', 'o', 'p', 'r', 't', 'u'] { s.key(k(KeyCode::Char(c), KeyModifiers::CONTROL)); }
        assert_eq!(s.buf.text(), "x");
    }
}
