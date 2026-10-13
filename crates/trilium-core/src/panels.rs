//! Read/modify operations behind the note panels: backlinks, revisions, attributes, attachments, info.
use crate::{db::Db, write::*};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};

#[derive(Debug, Clone)]
pub struct Backlink { pub source: String, pub relation: String, pub is_link: bool }

#[derive(Debug, Clone)]
pub struct RevInfo {
    pub id: String, pub title: String, pub source: String, pub description: String,
    pub utc_created: String, pub utc_last_edited: String, pub size: i64, pub protected: bool,
}

#[derive(Debug, Clone)]
pub struct AttrRow { pub id: String, pub kind: String, pub name: String, pub value: String, pub inheritable: bool, pub owner: String }

#[derive(Debug, Clone)]
pub struct AttachmentInfo { pub id: String, pub role: String, pub mime: String, pub title: String, pub size: i64 }

#[derive(Debug, Clone, Default)]
pub struct NoteInfo {
    pub id: String, pub ntype: String, pub mime: String, pub protected: bool,
    pub date_created: String, pub date_modified: String, pub utc_created: String, pub utc_modified: String,
    pub blob_id: String, pub content_size: i64, pub revisions: i64, pub attachments: i64, pub children: i64, pub parents: Vec<String>,
}

const LINK_RELATIONS: &[&str] = &["internalLink", "imageLink", "includeNoteLink", "relationMapLink"];

