//! Collection ("book") note views: list/grid, table, geo list, kanban board and month calendar.
//! A collection's `#viewType` label picks the view; list-like views reuse the note pane's row selection,
//! board and calendar have their own cursor and keys.
use crate::app::{App, Kind, Overlay, PromptKind};
use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewType { Grid, List, Table, Board, Calendar, GeoMap }

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn pad(s: &str, w: usize) -> String {
    let mut t = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > w.saturating_sub(if s.width() > w { 1 } else { 0 }) { break; }
        t.push(ch);
        used += cw;
    }
    if s.width() > w { t.push('…'); used += 1; }
    format!("{t}{}", " ".repeat(w.saturating_sub(used)))
}

#[cfg(test)]
pub fn month_name(m: u32) -> &'static str { MONTHS[m as usize - 1] }

pub struct BoardData {
    pub group_by: String,
    /// (column value, "" for the "no value" column; cards = note ids)
    pub columns: Vec<(String, Vec<String>)>,
}

impl App {
    pub fn view_type(&self, note: &str) -> ViewType {
        match self.db.label_value(note, "viewType").ok().flatten().unwrap_or_default().to_lowercase().as_str() {
            "list" => ViewType::List, "table" => ViewType::Table, "board" => ViewType::Board, "calendar" => ViewType::Calendar,
            "geomap" => ViewType::GeoMap, _ => ViewType::Grid,
        }
    }

    /// True when the open note is a collection shown with a view that has its own keys.
    pub fn custom_collection(&self) -> Option<ViewType> {
        let v = self.view.as_ref()?;
        if !matches!(v.kind, Kind::Collection) { return None; }
        match self.view_type(&v.id) { t @ (ViewType::Board | ViewType::Calendar) => Some(t), _ => None }
    }

    fn children_of(&self, id: &str) -> Vec<String> {
        self.model.children.get(id).into_iter().flatten().filter(|b| self.show_archived || !self.model.archived.contains(&b.note_id)).map(|b| b.note_id.clone()).collect()
    }

    fn label_of(&self, note: &str, name: &str) -> String { self.db.label_value(note, name).ok().flatten().unwrap_or_default() }

    // ------------------------------------------------------------------ list / table / geo

    fn table_columns(&self, coll: &str, kids: &[String]) -> Vec<String> {
        let promoted: Vec<String> = self.db.attribute_rows(coll).unwrap_or_default().into_iter()
            .filter(|a| a.kind == "label" && a.name.starts_with("label:") && a.value.contains("promoted"))
            .map(|a| a.name["label:".len()..].to_string()).collect();
        if !promoted.is_empty() { return promoted; }
        let mut counts: HashMap<String, usize> = HashMap::new();
        for k in kids { for a in self.db.note_attributes(k).unwrap_or_default() {
            if a.kind == "label" && !matches!(a.name.as_str(), "iconClass" | "archived" | "sorted" | "viewType" | "cssClass" | "hidePromotedAttributes") && !a.name.contains(':') { *counts.entry(a.name).or_default() += 1; }
        } }
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(4).map(|(n, _)| n).collect()
    }

