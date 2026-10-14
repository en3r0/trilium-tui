//! Editing actions: prompts, confirmations, structure changes and external-editor round trips.
//! All persistent changes go through `trilium_core::edit`, which keeps sync bookkeeping correct.
use crate::app::{App, ClipMode, Clipboard, Confirm, Focus, Kind, Overlay, PickerKind, Prompt, PromptKind};
use crate::keymap::Action;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use trilium_core::model::ROOT;

impl App {
    /// (branchId, noteId, parentNoteId) of the tree selection.
    fn selected(&self) -> Option<(String, String, String)> {
        let r = self.rows.get(self.sel)?;
        let parent = self.model.children.iter().find(|(_, bs)| bs.iter().any(|b| b.id == r.branch_id)).map(|(p, _)| p.clone())?;
        Some((r.branch_id.clone(), r.note_id.clone(), parent))
    }

    pub fn poke(&self) { self.poke_sync(); }

    pub fn open_prompt_pub(&mut self, kind: PromptKind, label: &str, initial: &str) { self.open_prompt(kind, label, initial); }

    fn poke_sync(&self) {
        if self.sync_trigger.is_some() { self.sync_dirty.set(true); }
        if let Some(t) = &self.sync_trigger { let _ = t.send(()); }
    }

    fn fail(&mut self, what: &str, e: anyhow::Error) { self.status = format!("{what}: {e:#}"); }

    /// Reload after a write and put the cursor on `note_id`.
    fn changed(&mut self, note_id: Option<&str>) {
        self.reload();
        if let Some(id) = note_id {
            self.reveal(id);
            self.open_note(id, false, false);
        }
        self.poke_sync();
    }