impl Db {
    /// Relations pointing at this note (links from content and user relations).
    pub fn backlinks(&self, note_id: &str) -> Result<Vec<Backlink>> {
        let mut st = self.conn.prepare(
            "SELECT a.noteId, a.name FROM attributes a JOIN notes n ON n.noteId=a.noteId
             WHERE a.type='relation' AND a.value=? AND a.isDeleted=0 AND n.isDeleted=0 AND a.noteId != ? ORDER BY n.title")?;
        let rows = st.query_map(params![note_id, note_id], |r| {
            let name: String = r.get(1)?;
            Ok(Backlink { source: r.get(0)?, is_link: LINK_RELATIONS.contains(&name.as_str()), relation: name })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revisions(&self, note_id: &str) -> Result<Vec<RevInfo>> {
        let mut st = self.conn.prepare(
            "SELECT r.revisionId, r.title, r.source, r.description, r.utcDateCreated, r.utcDateLastEdited, COALESCE(LENGTH(b.content),0), r.isProtected
             FROM revisions r LEFT JOIN blobs b ON b.blobId=r.blobId WHERE r.noteId=? ORDER BY r.utcDateCreated DESC, r.rowid DESC")?;
        let rows = st.query_map([note_id], |r| Ok(RevInfo {
            id: r.get(0)?, title: r.get(1)?, source: r.get(2)?, description: r.get(3)?, utc_created: r.get(4)?,
            utc_last_edited: r.get(5)?, size: r.get(6)?, protected: r.get::<_, i64>(7)? != 0,
        }))?;
        let mut out: Vec<RevInfo> = rows.collect::<rusqlite::Result<_>>()?;
        for r in out.iter_mut().filter(|r| r.protected) { r.title = self.decrypt_title(&r.title); }
        Ok(out)
    }

    pub fn revision_content(&self, rev_id: &str) -> Result<Option<String>> {
        let v: Option<(rusqlite::types::Value, i64)> = self.conn.query_row(
            "SELECT b.content, r.isProtected FROM revisions r JOIN blobs b ON b.blobId=r.blobId WHERE r.revisionId=?", [rev_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        match v {
            None => Ok(None),
            Some((v, p)) => Ok(Some(String::from_utf8_lossy(&self.plain_bytes(p != 0, v)?).into_owned())),
        }
    }

    /// `restoreRevision`: snapshot the current state (source "restore"), replace the note's attachments with the
    /// revision's, then take title, type, mime and content from the revision.
    pub fn restore_revision(&self, rev_id: &str) -> Result<()> {
        let (note_id, rtype, rmime, rtitle, rprotected, rblob): (String, String, String, String, i64, Option<String>) = self.conn.query_row(
            "SELECT noteId,type,mime,title,isProtected,blobId FROM revisions WHERE revisionId=?", [rev_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?.context("revision not found")?;
        let note_protected = self.note_protected(&note_id)?;
        if (rprotected != 0) != note_protected { bail!("this revision was taken while the note was {}; protect or unprotect the note to match first", if rprotected != 0 { "protected" } else { "not protected" }); }
        if note_protected { self.key()?; }
        if rtype == "file" || rtype == "image" { bail!("restoring {rtype} revisions isn't supported (binary content would be corrupted)"); }
        let tx = self.begin()?;
        self.save_revision(&note_id, "", "restore")?;
        let old: Vec<String> = {
            let mut st = self.conn.prepare("SELECT attachmentId FROM attachments WHERE ownerId=? AND isDeleted=0")?;
            st.query_map([&note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for a in old { self.mark_deleted("attachments", "attachmentId", &a, None)?; }
        let mut content = self.revision_content(rev_id)?.unwrap_or_default();
        let ratts: Vec<(String, String, String, String, i64, Option<String>, bool)> = {
            let mut st = self.conn.prepare("SELECT attachmentId,role,mime,title,position,blobId,isProtected FROM attachments WHERE ownerId=? AND isDeleted=0")?;
            st.query_map([rev_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, i64>(6)? != 0)))?.collect::<rusqlite::Result<_>>()?
        };
        let prot = note_protected as i64;
        for (old_id, role, amime, atitle, pos, ablob, was_protected) in ratts {
            // an attachment saved while the note was in the other state is converted, not copied as it is
            let (atitle, ablob) = self.attachment_in_state(&atitle, ablob, &amime, was_protected, note_protected)?;
            let new_id = new_entity_id();
            self.conn.execute(
                "INSERT INTO attachments(attachmentId,ownerId,role,mime,title,isProtected,position,blobId,dateModified,utcDateModified,isDeleted) VALUES(?,?,?,?,?,?,?,?,?,?,0)",
                params![new_id, note_id, role, amime, atitle, prot, pos, ablob, now_local(), now_utc()])?;
            self.record_entity("attachments", "attachmentId", &new_id, "utcDateModified")?;
            content = content.replace(&format!("attachments/{old_id}"), &format!("attachments/{new_id}"));
        }
        // the restored text has other links than the one it replaces: keep the link relations (and so backlinks) in step
        let md = rtype == "code" && matches!(rmime.as_str(), "text/markdown" | "text/x-markdown" | "text/x-gfm");
        if rtype == "text" || md { content = self.save_links(&note_id, &content, md)?; }
        let new_blob = self.save_blob_for(note_protected, content.as_bytes(), is_string_note(&rtype, &rmime))?;
        let old_blob: Option<String> = self.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [&note_id], |r| r.get(0))?;
        self.conn.execute("UPDATE notes SET title=?, type=?, mime=?, blobId=?, dateModified=?, utcDateModified=? WHERE noteId=?",
            params![rtitle, rtype, rmime, new_blob, now_local(), now_utc(), note_id])?;
        self.record_entity("notes", "noteId", &note_id, "utcDateModified")?;
        if let Some(o) = old_blob.filter(|o| *o != new_blob) { self.drop_blob_if_unused(&o)?; }
        let _ = rblob;
        tx.commit()
    }

    /// Own attributes plus inherited ones (owner differs from the note).
    pub fn attribute_rows(&self, note_id: &str) -> Result<Vec<AttrRow>> {
        let own: Vec<AttrRow> = {
            let mut st = self.conn.prepare("SELECT attributeId,type,name,value,isInheritable FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
            st.query_map([note_id], |r| Ok(AttrRow { id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, value: r.get(3)?, inheritable: r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0, owner: note_id.into() }))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = own;
        // inherited: inheritable attributes of ancestors
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<String> = {
            let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            st.query_map([note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) || p == "root" { continue; }
            let mut st = self.conn.prepare("SELECT attributeId,type,name,value FROM attributes WHERE noteId=? AND isDeleted=0 AND isInheritable=1 ORDER BY position")?;
            for r in st.query_map([&p], |r| Ok(AttrRow { id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, value: r.get(3)?, inheritable: true, owner: p.clone() }))? { out.push(r?); }
            let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            stack.extend(st.query_map([&p], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn delete_attribute(&self, attr_id: &str) -> Result<()> {
        let tx = self.begin()?;
        self.mark_deleted("attributes", "attributeId", attr_id, None)?;
        tx.commit()
    }

    pub fn set_attribute_value(&self, attr_id: &str, value: &str) -> Result<()> {
        let tx = self.begin()?;
        let n = self.conn.execute("UPDATE attributes SET value=?, utcDateModified=? WHERE attributeId=? AND isDeleted=0", params![value, now_utc(), attr_id])?;
        if n == 0 { bail!("attribute not found"); }
        self.record_entity("attributes", "attributeId", attr_id, "utcDateModified")?;
        tx.commit()
    }

    /// Create or update an option (e.g. `revisionSnapshotTimeInterval`), recording the change when it is synced.
    pub fn set_option(&self, name: &str, value: &str, synced: bool) -> Result<()> {
        let tx = self.begin()?;
        self.conn.execute(
            "INSERT INTO options(name,value,isSynced,utcDateModified) VALUES(?1,?2,?3,?4)
             ON CONFLICT(name) DO UPDATE SET value=?2, utcDateModified=?4", params![name, value, synced as i64, now_utc()])?;
        if synced { self.record_entity("options", "name", name, "utcDateModified")?; }
        tx.commit()
    }

    /// Attach bytes to a note; returns the attachment id.
    pub fn add_attachment(&self, note_id: &str, title: &str, mime: &str, role: &str, bytes: &[u8]) -> Result<String> {
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| r.get(0)).optional()?;
        if exists.is_none() { bail!("note {note_id} doesn't exist"); }
        let protected = self.note_protected(note_id)?;
        let stored_title = if protected { self.encrypt_title(title)? } else { title.to_string() };
        let tx = self.begin()?;
        let blob = self.save_blob_for(protected, bytes, is_string_note("", mime) && std::str::from_utf8(bytes).is_ok())?;
        let id = new_entity_id();
        let pos: i64 = self.conn.query_row("SELECT COALESCE(MAX(position),0) FROM attachments WHERE ownerId=?", [note_id], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO attachments(attachmentId,ownerId,role,mime,title,isProtected,position,blobId,dateModified,utcDateModified,isDeleted) VALUES(?,?,?,?,?,?,?,?,?,?,0)",
            params![id, note_id, role, mime, stored_title, protected as i64, pos + 10, blob, now_local(), now_utc()])?;
        self.record_entity("attachments", "attachmentId", &id, "utcDateModified")?;
        tx.commit()?;
        Ok(id)
    }

    /// A collection view's stored configuration (`viewConfig` attachment such as `board.json`), if any.
    pub fn view_config(&self, note_id: &str, title: &str) -> Result<Option<serde_json::Value>> {
        let v: Option<rusqlite::types::Value> = self.conn.query_row(
            "SELECT b.content FROM attachments a JOIN blobs b ON b.blobId=a.blobId WHERE a.ownerId=? AND a.role='viewConfig' AND a.title=? AND a.isDeleted=0 LIMIT 1",
            params![note_id, title], |r| r.get(0)).optional()?;
        Ok(v.and_then(|v| match v { rusqlite::types::Value::Text(s) => serde_json::from_str(&s).ok(), rusqlite::types::Value::Blob(b) => serde_json::from_slice(&b).ok(), _ => None }))
    }

    /// Set (or with `None` remove) an own label: updates the existing one, adds it, or deletes it.
    pub fn set_label_value(&self, note_id: &str, name: &str, value: Option<&str>) -> Result<()> {
        let existing: Option<String> = self.conn.query_row(
            "SELECT attributeId FROM attributes WHERE noteId=? AND type='label' AND name=? AND isDeleted=0 ORDER BY position LIMIT 1", params![note_id, name], |r| r.get(0)).optional()?;
        match (existing, value) {
            (Some(id), Some(v)) => self.set_attribute_value(&id, v),
            (Some(id), None) => self.delete_attribute(&id),
            (None, Some(v)) => self.add_attribute(note_id, "label", name, v, false).map(|_| ()),
            (None, None) => Ok(()),
        }
    }

    pub fn attachments(&self, note_id: &str) -> Result<Vec<AttachmentInfo>> {
        let mut st = self.conn.prepare(
            "SELECT a.attachmentId,a.role,a.mime,a.title,COALESCE(LENGTH(b.content),0),a.isProtected FROM attachments a LEFT JOIN blobs b ON b.blobId=a.blobId
             WHERE a.ownerId=? AND a.isDeleted=0 ORDER BY a.position, a.title")?;
        let rows = st.query_map([note_id], |r| Ok((AttachmentInfo { id: r.get(0)?, role: r.get(1)?, mime: r.get(2)?, title: r.get(3)?, size: r.get(4)? }, r.get::<_, i64>(5)? != 0)))?;
        let rows: Vec<(AttachmentInfo, bool)> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().map(|(mut a, prot)| { if prot { a.title = self.decrypt_title(&a.title); } a }).collect())
    }

    pub fn attachment_bytes(&self, attachment_id: &str) -> Result<Option<Vec<u8>>> {
        let v: Option<(rusqlite::types::Value, i64)> = self.conn.query_row(
            "SELECT b.content, a.isProtected FROM attachments a JOIN blobs b ON b.blobId=a.blobId WHERE a.attachmentId=?", [attachment_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        v.map(|(v, p)| self.plain_bytes(p != 0, v)).transpose()
    }

    /// Does an `<img src>` / attachment id point at protected material? Exports must not carry that out in the clear.
    pub fn source_is_protected(&self, src: &str) -> bool {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"api/(attachments|images)/([A-Za-z0-9_]+)").expect("static regex"));
        let Some(c) = re.captures(src) else { return false };
        let table = if &c[1] == "attachments" { "SELECT isProtected FROM attachments WHERE attachmentId=?" } else { "SELECT isProtected FROM notes WHERE noteId=?" };
        self.conn.query_row(table, [&c[2]], |r| r.get::<_, i64>(0)).optional().ok().flatten().unwrap_or(0) != 0
    }

    pub fn attachment_protected(&self, attachment_id: &str) -> bool {
        self.conn.query_row("SELECT isProtected FROM attachments WHERE attachmentId=?", [attachment_id], |r| r.get::<_, i64>(0)).optional().ok().flatten().unwrap_or(0) != 0
    }

    /// Content of a note whose type is image or file (raw bytes).
    pub fn note_bytes(&self, note_id: &str) -> Result<Option<Vec<u8>>> {
        let v: Option<(rusqlite::types::Value, i64)> = self.conn.query_row(
            "SELECT b.content, n.isProtected FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId=?", [note_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        v.map(|(v, p)| self.plain_bytes(p != 0, v)).transpose()
    }

    /// Bytes behind an `<img src>` of a note: `api/images/<noteId>/<name>`, `api/attachments/<id>/image/<name>`
    /// (or `/download`), or a `data:` URL. Returns (file name, mime, bytes).
    pub fn image_source_bytes(&self, src: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        use base64::Engine as _;
        let src = src.trim();
        if let Some(rest) = src.strip_prefix("data:") {
            let Some((meta, data)) = rest.split_once(',') else { return Ok(None) };
            let mime = meta.split(';').next().unwrap_or("application/octet-stream").to_string();
            if !meta.contains("base64") { return Ok(None); }
            let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok();
            return Ok(bytes.map(|b| ("image".to_string(), mime, b)));
        }
        let path = src.split(['?', '#']).next().unwrap_or("");
        let segs: Vec<&str> = path.split('/').collect();
        let find = |key: &str| segs.iter().position(|s| *s == key);
        let decode_name = |s: &str| -> String {
            let mut out = Vec::new();
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    if let Some(v) = crate::transfer::hex_pair(b[i + 1], b[i + 2]) { out.push(v); i += 3; continue; }
                }
                out.push(b[i]); i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        };
        if let (Some(i), true) = (find("images"), segs.len() > 2) {
            if i + 1 >= segs.len() { return Ok(None); }
            if segs.get(i.wrapping_sub(1)) == Some(&"api") {
                let id = segs[i + 1];
                let name = segs.get(i + 2).map(|n| decode_name(n)).unwrap_or_else(|| "image".into());
                let mime: Option<String> = self.conn.query_row("SELECT mime FROM notes WHERE noteId=? AND isDeleted=0", [id], |r| r.get(0)).optional()?;
                if let (Some(mime), Some(bytes)) = (mime, self.note_bytes(id)?) { return Ok(Some((name, mime, bytes))); }
                return Ok(None);
            }
        }
        if let Some(i) = find("attachments") {
            if segs.get(i.wrapping_sub(1)) == Some(&"api") && segs.len() > i + 1 {
                let id = segs[i + 1];
                let name = segs.last().map(|n| decode_name(n)).filter(|n| n != "download" && !n.is_empty() && n != id).unwrap_or_else(|| "attachment".into());
                let row: Option<(String, String)> = self.conn.query_row("SELECT mime,title FROM attachments WHERE attachmentId=? AND isDeleted=0", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if let (Some((mime, title)), Some(bytes)) = (row, self.attachment_bytes(id)?) { return Ok(Some((if name == "attachment" { title } else { name }, mime, bytes))); }
            }
        }
        Ok(None)
    }

    pub fn note_info(&self, note_id: &str) -> Result<NoteInfo> {
        let mut i: NoteInfo = self.conn.query_row(
            "SELECT noteId,type,mime,isProtected,dateCreated,dateModified,utcDateCreated,utcDateModified,COALESCE(blobId,'') FROM notes WHERE noteId=?", [note_id],
            |r| Ok(NoteInfo { id: r.get(0)?, ntype: r.get(1)?, mime: r.get(2)?, protected: r.get::<_, i64>(3)? != 0, date_created: r.get(4)?, date_modified: r.get(5)?,
                utc_created: r.get(6)?, utc_modified: r.get(7)?, blob_id: r.get(8)?, ..Default::default() })).optional()?.context("note not found")?;
        i.content_size = self.conn.query_row("SELECT COALESCE(LENGTH(content),0) FROM blobs WHERE blobId=?", [&i.blob_id], |r| r.get(0)).unwrap_or(0);
        i.revisions = self.conn.query_row("SELECT COUNT(*) FROM revisions WHERE noteId=?", [note_id], |r| r.get(0))?;
        i.attachments = self.conn.query_row("SELECT COUNT(*) FROM attachments WHERE ownerId=? AND isDeleted=0", [note_id], |r| r.get(0))?;
        i.children = self.conn.query_row("SELECT COUNT(*) FROM branches WHERE parentNoteId=? AND isDeleted=0", [note_id], |r| r.get(0))?;
        let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
        i.parents = st.query_map([note_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_image_sources_do_not_panic() {
        let (_d, db) = fixture();
        for src in ["foo/api/images", "api/images", "api/images/abc/100%日x.png", "api/attachments/x%日/download", "%", "%日", "api/images/%zz/%4"] {
            assert!(db.image_source_bytes(src).is_ok(), "{src}");
        }
        assert_eq!(crate::transfer::hex_pair(b'4', b'1'), Some(0x41));
        assert_eq!(crate::transfer::hex_pair(0xe6, b'1'), None);
    }

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("p.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        db.set_option("revisionSnapshotTimeInterval", "0", true).unwrap();
        (dir, db)
    }

    fn assert_hashes(db: &Db) { for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); } }

    #[test]
    fn backlinks_list_sources_and_kinds() {
        let (_d, db) = fixture();
        let target = db.create_note("root", "Target", "text", "text/html", "t").unwrap();
        let linker = db.create_note("root", "Linker", "text", "text/html", &format!(r##"<a href="#root/{target}">x</a>"##)).unwrap();
        db.set_note_content(&linker, &format!(r##"<a href="#root/{target}">x</a> more"##)).unwrap();
        let other = db.create_note("root", "Other", "text", "text/html", "").unwrap();
        db.add_attribute(&other, "relation", "friend", &target, false).unwrap();
        let b = db.backlinks(&target).unwrap();
        assert_eq!(b.len(), 2);
        assert!(b.iter().any(|x| x.source == linker && x.is_link && x.relation == "internalLink"));
        assert!(b.iter().any(|x| x.source == other && !x.is_link && x.relation == "friend"));
    }

    #[test]
    fn revisions_list_diff_source_and_restore() {
        let (_d, db) = fixture();
        let n = db.create_note("root", "First title", "text", "text/html", "<p>v1</p>").unwrap();
        // with the interval at 0 two edits in the same millisecond would share one snapshot, as upstream
        let pause = || std::thread::sleep(std::time::Duration::from_millis(3));
        pause();
        db.set_note_content(&n, "<p>v2</p>").unwrap();
        pause();
        db.set_note_title(&n, "Second title").unwrap();
        pause();
        db.set_note_content(&n, "<p>v3</p>").unwrap();
        let revs = db.revisions(&n).unwrap();
        assert_eq!(revs.len(), 3, "each edit snapshots the state before it");
        let oldest = revs.last().unwrap();
        assert_eq!((oldest.title.as_str(), db.revision_content(&oldest.id).unwrap().as_deref()), ("First title", Some("<p>v1</p>")));
        db.restore_revision(&oldest.id).unwrap();
        assert_eq!(db.note_content(&n).unwrap().as_deref(), Some("<p>v1</p>"));
        let info = db.note_info(&n).unwrap();
        assert_eq!(info.revisions, 4, "restore saves a snapshot of what it replaced");
        let title: String = db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        assert_eq!(title, "First title");
        assert!(db.revisions(&n).unwrap().iter().any(|r| r.source == "restore"));
        assert_hashes(&db);
    }

    #[test]
    fn attributes_inherited_edit_delete() {
        let (_d, db) = fixture();
        let parent = db.create_note("root", "P", "text", "text/html", "").unwrap();
        db.add_attribute(&parent, "label", "color", "red", true).unwrap();
        let child = db.create_note(&parent, "C", "text", "text/html", "").unwrap();
        let own = db.add_attribute(&child, "label", "mine", "1", false).unwrap();
        let rows = db.attribute_rows(&child).unwrap();
        assert_eq!(rows.iter().map(|r| (r.name.as_str(), r.owner == child)).collect::<Vec<_>>(), [("mine", true), ("color", false)]);
        db.set_attribute_value(&own, "2").unwrap();
        assert_eq!(db.attribute_rows(&child).unwrap()[0].value, "2");
        db.delete_attribute(&own).unwrap();
        assert_eq!(db.attribute_rows(&child).unwrap().len(), 1);
        assert_hashes(&db);
    }

    #[test]
    fn attachments_and_info() {
        let (_d, db) = fixture();
        let n = db.create_note("root", "N", "text", "text/html", "<p>hello</p>").unwrap();
        let att = db.add_attachment(&n, "pic.png", "image/png", "file", b"\x89PNG\r\n\x1a\nxyz").unwrap();
        let a = db.attachments(&n).unwrap();
        assert_eq!((a[0].title.as_str(), a[0].size), ("pic.png", 11));
        assert_eq!(db.attachment_bytes(&att).unwrap().unwrap(), b"\x89PNG\r\n\x1a\nxyz");
        // <img src> resolution
        let (name, mime, bytes) = db.image_source_bytes(&format!("api/attachments/{att}/image/pic%20one.png")).unwrap().unwrap();
        assert_eq!((name.as_str(), mime.as_str(), bytes.len()), ("pic one.png", "image/png", 11));
        assert!(db.image_source_bytes(&format!("http://srv:8080/api/attachments/{att}/download")).unwrap().is_some());
        let img = db.create_note("root", "an image", "image", "image/jpeg", "").unwrap();
        assert!(db.image_source_bytes(&format!("/api/images/{img}/x.jpg")).unwrap().is_some());
        let (_, m, b) = db.image_source_bytes("data:image/png;base64,aGk=").unwrap().unwrap();
        assert_eq!((m.as_str(), b.as_slice()), ("image/png", &b"hi"[..]));
        assert!(db.image_source_bytes("https://example.com/a.png").unwrap().is_none());
        assert_hashes(&db);
        let i = db.note_info(&n).unwrap();
        assert_eq!((i.attachments, i.children, i.parents.len(), i.content_size), (1, 0, 1, 12));
    }

    #[test]
    fn view_config_labels_and_calendar_helpers() {
        let (_d, db) = fixture();
        let board = db.create_note("root", "Board", "book", "text/html", "").unwrap();
        let cfg = br#"{"columns":[{"value":"Todo"},{"value":"Done"}]}"#;
        db.add_attachment(&board, "board.json", "application/json", "viewConfig", cfg).unwrap();
        let v = db.view_config(&board, "board.json").unwrap().unwrap();
        assert_eq!(v["columns"][1]["value"], "Done");
        assert!(db.view_config(&board, "table.json").unwrap().is_none());
        let card = db.create_note(&board, "Card", "text", "text/html", "").unwrap();
        db.set_label_value(&card, "status", Some("Todo")).unwrap();
        db.set_label_value(&card, "status", Some("Done")).unwrap();
        assert_eq!(db.label_value(&card, "status").unwrap().as_deref(), Some("Done"));
        db.set_label_value(&card, "status", None).unwrap();
        assert!(db.label_value(&card, "status").unwrap().is_none());
        let today = db.today_note().unwrap();
        let d = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(db.existing_day_note(&d).unwrap().as_deref(), Some(today.as_str()));
        assert_eq!(db.existing_day_note("1999-01-01").unwrap(), None);
        assert_eq!(db.day_notes_in_month(&d[..7]).unwrap(), vec![(d, today)]);
        assert_hashes(&db);
    }
}
