//! Editing operations ported from Trilium's own semantics (`notes.ts`, `bnote.ts`, `bbranch.ts`,
//! `routes/api/branches.ts`): revisions, link relations, soft deletion, reordering and moving.
//! Every function leaves `entity_changes` complete so the next push sends everything needed.
use crate::{db::Db, panels::AttachmentInfo, write::*};
use anyhow::{Context, Result, bail};
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundLink { name: &'static str, value: String }

impl Db {
    pub(crate) fn option_i64(&self, name: &str, default: i64) -> i64 {
        self.conn.query_row("SELECT value FROM options WHERE name=?", [name], |r| r.get::<_, String>(0))
            .ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    fn note_row(&self, note_id: &str) -> Result<(String, String, String, i64, Option<String>, String)> {
        self.conn.query_row(
            "SELECT type,mime,title,isProtected,blobId,utcDateCreated FROM notes WHERE noteId=? AND isDeleted=0", [note_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .optional()?.with_context(|| format!("note {note_id} not found"))
    }

    // ------------------------------------------------------------ revisions

    /// `saveRevisionIfNeeded`: snapshot at most once per `revisionSnapshotTimeInterval`.
    fn save_revision_if_needed(&self, note_id: &str) -> Result<()> {
        let (ntype, _mime, _t, _p, _b, created) = self.note_row(note_id)?;
        if ntype == "file" || ntype == "image" { return Ok(()); }
        let disabled: Option<String> = self.conn.query_row(
            "SELECT value FROM attributes WHERE noteId=? AND type='label' AND name='disableVersioning' AND isDeleted=0",
            [note_id], |r| r.get(0)).optional()?;
        if disabled.is_some_and(|v| v != "false" && v != "0") { return Ok(()); } // isLabelTruthy
        let interval = self.option_i64("revisionSnapshotTimeInterval", 600);
        let now = chrono::Utc::now();
        let cutoff = (now - chrono::Duration::seconds(interval)).format("%Y-%m-%d %H:%M:%S%.3fZ").to_string();
        let recent: Option<String> = self.conn.query_row(
            "SELECT revisionId FROM revisions WHERE noteId=? AND utcDateCreated>=?", params![note_id, cutoff], |r| r.get(0)).optional()?;
        let age_ms = chrono::NaiveDateTime::parse_from_str(created.trim_end_matches('Z'), "%Y-%m-%d %H:%M:%S%.f")
            .map(|c| (now.naive_utc() - c).num_milliseconds()).unwrap_or(i64::MAX);
        if recent.is_none() && age_ms >= interval * 1000 { self.save_revision(note_id, "", "auto")?; }
        Ok(())
    }

    /// `BNote.saveRevision`: snapshot current title/type/content (and attachments) into a revision.
    pub fn save_revision(&self, note_id: &str, description: &str, source: &str) -> Result<String> {
        let (ntype, mime, title, protected, blob_id, _c) = self.note_row(note_id)?;
        if protected != 0 { self.key()?; }
        let (date_modified, utc_modified): (String, String) = self.conn.query_row(
            "SELECT dateModified,utcDateModified FROM notes WHERE noteId=?", [note_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut content = match blob_id.as_deref() {
            Some(b) => {
                let v = self.conn.query_row("SELECT content FROM blobs WHERE blobId=?", [b], |r| r.get::<_, rusqlite::types::Value>(0))?;
                self.plain_bytes(protected != 0, v)?
            }
            None => vec![],
        };
        let revision_id = new_entity_id();
        let tx = self.begin()?;
        // copy attachments to the revision and repoint references in text content
        let atts: Vec<(String, String, String, String, i64, Option<String>, i64)> = {
            let mut st = self.conn.prepare("SELECT attachmentId,role,mime,title,position,blobId,isProtected FROM attachments WHERE ownerId=? AND isDeleted=0")?;
            st.query_map([note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (old_id, role, amime, atitle, pos, ablob, aprot) in atts {
            let new_id = new_entity_id();
            let utc = now_utc();
            self.conn.execute(
                "INSERT INTO attachments(attachmentId,ownerId,role,mime,title,isProtected,position,blobId,dateModified,utcDateModified,isDeleted)
                 VALUES(?,?,?,?,?,?,?,?,?,?,0)", params![new_id, revision_id, role, amime, atitle, aprot, pos, ablob, now_local(), utc])?;
            self.record_entity("attachments", "attachmentId", &new_id, "utcDateModified")?;
            if ntype == "text" {
                if let Ok(text) = String::from_utf8(content.clone()) {
                    let text = text.replace(&format!("attachments/{old_id}"), &format!("attachments/{new_id}"));
                    let href = Regex::new(&format!(r#"(?i)href="[^"]*attachmentId={old_id}[^"]*""#)).unwrap();
                    content = href.replace_all(&text, format!(r#"href="api/attachments/{new_id}/download""#)).into_owned().into_bytes();
                }
            }
        }
        let new_blob = self.save_blob_for(protected != 0, &content, is_string_note(&ntype, &mime))?;
        let (now_u, now_l) = (now_utc(), now_local());
        self.conn.execute(
            "INSERT INTO revisions(revisionId,noteId,type,mime,title,description,source,isProtected,blobId,utcDateLastEdited,utcDateCreated,utcDateModified,dateLastEdited,dateCreated)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![revision_id, note_id, ntype, mime, title, description, source, protected, new_blob, utc_modified, now_u, now_u, date_modified, now_l])?;
        self.record_entity("revisions", "revisionId", &revision_id, "utcDateModified")?;
        tx.commit()?;
        Ok(revision_id)
    }

    // ------------------------------------------------------------ title & content

    pub fn set_note_title(&self, note_id: &str, title: &str) -> Result<()> {
        let protected = self.note_row(note_id)?.3 != 0;
        let old: String = self.conn.query_row("SELECT title FROM notes WHERE noteId=?", [note_id], |r| r.get(0))?;
        // a protected title is stored encrypted; the plaintext must never reach the database
        let stored = if protected { self.encrypt_title(title)? } else { title.to_string() };
        if protected { if self.decrypt_title(&old) == title { return Ok(()); } } else if old == title { return Ok(()); }
        self.save_revision_if_needed(note_id)?;
        let tx = self.begin()?;
        self.conn.execute("UPDATE notes SET title=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![stored, now_local(), now_utc(), note_id])?;
        self.record_entity("notes", "noteId", note_id, "utcDateModified")?;
        tx.commit()?;
        Ok(())
    }

    /// Replace content: revision if due, link relations maintained (text/markdown), blob swapped.
    pub fn set_note_content(&self, note_id: &str, content: &str) -> Result<()> {
        let (ntype, mime, _t, protected, old_blob, _c) = self.note_row(note_id)?;
        if protected != 0 { self.key()?; }
        if ntype == "file" || ntype == "image" { bail!("{ntype} notes hold binary data; replace them with set_note_bytes"); }
        let is_md = ntype == "code" && matches!(mime.as_str(), "text/markdown" | "text/x-markdown" | "text/x-gfm");
        let current = self.note_content(note_id)?.unwrap_or_default();
        if current == content && !(ntype == "text" && normalize_links(content) != content) { return Ok(()); }
        self.save_revision_if_needed(note_id)?;
        let tx = self.begin()?;
        let content = if ntype == "text" || is_md { self.save_links(note_id, content, is_md)? } else { content.to_string() };
        let new_blob = self.save_blob_for(protected != 0, content.as_bytes(), is_string_note(&ntype, &mime))?;
        if old_blob.as_deref() != Some(&new_blob) {
            self.conn.execute("UPDATE notes SET blobId=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![new_blob, now_local(), now_utc(), note_id])?;
            self.record_entity("notes", "noteId", note_id, "utcDateModified")?;
            if let Some(old) = old_blob { self.drop_blob_if_unused(&old)?; }
        }
        tx.commit()?;
        Ok(())
    }

    /// Create an `image` (mime image/*) or `file` note holding raw bytes, like Trilium's file import.
    pub fn create_file_note(&self, parent_id: &str, title: &str, mime: &str, bytes: &[u8]) -> Result<String> {
        let ntype = if mime.starts_with("image/") && mime != "image/svg+xml" { "image" } else { "file" };
        let tx = self.begin()?;
        let id = self.insert_note_raw(parent_id, title, ntype, mime, "", None, None)?;
        self.set_note_bytes(&id, bytes)?;
        self.sort_children_if_needed(parent_id)?;
        tx.commit()?;
        Ok(id)
    }

    /// Change a note's type and mime (content is kept as stored; turning a text note into a `book` collection is lossless).
    pub fn set_note_type(&self, note_id: &str, ntype: &str, mime: &str) -> Result<()> {
        const TYPES: &[&str] = &["text", "code", "file", "image", "search", "book", "relationMap", "render", "canvas", "mermaid", "mindMap", "noteMap", "webView"];
        if !TYPES.contains(&ntype) { bail!("unknown note type '{ntype}'"); }
        let (old_type, ..) = self.note_row(note_id)?;
        if old_type == ntype { return Ok(()); }
        let tx = self.begin()?;
        self.conn.execute("UPDATE notes SET type=?, mime=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![ntype, mime, now_local(), now_utc(), note_id])?;
        self.record_entity("notes", "noteId", note_id, "utcDateModified")?;
        tx.commit()
    }

    /// Replace a note's content with raw bytes (image / file notes). No revision is taken (those types are versioned separately).
    pub fn set_note_bytes(&self, note_id: &str, bytes: &[u8]) -> Result<()> {
        let (_t, _m, _ti, protected, old_blob, _c) = self.note_row(note_id)?;
        if protected != 0 { self.key()?; }
        let tx = self.begin()?;
        let new_blob = self.save_blob_for(protected != 0, bytes, false)?;
        if old_blob.as_deref() != Some(&new_blob) {
            self.conn.execute("UPDATE notes SET blobId=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![new_blob, now_local(), now_utc(), note_id])?;
            self.record_entity("notes", "noteId", note_id, "utcDateModified")?;
            if let Some(old) = old_blob { self.drop_blob_if_unused(&old)?; }
        }
        tx.commit()
    }

    /// `saveLinks` for text and markdown notes: keeps `internalLink` / `imageLink` /
    /// `includeNoteLink` relations in sync with the content. Returns the normalized content.
    pub(crate) fn save_links(&self, note_id: &str, content: &str, markdown: bool) -> Result<String> {
        static IMG: OnceLock<Regex> = OnceLock::new();
        static INT: OnceLock<Regex> = OnceLock::new();
        static INC: OnceLock<Regex> = OnceLock::new();
        static MD_IMG: OnceLock<Regex> = OnceLock::new();
        static MD_INT: OnceLock<Regex> = OnceLock::new();
        static MD_WIKI: OnceLock<Regex> = OnceLock::new();
        let mut found: Vec<FoundLink> = Vec::new();
        let out = if markdown {
            for c in re(&MD_IMG, r"api/images/([a-zA-Z0-9_]+)/").captures_iter(content) { found.push(FoundLink { name: "imageLink", value: c[1].into() }); }
            for c in re(&MD_INT, r"#root[a-zA-Z0-9_/]*/([a-zA-Z0-9_]+)").captures_iter(content) { found.push(FoundLink { name: "internalLink", value: c[1].into() }); }
            for c in re(&MD_WIKI, r"\[\[([a-zA-Z0-9_]+)\]\]").captures_iter(content) { found.push(FoundLink { name: "internalLink", value: c[1].into() }); }
            content.to_string()
        } else {
            for c in re(&IMG, r#"src="[^"]*api/images/([a-zA-Z0-9_]+)/"#).captures_iter(content) { found.push(FoundLink { name: "imageLink", value: c[1].into() }); }
            for c in re(&INT, r#"href="[^"]*#root[a-zA-Z0-9_/]*/([a-zA-Z0-9_]+)/?(?:\?[^"]*)?""#).captures_iter(content) { found.push(FoundLink { name: "internalLink", value: c[1].into() }); }
            for c in re(&INC, r#"<(?:figure|section) class="include-note[^>]+data-note-id="([a-zA-Z0-9_]+)"[^>]*>"#).captures_iter(content) { found.push(FoundLink { name: "includeNoteLink", value: c[1].into() }); }
            normalize_links(content)
        };
        let existing: Vec<(String, String, String)> = {
            let mut st = self.conn.prepare(
                "SELECT attributeId,name,value FROM attributes WHERE noteId=? AND type='relation' AND isDeleted=0
                 AND name IN ('internalLink','imageLink','relationMapLink','includeNoteLink')")?;
            st.query_map([note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut present: Vec<(String, String)> = existing.iter().map(|(_, n, v)| (n.clone(), v.clone())).collect();
        for f in &found {
            let target_exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [&f.value], |r| r.get(0)).optional()?;
            if target_exists.is_none() || present.iter().any(|(n, v)| n == f.name && *v == f.value) { continue; }
            self.add_attribute(note_id, "relation", f.name, &f.value, false)?;
            present.push((f.name.to_string(), f.value.clone()));
        }
        for (id, name, value) in existing {
            if !found.iter().any(|f| f.name == name && f.value == value) { self.mark_deleted("attributes", "attributeId", &id, None)?; }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ attributes

    pub fn add_attribute(&self, note_id: &str, kind: &str, name: &str, value: &str, inheritable: bool) -> Result<String> {
        self.note_row(note_id).with_context(|| format!("note {note_id} doesn't exist"))?;
        let id = new_entity_id();
        let pos: i64 = self.conn.query_row("SELECT COALESCE(MAX(position),0) FROM attributes WHERE noteId=? AND isDeleted=0", [note_id], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO attributes(attributeId,noteId,type,name,value,position,utcDateModified,isDeleted,isInheritable) VALUES(?,?,?,?,?,?,?,0,?)",
            params![id, note_id, kind, name, value, pos + 10, now_utc(), inheritable as i64])?;
        self.record_entity("attributes", "attributeId", &id, "utcDateModified")?;
        if kind == "label" && name == "sorted" { self.sort_children_if_needed(note_id)?; }
        Ok(id)
    }

    // ------------------------------------------------------------ deletion

    /// `markAsDeleted`: soft delete with a deleteId; the server erases later.
    pub(crate) fn mark_deleted(&self, table: &str, pk: &str, id: &str, delete_id: Option<&str>) -> Result<()> {
        let utc = now_utc();
        self.conn.execute(&format!("UPDATE {table} SET isDeleted=1, deleteId=?, utcDateModified=? WHERE {pk}=?"), params![delete_id, utc, id])?;
        if matches!(table, "notes" | "attachments") {
            self.conn.execute(&format!("UPDATE {table} SET dateModified=? WHERE {pk}=?"), params![now_local(), id])?;
        }
        self.record_entity(table, pk, id, "utcDateModified")
    }

    /// `BBranch.deleteBranch`: removes this placement; the note (and, recursively, children
    /// without other parents) is deleted when it was the last strong placement.
    pub fn delete_branch(&self, branch_id: &str) -> Result<()> {
        let tx = self.begin()?;
        let delete_id = new_entity_id()[..10].to_string();
        self.delete_branch_inner(branch_id, &delete_id, 0)?;
        tx.commit()?;
        Ok(())
    }

    fn delete_branch_inner(&self, branch_id: &str, delete_id: &str, depth: usize) -> Result<()> {
        if depth > 200 { bail!("tree too deep (cycle?)"); }
        let (note_id, parent_id): (String, String) = self.conn.query_row(
            "SELECT noteId,parentNoteId FROM branches WHERE branchId=? AND isDeleted=0", [branch_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?.with_context(|| format!("branch {branch_id} not found"))?;
        if note_id == "root" { bail!("Can't delete root"); }
        let weak = |p: &str| p == "_share" || p == "_lbBookmarks";
        self.mark_deleted("branches", "branchId", branch_id, Some(delete_id))?;
        let _ = parent_id;
        let remaining: Vec<(String, String)> = {
            let mut st = self.conn.prepare("SELECT branchId,parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            st.query_map([&note_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        if remaining.iter().any(|(_, p)| !weak(p)) { return Ok(()); }
        for (b, _) in &remaining { self.mark_deleted("branches", "branchId", b, Some(delete_id))?; }
        let children: Vec<String> = {
            let mut st = self.conn.prepare("SELECT branchId FROM branches WHERE parentNoteId=? AND isDeleted=0")?;
            st.query_map([&note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for c in children { self.delete_branch_inner(&c, delete_id, depth + 1)?; }
        let owned: Vec<String> = {
            let mut st = self.conn.prepare("SELECT attributeId FROM attributes WHERE noteId=? AND isDeleted=0")?;
            st.query_map([&note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for a in owned { self.mark_deleted("attributes", "attributeId", &a, Some(delete_id))?; }
        let incoming: Vec<String> = {
            let mut st = self.conn.prepare("SELECT attributeId FROM attributes WHERE type='relation' AND value=? AND isDeleted=0")?;
            st.query_map([&note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for a in incoming { self.mark_deleted("attributes", "attributeId", &a, Some(delete_id))?; }
        let atts: Vec<String> = {
            let mut st = self.conn.prepare("SELECT attachmentId FROM attachments WHERE ownerId=? AND isDeleted=0")?;
            st.query_map([&note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for a in atts { self.mark_deleted("attachments", "attachmentId", &a, Some(delete_id))?; }
        self.mark_deleted("notes", "noteId", &note_id, Some(delete_id))
    }

    // ------------------------------------------------------------ ordering & moving

    fn put_note_reordering(&self, parent_id: &str) -> Result<()> {
        self.put_entity_change("note_reordering", parent_id, "N/A", &now_utc())
    }

    fn branch_info(&self, branch_id: &str) -> Result<(String, String, i64, Option<String>)> {
        self.conn.query_row("SELECT noteId,parentNoteId,notePosition,prefix FROM branches WHERE branchId=? AND isDeleted=0", [branch_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?.with_context(|| format!("branch {branch_id} not found"))
    }

    /// `validateParentChild`: no root moves, no cycles, no duplicate placement.
    fn validate_move(&self, new_parent: &str, note_id: &str, branch_id: Option<&str>) -> Result<()> {
        if ["root", "_hidden", "_share", "_lbRoot", "_lbAvailableLaunchers", "_lbVisibleLaunchers"].contains(&note_id) { bail!("cannot change this note's location"); }
        let pt: Option<String> = self.conn.query_row("SELECT type FROM notes WHERE noteId=? AND isDeleted=0", [new_parent], |r| r.get(0)).optional()?;
        if pt.is_none() { bail!("the target note {new_parent} doesn't exist"); }
        if pt.as_deref() == Some("launcher") && new_parent != "_lbBookmarks" { bail!("launcher notes can't have children"); }
        if new_parent == note_id { bail!("cannot move a note into itself"); }
        let dup: Option<String> = self.conn.query_row(
            "SELECT branchId FROM branches WHERE noteId=? AND parentNoteId=? AND +isDeleted=0", params![note_id, new_parent], |r| r.get(0)).optional()?;
        if dup.as_deref().is_some_and(|d| Some(d) != branch_id) { bail!("the note is already in that parent"); }
        // cycle: new_parent must not be a descendant of note_id
        let mut stack = vec![new_parent.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(n) = stack.pop() {
            if n == note_id { bail!("cannot move a note into its own subtree"); }
            if !seen.insert(n.clone()) { continue; }
            let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            stack.extend(st.query_map([&n], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(())
    }

    /// Move `branch` directly before `target` (possibly under another parent).
    pub fn move_branch_before(&self, branch_id: &str, target_id: &str) -> Result<()> { self.move_branch(branch_id, target_id, false) }
    pub fn move_branch_after(&self, branch_id: &str, target_id: &str) -> Result<()> { self.move_branch(branch_id, target_id, true) }

    fn move_branch(&self, branch_id: &str, target_id: &str, after: bool) -> Result<()> {
        let (note_id, parent, _pos, prefix) = self.branch_info(branch_id)?;
        let (_tn, t_parent, t_pos, _tp) = self.branch_info(target_id)?;
        self.validate_move(&t_parent, &note_id, Some(branch_id))?;
        let tx = self.begin()?;
        // positions of siblings shift WITHOUT touching utcDateModified (upstream does the same,
        // reordering is synced through the note_reordering change instead)
        let new_pos = if after { t_pos + 10 } else { t_pos };
        let cmp = if after { ">" } else { ">=" };
        self.conn.execute(&format!("UPDATE branches SET notePosition=notePosition+10 WHERE parentNoteId=? AND notePosition {cmp} ? AND isDeleted=0"), params![t_parent, t_pos])?;
        if parent == t_parent {
            self.conn.execute("UPDATE branches SET notePosition=?, utcDateModified=? WHERE branchId=?", params![new_pos, now_utc(), branch_id])?;
            self.record_entity("branches", "branchId", branch_id, "utcDateModified")?;
        } else {
            let new_id = format!("{t_parent}_{note_id}");
            self.conn.execute(
                "INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,?,0,0,?)",
                params![new_id, note_id, t_parent, new_pos, prefix, now_utc()])?;
            self.record_entity("branches", "branchId", &new_id, "utcDateModified")?;
            self.mark_deleted("branches", "branchId", branch_id, None)?;
            self.put_note_reordering(&parent)?;
        }
        self.put_note_reordering(&t_parent)?;
        tx.commit()?;
        Ok(())
    }

    /// `cloneNoteToParentNote`: add a placement of `note_id` under `parent_id` (appended last).
    pub fn clone_note(&self, note_id: &str, parent_id: &str, prefix: Option<&str>) -> Result<String> {
        let ptype: Option<String> = self.conn.query_row("SELECT type FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)).optional()?;
        let Some(ptype) = ptype else { bail!("the target note is deleted or missing") };
        if ptype == "search" { bail!("can't clone into a search note"); }
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| r.get(0)).optional()?;
        if exists.is_none() { bail!("the note to clone is deleted or missing"); }
        self.validate_move(parent_id, note_id, None)?;
        let tx = self.begin()?;
        let pos: i64 = self.conn.query_row("SELECT COALESCE(MAX(notePosition),0) FROM branches WHERE parentNoteId=? AND isDeleted=0 AND noteId != '_hidden'", [parent_id], |r| r.get(0))?;
        let branch_id = format!("{parent_id}_{note_id}");
        self.conn.execute(
            "INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,?,0,0,?)",
            params![branch_id, note_id, parent_id, pos + 10, prefix.filter(|p| !p.trim().is_empty()), now_utc()])?;
        self.record_entity("branches", "branchId", &branch_id, "utcDateModified")?;
        self.sort_children_if_needed(parent_id)?;
        tx.commit()?;
        Ok(branch_id)
    }

    /// Move a placement to the end of another parent (`moveBranchToNote`).
    pub fn move_branch_to(&self, branch_id: &str, new_parent: &str) -> Result<String> {
        let (note_id, parent, _pos, prefix) = self.branch_info(branch_id)?;
        if parent == new_parent { bail!("already in that parent"); }
        self.validate_move(new_parent, &note_id, Some(branch_id))?;
        let tx = self.begin()?;
        let last: i64 = self.conn.query_row("SELECT COALESCE(MAX(notePosition),0) FROM branches WHERE parentNoteId=? AND isDeleted=0 AND noteId != '_hidden'", [new_parent], |r| r.get(0))?;
        let new_id = format!("{new_parent}_{note_id}");
        self.conn.execute(
            "INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,?,0,0,?)",
            params![new_id, note_id, new_parent, last + 10, prefix, now_utc()])?;
        self.record_entity("branches", "branchId", &new_id, "utcDateModified")?;
        self.mark_deleted("branches", "branchId", branch_id, None)?;
        self.put_note_reordering(&parent)?;
        self.put_note_reordering(new_parent)?;
        self.sort_children_if_needed(new_parent)?;
        tx.commit()?;
        Ok(new_id)
    }

    /// `duplicateSubtree`: deep copy of a note (and, with `with_children`, everything below) under `parent_id`,
    /// right after the original when it lives there. Links and relations pointing inside the copied tree are
    /// remapped to the copies; the new root's title gets " (dup)". Returns the new note id.
    pub fn duplicate_subtree(&self, note_id: &str, parent_id: &str, with_children: bool) -> Result<String> {
        if note_id == "root" { bail!("duplicating root is not possible"); }
        if self.is_descendant_of(parent_id, note_id)? { bail!("can't duplicate a note into its own subtree"); }
        let protected: Option<i64> = self.conn.query_row("SELECT isProtected FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| r.get(0)).optional()?;
        if protected.context("note not found")? != 0 { self.key()?; }
        let tx = self.begin()?;
        // pre-generate ids for every note of the tree so references can be remapped
        let mut mapping: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut stack = vec![note_id.to_string()];
        while let Some(n) = stack.pop() {
            if mapping.contains_key(&n) { continue; }
            mapping.insert(n.clone(), new_entity_id());
            if with_children {
                let mut st = self.conn.prepare("SELECT noteId FROM branches WHERE parentNoteId=? AND +isDeleted=0")?;
                stack.extend(st.query_map([&n], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
            }
        }
        let orig_pos: Option<i64> = self.conn.query_row("SELECT notePosition FROM branches WHERE noteId=? AND parentNoteId=? AND isDeleted=0", params![note_id, parent_id], |r| r.get(0)).optional()?;
        let mut done = std::collections::HashSet::new();
        let new_root = self.duplicate_inner(note_id, parent_id, orig_pos.map(|p| p + 1), &mapping, with_children, &mut done, 0)?;
        let (raw, prot): (String, i64) = self.conn.query_row("SELECT title,isProtected FROM notes WHERE noteId=?", [&new_root], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let title = if prot != 0 { self.decrypt_title(&raw) } else { raw };
        if !title.ends_with("(dup)") && !title.starts_with("(dup)") {
            let new_title = format!("{title} (dup)");
            let stored = if prot != 0 { self.encrypt_title(&new_title)? } else { new_title };
            self.conn.execute("UPDATE notes SET title=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![stored, now_local(), now_utc(), new_root])?;
            self.record_entity("notes", "noteId", &new_root, "utcDateModified")?;
        }
        self.sort_children_if_needed(parent_id)?;
        tx.commit()?;
        Ok(new_root)
    }

    #[allow(clippy::too_many_arguments)]
    fn duplicate_inner(&self, orig: &str, parent: &str, position: Option<i64>, mapping: &std::collections::HashMap<String, String>,
                       with_children: bool, done: &mut std::collections::HashSet<String>, depth: usize) -> Result<String> {
        if depth > 64 { bail!("tree too deep"); }
        let new_id = mapping.get(orig).cloned().context("note is not part of the duplicated tree")?;
        if done.contains(orig) {
            // a note with several placements inside the copied tree: only add the branch
            let pos = position.unwrap_or(0);
            let bid = format!("{parent}_{new_id}");
            self.conn.execute("INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,NULL,0,0,?)", params![bid, new_id, parent, pos, now_utc()])?;
            self.record_entity("branches", "branchId", &bid, "utcDateModified")?;
            return Ok(new_id);
        }
        done.insert(orig.to_string());
        let (title, ntype, mime, protected): (String, String, String, i64) = self.conn.query_row("SELECT title,type,mime,isProtected FROM notes WHERE noteId=? AND isDeleted=0", [orig], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        // work on plaintext; the copy is protected again below, wherever it lands
        let title = if protected != 0 { self.key()?; self.decrypt_title(&title) } else { title };
        let pos = position;
        if ntype == "file" || ntype == "image" {
            // binary content must be copied byte for byte, never through a string
            self.insert_note_with_id(&new_id, parent, &title, &ntype, &mime, "", None, pos)?;
            if let Some(bytes) = self.note_bytes(orig)? { self.set_note_bytes(&new_id, &bytes)?; }
        } else {
            let mut content = self.note_content(orig)?.unwrap_or_default();
            if matches!(ntype.as_str(), "text" | "relationMap" | "search") {
                for (o, n) in mapping { content = content.replace(o.as_str(), n); }
            }
            self.insert_note_with_id(&new_id, parent, &title, &ntype, &mime, &content, None, pos)?;
        }
        // attachments travel with the note; text that points at them is re-pointed at the copies
        let atts: Vec<AttachmentInfo> = self.attachments(orig)?;
        if !atts.is_empty() {
            let mut amap: Vec<(String, String)> = vec![];
            for a in &atts {
                if let Some(bytes) = self.attachment_bytes(&a.id)? { amap.push((a.id.clone(), self.add_attachment(&new_id, &a.title, &a.mime, &a.role, &bytes)?)); }
            }
            if ntype == "text" && !amap.is_empty() {
                let mut c = self.note_content(&new_id)?.unwrap_or_default();
                let before = c.clone();
                for (o, n) in &amap { c = c.replace(o.as_str(), n); }
                if c != before { self.set_note_content(&new_id, &c)?; }
            }
        }
        if protected != 0 && !self.note_protected(&new_id)? { self.protect_note(&new_id, true, false)?; }
        let attrs: Vec<(String, String, String, i64, Option<i64>)> = {
            let mut st = self.conn.prepare("SELECT type,name,value,position,isInheritable FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
            st.query_map([orig], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (kind, name, value, apos, inh) in attrs {
            let value = if kind == "relation" { mapping.get(&value).cloned().unwrap_or(value) } else { value };
            // link relations may already exist on the copy (maintained from its content)
            let dup: Option<i64> = self.conn.query_row("SELECT 1 FROM attributes WHERE noteId=? AND type=? AND name=? AND value=? AND isDeleted=0", params![new_id, kind, name, value], |r| r.get(0)).optional()?;
            if dup.is_some() && kind == "relation" && matches!(name.as_str(), "internalLink" | "imageLink" | "includeNoteLink" | "relationMapLink") { continue; }
            let id = new_entity_id();
            self.conn.execute("INSERT INTO attributes(attributeId,noteId,type,name,value,position,utcDateModified,isDeleted,isInheritable) VALUES(?,?,?,?,?,?,?,0,?)",
                params![id, new_id, kind, name, value, apos, now_utc(), inh.unwrap_or(0)])?;
            self.record_entity("attributes", "attributeId", &id, "utcDateModified")?;
        }
        if with_children {
            let kids: Vec<(String, i64)> = {
                let mut st = self.conn.prepare("SELECT noteId,notePosition FROM branches WHERE parentNoteId=? AND isDeleted=0 ORDER BY notePosition")?;
                st.query_map([orig], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for (kid, kpos) in kids { self.duplicate_inner(&kid, &new_id, Some(kpos), mapping, true, done, depth + 1)?; }
        }
        Ok(new_id)
    }

    /// Ctrl+Up: swap with the previous sibling.
    pub fn move_branch_up(&self, branch_id: &str) -> Result<bool> {
        let (_n, parent, pos, _p) = self.branch_info(branch_id)?;
        let prev: Option<String> = self.conn.query_row(
            "SELECT branchId FROM branches WHERE parentNoteId=? AND isDeleted=0 AND notePosition<? ORDER BY notePosition DESC LIMIT 1",
            params![parent, pos], |r| r.get(0)).optional()?;
        match prev { Some(p) => { self.move_branch_before(branch_id, &p)?; Ok(true) } None => Ok(false) }
    }

    /// Ctrl+Down: swap with the next sibling.
    pub fn move_branch_down(&self, branch_id: &str) -> Result<bool> {
        let (_n, parent, pos, _p) = self.branch_info(branch_id)?;
        let next: Option<String> = self.conn.query_row(
            "SELECT branchId FROM branches WHERE parentNoteId=? AND isDeleted=0 AND notePosition>? ORDER BY notePosition ASC LIMIT 1",
            params![parent, pos], |r| r.get(0)).optional()?;
        match next { Some(n) => { self.move_branch_after(branch_id, &n)?; Ok(true) } None => Ok(false) }
    }

    /// Ctrl+Left: move to just after its parent, in the grandparent.
    pub fn move_branch_out(&self, branch_id: &str) -> Result<bool> {
        let (_n, parent, _pos, _p) = self.branch_info(branch_id)?;
        let parent_branch: Option<String> = self.conn.query_row(
            "SELECT branchId FROM branches WHERE noteId=? AND +isDeleted=0 LIMIT 1", [&parent], |r| r.get(0)).optional()?;
        match parent_branch.filter(|_| parent != "root") { Some(pb) => { self.move_branch_after(branch_id, &pb)?; Ok(true) } None => Ok(false) }
    }

    /// Ctrl+Right: become the last child of the previous sibling.
    pub fn move_branch_in(&self, branch_id: &str) -> Result<bool> {
        let (note_id, parent, pos, prefix) = self.branch_info(branch_id)?;
        let prev: Option<String> = self.conn.query_row(
            "SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0 AND notePosition<? ORDER BY notePosition DESC LIMIT 1",
            params![parent, pos], |r| r.get(0)).optional()?;
        let Some(new_parent) = prev else { return Ok(false) };
        self.validate_move(&new_parent, &note_id, Some(branch_id))?;
        let tx = self.begin()?;
        let last: i64 = self.conn.query_row("SELECT COALESCE(MAX(notePosition),0) FROM branches WHERE parentNoteId=? AND isDeleted=0", [&new_parent], |r| r.get(0))?;
        let new_id = format!("{new_parent}_{note_id}");
        self.conn.execute(
            "INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,?,0,0,?)",
            params![new_id, note_id, new_parent, last + 10, prefix, now_utc()])?;
        self.record_entity("branches", "branchId", &new_id, "utcDateModified")?;
        self.mark_deleted("branches", "branchId", branch_id, None)?;
        self.put_note_reordering(&parent)?;
        self.put_note_reordering(&new_parent)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn set_branch_prefix(&self, branch_id: &str, prefix: &str) -> Result<()> {
        self.branch_info(branch_id)?;
        let p = prefix.trim();
        let tx = self.begin()?;
        self.conn.execute("UPDATE branches SET prefix=?, utcDateModified=? WHERE branchId=?", params![if p.is_empty() { None } else { Some(p) }, now_utc(), branch_id])?;
        self.record_entity("branches", "branchId", branch_id, "utcDateModified")?;
        tx.commit()?;
        Ok(())
    }

    /// Create a note directly after `branch_id` (Ctrl+O, "create note after").
    pub fn create_note_after(&self, branch_id: &str, title: &str, ntype: &str, mime: &str, content: &str) -> Result<String> {
        let (_n, parent, pos, _p) = self.branch_info(branch_id)?;
        let id = self.create_note(&parent, title, ntype, mime, content)?;
        let new_branch = format!("{parent}_{id}");
        self.move_branch_after(&new_branch, branch_id).or_else(|_| -> Result<()> { let _ = pos; Ok(()) })?;
        Ok(id)
    }

    /// Branch id of the note's first placement.
    pub fn first_branch_of(&self, note_id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT branchId FROM branches WHERE noteId=? AND +isDeleted=0 ORDER BY notePosition LIMIT 1", [note_id], |r| r.get(0)).optional()?)
    }
}

/// Absolute server references are stripped so links work across instances (`findInternalLinks`
/// and `findImageLinks` do this to every saved text note).
pub fn normalize_links(content: &str) -> String {
    static A: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    let s = re(&A, r#"src="[^"]*/api/images/"#).replace_all(content, r#"src="api/images/"#);
    re(&B, r#"href="[^"]*#root"#).replace_all(&s, r##"href="#root"##).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("e.db")).unwrap();
        db.set_state("instanceId", "testinstance").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn assert_hashes_ok(db: &Db) {
        for (name, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{name} hashes must verify"); }
    }

    fn children(db: &Db, parent: &str) -> Vec<String> {
        let m = Model::load(db).unwrap();
        m.children.get(parent).map(|c| c.iter().map(|b| m.title_of(&b.note_id).to_string()).collect()).unwrap_or_default()
    }

    #[test]
    fn review_regressions_binary_protected_and_missing_targets() {
        let (_d, db) = fixture();
        // binary notes survive duplication byte for byte, with their attachments
        let bytes: Vec<u8> = (0..=255u8).collect();
        let folder = db.create_note("root", "Folder", "text", "text/html", "").unwrap();
        let file = db.create_file_note(&folder, "blob.bin", "application/octet-stream", &bytes).unwrap();
        let att = db.add_attachment(&folder, "a.png", "image/png", "image", &bytes).unwrap();
        db.set_note_content(&folder, &format!("<p><img src=\"api/attachments/{att}/image/a.png\"></p>")).unwrap();
        let copy = db.duplicate_subtree(&folder, "root", true).unwrap();
        let m = Model::load(&db).unwrap();
        let copied_file = m.children[&copy][0].note_id.clone();
        assert_ne!(copied_file, file);
        assert_eq!(db.note_bytes(&copied_file).unwrap().unwrap(), bytes, "binary copy is byte-exact");
        let atts = db.attachments(&copy).unwrap();
        assert_eq!(atts.len(), 1);
        assert_ne!(atts[0].id, att);
        assert_eq!(db.attachment_bytes(&atts[0].id).unwrap().unwrap(), bytes);
        assert!(db.note_content(&copy).unwrap().unwrap().contains(&atts[0].id), "content points at the copy's own attachment");
        assert_hashes_ok(&db);
        // text writes can't touch binary notes
        assert!(db.set_note_content(&file, "oops").is_err());
        assert_eq!(db.note_bytes(&file).unwrap().unwrap(), bytes);
        // locked protected notes: no rename and no duplicate (even as a descendant)
        let secret = db.create_note(&folder, "secret", "text", "text/html", "x").unwrap();
        db.conn.execute("UPDATE notes SET isProtected=1 WHERE noteId=?", [&secret]).unwrap();
        assert!(db.set_note_title(&secret, "plain").is_err());
        assert!(db.duplicate_subtree(&folder, "root", true).is_err(), "protected descendant blocks the copy");
        let before = db.conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0)).unwrap();
        assert!(db.duplicate_subtree(&folder, "root", true).is_err());
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0)).unwrap(), before, "a failed duplicate leaves nothing behind");
        // dangling references are refused
        let b = db.first_branch_of(&file).unwrap().unwrap();
        assert!(db.move_branch_to(&b, "NOPE").is_err());
        assert!(db.add_attribute("NOPE", "label", "x", "", false).is_err());
        assert!(db.add_attachment("NOPE", "t", "text/plain", "file", b"x").is_err());
    }

    #[test]
    fn duplicate_does_not_double_link_relations_and_binary_revisions_are_not_restored() {
        let (_d, db) = fixture();
        let target = db.create_note("root", "T", "text", "text/html", "").unwrap();
        let n = db.create_note("root", "N", "text", "text/html", "").unwrap();
        let att = db.add_attachment(&n, "a.bin", "application/octet-stream", "file", b"x").unwrap();
        db.set_note_content(&n, &format!(r##"<a href="#root/{target}">x</a><a href="api/attachments/{att}/download">f</a>"##)).unwrap();
        let copy = db.duplicate_subtree(&n, "root", false).unwrap();
        let links: Vec<_> = db.attribute_rows(&copy).unwrap().into_iter().filter(|a| a.name == "internalLink").collect();
        assert_eq!(links.len(), 1);
        let f = db.create_file_note("root", "f.bin", "application/octet-stream", &[0, 255, 1]).unwrap();
        let rev = db.save_revision(&f, "", "manual").unwrap();
        assert!(db.restore_revision(&rev).is_err());
        assert_eq!(db.note_bytes(&f).unwrap().unwrap(), vec![0, 255, 1]);
    }

    #[test]
    fn delete_recurses_but_spares_clones() {
        let (_d, db) = fixture();
        let a = db.create_note("root", "A", "text", "text/html", "a").unwrap();
        let b = db.create_note(&a, "B", "text", "text/html", "b").unwrap();
        let c = db.create_note(&b, "C", "text", "text/html", "c").unwrap();
        let shared = db.create_note(&a, "Shared", "text", "text/html", "s").unwrap();
        let other = db.create_note("root", "Other", "text", "text/html", "o").unwrap();
        // clone Shared under Other by hand (cloning proper comes later)
        db.conn.execute("INSERT INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,10,NULL,0,0,'2024-01-01 00:00:00.000Z')",
            params![format!("{other}_{shared}"), shared, other]).unwrap();
        db.record_entity("branches", "branchId", &format!("{other}_{shared}"), "utcDateModified").unwrap();
        db.add_attribute(&c, "label", "x", "1", false).unwrap();
        db.delete_branch(&format!("root_{a}")).unwrap();
        let live = |id: &str| -> i64 { db.conn.query_row("SELECT isDeleted FROM notes WHERE noteId=?", [id], |r| r.get(0)).unwrap() };
        assert_eq!((live(&a), live(&b), live(&c)), (1, 1, 1));
        assert_eq!(live(&shared), 0, "clone elsewhere survives");
        assert_eq!(children(&db, &a), Vec::<String>::new());
        let attrs: i64 = db.conn.query_row("SELECT COUNT(*) FROM attributes WHERE isDeleted=0 AND noteId=?", [&c], |r| r.get(0)).unwrap();
        assert_eq!(attrs, 0);
        assert!(db.delete_branch("root_root").is_err());
        assert_hashes_ok(&db);
    }

    #[test]
    fn reorder_and_move_between_parents() {
        let (_d, db) = fixture();
        let a = db.create_note("root", "A", "text", "text/html", "").unwrap();
        let b = db.create_note("root", "B", "text", "text/html", "").unwrap();
        let c = db.create_note("root", "C", "text", "text/html", "").unwrap();
        assert_eq!(children(&db, "root"), ["A", "B", "C"]);
        assert!(db.move_branch_up(&format!("root_{c}")).unwrap());
        assert_eq!(children(&db, "root"), ["A", "C", "B"]);
        assert!(db.move_branch_down(&format!("root_{a}")).unwrap());
        assert_eq!(children(&db, "root"), ["C", "A", "B"]);
        assert!(!db.move_branch_up(&format!("root_{c}")).unwrap(), "already first");
        // into previous sibling, and back out
        assert!(db.move_branch_in(&format!("root_{b}")).unwrap());
        assert_eq!(children(&db, "root"), ["C", "A"]);
        assert_eq!(children(&db, &a), ["B"]);
        assert!(db.move_branch_out(&format!("{a}_{b}")).unwrap());
        assert_eq!(children(&db, "root"), ["C", "A", "B"]);
        // cycles are refused
        let inner = db.create_note(&a, "Inner", "text", "text/html", "").unwrap();
        assert!(db.move_branch_before(&format!("root_{a}"), &format!("{a}_{inner}")).is_err());
        let reorder: i64 = db.conn.query_row("SELECT COUNT(*) FROM entity_changes WHERE entityName='note_reordering'", [], |r| r.get(0)).unwrap();
        assert!(reorder >= 1);
        assert_hashes_ok(&db);
    }

    #[test]
    fn content_edit_maintains_links_and_revisions() {
        let (_d, db) = fixture();
        db.conn.execute("INSERT INTO options(name,value,isSynced,utcDateModified) VALUES('revisionSnapshotTimeInterval','0',1,'2024-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("options", "name", "revisionSnapshotTimeInterval", "utcDateModified").unwrap();
        let target = db.create_note("root", "Target", "text", "text/html", "t").unwrap();
        let n = db.create_note("root", "N", "text", "text/html", "v1").unwrap();
        db.set_note_content(&n, &format!(r##"<p><a class="reference-link" href="http://srv:8080/#root/root/{target}">T</a></p>"##)).unwrap();
        let rels = |db: &Db| -> Vec<(String, String)> {
            let mut st = db.conn.prepare("SELECT name,value FROM attributes WHERE noteId=? AND isDeleted=0 AND type='relation'").unwrap();
            st.query_map([&n], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(rels(&db), vec![("internalLink".to_string(), target.clone())]);
        assert_eq!(db.note_content(&n).unwrap().unwrap(), format!(r##"<p><a class="reference-link" href="#root/root/{target}">T</a></p>"##), "absolute host stripped");
        db.set_note_content(&n, "<p>no links now</p>").unwrap();
        assert!(rels(&db).is_empty(), "stale link relation is deleted");
        let revs: i64 = db.conn.query_row("SELECT COUNT(*) FROM revisions WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        assert_eq!(revs, 2, "each edit past the interval snapshots the previous state");
        // the first revision holds the very first content
        let first: String = db.conn.query_row(
            "SELECT b.content FROM revisions r JOIN blobs b ON b.blobId=r.blobId WHERE r.noteId=? ORDER BY r.utcDateCreated, r.rowid LIMIT 1", [&n], |r| r.get(0)).unwrap();
        assert_eq!(first, "v1");
        // unchanged content is a no-op
        let before: i64 = db.conn.query_row("SELECT COUNT(*) FROM entity_changes", [], |r| r.get(0)).unwrap();
        db.set_note_content(&n, "<p>no links now</p>").unwrap();
        let after: i64 = db.conn.query_row("SELECT COUNT(*) FROM entity_changes", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
        assert_hashes_ok(&db);
    }

    #[test]
    fn prefix_title_and_create_after() {
        let (_d, db) = fixture();
        let a = db.create_note("root", "A", "text", "text/html", "").unwrap();
        let _c = db.create_note("root", "C", "text", "text/html", "").unwrap();
        db.create_note_after(&format!("root_{a}"), "B", "text", "text/html", "").unwrap();
        assert_eq!(children(&db, "root"), ["A", "B", "C"]);
        db.set_note_title(&a, "A renamed").unwrap();
        db.set_branch_prefix(&format!("root_{a}"), "  pre ").unwrap();
        let m = Model::load(&db).unwrap();
        assert_eq!(m.display_title(&m.children["root"][0]), "pre - A renamed");
        assert_hashes_ok(&db);
    }

    #[test]
    fn clone_move_and_duplicate() {
        let (_d, db) = fixture();
        let a = db.create_note("root", "A", "text", "text/html", "").unwrap();
        let b = db.create_note("root", "B", "text", "text/html", "").unwrap();
        let x = db.create_note(&a, "X", "text", "text/html", "x").unwrap();
        // clone X under B: same note, second placement
        db.clone_note(&x, &b, Some("pre")).unwrap();
        assert_eq!(children(&db, &b), ["X"]);
        assert_eq!(children(&db, &a), ["X"]);
        assert!(db.clone_note(&x, &b, None).is_err(), "already there");
        assert!(db.clone_note(&a, &x, None).is_err(), "cycle");
        assert!(db.clone_note(&x, "root", None).is_ok());
        // deleting one placement keeps the note
        db.delete_branch(&format!("{b}_{x}")).unwrap();
        let alive: i64 = db.conn.query_row("SELECT isDeleted FROM notes WHERE noteId=?", [&x], |r| r.get(0)).unwrap();
        assert_eq!(alive, 0);
        // move A's child to B (end position)
        db.move_branch_to(&format!("{a}_{x}"), &b).unwrap();
        assert_eq!(children(&db, &a), Vec::<String>::new());
        assert_eq!(children(&db, &b), ["X"]);

        // deep duplicate with internal link and relation remapping
        let p = db.create_note("root", "Proj", "text", "text/html", "").unwrap();
        let c1 = db.create_note(&p, "C1", "text", "text/html", "").unwrap();
        let c2 = db.create_note(&p, "C2", "text", "text/html", &format!(r##"<a href="#root/{p}/{c1}">to c1</a>"##)).unwrap();
        db.add_attribute(&c2, "relation", "friend", &c1, false).unwrap();
        db.add_attribute(&c2, "relation", "outside", &a, false).unwrap();
        db.add_attribute(&c2, "label", "k", "v", true).unwrap();
        let dup = db.duplicate_subtree(&p, "root", true).unwrap();
        assert_eq!(children(&db, "root").iter().filter(|t| t.starts_with("Proj")).cloned().collect::<Vec<_>>(), ["Proj", "Proj (dup)"], "the copy sits right after the original");
        let m = Model::load(&db).unwrap();
        let kids: Vec<_> = m.children[&dup].iter().map(|b| b.note_id.clone()).collect();
        assert_eq!(kids.len(), 2);
        let (n1, n2) = (&kids[0], &kids[1]);
        assert_ne!(n1, &c1);
        assert_eq!(db.note_content(n2).unwrap().unwrap(), format!(r##"<a href="#root/{dup}/{n1}">to c1</a>"##), "links inside the tree point at the copies");
        let rel: Vec<(String, String)> = db.attribute_rows(n2).unwrap().into_iter().filter(|a| a.kind == "relation" && a.owner == *n2 && a.name != "internalLink").map(|a| (a.name, a.value)).collect();
        assert!(rel.contains(&("friend".to_string(), n1.clone())) && rel.contains(&("outside".to_string(), a.clone())), "{rel:?}");
        assert!(db.attribute_rows(n2).unwrap().iter().any(|a| a.name == "k" && a.value == "v" && a.inheritable));
        // the original is untouched
        assert_eq!(db.note_content(&c2).unwrap().unwrap(), format!(r##"<a href="#root/{p}/{c1}">to c1</a>"##));
        assert!(db.duplicate_subtree("root", "root", true).is_err());
        assert_hashes_ok(&db);
    }

    #[test]
    fn duplicating_into_own_subtree_is_refused_not_a_crash() {
        let (_d, db) = fixture();
        let a = db.create_note("root", "A", "text", "text/html", "").unwrap();
        let b = db.create_note(&a, "B", "text", "text/html", "").unwrap();
        assert!(db.duplicate_subtree(&a, &b, true).is_err());
        assert!(db.duplicate_subtree(&a, &a, false).is_err());
        assert_eq!(children(&db, &a), ["B"], "nothing was created");
        assert_hashes_ok(&db);
    }
}
