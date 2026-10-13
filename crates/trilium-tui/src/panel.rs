//! The note panel (like the web app's ribbon): attributes, backlinks, revisions, info, attachments.
use crate::app::{App, Focus, Overlay, PromptKind};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use similar::{ChangeTag, TextDiff};
use trilium_core::panels::{AttachmentInfo, AttrRow, Backlink, NoteInfo, RevInfo};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelTab { Attributes, Backlinks, Revisions, Info, Attachments }

impl PanelTab {
    pub const ALL: [PanelTab; 5] = [PanelTab::Attributes, PanelTab::Backlinks, PanelTab::Revisions, PanelTab::Info, PanelTab::Attachments];
    pub fn title(self) -> &'static str {
        match self { Self::Attributes => "Attributes", Self::Backlinks => "Backlinks", Self::Revisions => "Revisions", Self::Info => "Info", Self::Attachments => "Attachments" }
    }
}

pub struct Detail { pub rev_id: String, pub title: String, pub diff: Vec<Line<'static>>, pub text: Vec<Line<'static>>, pub show_text: bool, pub scroll: usize }

pub enum Pending { DeleteAttribute(String), Restore(String) }

pub struct Panel {
    pub note_id: String,
    pub tab: PanelTab,
    pub sel: usize,
    pub attrs: Vec<AttrRow>,
    pub backlinks: Vec<Backlink>,
    pub revs: Vec<RevInfo>,
    pub info: NoteInfo,
    pub atts: Vec<AttachmentInfo>,
    pub detail: Option<Detail>,
    pub pending: Option<(Pending, String)>,
}

impl Panel {
    fn len(&self) -> usize {
        match self.tab { PanelTab::Attributes => self.attrs.len(), PanelTab::Backlinks => self.backlinks.len(), PanelTab::Revisions => self.revs.len(),
            PanelTab::Attachments => self.atts.len(), PanelTab::Info => 0 }
    }
    pub fn count(&self, t: PanelTab) -> Option<usize> {
        match t { PanelTab::Attributes => Some(self.attrs.len()), PanelTab::Backlinks => Some(self.backlinks.len()), PanelTab::Revisions => Some(self.revs.len()),
            PanelTab::Attachments => Some(self.atts.len()), PanelTab::Info => None }
    }
}

/// Plain text used for diffs: rendered text for HTML notes, the stored text otherwise.
fn diff_text(ntype: &str, mime: &str, content: &str) -> String {
    if ntype == "text" && mime == "text/html" {
        crate::render::plain_lines(content, 100).join("\n")
    } else { content.to_string() }
}

