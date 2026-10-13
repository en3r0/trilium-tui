//! Local write operations. Each one mirrors what the official client does on save:
//! write the row, then record an `entity_changes` row whose hash the server and every other
//! client will verify (see hash.rs).
use crate::{db::Db, hash};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rand::RngExt;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha512};

pub fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3fZ").to_string()
}

pub fn now_local() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f%z").to_string()
}

/// Same alphabet and length as Trilium's `newEntityId()` (randtoken, 12 alphanumerics).
pub fn new_entity_id() -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut r = rand::rng();
    (0..12).map(|_| A[r.random_range(0..A.len())] as char).collect()
}

/// `hashedBlobId`: first 20 chars of base64(sha512(content)) with `+`->`X`, `/`->`Y`.
pub fn hashed_blob_id(content: &[u8]) -> String {
    B64.encode(Sha512::digest(content)).replace('+', "X").replace('/', "Y")[..20].to_string()
}

/// `isStringNote` (utils/index.ts): decides TEXT vs BLOB storage for content.
pub fn is_string_note(ntype: &str, mime: &str) -> bool {
    const TYPES: &[&str] = &["text", "code", "relationMap", "search", "render", "book", "mermaid", "canvas", "webView"];
    const MIMES: &[&str] = &["application/javascript", "application/x-javascript", "application/json", "application/x-sql",
        "image/svg+xml", "application/inkml+xml"];
    TYPES.contains(&ntype) || mime.starts_with("text/") || MIMES.contains(&mime)
}

