//! Text buffer behind the built-in editor. It edits a note's stored source byte-for-byte: lines
//! are split on `\n` only, so a `\r` stays attached to its line and nothing is normalised.
//! Cursor columns count chars; a trailing `\r` is never reachable by the cursor.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snap { lines: Vec<String>, cur: (usize, usize) }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Last { Insert, Delete, Other }

pub struct EditBuf {
    pub lines: Vec<String>,
    /// (row, col) in chars
    pub cur: (usize, usize),
    pub anchor: Option<(usize, usize)>,
    pub dirty: bool,
    pub crlf: bool,
    pub scroll: usize,
    pub hscroll: usize,
    want_col: usize,
    original: String,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last: Last,
}

fn char_len(s: &str) -> usize { s.chars().count() }
fn visible_len(s: &str) -> usize { char_len(s.strip_suffix('\r').unwrap_or(s)) }
fn byte_at(s: &str, col: usize) -> usize { s.char_indices().nth(col).map_or(s.len(), |(i, _)| i) }
fn fold(c: char) -> impl Iterator<Item = char> { c.to_lowercase() }
fn is_word(c: char) -> bool { c.is_alphanumeric() || c == '_' }

impl EditBuf {
    pub fn new(text: &str) -> Self {
        let lines: Vec<String> = text.split('\n').map(String::from).collect();
        let crlf = text.matches("\r\n").count() * 2 > text.matches('\n').count();
        EditBuf { lines, cur: (0, 0), anchor: None, dirty: false, crlf, scroll: 0, hscroll: 0, want_col: 0, original: text.to_string(), undo: vec![], redo: vec![], last: Last::Other }
    }

    pub fn text(&self) -> String { self.lines.join("\n") }
    pub fn mark_saved(&mut self) { self.original = self.text(); self.dirty = false; }
    /// Cheap unless something was edited: compares line by line without building the whole text.
    pub fn changed(&self) -> bool {
        if !self.dirty { return false; }
        !self.lines.iter().map(String::as_str).eq(self.original.split('\n'))
    }
    pub fn line_len(&self, row: usize) -> usize { visible_len(&self.lines[row]) }