    pub fn run_edit_action(&mut self, a: Action) {
        let sel = self.selected();
        match a {
            Action::EditTitle => {
                if let Some((_, note, _)) = sel {
                    let title = self.model.title_of(&note).to_string();
                    self.open_prompt(PromptKind::Rename(note), "Rename note", &title);
                }
            }
            Action::EditPrefix => {
                if let Some((branch, note, parent)) = sel {
                    let prefix = self.model.children.get(&parent).and_then(|c| c.iter().find(|b| b.id == branch)).and_then(|b| b.prefix.clone()).unwrap_or_default();
                    let _ = note;
                    self.open_prompt(PromptKind::Prefix(branch), "Branch prefix (empty to clear)", &prefix);
                }
            }
            Action::CreateNoteAfter => match sel {
                Some((branch, ..)) => self.open_prompt(PromptKind::NewAfter(branch), "New note after (title)", ""),
                None => self.open_prompt(PromptKind::NewInto(self.hoisted.clone()), "New note (title)", ""),
            },
            Action::CreateNoteInto => {
                let parent = sel.map(|s| s.1).unwrap_or_else(|| self.hoisted.clone());
                self.open_prompt(PromptKind::NewInto(parent), "New child note (title)", "");
            }
            Action::DeleteNotes => {
                let sel_items = self.selection();
                if sel_items.is_empty() { return; }
                let message = if sel_items.len() > 1 {
                    format!("Delete {} selected notes (and their descendants)?", sel_items.len())
                } else {
                    let (_, note) = &sel_items[0];
                    let parent = sel.as_ref().map(|s| s.2.clone()).unwrap_or_default();
                    let clones = self.model.parents.get(note).map_or(0, Vec::len);
                    if clones > 1 {
                        format!("Remove '{}' from '{}'? (it stays in {} other place(s))", self.model.title_of(note), self.model.title_of(&parent), clones - 1)
                    } else {
                        let n = self.count_descendants(note);
                        if n == 0 { format!("Delete '{}'?", self.model.title_of(note)) } else { format!("Delete '{}' and its {} descendant note(s)?", self.model.title_of(note), n) }
                    }
                };
                self.overlay = Overlay::Confirm(Confirm { message, branches: sel_items.into_iter().map(|(b, _)| b).collect() });
            }
            Action::MoveUp | Action::MoveDown | Action::MoveOut | Action::MoveIn => {
                let Some((branch, note, _)) = sel else { return };
                let r = match a {
                    Action::MoveUp => self.db.move_branch_up(&branch),
                    Action::MoveDown => self.db.move_branch_down(&branch),
                    Action::MoveOut => self.db.move_branch_out(&branch),
                    _ => self.db.move_branch_in(&branch),
                };
                match r {
                    Ok(true) => self.changed(Some(&note)),
                    Ok(false) => self.status = "can't move further".into(),
                    Err(e) => self.fail("move", e),
                }
            }
            Action::AddLabel | Action::AddRelation => {
                let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return };
                let relation = a == Action::AddRelation;
                let label = if relation { "New relation: name=target note" } else { "New label: name or name=value" };
                self.open_prompt(PromptKind::Attribute { note_id: id, relation }, label, "");
            }
            _ => {}
        }
    }

    /// This week's (or quarter's) journal note.
    pub fn open_period(&mut self, week: bool) {
        use chrono::Datelike;
        if self.sync_trigger.is_some() && !self.synced_once {
            self.status = "wait for the first sync to finish before creating journal notes".into();
            return;
        }
        let now = chrono::Local::now();
        let r = if week {
            self.db.week_of(&now.format("%Y-%m-%d").to_string()).and_then(|w| self.db.week_note(&w))
        } else { self.db.quarter_note(&format!("{}-Q{}", now.year(), now.month0() / 3 + 1)) };
        match r {
            Ok(id) => { self.changed(Some(&id)); self.focus = Focus::Note; }
            Err(e) => self.fail(if week { "this week" } else { "this quarter" }, e),
        }
    }

    /// Today's journal note (created with its year/month parents and template if missing).
    pub fn open_today(&mut self) {
        // before the first sync finishes the replica may be missing a day note another client already made;
        // creating one now would leave two. Opening an existing note is always fine.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if self.sync_trigger.is_some() && !self.synced_once && self.db.existing_day_note(&today).ok().flatten().is_none() {
            self.want_today = true;
            self.status = "waiting for the first sync before creating today's note…".into();
            return;
        }
        self.want_today = false;
        match self.db.today_note() {
            Ok(id) => { self.changed(Some(&id)); self.focus = Focus::Note; }
            Err(e) => self.fail("today", e),
        }
    }

    fn expand_path(p: &str) -> std::path::PathBuf {
        let p = p.trim();
        match p.strip_prefix("~/") { Some(rest) => dirs::home_dir().unwrap_or_default().join(rest), None if p == "~" => dirs::home_dir().unwrap_or_default(), None => std::path::PathBuf::from(p) }
    }

    pub fn run_transfer_action(&mut self, a: Action) {
        let Some(note) = self.rows.get(self.sel).map(|r| r.note_id.clone()).or_else(|| self.view.as_ref().map(|v| v.id.clone())) else { return };
        if matches!(a, Action::ExportNote | Action::ExportOpml | Action::ExportSite | Action::ExportMarkdown | Action::ExportEnex) {
            let dir = std::env::var_os("TRILIUM_TUI_DOWNLOAD_DIR").map(std::path::PathBuf::from).or_else(dirs::download_dir).or_else(dirs::home_dir).unwrap_or_default();
            let name = trilium_core::transfer::sanitize_filename(self.model.title_of(&note));
            let name = if name.is_empty() { "export".to_string() } else { name };
            let (ext, kind, label): (&str, PromptKind, &str) = match a {
                Action::ExportOpml => ("opml", PromptKind::ExportOpmlPath(note), "Export subtree to (OPML file)"),
                Action::ExportSite => ("site.zip", PromptKind::ExportSitePath(note), "Export subtree as a static website to (ZIP file)"),
                Action::ExportMarkdown => ("md.zip", PromptKind::ExportMarkdownPath(note), "Export subtree as Markdown files to (ZIP file)"),
                Action::ExportEnex => ("enex", PromptKind::ExportEnexPath(note), "Export subtree as an Evernote notebook to (.enex file)"),
                _ => ("zip", PromptKind::ExportPath(note), "Export subtree to (ZIP file)"),
            };
            self.open_prompt(kind, label, &dir.join(format!("{name}.{ext}")).to_string_lossy());
        } else if a == Action::ImportObsidian {
            self.open_prompt(PromptKind::ImportFormatted(note, "obsidian"), "Import an Obsidian vault into the selected note: path to the .zip or folder", "");
        } else if a == Action::ImportAnytype {
            self.open_prompt(PromptKind::ImportFormatted(note, "anytype"), "Import an Anytype JSON export into the selected note: path to the .zip", "");
        } else if a == Action::ImportKeep {
            self.open_prompt(PromptKind::ImportFormatted(note, "keep"), "Import a Google Keep Takeout into the selected note: path to the .zip", "");
        } else if a == Action::ImportNotion {
            self.open_prompt(PromptKind::ImportFormatted(note, "notion"), "Import a Notion HTML export into the selected note: path to the .zip", "");
        } else {
            self.open_prompt(PromptKind::ImportPath(note), "Import into the selected note: path to a .zip, .opml, .enex, folder or file; Obsidian/Notion/Anytype/Keep exports are detected", "");
        }
    }

    pub fn run_capture_action(&mut self, a: Action) {
        if a == Action::CaptureToday {
            self.open_prompt(PromptKind::Append { note: String::new(), today: true }, "Capture into today's note", "");
            return;
        }
        let Some(v) = self.view.as_ref() else { return };
        match v.kind {
            Kind::Html | Kind::Code => self.open_prompt(PromptKind::Append { note: v.id.clone(), today: false }, &format!("Append to '{}'", self.model.title_of(&v.id)), ""),
            _ => self.status = "only text and code notes can be appended to".into(),
        }
    }

    /// Append one line: a paragraph to HTML notes, a new line to code / markdown notes.
    fn append_line(&mut self, note: &str, text: &str) -> anyhow::Result<()> {
        let n = self.model.notes.get(note).cloned().ok_or_else(|| anyhow::anyhow!("note not found"))?;
        let cur = self.db.note_content(note)?.unwrap_or_default();
        let new = if n.ntype == "text" {
            let esc = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            format!("{cur}<p>{esc}</p>")
        } else if cur.is_empty() || cur.ends_with('\n') { format!("{cur}{text}\n") } else { format!("{cur}\n{text}\n") };
        self.db.set_note_content(note, &new)
    }

    pub fn save_revision_now(&mut self) {
        let Some(id) = self.view.as_ref().map(|v| v.id.clone()) else { return };
        match self.db.save_revision(&id, "", "manual") {
            Ok(_) => { self.status = "revision saved".into(); self.poke_sync(); }
            Err(e) => self.fail("revision", e),
        }
    }

    fn count_descendants(&self, note: &str) -> usize {
        let mut n = 0;
        let mut stack = vec![note.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x.clone()) { continue; }
            for b in self.model.children.get(&x).into_iter().flatten() { n += 1; stack.push(b.note_id.clone()); }
        }
        n
    }

    fn open_prompt(&mut self, kind: PromptKind, label: &str, initial: &str) {
        let text: Vec<char> = initial.chars().collect();
        let cursor = text.len();
        self.overlay = Overlay::Prompt(Prompt { kind, label: label.into(), text, cursor, secret: false });
    }

    fn open_secret_prompt(&mut self, kind: PromptKind, label: &str) {
        self.overlay = Overlay::Prompt(Prompt { kind, label: label.into(), text: vec![], cursor: 0, secret: true });
    }

    pub fn prompt_key(&mut self, ev: KeyEvent) {
        let Overlay::Prompt(p) = &mut self.overlay else { return };
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Left => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Right => p.cursor = (p.cursor + 1).min(p.text.len()),
            KeyCode::Home => p.cursor = 0,
            KeyCode::End => p.cursor = p.text.len(),
            KeyCode::Char('a') if ctrl => p.cursor = 0,
            KeyCode::Char('e') if ctrl => p.cursor = p.text.len(),
            KeyCode::Char('u') if ctrl => { p.text.drain(..p.cursor); p.cursor = 0; }
            KeyCode::Char('w') if ctrl => {
                let mut i = p.cursor;
                while i > 0 && p.text[i - 1] == ' ' { i -= 1; }
                while i > 0 && p.text[i - 1] != ' ' { i -= 1; }
                p.text.drain(i..p.cursor);
                p.cursor = i;
            }
            KeyCode::Backspace => if p.cursor > 0 { p.cursor -= 1; p.text.remove(p.cursor); },
            KeyCode::Delete => if p.cursor < p.text.len() { p.text.remove(p.cursor); },
            KeyCode::Char(c) if !ctrl && !ev.modifiers.contains(KeyModifiers::ALT) => { p.text.insert(p.cursor, c); p.cursor += 1; }
            _ => {}
        }
    }

    fn submit_prompt(&mut self) {
        let Overlay::Prompt(p) = std::mem::replace(&mut self.overlay, Overlay::None) else { return };
        let text: String = p.text.iter().collect();
        match p.kind {
            PromptKind::InsertImage => self.insert_image_from(&text),
            PromptKind::Unlock => self.finish_unlock(&text),
            PromptKind::DeleteBackups => {
                self.status = if text.trim().eq_ignore_ascii_case("yes") { format!("deleted {} backup(s)", self.db.delete_backups()) } else { "nothing deleted".into() };
            }
            PromptKind::NewPassword => {
                if text.is_empty() { self.status = "password can't be empty".into(); return; }
                self.open_secret_prompt(PromptKind::ConfirmPassword(text), "Repeat the new protected-notes password");
            }
            PromptKind::ConfirmPassword(first) => {
                if first != text { self.status = "the passwords differ; nothing was set".into(); return; }
                match self.db.set_password(&text) {
                    Ok(()) => { self.reload(); self.poke_sync(); self.status = "protected-notes password set; the session is unlocked".into(); }
                    Err(e) => self.fail("set password", e),
                }
            }
            PromptKind::Rename(note) => {
                let t = text.trim();
                if t.is_empty() { self.status = "title can't be empty".into(); return; }
                match self.db.set_note_title(&note, t) {
                    Ok(()) => { self.changed(Some(&note)); self.focus = Focus::Note; }
                    Err(e) => self.fail("rename", e),
                }
            }
            PromptKind::NewAfter(branch) => {
                let title = if text.trim().is_empty() { "New note" } else { text.trim() };
                match self.db.create_note_after(&branch, title, "text", "text/html", "") {
                    Ok(id) => { self.changed(Some(&id)); self.focus = Focus::Note; }
                    Err(e) => self.fail("create", e),
                }
            }
            PromptKind::NewInto(parent) => {
                let title = if text.trim().is_empty() { "New note" } else { text.trim() };
                match self.db.create_note(&parent, title, "text", "text/html", "") {
                    Ok(id) => {
                        if parent != ROOT && parent != self.hoisted {
                            if let Some(b) = self.model.children.get(&self.model.parents.get(&parent).and_then(|p| p.first()).cloned().unwrap_or_default()).and_then(|c| c.iter().find(|b| b.note_id == parent)) {
                                self.expanded.insert(b.id.clone());
                            }
                        }
                        self.changed(Some(&id));
                        self.focus = Focus::Note;
                    }
                    Err(e) => self.fail("create", e),
                }
            }
            PromptKind::Prefix(branch) => match self.db.set_branch_prefix(&branch, &text) {
                Ok(()) => self.changed(None),
                Err(e) => self.fail("prefix", e),
            },
            PromptKind::Append { note, today } => {
                if text.trim().is_empty() { self.status = "nothing to add".into(); return; }
                let target = if today { self.db.today_note().map_err(|e| e.to_string()) } else { Ok(note) };
                match target {
                    Ok(id) => {
                        self.reload();
                        match self.append_line(&id, text.trim()) {
                            Ok(()) => { let cur = self.view.as_ref().map(|v| v.id.clone()); self.changed(cur.as_deref()); self.status = format!("added to '{}'", self.model.title_of(&id)); }
                            Err(e) => self.fail("append", e),
                        }
                    }
                    Err(e) => self.status = format!("today's note: {e}"),
                }
            }
            k @ (PromptKind::NewCard { .. } | PromptKind::NewEvent { .. }) => self.create_collection_item(&k, &text),
            PromptKind::ExportPath(note) => {
                let path = Self::expand_path(&text);
                let r = self.db.export_zip_file(&note, &path);
                self.status = match r { Ok(st) => format!("exported {} notes, {} attachments to {}", st.notes, st.attachments, path.display()), Err(e) => format!("export failed: {e:#}") };
            }
            PromptKind::ExportOpmlPath(note) => {
                let path = Self::expand_path(&text);
                let r = self.db.export_opml_file(&note, &path).map_err(|e| format!("{e:#}"));
                self.status = match r { Ok(n) => format!("exported {n} notes to {}", path.display()), Err(e) => format!("export failed: {e}") };
            }
            PromptKind::ExportMarkdownPath(note) => {
                let path = Self::expand_path(&text);
                self.status = match self.db.export_markdown_file(&note, &path) {
                    Ok(st) => format!("exported {} notes, {} attachments as Markdown to {}", st.notes, st.attachments, path.display()),
                    Err(e) => format!("export failed: {e:#}"),
                };
            }
            PromptKind::ExportEnexPath(note) => {
                let path = Self::expand_path(&text);
                self.status = match self.db.export_enex_file(&note, &path) {
                    Ok(n) => format!("exported {n} notes to {}", path.display()),
                    Err(e) => format!("export failed: {e:#}"),
                };
            }
            PromptKind::ExportSitePath(note) => {
                let path = Self::expand_path(&text);
                self.status = match self.db.export_site_file(&note, &path) {
                    Ok(st) => format!("exported {} pages, {} attachments as a website to {}", st.notes, st.attachments, path.display()),
                    Err(e) => format!("export failed: {e:#}"),
                };
            }
            k @ (PromptKind::ImportPath(_) | PromptKind::ImportFormatted(..)) => {
                let (parent, format) = match k { PromptKind::ImportPath(p) => (p, None), PromptKind::ImportFormatted(p, f) => (p, Some(f.to_string())), _ => unreachable!() };
                let path = Self::expand_path(&text);
                match self.db.import_path(&path, &parent, &trilium_core::transfer::ImportOpts { format, ..Default::default() }) {
                    Ok(st) => {
                        if let Some(b) = self.rows.iter().find(|r| r.note_id == parent).map(|r| r.branch_id.clone()) { self.expanded.insert(b); }
                        self.changed(st.root.as_deref());
                        self.status = format!("imported {} notes, {} attachments", st.notes, st.attachments);
                    }
                    Err(e) => self.status = format!("import failed: {e:#}"),
                }
            }
            PromptKind::AttrValue(attr_id) => match self.db.set_attribute_value(&attr_id, &text) {
                Ok(()) => { let id = self.view.as_ref().map(|v| v.id.clone()); self.changed(id.as_deref()); }
                Err(e) => self.fail("edit attribute", e),
            },
            PromptKind::Attribute { note_id, relation } => {
                let t = text.trim().trim_start_matches(['#', '~']);
                let (name, value) = t.split_once('=').map(|(n, v)| (n.trim(), v.trim())).unwrap_or((t, ""));
                if name.is_empty() { self.status = "attribute name can't be empty".into(); return; }
                let value = if relation {
                    match self.resolve_note(value) {
                        Ok(id) => id,
                        Err(m) => { self.status = m; return; }
                    }
                } else { value.to_string() };
                match self.db.add_attribute(&note_id, if relation { "relation" } else { "label" }, name, &value, false) {
                    Ok(_) => { self.changed(Some(&note_id)); }
                    Err(e) => self.fail("attribute", e),
                }
            }
        }
    }

    /// Note id, or an exact (case-insensitive) unique title.
    fn resolve_note(&self, s: &str) -> Result<String, String> {
        if self.model.notes.contains_key(s) { return Ok(s.to_string()); }
        let hits: Vec<&String> = self.model.notes.values().filter(|n| n.title.eq_ignore_ascii_case(s)).map(|n| &n.id).collect();
        match hits.len() {
            1 => Ok(hits[0].clone()),
            0 => Err(format!("no note with id or title '{s}'")),
            n => Err(format!("{n} notes are titled '{s}'; use the note id")),
        }
    }

    pub fn confirm_key(&mut self, ev: KeyEvent) {
        let Overlay::Confirm(c) = &self.overlay else { return };
        match ev.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let branches = c.branches.clone();
                self.overlay = Overlay::None;
                let old_sel = self.sel;
                let (mut ok, mut failed) = (0, None);
                for b in &branches {
                    match self.db.delete_branch(b) { Ok(()) => ok += 1, Err(e) => { failed.get_or_insert(format!("{e:#}")); } }
                }
                self.marked.clear();
                if ok > 0 {
                    self.reload();
                    self.sel = old_sel.min(self.rows.len().saturating_sub(1));
                    if let Some(r) = self.rows.get(self.sel) { let id = r.note_id.clone(); self.open_note(&id, false, false); } else { self.view = None; }
                    self.poke_sync();
                }
                self.status = match failed { Some(f) => format!("deleted {ok}, failed: {f}"), None => if ok == 1 { "deleted".into() } else { format!("deleted {ok} notes") } };
            }
            _ => self.overlay = Overlay::None,
        }
    }

    // ------------------------------------------------------------ multi-select, clipboard, clone / move

    /// The notes an operation applies to: the marked rows, or the row under the cursor.
    pub fn selection(&self) -> Vec<(String, String)> {
        if !self.marked.is_empty() { return self.marked.clone(); }
        self.rows.get(self.sel).map(|r| vec![(r.branch_id.clone(), r.note_id.clone())]).unwrap_or_default()
    }

    fn mark(&mut self, branch: &str, note: &str) {
        if !self.is_marked(branch) { self.marked.push((branch.to_string(), note.to_string())); }
    }

    pub fn run_selection_action(&mut self, a: Action) {
        let Some(cur) = self.rows.get(self.sel).cloned() else { return };
        match a {
            Action::ToggleMark => {
                if self.is_marked(&cur.branch_id) { self.marked.retain(|(b, _)| *b != cur.branch_id); } else { self.mark(&cur.branch_id, &cur.note_id); }
                self.status = format!("{} selected", self.marked.len());
            }
            Action::SelectAbove | Action::SelectBelow => {
                self.mark(&cur.branch_id, &cur.note_id);
                let before = self.sel;
                self.move_sel(if a == Action::SelectAbove { -1 } else { 1 });
                if self.sel != before { if let Some(r) = self.rows.get(self.sel).cloned() { self.mark(&r.branch_id, &r.note_id); } }
                self.status = format!("{} selected", self.marked.len());
            }
            Action::SelectAllSiblings => {
                let parent = self.model.branch_parent.get(&cur.branch_id).cloned();
                let rows: Vec<(String, String)> = self.rows.iter().filter(|r| self.model.branch_parent.get(&r.branch_id) == parent.as_ref()).map(|r| (r.branch_id.clone(), r.note_id.clone())).collect();
                for (b, n) in rows { self.mark(&b, &n); }
                self.status = format!("{} selected", self.marked.len());
            }
            Action::CopyNotes | Action::CutNotes => {
                let items = self.selection();
                let mode = if a == Action::CopyNotes { ClipMode::Copy } else { ClipMode::Cut };
                self.status = format!("{} note(s) {} — select a target note and paste (Ctrl+V)", items.len(), if mode == ClipMode::Copy { "copied" } else { "cut" });
                self.clipboard = Some(Clipboard { mode, items });
                self.marked.clear();
            }
            Action::PasteNotes => {
                let Some(clip) = self.clipboard.take() else { self.status = "clipboard is empty".into(); return };
                let target = cur.note_id.clone();
                let (mut ok, mut failed) = (0, None);
                for (branch, note) in &clip.items {
                    let r = match clip.mode { ClipMode::Copy => self.db.clone_note(note, &target, None).map(|_| ()), ClipMode::Cut => self.db.move_branch_to(branch, &target).map(|_| ()) };
                    match r { Ok(()) => ok += 1, Err(e) => { failed.get_or_insert(format!("{e:#}")); } }
                }
                if clip.mode == ClipMode::Copy { self.clipboard = Some(clip); } // copies can be pasted repeatedly
                if ok > 0 {
                    if cur.expandable || true { self.expanded.insert(cur.branch_id.clone()); }
                    self.reload();
                    self.poke_sync();
                }
                self.status = match failed { Some(f) => format!("pasted {ok}, failed: {f}"), None => format!("pasted {ok} note(s) into '{}'", self.model.title_of(&target)) };
            }
            Action::CloneTo | Action::MoveTo => {
                let items = self.selection();
                self.open_picker(if a == Action::CloneTo { PickerKind::CloneTo } else { PickerKind::MoveTo });
                if let Overlay::Picker(p) = &mut self.overlay { p.targets = items; }
            }
            Action::DuplicateNote => {
                let items = self.selection();
                let mut last = None;
                let (mut ok, mut failed) = (0, None);
                for (branch, note) in &items {
                    let parent = self.model.branch_parent.get(branch).cloned().unwrap_or_else(|| ROOT.into());
                    match self.db.duplicate_subtree(note, &parent, true) { Ok(id) => { ok += 1; last = Some(id); } Err(e) => { failed.get_or_insert(format!("{e:#}")); } }
                }
                self.marked.clear();
                if let Some(id) = last { self.changed(Some(&id)); }
                self.status = match failed { Some(f) => format!("duplicated {ok}, failed: {f}"), None => format!("duplicated {ok} note(s)") };
            }
            _ => {}
        }
    }

    /// Finish "clone to…" / "move to…": apply to every target under the chosen note.
    pub fn apply_to_target(&mut self, kind: PickerKind, targets: &[(String, String)], target_note: &str) {
        let (mut ok, mut failed) = (0, None);
        for (branch, note) in targets {
            let r = if kind == PickerKind::CloneTo { self.db.clone_note(note, target_note, None).map(|_| ()) } else { self.db.move_branch_to(branch, target_note).map(|_| ()) };
            match r { Ok(()) => ok += 1, Err(e) => { failed.get_or_insert(format!("{e:#}")); } }
        }
        self.marked.clear();
        if ok > 0 { self.reload(); self.reveal(target_note); self.poke_sync(); }
        let verb = if kind == PickerKind::CloneTo { "cloned" } else { "moved" };
        self.status = match failed { Some(f) => format!("{verb} {ok}, failed: {f}"), None => format!("{verb} {ok} note(s) to '{}'", self.model.title_of(target_note)) };
    }

    // ------------------------------------------------------------ external editor

    fn editable_id(&mut self) -> Option<String> {
        let v = self.view.as_ref()?;
        if self.model.notes.get(&v.id).is_some_and(|n| n.ntype == "file") { self.status = "file notes are read-only here (the bytes must stay exact)".into(); return None; }
        match v.kind {
            Kind::Html | Kind::Code | Kind::Unsupported(_) => Some(v.id.clone()),
            Kind::Protected => { self.status = "this note is locked; unlock protected notes first (command palette)".into(); None }
            Kind::Collection | Kind::Binary | Kind::Image | Kind::SearchResults(_) => { self.status = "this note has no text content to edit".into(); None }
        }
    }

    /// Edit the note in place (text notes); anything else, or markup that would not round-trip, opens the source editor instead.
    pub fn request_edit(&mut self) {
        let Some(id) = self.editable_id() else { return };
        if let Err(e) = self.db.ensure_writable() { self.status = format!("{e:#}"); return; }
        let Some(n) = self.model.notes.get(&id) else { return };
        if n.ntype != "text" { self.request_edit_source(); return; }
        let title = n.title.clone();
        let content = match self.db.note_content(&id) { Ok(c) => c.unwrap_or_default(), Err(e) => { self.fail("open for editing", e); return; } };
        if let Some(why) = trilium_core::richtext::unsupported_reason(&content) {
            self.request_edit_source();
            if matches!(self.overlay, Overlay::Edit(_)) { self.status = format!("opened the HTML source editor: {why}"); }
            return;
        }
        match crate::rich_view::RichSession::new(&id, &title, &content) {
            Some(r) => { self.rich = Some(Box::new(r)); self.focus = Focus::Note; }
            None => {
                self.request_edit_source();
                if matches!(self.overlay, Overlay::Edit(_)) { self.status = "opened the HTML source editor: this note has no plain text to edit in place".into(); }
            }
        }
    }

    /// Open the built-in source editor on the note's stored text.
    pub fn request_edit_source(&mut self) {
        let Some(id) = self.editable_id() else { return };
        if let Err(e) = self.db.ensure_writable() { self.status = format!("{e:#}"); return; }
        let Some(n) = self.model.notes.get(&id) else { return };
        let mime = if n.ntype == "text" { "text/html".to_string() } else { n.mime.clone() };
        let title = n.title.clone();
        // a failure (a locked or damaged protected note) must not open as an empty note that could then overwrite the real text
        let content = match self.db.note_content(&id) { Ok(c) => c.unwrap_or_default(), Err(e) => { self.fail("open for editing", e); return; } };
        self.overlay = Overlay::Edit(Box::new(crate::edit_view::EditSession::new(&id, &title, &mime, &content)));
    }

    /// Hand the note to $VISUAL / $EDITOR instead.
    pub fn request_edit_external(&mut self) {
        let Some(id) = self.editable_id() else { return };
        // the external editor works on plaintext temp files; a protected note must not leave its text on disk
        if self.model.notes.get(&id).is_some_and(|n| n.protected) {
            self.status = "protected notes are edited only in the built-in editor (no plaintext temp files)".into();
            return;
        }
        // fail now, not after minutes of typing
        if let Err(e) = self.db.ensure_writable() { self.status = format!("{e:#}"); return; }
        self.edit_base = self.db.note_content(&id).ok().flatten();
        self.pending_edit = Some(id);
    }

    /// Keep text the user typed that could not be saved, so a failed save never costs them their work.
    fn save_recovery(&self, id: &str, text: &str) -> Option<std::path::PathBuf> {
        // a protected note's text goes to disk encrypted only
        let encrypted;
        let (text, ext) = if self.model.notes.get(id).is_some_and(|n| n.protected) {
            encrypted = self.db.encrypt_text(text).ok()?;
            (encrypted.as_str(), "enc")
        } else { (text, "txt") };
        let dir = self.db.conn.path().map(std::path::PathBuf::from)?.parent()?.join("lost-edits");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{id}-{}.{ext}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        let mut oo = std::fs::OpenOptions::new();
        oo.create_new(true).write(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; oo.mode(0o600); }
        use std::io::Write;
        oo.open(&path).ok()?.write_all(text.as_bytes()).ok()?;
        Some(path)
    }

    /// Terminated by a signal (SIGTERM/SIGHUP): keep whatever the built-in editor was holding.
    pub fn save_unsaved_on_exit(&mut self) {
        if let Overlay::Edit(s) = &self.overlay {
            if s.buf.changed() { let (id, text) = (s.note_id.clone(), s.buf.text()); self.save_recovery(&id, &text); }
        }
        if let Some(r) = &self.rich {
            if r.dirty() { let (id, text) = (r.note_id.clone(), r.html()); self.save_recovery(&id, &text); }
        }
    }

    pub fn edit_key(&mut self, ev: KeyEvent) {
        use crate::edit_view::Outcome;
        let Overlay::Edit(s) = &mut self.overlay else { return };
        let out = s.key(ev);
        let id = s.note_id.clone();
        let text = if matches!(out, Outcome::Save | Outcome::SaveAndClose) { s.buf.text() } else { String::new() };
        if out == Outcome::ToRich {
            let Overlay::Edit(s) = &mut self.overlay else { return };
            let text = s.buf.text();
            if let Some(why) = trilium_core::richtext::unsupported_reason(&text) { s.msg = format!("can't edit this in place: {why}"); return; }
            let (title, base) = (s.title.clone(), s.base.clone());
            match crate::rich_view::RichSession::new(&id, &title, &text) {
                Some(mut r) => {
                    r.base = base.clone();
                    if text != base { r.mark_dirty(); }
                    let (row, col) = s.buf.cur;
                    let off: usize = s.buf.lines.iter().take(row).map(|l| l.len() + 1).sum::<usize>()
                        + s.buf.lines.get(row).map_or(0, |l| l.char_indices().nth(col).map_or(l.len(), |(i, _)| i));
                    r.place_at_html_offset(off);
                    self.overlay = Overlay::None;
                    self.rich = Some(Box::new(r));
                }
                None => s.msg = "no plain text in this note to edit in place".into(),
            }
            return;
        }
        match out {
            Outcome::None | Outcome::ToRich => {}
            Outcome::Close => { self.overlay = Overlay::None; self.finish_new_attachments(); }
            Outcome::Save | Outcome::SaveAndClose => {
                let stored = self.db.note_content(&id).ok().flatten().unwrap_or_default();
                if let Overlay::Edit(s) = &mut self.overlay { if s.needs_overwrite_confirmation(&stored) { return; } }
                let unchanged = self.db.note_content(&id).ok().flatten().is_some_and(|c| c == text);
                let res = if unchanged { Ok(()) } else { self.db.set_note_content(&id, &text) };
                match res {
                    Ok(()) => {
                        if !unchanged { self.changed(Some(&id)); }
                        if out == Outcome::SaveAndClose { self.overlay = Overlay::None; self.status = "saved".into(); self.finish_new_attachments(); }
                        else {
                            let stored = self.db.note_content(&id).ok().flatten().unwrap_or(text);
                            if let Overlay::Edit(s) = &mut self.overlay { s.saved(stored); }
                        }
                    }
                    Err(e) => {
                        // the editor covers the status bar: say it where the user is looking
                        if let Overlay::Edit(s) = &mut self.overlay { s.msg = format!("save failed: {e:#}"); }
                        self.fail("save", e)
                    }
                }
            }
        }
    }

    /// Delete pictures that were stored for an edit but ended up in no saved note text (discarded edit, removed image).
    pub fn finish_new_attachments(&mut self) {
        for (note, att) in std::mem::take(&mut self.new_attachments) {
            let stored = self.db.note_content(&note).ok().flatten().unwrap_or_default();
            if !stored.contains(&att) { let _ = self.db.delete_attachment(&att); }
        }
    }

    /// `Alt+P` answer: a file path (also as dropped onto the terminal) or, when empty, the clipboard's image.
    fn insert_image_from(&mut self, input: &str) {
        let Some(note) = self.rich.as_ref().map(|r| r.note_id.clone()) else { return };
        let (bytes, name) = match image_bytes_from(input) {
            Ok(v) => v,
            Err(e) => { self.status = e; return; }
        };
        let Some(mime) = image_mime(&bytes) else { self.status = "that isn't a PNG, JPEG, GIF or WebP picture".into(); return };
        if image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok()).is_none() {
            self.status = "can't read that picture".into();
            return;
        }
        let id = match self.db.add_attachment(&note, &name, mime, "image", &bytes) {
            Ok(id) => id,
            Err(e) => { self.fail("store picture", e); return; }
        };
        self.new_attachments.push((note, id.clone()));
        if let Some(r) = self.rich.as_mut() { r.insert_image(&format!("api/attachments/{id}/image/{}", percent_encode(&name))); }
        self.status = format!("inserted {name} ({} KB)", bytes.len().div_ceil(1000));
    }

    /// Keys while the note pane is being edited in place.
    pub fn rich_key(&mut self, ev: KeyEvent) {
        use crate::rich_view::Outcome;
        let Some(mut r) = self.rich.take() else { return };
        let out = r.key(ev);
        let id = r.note_id.clone();
        match out {
            Outcome::None => { self.rich = Some(r); }
            Outcome::ToggleImages => { self.rich = Some(r); self.toggle_inline_images(); }
            Outcome::PickNote => { self.rich = Some(r); self.open_picker(PickerKind::LinkNote); }
            Outcome::PickImage => {
                self.rich = Some(r);
                self.open_prompt(PromptKind::InsertImage, "Image file (drop or paste a path; empty = image on the clipboard)", "");
            }
            Outcome::Follow(target) => {
                use trilium_core::richtext::LinkTarget;
                self.rich = Some(r);
                match target {
                    LinkTarget::Image(src) => match self.db.image_source_bytes(&src) {
                        Ok(Some((name, mime, bytes))) => self.show_picture(&name, &name, &mime, bytes),
                        Ok(None) => self.status = format!("picture '{src}' isn't in the replica"),
                        Err(e) => self.status = format!("{e:#}"),
                    },
                    LinkTarget::Href(href) => {
                        let href = href.trim().to_string();
                        if let Some(id) = crate::app::note_id_from_href(&href) {
                            let dirty = self.rich.as_ref().is_some_and(|r| r.dirty());
                            if !self.model.notes.contains_key(&id) { self.status = format!("linked note {id} not found (not synced or deleted)"); }
                            else if dirty { self.status = "save first (Ctrl+S) or close the editor: following a note link leaves this note".into(); }
                            else {
                                self.rich = None;
                                self.finish_new_attachments();
                                self.open_note(&id, true, true);
                                self.focus = Focus::Note;
                            }
                        } else if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("mailto:") {
                            let ok = std::process::Command::new("xdg-open").arg(&href).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok();
                            self.status = if ok { format!("opened {href}") } else { format!("xdg-open failed for {href}") };
                        } else { self.status = format!("can't follow '{href}'"); }
                    }
                }
            }
            Outcome::Close => { self.status.clear(); self.finish_new_attachments(); }
            Outcome::ToSource => {
                // carry unsaved edits over to the source editor
                let text = r.html();
                let dirty = r.dirty() || text != r.base;
                let n = self.model.notes.get(&id);
                let (title, mime) = (r.title.clone(), n.map(|n| if n.ntype == "text" { "text/html".to_string() } else { n.mime.clone() }).unwrap_or_else(|| "text/html".into()));
                let mut s = crate::edit_view::EditSession::new(&id, &title, &mime, &text);
                s.base = r.base.clone();
                if dirty { s.buf.set_stored(&r.base); }
                // keep the cursor where it was: its line and column in the HTML
                let before = &text[..r.html_offset().min(text.len())];
                let row = before.matches('\n').count();
                s.buf.place_cursor((row, before.rsplit('\n').next().map_or(0, |l| l.chars().count())));
                self.overlay = Overlay::Edit(Box::new(s));
            }
            Outcome::Save | Outcome::SaveAndClose => {
                let text = r.html();
                let stored = self.db.note_content(&id).ok().flatten().unwrap_or_default();
                if r.needs_overwrite_confirmation(&stored) { self.rich = Some(r); return; }
                let unchanged = stored == text;
                let res = if unchanged { Ok(()) } else { self.db.set_note_content(&id, &text) };
                match res {
                    Ok(()) => {
                        if !unchanged { self.changed(Some(&id)); }
                        if out == Outcome::SaveAndClose { self.status = "saved".into(); self.finish_new_attachments(); }
                        else {
                            let stored = self.db.note_content(&id).ok().flatten().unwrap_or(text);
                            r.saved(stored);
                            self.rich = Some(r);
                        }
                    }
                    Err(e) => {
                        r.msg = format!("save failed: {e:#}");
                        self.rich = Some(r);
                        self.fail("save", e)
                    }
                }
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let (Some(r), Overlay::None) = (self.rich.as_mut(), &self.overlay) { r.paste(text); return; }
        match &mut self.overlay {
            Overlay::Edit(s) => s.paste(text),
            Overlay::Prompt(p) => { for c in text.lines().next().unwrap_or("").chars() { p.text.insert(p.cursor, c); p.cursor += 1; } }
            Overlay::Picker(_) => { for c in text.lines().next().unwrap_or("").chars() { self.picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)); } }
            _ => {}
        }
    }

    /// Content and file extension to hand to the editor.
    pub fn edit_payload(&self, id: &str) -> Option<(String, &'static str)> {
        let n = self.model.notes.get(id)?;
        let content = self.db.note_content(id).ok().flatten().unwrap_or_default();
        Some((content, crate::editor::extension_for(&n.ntype, &n.mime)))
    }

    pub fn finish_edit(&mut self, id: &str, result: anyhow::Result<Option<String>>) {
        let base = self.edit_base.take();
        match result {
            Ok(None) => self.status = "no changes".into(),
            Ok(Some(new)) => {
                // a sync may have changed the note while the editor was open; never overwrite that unseen
                let stored = self.db.note_content(id).ok().flatten();
                if base.is_some() && stored != base {
                    let kept = self.save_recovery(id, &new);
                    self.status = match kept {
                        Some(p) => format!("⚠ the note changed while you were editing; NOT overwritten. Your text is in {}", p.display()),
                        None => "⚠ the note changed while you were editing and your text could not be saved".into(),
                    };
                    return;
                }
                match self.db.set_note_content(id, &new) {
                    Ok(()) => { self.changed(Some(id)); self.status = "saved".into(); }
                    Err(e) => match self.save_recovery(id, &new) {
                        Some(p) => self.status = format!("save failed: {e:#}. Your text is in {}", p.display()),
                        None => self.fail("save", e),
                    },
                }
            }
            Err(e) => self.fail("editor", e),
        }
    }
}