impl Db {
    /// Create the `root` note (with its change record) in an empty database, e.g. for local-only use. No-op if present.
    pub fn ensure_root(&self) -> Result<()> {
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId='root'", [], |r| r.get(0)).optional()?;
        if exists.is_some() { return Ok(()); }
        let tx = self.begin()?;
        let (local, utc) = (now_local(), now_utc());
        self.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified) VALUES('root','root',0,'text','text/html',NULL,0,?,?,?,?)", params![local, local, utc, utc])?;
        self.record_entity("notes", "noteId", "root", "utcDateModified")?;
        tx.commit()
    }

    pub fn instance_id(&self) -> Result<String> {
        self.state("instanceId")?.context("database has no instanceId (run sync first)")
    }

    /// Full row as JSON, the shape used on the wire.
    pub fn row_json(&self, table: &str, pk: &str, id: &str) -> Result<Option<Value>> {
        let mut stmt = self.conn.prepare(&format!("SELECT * FROM {table} WHERE {pk}=?"))?;
        let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let row = stmt.query_row([id], |r| {
            let mut m = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                m.insert(c.clone(), match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                    rusqlite::types::ValueRef::Real(f) => Value::from(f),
                    // wire format: blob content is ALWAYS base64, even when we store it as TEXT
                    rusqlite::types::ValueRef::Text(t) if table == "blobs" && c == "content" => Value::from(B64.encode(t)),
                    rusqlite::types::ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                    rusqlite::types::ValueRef::Blob(b) => Value::from(B64.encode(b)), // wire format for blob content
                });
            }
            Ok(Value::Object(m))
        }).optional()?;
        Ok(row)
    }

    pub(crate) fn put_entity_change(&self, name: &str, id: &str, hash: &str, date_changed: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO entity_changes(entityName,entityId,hash,isErased,changeId,componentId,instanceId,isSynced,utcDateChanged)
             VALUES(?,?,?,0,?,'tui',?,1,?)",
            params![name, id, hash, new_entity_id(), self.instance_id()?, date_changed],
        )?;
        Ok(())
    }

    /// Store content as a blob (deduplicated by content hash), returning its blobId.
    pub(crate) fn save_blob(&self, content: &[u8], as_text: bool) -> Result<String> {
        let blob_id = hashed_blob_id(content);
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM blobs WHERE blobId=?", [&blob_id], |r| r.get(0)).optional()?;
        if exists.is_some() { self.republish_if_orphan(&blob_id)?; }
        if exists.is_none() {
            let (local, utc) = (now_local(), now_utc());
            if as_text {
                self.conn.execute("INSERT INTO blobs(blobId,content,dateModified,utcDateModified) VALUES(?,?,?,?)",
                    params![blob_id, String::from_utf8(content.to_vec())?, local, utc])?;
            } else {
                self.conn.execute("INSERT INTO blobs(blobId,content,dateModified,utcDateModified) VALUES(?,?,?,?)",
                    params![blob_id, content, local, utc])?;
            }
            self.put_entity_change("blobs", &blob_id, &hash::blob_hash(&blob_id, content, None), &utc)?;
        }
        Ok(blob_id)
    }

    /// A blob that nothing refers to may already have been dropped by the server (as unused); handing it out again means
    /// announcing it again, or the next note that uses it would point at content the server no longer has.
    pub(crate) fn republish_if_orphan(&self, blob_id: &str) -> Result<()> {
        for t in ["notes", "attachments", "revisions"] {
            let used: Option<i64> = self.conn.query_row(&format!("SELECT 1 FROM {t} WHERE blobId=? LIMIT 1"), [blob_id], |r| r.get(0)).optional()?;
            if used.is_some() { return Ok(()); }
        }
        let v: Option<rusqlite::types::Value> = self.conn.query_row("SELECT content FROM blobs WHERE blobId=?", [blob_id], |r| r.get(0)).optional()?;
        let Some(v) = v else { return Ok(()) };
        let bytes = match v { rusqlite::types::Value::Text(s) => s.into_bytes(), rusqlite::types::Value::Blob(b) => b, _ => vec![] };
        self.put_entity_change("blobs", blob_id, &hash::blob_hash(blob_id, &bytes, None), &now_utc())
    }

    pub(crate) fn record_entity(&self, table: &str, pk: &str, id: &str, date_field: &str) -> Result<()> {
        let row = self.row_json(table, pk, id)?.context("row vanished")?;
        let h = hash::entity_hash(table, &row, row["isDeleted"].as_i64() == Some(1), hash::Missing::Null)
            .context("no hash rule")?;
        self.put_entity_change(table, id, &h, row[date_field].as_str().context("date field")?)
    }

    /// Create a note under `parent_id` (appended after its last child), applying the same
    /// creation-time behaviour as the web app: `child:` attribute inheritance (which can attach a
    /// template) and re-sorting of `#sorted` parents. Returns the new noteId.
    pub fn create_note(&self, parent_id: &str, title: &str, ntype: &str, mime: &str, content: &str) -> Result<String> {
        let tx = self.begin()?;
        let note_id = self.insert_note_raw(parent_id, title, ntype, mime, content, None, None)?;
        self.copy_child_attributes_pub(parent_id, &note_id, ntype)?;
        self.sort_children_if_needed(parent_id)?;
        tx.commit()?;
        Ok(note_id)
    }

    /// Insert note + branch + change records only (no inheritance / sorting). `position` overrides the append position.
    pub(crate) fn insert_note_raw(&self, parent_id: &str, title: &str, ntype: &str, mime: &str, content: &str, prefix: Option<&str>, position: Option<i64>) -> Result<String> {
        self.insert_note_with_id(&new_entity_id(), parent_id, title, ntype, mime, content, prefix, position)
    }

    pub(crate) fn insert_note_with_id(&self, note_id: &str, parent_id: &str, title: &str, ntype: &str, mime: &str, content: &str, prefix: Option<&str>, position: Option<i64>) -> Result<String> {
        let parent_exists: Option<i64> = self.conn.query_row(
            "SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)).optional()?;
        if parent_exists.is_none() { bail!("parent note {parent_id} not found"); }
        let note_id = note_id.to_string();
        let (local, utc) = (now_local(), now_utc());
        // a child of a protected note is protected too (refused while locked: it would store the text in the clear)
        let protected = self.note_protected(parent_id)?;
        let blob_id = self.save_blob_for(protected, content.as_bytes(), is_string_note(ntype, mime))?;
        let stored_title = if protected { self.encrypt_title(title)? } else { title.to_string() };
        self.conn.execute(
            "INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
             VALUES(?,?,?,?,?,?,0,?,?,?,?)",
            params![note_id, stored_title, protected as i64, ntype, mime, blob_id, local, local, utc, utc])?;
        self.record_entity("notes", "noteId", &note_id, "utcDateModified")?;
        let pos = match position {
            Some(p) => p,
            None => self.conn.query_row(
                "SELECT COALESCE(MAX(notePosition),0) FROM branches WHERE parentNoteId=? AND isDeleted=0 AND noteId != '_hidden'",
                [parent_id], |r| r.get::<_, i64>(0))? + 10,
        };
        let branch_id = format!("{parent_id}_{note_id}");
        self.conn.execute(
            "INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified)
             VALUES(?,?,?,?,?,0,0,?)",
            params![branch_id, note_id, parent_id, pos, prefix.filter(|p| !p.trim().is_empty()), utc])?;
        self.record_entity("branches", "branchId", &branch_id, "utcDateModified")?;
        Ok(note_id)
    }

    /// Mirrors `deleteBlobIfNotUsed`: unreferenced blobs and their change rows are purged outright.
    pub(crate) fn drop_blob_if_unused(&self, blob_id: &str) -> Result<()> {
        for t in ["notes", "attachments", "revisions"] {
            let used: Option<i64> = self.conn.query_row(&format!("SELECT 1 FROM {t} WHERE blobId=? LIMIT 1"), [blob_id], |r| r.get(0)).optional()?;
            if used.is_some() { return Ok(()); }
        }
        self.conn.execute("DELETE FROM blobs WHERE blobId=?", [blob_id])?;
        self.conn.execute("DELETE FROM entity_changes WHERE entityName='blobs' AND entityId=?", [blob_id])?;
        // the text index must not outlive the content (it can hold the plaintext of a note that has just been protected)
        let _ = self.conn.execute("DELETE FROM tui_fts WHERE rowid IN (SELECT id FROM tui_fts_map WHERE blobId=?)", [blob_id]);
        let _ = self.conn.execute("DELETE FROM tui_fts_map WHERE blobId=?", [blob_id]);
        Ok(())
    }

    /// Delete blobs that no note, attachment or revision refers to, with their change rows and index entries
    /// (what `drop_blob_if_unused` does one blob at a time). Returns how many went.
    pub fn sweep_orphan_blobs(&self) -> Result<usize> {
        let orphans: Vec<String> = {
            let mut st = self.conn.prepare(
                "SELECT blobId FROM blobs WHERE blobId NOT IN (SELECT blobId FROM notes WHERE blobId IS NOT NULL)
                 AND blobId NOT IN (SELECT blobId FROM attachments WHERE blobId IS NOT NULL)
                 AND blobId NOT IN (SELECT blobId FROM revisions WHERE blobId IS NOT NULL)")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        if orphans.is_empty() { return Ok(0); }
        let tx = self.begin()?;
        for b in &orphans { self.drop_blob_if_unused(b)?; }
        tx.commit()?;
        let _ = self.conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        Ok(orphans.len())
    }

    /// Note content as text (lossy for non-UTF-8 blobs).
    pub fn note_content(&self, note_id: &str) -> Result<Option<String>> {
        let r: Option<(rusqlite::types::Value, i64)> = self.conn.query_row(
            "SELECT b.content, n.isProtected FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId=?", [note_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        match r {
            None => Ok(None),
            Some((v, p)) => Ok(Some(String::from_utf8_lossy(&self.plain_bytes(p != 0, v)?).into_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.set_state("instanceId", "testinstance").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    #[test]
    fn blob_content_is_base64_on_the_wire_even_when_stored_as_text() {
        let (_d, db) = db();
        let id = db.create_note("root", "t", "text", "text/html", "<p>héllo</p>").unwrap();
        let blob_id: String = db.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [&id], |r| r.get(0)).unwrap();
        let row = db.row_json("blobs", "blobId", &blob_id).unwrap().unwrap();
        assert_eq!(B64.decode(row["content"].as_str().unwrap()).unwrap(), "<p>héllo</p>".as_bytes());
    }

    #[test]
    fn created_rows_verify_and_edits_replace_blobs() {
        let (_d, db) = db();
        let id = db.create_note("root", "t", "text", "text/html", "a").unwrap();
        db.set_note_content(&id, "b").unwrap();
        db.set_note_title(&id, "t2").unwrap();
        assert_eq!(db.note_content(&id).unwrap().as_deref(), Some("b"));
        let blobs: i64 = db.conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0)).unwrap();
        assert_eq!(blobs, 1, "old blob must be purged");
        let ecs: i64 = db.conn.query_row("SELECT COUNT(*) FROM entity_changes WHERE entityName='blobs'", [], |r| r.get(0)).unwrap();
        assert_eq!(ecs, 1);
        for (name, (_ok, bad)) in db.recompute_hashes().unwrap() {
            assert_eq!(bad, 0, "{name} hashes must verify");
        }
    }
}
