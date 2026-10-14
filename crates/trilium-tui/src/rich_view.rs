//! In-place editing of a text note: the note pane itself becomes the editor (no tags on screen).
//! The model is `trilium_core::richtext::Doc`; this file owns the layout (wrapping + cursor <-> screen mapping), keys and drawing.
use crate::images::{ImageHost, InlinePlace, NoImages, cells_for};
use crate::render;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout as RLayout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::HashMap;
use trilium_core::htmltree::Node;
use trilium_core::richtext::{self as rt, Anc, Cursor, Doc, Item, ListKind, Outline, Unit, mark};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Resolver<'a> = &'a dyn Fn(&str) -> Option<(String, String)>;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode { Edit, Link, Find, ConfirmClose }

#[derive(PartialEq, Eq, Clone, Copy)]
enum Last { Other, Typing, Delete }

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome { None, Save, Close, SaveAndClose, ToSource, PickNote, PickImage, ToggleImages, Follow(rt::LinkTarget) }

struct Snap { nodes: Vec<Node>, cursor: Cursor, version: u64 }

/// One laid-out line of a text block: which units it shows, and the quote bars / list marker in front of it.
#[derive(Clone)]
struct LineRef {
    block: usize,
    u0: usize,
    u1: usize,
    vis: usize,
    last: bool,
    prefix: Vec<Span<'static>>,
    pw: usize,
    /// columns of the to-do marker, when this line shows one
    checkbox: Option<(usize, usize)>,
}

impl LineRef {
    fn contains(&self, off: usize) -> bool { off >= self.u0 && (off < self.u1 || self.last) }
}

/// One cell's slice of a table screen row.
struct CellSeg { x: usize, w: usize, line: Option<LineRef>, end: Option<Cursor> }

enum RowKind {
    Text(LineRef),
    Cells(Vec<CellSeg>),
    Plain { prefix: Vec<Span<'static>>, line: Line<'static> },
}

struct Layout {
    rows: Vec<RowKind>,
    units: Vec<Vec<Unit>>,
    pre: Vec<bool>,
    styles: Vec<Style>,
    first_row: Vec<usize>,
    width: usize,
    /// opaque elements were drawn for real (false: placeholders, good enough for cursor movement)
    complete: bool,
    /// pictures shown inline when this was laid out, and where
    inline: bool,
    images: Vec<InlinePlace>,
}

pub struct RichSession {
    pub note_id: String,
    pub title: String,
    doc: Doc,
    cursor: Cursor,
    anchor: Option<Cursor>,
    goal: Option<usize>,
    version: u64,
    saved_version: u64,
    next_version: u64,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last: Last,
    last_end: Option<Cursor>,
    /// formats toggled at the caret with nothing selected: applied to the text typed next
    pending: Vec<(u16, bool)>,
    mode: Mode,
    input: String,
    /// the link prompt inserts a new link (text = the address) instead of linking the selection
    link_insert: bool,
    last_find: String,
    clip: String,
    pub msg: String,
    /// what the database held when the editor opened or last saved
    pub base: String,
    force: bool,
    layout: Option<Layout>,
    opaque: HashMap<(String, usize), Vec<Line<'static>>>,
    width: usize,
    height: usize,
    scroll: usize,
    followed: (Cursor, u64, usize, usize),
    pub area: Rect,
    dragging: bool,
}

fn system_copy(t: &str) { crate::edit_view::system_copy(t) }
fn system_paste() -> Option<String> { crate::edit_view::system_paste() }

fn heading_style(level: u8) -> Style {
    match level {
        0 => Style::default(),
        1 => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        2 => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        3 => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        _ => Style::default().add_modifier(Modifier::BOLD),
    }
}

fn marks_style(m: u16) -> Style {
    let mut s = Style::default();
    if m & mark::BOLD != 0 { s = s.add_modifier(Modifier::BOLD); }
    if m & mark::ITALIC != 0 { s = s.add_modifier(Modifier::ITALIC); }
    if m & mark::UNDERLINE != 0 { s = s.add_modifier(Modifier::UNDERLINED); }
    if m & mark::STRIKE != 0 { s = s.add_modifier(Modifier::CROSSED_OUT); }
    if m & mark::CODE != 0 { s = s.fg(Color::Yellow); }
    if m & mark::HIGHLIGHT != 0 { s = s.bg(Color::Yellow).fg(Color::Black); }
    if m & mark::LINK != 0 { s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED); }
    s
}

/// What a unit looks like on screen and how many cells it takes; hostile text never puts control characters in a cell.
fn cell(u: &Unit, pre: bool) -> (String, usize) {
    if let Some(l) = &u.label {
        let t: String = l.chars().map(|c| if c.is_control() { '·' } else { c }).collect();
        let w = t.width();
        return (t, w);
    }
    match u.ch {
        '\t' if pre => ("    ".into(), 4),
        '\t' | '\n' | '\r' | '\u{a0}' => (" ".into(), 1),
        c if c.is_control() => ("·".into(), 1),
        c => (c.to_string(), c.width().unwrap_or(0)),
    }
}

fn breakable(u: &Unit) -> bool { u.label.is_none() && matches!(u.ch, ' ' | '\t' | '\n' | '\r') && !u.br }

fn spaces(n: usize) -> Span<'static> { Span::raw(" ".repeat(n)) }

/// Quote bars and list markers in front of a block: (first line, continuation lines, width, to-do marker columns).
fn prefix_of(bi: &rt::BlockInfo, dim: Style) -> (Vec<Span<'static>>, Vec<Span<'static>>, usize, Option<(usize, usize)>) {
    let last_li = bi.anc.iter().rposition(|a| matches!(a, Anc::Li { .. }));
    let (mut first, mut cont, mut col, mut cb) = (vec![], vec![], 0usize, None);
    for (k, a) in bi.anc.iter().enumerate() {
        match a {
            Anc::Quote => { first.push(Span::styled("│ ", dim)); cont.push(Span::styled("│ ", dim)); col += 2; }
            Anc::Li { ordinal, first: is_first } => {
                let todo = bi.checkbox.filter(|_| Some(k) == last_li);
                let marker = match (todo, ordinal) { (Some(true), _) => "☑ ".to_string(), (Some(false), _) => "☐ ".to_string(), (None, Some(o)) => format!("{o}. "), (None, None) => "• ".to_string() };
                let w = marker.width();
                if *is_first {
                    let st = match todo { Some(true) => Style::default().fg(Color::Green), Some(false) => Style::default().fg(Color::Yellow), None => dim };
                    first.push(Span::styled(marker, st));
                    if todo.is_some() { cb = Some((col, col + w)); }
                } else { first.push(spaces(w)); }
                cont.push(spaces(w));
                col += w;
            }
        }
    }
    (first, cont, col, cb)
}

/// Wrap a block's units into lines of at most `avail` cells (after its prefix).
fn block_lines(block: usize, units: &[Unit], pre: bool, bi: &rt::BlockInfo, avail_total: usize, dim: Style) -> Vec<LineRef> {
    let (first, cont, pw, cb) = prefix_of(bi, dim);
    let avail = avail_total.saturating_sub(pw).max(4);
    let widths: Vec<usize> = units.iter().map(|u| cell(u, pre).1).collect();
    let n = units.len();
    let mut segs: Vec<(usize, usize, usize)> = vec![]; // (u0, u1, vis)
    let (mut start, mut x, mut brk, mut k) = (0usize, 0usize, None::<usize>, 0usize);
    while k < n {
        let u = &units[k];
        if u.br || (pre && u.ch == '\n' && u.label.is_none()) {
            segs.push((start, k + 1, k));
            k += 1; start = k; x = 0; brk = None;
            continue;
        }
        let w = widths[k];
        if !pre && breakable(u) && x + w > avail {
            segs.push((start, k + 1, k));
            k += 1; start = k; x = 0; brk = None;
            continue;
        }
        if x + w > avail && x > 0 {
            match brk.filter(|b| *b >= start) {
                Some(b) if !pre => { segs.push((start, b + 1, b)); k = b + 1; }
                _ => { segs.push((start, k, k)); }
            }
            start = k; x = 0; brk = None;
            continue;
        }
        x += w;
        if !pre && breakable(u) { brk = Some(k); }
        k += 1;
    }
    // the remainder is the final line (where a cursor at the very end lives)
    segs.push((start, n, n));
    let count = segs.len();
    segs.into_iter().enumerate().map(|(i, (u0, u1, vis))| {
        let is_first = i == 0;
        LineRef { block, u0, u1, vis, last: i + 1 == count, prefix: if is_first { first.clone() } else { cont.clone() }, pw, checkbox: if is_first { cb } else { None } }
    }).collect()
}