impl App {
    pub fn open_panel(&mut self, tab: PanelTab) {
        let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return };
        let load = || -> anyhow::Result<Panel> {
            Ok(Panel {
                attrs: self.db.attribute_rows(&id)?, backlinks: self.db.backlinks(&id)?, revs: self.db.revisions(&id)?,
                info: self.db.note_info(&id)?, atts: self.db.attachments(&id)?, note_id: id.clone(), tab, sel: 0, detail: None, pending: None,
            })
        };
        match load() { Ok(p) => self.overlay = Overlay::Panel(Box::new(p)), Err(e) => self.status = format!("panel: {e:#}") }
    }

    fn refresh_panel(&mut self) {
        let Overlay::Panel(p) = &self.overlay else { return };
        let (tab, sel, id) = (p.tab, p.sel, p.note_id.clone());
        self.open_panel(tab);
        if let Overlay::Panel(p) = &mut self.overlay { p.sel = sel.min(p.len().saturating_sub(1)); }
        let _ = id;
    }

    fn open_revision_detail(&mut self) {
        let Overlay::Panel(p) = &self.overlay else { return };
        let Some(rev) = p.revs.get(p.sel).cloned() else { return };
        let Some(n) = self.model.notes.get(&p.note_id).cloned() else { return };
        let old = self.db.revision_content(&rev.id).ok().flatten().unwrap_or_default();
        let cur = self.db.note_content(&p.note_id).ok().flatten().unwrap_or_default();
        let (old_t, cur_t) = (diff_text(&n.ntype, &n.mime, &old), diff_text(&n.ntype, &n.mime, &cur));
        let diff = TextDiff::from_lines(&old_t, &cur_t);
        let mut lines = vec![];
        if rev.title != n.title {
            lines.push(Line::from(vec![Span::styled(format!("- title: {}", rev.title), Style::default().fg(Color::Red)), Span::raw("   "), Span::styled(format!("+ title: {}", n.title), Style::default().fg(Color::Green))]));
        }
        for c in diff.iter_all_changes() {
            let (sign, color) = match c.tag() { ChangeTag::Delete => ("- ", Some(Color::Red)), ChangeTag::Insert => ("+ ", Some(Color::Green)), ChangeTag::Equal => ("  ", None) };
            let text = format!("{sign}{}", c.value().trim_end_matches('\n'));
            lines.push(Line::from(Span::styled(text, color.map(|c| Style::default().fg(c)).unwrap_or_default())));
        }
        if lines.iter().all(|l| l.spans.iter().all(|s| s.content.starts_with("  "))) {
            lines.insert(0, Line::from(Span::styled("(identical to the current content)", Style::default().fg(Color::DarkGray))));
        }
        let text = old_t.lines().map(|l| Line::from(l.to_string())).collect();
        if let Overlay::Panel(p) = &mut self.overlay {
            p.detail = Some(Detail { rev_id: rev.id, title: rev.title, diff: lines, text, show_text: false, scroll: 0 });
        }
    }

    pub fn panel_key(&mut self, ev: KeyEvent) {
        let Overlay::Panel(p) = &mut self.overlay else { return };
        // pending y/n confirmation swallows the next key
        if let Some((action, _)) = p.pending.take() {
            if matches!(ev.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                let r = match action {
                    Pending::DeleteAttribute(id) => self.db.delete_attribute(&id),
                    Pending::Restore(id) => self.db.restore_revision(&id),
                };
                let was_restore = matches!(self.overlay, Overlay::Panel(ref p) if p.detail.is_some());
                match r {
                    Ok(()) => {
                        let note = if let Overlay::Panel(p) = &self.overlay { p.note_id.clone() } else { String::new() };
                        self.reload();
                        self.open_note(&note, false, false);
                        self.poke();
                        self.refresh_panel();
                        if was_restore { if let Overlay::Panel(p) = &mut self.overlay { p.detail = None; } }
                        self.status = "done".into();
                    }
                    Err(e) => self.status = format!("failed: {e:#}"),
                }
            }
            return;
        }
        let page = 10usize;
        // revision detail view
        if let Some(d) = p.detail.as_mut() {
            match ev.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left => p.detail = None,
                KeyCode::Up | KeyCode::Char('k') => d.scroll = d.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => d.scroll += 1,
                KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(page),
                KeyCode::PageDown => d.scroll += page,
                KeyCode::Char('v') => { d.show_text = !d.show_text; d.scroll = 0; }
                KeyCode::Char('r') => { let id = d.rev_id.clone(); let t = d.title.clone(); p.pending = Some((Pending::Restore(id), format!("Restore revision \"{t}\"? Current state is saved as a revision first. (y/n)"))); }
                _ => {}
            }
            return;
        }
        let n = PanelTab::ALL.len();
        let idx = PanelTab::ALL.iter().position(|t| *t == p.tab).unwrap();
        match ev.code {
            KeyCode::Esc | KeyCode::Char('q') => { self.overlay = Overlay::None; return; }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => { p.tab = PanelTab::ALL[(idx + 1) % n]; p.sel = 0; }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => { p.tab = PanelTab::ALL[(idx + n - 1) % n]; p.sel = 0; }
            KeyCode::Char(c @ '1'..='5') => { p.tab = PanelTab::ALL[c as usize - '1' as usize]; p.sel = 0; }
            KeyCode::Up | KeyCode::Char('k') => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => p.sel = (p.sel + 1).min(p.len().saturating_sub(1)),
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(page),
            KeyCode::PageDown => p.sel = (p.sel + page).min(p.len().saturating_sub(1)),
            _ => self.panel_tab_key(ev),
        }
    }

    fn panel_tab_key(&mut self, ev: KeyEvent) {
        let Overlay::Panel(p) = &mut self.overlay else { return };
        let (tab, sel, note_id) = (p.tab, p.sel, p.note_id.clone());
        match (tab, ev.code) {
            (PanelTab::Attributes, KeyCode::Char('d') | KeyCode::Delete) => {
                if let Some(a) = p.attrs.get(sel) {
                    if a.owner != note_id { self.status = "inherited attributes are edited on the note that owns them".into(); return; }
                    let msg = format!("Delete {}{}? (y/n)", if a.kind == "label" { "#" } else { "~" }, a.name);
                    p.pending = Some((Pending::DeleteAttribute(a.id.clone()), msg));
                }
            }
            (PanelTab::Attributes, KeyCode::Char('e')) => {
                if let Some(a) = p.attrs.get(sel).cloned() {
                    if a.owner != note_id { self.status = "inherited attributes are edited on the note that owns them".into(); return; }
                    self.panel_return = Some(PanelTab::Attributes);
                    self.open_prompt_pub(PromptKind::AttrValue(a.id.clone()), &format!("Value of {}{}", if a.kind == "label" { "#" } else { "~" }, a.name), &a.value);
                }
            }
            (PanelTab::Attributes, KeyCode::Char('a')) | (PanelTab::Attributes, KeyCode::Char('A')) => {
                let relation = ev.code == KeyCode::Char('A');
                self.panel_return = Some(PanelTab::Attributes);
                self.open_prompt_pub(PromptKind::Attribute { note_id, relation }, if relation { "New relation: name=target note" } else { "New label: name or name=value" }, "");
            }
            (PanelTab::Attributes, KeyCode::Enter) => {
                if let Some(a) = p.attrs.get(sel) {
                    if a.kind == "relation" && self.model.notes.contains_key(&a.value) { let t = a.value.clone(); self.overlay = Overlay::None; self.open_note(&t, true, true); }
                }
            }
            (PanelTab::Backlinks, KeyCode::Enter) => {
                if let Some(b) = p.backlinks.get(sel) { let t = b.source.clone(); self.overlay = Overlay::None; self.open_note(&t, true, true); self.focus = Focus::Note; }
            }
            (PanelTab::Revisions, KeyCode::Enter) => self.open_revision_detail(),
            (PanelTab::Attachments, KeyCode::Enter | KeyCode::Char('v')) => {
                if let Some(a) = p.atts.get(sel).cloned() {
                    if crate::images::is_image_mime(&a.mime) || ev.code == KeyCode::Char('v') {
                        match self.db.attachment_bytes(&a.id) {
                            Ok(Some(bytes)) => { self.panel_return = Some(PanelTab::Attachments); self.show_picture(&a.title, &a.title, &a.mime, bytes); }
                            _ => self.status = "attachment content unavailable".into(),
                        }
                    } else { self.save_attachment(&a); }
                }
            }
            (PanelTab::Attachments, KeyCode::Char('x')) => { if let Some(a) = p.atts.get(sel).cloned() { self.save_attachment(&a); } }
            (PanelTab::Attachments, KeyCode::Char('o') | KeyCode::Char('O')) => {
                if let Some(a) = p.atts.get(sel).cloned() {
                    match self.db.attachment_bytes(&a.id) {
                        Ok(Some(bytes)) => self.status = match crate::images::open_externally(&a.title, &bytes) { Ok(_) => "opened in your default application".into(), Err(e) => format!("{e:#}") },
                        _ => self.status = "attachment content unavailable".into(),
                    }
                }
            }
            _ => {}
        }
    }

    fn save_attachment(&mut self, a: &AttachmentInfo) {
        match self.db.attachment_bytes(&a.id) {
            Ok(Some(bytes)) => self.status = match crate::images::save_unique(&a.title, &bytes) { Ok(path) => format!("saved {}", path.display()), Err(e) => format!("{e:#}") },
            Ok(None) => self.status = "attachment content unavailable (protected?)".into(),
            Err(e) => self.status = format!("{e:#}"),
        }
    }
}
