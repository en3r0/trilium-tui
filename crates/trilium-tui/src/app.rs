//! Application state and behaviour. UI-framework free except for the cached `Line`s,
//! so behaviour can be tested without a terminal.
use crate::keymap::{Action, Keymap};
use crate::sync::SyncMsg;
use anyhow::Result;
use nucleo_matcher::{Config, Matcher, pattern::{CaseMatching, Normalization, Pattern}};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::{collections::HashSet, sync::mpsc::Sender, time::Instant};
use trilium_core::{db::Db, model::{Attribute, Model, ROOT}};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus { Tree, Note }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode { Rendered, Source }

#[derive(Clone, PartialEq, Debug)]
pub enum Kind {
    Html,
    Code,
    Collection,
    /// A saved search note: the ids of its current results
    SearchResults(Vec<String>),
    Binary,
    /// An image note: shown as a picture
    Image,
    Protected,
    /// A type that needs the browser's renderer (canvas, mermaid, ...): source is shown instead.
    Unsupported(String),
}

pub struct NoteView {
    pub id: String,
    pub kind: Kind,
    pub content: String,
    pub size: usize,
    pub attrs: Vec<Attribute>,
    pub mode: ViewMode,
    pub scroll: usize,
    /// selected row for list-like views (collections, saved searches)
    pub sel: usize,
    /// links in the rendered view and which one is selected
    pub links: Vec<crate::render::Link>,
    pub link_spans: Vec<Vec<Option<u32>>>,
    pub sel_link: Option<usize>,
    pub picture: Option<Box<crate::images::Picture>>,
    /// calendar cursor and board (column, card) cursor of collection views
    pub cal: Option<chrono::NaiveDate>,
    pub board: (usize, usize),
    pub cache: Option<(usize, ViewMode, Vec<Line<'static>>)>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub branch_id: String,
    pub note_id: String,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
    pub label: String,
    pub cloned: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickerKind { Jump, Search, Command, CloneTo, MoveTo }

pub struct Picker {
    pub kind: PickerKind,
    pub error: Option<String>,
    /// (branch, note) the clone/move operation applies to
    pub targets: Vec<(String, String)>,
    pub query: String,
    pub results: Vec<String>,
    pub sel: usize,
    pub executed: bool,
    recent: Vec<String>,
}

pub enum PromptKind {
    Rename(String),
    NewAfter(String),
    NewInto(String),
    Prefix(String),
    Attribute { note_id: String, relation: bool },
    AttrValue(String),
    ExportPath(String),
    ExportOpmlPath(String),
    ExportMarkdownPath(String),
    ExportEnexPath(String),
    ExportSitePath(String),
    ImportPath(String),
    ImportFormatted(String, &'static str),
    Append { note: String, today: bool },
    NewCard { board: String, group_by: String, value: String },
    NewEvent { calendar: String, date: String },
    /// Protected-notes password (unlock); if none exists yet the flow continues with NewPassword
    Unlock,
    NewPassword,
    ConfirmPassword(String),
    DeleteBackups,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub label: String,
    pub text: Vec<char>,
    pub cursor: usize,
    /// typed characters are shown as dots (passwords)
    pub secret: bool,
}

pub struct Confirm {
    pub message: String,
    pub branches: Vec<String>,
}

pub enum Overlay { None, Edit(Box<crate::edit_view::EditSession>), Picker(Picker), Help, Prompt(Prompt), Confirm(Confirm), Panel(Box<crate::panel::Panel>), Image(Box<crate::images::Picture>) }

/// Note id from an internal link: `#root/a/b/<id>`, `<server>/#root/<id>?x`, or `#<id>`.
pub fn note_id_from_href(href: &str) -> Option<String> {
    let frag = href.split_once('#').map(|(_, f)| f)?;
    let path = frag.split(['?', '&']).next()?.trim_end_matches('/');
    let last = path.rsplit('/').next()?;
    (!last.is_empty() && last != "root" && last.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| last.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipMode { Copy, Cut }

pub struct Clipboard { pub mode: ClipMode, pub items: Vec<(String, String)> }

/// The tree list stops here: expanded clones can multiply rows per level, and a crafted import must not make every launch run out of memory.
const MAX_TREE_ROWS: usize = 200_000;

pub enum SyncState { Off, Syncing, Ok(Instant, usize), Error(String) }

#[derive(Default)]
pub struct TabState {
    pub view: Option<NoteView>,
    pub history: Vec<String>,
    pub hist_idx: usize,
}

pub struct App {
    pub db: Db,
    pub model: Model,
    pub keymap: Keymap,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub tree_scroll: usize,
    pub expanded: HashSet<String>,
    pub show_archived: bool,
    pub hoisted: String,
    pub focus: Focus,
    pub overlay: Overlay,
    pub history: Vec<String>,
    pub hist_idx: usize,
    pub view: Option<NoteView>,
    pub status: String,
    pub sync: SyncState,
    pub sync_trigger: Option<Sender<()>>,
    pub server_url: Option<String>,
    pub quit: bool,
    pub tree_height: usize,
    pub note_height: usize,
    pub tree_area: Rect,
    pub note_area: Rect,
    /// note whose content should be opened in $EDITOR by the main loop (needs the terminal)
    pub pending_edit: Option<String>,
    /// last key or mouse event (protected-session timeout)
    pub last_input: Instant,
    pub synced_once: bool,
    pub want_today: bool,
    /// what the note held when it was handed to the external editor
    pub edit_base: Option<String>,
    pub edit_area: Rect,
    /// the in-place editor, when the note pane is being edited
    pub rich: Option<Box<crate::rich_view::RichSession>>,
    pub help_scroll: std::cell::Cell<usize>,
    /// Inactive tabs live here; the active tab's state is in `view`/`history`/`hist_idx`.
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub closed_tabs: Vec<TabState>,
    pub searcher: Option<trilium_core::search::Searcher>,
    /// reopen this panel tab after a prompt started from the panel closes
    pub panel_return: Option<crate::panel::PanelTab>,
    /// rows marked for multi-select: (branch id, note id)
    pub marked: Vec<(String, String)>,
    pub picker: Option<ratatui_image::picker::Picker>,
    pub clipboard: Option<Clipboard>,
    pub index_ready: bool,
}

impl App {
    pub fn new(db_path: &std::path::Path, server_url: Option<String>) -> Result<Self> {
        let db = Db::open(db_path)?;
        let model = Model::load(&db)?;
        let server_url = server_url.or(db.state("serverUrl")?);
        let mut app = App {
            db, model, keymap: Keymap::load(), rows: vec![], sel: 0, tree_scroll: 0,
            expanded: HashSet::new(), show_archived: false, hoisted: ROOT.into(), focus: Focus::Tree,
            overlay: Overlay::None, history: vec![], hist_idx: 0, view: None, status: String::new(),
            sync: SyncState::Off, sync_trigger: None, server_url, quit: false, tree_height: 20, note_height: 20, tree_area: Rect::default(), note_area: Rect::default(), pending_edit: None, last_input: Instant::now(), synced_once: false, want_today: false, edit_base: None, edit_area: Rect::default(), rich: None, help_scroll: std::cell::Cell::new(0), tabs: vec![TabState::default()], active_tab: 0, closed_tabs: vec![], searcher: None, panel_return: None, marked: vec![], picker: None, clipboard: None, index_ready: true,
        };
        app.expanded = app.model.children.values().flatten().filter(|b| b.expanded).map(|b| b.id.clone()).collect();
        app.rebuild_rows();
        if let Some(first) = app.rows.first().map(|r| r.note_id.clone()) { app.open_note(&first, true, false); }
        Ok(app)
    }

    // ---------------------------------------------------------------- tree

    pub fn rebuild_rows(&mut self) {
        let keep = self.rows.get(self.sel).map(|r| r.branch_id.clone());
        let mut rows = Vec::new();
        let hoisted = self.hoisted.clone();
        self.walk(&hoisted, 0, &mut rows, 0);
        self.rows = rows;
        self.sel = keep.and_then(|k| self.rows.iter().position(|r| r.branch_id == k)).unwrap_or(0).min(self.rows.len().saturating_sub(1));
    }

    fn walk(&self, parent: &str, depth: usize, out: &mut Vec<Row>, guard: usize) {
        if guard > 64 || out.len() >= MAX_TREE_ROWS { return; }
        let Some(children) = self.model.children.get(parent) else { return };
        for b in children {
            if out.len() >= MAX_TREE_ROWS { return; }
            if parent == ROOT && b.note_id == "_hidden" { continue; }
            if !self.show_archived && self.model.archived.contains(&b.note_id) { continue; }
            let expandable = self.model.has_children(&b.note_id, self.show_archived);
            let expanded = expandable && self.expanded.contains(&b.id);
            out.push(Row {
                branch_id: b.id.clone(), note_id: b.note_id.clone(), depth, expandable, expanded,
                label: self.model.display_title(b),
                cloned: self.model.parents.get(&b.note_id).is_some_and(|p| p.len() > 1),
            });
            if expanded { self.walk(&b.note_id, depth + 1, out, guard + 1); }
        }
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() { return; }
        let n = (self.sel as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        if n != self.sel { self.sel = n; self.activate_selected(false); }
    }

    pub fn select_index(&mut self, i: usize) {
        if self.rows.is_empty() { return; }
        self.sel = i.min(self.rows.len() - 1);
        self.activate_selected(false);
    }

    fn activate_selected(&mut self, _focus_note: bool) {
        if let Some(r) = self.rows.get(self.sel) {
            let id = r.note_id.clone();
            self.open_note(&id, true, false);
        }
    }

    pub fn toggle_expand(&mut self, expand: Option<bool>) {
        let Some(r) = self.rows.get(self.sel).cloned() else { return };
        if !r.expandable { return; }
        let want = expand.unwrap_or(!r.expanded);
        if want { self.expanded.insert(r.branch_id); } else { self.expanded.remove(&r.branch_id); }
        self.rebuild_rows();
    }

    pub fn left(&mut self) {
        let Some(r) = self.rows.get(self.sel).cloned() else { return };
        if r.expanded { self.toggle_expand(Some(false)); return; }
        // go to parent row
        if let Some(i) = self.rows[..self.sel].iter().rposition(|p| p.depth + 1 == r.depth) { self.select_index(i); }
    }

    pub fn right(&mut self) {
        let Some(r) = self.rows.get(self.sel).cloned() else { return };
        if r.expandable && !r.expanded { self.toggle_expand(Some(true)); } else if r.expandable { self.move_sel(1); }
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.rebuild_rows();
        self.reveal_current();
    }

    pub fn collapse_subtree(&mut self) {
        let Some(r) = self.rows.get(self.sel).cloned() else { return };
        let mut stack = vec![r.note_id];
        while let Some(n) = stack.pop() {
            for b in self.model.children.get(&n).into_iter().flatten() {
                self.expanded.remove(&b.id);
                stack.push(b.note_id.clone());
            }
        }
        self.rebuild_rows();
    }

    /// Expand ancestors of the open note and put the cursor on it.
    pub fn reveal_current(&mut self) {
        let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return };
        self.reveal(&id);
    }

    pub(crate) fn reveal(&mut self, id: &str) {
        if let Some(path) = self.model.path_to(id) {
            let mut parent = ROOT.to_string();
            for step in &path[..path.len().saturating_sub(1)] {
                if let Some(b) = self.model.children.get(&parent).and_then(|c| c.iter().find(|b| &b.note_id == step)) {
                    self.expanded.insert(b.id.clone());
                }
                parent = step.clone();
            }
            // note outside the hoisted subtree: un-hoist so it can be shown
            if self.hoisted != ROOT && !path.contains(&self.hoisted) { self.hoisted = ROOT.into(); }
            self.rebuild_rows();
            if let Some(i) = self.rows.iter().position(|r| r.note_id == id) { self.sel = i; }
        }
    }

    pub fn hoist(&mut self) {
        if let Some(r) = self.rows.get(self.sel) {
            if r.expandable { self.hoisted = r.note_id.clone(); self.expanded.insert(r.branch_id.clone()); self.sel = 0; self.rebuild_rows(); }
        }
    }

    pub fn unhoist(&mut self) {
        let prev = self.hoisted.clone();
        self.hoisted = ROOT.into();
        self.rebuild_rows();
        self.reveal(&prev);
    }

    // ---------------------------------------------------------------- search

    fn ensure_searcher(&mut self) -> anyhow::Result<()> {
        if self.searcher.is_none() {
            let s = trilium_core::search::Searcher::new(&self.db)?;
            s.set_index_ready(self.index_ready);
            self.searcher = Some(s);
        }
        Ok(())
    }

    /// Run a Trilium search query; returns (note ids, error message).
    pub fn run_search(&mut self, query: &str, limit: usize) -> (Vec<String>, Option<String>) {
        let opts = trilium_core::search::SearchOpts { limit: Some(limit), include_archived: self.show_archived, ..Default::default() };
        let out = match self.ensure_searcher() {
            Ok(()) => self.searcher.as_ref().unwrap().search(&self.db, query, &opts),
            Err(e) => Err(e),
        };
        match out {
            Ok(o) => (o.hits.into_iter().map(|h| h.note_id).collect(), o.error),
            Err(e) => (vec![], Some(format!("{e:#}"))),
        }
    }

    // ---------------------------------------------------------------- notes

    pub fn open_note(&mut self, id: &str, push_history: bool, reveal: bool) {
        let Some(note) = self.model.notes.get(id).cloned() else { self.status = format!("note {id} not found"); return };
        let attrs = self.db.note_attributes(id).unwrap_or_default();
        let (kind, content, size) = if note.protected && !self.db.is_unlocked() {
            (Kind::Protected, String::new(), 0)
        } else {
            match note.ntype.as_str() {
                "text" => self.load_string(id, Kind::Html),
                "code" => self.load_string(id, Kind::Code),
                "book" => (Kind::Collection, String::new(), 0),
                "search" => {
                    let q = attrs.iter().find(|a| a.kind == "label" && a.name == "searchString").map(|a| a.value.clone()).unwrap_or_default();
                    let ids = if q.trim().is_empty() { vec![] } else { self.run_search(&q, 500).0 };
                    (Kind::SearchResults(ids), q, 0)
                }
                "image" => (Kind::Image, String::new(), 0),
                "file" | "pdf" => {
                    let size: i64 = self.db.conn.query_row(
                        "SELECT COALESCE(length(b.content),0) FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId=?",
                        [id], |r| r.get(0)).unwrap_or(0);
                    (Kind::Binary, String::new(), size as usize)
                }
                other => self.load_string(id, Kind::Unsupported(other.to_string())),
            }
        };
        let markdown = matches!(kind, Kind::Code) && crate::render::is_markdown_mime(&note.mime);
        let mode = if matches!(kind, Kind::Html) || markdown { ViewMode::Rendered } else { ViewMode::Source };
        self.view = Some(NoteView { id: id.into(), kind, content, size, attrs, mode, scroll: 0, sel: 0, links: vec![], link_spans: vec![], sel_link: None, picture: None, cal: None, board: (0, 0), cache: None });
        if push_history && self.history.get(self.hist_idx).map(String::as_str) != Some(id) {
            self.history.truncate(if self.history.is_empty() { 0 } else { self.hist_idx + 1 });
            self.history.push(id.into());
            self.hist_idx = self.history.len() - 1;
        }
        if reveal { self.reveal(id); }
    }

    fn load_string(&self, id: &str, kind: Kind) -> (Kind, String, usize) {
        let c = self.db.note_content(id).ok().flatten().unwrap_or_default();
        let n = c.len();
        (kind, c, n)
    }

    pub fn history_step(&mut self, back: bool) {
        let target = if back { self.hist_idx.checked_sub(1) } else { Some(self.hist_idx + 1).filter(|i| *i < self.history.len()) };
        if let Some(i) = target {
            self.hist_idx = i;
            let id = self.history[i].clone();
            self.open_note(&id, false, true);
        }
    }

    /// Note ids listed by a collection or saved-search view (display order).
    pub fn list_items(&self, v: &NoteView) -> Vec<String> {
        match &v.kind {
            Kind::SearchResults(ids) => ids.clone(),
            Kind::Collection => self.model.children.get(&v.id).into_iter().flatten()
                .filter(|b| self.show_archived || !self.model.archived.contains(&b.note_id)).map(|b| b.note_id.clone()).collect(),
            _ => vec![],
        }
    }

    pub fn image_picker(&mut self) -> &ratatui_image::picker::Picker {
        self.picker.get_or_insert_with(ratatui_image::picker::Picker::halfblocks)
    }

    /// Decode the open image note (once) so the pane can draw it.
    pub fn ensure_note_picture(&mut self) {
        let Some(v) = self.view.as_ref() else { return };
        if !matches!(v.kind, Kind::Image) || v.picture.is_some() { return; }
        let id = v.id.clone();
        let Some(n) = self.model.notes.get(&id).cloned() else { return };
        let bytes = self.db.note_bytes(&id).ok().flatten().unwrap_or_default();
        let name = crate::images::file_name_for(&n.title, &n.mime);
        let pic = crate::images::Picture::load(self.image_picker(), &n.title, &name, &n.mime, bytes);
        if let Some(v) = self.view.as_mut() { v.picture = Some(Box::new(pic)); }
    }

    /// Open the picture viewer on the open image note or any bytes.
    pub fn show_picture(&mut self, title: &str, filename: &str, mime: &str, bytes: Vec<u8>) {
        let pic = crate::images::Picture::load(self.image_picker(), title, filename, mime, bytes);
        self.overlay = Overlay::Image(Box::new(pic));
    }

    pub fn image_key(&mut self, ev: KeyEvent) {
        let Overlay::Image(p) = &self.overlay else { return };
        match ev.code {
            KeyCode::Char('x') => match crate::images::save_unique(&p.filename, &p.bytes) { Ok(path) => self.status = format!("saved {}", path.display()), Err(e) => self.status = format!("{e:#}") },
            KeyCode::Char('O') | KeyCode::Char('o') => match crate::images::open_externally(&p.filename, &p.bytes) { Ok(_) => self.status = "opened in your default viewer".into(), Err(e) => self.status = format!("{e:#}") },
            _ => self.overlay = Overlay::None,
        }
    }

    /// Save / open the file or image the open note holds.
    pub fn note_file_action(&mut self, open: bool) {
        let Some(v) = self.view.as_ref() else { return };
        if !matches!(v.kind, Kind::Binary | Kind::Image) { return; }
        let Some(n) = self.model.notes.get(&v.id).cloned() else { return };
        let Some(bytes) = self.db.note_bytes(&n.id).ok().flatten() else { self.status = "no content to save".into(); return };
        let name = crate::images::file_name_for(&n.title, &n.mime);
        let r = if open { crate::images::open_externally(&name, &bytes).map(|_| "opened in your default application".to_string()) }
                else { crate::images::save_unique(&name, &bytes).map(|p| format!("saved {}", p.display())) };
        self.status = match r { Ok(m) => m, Err(e) => format!("{e:#}") };
    }

    /// Select the next/previous link of the rendered view and scroll it into sight.
    pub fn step_link(&mut self, forward: bool) {
        let Some(v) = self.view.as_mut() else { return };
        if v.links.is_empty() { self.status = "no links in this note".into(); return; }
        let n = v.links.len();
        let cur = v.sel_link;
        let next = match (cur, forward) { (None, true) => 0, (None, false) => n - 1, (Some(c), true) => (c + 1) % n, (Some(c), false) => (c + n - 1) % n };
        v.sel_link = Some(next);
        if let Some(line) = v.link_spans.iter().position(|spans| spans.contains(&Some(next as u32))) {
            let h = self.note_height.max(1);
            if line < v.scroll || line >= v.scroll + h { v.scroll = line.saturating_sub(h / 3); }
        }
        let l = &v.links[next];
        self.status = format!("link {}/{}: \"{}\" -> {}   (Enter follows)", next + 1, n, l.text, l.href);
    }

    /// Enter in the note pane: zoom an image note, otherwise follow the selected link.
    fn enter_note(&mut self) {
        let Some(v) = self.view.as_ref() else { return };
        if matches!(v.kind, Kind::Image) {
            let id = v.id.clone();
            if let Some(n) = self.model.notes.get(&id).cloned() {
                if let Some(bytes) = self.db.note_bytes(&id).ok().flatten() {
                    let name = crate::images::file_name_for(&n.title, &n.mime);
                    self.show_picture(&n.title, &name, &n.mime, bytes);
                }
            }
            return;
        }
        self.follow_link();
    }

    /// Follow the selected link: internal links open the note, web links open the browser.
    pub fn follow_link(&mut self) {
        let Some(v) = self.view.as_ref() else { return };
        let Some(link) = v.sel_link.and_then(|i| v.links.get(i)).cloned() else {
            self.status = "no link selected: press ] to select one".into();
            return;
        };
        let href = link.href.trim();
        if link.image {
            match self.db.image_source_bytes(href) {
                Ok(Some((name, mime, bytes))) => { let t = if link.text.is_empty() { name.clone() } else { link.text.clone() }; self.show_picture(&t, &name, &mime, bytes); }
                Ok(None) if href.starts_with("http://") || href.starts_with("https://") => {
                    let ok = std::process::Command::new("xdg-open").arg(href).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok();
                    self.status = if ok { format!("opened {href} in the browser (remote image)") } else { "xdg-open failed".into() };
                }
                Ok(None) => self.status = format!("image '{href}' isn't in the replica (not synced, or an unsupported source)"),
                Err(e) => self.status = format!("{e:#}"),
            }
            return;
        }
        if let Some(id) = note_id_from_href(href) {
            if self.model.notes.contains_key(&id) { self.open_note(&id, true, true); self.focus = Focus::Note; }
            else { self.status = format!("linked note {id} not found (not synced or deleted)"); }
        } else if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("mailto:") {
            let ok = std::process::Command::new("xdg-open").arg(href).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok();
            self.status = if ok { format!("opened {href}") } else { format!("xdg-open failed for {href}") };
        } else {
            self.status = format!("can't follow '{href}'");
        }
    }

    pub fn toggle_source(&mut self) {
        let md = self.view.as_ref().is_some_and(|v| matches!(v.kind, Kind::Code) && self.model.notes.get(&v.id).is_some_and(|n| crate::render::is_markdown_mime(&n.mime)));
        if let Some(v) = self.view.as_mut() {
            if matches!(v.kind, Kind::Html) || md {
                v.mode = if v.mode == ViewMode::Rendered { ViewMode::Source } else { ViewMode::Rendered };
                v.scroll = 0;
            }
        }
    }

    pub fn scroll(&mut self, delta: isize) {
        let h = self.note_height;
        if let Some(v) = self.view.as_mut() {
            let total = v.cache.as_ref().map(|c| c.2.len()).unwrap_or(0);
            let max = total.saturating_sub(h.max(1) / 2);
            v.scroll = (v.scroll as isize + delta).clamp(0, max as isize) as usize;
        }
    }

    pub fn browser_url(&self) -> Option<String> {
        let base = self.server_url.as_ref()?.trim_end_matches('/');
        let id = &self.view.as_ref()?.id;
        let path = self.model.path_to(id)?;
        Some(format!("{base}/#root/{}", path.join("/")))
    }

    pub fn open_in_browser(&mut self) {
        match self.browser_url() {
            Some(url) => {
                let ok = std::process::Command::new("xdg-open").arg(&url)
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok();
                self.status = if ok { format!("opened {url}") } else { format!("xdg-open failed: {url}") };
            }
            None => self.status = "no server URL known (sync with `trilium-cli sync <url>` first)".into(),
        }
    }

    // ---------------------------------------------------------------- sync

    /// Remote changes that lost to newer unpushed local edits: say so and keep a log.
    fn report_conflicts(&mut self, conflicts: &[(String, String)]) {
        let names: Vec<String> = conflicts.iter().map(|(e, id)| match e.as_str() {
            "notes" => format!("'{}'", self.log_title(id)),
            other => format!("{other} {id}"),
        }).collect();
        self.status = format!("⚠ sync conflict: a remote change to {} was older than your edit and was not applied", names.join(", "));
        if let Some(dir) = self.db.conn.path().map(std::path::PathBuf::from).and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            use std::io::Write;
            let mut oo = std::fs::OpenOptions::new();
            oo.create(true).append(true);
            #[cfg(unix)]
            { use std::os::unix::fs::OpenOptionsExt; oo.mode(0o600); }
            if let Ok(mut f) = oo.open(dir.join("conflicts.log")) {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                for (e, id) in conflicts {
                    let title: String = if e == "notes" { self.log_title(id).chars().map(|c| if c.is_control() { ' ' } else { c }).collect() } else { String::new() };
                    let _ = writeln!(f, "{now}\tkept-local\t{e}\t{id}\t{title}");
                }
            }
        }
    }

    pub fn on_sync(&mut self, msg: SyncMsg) {
        match msg {
            SyncMsg::Syncing => self.sync = SyncState::Syncing,
            SyncMsg::Done { pulled, pushed, conflicts, overwritten } => {
                self.sync = SyncState::Ok(Instant::now(), pushed);
                self.synced_once = true;
                if pulled > 0 { self.reload(); }
                if self.want_today { self.open_today(); }
                if !conflicts.is_empty() { self.report_conflicts(&conflicts); }
                else if overwritten > 0 { self.status = format!("⚠ {overwritten} of your unsynced edit(s) were replaced by newer changes from the server; the old text is in the 'overwritten' folder next to the database"); }
            }
            SyncMsg::Error(e) => {
                self.sync = SyncState::Error(e);
                // offline: better a note that may need merging later than no journal at all
                if self.want_today { self.synced_once = true; self.open_today(); }
            }
            SyncMsg::IndexReady => { self.index_ready = true; self.searcher = None; }
        }
    }

    /// A note title for logs and messages that outlive the session: never a protected one.
    fn log_title(&self, id: &str) -> String {
        if self.model.notes.get(id).is_some_and(|n| n.protected) { "[protected]".into() } else { self.model.title_of(id).to_string() }
    }

    /// Re-read the database after external changes, keeping selection and open note.
    pub fn reload(&mut self) {
        if let Ok(m) = Model::load(&self.db) {
            self.model = m;
            self.searcher = None;
            let open = self.view.as_ref().map(|v| (v.id.clone(), v.mode, v.scroll));
            self.rebuild_rows();
            if let Some((id, mode, scroll)) = open {
                if self.model.notes.contains_key(&id) {
                    self.open_note(&id, false, false);
                    if let Some(v) = self.view.as_mut() { v.scroll = scroll; if matches!(v.kind, Kind::Html) { v.mode = mode; } }
                } else {
                    // the open note was deleted (here or by a sync): nothing to show, and nothing may index it any more
                    self.view = None;
                }
            }
        }
    }

    // ---------------------------------------------------------------- overlays

    pub fn open_picker(&mut self, kind: PickerKind) {
        let mut recent: Vec<&trilium_core::model::Note> = self.model.notes.values()
            .filter(|n| n.id != ROOT && !n.id.starts_with('_') && (!n.protected || self.db.is_unlocked())).collect();
        recent.sort_by(|a, b| b.utc_modified.cmp(&a.utc_modified));
        let mut recent_ids: Vec<String> = self.history.iter().rev().cloned().collect();
        recent_ids.extend(recent.iter().take(50).map(|n| n.id.clone()));
        let mut seen = HashSet::new();
        recent_ids.retain(|i| seen.insert(i.clone()) && self.model.notes.contains_key(i));
        recent_ids.truncate(50);
        let results = match kind {
            PickerKind::Jump | PickerKind::CloneTo | PickerKind::MoveTo => recent_ids.clone(),
            PickerKind::Command => Action::ALL.iter().map(|(a, _)| format!("{a:?}")).collect(),
            PickerKind::Search => vec![],
        };
        self.overlay = Overlay::Picker(Picker { kind, error: None, targets: vec![], query: String::new(), results, sel: 0, executed: false, recent: recent_ids });
    }

    fn refresh_jump(&mut self) {
        let Overlay::Picker(p) = &mut self.overlay else { return };
        if p.kind == PickerKind::Command {
            let pattern = Pattern::parse(&p.query, CaseMatching::Smart, Normalization::Smart);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, usize)> = Action::ALL.iter().enumerate()
                .filter_map(|(i, (_, t))| pattern.score(nucleo_matcher::Utf32Str::new(t, &mut buf), &mut matcher).map(|s| (s, i))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            p.results = scored.into_iter().map(|(_, i)| format!("{:?}", Action::ALL[i].0)).collect();
            p.sel = 0;
            return;
        }
        if p.query.trim().is_empty() { p.results = p.recent.clone(); p.sel = 0; return; }
        let pattern = Pattern::parse(&p.query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let cands: Vec<(&str, &str)> = self.model.notes.values()
            .filter(|n| n.id != ROOT && !n.id.starts_with('_') && (self.show_archived || !self.model.archived.contains(&n.id)))
            .map(|n| (n.title.as_str(), n.id.as_str())).collect();
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, &str, &str)> = Vec::new();
        for (t, id) in &cands {
            if let Some(sc) = pattern.score(nucleo_matcher::Utf32Str::new(t, &mut buf), &mut matcher) { scored.push((sc, *t, *id)); }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
        p.results = scored.into_iter().take(50).map(|(_, _, id)| id.to_string()).collect();
        p.sel = 0;
    }

    // ---------------------------------------------------------------- input

    pub fn handle_key(&mut self, ev: KeyEvent) {
        self.last_input = Instant::now();
        if matches!(self.overlay, Overlay::Help) {
            let s = self.help_scroll.get();
            match ev.code {
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll.set(s + 1),
                KeyCode::Up | KeyCode::Char('k') => self.help_scroll.set(s.saturating_sub(1)),
                KeyCode::PageDown | KeyCode::Char(' ') => self.help_scroll.set(s + 10),
                KeyCode::PageUp => self.help_scroll.set(s.saturating_sub(10)),
                KeyCode::Home | KeyCode::Char('g') => self.help_scroll.set(0),
                KeyCode::End | KeyCode::Char('G') => self.help_scroll.set(usize::MAX / 2),
                _ => { self.overlay = Overlay::None; self.help_scroll.set(0); }
            }
            return;
        }
        if matches!(self.overlay, Overlay::Edit(_)) {
            self.edit_key(ev);
            return;
        }
        if self.rich.is_some() && matches!(self.overlay, Overlay::None) {
            if ev.code == KeyCode::F(1) { self.overlay = Overlay::Help; return; }
            self.rich_key(ev);
            return;
        }
        if matches!(self.overlay, Overlay::Picker(_)) {
            self.picker_key(ev);
            return;
        }
        if matches!(self.overlay, Overlay::Prompt(_)) {
            self.prompt_key(ev);
            if matches!(self.overlay, Overlay::None) { if let Some(t) = self.panel_return.take() { self.open_panel(t); } }
            return;
        }
        if matches!(self.overlay, Overlay::Panel(_)) {
            self.panel_key(ev);
            return;
        }
        if matches!(self.overlay, Overlay::Image(_)) {
            self.image_key(ev);
            if matches!(self.overlay, Overlay::None) { if let Some(t) = self.panel_return.take() { self.open_panel(t); } }
            return;
        }
        if matches!(self.overlay, Overlay::Confirm(_)) {
            self.confirm_key(ev);
            return;
        }
        self.status.clear();
        if let Some(action) = self.keymap.action_for(&ev) {
            if !(action.tree_only() && self.focus != Focus::Tree) {
                self.run_action(action);
                return;
            }
        }
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match (ev.code, ctrl) {
            (KeyCode::Char('q'), false) | (KeyCode::Char('q'), true) => { self.quit = true; return; }
            (KeyCode::Char('c'), true) => { self.status = "Ctrl+C copies notes in the tree; q quits".into(); return; }
            (KeyCode::Char('/'), false) => { self.open_picker(PickerKind::Search); return; }
            (KeyCode::Char(':'), false) => { self.open_picker(PickerKind::Command); return; }
            (KeyCode::Char('?'), false) => { self.overlay = Overlay::Help; return; }
            (KeyCode::Char('o'), false) => { self.open_in_browser(); return; }
            (KeyCode::Char('i'), false) => { self.open_panel(crate::panel::PanelTab::Info); return; }
            (KeyCode::Char('b'), false) if self.focus == Focus::Tree => { self.open_panel(crate::panel::PanelTab::Backlinks); return; }
            (KeyCode::Char('v'), false) if self.focus == Focus::Tree => { self.open_panel(crate::panel::PanelTab::Revisions); return; }
            (KeyCode::Char('e'), false) => { self.request_edit(); return; }
            (KeyCode::Char('e'), true) => { self.request_edit_source(); return; }
            (KeyCode::Char('E'), _) => { self.request_edit_external(); return; }
            (KeyCode::Tab, false) | (KeyCode::BackTab, _) => {
                self.focus = if self.focus == Focus::Tree { Focus::Note } else { Focus::Tree };
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Tree => self.tree_key(ev),
            Focus::Note => self.note_key(ev),
        }
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        self.last_input = Instant::now();
        if let (Some(r), Overlay::None) = (self.rich.as_mut(), &self.overlay) {
            r.mouse(ev);
            return;
        }
        if let Overlay::Edit(s) = &mut self.overlay {
            let ta = crate::edit_view::text_area(self.edit_area);
            match ev.kind {
                MouseEventKind::ScrollUp => s.scroll_by(-3),
                MouseEventKind::ScrollDown => s.scroll_by(3),
                MouseEventKind::Down(_) => s.click(ta, ev.column, ev.row),
                _ => {}
            }
            return;
        }
        if !matches!(self.overlay, Overlay::None) { return; }
        let (x, y) = (ev.column, ev.row);
        let in_rect = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let in_tree = in_rect(self.tree_area);
        match ev.kind {
            MouseEventKind::ScrollUp if in_tree => self.move_sel(-3),
            MouseEventKind::ScrollDown if in_tree => self.move_sel(3),
            MouseEventKind::ScrollUp if in_rect(self.note_area) => self.scroll(-3),
            MouseEventKind::ScrollDown if in_rect(self.note_area) => self.scroll(3),
            MouseEventKind::Down(MouseButton::Left) if in_tree => {
                // area includes the border: first row is at y+1
                let row = self.tree_scroll + (y.saturating_sub(self.tree_area.y + 1)) as usize;
                if row < self.rows.len() {
                    let indent_end = self.tree_area.x + 1 + (self.rows[row].depth * 2) as u16 + 2;
                    self.focus = Focus::Tree;
                    self.select_index(row);
                    if x < indent_end { self.toggle_expand(None); }
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_rect(self.note_area) => self.focus = Focus::Note,
            _ => {}
        }
    }

    fn tree_key(&mut self, ev: KeyEvent) {
        let page = self.tree_height.max(2) as isize - 1;
        match ev.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Left | KeyCode::Char('h') => self.left(),
            KeyCode::Right | KeyCode::Char('l') => self.right(),
            KeyCode::Char(' ') => self.toggle_expand(None),
            KeyCode::Char('t') => self.open_today(),
            KeyCode::Char('c') => self.run_action(Action::CaptureToday),
            KeyCode::Char('m') => self.run_action(Action::ToggleMark),
            KeyCode::Esc => { if !self.marked.is_empty() { self.marked.clear(); self.status = "selection cleared".into(); } }
            KeyCode::Enter => self.run_action(Action::EditTitle),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Home | KeyCode::Char('g') => self.select_index(0),
            KeyCode::End | KeyCode::Char('G') => self.select_index(usize::MAX / 2),
            _ => {}
        }
    }

    fn note_key(&mut self, ev: KeyEvent) {
        let page = self.note_height.max(2) as isize - 1;
        // board and calendar collections have their own cursor and keys
        if self.collection_key(ev) { return; }
        // list-like notes (collections, saved searches): the cursor moves between entries
        let items = self.view.as_ref().map(|v| self.list_items(v)).unwrap_or_default();
        if !items.is_empty() {
            let cur = self.view.as_ref().map_or(0, |v| v.sel);
            let step = |d: isize| (cur as isize + d).clamp(0, items.len() as isize - 1) as usize;
            let new = match ev.code {
                KeyCode::Up | KeyCode::Char('k') => Some(step(-1)),
                KeyCode::Down | KeyCode::Char('j') => Some(step(1)),
                KeyCode::PageUp => Some(step(-page)),
                KeyCode::PageDown => Some(step(page)),
                KeyCode::Home | KeyCode::Char('g') => Some(0),
                KeyCode::End | KeyCode::Char('G') => Some(items.len() - 1),
                _ => None,
            };
            if let Some(n) = new { if let Some(v) = self.view.as_mut() { v.sel = n; } return; }
            if ev.code == KeyCode::Enter {
                if let Some(id) = items.get(cur).cloned() { self.open_note(&id, true, true); }
                return;
            }
        }
        match ev.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll(isize::MAX / 2),
            KeyCode::Char('s') => self.toggle_source(),
            KeyCode::Char('a') => self.run_action(Action::AppendToNote),
            KeyCode::Char('x') => self.note_file_action(false),
            KeyCode::Char('O') => self.note_file_action(true),
            KeyCode::Char(']') => self.step_link(true),
            KeyCode::Char('[') => self.step_link(false),
            KeyCode::Enter => self.enter_note(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => self.focus = Focus::Tree,
            _ => {}
        }
    }

    pub fn run_action(&mut self, a: Action) {
        match a {
            Action::Back => self.history_step(true),
            Action::Forward => self.history_step(false),
            Action::JumpToNote => self.open_picker(PickerKind::Jump),
            Action::QuickSearch => self.open_picker(PickerKind::Search),
            Action::ScrollToActiveNote => self.reveal_current(),
            Action::CollapseTree => self.collapse_all(),
            Action::CollapseSubtree => self.collapse_subtree(),
            Action::ToggleArchived => { self.show_archived = !self.show_archived; self.rebuild_rows(); self.status = format!("archived notes {}", if self.show_archived { "shown" } else { "hidden" }); }
            Action::SyncNow => match &self.sync_trigger {
                Some(t) => { let _ = t.send(()); self.status = "sync requested".into(); }
                None => self.status = "sync is off (no server configured)".into(),
            },
            Action::Hoist => self.hoist(),
            Action::FollowLink => self.follow_link(),
            Action::ExportNote | Action::ExportOpml | Action::ExportSite | Action::ExportMarkdown | Action::ExportEnex | Action::ImportHere | Action::ImportObsidian | Action::ImportNotion | Action::ImportAnytype | Action::ImportKeep => self.run_transfer_action(a),
            Action::AppendToNote | Action::CaptureToday => self.run_capture_action(a),
            Action::ShowAttributes => self.open_panel(crate::panel::PanelTab::Attributes),
            Action::ShowBacklinks => self.open_panel(crate::panel::PanelTab::Backlinks),
            Action::ShowRevisions => self.open_panel(crate::panel::PanelTab::Revisions),
            Action::ShowNoteInfo => self.open_panel(crate::panel::PanelTab::Info),
            Action::ShowAttachments => self.open_panel(crate::panel::PanelTab::Attachments),
            Action::Unhoist => self.unhoist(),
            Action::Help => self.overlay = Overlay::Help,
            Action::CommandPalette => self.open_picker(PickerKind::Command),
            Action::ToggleSource => self.toggle_source(),
            Action::OpenInBrowser => self.open_in_browser(),
            Action::EditContent => self.request_edit(),
            Action::EditSource => self.request_edit_source(),
            Action::EditExternal => self.request_edit_external(),
            Action::Quit => self.quit = true,
            Action::OpenToday => self.open_today(),
            Action::OpenWeek | Action::OpenQuarter => self.open_period(a == Action::OpenWeek),
            Action::CopyNotes | Action::CutNotes | Action::PasteNotes | Action::SelectAllSiblings | Action::SelectAbove | Action::SelectBelow
                | Action::CloneTo | Action::MoveTo | Action::DuplicateNote | Action::ToggleMark => self.run_selection_action(a),
            Action::NewTab | Action::CloseTab | Action::ReopenTab | Action::NextTab | Action::PrevTab | Action::GotoTab(_) => self.run_tab_action(a),
            Action::SaveRevision => self.save_revision_now(),
            Action::UnlockProtected | Action::LockProtected | Action::ProtectNote | Action::UnprotectNote | Action::ProtectSubtree | Action::UnprotectSubtree | Action::DeleteBackups => self.run_protect_action(a),
            other => self.run_edit_action(other),
        }
    }

    pub fn is_marked(&self, branch_id: &str) -> bool { self.marked.iter().any(|(b, _)| b == branch_id) }

    pub(crate) fn picker_key(&mut self, ev: KeyEvent) {
        let Overlay::Picker(p) = &mut self.overlay else { return };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match ev.code {
            KeyCode::Esc => { self.overlay = Overlay::None; }
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => p.sel = (p.sel + 1).min(p.results.len().saturating_sub(1)),
            KeyCode::Char('p') if ctrl => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('n') if ctrl => p.sel = (p.sel + 1).min(p.results.len().saturating_sub(1)),
            KeyCode::Char('u') if ctrl => { p.query.clear(); p.executed = false; if p.kind != PickerKind::Search { self.refresh_jump(); } else { p.results.clear(); } }
            KeyCode::Backspace => {
                p.query.pop();
                p.executed = false;
                p.error = None;
                if p.kind != PickerKind::Search { self.refresh_jump(); } else { p.results.clear(); }
            }
            KeyCode::Char(c) if !ctrl && !ev.modifiers.contains(KeyModifiers::ALT) => {
                p.query.push(c);
                p.executed = false;
                if p.kind != PickerKind::Search { self.refresh_jump(); } else { p.results.clear(); }
            }
            KeyCode::Enter => {
                if p.kind == PickerKind::Search && !p.executed {
                    let q = p.query.clone();
                    let (ids, err) = self.run_search(&q, 200);
                    let Overlay::Picker(p) = &mut self.overlay else { return };
                    p.results = ids; p.sel = 0; p.executed = true; p.error = err;
                    return;
                }
                if let Some(id) = p.results.get(p.sel).cloned() {
                    let kind = p.kind;
                    let targets = p.targets.clone();
                    self.overlay = Overlay::None;
                    if matches!(kind, PickerKind::CloneTo | PickerKind::MoveTo) {
                        self.apply_to_target(kind, &targets, &id);
                    } else if kind == PickerKind::Command {
                        if let Some((a, _)) = Action::ALL.iter().find(|(a, _)| format!("{a:?}") == id) { self.run_action(*a); }
                    } else {
                        self.open_note(&id, true, true);
                        self.focus = Focus::Note;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use chrono::Datelike;

    /// root -> Alpha(Beta(Gamma), Delta) , Epsilon ; Alpha also holds a list note.
    fn fixture() -> (tempfile::TempDir, App, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        let db = Db::open(&path).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        let a = db.create_note("root", "Alpha", "text", "text/html", "<p>alpha <b>body</b></p>").unwrap();
        let b = db.create_note(&a, "Beta", "text", "text/html", "<ul><li>x</li></ul>").unwrap();
        let g = db.create_note(&b, "Gamma", "code", "text/x-python", "print('hi')").unwrap();
        let d = db.create_note(&a, "Delta", "canvas", "application/json", "{\"elements\":[]}").unwrap();
        let e = db.create_note("root", "Epsilon needle", "text", "text/html", "<p>find the haystack</p>").unwrap();
        drop(db);
        let app = App::new(&path, None).unwrap();
        (dir, app, vec![a, b, g, d, e])
    }

    #[test]
    fn protected_notes_lock_unlock_edit_and_autolock() {
        let (_d, mut app, ids) = fixture();
        let alpha = ids[0].clone();
        let row = app.rows.iter().position(|r| r.note_id == alpha).unwrap();
        app.select_index(row);
        // no password yet: protecting asks for a new one, twice
        app.run_action(Action::ProtectNote);
        assert!(matches!(&app.overlay, Overlay::Prompt(p) if p.secret && matches!(p.kind, PromptKind::NewPassword)));
        typed(&mut app, "s3cret");
        app.handle_key(key(KeyCode::Enter));
        typed(&mut app, "s3cret");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.db.is_unlocked(), "{}", app.status);
        app.run_action(Action::ProtectNote);
        assert!(app.model.notes[&alpha].protected, "{}", app.status);
        assert_eq!(app.model.title_of(&alpha), "Alpha", "unlocked sessions show real titles");
        // locked: placeholder title, locked view, no editing
        app.run_action(Action::LockProtected);
        assert_eq!(app.model.title_of(&alpha), "[protected]");
        app.open_note(&alpha, false, false);
        assert!(matches!(app.view.as_ref().unwrap().kind, Kind::Protected));
        app.run_action(Action::EditContent);
        assert!(!matches!(app.overlay, Overlay::Edit(_)) && app.rich.is_none());
        let shown = screen(&mut app, 100, 30);
        assert!(!shown.contains("alpha body") && shown.contains("locked"), "{shown}");
        // wrong password, then the right one
        app.run_action(Action::UnlockProtected);
        typed(&mut app, "nope");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("wrong password") && !app.db.is_unlocked());
        app.run_action(Action::UnlockProtected);
        typed(&mut app, "s3cret");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.db.is_unlocked() && app.model.title_of(&alpha) == "Alpha");
        assert!(matches!(app.view.as_ref().unwrap().kind, Kind::Html));
        // the external editor is refused (plaintext temp file), the built-in one works
        app.run_action(Action::EditExternal);
        assert!(app.pending_edit.is_none() && app.status.contains("built-in"), "{}", app.status);
        app.run_action(Action::EditSource);
        assert!(matches!(app.overlay, Overlay::Edit(_)));
        app.handle_key(key(KeyCode::Esc));
        app.overlay = Overlay::None;
        app.run_action(Action::EditContent);
        assert!(app.rich.is_some(), "protected notes edit in place too once unlocked");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.rich.is_none());
        // locking while an in-place edit holds unsaved text keeps that text, encrypted, and closes the editor
        app.run_action(Action::EditContent);
        app.handle_key(key(KeyCode::Char('Z')));
        app.check_autolock();
        assert!(app.rich.is_some(), "no auto-lock while an editor holds text");
        app.run_action(Action::LockProtected);
        assert!(app.rich.is_none());
        let lost = app.db.conn.path().map(std::path::PathBuf::from).unwrap().parent().unwrap().join("lost-edits");
        let kept: Vec<_> = std::fs::read_dir(&lost).unwrap().flatten().collect();
        assert!(kept.len() == 1 && kept[0].path().extension().is_some_and(|e| e == "enc"), "{kept:?}");
        assert!(!std::fs::read_to_string(kept[0].path()).unwrap().contains("alpha body"), "the recovery file must not hold plaintext");
        app.run_action(Action::UnlockProtected);
        typed(&mut app, "s3cret");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.db.is_unlocked());
        // locking closes everything built from decrypted text: panels, and the views of other tabs
        app.run_action(Action::ShowRevisions);
        assert!(matches!(app.overlay, Overlay::Panel(_)));
        app.run_action(Action::NewTab);
        app.open_note(&alpha, false, false);
        app.run_action(Action::PrevTab);
        app.lock_protected("locked");
        assert!(matches!(app.overlay, Overlay::None), "the panel is gone");
        assert!(app.tabs.iter().all(|t| t.view.as_ref().is_none_or(|v| v.id != alpha || v.content.is_empty())), "no tab keeps the decrypted text");
        app.run_action(Action::UnlockProtected);
        typed(&mut app, "s3cret");
        app.handle_key(key(KeyCode::Enter));
        app.open_note(&alpha, false, false);
        // inactivity locks the session
        app.db.set_option("protectedSessionTimeout", "1", false).unwrap();
        app.last_input = Instant::now() - std::time::Duration::from_secs(5);
        app.check_autolock();
        assert!(!app.db.is_unlocked() && app.status.contains("inactivity"));
    }

    #[test]
    fn review_regressions_deleted_open_note_and_long_titles() {
        let (_d, mut app, ids) = fixture();
        let alpha = ids[0].clone();
        app.open_note(&alpha, false, false);
        // the open note disappears (as when a sync deletes it): reload and draw must cope
        app.db.conn.execute("UPDATE notes SET isDeleted=1 WHERE noteId=?", [&alpha]).unwrap();
        app.db.conn.execute("UPDATE branches SET isDeleted=1 WHERE noteId=?", [&alpha]).unwrap();
        app.reload();
        assert!(app.view.is_none());
        let _ = screen(&mut app, 100, 30);
        // a very long title doesn't make drawing quadratic
        let long = "w".repeat(200_000);
        let c = app.db.create_note("root", &long, "text", "text/html", "<p>x</p>").unwrap();
        app.reload();
        app.open_note(&c, false, true);
        let t = std::time::Instant::now();
        let _ = screen(&mut app, 100, 30);
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }

    /// Slow in a debug build (the parser alone takes minutes): `cargo test -p trilium-tui -- --ignored deep_html`.
    #[test]
    #[ignore]
    fn deep_html_is_cut_off_instead_of_overflowing_the_stack() {
        let (_d, mut app, _ids) = fixture();
        let deep = format!("{}x{}", "<div>".repeat(40_000), "</div>".repeat(40_000));
        let b = app.db.create_note("root", "Deep", "text", "text/html", &deep).unwrap();
        app.reload();
        app.open_note(&b, false, true);
        let _ = screen(&mut app, 100, 30);
    }

    fn labels(app: &App) -> Vec<String> { app.rows.iter().map(|r| format!("{}{}", "-".repeat(r.depth), r.label)).collect() }
    fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }
    fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }
    fn typed(app: &mut App, s: &str) { for c in s.chars() { app.handle_key(key(KeyCode::Char(c))); } }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn tree_expand_collapse_and_navigation_open_notes() {
        let (_d, mut app, ids) = fixture();
        assert_eq!(labels(&app), ["Alpha", "Epsilon needle"]);
        assert_eq!(app.view.as_ref().unwrap().id, ids[0], "first note opens on start");
        app.handle_key(key(KeyCode::Right));
        assert_eq!(labels(&app), ["Alpha", "-Beta", "-Delta", "Epsilon needle"]);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.view.as_ref().unwrap().id, ids[1], "moving the cursor opens the note");
        app.handle_key(key(KeyCode::Left)); // Beta is collapsed -> goes to parent
        assert_eq!(app.rows[app.sel].label, "Alpha");
        app.handle_key(key(KeyCode::Left)); // collapse Alpha
        assert_eq!(labels(&app), ["Alpha", "Epsilon needle"]);
    }

    #[test]
    fn jump_reveals_note_and_history_works() {
        let (_d, mut app, ids) = fixture();
        app.handle_key(ctrl('j'));
        typed(&mut app, "gamma");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, ids[2]);
        assert_eq!(labels(&app), ["Alpha", "-Beta", "--Gamma", "-Delta", "Epsilon needle"], "ancestors expanded");
        assert_eq!(app.rows[app.sel].label, "Gamma");
        assert_eq!(app.view.as_ref().unwrap().kind, Kind::Code);
        app.run_action(Action::Back);
        assert_eq!(app.view.as_ref().unwrap().id, ids[0]);
        app.run_action(Action::Forward);
        assert_eq!(app.view.as_ref().unwrap().id, ids[2]);
    }

    #[test]
    fn search_runs_on_enter_then_opens() {
        let (_d, mut app, ids) = fixture();
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "haystack");
        assert!(matches!(&app.overlay, Overlay::Picker(p) if p.results.is_empty()), "no search before Enter");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Picker(p) if p.results == vec![ids[4].clone()]));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.view.as_ref().unwrap().id, ids[4]);
        assert_eq!(app.focus, Focus::Note);
    }

    #[test]
    fn unsupported_types_show_source_and_browser_hint() {
        let (_d, mut app, ids) = fixture();
        app.open_note(&ids[3], true, true);
        let v = app.view.as_ref().unwrap();
        assert!(matches!(&v.kind, Kind::Unsupported(t) if t == "canvas"));
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("need the browser renderer"), "{s}");
        assert!(s.contains("elements"), "source is shown:\n{s}");
    }

    #[test]
    fn rendered_and_source_views_and_toggle() {
        let (_d, mut app, ids) = fixture();
        app.open_note(&ids[0], true, true);
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("alpha body") && !s.contains("<b>"), "{s}");
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('s')));
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("<b>body</b>") && s.contains("[text source]"), "{s}");
    }

    #[test]
    fn hoist_unhoist_and_collapse() {
        let (_d, mut app, _ids) = fixture();
        app.run_action(Action::Hoist);
        assert_eq!(labels(&app), ["Beta", "Delta"]);
        app.run_action(Action::Unhoist);
        assert!(labels(&app).contains(&"Alpha".to_string()));
        app.run_action(Action::CollapseTree);
        assert_eq!(labels(&app).len(), 2, "{:?}", labels(&app));
    }

    #[test]
    fn narrow_terminal_shows_only_focused_pane() {
        let (_d, mut app, _ids) = fixture();
        let s = screen(&mut app, 60, 20);
        assert!(s.contains("Notes") && !s.contains("[text]"));
        app.handle_key(key(KeyCode::Tab));
        let s = screen(&mut app, 60, 20);
        assert!(s.contains("[text]") && !s.contains("Epsilon"));
    }

    #[test]
    fn help_overlay_lists_trilium_bindings() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(key(KeyCode::F(1)));
        let s = screen(&mut app, 100, 40);
        assert!(s.contains("Ctrl+J") && s.contains("jump to note"), "{s}");
        app.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(app.overlay, Overlay::None));
    }

    fn ctrl_key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::CONTROL) }

    #[test]
    fn rename_with_enter_in_tree_only() {
        let (_d, mut app, ids) = fixture();
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None), "Enter in the note pane must not rename");
        app.focus = Focus::Tree;
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::Prompt(_)));
        for _ in 0..5 { app.handle_key(key(KeyCode::Backspace)); }
        typed(&mut app, "Omega");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(labels(&app)[0], "Omega");
        assert_eq!(app.model.title_of(&ids[0]), "Omega");
        // the change is recorded for sync
        let n: i64 = app.db.conn.query_row("SELECT COUNT(*) FROM entity_changes WHERE entityName='notes' AND entityId=?", [&ids[0]], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn create_child_and_sibling() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(ctrl('p'));
        typed(&mut app, "Child one");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, app.model.notes.values().find(|n| n.title == "Child one").unwrap().id);
        assert!(labels(&app).contains(&"-Child one".to_string()) || labels(&app).iter().any(|l| l.ends_with("Child one")), "{:?}", labels(&app));
        app.focus = Focus::Tree;
        app.handle_key(ctrl('o'));
        typed(&mut app, "Sibling");
        app.handle_key(key(KeyCode::Enter));
        let l = labels(&app);
        let i = l.iter().position(|x| x.ends_with("Child one")).unwrap();
        assert!(l[i + 1].ends_with("Sibling"), "{l:?}");
    }

    #[test]
    fn delete_asks_for_confirmation() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(key(KeyCode::Delete));
        let Overlay::Confirm(c) = &app.overlay else { panic!("expected confirm") };
        assert!(c.message.contains("Alpha") && c.message.contains("descendant"), "{}", c.message);
        app.handle_key(key(KeyCode::Char('n')));
        assert!(labels(&app).contains(&"Alpha".to_string()), "cancel keeps it");
        app.handle_key(key(KeyCode::Delete));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(labels(&app), ["Epsilon needle"]);
        assert_eq!(app.view.as_ref().unwrap().id, app.model.notes.values().find(|n| n.title == "Epsilon needle").unwrap().id);
    }

    #[test]
    fn move_keys_reorder_and_follow_the_note() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(ctrl_key(KeyCode::Down));
        assert_eq!(labels(&app), ["Epsilon needle", "Alpha"]);
        assert_eq!(app.rows[app.sel].label, "Alpha", "cursor follows the moved note");
        app.handle_key(ctrl_key(KeyCode::Up));
        assert_eq!(labels(&app), ["Alpha", "Epsilon needle"]);
        app.handle_key(ctrl_key(KeyCode::Right)); // Alpha has no previous sibling
        assert_eq!(app.status, "can't move further");
        app.handle_key(key(KeyCode::Down)); // Epsilon
        app.handle_key(ctrl_key(KeyCode::Right)); // into Alpha
        let parent_of_eps = app.model.parents[&app.view.as_ref().unwrap().id][0].clone();
        assert_eq!(app.model.title_of(&parent_of_eps), "Alpha");
        app.handle_key(ctrl_key(KeyCode::Left)); // back out, after Alpha
        let parent_of_eps = app.model.parents[&app.view.as_ref().unwrap().id][0].clone();
        assert_eq!(parent_of_eps, "root");
    }

    #[test]
    fn external_edit_result_is_saved_and_shown() {
        let (_d, mut app, ids) = fixture();
        app.finish_edit(&ids[0], Ok(Some("<p>rewritten</p>".into())));
        assert_eq!(app.status, "saved");
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("rewritten"), "{s}");
        app.finish_edit(&ids[0], Ok(None));
        assert_eq!(app.status, "no changes");
        app.finish_edit(&ids[0], Err(anyhow::anyhow!("boom")));
        assert!(app.status.contains("boom"));
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('E')));
        assert_eq!(app.pending_edit.as_deref(), Some(ids[0].as_str()));
        let (content, ext) = app.edit_payload(&ids[0]).unwrap();
        assert_eq!((content.as_str(), ext), ("<p>rewritten</p>", ".html"));
    }

    #[test]
    fn labels_and_relations_via_prompt() {
        let (_d, mut app, ids) = fixture();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        typed(&mut app, "#status=done");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        typed(&mut app, "friend=epsilon needle");
        app.handle_key(key(KeyCode::Enter));
        let attrs = &app.view.as_ref().unwrap().attrs;
        assert!(attrs.iter().any(|a| a.kind == "label" && a.name == "status" && a.value == "done"), "{attrs:?}");
        assert!(attrs.iter().any(|a| a.kind == "relation" && a.name == "friend" && a.value == ids[4]), "{attrs:?}");
        let s = screen(&mut app, 120, 24);
        assert!(s.contains("#status=done") && s.contains("~friend=Epsilon needle"), "{s}");
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        typed(&mut app, "x=no such note");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("no note"), "{}", app.status);
    }

    #[test]
    fn command_palette_runs_actions_by_name() {
        let (_d, mut app, ids) = fixture();
        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert!(matches!(&app.overlay, Overlay::Picker(p) if p.kind == PickerKind::Command && p.results.len() == Action::ALL.len()));
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("Command palette") && s.contains("Ctrl+J"), "{s}");
        typed(&mut app, "renam");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Prompt(_)), "palette ran Rename");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char(':')));
        typed(&mut app, "revision");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status, "revision saved");
        let n: i64 = app.db.conn.query_row("SELECT COUNT(*) FROM revisions WHERE noteId=? AND source='manual'", [&ids[0]], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn tabs_have_independent_notes_and_history() {
        let (_d, mut app, ids) = fixture();
        app.open_note(&ids[0], true, true);
        app.run_action(Action::NewTab);
        assert_eq!((app.tabs.len(), app.active_tab), (2, 1));
        assert_eq!(app.view.as_ref().unwrap().id, ids[0], "new tab starts on the current note");
        app.open_note(&ids[4], true, true);
        assert_eq!(app.tab_titles().iter().map(|t| t.0.clone()).collect::<Vec<_>>(), ["Alpha", "Epsilon needle"]);
        app.run_action(Action::GotoTab(0));
        assert_eq!(app.view.as_ref().unwrap().id, ids[0]);
        app.run_action(Action::NextTab);
        assert_eq!(app.view.as_ref().unwrap().id, ids[4]);
        app.run_action(Action::Back);
        assert_eq!(app.view.as_ref().unwrap().id, ids[0], "history is per tab");
        let s = screen(&mut app, 100, 24);
        assert!(s.lines().next().unwrap().contains("1 Alpha"), "{s}");
        app.run_action(Action::CloseTab);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.view.as_ref().unwrap().id, ids[0]);
        app.run_action(Action::CloseTab);
        assert!(app.status.contains("last tab"));
        app.run_action(Action::ReopenTab);
        assert_eq!(app.tabs.len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert_eq!(app.active_tab, 0);
    }

    #[test]
    fn today_creates_journal_and_opens_it() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(key(KeyCode::Char('t')));
        let v = app.view.as_ref().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(app.db.label_value(&v.id, "dateNote").unwrap().as_deref(), Some(today.as_str()));
        assert_eq!(app.focus, Focus::Note);
        let l = labels(&app);
        assert!(l.iter().any(|x| x == "Calendar"), "{l:?}");
        let again = v.id.clone();
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.view.as_ref().unwrap().id, again, "second call finds the same note");
    }

    #[test]
    fn search_overlay_speaks_trilium_syntax_and_reports_errors() {
        let (_d, mut app, ids) = fixture();
        app.db.add_attribute(&ids[4], "label", "status", "done", false).unwrap();
        app.reload();
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "#status = done");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Picker(p) if p.results == vec![ids[4].clone()] && p.error.is_none()));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "#a and #b or #c");
        app.handle_key(key(KeyCode::Enter));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Mixed usage of AND/OR"), "{s}");
        // orderBy and property queries work from the UI as well
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('/')));
        typed(&mut app, "note.type = text orderBy note.title desc limit 2");
        app.handle_key(key(KeyCode::Enter));
        let Overlay::Picker(p) = &app.overlay else { panic!() };
        assert_eq!(p.results.len(), 2);
        // fewer than 5 results: upstream re-sorts by score/depth/path title (so "Epsilon needle" precedes "root")
        assert_eq!((app.model.title_of(&p.results[0]), app.model.title_of(&p.results[1])), ("Epsilon needle", "root"));
    }

    #[test]
    fn saved_search_note_lists_results_and_enter_opens_one() {
        let (_d, mut app, ids) = fixture();
        let sid = app.db.create_note("root", "My search", "search", "application/json", "{}").unwrap();
        app.db.add_attribute(&sid, "label", "searchString", "note.title *=* needle or note.title *=* alpha", false).unwrap();
        app.reload();
        app.open_note(&sid, true, true);
        assert!(matches!(&app.view.as_ref().unwrap().kind, Kind::SearchResults(r) if r.len() == 2));
        let s = screen(&mut app, 110, 24);
        assert!(s.contains("Saved search") && s.contains("Epsilon needle") && s.contains("Alpha"), "{s}");
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let opened = app.view.as_ref().unwrap().id.clone();
        assert!(opened == ids[0] || opened == ids[4]);
    }

    #[test]
    fn collection_rows_are_navigable() {
        let (_d, mut app, ids) = fixture();
        let book = app.db.create_note("root", "Shelf", "book", "text/html", "").unwrap();
        for t in ["One", "Two", "Three"] { app.db.create_note(&book, t, "text", "text/html", t).unwrap(); }
        app.reload();
        app.open_note(&book, true, true);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.model.title_of(&app.view.as_ref().unwrap().id), "Three");
        let _ = ids;
    }

    #[test]
    fn hrefs_resolve_to_note_ids() {
        assert_eq!(note_id_from_href("#root/abc/DEF_1"), Some("DEF_1".into()));
        assert_eq!(note_id_from_href("http://srv:8080/#root/a/b/XYZ?x=1"), Some("XYZ".into()));
        assert_eq!(note_id_from_href("#NOTE9"), Some("NOTE9".into()));
        assert_eq!(note_id_from_href("https://example.com/page"), None);
        assert_eq!(note_id_from_href("#root"), None);
    }

    #[test]
    fn links_can_be_selected_and_followed() {
        let (_d, mut app, ids) = fixture();
        let hub = app.db.create_note("root", "Hub", "text", "text/html",
            &format!(r##"<p>go to <a class="reference-link" href="#root/{}">Epsilon</a> or <a href="https://example.com/x">the web</a> or <a href="#root/NOSUCHNOTE">ghost</a></p>"##, ids[4])).unwrap();
        app.reload();
        app.open_note(&hub, true, true);
        app.focus = Focus::Note;
        let _ = screen(&mut app, 100, 24); // renders and collects links
        assert_eq!(app.view.as_ref().unwrap().links.len(), 3);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("no link selected"), "{}", app.status);
        app.handle_key(key(KeyCode::Char(']')));
        assert!(app.status.contains("\"Epsilon\"") && app.status.contains(&ids[4]), "{}", app.status);
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("Epsilon"));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, ids[4], "internal link opens the note");
        app.run_action(Action::Back);
        assert_eq!(app.view.as_ref().unwrap().id, hub, "history leads back to the linking note");
        let _ = screen(&mut app, 100, 24);
        app.handle_key(key(KeyCode::Char('['))); // wraps to the last link (ghost)
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("not found"), "{}", app.status);
        assert_eq!(app.view.as_ref().unwrap().id, hub);
    }

    fn open_tab(app: &mut App, t: crate::panel::PanelTab) { app.open_panel(t); }

    #[test]
    fn panel_attributes_edit_and_delete() {
        use crate::panel::PanelTab;
        let (_d, mut app, ids) = fixture();
        app.db.add_attribute(&ids[0], "label", "mood", "ok", false).unwrap();
        app.reload();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Attributes 1") && s.contains("#mood") && s.contains("= ok"), "{s}");
        app.handle_key(key(KeyCode::Char('e')));
        assert!(matches!(&app.overlay, Overlay::Prompt(p) if p.text.iter().collect::<String>() == "ok"));
        for _ in 0..2 { app.handle_key(key(KeyCode::Backspace)); }
        typed(&mut app, "great");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Panel(p) if p.tab == PanelTab::Attributes && p.attrs[0].value == "great"), "panel reopens after the prompt");
        app.handle_key(key(KeyCode::Char('d')));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Delete #mood? (y/n)"), "{s}");
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(&app.overlay, Overlay::Panel(p) if p.attrs.is_empty()));
        app.handle_key(key(KeyCode::Char('a')));
        typed(&mut app, "tag=1");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Panel(p) if p.attrs.len() == 1 && p.attrs[0].name == "tag"));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn panel_backlinks_navigate() {
        let (_d, mut app, ids) = fixture();
        let linker = app.db.create_note("root", "Linker", "text", "text/html", &format!(r##"<a href="#root/{}">e</a>"##, ids[4])).unwrap();
        app.db.set_note_content(&linker, &format!(r##"<a href="#root/{}">e</a>!"##, ids[4])).unwrap();
        app.reload();
        app.open_note(&ids[4], true, true);
        app.focus = Focus::Tree;
        app.handle_key(key(KeyCode::Char('b')));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Backlinks 1") && s.contains("Linker"), "{s}");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, linker);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn panel_revisions_show_diff_and_restore() {
        let (_d, mut app, ids) = fixture();
        app.db.set_option("revisionSnapshotTimeInterval", "0", true).unwrap();
        app.db.set_note_content(&ids[0], "<p>second version</p>").unwrap();
        app.db.set_note_content(&ids[0], "<p>third version</p>").unwrap();
        app.reload();
        app.open_note(&ids[0], true, true);
        open_tab(&mut app, crate::panel::PanelTab::Revisions);
        app.handle_key(key(KeyCode::Down)); // oldest revision is last; select the first revision of two
        app.handle_key(key(KeyCode::Enter));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("diff: revision -> current") && s.contains("- ") && s.contains("+ "), "{s}");
        assert!(s.contains("alpha body") || s.contains("second version"), "{s}");
        app.handle_key(key(KeyCode::Char('v')));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("revision text"), "{s}");
        app.handle_key(key(KeyCode::Char('r')));
        assert!(screen(&mut app, 120, 30).contains("Restore revision"));
        app.handle_key(key(KeyCode::Char('y')));
        let content = app.db.note_content(&ids[0]).unwrap().unwrap();
        assert!(content.contains("alpha") || content.contains("second"), "restored: {content}");
        assert_ne!(content, "<p>third version</p>");
        assert!(app.db.revisions(&ids[0]).unwrap().iter().any(|r| r.source == "restore"));
    }

    #[test]
    fn panel_info_and_attachments() {
        let _env = DL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (d, mut app, ids) = fixture();
        app.db.add_attachment(&ids[0], "note.txt", "text/plain", "file", b"hello attachment").unwrap();
        app.open_note(&ids[0], true, true);
        app.handle_key(key(KeyCode::Char('i')));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Note ID") && s.contains(&ids[0]) && s.contains("Alpha"), "{s}");
        let dl = d.path().join("downloads");
        std::fs::create_dir_all(&dl).unwrap();
        unsafe { std::env::set_var("TRILIUM_TUI_DOWNLOAD_DIR", &dl); }
        app.handle_key(key(KeyCode::Char('5')));
        assert!(screen(&mut app, 120, 30).contains("note.txt"));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(std::fs::read(dl.join("note.txt")).unwrap(), b"hello attachment");
        app.handle_key(key(KeyCode::Enter)); // second save must not overwrite
        assert!(dl.join("1-note.txt").exists());
    }

    /// Serialises the tests that point TRILIUM_TUI_DOWNLOAD_DIR at their own temp dir.
    static DL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn shift(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::SHIFT) }

    #[test]
    fn copy_paste_clones_and_cut_paste_moves() {
        let (_d, mut app, ids) = fixture();
        // copy Epsilon, paste into Alpha: a second placement of the same note
        app.select_index(1);
        assert_eq!(app.rows[app.sel].note_id, ids[4]);
        app.handle_key(ctrl('c'));
        assert!(app.status.contains("copied") && !app.quit, "{}", app.status);
        app.select_index(0);
        app.handle_key(ctrl('v'));
        assert!(app.status.contains("pasted 1"), "{}", app.status);
        assert_eq!(app.model.parents[&ids[4]].len(), 2, "cloned, not moved");
        assert!(app.clipboard.is_some(), "a copy can be pasted again");
        // cut Beta (child of Alpha) and paste into Epsilon: moved
        app.rows.iter().position(|r| r.note_id == ids[1]).map(|i| app.select_index(i));
        assert_eq!(app.rows[app.sel].note_id, ids[1]);
        app.handle_key(ctrl('x'));
        let eps = app.rows.iter().position(|r| r.note_id == ids[4]).unwrap();
        app.select_index(eps);
        app.handle_key(ctrl('v'));
        assert_eq!(app.model.parents[&ids[1]].len(), 1);
        assert_eq!(app.model.title_of(&app.model.parents[&ids[1]][0]), "Epsilon needle");
        assert!(app.clipboard.is_none(), "a cut is consumed by pasting");
        // pasting a note into its own subtree is refused and reported, nothing changes
        let alpha = app.rows.iter().position(|r| r.note_id == ids[0]).unwrap();
        app.select_index(alpha);
        app.handle_key(ctrl('c'));
        app.run_action(Action::CollapseTree);
        app.expanded.insert(format!("root_{}", ids[0]));
        app.rebuild_rows();
        let delta = app.rows.iter().position(|r| r.note_id == ids[3]).unwrap();
        app.select_index(delta);
        app.handle_key(ctrl('v'));
        assert!(app.status.contains("failed") && app.status.contains("subtree"), "{}", app.status);
        assert_eq!(app.model.parents[&ids[0]].len(), 1);
    }

    #[test]
    fn marking_rows_and_deleting_several() {
        let (_d, mut app, ids) = fixture();
        app.handle_key(shift(KeyCode::Down));
        assert_eq!(app.marked.len(), 2, "shift+down marks the current and the next row");
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("Alpha") && s.contains("Epsilon"), "{s}");
        app.handle_key(key(KeyCode::Delete));
        let Overlay::Confirm(c) = &app.overlay else { panic!() };
        assert!(c.message.contains("2 selected"), "{}", c.message);
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.rows.is_empty() && app.marked.is_empty(), "{:?}", labels(&app));
        assert!(app.status.contains("deleted 2"));
        let _ = ids;
    }

    #[test]
    fn select_all_siblings_and_escape() {
        let (_d, mut app, _ids) = fixture();
        app.handle_key(ctrl('a'));
        assert_eq!(app.marked.len(), 2);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.marked.is_empty());
        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.marked.len(), 1);
        app.handle_key(key(KeyCode::Char('m')));
        assert!(app.marked.is_empty());
    }

    #[test]
    fn clone_and_move_to_a_picked_note() {
        let (_d, mut app, ids) = fixture();
        app.select_index(1); // Epsilon
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert!(matches!(&app.overlay, Overlay::Picker(p) if p.kind == PickerKind::CloneTo && p.targets.len() == 1));
        typed(&mut app, "alpha");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("cloned 1"), "{}", app.status);
        assert_eq!(app.model.parents[&ids[4]].len(), 2);
        // moving into a descendant is refused and reported
        let alpha = app.rows.iter().position(|r| r.note_id == ids[0]).unwrap();
        app.select_index(alpha);
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        typed(&mut app, "beta");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("failed") && app.status.contains("subtree"), "{}", app.status);
        assert_eq!(app.model.title_of(&app.model.parents[&ids[0]][0]), "root", "nothing moved");
    }

    #[test]
    fn duplicate_subtree_from_the_palette() {
        let (_d, mut app, ids) = fixture();
        app.select_index(0); // Alpha with children
        app.run_action(Action::DuplicateNote);
        assert!(app.status.contains("duplicated 1"), "{}", app.status);
        let l = labels(&app);
        assert!(l.iter().any(|x| x == "Alpha (dup)"), "{l:?}");
        let dup = app.model.notes.values().find(|n| n.title == "Alpha (dup)").unwrap().id.clone();
        assert_eq!(app.model.children[&dup].len(), app.model.children[&ids[0]].len());
    }

    /// A small PNG made with the image crate (no fixture files).
    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 160]));
        let mut out = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn image_notes_are_drawn_zoomed_and_saved() {
        let _env = DL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (d, mut app, _ids) = fixture();
        let img = app.db.create_file_note("root", "Gradient", "image/png", &png(64, 48)).unwrap();
        app.reload();
        app.open_note(&img, true, true);
        assert_eq!(app.view.as_ref().unwrap().kind, Kind::Image);
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("Gradient.png") && s.contains("64x48") && s.contains("image/png"), "{s}");
        assert!(s.contains('▀') || s.contains('▄'), "half-block pixels are drawn in tests:\n{s}");
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::Image(_)));
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("any other key closes"), "{s}");
        let dl = d.path().join("dl");
        std::fs::create_dir_all(&dl).unwrap();
        unsafe { std::env::set_var("TRILIUM_TUI_DOWNLOAD_DIR", &dl); }
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.status.contains("saved"), "{}", app.status);
        assert_eq!(std::fs::read(dl.join("Gradient.png")).unwrap(), png(64, 48));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        app.handle_key(key(KeyCode::Char('x'))); // note-level save as well, never overwriting
        assert!(dl.join("1-Gradient.png").exists());
    }

    #[test]
    fn embedded_images_can_be_selected_and_viewed() {
        let (_d, mut app, _ids) = fixture();
        let note = app.db.create_note("root", "With picture", "text", "text/html", "").unwrap();
        let att = app.db.add_attachment(&note, "pic.png", "image/png", "image", &png(40, 40)).unwrap();
        app.db.set_note_content(&note, &format!(r##"<p>before</p><figure class="image"><img src="api/attachments/{att}/image/pic.png" alt="my picture"></figure><p>after <a href="#root/x">link</a></p>"##)).unwrap();
        app.reload();
        app.open_note(&note, true, true);
        app.focus = Focus::Note;
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("[image: my picture]"), "{s}");
        app.handle_key(key(KeyCode::Char(']')));
        assert!(app.status.contains("my picture"), "{}", app.status);
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Image(p) if p.proto.is_some() && p.dims == (40, 40)));
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("my picture") && s.contains("40x40"), "{s}");
        app.handle_key(key(KeyCode::Esc));
        // missing source is reported, not fatal
        app.db.set_note_content(&note, r#"<img src="api/attachments/NOPE/image/x.png" alt="gone">"#).unwrap();
        app.reload();
        app.open_note(&note, false, false);
        let _ = screen(&mut app, 100, 24);
        app.handle_key(key(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("isn't in the replica"), "{}", app.status);
    }

    #[test]
    fn undecodable_and_svg_images_explain_themselves() {
        let (_d, mut app, _ids) = fixture();
        let bad = app.db.create_file_note("root", "Broken", "image/png", b"definitely not a png").unwrap();
        app.reload();
        app.open_note(&bad, true, true);
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("can't decode"), "{s}");
        let svg = app.db.create_file_note("root", "Vector", "image/svg+xml", b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
        app.reload();
        app.open_note(&svg, true, true);
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("[file]") || s.contains("Binary content") || s.contains("save it"), "svg is a file note with save/open hints:\n{s}");
    }

    #[test]
    fn attachments_tab_views_images() {
        let (_d, mut app, ids) = fixture();
        app.db.add_attachment(&ids[0], "shot.png", "image/png", "file", &png(30, 20)).unwrap();
        app.open_note(&ids[0], true, true);
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('5')));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Overlay::Image(p) if p.dims == (30, 20)));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::Panel(_)), "closing the viewer returns to the panel");
    }

    #[test]
    fn export_and_import_from_the_palette() {
        let (d, mut app, ids) = fixture();
        let zip = d.path().join("out.zip");
        app.select_index(0); // Alpha with children
        app.run_action(Action::ExportNote);
        let Overlay::Prompt(p) = &mut app.overlay else { panic!("expected a prompt") };
        assert!(p.text.iter().collect::<String>().ends_with("Alpha.zip"));
        p.text = zip.to_string_lossy().chars().collect();
        p.cursor = p.text.len();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("exported 4 notes"), "{}", app.status);
        assert!(zip.exists());
        // import it under Epsilon
        let eps = app.rows.iter().position(|r| r.note_id == ids[4]).unwrap();
        app.select_index(eps);
        app.run_action(Action::ImportHere);
        typed(&mut app, &zip.to_string_lossy());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("imported 4 notes"), "{}", app.status);
        let kids: Vec<String> = app.model.children[&ids[4]].iter().map(|b| app.model.title_of(&b.note_id).to_string()).collect();
        assert_eq!(kids, ["Alpha"]);
        // a bad path is reported, not fatal
        app.run_action(Action::ImportHere);
        typed(&mut app, "/definitely/not/here.zip");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("import failed"), "{}", app.status);
    }

    #[test]
    fn opml_site_enex_and_week_notes_from_the_palette() {
        let (d, mut app, ids) = fixture();
        app.select_index(0);
        // OPML out and back in
        let opml = d.path().join("out.opml");
        app.run_action(Action::ExportOpml);
        let Overlay::Prompt(p) = &mut app.overlay else { panic!("export prompt") };
        p.text = opml.to_string_lossy().chars().collect();
        p.cursor = p.text.len();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("exported 4 notes"), "{}", app.status);
        let eps = app.rows.iter().position(|r| r.note_id == ids[4]).unwrap();
        app.select_index(eps);
        app.run_action(Action::ImportHere);
        typed(&mut app, &opml.to_string_lossy());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("imported 4 notes"), "{}", app.status);
        // a static website
        let site = d.path().join("out.site.zip");
        app.select_index(0);
        app.run_action(Action::ExportSite);
        let Overlay::Prompt(p) = &mut app.overlay else { panic!("export prompt") };
        p.text = site.to_string_lossy().chars().collect();
        p.cursor = p.text.len();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("as a website"), "{}", app.status);
        assert!(std::fs::metadata(&site).unwrap().len() > 500);
        // an Evernote export
        let enex = d.path().join("Notebook.enex");
        std::fs::write(&enex, "<en-export><note><title>From Evernote</title><content><![CDATA[<en-note><div>hi</div></en-note>]]></content></note></en-export>").unwrap();
        app.run_action(Action::ImportHere);
        typed(&mut app, &enex.to_string_lossy());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("imported 2 notes"), "{}", app.status);
        assert!(app.model.notes.values().any(|n| n.title == "From Evernote"));
        // Markdown and Evernote exports, then an Obsidian vault folder
        for (action, file, want) in [(Action::ExportMarkdown, "out.md.zip", "as Markdown"), (Action::ExportEnex, "out.enex", " notes to ")] {
            app.select_index(0);
            app.run_action(action);
            let target = d.path().join(file);
            let Overlay::Prompt(p) = &mut app.overlay else { panic!("export prompt") };
            p.text = target.to_string_lossy().chars().collect();
            p.cursor = p.text.len();
            app.handle_key(key(KeyCode::Enter));
            assert!(app.status.contains(want) && target.exists(), "{}", app.status);
        }
        let vault = d.path().join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::write(vault.join("Idea.md"), "---\ntags: [spark]\n---\nA [[Plan]] for **later**").unwrap();
        std::fs::write(vault.join("Plan.md"), "the plan").unwrap();
        app.select_index(0);
        app.run_action(Action::ImportObsidian);
        typed(&mut app, &vault.to_string_lossy());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.status.contains("imported 3 notes"), "{}", app.status);
        assert!(app.model.notes.values().any(|n| n.title == "Idea"));
        // week notes only exist when the calendar root asks for them
        app.run_action(Action::OpenWeek);
        assert!(app.status.contains("enableWeekNote"), "{}", app.status);
        let root = app.db.calendar_root(true).unwrap().unwrap();
        app.db.add_attribute(&root, "label", "enableWeekNote", "", false).unwrap();
        app.run_action(Action::OpenWeek);
        let id = app.view.as_ref().unwrap().id.clone();
        assert!(app.model.title_of(&id).starts_with("Week "), "{}", app.model.title_of(&id));
        app.run_action(Action::OpenQuarter);
        let id = app.view.as_ref().unwrap().id.clone();
        assert!(app.model.title_of(&id).starts_with("Quarter "), "{}", app.model.title_of(&id));
    }

    fn collection(app: &mut App, title: &str, view: &str) -> String {
        let id = app.db.create_note("root", title, "book", "text/html", "").unwrap();
        app.db.add_attribute(&id, "label", "viewType", view, false).unwrap();
        id
    }

    #[test]
    fn list_and_table_views() {
        let (_d, mut app, _ids) = fixture();
        let list = collection(&mut app, "Plain list", "list");
        let t = app.db.create_note(&list, "First", "text", "text/html", "<p>preview text <b>here</b></p>").unwrap();
        let _ = t;
        let table = collection(&mut app, "Books table", "table");
        for (title, year, author) in [("Dune", "1965", "Herbert"), ("Neuromancer", "1984", "Gibson")] {
            let n = app.db.create_note(&table, title, "text", "text/html", "").unwrap();
            app.db.add_attribute(&n, "label", "year", year, false).unwrap();
            app.db.add_attribute(&n, "label", "author", author, false).unwrap();
        }
        app.reload();
        app.open_note(&list, true, true);
        let s = screen(&mut app, 110, 24);
        assert!(s.contains("First") && s.contains("preview text here"), "{s}");
        app.open_note(&table, true, true);
        let s = screen(&mut app, 110, 24);
        assert!(s.contains("Title") && s.contains("author") && s.contains("year"), "{s}");
        assert!(s.contains("Dune") && s.contains("1965") && s.contains("Herbert") && s.contains("Gibson"), "{s}");
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.model.title_of(&app.view.as_ref().unwrap().id), "Neuromancer");
        // geo list shows the coordinates label
        let geo = collection(&mut app, "Places", "geoMap");
        let p = app.db.create_note(&geo, "Rome", "text", "text/html", "").unwrap();
        app.db.add_attribute(&p, "label", "geolocation", "41.9,12.5", false).unwrap();
        app.reload();
        app.open_note(&geo, true, true);
        let s = screen(&mut app, 110, 24);
        assert!(s.contains("geolocation") && s.contains("41.9,12.5") && s.contains("Rome"), "{s}");
    }

    #[test]
    fn board_view_moves_and_creates_cards() {
        let (_d, mut app, _ids) = fixture();
        let board = collection(&mut app, "Tasks", "board");
        for (t, st) in [("Write code", "Todo"), ("Test it", "Todo"), ("Ship", "Doing"), ("Orphan", "")] {
            let n = app.db.create_note(&board, t, "text", "text/html", "").unwrap();
            if !st.is_empty() { app.db.add_attribute(&n, "label", "status", st, false).unwrap(); }
        }
        app.reload();
        app.open_note(&board, true, true);
        app.focus = Focus::Note;
        let s = screen(&mut app, 120, 28);
        assert!(s.contains("(no value) (1)") && s.contains("Todo (2)") && s.contains("Doing (1)"), "{s}");
        assert!(s.contains("Write code") && s.contains("Ship") && s.contains("Orphan"), "{s}");
        // move the orphan card right (into Todo), by label update
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        let orphan = app.model.notes.values().find(|n| n.title == "Orphan").unwrap().id.clone();
        assert_eq!(app.db.label_value(&orphan, "status").unwrap().as_deref(), Some("Todo"));
        let s = screen(&mut app, 120, 28);
        assert!(s.contains("Todo (3)"), "{s}");
        // and back to the no-value column removes the label
        app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.db.label_value(&orphan, "status").unwrap(), None);
        // new card in the Doing column
        app.handle_key(key(KeyCode::Right)); // Todo
        app.handle_key(key(KeyCode::Right)); // Doing
        app.handle_key(key(KeyCode::Char('n')));
        typed(&mut app, "Review");
        app.handle_key(key(KeyCode::Enter));
        let review = app.model.notes.values().find(|n| n.title == "Review").unwrap().id.clone();
        assert_eq!(app.db.label_value(&review, "status").unwrap().as_deref(), Some("Doing"));
        assert!(screen(&mut app, 120, 28).contains("Doing (2)"));
        // configured column order from board.json wins
        app.db.add_attachment(&board, "board.json", "application/json", "viewConfig", br#"{"columns":[{"value":"Doing"},{"value":"Todo"},{"value":"Done"}]}"#).unwrap();
        app.reload();
        app.open_note(&board, false, false);
        let d = app.board_data(&board);
        assert_eq!(d.columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["", "Doing", "Todo", "Done"]);
    }

    #[test]
    fn journal_calendar_opens_and_creates_day_notes() {
        let (_d, mut app, _ids) = fixture();
        let today_note = app.db.today_note().unwrap();
        let root = app.db.calendar_root_id().unwrap().unwrap();
        // a journal root shown as a calendar collection is a book note
        app.db.set_note_type(&root, "book", "text/html").unwrap();
        app.db.add_attribute(&root, "label", "viewType", "calendar", false).unwrap();
        app.reload();
        app.open_note(&root, true, true);
        app.focus = Focus::Note;
        let now = chrono::Local::now().date_naive();
        let s = screen(&mut app, 110, 30);
        assert!(s.contains(&now.format("%Y").to_string()) && s.contains("Mo") && s.contains("Su"), "{s}");
        assert!(s.contains(&now.format("%A, %-d %B %Y").to_string()), "cursor starts on today:\n{s}");
        assert!(s.contains(app.model.title_of(&today_note)), "today's day note is listed:\n{s}");
        // Enter on today opens the existing day note
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, today_note);
        // go back, move to another day and open: a new day note is created
        app.run_action(Action::Back);
        let before = app.model.notes.len();
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Enter));
        let tomorrow = (now + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let v = app.view.as_ref().unwrap();
        assert_eq!(app.db.label_value(&v.id, "dateNote").unwrap().as_deref(), Some(tomorrow.as_str()));
        assert!(app.model.notes.len() > before);
        // month navigation
        app.run_action(Action::Back);
        app.handle_key(key(KeyCode::Char(']')));
        let next = if now.month() == 12 { 1 } else { now.month() + 1 };
        assert!(screen(&mut app, 110, 30).contains(crate::collections::month_name(next)));
    }

    #[test]
    fn event_calendar_shows_ranges_and_adds_events() {
        let (_d, mut app, _ids) = fixture();
        let cal = collection(&mut app, "Events", "calendar");
        let now = chrono::Local::now().date_naive();
        let first = now.with_day(1).unwrap();
        let ev = app.db.create_note(&cal, "Conference", "text", "text/html", "").unwrap();
        app.db.add_attribute(&ev, "label", "startDate", &first.format("%Y-%m-%d").to_string(), false).unwrap();
        app.db.add_attribute(&ev, "label", "endDate", &(first + chrono::Duration::days(2)).format("%Y-%m-%d").to_string(), false).unwrap();
        app.reload();
        app.open_note(&cal, true, true);
        app.focus = Focus::Note;
        let items = app.calendar_items(&cal, now);
        for off in 0..=2 { assert_eq!(items[&(first + chrono::Duration::days(off))].len(), 1, "day {off}"); }
        assert!(!items.contains_key(&(first + chrono::Duration::days(3))));
        // move the cursor to the event's first day and open it
        if let Some(v) = app.view.as_mut() { v.cal = Some(first); }
        assert!(screen(&mut app, 110, 30).contains("Conference"));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, ev);
        // add a new event on a chosen day
        app.open_note(&cal, false, false);
        if let Some(v) = app.view.as_mut() { v.cal = Some(first + chrono::Duration::days(10)); }
        app.handle_key(key(KeyCode::Char('n')));
        typed(&mut app, "Dentist");
        app.handle_key(key(KeyCode::Enter));
        let d = app.model.notes.values().find(|n| n.title == "Dentist").unwrap().id.clone();
        assert_eq!(app.db.label_value(&d, "startDate").unwrap().as_deref(), Some((first + chrono::Duration::days(10)).format("%Y-%m-%d").to_string().as_str()));
    }

    #[test]
    fn markdown_notes_render_and_toggle_to_source() {
        let (_d, mut app, ids) = fixture();
        let md = app.db.create_note("root", "Notes.md", "code", "text/x-markdown",
            &format!("# Heading\n\nSome **bold** text and a wiki link to [[{}]].\n\n- [ ] todo\n- [x] done\n", ids[4])).unwrap();
        app.reload();
        app.open_note(&md, true, true);
        assert_eq!(app.view.as_ref().unwrap().mode, ViewMode::Rendered, "markdown opens rendered");
        app.focus = Focus::Note;
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("Some bold text") && s.contains("Epsilon needle") && s.contains("☐ todo") && s.contains("☑ done") && !s.contains("**bold**"), "{s}");
        assert!(s.contains("[code]") || s.contains("Notes.md"), "{s}");
        app.handle_key(key(KeyCode::Char(']')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.view.as_ref().unwrap().id, ids[4], "wiki links are followable");
        app.run_action(Action::Back);
        app.handle_key(key(KeyCode::Char('s')));
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("**bold**") && s.contains("source"), "{s}");
        // plain code notes are unaffected
        let py = app.db.create_note("root", "x.py", "code", "text/x-python", "print(1)").unwrap();
        app.reload();
        app.open_note(&py, true, true);
        assert_eq!(app.view.as_ref().unwrap().mode, ViewMode::Source);
    }

    #[test]
    fn built_in_editor_saves_losslessly_and_asks_before_discarding() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>one</p>\r\n<p>two</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(matches!(app.overlay, Overlay::Edit(_)));
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Char('!')));
        // Ctrl+S saves and stays open
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(app.overlay, Overlay::Edit(_)));
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>one</p>!\r\n<p>two</p>");
        // untouched buffer closes without prompting; no write
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        // dirty buffer: discard leaves the note alone
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('Z')));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::Edit(_)));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>one</p>!\r\n<p>two</p>");
        // paste goes into the editor, newlines intact
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        app.handle_paste("x\ny");
        let Overlay::Edit(s) = &app.overlay else { panic!() };
        assert!(s.buf.text().starts_with("x\r\ny<p>one"));
    }

    #[test]
    fn hostile_note_text_never_puts_control_characters_on_screen() {
        let (_d, mut app, ids) = fixture();
        let evil = "\x1b]0;pwned\x07\x1b[2J\u{9b}31m\u{85}";
        app.db.set_note_title(&ids[0], &format!("T{evil}itle")).unwrap();
        app.db.set_note_content(&ids[0], &format!("<p>body{evil}text &#27;[31m</p><a href=\"http://x/{evil}\">link{evil}</a>")).unwrap();
        app.db.add_attribute(&ids[0], "label", &format!("la{evil}bel"), &format!("va{evil}lue"), false).unwrap();
        app.reload(); app.reveal(&ids[0]);
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for mode in 0..3 {
            if mode == 1 { app.focus = Focus::Note; app.handle_key(key(KeyCode::Char('s'))); }
            if mode == 2 { app.handle_key(key(KeyCode::Char('i'))); }
            t.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let buf = t.backend().buffer().clone();
            for y in 0..30 { for x in 0..100 {
                let sym = buf[(x, y)].symbol().to_string();
                assert!(!sym.chars().any(|c| c.is_control()), "control char in cell ({x},{y}) mode {mode}: {sym:?}");
            } }
        }
    }

    #[test]
    fn saving_over_a_note_that_changed_underneath_needs_a_second_ctrl_s() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>mine</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('!')));
        app.db.set_note_content(&ids[0], "<p>from sync</p>").unwrap();
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(save);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>from sync</p>", "first Ctrl+S only warns");
        app.handle_key(save);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "!<p>mine</p>");
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(save);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "!?<p>mine</p>", "no warning when nothing changed underneath");
    }

    #[test]
    fn external_edit_of_a_note_that_changed_meanwhile_is_kept_aside_not_written() {
        let (d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>base</p>").unwrap();
        app.edit_base = Some("<p>base</p>".into());
        app.db.set_note_content(&ids[0], "<p>changed by sync</p>").unwrap();
        app.finish_edit(&ids[0], Ok(Some("<p>my edit</p>".into())));
        assert!(app.status.contains("NOT overwritten"), "{}", app.status);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>changed by sync</p>");
        let kept: Vec<_> = std::fs::read_dir(d.path().join("lost-edits")).unwrap().collect();
        assert_eq!(std::fs::read_to_string(kept[0].as_ref().unwrap().path()).unwrap(), "<p>my edit</p>");
    }

    #[test]
    fn quick_append_and_capture_to_today() {
        let (_d, mut app, ids) = fixture();
        app.open_note(&ids[0], true, true);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('a')));
        typed(&mut app, "buy <milk> & eggs");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>alpha <b>body</b></p><p>buy &lt;milk&gt; &amp; eggs</p>");
        assert!(app.status.contains("added to 'Alpha'"), "{}", app.status);
        // code notes get a new line
        let py = app.db.create_note("root", "t.py", "code", "text/x-python", "x = 1").unwrap();
        app.reload();
        app.open_note(&py, true, true);
        app.handle_key(key(KeyCode::Char('a')));
        typed(&mut app, "y = 2");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.db.note_content(&py).unwrap().unwrap(), "x = 1\ny = 2\n");
        // capture into today's note from the tree
        app.focus = Focus::Tree;
        app.handle_key(key(KeyCode::Char('c')));
        typed(&mut app, "call the dentist");
        app.handle_key(key(KeyCode::Enter));
        let today = app.db.today_note().unwrap();
        assert!(app.db.note_content(&today).unwrap().unwrap().contains("<p>call the dentist</p>"));
        // empty input is ignored; collections refuse
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status, "nothing to add");
    }

    #[test]
    fn e_edits_in_place_and_saves_only_what_changed() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<h2>Head</h2>\r\n<p>one <a href=\"http://x\">link</a></p><table><tr><td>cell</td></tr></table>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.rich.is_some() && matches!(app.overlay, Overlay::None), "e edits in place");
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("Head") && shown.contains("one link") && shown.contains("cell") && !shown.contains("<h2>") && !shown.contains("href"), "{shown}");
        // an untouched session writes nothing
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<h2>Head</h2>\r\n<p>one <a href=\"http://x\">link</a></p><table><tr><td>cell</td></tr></table>");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('X')));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<h2>Head</h2>\r\n<p>Xone <a href=\"http://x\">link</a></p><table><tr><td>cell</td></tr></table>", "only the touched text changed");
        assert!(app.rich.is_some(), "Ctrl+S keeps editing");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.rich.is_none(), "clean editor closes at once");
        assert!(screen(&mut app, 100, 30).contains("Xone link"), "the pane shows the saved note again");
    }

    #[test]
    fn e_starts_editing_the_highlighted_note_from_the_tree() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>tree</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Tree;
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.rich.is_some() && app.focus == Focus::Note, "e edits the highlighted note even with the tree focused");
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.rich.as_ref().unwrap().html(), "<p>!tree</p>");
    }

    #[test]
    fn f2_carries_unsaved_edits_between_in_place_and_source_editing() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>one</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Char('!')));
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.rich.is_none());
        let Overlay::Edit(s) = &app.overlay else { panic!("source editor expected") };
        assert_eq!(s.buf.text(), "<p>one!</p>");
        assert!(s.buf.changed(), "the carried edit still counts as unsaved");
        // break the markup in the source editor: it refuses to go back in place until it is fixed
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        for c in "<b>".chars() { app.handle_key(key(KeyCode::Char(c))); }
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.rich.is_none() && matches!(app.overlay, Overlay::Edit(_)), "markup that would not round-trip stays in the source editor");
        for _ in 0..3 { app.handle_key(key(KeyCode::Backspace)); }
        app.handle_key(key(KeyCode::F(2)));
        assert!(app.rich.is_some() && matches!(app.overlay, Overlay::None));
        assert_eq!(app.rich.as_ref().unwrap().html(), "<p>one!</p>");
        assert!(app.rich.as_ref().unwrap().dirty());
        // nothing was stored by switching
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>one</p>");
    }

    #[test]
    fn f2_keeps_the_cursor_where_it_was_in_both_directions() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<h2>Head</h2><p>one <strong>two</strong> three</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::Down));
        for _ in 0..6 { app.handle_key(key(KeyCode::Right)); } // "one tw|o"
        app.handle_key(key(KeyCode::F(2)));
        let Overlay::Edit(s) = &app.overlay else { panic!() };
        let line = &s.buf.lines[s.buf.cur.0];
        let before: String = line.chars().take(s.buf.cur.1).collect();
        assert!(before.ends_with("<strong>tw"), "source cursor sits at the same spot: {before:?}");
        // move in the source, come back: the in-place cursor follows
        for _ in 0..2 { app.handle_key(key(KeyCode::Right)); } // past "o" and into "</strong>"
        app.handle_key(key(KeyCode::F(2)));
        let r = app.rich.as_ref().expect("back in place");
        assert_eq!(r.html_offset(), "<h2>Head</h2><p>one <strong>two".len() , "an offset inside a tag lands at the end of the text");
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.rich.as_ref().unwrap().html(), "<h2>Head</h2><p>one <strong>two!</strong> three</p>");
    }

    #[test]
    fn notes_that_cannot_be_edited_in_place_fall_back_to_the_source_editor_with_a_reason() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>a</p><!-- keep me --><p>b</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.rich.is_none() && matches!(app.overlay, Overlay::Edit(_)));
        assert!(app.status.contains("source editor") && app.status.contains("comments"), "{}", app.status);
    }

    #[test]
    fn in_place_editor_asks_before_discarding_and_guards_against_changes_underneath() {
        let (_d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>mine</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::Char('!')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.rich.is_some(), "dirty: asks first");
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.rich.is_some(), "any other key keeps editing");
        app.db.set_note_content(&ids[0], "<p>from sync</p>").unwrap();
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(save);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>from sync</p>", "first Ctrl+S only warns");
        app.handle_key(save);
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>!mine</p>");
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.rich.is_none());
        assert_eq!(app.db.note_content(&ids[0]).unwrap().unwrap(), "<p>!mine</p>", "discard leaves the stored note alone");
    }

    #[test]
    fn unsaved_in_place_text_is_kept_when_the_app_is_killed() {
        let (d, mut app, ids) = fixture();
        app.db.set_note_content(&ids[0], "<p>mine</p>").unwrap();
        app.reload(); app.reveal(&ids[0]);
        app.focus = Focus::Note;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::Char('!')));
        app.save_unsaved_on_exit();
        let dir = d.path().join("lost-edits");
        let kept: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().collect()).unwrap_or_default();
        assert_eq!(kept.len(), 1, "one recovery file in {dir:?}");
        assert_eq!(std::fs::read_to_string(kept[0].path()).unwrap(), "!<p>mine</p>".replace("!<p>mine</p>", "<p>!mine</p>"));
    }

}