/// Cut styled text to at most `max` cells (an object wider than its table column).
fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut used = 0usize;
    for sp in spans {
        if used >= max { break; }
        let mut text = String::new();
        for ch in sp.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > max { break; }
            used += w;
            text.push(ch);
        }
        out.push(Span::styled(text, sp.style));
    }
    out
}

/// Width of the widest unwrapped line of a block (what a table column would like to be).
fn natural_width(units: &[Unit], pre: bool) -> usize {
    let (mut best, mut x) = (0usize, 0usize);
    for u in units {
        if u.br || (pre && u.ch == '\n' && u.label.is_none()) { best = best.max(x); x = 0; } else { x += cell(u, pre).1; }
    }
    best.max(x)
}

impl Layout {
    fn build(doc: &Doc, ol: &Outline, width: usize, opaque: &mut HashMap<(String, usize), Vec<Line<'static>>>, resolver: Option<Resolver>, host: &mut dyn ImageHost, max_rows: usize) -> Layout {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lay = Layout { rows: vec![], units: vec![], pre: vec![], styles: vec![], first_row: vec![0; ol.blocks.len()], width, complete: resolver.is_some(), inline: host.enabled(), images: vec![] };
        for bi in &ol.blocks {
            lay.units.push(doc.units(&bi.block));
            lay.pre.push(bi.block.pre);
            let mut st = heading_style(bi.heading);
            if bi.block.pre { st = st.fg(Color::Yellow); }
            if bi.anc.iter().any(|a| matches!(a, Anc::Quote)) { st = st.add_modifier(Modifier::ITALIC); }
            lay.styles.push(st);
        }
        let item_in_list = |it: &Item| match it {
            Item::Block(i) => ol.blocks[*i].anc.iter().any(|a| matches!(a, Anc::Li { .. })),
            _ => false,
        };
        let blank_looking = |lay: &Layout, it: Option<&Item>| match it {
            Some(Item::Block(i)) => !item_in_list(it.unwrap()) && lay.units[*i].iter().all(|u| u.label.is_none() && (u.ch.is_whitespace() || matches!(u.ch, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}'))),
            _ => false,
        };
        for (n, item) in ol.items.iter().enumerate() {
            match item {
                Item::Block(i) => {
                    lay.first_row[*i] = lay.rows.len();
                    let lines = block_lines(*i, &lay.units[*i], lay.pre[*i], &ol.blocks[*i], width, dim);
                    let pw = lines.first().map_or(0, |l| l.pw);
                    for l in lines { lay.rows.push(RowKind::Text(l)); }
                    // a block that is nothing but a picture gets room to show it (the caption row above stays the cursor's row)
                    let only_src = (lay.units[*i].len() == 1).then(|| lay.units[*i][0].src.clone()).flatten();
                    if let Some(src) = only_src {
                        if host.enabled() {
                            if let Some(dims) = host.dims(&src) {
                                let (cols, rows) = cells_for(dims, host.font(), width.saturating_sub(pw).max(4), max_rows);
                                lay.images.push(InlinePlace { row: lay.rows.len(), rows, cols, x: pw, src });
                                for _ in 0..rows { lay.rows.push(RowKind::Plain { prefix: vec![], line: Line::default() }); }
                            }
                        }
                    }
                }
                Item::Opaque { html, anc } => {
                    let mut prefix = vec![];
                    let mut pw = 0;
                    for a in anc {
                        match a { Anc::Quote => { prefix.push(Span::styled("│ ", dim)); pw += 2; } Anc::Li { .. } => { prefix.push(spaces(2)); pw += 2; } }
                    }
                    let inner = width.saturating_sub(pw).max(10);
                    let lines = match (opaque.get(&(html.clone(), inner)), resolver) {
                        (Some(l), _) => l.clone(),
                        (None, Some(res)) => {
                            let l = render::render_html_full(html, inner, res).lines;
                            if opaque.len() > 200 { opaque.clear(); }
                            opaque.insert((html.clone(), inner), l.clone());
                            l
                        }
                        (None, None) => vec![Line::from(Span::styled("[table or other object]", dim))],
                    };
                    for l in lines { lay.rows.push(RowKind::Plain { prefix: prefix.clone(), line: l }); }
                }
                Item::Table(trows) => lay.table(trows, ol, width, dim),
            }
            // a blank line between blocks, except between list items; an empty paragraph is itself the blank line
            let next_in_list = ol.items.get(n + 1).is_some_and(item_in_list);
            if n + 1 < ol.items.len() && !(item_in_list(item) && next_in_list) && !blank_looking(&lay, Some(item)) && !blank_looking(&lay, ol.items.get(n + 1)) {
                lay.rows.push(RowKind::Plain { prefix: vec![], line: Line::default() });
            }
        }
        lay
    }

    /// A table as a grid: columns sized to their content (shrunk to fit), each cell wrapped inside its column.
    fn table(&mut self, trows: &[rt::TableRow], ol: &Outline, width: usize, dim: Style) {
        let n = trows.iter().map(|r| r.cells.len()).max().unwrap_or(1).max(1);
        let sep = 3usize;
        let mut widths = vec![1usize; n];
        for r in trows {
            for (j, cell_items) in r.cells.iter().enumerate() {
                for it in cell_items {
                    if let Item::Block(i) = it { widths[j] = widths[j].max(prefix_of(&ol.blocks[*i], dim).2 + natural_width(&self.units[*i], self.pre[*i])); }
                }
            }
        }
        while widths.iter().sum::<usize>() + sep * (n - 1) > width {
            let (k, &m) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if m <= 6 { break; }
            widths[k] -= 1;
        }
        let mut xs = vec![0usize; n];
        for j in 1..n { xs[j] = xs[j - 1] + widths[j - 1] + sep; }
        for r in trows {
            let mut cell_lines: Vec<Vec<LineRef>> = vec![];
            let mut ends: Vec<Option<Cursor>> = vec![];
            for j in 0..n {
                let (mut lines, mut end) = (vec![], None);
                for it in r.cells.get(j).into_iter().flatten() {
                    if let Item::Block(i) = it {
                        lines.extend(block_lines(*i, &self.units[*i], self.pre[*i], &ol.blocks[*i], widths[j], dim));
                        end = Some(Cursor { block: *i, off: self.units[*i].len() });
                    }
                }
                cell_lines.push(lines);
                ends.push(end);
            }
            let start = self.rows.len();
            for it in r.cells.iter().flatten() { if let Item::Block(i) = it { self.first_row[*i] = start; } }
            for s in 0..cell_lines.iter().map(Vec::len).max().unwrap_or(1).max(1) {
                let segs = (0..n).map(|j| CellSeg { x: xs[j], w: widths[j], line: cell_lines[j].get(s).cloned(), end: ends[j] }).collect();
                self.rows.push(RowKind::Cells(segs));
            }
            if r.header {
                let rule: String = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─");
                self.rows.push(RowKind::Plain { prefix: vec![], line: Line::from(Span::styled(rule, dim)) });
            }
        }
    }

    /// The laid-out line holding a cursor position: (row, line, x of its cell, width of its cell).
    fn find_line(&self, c: Cursor) -> Option<(usize, &LineRef, usize, Option<usize>)> {
        let start = *self.first_row.get(c.block)?;
        for r in start..self.rows.len() {
            match &self.rows[r] {
                RowKind::Text(l) => {
                    if l.block == c.block && l.contains(c.off) { return Some((r, l, 0, None)); }
                    if l.block > c.block { break; }
                }
                RowKind::Cells(segs) => {
                    for s in segs {
                        if let Some(l) = &s.line { if l.block == c.block && l.contains(c.off) { return Some((r, l, s.x, Some(s.w))); } }
                    }
                }
                RowKind::Plain { .. } => {}
            }
        }
        None
    }

    /// (row, column) of a cursor position.
    fn locate(&self, c: Cursor) -> (usize, usize) {
        match self.find_line(c) {
            Some((r, l, x, w)) => {
                let units = &self.units[c.block];
                let to = c.off.min(l.vis).min(units.len()).max(l.u0);
                let col: usize = l.pw + units[l.u0..to].iter().map(|u| cell(u, self.pre[c.block]).1).sum::<usize>();
                // an object wider than its table column is clipped; the cursor stays inside the cell
                (r, x + w.map_or(col, |w| col.min(w.saturating_sub(1))))
            }
            None => (self.first_row.get(c.block).copied().unwrap_or(0), 0),
        }
    }

    fn at_line(&self, l: &LineRef, col: usize) -> Cursor {
        let want = col.saturating_sub(l.pw);
        let units = &self.units[l.block];
        let (mut acc, mut off) = (0usize, l.u0);
        while off < l.vis {
            let w = cell(&units[off], self.pre[l.block]).1;
            if want < acc + w.div_ceil(2) { break; }
            acc += w;
            off += 1;
        }
        Cursor { block: l.block, off }
    }

    /// Cursor at a screen cell of a text or table row.
    fn at(&self, row: usize, col: usize) -> Option<Cursor> {
        match self.rows.get(row)? {
            RowKind::Text(l) => Some(self.at_line(l, col)),
            RowKind::Cells(segs) => {
                let seg = segs.iter().rev().find(|s| col >= s.x).or(segs.first())?;
                match &seg.line {
                    Some(l) => Some(self.at_line(l, col.saturating_sub(seg.x))),
                    None => seg.end,
                }
            }
            RowKind::Plain { .. } => None,
        }
    }

    fn is_text(&self, row: usize) -> bool {
        match self.rows.get(row) {
            Some(RowKind::Text(_)) => true,
            Some(RowKind::Cells(segs)) => segs.iter().any(|s| s.line.is_some() || s.end.is_some()),
            _ => false,
        }
    }

    /// To-do marker under a click, if any.
    fn checkbox_at(&self, row: usize, col: usize) -> bool {
        let hit = |l: &LineRef, x: usize| l.checkbox.is_some_and(|(s, e)| col >= x + s && col < x + e);
        match self.rows.get(row) {
            Some(RowKind::Text(l)) => hit(l, 0),
            Some(RowKind::Cells(segs)) => segs.iter().rev().find(|s| col >= s.x).and_then(|s| s.line.as_ref().map(|l| hit(l, s.x))).unwrap_or(false),
            _ => false,
        }
    }
}

impl RichSession {
    /// `None` when the note has no editable text at all (nothing but tables, say).
    pub fn new(note_id: &str, title: &str, html: &str) -> Option<RichSession> {
        let doc = Doc::parse(html);
        if doc.outline().blocks.is_empty() { return None; }
        Some(RichSession {
            note_id: note_id.into(), title: title.into(), doc, cursor: Cursor::default(), anchor: None, goal: None,
            version: 0, saved_version: 0, next_version: 1, undo: vec![], redo: vec![], last: Last::Other, last_end: None, pending: vec![],
            mode: Mode::Edit, input: String::new(), link_insert: false, last_find: String::new(), clip: String::new(), msg: String::new(), base: html.to_string(), force: false,
            layout: None, opaque: HashMap::new(), width: 80, height: 20, scroll: 0, followed: (Cursor::default(), 0, 0, 0), area: Rect::default(), dragging: false,
        })
    }

    pub fn html(&self) -> String { self.doc.html() }
    /// Byte offset of the cursor in `html()`.
    pub fn html_offset(&self) -> usize { self.doc.html_offset(self.cursor) }
    /// Put the cursor where byte offset `off` of the note's HTML is.
    pub fn place_at_html_offset(&mut self, off: usize) {
        self.cursor = Doc::cursor_at_html_offset(&self.doc.html(), off);
        self.anchor = None;
    }
    pub fn dirty(&self) -> bool { self.version != self.saved_version }
    /// Mark the session as holding edits that are not stored yet (used when switching over from the source editor).
    pub fn mark_dirty(&mut self) { self.version = self.next_version; self.next_version += 1; }

    pub fn saved(&mut self, stored: String) {
        self.saved_version = self.version;
        self.base = stored;
        self.force = false;
        self.msg = "saved".into();
        // typing after a save is a new undo step, so undo can return to exactly what was saved
        self.last = Last::Other;
    }

    pub fn needs_overwrite_confirmation(&mut self, stored: &str) -> bool {
        if stored == self.base || self.force { return false; }
        self.force = true;
        self.mode = Mode::Edit;
        self.msg = "⚠ this note changed elsewhere (sync?) since you opened it — Ctrl+S again to overwrite it".into();
        true
    }

    // ---------------------------------------------------------------- layout & positions

    fn ensure_layout(&mut self, resolver: Option<Resolver>, host: Option<&mut dyn ImageHost>) {
        let mut none = NoImages;
        let (host, have): (&mut dyn ImageHost, bool) = match host { Some(h) => (h, true), None => (&mut none, false) };
        let want_inline = have.then(|| host.enabled());
        let stale = match &self.layout {
            None => true,
            Some(l) => l.width != self.width || (resolver.is_some() && !l.complete) || want_inline.is_some_and(|w| w != l.inline),
        };
        if !stale { return; }
        let ol = self.doc.outline();
        let max_rows = (self.height * 2 / 3).clamp(4, 30);
        self.layout = Some(Layout::build(&self.doc, &ol, self.width, &mut self.opaque, resolver, host, max_rows));
        self.cursor = self.doc.clamp(self.cursor, &ol);
        if let Some(a) = self.anchor { self.anchor = Some(self.doc.clamp(a, &ol)); }
    }

    fn lay(&mut self) -> &Layout {
        self.ensure_layout(None, None);
        self.layout.as_ref().unwrap()
    }

    fn sel(&self) -> Option<(Cursor, Cursor)> {
        let a = self.anchor?;
        if a == self.cursor { None } else if a < self.cursor { Some((a, self.cursor)) } else { Some((self.cursor, a)) }
    }

    fn block_len(&self, block: usize) -> usize {
        let ol = self.doc.outline();
        ol.blocks.get(block).map_or(0, |b| self.doc.block_len(&b.block))
    }

    fn move_to(&mut self, c: Cursor, extend: bool) {
        if extend { if self.anchor.is_none() { self.anchor = Some(self.cursor); } } else { self.anchor = None; }
        self.cursor = c;
        self.goal = None;
        self.pending.clear();
        self.last = Last::Other;
    }

    fn blocks_count(&self) -> usize { self.doc.outline().blocks.len() }

    fn left(&mut self, extend: bool) {
        if let (Some((a, _)), false) = (self.sel(), extend) { self.move_to(a, false); return; }
        let c = self.cursor;
        let n = if c.off > 0 { Cursor { block: c.block, off: c.off - 1 } } else if c.block > 0 { Cursor { block: c.block - 1, off: self.block_len(c.block - 1) } } else { c };
        self.move_to(n, extend);
    }

    fn right(&mut self, extend: bool) {
        if let (Some((_, b)), false) = (self.sel(), extend) { self.move_to(b, false); return; }
        let c = self.cursor;
        let len = self.block_len(c.block);
        let n = if c.off < len { Cursor { block: c.block, off: c.off + 1 } } else if c.block + 1 < self.blocks_count() { Cursor { block: c.block + 1, off: 0 } } else { c };
        self.move_to(n, extend);
    }

    fn vertical(&mut self, rows: isize, extend: bool) {
        let cur = self.cursor;
        let goal = self.goal;
        let target = {
            let lay = self.lay();
            let (row, col) = lay.locate(cur);
            let want = goal.unwrap_or(col);
            let step = rows.signum();
            let (mut r, mut left) = (row as isize, rows.abs());
            let mut found = None;
            while left > 0 {
                r += step;
                if r < 0 || r as usize >= lay.rows.len() { break; }
                if lay.is_text(r as usize) { left -= 1; found = Some(r as usize); }
            }
            match found {
                Some(r) => (lay.at(r, want), Some(want)),
                None => (None, Some(want)),
            }
        };
        match target {
            (Some(c), want) => { self.move_to(c, extend); self.goal = want; }
            // already on the first/last line: go to the very start/end
            (None, _) => { let c = if rows < 0 { Cursor::default() } else { self.doc.end_cursor() }; self.move_to(c, extend); }
        }
    }

    fn row_edge(&mut self, end: bool, extend: bool) {
        let cur = self.cursor;
        let c = {
            let lay = self.lay();
            match lay.find_line(cur) {
                Some((_, l, _, _)) => Cursor { block: l.block, off: if end { if l.last { l.u1 } else { l.vis } } else { l.u0 } },
                None => cur,
            }
        };
        self.move_to(c, extend);
    }

    // ---------------------------------------------------------------- mutation

    /// Run an edit; it becomes one undo step only if it changed the document (typing/deleting in a row share a step).
    fn mutate(&mut self, kind: Last, f: impl FnOnce(&mut Doc, Cursor, Option<Cursor>) -> Cursor) {
        let before = self.doc.nodes.clone();
        let (old_c, old_a, old_version) = (self.cursor, self.anchor, self.version);
        let new = f(&mut self.doc, self.cursor, self.anchor);
        let ol = self.doc.outline();
        self.cursor = self.doc.clamp(new, &ol);
        self.anchor = None;
        if self.doc.nodes == before { return; }
        let continuing = kind != Last::Other && self.last == kind && self.last_end == Some(old_c) && old_a.is_none();
        if !continuing {
            self.undo.push(Snap { nodes: before, cursor: old_c, version: old_version });
            if self.undo.len() > 300 { self.undo.remove(0); }
            self.redo.clear();
        }
        self.last = kind;
        self.version = self.next_version;
        self.next_version += 1;
        self.layout = None;
        self.goal = None;
        self.last_end = Some(self.cursor);
    }

    fn restore(&mut self, snap: Snap) -> Snap {
        let cur = Snap { nodes: std::mem::replace(&mut self.doc.nodes, snap.nodes), cursor: self.cursor, version: self.version };
        self.cursor = snap.cursor;
        self.anchor = None;
        self.version = snap.version;
        self.layout = None;
        self.pending.clear();
        self.last = Last::Other;
        cur
    }

    fn undo(&mut self) {
        match self.undo.pop() {
            Some(s) => { let cur = self.restore(s); self.redo.push(cur); }
            None => self.msg = "nothing to undo".into(),
        }
    }

    fn redo(&mut self) {
        match self.redo.pop() {
            Some(s) => { let cur = self.restore(s); self.undo.push(cur); }
            None => self.msg = "nothing to redo".into(),
        }
    }

    fn delete_selection_in(doc: &mut Doc, c: Cursor, a: Option<Cursor>) -> Cursor {
        match a { Some(a) if a != c => doc.delete_range(a, c), _ => c }
    }

    fn type_text(&mut self, s: &str, from_key: bool) {
        let pending = self.pending.clone();
        self.mutate(Last::Typing, |d, c, a| {
            let c = Self::delete_selection_in(d, c, a);
            let n = d.insert_text(c, s);
            for (flag, on) in &pending { d.set_mark(c, n, *flag, *on); }
            if from_key && s == " " { if let Some(f) = d.autoformat(n) { return f; } }
            n
        });
        self.pending = pending;
    }

    pub fn paste(&mut self, text: &str) {
        if self.mode != Mode::Edit { if self.mode != Mode::ConfirmClose { self.input.push_str(text.lines().next().unwrap_or("")); } return; }
        let text: String = text.replace("\r\n", "\n").replace('\r', "\n").chars().filter(|c| *c == '\n' || *c == '\t' || !c.is_control()).collect();
        if text.is_empty() { return; }
        self.mutate(Last::Other, |d, c, a| {
            let mut c = Self::delete_selection_in(d, c, a);
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 { c = d.split(c); }
                c = d.insert_text(c, line);
            }
            c
        });
    }

    fn word_delete(&mut self, back: bool) {
        self.mutate(Last::Delete, |d, c, a| {
            if let Some(a) = a.filter(|a| *a != c) { return d.delete_range(a, c); }
            let t = if back { rt::word_back(d, c) } else { rt::word_forward(d, c) };
            let (lo, hi) = if t < c { (t, c) } else { (c, t) };
            d.delete_range(lo, hi)
        });
    }

    fn copy(&mut self, cut: bool) {
        if let Some((a, b)) = self.sel() {
            let t = self.doc.selected_text(a, b);
            system_copy(&t);
            self.clip = t;
            if cut { self.mutate(Last::Other, |d, _, _| d.delete_range(a, b)); }
            self.msg = if cut { "cut".into() } else { "copied".into() };
        }
    }

    fn toggle(&mut self, flag: u16, name: &str) {
        match self.sel() {
            Some((a, b)) => {
                self.mutate(Last::Other, |d, _, _| { d.toggle_mark(a, b, flag); b });
                // keep the selection so several formats can be applied in a row
                self.anchor = Some(a);
                self.cursor = b;
            }
            None => {
                // nothing selected: the next typed text gets (or loses) the format
                let c = self.cursor;
                let prev = Cursor { block: c.block, off: c.off.saturating_sub(1) };
                let has = c.off > 0 && self.doc.marks_in(prev, c) & flag != 0;
                let now = self.pending.iter().find(|(f, _)| *f == flag).map_or(has, |(_, on)| *on);
                self.pending.retain(|(f, _)| *f != flag);
                self.pending.push((flag, !now));
                self.msg = format!("{name} {}", if now { "off" } else { "on" });
            }
        }
    }

    fn set_block(&mut self, tag: &str) {
        self.mutate(Last::Other, |d, c, _| { d.set_block_tag(c, tag); c });
    }

    fn list(&mut self, kind: ListKind) {
        self.mutate(Last::Other, |d, c, _| d.toggle_list(c, kind).unwrap_or(c));
    }

    fn find_next(&mut self) {
        if self.last_find.is_empty() { self.msg = "nothing to find".into(); return; }
        let from = self.sel().map(|(_, b)| b).unwrap_or(self.cursor);
        match self.doc.find(&self.last_find, from) {
            Some((a, b)) => { self.anchor = Some(a); self.cursor = b; self.goal = None; }
            None => self.msg = format!("'{}' not found", self.last_find),
        }
    }

    fn open_link_prompt(&mut self) {
        if let Some((a, b)) = self.sel() {
            self.input = self.doc.link_at(a).filter(|(s, e, _)| *s <= a && b <= *e).map(|(_, _, h)| h).unwrap_or_default();
        } else if let Some((s, e, h)) = self.doc.link_at(self.cursor) {
            self.anchor = Some(s);
            self.cursor = e;
            self.input = h;
        } else {
            self.input.clear();
            self.link_insert = true;
            self.mode = Mode::Link;
            return;
        }
        self.link_insert = false;
        self.mode = Mode::Link;
    }

    /// Link to a note: the selection becomes a link to it, otherwise its title is inserted as a note link.
    pub fn link_note(&mut self, id: &str, title: &str) {
        let href = format!("#root/{id}");
        match self.sel() {
            Some((a, b)) => self.mutate(Last::Other, |d, _, _| { d.set_link(a, b, Some(&href)); b }),
            None => self.mutate(Last::Other, |d, c, _| d.insert_note_link(c, id, title)),
        }
    }

    pub fn insert_image(&mut self, src: &str) {
        self.mutate(Last::Other, |d, c, a| { let c = Self::delete_selection_in(d, c, a); d.insert_image(c, src) });
    }

    // ---------------------------------------------------------------- keys

    pub fn key(&mut self, ev: KeyEvent) -> Outcome {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
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
                    KeyCode::Enter => { self.last_find = self.input.clone(); self.mode = Mode::Edit; self.find_next(); }
                    KeyCode::Backspace => { self.input.pop(); }
                    KeyCode::Char(c) if !ctrl && !alt && !other => self.input.push(c),
                    _ => {}
                }
                return Outcome::None;
            }
            Mode::Link => {
                match ev.code {
                    KeyCode::Esc => self.mode = Mode::Edit,
                    KeyCode::Enter => {
                        let href = self.input.trim().to_string();
                        if self.link_insert {
                            if !href.is_empty() { self.mutate(Last::Other, |d, c, _| d.insert_link_text(c, &href, &href)); }
                        } else if let Some((a, b)) = self.sel() {
                            self.mutate(Last::Other, |d, _, _| { d.set_link(a, b, (!href.is_empty()).then_some(href.as_str())); b });
                        }
                        self.mode = Mode::Edit;
                    }
                    KeyCode::Backspace => { self.input.pop(); }
                    KeyCode::Char(c) if !ctrl && !alt && !other => self.input.push(c),
                    _ => {}
                }
                return Outcome::None;
            }
            Mode::Edit => {}
        }
        if self.blocks_count() == 0 { return if matches!(ev.code, KeyCode::Esc) { Outcome::Close } else { Outcome::None }; }
        match (ev.code, ctrl) {
            (KeyCode::Char('s'), true) => return Outcome::Save,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), true) => {
                if self.dirty() { self.mode = Mode::ConfirmClose; return Outcome::None; }
                return Outcome::Close;
            }
            (KeyCode::F(2), _) | (KeyCode::Char('e'), true) => return Outcome::ToSource,
            (KeyCode::Char('z'), true) if !shift => self.undo(),
            (KeyCode::Char('y'), true) | (KeyCode::Char('z'), true) | (KeyCode::Char('Z'), true) => self.redo(),
            (KeyCode::Char('a'), true) => { self.anchor = Some(Cursor::default()); self.cursor = self.doc.end_cursor(); }
            (KeyCode::Char('c'), true) => self.copy(false),
            (KeyCode::Char('x'), true) => self.copy(true),
            (KeyCode::Char('v'), true) => { let t = system_paste().filter(|t| !t.is_empty()).unwrap_or_else(|| self.clip.clone()); self.paste(&t); }
            (KeyCode::Insert, _) if shift => { let t = system_paste().filter(|t| !t.is_empty()).unwrap_or_else(|| self.clip.clone()); self.paste(&t); }
            (KeyCode::Char('b'), true) => self.toggle(mark::BOLD, "bold"),
            (KeyCode::Char('i'), true) => self.toggle(mark::ITALIC, "italic"),
            (KeyCode::Char('u'), true) => self.toggle(mark::UNDERLINE, "underline"),
            (KeyCode::Char('k'), true) => self.open_link_prompt(),
            (KeyCode::Char('o'), true) | (KeyCode::Enter, true) => {
                return match self.doc.target_at(self.cursor) {
                    Some(t) => Outcome::Follow(t),
                    None => { self.msg = "no link or picture at the cursor".into(); Outcome::None }
                };
            }
            (KeyCode::Char('f'), true) => { self.mode = Mode::Find; self.input = self.sel().map(|(a, b)| self.doc.selected_text(a, b)).filter(|s| !s.contains('\n')).unwrap_or_else(|| self.last_find.clone()); }
            (KeyCode::F(3), _) => self.find_next(),
            (KeyCode::Char(' '), true) => { let c = self.cursor; self.mutate(Last::Other, |d, _, _| { d.toggle_checkbox(c); c }); }
            (KeyCode::Char('w'), true) | (KeyCode::Backspace, true) => self.word_delete(true),
            (KeyCode::Delete, true) => self.word_delete(false),
            (KeyCode::Left, true) => { let t = rt::word_back(&self.doc, self.cursor); self.move_to(t, shift); }
            (KeyCode::Right, true) => { let t = rt::word_forward(&self.doc, self.cursor); self.move_to(t, shift); }
            (KeyCode::Left, false) if alt => { let t = rt::word_back(&self.doc, self.cursor); self.move_to(t, shift); }
            (KeyCode::Right, false) if alt => { let t = rt::word_forward(&self.doc, self.cursor); self.move_to(t, shift); }
            (KeyCode::Left, _) => self.left(shift),
            (KeyCode::Right, _) => self.right(shift),
            (KeyCode::Up, _) => self.vertical(-1, shift),
            (KeyCode::Down, _) => self.vertical(1, shift),
            (KeyCode::PageUp, _) => { let n = self.height.saturating_sub(1).max(1) as isize; self.vertical(-n, shift) }
            (KeyCode::PageDown, _) => { let n = self.height.saturating_sub(1).max(1) as isize; self.vertical(n, shift) }
            (KeyCode::Home, true) => self.move_to(Cursor::default(), shift),
            (KeyCode::End, true) => { let e = self.doc.end_cursor(); self.move_to(e, shift) }
            (KeyCode::Home, _) => self.row_edge(false, shift),
            (KeyCode::End, _) => self.row_edge(true, shift),
            (KeyCode::Enter, _) if shift || alt => self.mutate(Last::Other, |d, c, a| { let c = Self::delete_selection_in(d, c, a); d.soft_break(c) }),
            (KeyCode::Enter, _) => self.mutate(Last::Other, |d, c, a| { let c = Self::delete_selection_in(d, c, a); d.split(c) }),
            (KeyCode::Backspace, _) => self.mutate(Last::Delete, |d, c, a| match a.filter(|a| *a != c) { Some(a) => d.delete_range(a, c), None => d.delete_back(c) }),
            (KeyCode::Delete, _) => self.mutate(Last::Delete, |d, c, a| match a.filter(|a| *a != c) { Some(a) => d.delete_range(a, c), None => d.delete_forward(c) }),
            (KeyCode::Tab, _) => {
                let c = self.cursor;
                let pre = self.layout.as_ref().map_or(false, |l| l.pre.get(c.block).copied().unwrap_or(false));
                if pre { self.type_text("    ", false); }
                else { self.mutate(Last::Other, |d, c, _| { d.indent(c); c }); }
            }
            (KeyCode::BackTab, _) => self.mutate(Last::Other, |d, c, _| d.outdent(c).unwrap_or(c)),
            (KeyCode::Char(ch), false) if alt && !other => match ch {
                '0' => self.set_block("p"),
                '1'..='6' => self.set_block(&format!("h{ch}")),
                'u' => self.list(ListKind::Bullet),
                'o' => self.list(ListKind::Numbered),
                't' => self.list(ListKind::Todo),
                'q' => self.mutate(Last::Other, |d, c, _| { d.quote(c); c }),
                'l' => return Outcome::PickNote,
                'p' => return Outcome::PickImage,
                'v' => return Outcome::ToggleImages,
                'i' => self.toggle(mark::ITALIC, "italic"),
                's' => self.toggle(mark::STRIKE, "strikethrough"),
                'c' => self.toggle(mark::CODE, "code"),
                'h' => self.toggle(mark::HIGHLIGHT, "highlight"),
                'x' => { let c = self.cursor; self.mutate(Last::Other, |d, _, _| { d.toggle_checkbox(c); c }); }
                _ => {}
            },
            (KeyCode::Char(ch), false) if !alt && !other => { let mut b = [0u8; 4]; self.type_text(ch.encode_utf8(&mut b), true) }
            _ => {}
        }
        Outcome::None
    }

    // ---------------------------------------------------------------- mouse

    pub fn mouse(&mut self, ev: MouseEvent) {
        let a = self.area;
        let inside = ev.column >= a.x && ev.column < a.x + a.width && ev.row >= a.y && ev.row < a.y + a.height;
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::Down(MouseButton::Left) if inside && self.mode == Mode::Edit => {
                let (row, col) = (self.scroll + (ev.row - a.y) as usize, (ev.column - a.x) as usize);
                let hit = self.hit(row, col);
                if let Some((c, toggle)) = hit {
                    if toggle { self.mutate(Last::Other, |d, c, _| { d.toggle_checkbox(c); c }); }
                    self.move_to(c, false);
                    self.dragging = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging && self.mode == Mode::Edit => {
                let row = (self.scroll as isize + ev.row as isize - a.y as isize).max(0) as usize;
                let col = (ev.column as isize - a.x as isize).max(0) as usize;
                if let Some((c, _)) = self.hit(row, col) {
                    if self.anchor.is_none() { self.anchor = Some(self.cursor); }
                    self.cursor = c;
                    self.goal = None;
                }
            }
            MouseEventKind::Up(_) => self.dragging = false,
            _ => {}
        }
    }

    /// The cursor for a click at (row, col) of the laid-out text; `true` when it hit a to-do marker.
    fn hit(&mut self, row: usize, col: usize) -> Option<(Cursor, bool)> {
        let lay = self.lay();
        let mut r = row.min(lay.rows.len().saturating_sub(1));
        // blank/object rows: use the nearest text row above, else below
        while r > 0 && !lay.is_text(r) { r -= 1; }
        if !lay.is_text(r) { r = (0..lay.rows.len()).find(|i| lay.is_text(*i))?; }
        let toggle = r == row && lay.checkbox_at(row, col);
        Some((lay.at(r, col)?, toggle))
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let total = self.layout.as_ref().map_or(0, |l| l.rows.len()) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, (total - 1).max(0)) as usize;
    }

    // ---------------------------------------------------------------- drawing

    pub fn draw(&mut self, f: &mut Frame, area: Rect, resolver: Resolver, host: &mut dyn ImageHost) {
        let accent = Style::default().fg(Color::Cyan);
        let dim = Style::default().fg(Color::DarkGray);
        let [text_r, foot_r] = RLayout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        self.area = text_r;
        self.width = (text_r.width as usize).max(8);
        self.height = text_r.height as usize;
        self.ensure_layout(Some(resolver), Some(&mut *host));
        let lay = self.layout.as_ref().unwrap();
        let (crow, ccol) = lay.locate(self.cursor);
        // keep the cursor in view only when it (or the window) moved, so the mouse wheel can scroll away from it
        let marker = (self.cursor, self.version, self.width, self.height);
        if marker != self.followed {
            if crow < self.scroll { self.scroll = crow; }
            if self.height > 0 && crow >= self.scroll + self.height { self.scroll = crow + 1 - self.height; }
            self.followed = marker;
        }
        self.scroll = self.scroll.min(lay.rows.len().saturating_sub(1));
        let sel = self.sel();
        let in_sel = |c: Cursor| sel.is_some_and(|(a, b)| c >= a && c < b);
        // a line of a block: its prefix, then its units styled by heading/quote/marks, selection reversed
        let line_spans = |l: &LineRef, tail: bool| -> Vec<Span<'static>> {
            let mut spans = l.prefix.clone();
            let units = &lay.units[l.block];
            let (base, pre) = (lay.styles[l.block], lay.pre[l.block]);
            let (mut run, mut run_style) = (String::new(), Style::default());
            for (i, u) in units.iter().enumerate().take(l.vis).skip(l.u0) {
                let (t, _) = cell(u, pre);
                let mut st = base.patch(marks_style(u.marks));
                if u.label.is_some() { st = st.fg(Color::Magenta); }
                if in_sel(Cursor { block: l.block, off: i }) { st = st.add_modifier(Modifier::REVERSED); }
                if st != run_style && !run.is_empty() { spans.push(Span::styled(std::mem::take(&mut run), run_style)); }
                run_style = st;
                run.push_str(&t);
            }
            if !run.is_empty() { spans.push(Span::styled(run, run_style)); }
            // a selection running on past this block's end shows as a highlighted cell
            if tail && (l.last && sel.is_some_and(|(_, b)| b.block > l.block) || (!l.last && l.vis < l.u1 && in_sel(Cursor { block: l.block, off: l.vis }))) {
                spans.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
            }
            spans
        };
        let mut out: Vec<Line> = Vec::with_capacity(self.height);
        for row in lay.rows.iter().skip(self.scroll).take(self.height) {
            match row {
                RowKind::Plain { prefix, line } => {
                    let mut spans = prefix.clone();
                    spans.extend(line.spans.iter().cloned());
                    out.push(Line::from(spans));
                }
                RowKind::Text(l) => out.push(Line::from(line_spans(l, true))),
                RowKind::Cells(segs) => {
                    let mut spans: Vec<Span<'static>> = vec![];
                    for (j, seg) in segs.iter().enumerate() {
                        if j > 0 { spans.push(Span::styled(" │ ", dim)); }
                        let cell_spans = clip_spans(seg.line.as_ref().map(|l| line_spans(l, false)).unwrap_or_default(), seg.w);
                        let used: usize = cell_spans.iter().map(|s| s.content.width()).sum();
                        spans.extend(cell_spans);
                        if j + 1 < segs.len() { spans.push(spaces(seg.w.saturating_sub(used))); }
                    }
                    out.push(Line::from(spans));
                }
            }
        }
        f.render_widget(Paragraph::new(out), text_r);
        // pictures go over the rows reserved for them; one that is only partly on screen waits until it fits
        for pl in &lay.images {
            if pl.row < self.scroll || pl.row + pl.rows > self.scroll + self.height || pl.x >= text_r.width as usize { continue; }
            let r = Rect { x: text_r.x + pl.x as u16, y: text_r.y + (pl.row - self.scroll) as u16, width: pl.cols.min(text_r.width as usize - pl.x) as u16, height: pl.rows as u16 };
            if let Err(e) = host.draw(f, r, &pl.src) {
                f.render_widget(Paragraph::new(Line::from(Span::styled(format!("(can't show the picture: {e})"), dim))), Rect { height: 1, ..r });
            }
        }

        let key = |k: &str, what: &str| -> Vec<Span<'static>> { vec![Span::styled(k.to_string(), accent), Span::styled(format!(" {what}  "), dim)] };
        let foot = match self.mode {
            Mode::Find => Line::from(vec![Span::styled(" Find: ", accent), Span::raw(self.input.clone()), Span::styled("▏", accent), Span::styled("   Enter next · Esc cancel", dim)]),
            Mode::Link => Line::from(vec![Span::styled(" Link to: ", accent), Span::raw(self.input.clone()), Span::styled("▏", accent), Span::styled("   Enter apply (empty removes) · Esc cancel", dim)]),
            Mode::ConfirmClose => Line::from(Span::styled(" Unsaved changes — y save & close · d discard · any other key keeps editing", Style::default().fg(Color::Yellow))),
            Mode::Edit if !self.msg.is_empty() => Line::from(vec![Span::styled(format!(" {}", self.msg), accent)]),
            Mode::Edit => {
                let mut v = vec![Span::raw(" ")];
                for (k, w) in [("F1", "help"), ("^S", "save"), ("Esc", "close"), ("^B ^U Alt+I", "format"), ("^K/Alt+L", "link"), ("Alt+P", "image"), ("F2", "source")] { v.extend(key(k, w)); }
                Line::from(v)
            }
        };
        f.render_widget(Paragraph::new(foot), foot_r);

        if self.mode == Mode::Edit && ccol < text_r.width as usize && crow >= self.scroll && crow < self.scroll + self.height {
            f.set_cursor_position((text_r.x + ccol as u16, text_r.y + (crow - self.scroll) as u16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};
    use ratatui::{Terminal, backend::TestBackend};

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent { KeyEvent { code, modifiers: m, kind: KeyEventKind::Press, state: KeyEventState::NONE } }
    fn press(s: &mut RichSession, code: KeyCode) -> Outcome { s.key(k(code, KeyModifiers::NONE)) }
    fn ctrl(s: &mut RichSession, c: char) -> Outcome { s.key(k(KeyCode::Char(c), KeyModifiers::CONTROL)) }
    fn alt(s: &mut RichSession, c: char) -> Outcome { s.key(k(KeyCode::Char(c), KeyModifiers::ALT)) }
    fn typ(s: &mut RichSession, t: &str) { for c in t.chars() { press(s, KeyCode::Char(c)); } }
    fn session(h: &str) -> RichSession { RichSession::new("n", "T", h).unwrap() }
    fn screen(s: &mut RichSession, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| s.draw(f, f.area(), &|_| None, &mut NoImages)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn typing_enter_backspace_and_save_flow_leave_the_rest_untouched() {
        let mut s = session("<h2>Title</h2><p>one</p>\r\n<p>two</p>");
        assert!(!s.dirty());
        press(&mut s, KeyCode::Down); press(&mut s, KeyCode::End);
        typ(&mut s, "!");
        assert_eq!(s.html(), "<h2>Title</h2><p>one!</p>\r\n<p>two</p>");
        assert!(s.dirty());
        press(&mut s, KeyCode::Enter);
        typ(&mut s, "new");
        assert_eq!(s.html(), "<h2>Title</h2><p>one!</p><p>new</p>\r\n<p>two</p>");
        assert_eq!(ctrl(&mut s, 's'), Outcome::Save);
        s.saved(s.html());
        assert!(!s.dirty());
        // edit after the save: Esc asks first; undoing back to the saved state makes it clean again
        typ(&mut s, "?");
        assert!(s.dirty());
        assert_eq!(press(&mut s, KeyCode::Esc), Outcome::None, "dirty: asks before closing");
        press(&mut s, KeyCode::Char('x'));
        ctrl(&mut s, 'z');
        assert!(!s.dirty());
        assert_eq!(press(&mut s, KeyCode::Esc), Outcome::Close);
    }

    #[test]
    fn undo_walks_back_to_the_clean_state_and_redo_forward() {
        let mut s = session("<p>abc</p>");
        press(&mut s, KeyCode::End);
        typ(&mut s, "def");
        press(&mut s, KeyCode::Enter);
        typ(&mut s, "x");
        assert_eq!(s.html(), "<p>abcdef</p><p>x</p>");
        ctrl(&mut s, 'z'); // typing run "x"
        assert_eq!(s.html(), "<p>abcdef</p><p>&nbsp;</p>");
        ctrl(&mut s, 'z'); // the Enter
        assert_eq!(s.html(), "<p>abcdef</p>");
        ctrl(&mut s, 'z'); // typing run "def"
        assert_eq!(s.html(), "<p>abc</p>");
        assert!(!s.dirty(), "undoing everything is clean again");
        ctrl(&mut s, 'y');
        assert_eq!(s.html(), "<p>abcdef</p>");
        assert!(s.dirty());
    }

    #[test]
    fn selection_formatting_and_pending_format() {
        let mut s = session("<p>one two</p>");
        for _ in 0..4 { press(&mut s, KeyCode::Right); }
        for _ in 0..3 { s.key(k(KeyCode::Right, KeyModifiers::SHIFT)); }
        ctrl(&mut s, 'b');
        assert_eq!(s.html(), "<p>one <strong>two</strong></p>");
        ctrl(&mut s, 'i');
        assert_eq!(s.html(), "<p>one <i><strong>two</strong></i></p>", "the selection stays so formats stack");
        alt(&mut s, 'i');
        assert_eq!(s.html(), "<p>one <strong>two</strong></p>", "Alt+I is italic too (Ctrl+I is Tab in terminals without the kitty protocol)");
        // nothing selected: Ctrl+B switches bold on for the text typed next, and Ctrl+B again would switch it off
        let mut s = session("<p>one</p>");
        press(&mut s, KeyCode::End);
        ctrl(&mut s, 'b');
        typ(&mut s, "xy");
        assert_eq!(s.html(), "<p>one<strong>xy</strong></p>");
        let mut s = session("<p><strong>bo</strong></p>");
        press(&mut s, KeyCode::End);
        ctrl(&mut s, 'b');
        typ(&mut s, "z");
        assert_eq!(s.html(), "<p><strong>bo</strong>z</p>");
    }

    #[test]
    fn markdown_triggers_tab_and_enter_in_lists() {
        let mut s = session("<p>&nbsp;</p>");
        typ(&mut s, "- one");
        press(&mut s, KeyCode::Enter);
        typ(&mut s, "two");
        press(&mut s, KeyCode::Tab);
        press(&mut s, KeyCode::Enter);
        typ(&mut s, "three");
        s.key(k(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(s.html(), "<ul><li>one<ul><li>two</li></ul></li><li>three</li></ul>");
        press(&mut s, KeyCode::Enter);
        press(&mut s, KeyCode::Enter);
        assert!(s.html().ends_with("</ul><p>&nbsp;</p>"), "{}", s.html());
    }

    #[test]
    fn headings_links_and_find() {
        let mut s = session("<p>see docs here</p><p>more docs</p>");
        alt(&mut s, '2');
        assert_eq!(s.html(), "<h2>see docs here</h2><p>more docs</p>");
        ctrl(&mut s, 'f'); typ(&mut s, "docs"); press(&mut s, KeyCode::Enter);
        assert_eq!(s.sel().map(|(a, b)| (a.off, b.off)), Some((4, 8)));
        ctrl(&mut s, 'k'); typ(&mut s, "http://x"); press(&mut s, KeyCode::Enter);
        assert_eq!(s.html(), "<h2>see <a href=\"http://x\">docs</a> here</h2><p>more docs</p>");
        press(&mut s, KeyCode::F(3));
        assert_eq!(s.sel().map(|(a, b)| (a.block, a.off, b.off)), Some((1, 5, 9)));
    }

    #[test]
    fn to_do_items_toggle_with_keys_and_mouse() {
        let mut s = session("<ul class=\"todo-list\"><li><label class=\"todo-list__label\"><input type=\"checkbox\" disabled=\"disabled\"><span class=\"todo-list__label__description\">task</span></label></li></ul>");
        let shown = screen(&mut s, 40, 6);
        assert!(shown.starts_with("☐ task"), "{shown}");
        ctrl(&mut s, ' ');
        assert!(screen(&mut s, 40, 6).starts_with("☑ task"));
        s.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 0, row: 0, modifiers: KeyModifiers::NONE });
        assert!(screen(&mut s, 40, 6).starts_with("☐ task"), "clicking the marker toggles it");
    }

    #[test]
    fn drawing_wraps_text_shows_no_tags_and_keeps_the_cursor_on_screen() {
        let long = "word ".repeat(40);
        let mut s = session(&format!("<h1>Head</h1><p>{long}</p><ul><li>a</li><li>b<ul><li>c</li></ul></li></ul><blockquote><p>quoted</p></blockquote><table><tr><td>x</td></tr></table><p>end</p>"));
        let shown = screen(&mut s, 30, 24);
        assert!(!shown.contains('<') && !shown.contains("&nbsp;"), "{shown}");
        assert!(shown.contains("• a") && shown.contains("• c") && shown.contains("│ quoted"), "{shown}");
        ctrl(&mut s, 'e');
        s.key(k(KeyCode::End, KeyModifiers::CONTROL));
        let shown = screen(&mut s, 30, 5);
        assert!(shown.contains("end"), "{shown}");
        for (w, h) in [(1u16, 1u16), (3, 2), (10, 2), (40, 3)] { screen(&mut s, w, h); typ(&mut s, "x"); screen(&mut s, w, h); }
    }

    #[test]
    fn up_and_down_keep_the_column_across_wrapped_and_short_lines() {
        let mut s = session("<p>abcdefghij</p><p>xy</p><p>0123456789</p>");
        screen(&mut s, 40, 10);
        for _ in 0..6 { press(&mut s, KeyCode::Right); }
        press(&mut s, KeyCode::Down);
        assert_eq!((s.cursor.block, s.cursor.off), (1, 2), "clamped to the short line");
        press(&mut s, KeyCode::Down);
        assert_eq!((s.cursor.block, s.cursor.off), (2, 6), "and the wanted column comes back");
        press(&mut s, KeyCode::Up); press(&mut s, KeyCode::Up);
        assert_eq!((s.cursor.block, s.cursor.off), (0, 6));
        press(&mut s, KeyCode::Up);
        assert_eq!((s.cursor.block, s.cursor.off), (0, 0), "Up on the first line goes to the start");
    }

    #[test]
    fn clicks_and_paste_and_close_prompt() {
        let mut s = session("<p>hello</p><p>world</p>");
        screen(&mut s, 40, 10);
        s.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 2, modifiers: KeyModifiers::NONE });
        assert_eq!((s.cursor.block, s.cursor.off), (1, 3));
        s.paste("A\nB\tC\u{1b}");
        assert_eq!(s.html(), "<p>hello</p><p>worA</p><p>B&nbsp;&nbsp;&nbsp;&nbsp;Cld</p>");
        assert_eq!(press(&mut s, KeyCode::Esc), Outcome::None);
        assert_eq!(press(&mut s, KeyCode::Char('d')), Outcome::Close);
    }

    const TABLE: &str = "<p>before</p><figure class=\"table\"><table><thead><tr><th>Name</th><th>Qty</th></tr></thead><tbody><tr><td>apple</td><td>3</td></tr><tr><td>a rather long pear name</td><td>&nbsp;</td></tr></tbody></table></figure><p>after</p>";

    #[test]
    fn tables_draw_as_a_grid_and_cells_edit_in_place() {
        let mut s = session(TABLE);
        let shown = screen(&mut s, 60, 14);
        assert!(shown.contains("Name") && shown.contains(" │ Qty") && shown.contains("apple") && shown.contains("─┼─"), "{shown}");
        assert!(!shown.contains('<'), "{shown}");
        // click into the Qty cell of the apple row, replace its text
        s.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 31, row: 4, modifiers: KeyModifiers::NONE });
        let shown = screen(&mut s, 60, 14);
        let row = shown.lines().position(|l| l.contains("apple")).unwrap() as u16;
        s.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 33, row, modifiers: KeyModifiers::NONE });
        press(&mut s, KeyCode::Backspace);
        typ(&mut s, "12");
        assert!(s.html().contains("<td>apple</td><td>12</td>"), "{}", s.html());
        // Backspace at the start of a cell does nothing; so does Delete at its end
        let h = s.html();
        press(&mut s, KeyCode::Home);
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.html(), h);
        // typing into the empty cell
        s.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 35, row: row + 1, modifiers: KeyModifiers::NONE });
        typ(&mut s, "ok");
        assert!(s.html().contains("<td>ok</td>"), "{}", s.html());
        assert!(screen(&mut s, 60, 14).contains("ok"));
    }

    #[test]
    fn arrows_walk_through_cells_and_out_of_the_table() {
        let mut s = session(TABLE);
        screen(&mut s, 60, 14);
        press(&mut s, KeyCode::Down); // before -> Name
        assert_eq!((s.cursor.block, s.cursor.off), (1, 0));
        press(&mut s, KeyCode::Right); press(&mut s, KeyCode::Right); press(&mut s, KeyCode::Right); press(&mut s, KeyCode::Right);
        assert_eq!((s.cursor.block, s.cursor.off), (1, 4), "end of the first cell");
        press(&mut s, KeyCode::Right);
        assert_eq!((s.cursor.block, s.cursor.off), (2, 0), "next cell, not a join");
        press(&mut s, KeyCode::Down);
        assert_eq!(s.cursor.block, 4, "down stays in the Qty column");
        press(&mut s, KeyCode::Down);
        assert_eq!(s.cursor.block, 6, "and into the next row");
        press(&mut s, KeyCode::Down);
        assert_eq!(s.cursor.block, 7, "then out of the table");
        press(&mut s, KeyCode::Up); press(&mut s, KeyCode::Up);
        assert_eq!(s.cursor.block, 4);
        press(&mut s, KeyCode::Enter);
        typ(&mut s, "x");
        assert!(s.html().contains("<td><br>x3</td>"), "Enter in a cell is a line break: {}", s.html());
    }

    /// A host that knows one picture and records where it was asked to draw it.
    struct FakeHost { on: bool, drawn: Vec<Rect> }
    impl ImageHost for FakeHost {
        fn enabled(&self) -> bool { self.on }
        fn font(&self) -> (u16, u16) { (10, 20) }
        fn dims(&mut self, src: &str) -> Option<(u32, u32)> { (src == "pic.png").then_some((200, 100)) } // 20 x 5 cells
        fn draw(&mut self, _: &mut Frame, area: Rect, _: &str) -> Result<(), String> { self.drawn.push(area); Ok(()) }
    }

    #[test]
    fn inline_pictures_reserve_rows_that_the_cursor_skips() {
        let html = "<p>top</p><figure class=\"image\"><img src=\"pic.png\"></figure><p>bottom</p><p><img src=\"other.png\"></p>";
        let mut s = session(html);
        let mut host = FakeHost { on: true, drawn: vec![] };
        let mut t = Terminal::new(TestBackend::new(40, 20)).unwrap();
        t.draw(|f| s.draw(f, f.area(), &|_| None, &mut host)).unwrap();
        let shown: Vec<String> = { let b = t.backend().buffer().clone(); (0..20).map(|y| (0..40).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect() };
        assert_eq!(host.drawn.len(), 1, "only the picture the host knows is drawn (the other stays a label)");
        let r = host.drawn[0];
        assert_eq!((r.width, r.height), (20, 5));
        assert_eq!(shown[r.y as usize - 1], "[image: pic.png]", "the caption sits right above: {shown:?}");
        let bottom = shown.iter().position(|l| l == "bottom").unwrap();
        assert!(bottom as u16 >= r.y + r.height, "text continues below the reserved rows");
        assert!(shown.iter().any(|l| l == "[image: other.png]"));
        // Up/Down never rest on the filler rows: top -> caption -> bottom
        press(&mut s, KeyCode::Down);
        assert_eq!(s.cursor.block, 1);
        press(&mut s, KeyCode::Down);
        assert_eq!(s.cursor.block, 2);
        // a picture taller than what is left of the window waits
        let mut small = session(html);
        let mut host = FakeHost { on: true, drawn: vec![] };
        let mut t = Terminal::new(TestBackend::new(40, 5)).unwrap();
        t.draw(|f| small.draw(f, f.area(), &|_| None, &mut host)).unwrap();
        assert!(host.drawn.is_empty(), "a window too short for the whole picture shows none of it");
        // off: labels only, no reserved rows
        let mut off = FakeHost { on: false, drawn: vec![] };
        let mut t = Terminal::new(TestBackend::new(40, 20)).unwrap();
        t.draw(|f| s.draw(f, f.area(), &|_| None, &mut off)).unwrap();
        assert!(off.drawn.is_empty());
        let b = t.backend().buffer().clone();
        let rows: Vec<String> = (0..20).map(|y| (0..40).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
        let (cap, bot) = (rows.iter().position(|l| l == "[image: pic.png]").unwrap(), rows.iter().position(|l| l == "bottom").unwrap());
        assert!(bot <= cap + 3, "without pictures the text closes up again: {rows:?}");
    }

    #[test]
    fn hostile_text_never_reaches_a_cell_as_a_control_character() {
        let mut s = session("<p>a\u{1b}[31mb\u{9b}c<img alt=\"x\u{1b}y\" src=\"p.png\"></p>");
        let shown = screen(&mut s, 40, 5);
        assert!(!shown.chars().any(|c| c.is_control() && c != '\n'), "{shown:?}");
    }

    /// Opt-in: `RICH_SHOW=note.html cargo test -p trilium-tui show_note -- --ignored --nocapture` prints the in-place view of a note.
    #[test]
    #[ignore]
    fn show_note() {
        let Ok(path) = std::env::var("RICH_SHOW") else { return };
        let html = std::fs::read_to_string(path).unwrap();
        let mut s = RichSession::new("n", "Title", &html).unwrap();
        let (w, h) = (std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(90), std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(34));
        println!("{}", screen(&mut s, w, h));
    }

    /// Opt-in timing: `RICH_SHOW=big.html cargo test --release -p trilium-tui type_speed -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn type_speed() {
        let Ok(path) = std::env::var("RICH_SHOW") else { return };
        let html = std::fs::read_to_string(path).unwrap();
        let mut s = RichSession::new("n", "Title", &html).unwrap();
        screen(&mut s, 100, 40);
        let blocks = s.blocks_count();
        let t = std::time::Instant::now();
        for _ in 0..(blocks / 2) { press(&mut s, KeyCode::Down); }
        let nav = t.elapsed() / (blocks / 2).max(1) as u32;
        let t = std::time::Instant::now();
        let n = 40;
        for i in 0..n { press(&mut s, KeyCode::Char(if i % 2 == 0 { 'a' } else { 'b' })); screen(&mut s, 100, 40); }
        println!("{} KB, {} blocks: {:?} per Down key, {:?} per typed key + redraw", html.len() / 1000, blocks, nav, t.elapsed() / n);
    }

    /// Opt-in: `RICH_NOTES_DIR=dir cargo test --release -p trilium-tui real_notes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_notes_lay_out_consistently_and_survive_navigation() {
        let Ok(dir) = std::env::var("RICH_NOTES_DIR") else { return };
        let (mut notes, mut positions, mut clipped) = (0usize, 0usize, 0usize);
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let html = std::fs::read_to_string(e.path()).unwrap();
            if rt::unsupported_reason(&html).is_some() { continue; }
            let Some(mut s) = RichSession::new("n", "T", &html) else { continue };
            notes += 1;
            for w in [20u16, 45, 90] {
                let mut t = Terminal::new(TestBackend::new(w, 12)).unwrap();
                t.draw(|f| s.draw(f, f.area(), &|_| None, &mut NoImages)).unwrap();
                let ol = s.doc.outline();
                let lay = s.layout.as_ref().unwrap();
                for (i, bi) in ol.blocks.iter().enumerate() {
                    for off in 0..=s.doc.block_len(&bi.block) {
                        let c = Cursor { block: i, off };
                        let (r, col) = lay.locate(c);
                        assert!(lay.find_line(c).is_some(), "{}: no line for {c:?} at width {w}", e.path().display());
                        let back = lay.at(r, col).unwrap_or_else(|| panic!("{}: no cursor at ({r},{col}) for {c:?}", e.path().display()));
                        assert_eq!(back.block, c.block, "{}: {c:?} -> ({r},{col}) leaked into {back:?} at width {w}", e.path().display());
                        // positions that were not squeezed into a narrow table column map back exactly
                        if lay.locate(back) != (r, col) { clipped += 1; }
                        positions += 1;
                    }
                }
            }
            // walk the whole document with Down and PageDown, then type a little at the end
            let mut t = Terminal::new(TestBackend::new(60, 14)).unwrap();
            for step in 0..400 {
                press(&mut s, if step % 7 == 6 { KeyCode::PageDown } else { KeyCode::Down });
                t.draw(|f| s.draw(f, f.area(), &|_| None, &mut NoImages)).unwrap();
            }
            s.key(k(KeyCode::End, KeyModifiers::CONTROL));
            typ(&mut s, "hi ");
            press(&mut s, KeyCode::Enter);
            t.draw(|f| s.draw(f, f.area(), &|_| None, &mut NoImages)).unwrap();
            ctrl(&mut s, 'z'); ctrl(&mut s, 'z');
            t.draw(|f| s.draw(f, f.area(), &|_| None, &mut NoImages)).unwrap();
        }
        eprintln!("{notes} notes, {positions} cursor positions mapped to screen cells and back ({clipped} inside clipped objects)");
    }
}