impl App {
    /// Unlock / lock / protect / unprotect (command palette).
    pub fn run_protect_action(&mut self, a: Action) {
        match a {
            Action::UnlockProtected => {
                if self.db.is_unlocked() { self.status = "protected notes are already unlocked".into(); }
                else if self.db.has_password() { self.open_secret_prompt(PromptKind::Unlock, "Protected-notes password"); }
                else { self.open_secret_prompt(PromptKind::NewPassword, "No protected-notes password yet: choose one (it can't be recovered)"); }
            }
            Action::DeleteBackups => {
                let n = self.db.backups().len();
                if n == 0 { self.status = "no automatic backups found".into(); }
                else { self.open_prompt(PromptKind::DeleteBackups, &format!("Delete {n} automatic backup(s)? Type yes"), ""); }
            }
            Action::LockProtected => {
                self.lock_protected("protected notes locked");
            }
            Action::ProtectNote | Action::UnprotectNote | Action::ProtectSubtree | Action::UnprotectSubtree => {
                let Some((_, note, _)) = self.selected() else { return };
                let protect = matches!(a, Action::ProtectNote | Action::ProtectSubtree);
                let recursive = matches!(a, Action::ProtectSubtree | Action::UnprotectSubtree);
                if let Err(e) = self.db.ensure_writable() { self.status = format!("{e:#}"); return; }
                if !self.db.is_unlocked() {
                    // protecting needs the key too: ask for it (or for a first password) and tell the user to repeat
                    self.run_protect_action(Action::UnlockProtected);
                    return;
                }
                match self.db.protect_note(&note, protect, recursive) {
                    Ok(n) => {
                        self.changed(Some(&note));
                        self.status = format!("{} {n} note(s)", if protect { "protected" } else { "unprotected" });
                        if protect && !self.db.backups().is_empty() { self.status.push_str("; older backups still hold the plaintext (palette: Delete the automatic backups)"); }
                    }
                    Err(e) => self.fail(if protect { "protect" } else { "unprotect" }, e),
                }
            }
            _ => {}
        }
    }