    /// Lines for list, grid, table and geo views: two header lines, then one line per child.
    pub fn collection_lines(&self, id: &str, width: usize) -> Vec<Line<'static>> {
        let vt = self.view_type(id);
        let dim = Style::default().fg(Color::DarkGray);
        let kids = self.children_of(id);
        let mut out = vec![];
        match vt {
            ViewType::Table | ViewType::GeoMap => {
                let cols = if vt == ViewType::GeoMap { vec!["geolocation".to_string()] } else { self.table_columns(id, &kids) };
                let title_w = (width * 2 / 5).clamp(14, 48).min(width.saturating_sub(10));
                let rest = width.saturating_sub(title_w + 3);
                let col_w = if cols.is_empty() { 0 } else { (rest / cols.len()).max(8) };
                let mut head = vec![Span::styled(pad("Title", title_w), Style::default().add_modifier(Modifier::BOLD))];
                for c in &cols { head.push(Span::styled(format!(" │ {}", pad(c, col_w.saturating_sub(3))), Style::default().add_modifier(Modifier::BOLD))); }
                out.push(Line::from(head));
                out.push(Line::from(Span::styled("─".repeat(width.min(title_w + cols.len() * col_w + 3)), dim)));
                for k in &kids {
                    let mut row = vec![Span::raw(pad(self.model.title_of(k), title_w))];
                    for c in &cols {
                        let v = self.db.attribute_rows(k).unwrap_or_default().into_iter().find(|a| a.name == *c).map(|a| if a.kind == "relation" { self.model.title_of(&a.value).to_string() } else { a.value }).unwrap_or_default();
                        row.push(Span::styled(format!(" │ {}", pad(&v, col_w.saturating_sub(3))), if v.is_empty() { dim } else { Style::default() }));
                    }
                    out.push(Line::from(row));
                }
                if kids.is_empty() { out.push(Line::from(Span::styled("(no child notes)", dim))); }
            }
            _ => {
                out.push(Line::from(Span::styled(format!("Collection · {} child note(s) · Enter opens the selected note", kids.len()), dim)));
                out.push(Line::default());
                for k in &kids {
                    let n = &self.model.notes[k];
                    let preview = if kids.len() <= 300 && n.ntype == "text" && !n.protected {
                        let c = self.db.note_content(k).ok().flatten().unwrap_or_default();
                        let t = trilium_core::search::text::strip_tags(&c);
                        trilium_core::search::text::unescape_html(&t).split_whitespace().collect::<Vec<_>>().join(" ")
                    } else { String::new() };
                    let title = self.model.title_of(k).to_string();
                    let room = width.saturating_sub(title.width() + n.ntype.len() + 8);
                    let mut spans = vec![Span::raw("▸ "), Span::raw(title), Span::styled(format!("  [{}]", n.ntype), dim)];
                    if room > 8 && !preview.is_empty() { spans.push(Span::styled(format!("  {}", pad(&preview, room)), dim)); }
                    out.push(Line::from(spans));
                }
                if kids.is_empty() { out.push(Line::from(Span::styled("(no child notes)", dim))); }
            }
        }
        out
    }

    // ------------------------------------------------------------------ board

    pub fn board_data(&self, id: &str) -> BoardData {
        let group_by = { let g = self.label_of(id, "board:groupBy"); let g = g.trim().trim_start_matches('#').to_string(); if g.is_empty() { "status".to_string() } else { g } };
        let key = if group_by == "status" { "columns".to_string() } else { format!("{group_by}ViewColumns") };
        let mut columns: Vec<(String, Vec<String>)> = vec![];
        if let Some(cfg) = self.db.view_config(id, "board.json").ok().flatten() {
            for c in cfg.get(&key).and_then(|c| c.as_array()).into_iter().flatten() {
                if let Some(v) = c.get("value").and_then(|v| v.as_str()) { if !columns.iter().any(|(n, _)| n == v) { columns.push((v.to_string(), vec![])); } }
            }
        }
        let mut none = vec![];
        for k in self.children_of(id) {
            let v = self.label_of(&k, &group_by);
            if v.is_empty() { none.push(k); continue; }
            match columns.iter_mut().find(|(n, _)| *n == v) { Some((_, cards)) => cards.push(k), None => columns.push((v, vec![k])) }
        }
        if !none.is_empty() || columns.is_empty() { columns.insert(0, (String::new(), none)); }
        BoardData { group_by, columns }
    }

    fn board_lines(&self, id: &str, width: usize, height: usize) -> Vec<Line<'static>> {
        let b = self.board_data(id);
        let dim = Style::default().fg(Color::DarkGray);
        let (cc, cr) = self.view.as_ref().map(|v| v.board).unwrap_or((0, 0));
        let cc = cc.min(b.columns.len().saturating_sub(1));
        let per = (width / 24).clamp(1, b.columns.len().max(1));
        let first = cc.saturating_sub(per - 1).min(b.columns.len().saturating_sub(per));
        let cw = (width.saturating_sub((per - 1) * 3)) / per;
        let shown: Vec<usize> = (first..(first + per).min(b.columns.len())).collect();
        let mut out = vec![Line::from(Span::styled(format!("Board grouped by #{} · h/l column · j/k card · H/L move card · n new card · Enter open", b.group_by), dim)), Line::default()];
        let mut head = vec![];
        for (i, &ci) in shown.iter().enumerate() {
            if i > 0 { head.push(Span::styled(" │ ", dim)); }
            let (name, cards) = &b.columns[ci];
            let label = format!("{} ({})", if name.is_empty() { "(no value)" } else { name }, cards.len());
            let st = if ci == cc { Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { Style::default().add_modifier(Modifier::BOLD) };
            head.push(Span::styled(pad(&label, cw), st));
        }
        out.push(Line::from(head));
        out.push(Line::from(Span::styled("─".repeat(width), dim)));
        let rows = height.saturating_sub(5).max(1);
        let sel_row_scroll = cr.saturating_sub(rows.saturating_sub(1));
        for r in sel_row_scroll..sel_row_scroll + rows {
            let mut spans = vec![];
            let mut any = false;
            for (i, &ci) in shown.iter().enumerate() {
                if i > 0 { spans.push(Span::styled(" │ ", dim)); }
                match b.columns[ci].1.get(r) {
                    Some(card) => {
                        any = true;
                        let st = if ci == cc && r == cr { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
                        spans.push(Span::styled(pad(self.model.title_of(card), cw), st));
                    }
                    None => spans.push(Span::raw(" ".repeat(cw))),
                }
            }
            if any || r == sel_row_scroll { out.push(Line::from(spans)); }
        }
        out
    }

    fn board_key(&mut self, ev: KeyEvent) -> bool {
        let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return false };
        let b = self.board_data(&id);
        let (mut col, mut row) = self.view.as_ref().map(|v| v.board).unwrap_or((0, 0));
        col = col.min(b.columns.len().saturating_sub(1));
        row = row.min(b.columns[col].1.len().saturating_sub(1));
        let card = b.columns[col].1.get(row).cloned();
        let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
        match ev.code {
            KeyCode::Left | KeyCode::Char('h') if !shift => { col = col.saturating_sub(1); row = row.min(b.columns[col].1.len().saturating_sub(1)); }
            KeyCode::Right | KeyCode::Char('l') if !shift => { col = (col + 1).min(b.columns.len() - 1); row = row.min(b.columns[col].1.len().saturating_sub(1)); }
            KeyCode::Up | KeyCode::Char('k') => row = row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => row = (row + 1).min(b.columns[col].1.len().saturating_sub(1)),
            KeyCode::Char('H') | KeyCode::Char('L') | KeyCode::Left | KeyCode::Right => {
                let left = matches!(ev.code, KeyCode::Char('H') | KeyCode::Left);
                // target column value; None = clear the label (the "(no value)" column, even while it is hidden because it is empty)
                let target: Option<Option<String>> = if left {
                    if col > 0 { Some(Some(b.columns[col - 1].0.clone())) } else if !b.columns[0].0.is_empty() { Some(None) } else { None }
                } else { Some(col + 1).filter(|c| *c < b.columns.len()).map(|c| Some(b.columns[c].0.clone())) };
                if let (Some(card), Some(target)) = (card, target) {
                    let value = target.filter(|v| !v.is_empty());
                    match self.db.set_label_value(&card, &b.group_by, value.as_deref()) {
                        Ok(()) => {
                            self.reload(); self.poke();
                            let nd = self.board_data(&id);
                            // follow the card: its column is found by value because columns can appear or vanish
                            let want = value.unwrap_or_default();
                            col = nd.columns.iter().position(|(n, _)| *n == want).unwrap_or(0);
                            row = nd.columns.get(col).and_then(|c| c.1.iter().position(|x| *x == card)).unwrap_or(0);
                        }
                        Err(e) => self.status = format!("move failed: {e:#}"),
                    }
                }
            }
            KeyCode::Char('n') => {
                let value = b.columns[col].0.clone();
                self.open_prompt_pub(PromptKind::NewCard { board: id.clone(), group_by: b.group_by.clone(), value }, "New card title", "");
            }
            KeyCode::Enter => { if let Some(c) = card { self.open_note(&c, true, true); } return true; }
            _ => return false,
        }
        if let Some(v) = self.view.as_mut() { v.board = (col, row); }
        true
    }

    // ------------------------------------------------------------------ calendar

    fn week_start(&self) -> u32 { self.db.conn.query_row("SELECT value FROM options WHERE name='firstDayOfWeek'", [], |r| r.get::<_, String>(0)).ok().and_then(|v| v.parse().ok()).unwrap_or(1) }

    /// Items per date for the month of `cursor`: day notes (for a journal root) and events (`#startDate`..`#endDate`).
    pub fn calendar_items(&self, id: &str, cursor: NaiveDate) -> HashMap<NaiveDate, Vec<(String, bool)>> {
        let mut m: HashMap<NaiveDate, Vec<(String, bool)>> = HashMap::new();
        let month = cursor.format("%Y-%m").to_string();
        if self.db.has_label(id, "calendarRoot").unwrap_or(false) {
            for (d, note) in self.db.day_notes_in_month(&month).unwrap_or_default() {
                if let Ok(date) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") { m.entry(date).or_default().push((note, true)); }
            }
        }
        let first = NaiveDate::from_ymd_opt(cursor.year(), cursor.month(), 1).unwrap();
        let last = (first + Duration::days(32)).with_day(1).unwrap() - Duration::days(1);
        for k in self.children_of(id) {
            let Ok(start) = NaiveDate::parse_from_str(self.label_of(&k, "startDate").get(..10).unwrap_or(""), "%Y-%m-%d") else { continue };
            let end = NaiveDate::parse_from_str(self.label_of(&k, "endDate").get(..10).unwrap_or(""), "%Y-%m-%d").unwrap_or(start).max(start);
            let (from, to) = (start.max(first), end.min(last));
            let mut d = from;
            while d <= to { m.entry(d).or_default().push((k.clone(), false)); d += Duration::days(1); }
        }
        m
    }

    fn calendar_lines(&self, id: &str, width: usize, height: usize) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);
        let today = Local::now().date_naive();
        let cur = self.view.as_ref().and_then(|v| v.cal).unwrap_or(today);
        let items = self.calendar_items(id, cur);
        let ws = self.week_start();
        let first = NaiveDate::from_ymd_opt(cur.year(), cur.month(), 1).unwrap();
        let offset = (first.weekday().number_from_monday() + 7 - ws) % 7;
        let cell = (width.saturating_sub(2) / 7).clamp(4, 12);
        let names = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        let mut out = vec![Line::from(vec![Span::styled(format!("{} {}", MONTHS[cur.month0() as usize], cur.year()), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("   ←→↑↓ day/week · [ ] month · t today · Enter open · n new event", dim)]), Line::default()];
        out.push(Line::from((0..7).map(|i| Span::styled(format!("{:^cell$}", names[((ws - 1 + i) % 7) as usize]), dim)).collect::<Vec<_>>()));
        let days_in_month = ((first + Duration::days(32)).with_day(1).unwrap() - first).num_days() as u32;
        let mut day = 1u32;
        let mut week = 0;
        while day <= days_in_month && week < 6 {
            let mut spans = vec![];
            for dow in 0..7u32 {
                if (week == 0 && dow < offset) || day > days_in_month { spans.push(Span::raw(" ".repeat(cell))); continue; }
                let date = NaiveDate::from_ymd_opt(cur.year(), cur.month(), day).unwrap();
                let n = items.get(&date).map_or(0, Vec::len);
                let mark = if n == 0 { " ".to_string() } else if n == 1 { "•".to_string() } else { n.min(9).to_string() };
                let text = format!("{:^cell$}", format!("{day:>2}{mark}"));
                let mut st = Style::default();
                if n > 0 { st = st.fg(Color::Cyan); }
                if date == today { st = st.add_modifier(Modifier::BOLD | Modifier::UNDERLINED); }
                if date == cur { st = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD); }
                spans.push(Span::styled(text, st));
                day += 1;
            }
            out.push(Line::from(spans));
            week += 1;
        }
        out.push(Line::default());
        out.push(Line::from(Span::styled(format!("{}", cur.format("%A, %-d %B %Y")), Style::default().add_modifier(Modifier::BOLD))));
        let day_items = items.get(&cur).cloned().unwrap_or_default();
        if day_items.is_empty() { out.push(Line::from(Span::styled(if self.db.has_label(id, "calendarRoot").unwrap_or(false) { "No day note yet — Enter creates it" } else { "Nothing on this day — n adds an event" }, dim))); }
        for (note, is_day) in day_items.iter().take(height.saturating_sub(out.len()).max(1)) {
            out.push(Line::from(vec![Span::raw(if *is_day { "▸ " } else { "• " }), Span::raw(self.model.title_of(note).to_string())]));
        }
        out
    }

    fn calendar_key(&mut self, ev: KeyEvent) -> bool {
        let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return false };
        let today = Local::now().date_naive();
        let cur = self.view.as_ref().and_then(|v| v.cal).unwrap_or(today);
        let shift_month = |d: NaiveDate, delta: i32| -> NaiveDate {
            let total = d.year() * 12 + d.month0() as i32 + delta;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
            let last = ((NaiveDate::from_ymd_opt(y, m, 1).unwrap() + Duration::days(32)).with_day(1).unwrap() - Duration::days(1)).day();
            NaiveDate::from_ymd_opt(y, m, d.day().min(last)).unwrap()
        };
        let new = match ev.code {
            KeyCode::Left | KeyCode::Char('h') => cur - Duration::days(1),
            KeyCode::Right | KeyCode::Char('l') => cur + Duration::days(1),
            KeyCode::Up | KeyCode::Char('k') => cur - Duration::days(7),
            KeyCode::Down | KeyCode::Char('j') => cur + Duration::days(7),
            KeyCode::PageUp | KeyCode::Char('[') => shift_month(cur, -1),
            KeyCode::PageDown | KeyCode::Char(']') => shift_month(cur, 1),
            KeyCode::Char('t') => today,
            KeyCode::Char('n') => {
                self.open_prompt_pub(PromptKind::NewEvent { calendar: id, date: cur.format("%Y-%m-%d").to_string() }, "New event title", "");
                return true;
            }
            KeyCode::Enter => {
                let date = cur.format("%Y-%m-%d").to_string();
                let items = self.calendar_items(&id, cur);
                if self.db.has_label(&id, "calendarRoot").unwrap_or(false) {
                    match self.db.day_note(&date) {
                        Ok(note) => { self.reload(); self.open_note(&note, true, true); self.poke(); }
                        Err(e) => self.status = format!("{e:#}"),
                    }
                } else if let Some((note, _)) = items.get(&cur).and_then(|v| v.first()).cloned() {
                    self.open_note(&note, true, true);
                } else { self.status = "nothing on this day (n adds an event)".into(); }
                return true;
            }
            _ => return false,
        };
        if let Some(v) = self.view.as_mut() { v.cal = Some(new); }
        true
    }

    // ------------------------------------------------------------------ hooks used by the note pane

    /// Lines for the board / calendar views (drawn every frame, no cache).
    pub fn custom_collection_lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let Some(v) = self.view.as_ref() else { return vec![] };
        match self.view_type(&v.id) {
            ViewType::Board => self.board_lines(&v.id, width, height),
            ViewType::Calendar => self.calendar_lines(&v.id, width, height),
            _ => vec![],
        }
    }

    /// Keys of board / calendar views; returns true when handled.
    pub fn collection_key(&mut self, ev: KeyEvent) -> bool {
        match self.custom_collection() {
            Some(ViewType::Board) => self.board_key(ev),
            Some(ViewType::Calendar) => self.calendar_key(ev),
            _ => false,
        }
    }

    /// Finish the "new card" / "new event" prompts.
    pub fn create_collection_item(&mut self, prompt: &PromptKind, title: &str) {
        let title = if title.trim().is_empty() { "New note" } else { title.trim() };
        let r = match prompt {
            PromptKind::NewCard { board, group_by, value } => self.db.create_note(board, title, "text", "text/html", "").and_then(|id| {
                if !value.is_empty() { self.db.set_label_value(&id, group_by, Some(value))?; }
                Ok(id)
            }),
            PromptKind::NewEvent { calendar, date } => self.db.create_note(calendar, title, "text", "text/html", "").and_then(|id| {
                self.db.add_attribute(&id, "label", "startDate", date, false)?;
                Ok(id)
            }),
            _ => return,
        };
        match r {
            Ok(_) => { let id = self.view.as_ref().map(|v| v.id.clone()); self.reload(); if let Some(id) = id { self.open_note(&id, false, false); } self.poke(); self.status = "created".into(); }
            Err(e) => self.status = format!("create failed: {e:#}"),
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &Overlay) {}