    fn snapshot(&mut self, kind: Last) {
        // a zero-width selection is no selection; left behind it would swallow later typing
        if self.anchor == Some(self.cur) { self.anchor = None; }
        if !(kind != Last::Other && kind == self.last) {
            self.undo.push(Snap { lines: self.lines.clone(), cur: self.cur });
            let chars: usize = self.lines.iter().map(String::len).sum();
            let cap = (32_000_000 / chars.max(1)).clamp(8, 300);
            while self.undo.len() > cap { self.undo.remove(0); }
        }
        self.redo.clear();
        self.last = kind;
        self.dirty = true;
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(Snap { lines: std::mem::replace(&mut self.lines, s.lines), cur: self.cur });
            self.cur = s.cur; self.anchor = None; self.last = Last::Other; self.want_col = self.cur.1; self.dirty = true;
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(Snap { lines: std::mem::replace(&mut self.lines, s.lines), cur: self.cur });
            self.cur = s.cur; self.anchor = None; self.last = Last::Other; self.want_col = self.cur.1; self.dirty = true;
        }
    }

    // ------------------------------------------------------------ selection

    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        (a != self.cur).then(|| if a < self.cur { (a, self.cur) } else { (self.cur, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        if s.0 == e.0 { return Some(self.lines[s.0].chars().skip(s.1).take(e.1 - s.1).collect()); }
        let mut out: String = self.lines[s.0].chars().skip(s.1).collect();
        for r in s.0 + 1..e.0 { out.push('\n'); out.push_str(&self.lines[r]); }
        out.push('\n');
        out.extend(self.lines[e.0].chars().take(e.1));
        Some(out)
    }

    pub fn delete_selection(&mut self) -> bool {
        if self.selection().is_none() { self.anchor = None; return false; }
        self.snapshot(Last::Other);
        self.remove_selection()
    }

    fn remove_selection(&mut self) -> bool {
        let Some((s, e)) = self.selection() else { self.anchor = None; return false };
        let tail: String = self.lines[e.0].chars().skip(e.1).collect();
        let head: String = self.lines[s.0].chars().take(s.1).collect();
        self.lines.splice(s.0..=e.0, [head + &tail]);
        self.cur = s; self.anchor = None; self.want_col = s.1;
        true
    }

    pub fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        let last = self.lines.len() - 1;
        self.cur = (last, self.line_len(last));
    }

    // ------------------------------------------------------------ editing

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() { return; }
        if self.selection().is_some() { self.snapshot(Last::Other); self.remove_selection(); }
        else if s.chars().count() == 1 && s != "\n" {
            // a run of typing is one undo step, but each new word starts a fresh one
            let c = s.chars().next().unwrap();
            let before = self.lines[self.cur.0].chars().nth(self.cur.1.wrapping_sub(1));
            if c.is_whitespace() && before.is_some_and(|p| !p.is_whitespace()) {
                self.snapshot(Last::Other);
                self.last = Last::Insert;
            } else { self.snapshot(Last::Insert); }
        } else { self.snapshot(Last::Other); }
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let (r, c) = self.cur;
        let at = byte_at(&self.lines[r], c);
        if !s.contains('\n') {
            self.lines[r].insert_str(at, &s);
            self.cur.1 = c + char_len(&s);
            self.want_col = self.cur.1;
            return;
        }
        // multi-line: build the new lines once and splice (linear in the pasted size)
        let tail = self.lines[r].split_off(at);
        let mut pieces = s.split('\n');
        self.lines[r].push_str(pieces.next().unwrap_or(""));
        let mut add: Vec<String> = Vec::new();
        let mut prev_is_cur = true;
        let mut last_len = 0;
        for p in pieces {
            if prev_is_cur { if self.crlf && !self.lines[r].ends_with('\r') { self.lines[r].push('\r'); } prev_is_cur = false; }
            else if let Some(l) = add.last_mut() { if self.crlf && !l.ends_with('\r') { l.push('\r'); } }
            add.push(p.to_string());
            last_len = char_len(p);
        }
        if let Some(l) = add.last_mut() { l.push_str(&tail); }
        let n = add.len();
        self.lines.splice(r + 1..r + 1, add);
        self.cur = (r + n, last_len);
        self.want_col = last_len;
    }

    pub fn insert_char(&mut self, c: char) { self.insert_str(&c.to_string()); }

    /// Enter: split the line, carrying the indentation over.
    pub fn newline(&mut self) {
        self.snapshot(Last::Other);
        self.remove_selection();
        self.break_line(true);
    }

    fn break_line(&mut self, indent: bool) {
        let (r, c) = self.cur;
        let at = byte_at(&self.lines[r], c);
        let tail = self.lines[r].split_off(at);
        let ws: String = if indent { self.lines[r].chars().take_while(|c| *c == ' ' || *c == '\t').collect() } else { String::new() };
        if self.crlf && !self.lines[r].ends_with('\r') { self.lines[r].push('\r'); }
        let ws_len = char_len(&ws);
        self.lines.insert(r + 1, ws + &tail);
        self.cur = (r + 1, ws_len);
        self.want_col = ws_len;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() { return; }
        let (r, c) = self.cur;
        if c == 0 && r == 0 { return; }
        self.snapshot(Last::Delete);
        if c == 0 {
            let line = self.lines.remove(r);
            let prev = &mut self.lines[r - 1];
            if prev.ends_with('\r') { prev.pop(); }
            let col = char_len(prev);
            prev.push_str(&line);
            self.cur = (r - 1, col);
        } else {
            let at = byte_at(&self.lines[r], c - 1);
            self.lines[r].remove(at);
            self.cur.1 = c - 1;
        }
        self.want_col = self.cur.1;
    }

    pub fn delete(&mut self) {
        if self.delete_selection() { return; }
        let (r, c) = self.cur;
        if c >= self.line_len(r) && r + 1 >= self.lines.len() { return; }
        self.snapshot(Last::Delete);
        if c >= self.line_len(r) {
            let next = self.lines.remove(r + 1);
            if self.lines[r].ends_with('\r') { self.lines[r].pop(); }
            self.lines[r].push_str(&next);
        } else {
            let at = byte_at(&self.lines[r], c);
            self.lines[r].remove(at);
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.selection().is_some() { self.delete_selection(); return; }
        let end = self.cur;
        self.anchor = Some(end);
        self.word_left(false);
        self.anchor = Some(end);
        self.delete_selection();
    }

    pub fn delete_line(&mut self) {
        self.snapshot(Last::Other);
        let r = self.cur.0;
        if self.lines.len() == 1 { self.lines[0].clear(); }
        else if r + 1 == self.lines.len() {
            // removing the unterminated last line also removes the break that ended the one before it
            self.lines.remove(r);
            if let Some(l) = self.lines.last_mut() { if l.ends_with('\r') { l.pop(); } }
        } else { self.lines.remove(r); }
        let r = r.min(self.lines.len() - 1);
        self.cur = (r, self.cur.1.min(self.line_len(r)));
        self.anchor = None;
    }

    pub fn duplicate_line(&mut self) {
        self.snapshot(Last::Other);
        // a selection anchored in the old text would point at the wrong place once a line is inserted
        self.anchor = None;
        let r = self.cur.0;
        let mut copy = self.lines[r].clone();
        if r + 1 == self.lines.len() {
            // the old last line gains the line break, the copy becomes the new unterminated last line
            if self.crlf && !self.lines[r].ends_with('\r') { self.lines[r].push('\r'); }
            if copy.ends_with('\r') { copy.pop(); }
        }
        self.lines.insert(r + 1, copy);
        self.cur.0 = r + 1;
    }

    pub fn indent(&mut self, unit: &str) {
        let (s, e) = self.selection().unwrap_or((self.cur, self.cur));
        if self.selection().is_none() { self.insert_str(unit); return; }
        self.snapshot(Last::Other);
        for r in s.0..=e.0 { self.lines[r].insert_str(0, unit); }
        let n = char_len(unit);
        if let Some(a) = self.anchor.as_mut() { a.1 += n; }
        self.cur.1 += n;
    }

    pub fn dedent(&mut self, unit: &str) {
        let (s, e) = self.selection().unwrap_or((self.cur, self.cur));
        self.snapshot(Last::Other);
        for r in s.0..=e.0 {
            let strip = if self.lines[r].starts_with(unit) { unit.len() } else if self.lines[r].starts_with('\t') { 1 } else { 0 };
            self.lines[r].drain(..strip);
            if r == self.cur.0 { self.cur.1 = self.cur.1.saturating_sub(strip); }
            if let Some(a) = self.anchor.as_mut() { if a.0 == r { a.1 = a.1.saturating_sub(strip); } }
        }
    }

    // ------------------------------------------------------------ movement

    fn set_cur(&mut self, pos: (usize, usize), select: bool, keep_want: bool) {
        if select { if self.anchor.is_none() { self.anchor = Some(self.cur); } } else { self.anchor = None; }
        self.cur = pos;
        if !keep_want { self.want_col = pos.1; }
        self.last = Last::Other;
    }

    pub fn left(&mut self, select: bool) {
        if !select { if let Some((s, _)) = self.selection() { self.set_cur(s, false, false); return; } }
        let (r, c) = self.cur;
        let pos = if c > 0 { (r, c - 1) } else if r > 0 { (r - 1, self.line_len(r - 1)) } else { (0, 0) };
        self.set_cur(pos, select, false);
    }

    pub fn right(&mut self, select: bool) {
        if !select { if let Some((_, e)) = self.selection() { self.set_cur(e, false, false); return; } }
        let (r, c) = self.cur;
        let pos = if c < self.line_len(r) { (r, c + 1) } else if r + 1 < self.lines.len() { (r + 1, 0) } else { (r, c) };
        self.set_cur(pos, select, false);
    }

    pub fn up(&mut self, n: usize, select: bool) {
        let r = self.cur.0;
        if r == 0 { self.set_cur((0, 0), select, false); return; }
        let nr = r.saturating_sub(n);
        let c = self.want_col.min(self.line_len(nr));
        self.set_cur((nr, c), select, true);
    }

    pub fn down(&mut self, n: usize, select: bool) {
        let r = self.cur.0;
        if r + 1 >= self.lines.len() { let l = self.lines.len() - 1; self.set_cur((l, self.line_len(l)), select, false); return; }
        let nr = (r + n).min(self.lines.len() - 1);
        let c = self.want_col.min(self.line_len(nr));
        self.set_cur((nr, c), select, true);
    }

    /// Home: first non-blank, then column 0 on a second press.
    pub fn home(&mut self, select: bool) {
        let r = self.cur.0;
        let first = self.lines[r].chars().take_while(|c| *c == ' ' || *c == '\t').count().min(self.line_len(r));
        self.set_cur((r, if self.cur.1 == first { 0 } else { first }), select, false);
    }

    pub fn end(&mut self, select: bool) { let r = self.cur.0; self.set_cur((r, self.line_len(r)), select, false); }
    pub fn doc_start(&mut self, select: bool) { self.set_cur((0, 0), select, false); }
    pub fn doc_end(&mut self, select: bool) { let l = self.lines.len() - 1; self.set_cur((l, self.line_len(l)), select, false); }

    pub fn word_left(&mut self, select: bool) {
        let (mut r, mut c) = self.cur;
        loop {
            if c == 0 {
                if r == 0 { break; }
                r -= 1; c = self.line_len(r);
                break;
            }
            let chars: Vec<char> = self.lines[r].chars().collect();
            while c > 0 && !is_word(chars[c - 1]) { c -= 1; }
            while c > 0 && is_word(chars[c - 1]) { c -= 1; }
            break;
        }
        self.set_cur((r, c), select, false);
    }

    pub fn word_right(&mut self, select: bool) {
        let (mut r, mut c) = self.cur;
        let len = self.line_len(r);
        if c >= len {
            if r + 1 < self.lines.len() { r += 1; c = 0; }
        } else {
            let chars: Vec<char> = self.lines[r].chars().collect();
            while c < len && is_word(chars[c]) { c += 1; }
            while c < len && !is_word(chars[c]) { c += 1; }
        }
        self.set_cur((r, c), select, false);
    }

    /// Put the cursor somewhere without editing (mouse click): ends the current typing run.
    pub fn place_cursor(&mut self, pos: (usize, usize)) { self.set_cur(pos, false, false); }

    pub fn goto_line(&mut self, line: usize) {
        let r = line.saturating_sub(1).min(self.lines.len() - 1);
        self.set_cur((r, 0), false, false);
    }

    /// Next occurrence of `needle` (case-insensitive) after the cursor, wrapping; selects it.
    /// Works on chars of the original lines, so columns always refer to the real text.
    pub fn find_next(&mut self, needle: &str) -> bool {
        if needle.is_empty() { return false; }
        let n: Vec<char> = needle.chars().flat_map(fold).collect();
        let total = self.lines.len();
        let start = self.selection().map_or(self.cur, |(_, e)| e);
        for k in 0..=total {
            let r = (start.0 + k) % total;
            let line: Vec<char> = self.lines[r].chars().take(self.line_len(r)).collect();
            // fold char by char, remembering which original column each folded char came from
            let mut folded: Vec<char> = vec![];
            let mut origin: Vec<usize> = vec![];
            for (i, c) in line.iter().enumerate() { for f in fold(*c) { folded.push(f); origin.push(i); } }
            let (lo, hi) = if k == 0 { (start.1, usize::MAX) } else if k == total { (0, start.1) } else { (0, usize::MAX) };
            let mut s = 0;
            while s + n.len() <= folded.len() {
                if folded[s..s + n.len()] == n[..] && origin[s] >= lo && origin[s] < hi {
                    let end_col = origin[s + n.len() - 1] + 1;
                    self.anchor = Some((r, origin[s]));
                    self.cur = (r, end_col);
                    self.want_col = end_col;
                    self.last = Last::Other;
                    return true;
                }
                s += 1;
            }
        }
        false
    }

    /// Keep the cursor inside the viewport (display columns, tabs expanded by the caller).
    pub fn follow(&mut self, rows: usize, cols: usize, cur_col: usize) {
        if rows == 0 { return; }
        if self.cur.0 < self.scroll { self.scroll = self.cur.0; }
        if self.cur.0 >= self.scroll + rows { self.scroll = self.cur.0 + 1 - rows; }
        if cols > 0 {
            if cur_col < self.hscroll { self.hscroll = cur_col; }
            if cur_col >= self.hscroll + cols { self.hscroll = cur_col + 1 - cols; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_text_round_trips_byte_exact() {
        for t in ["", "a", "a\n", "<p>x</p>\r\n<p>y</p>\r\n\r\n", "mixed\r\nline\nend\r", "ünï \u{1F600}\n\n\n"] {
            let b = EditBuf::new(t);
            assert_eq!(b.text(), t);
            assert!(!b.changed());
        }
    }

    #[test]
    fn typing_enter_backspace_with_unicode() {
        let mut b = EditBuf::new("héllo");
        b.doc_end(false);
        b.insert_str(" wörld");
        assert_eq!(b.text(), "héllo wörld");
        b.home(false); b.home(false);
        b.right(false); b.right(false);
        b.newline();
        assert_eq!(b.text(), "hé\nllo wörld");
        b.backspace();
        assert_eq!(b.text(), "héllo wörld");
        assert_eq!(b.cur, (0, 2));
    }

    #[test]
    fn crlf_is_preserved_when_splitting_and_joining() {
        let mut b = EditBuf::new("one\r\ntwo\r\n");
        b.right(false);
        b.newline();
        assert_eq!(b.text(), "o\r\nne\r\ntwo\r\n");
        b.backspace();
        assert_eq!(b.text(), "one\r\ntwo\r\n");
        // the cursor can't sit after the \r
        b.end(false);
        assert_eq!(b.cur, (0, 3));
    }

    #[test]
    fn selection_copy_cut_and_multiline_replace() {
        let mut b = EditBuf::new("abc\ndef\nghi");
        b.right(false);
        b.down(2, true);
        assert_eq!(b.selected_text().as_deref(), Some("bc\ndef\ng"));
        b.insert_str("X");
        assert_eq!(b.text(), "aXhi");
        b.undo();
        assert_eq!(b.text(), "abc\ndef\nghi");
        b.redo();
        assert_eq!(b.text(), "aXhi");
    }

    #[test]
    fn undo_coalesces_typing_runs() {
        let mut b = EditBuf::new("");
        for c in "hello".chars() { b.insert_char(c); }
        b.undo();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn undo_steps_per_word_and_big_paste_is_fast() {
        let mut b = EditBuf::new("");
        for c in "one two".chars() { b.insert_char(c); }
        b.undo();
        assert_eq!(b.text(), "one");
        b.undo();
        assert_eq!(b.text(), "");
        let big = "line of text\n".repeat(200_000);
        let t = std::time::Instant::now();
        b.insert_str(&big);
        assert!(t.elapsed().as_secs() < 2);
        assert_eq!(b.lines.len(), 200_001);
        assert_eq!(b.cur, (200_000, 0));
        let mut c = EditBuf::new("ab\r\ncd");
        c.crlf = true;
        c.cur = (0, 1);
        c.insert_str("X\nY\nZ");
        assert_eq!(c.text(), "aX\r\nY\r\nZb\r\ncd");
    }

    #[test]
    fn collapsed_selection_does_not_swallow_typing_and_crlf_follows_the_majority() {
        let mut b = EditBuf::new("abc");
        b.right(true); b.left(true);
        b.insert_char('X'); b.insert_char('Y');
        assert_eq!(b.text(), "XYabc");
        let mut e = EditBuf::new("");
        e.select_all(); e.insert_char('a'); e.insert_char('b');
        assert_eq!(e.text(), "ab");
        let mut m = EditBuf::new("a\nb\r\nc\nd\n");
        assert!(!m.crlf);
        m.newline();
        assert!(!m.text().starts_with("\r"), "{:?}", m.text());
        assert!(EditBuf::new("a\r\nb\r\nc\n").crlf);
    }

    #[test]
    fn autoindent_words_and_find() {
        let mut b = EditBuf::new("  foo bar\nbaz Bar");
        b.end(false);
        b.newline();
        assert_eq!(b.cur, (1, 2));
        assert_eq!(b.text(), "  foo bar\n  \nbaz Bar");
        b.doc_start(false);
        b.word_right(false);
        assert_eq!(b.cur, (0, 2));
        assert!(b.find_next("bar"));
        assert_eq!(b.selected_text().as_deref(), Some("bar"));
        assert!(b.find_next("bar"));
        assert_eq!(b.selected_text().as_deref(), Some("Bar"));
        assert!(b.find_next("bar"), "wraps");
        assert!(!b.find_next("zzz"));
    }

    #[test]
    fn delete_joins_lines_and_edges_are_safe() {
        let mut b = EditBuf::new("a\nb");
        b.end(false);
        b.delete();
        assert_eq!(b.text(), "ab");
        b.doc_end(false);
        b.delete();
        b.doc_start(false);
        b.backspace();
        assert_eq!(b.text(), "ab");
        b.delete_line();
        assert_eq!(b.text(), "");
        b.delete_line();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn find_survives_case_folding_that_changes_length() {
        // 'İ' lower-cases to two chars; columns must still index the original text
        let mut b = EditBuf::new("İİİ x\nstraße STRASSE");
        assert!(b.find_next("x"));
        assert_eq!(b.cur, (0, 5));
        b.word_left(false);
        b.end(false);
        assert!(b.find_next("ß"));
        assert_eq!(b.selected_text().as_deref(), Some("ß"));
        assert!(b.find_next("İ"), "wraps back to the first line");
        assert_eq!(b.cur.0, 0);
    }

    #[test]
    fn undo_after_save_counts_as_a_change() {
        let mut b = EditBuf::new("a");
        b.insert_char('b');
        b.mark_saved();
        assert!(!b.changed());
        b.undo();
        assert!(b.changed());
    }

    #[test]
    fn random_keystrokes_never_panic_or_break_invariants() {
        // tiny xorshift so the test needs no dependency
        let mut x: u64 = 0x2545F4914F6CDD1D;
        let mut next = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        let mut b = EditBuf::new("héllo\r\nwörld İ\n\nend");
        for _ in 0..4000 {
            match next() % 22 {
                0 => b.insert_char(['a', 'é', 'İ', ' ', '\t', '😀'][(next() % 6) as usize]),
                1 => b.newline(),
                2 => b.backspace(),
                3 => b.delete(),
                4 => b.left(next() % 2 == 0),
                5 => b.right(next() % 2 == 0),
                6 => b.up(1, next() % 2 == 0),
                7 => b.down(1, next() % 2 == 0),
                8 => b.home(next() % 2 == 0),
                9 => b.end(next() % 2 == 0),
                10 => b.word_left(next() % 2 == 0),
                11 => b.word_right(next() % 2 == 0),
                12 => b.undo(),
                13 => b.redo(),
                14 => b.delete_line(),
                15 => b.duplicate_line(),
                16 => b.delete_word_back(),
                17 => { b.find_next("e"); }
                18 => b.insert_str("pa\r\nste\nd"),
                19 => b.indent("  "),
                20 => b.dedent("  "),
                _ => b.select_all(),
            }
            assert!(b.cur.0 < b.lines.len());
            assert!(b.cur.1 <= b.line_len(b.cur.0), "cursor {:?} on {:?}", b.cur, b.lines[b.cur.0]);
            if let Some(a) = b.anchor { assert!(a.0 < b.lines.len() && a.1 <= b.line_len(a.0), "anchor {a:?}"); }
            let _ = b.selected_text();
            let _ = b.changed();
        }
    }

    #[test]
    fn crlf_buffers_keep_their_line_ends_on_duplicate_and_delete_of_the_last_line() {
        let mut b = EditBuf::new("a\r\nb");
        b.doc_end(false);
        b.duplicate_line();
        assert_eq!(b.text(), "a\r\nb\r\nb");
        b.delete_line();
        assert_eq!(b.text(), "a\r\nb");
        b.delete_line();
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn pasted_crlf_is_normalised_to_the_buffer_style() {
        let mut b = EditBuf::new("x\r\n");
        b.insert_str("a\r\nb");
        assert_eq!(b.text(), "a\r\nbx\r\n");
    }
}