    fn finish_unlock(&mut self, password: &str) {
        match self.db.unlock(password) {
            Ok(true) => { self.reload(); self.status = "protected notes unlocked".into(); }
            Ok(false) => self.status = "wrong password".into(),
            Err(e) => self.fail("unlock", e),
        }
    }

    pub fn lock_protected(&mut self, msg: &str) {
        if !self.db.is_unlocked() { return; }
        // an in-place edit of a protected note cannot be saved once locked: keep its text (encrypted) and close it
        if self.rich.as_ref().is_some_and(|r| self.model.notes.get(&r.note_id).is_some_and(|n| n.protected)) {
            self.save_unsaved_on_exit();
            self.rich = None;
        }
        // everything that holds decrypted text goes: overlays built from it, and the views of other tabs
        if matches!(self.overlay, Overlay::Panel(_) | Overlay::Image(_) | Overlay::Picker(_) | Overlay::Prompt(_) | Overlay::Confirm(_)) { self.overlay = Overlay::None; }
        self.panel_return = None;
        self.picker = None;
        let protected = |model: &trilium_core::model::Model, id: &str| model.notes.get(id).is_some_and(|n| n.protected);
        let model = std::mem::take(&mut self.model);
        for t in self.tabs.iter_mut().chain(self.closed_tabs.iter_mut()) {
            if t.view.as_ref().is_some_and(|v| protected(&model, &v.id)) { t.view = None; }
        }
        self.model = model;
        self.db.lock();
        self.edit_base = None;
        self.pending_edit = None;
        self.reload();
        self.status = msg.into();
    }

    /// Lock after `protectedSessionTimeout` seconds without input (never while the built-in editor holds text).
    pub fn check_autolock(&mut self) {
        if !self.db.is_unlocked() || matches!(self.overlay, Overlay::Edit(_)) || self.rich.is_some() { return; }
        let secs = self.db.protected_session_timeout();
        if secs > 0 && self.last_input.elapsed().as_secs() >= secs { self.lock_protected("protected notes locked after inactivity"); }
    }
}

const MAX_PICTURE_BYTES: usize = 20 * 1024 * 1024;

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

/// A path as a terminal drop or paste delivers it: quotes, `file://`, `\ ` escapes, `~/`.
fn clean_path(input: &str) -> std::path::PathBuf {
    let mut t = input.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
    if let Some(rest) = t.strip_prefix("file://") {
        let b = rest.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) { out.push(v); i += 3; continue; }
            }
            out.push(b[i]);
            i += 1;
        }
        t = String::from_utf8_lossy(&out).into_owned();
    }
    let t = t.replace("\\ ", " ");
    match t.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| t.clone().into()),
        None => t.into(),
    }
}

fn image_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) { Some("image/png") }
    else if b.starts_with(&[0xFF, 0xD8, 0xFF]) { Some("image/jpeg") }
    else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { Some("image/gif") }
    else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { Some("image/webp") }
    else { None }
}

/// The picture to insert: a file, or (for empty input) whatever image the clipboard holds. Returns (bytes, file name).
fn image_bytes_from(input: &str) -> Result<(Vec<u8>, String), String> {
    if input.trim().is_empty() {
        for (prog, args) in [("wl-paste", &["--type", "image/png"][..]), ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"][..])] {
            if let Ok(o) = std::process::Command::new(prog).args(args).stderr(std::process::Stdio::null()).output() {
                if o.status.success() && !o.stdout.is_empty() {
                    if o.stdout.len() > MAX_PICTURE_BYTES { return Err("the clipboard picture is over 20 MB".into()); }
                    return Ok((o.stdout, format!("pasted-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S"))));
                }
            }
        }
        return Err("no picture on the clipboard (type or drop a file path instead)".into());
    }
    let path = clean_path(input);
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() { return Err(format!("{} isn't a file", path.display())); }
    if meta.len() as usize > MAX_PICTURE_BYTES { return Err("that picture is over 20 MB".into()); }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = crate::images::safe_name(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), "picture.png");
    Ok((bytes, name))
}
