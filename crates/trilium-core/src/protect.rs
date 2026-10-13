//! Protected notes: a password-derived data key unlocks encrypted titles and contents (Trilium's "protected session").
//! The key only ever lives in memory; `lock` wipes it.
use crate::{crypto, hash};
use crate::db::Db;
use crate::write::{hashed_blob_id, is_string_note, now_local, now_utc};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rusqlite::{OptionalExtension, params};

/// Error text for anything that needs the data key while the session is locked.
pub const LOCKED: &str = "protected notes are locked; unlock them with the protected-notes password first";
/// Shown instead of a protected title while locked.
pub const PLACEHOLDER: &str = "[protected]";
/// Protected blobs get their id from the plaintext under this prefix (so equal plaintext still dedups under encryption).
const BLOB_PREFIX: &str = "t$[nvQg7q)&_ENCRYPTED_?M:Bf&j3jr_";

pub fn is_locked_error(e: &anyhow::Error) -> bool { e.to_string() == LOCKED }

impl Db {
    fn option_str(&self, name: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM options WHERE name=?", [name], |r| r.get::<_, String>(0)).optional().ok().flatten()
    }

    /// Has a protected-notes password been set (here or on the synced server)?
    pub fn has_password(&self) -> bool {
        self.option_str("passwordVerificationHash").is_some_and(|h| !h.is_empty()) && self.option_str("encryptedDataKey").is_some_and(|h| !h.is_empty())
    }

    pub fn is_unlocked(&self) -> bool { self.data_key.borrow().is_some() }

    pub fn lock(&self) {
        if let Some(k) = self.data_key.borrow_mut().take() {
            let mut k = k;
            k.iter_mut().for_each(|b| *b = 0);
        }
    }

    /// Check the password and keep the data key in memory. `Ok(false)` = wrong password.
    pub fn unlock(&self, password: &str) -> Result<bool> {
        if !self.has_password() { bail!("no protected-notes password has been set"); }
        let vsalt = self.option_str("passwordVerificationSalt").context("passwordVerificationSalt option missing")?;
        let dsalt = self.option_str("passwordDerivedKeySalt").context("passwordDerivedKeySalt option missing")?;
        let want = self.option_str("passwordVerificationHash").unwrap_or_default();
        if B64.encode(crypto::scrypt_hash(password, &vsalt)?) != want { return Ok(false); }
        let derived = crypto::scrypt_hash(password, &dsalt)?;
        let enc = self.option_str("encryptedDataKey").unwrap_or_default();
        let Some(key) = crypto::decrypt(&derived, &enc) else { bail!("the stored data key can't be decrypted") };
        *self.data_key.borrow_mut() = Some(key);
        Ok(true)
    }

    /// First-time password (`setPassword`): only when none exists yet. Leaves the session unlocked.
    pub fn set_password(&self, password: &str) -> Result<()> {
        crypto::check_not_empty(password)?;
        if self.has_password() { bail!("a protected-notes password is already set; use change_password"); }
        // a replica linked to a server that hasn't pulled yet may be missing that server's password: setting one now would replace it
        if self.state("documentSecret")?.is_some() && self.state("lastSyncedPull")?.is_none() { bail!("this replica is linked to a server but hasn't synced yet; sync first (the server may already have a protected-notes password)"); }
        let key = crypto::random_token(16).into_bytes();
        self.write_password(password, &key)?;
        *self.data_key.borrow_mut() = Some(key);
        Ok(())
    }

    /// Re-wrap the same data key under a new password (existing protected data stays readable).
    pub fn change_password(&self, old: &str, new: &str) -> Result<()> {
        crypto::check_not_empty(new)?;
        if !self.unlock(old)? { bail!("wrong password"); }
        let key = self.data_key.borrow().clone().context(LOCKED)?;
        self.write_password(new, &key)
    }

    fn write_password(&self, password: &str, key: &[u8]) -> Result<()> {
        let (vsalt, dsalt) = (crypto::random_token(32), crypto::random_token(32));
        let hash = B64.encode(crypto::scrypt_hash(password, &vsalt)?);
        let enc = crypto::encrypt(&crypto::scrypt_hash(password, &dsalt)?, key);
        let tx = self.begin()?;
        self.set_option("passwordVerificationSalt", &vsalt, true)?;
        self.set_option("passwordDerivedKeySalt", &dsalt, true)?;
        self.set_option("encryptedDataKey", &enc, true)?;
        self.set_option("passwordVerificationHash", &hash, true)?;
        tx.commit()
    }

    pub(crate) fn key(&self) -> Result<Vec<u8>> { self.data_key.borrow().clone().context(LOCKED) }

    /// Encrypt arbitrary text with the session key (e.g. for recovery files).
    pub fn encrypt_text(&self, text: &str) -> Result<String> { self.encrypt_title(text) }

    /// Seconds of inactivity after which a session locks itself (`protectedSessionTimeout`, default 10 min; 0 = never).
    pub fn protected_session_timeout(&self) -> u64 { self.option_i64("protectedSessionTimeout", 600).max(0) as u64 }

    pub(crate) fn encrypt_title(&self, title: &str) -> Result<String> { Ok(crypto::encrypt(&self.key()?, title.as_bytes())) }

    /// Plaintext of a stored protected title, or the placeholder when locked / undecryptable.
    pub fn decrypt_title(&self, stored: &str) -> String {
        let Some(k) = self.data_key.borrow().clone() else { return PLACEHOLDER.into() };
        crypto::decrypt(&k, stored).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| PLACEHOLDER.into())
    }

    /// Store content as a blob, encrypted when `protected` (blobId derives from the plaintext, the stored text is the ciphertext).
    pub(crate) fn save_blob_for(&self, protected: bool, content: &[u8], as_text: bool) -> Result<String> {
        if !protected { return self.save_blob(content, as_text); }
        let key = self.key()?;
        let mut pre = BLOB_PREFIX.as_bytes().to_vec();
        pre.extend_from_slice(content);
        let blob_id = hashed_blob_id(&pre);
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM blobs WHERE blobId=?", [&blob_id], |r| r.get(0)).optional()?;
        if exists.is_some() { self.republish_if_orphan(&blob_id)?; }
        if exists.is_none() {
            let stored = crypto::encrypt(&key, content);
            let utc = now_utc();
            self.conn.execute("INSERT INTO blobs(blobId,content,dateModified,utcDateModified) VALUES(?,?,?,?)", params![blob_id, stored, now_local(), utc])?;
            self.put_entity_change("blobs", &blob_id, &hash::blob_hash(&blob_id, stored.as_bytes(), None), &utc)?;
        }
        Ok(blob_id)
    }

    /// Is this note protected? (`None` for a missing note.)
    pub(crate) fn note_protected(&self, note_id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT isProtected FROM notes WHERE noteId=?", [note_id], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) != 0)
    }

    /// Store `content` for an existing note the way the note itself is stored (encrypted when it is protected).
    pub(crate) fn save_blob_for_note(&self, note_id: &str, content: &[u8], as_text: bool) -> Result<String> {
        self.save_blob_for(self.note_protected(note_id)?, content, as_text)
    }

    /// Raw stored content → plaintext bytes.
    pub(crate) fn plain_bytes(&self, protected: bool, v: rusqlite::types::Value) -> Result<Vec<u8>> {
        let raw = match v { rusqlite::types::Value::Text(s) => s.into_bytes(), rusqlite::types::Value::Blob(b) => b, _ => vec![] };
        if !protected { return Ok(raw); }
        let key = self.key()?;
        crypto::decrypt(&key, &String::from_utf8_lossy(&raw)).context("protected content can't be decrypted (wrong key or damaged data)")
    }

    /// Protect or unprotect a note: title, content, attachments and revisions are re-encrypted / decrypted. Needs the session unlocked.
    pub fn protect_note(&self, note_id: &str, protect: bool, recursive: bool) -> Result<usize> {
        self.key()?;
        let mut ids = vec![note_id.to_string()];
        let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
        if recursive {
            let mut i = 0;
            while i < ids.len() {
                let mut st = self.conn.prepare("SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0")?;
                let kids: Vec<String> = st.query_map([&ids[i]], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                for k in kids { if seen.insert(k.clone()) { ids.push(k); } }
                i += 1;
                if ids.len() > 200_000 { bail!("subtree too large"); }
            }
        }
        let tx = self.begin()?;
        let mut changed = 0;
        for id in &ids { if self.protect_one(id, protect)? { changed += 1; } }
        tx.commit()?;
        if changed > 0 && protect {
            // the text index must not keep what was just encrypted: drop its rows and merge the segments that held them
            let _ = crate::search::index::prune_index(self);
            let _ = self.conn.execute("INSERT INTO tui_fts(tui_fts) VALUES('optimize')", []);
            // fold the WAL in so the superseded plaintext pages don't linger in the -wal file
            let _ = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(changed)
    }

    fn convert_title(&self, title: &str, protect: bool) -> Result<String> {
        if protect { self.encrypt_title(title) } else {
            let k = self.key()?;
            let b = crypto::decrypt(&k, title).context("a title can't be decrypted (wrong key or damaged data)")?;
            Ok(String::from_utf8_lossy(&b).into_owned())
        }
    }

    /// Re-store one blob under the other protection state; returns the new blobId.
    fn convert_blob(&self, blob_id: Option<String>, was_protected: bool, protect: bool, as_text: impl Fn(&[u8]) -> bool) -> Result<Option<String>> {
        let Some(old) = blob_id else { return Ok(None) };
        let v: Option<rusqlite::types::Value> = self.conn.query_row("SELECT content FROM blobs WHERE blobId=?", [&old], |r| r.get(0)).optional()?;
        let Some(v) = v else { return Ok(Some(old)) };
        let plain = self.plain_bytes(was_protected, v)?;
        let t = as_text(&plain);
        Ok(Some(self.save_blob_for(protect, &plain, t)?))
    }

    /// An attachment's title and blob as they must be stored for a note in state `to` (it may have been saved in the other state).
    pub(crate) fn attachment_in_state(&self, title: &str, blob: Option<String>, mime: &str, was: bool, to: bool) -> Result<(String, Option<String>)> {
        if was == to { return Ok((title.to_string(), blob)); }
        let new_title = self.convert_title(title, to)?;
        let new_blob = self.convert_blob(blob, was, to, |p| is_string_note("", mime) && std::str::from_utf8(p).is_ok())?;
        Ok((new_title, new_blob))
    }

    /// Bring one note (title, content, its attachments and its revisions with theirs) into the requested state. Rows already
    /// there are left alone, so running it again repairs a note whose rows ended up in mixed states.
    fn protect_one(&self, id: &str, protect: bool) -> Result<bool> {
        let (title, ntype, mime, cur, blob): (String, String, String, i64, Option<String>) = self.conn.query_row(
            "SELECT title,type,mime,isProtected,blobId FROM notes WHERE noteId=? AND isDeleted=0", [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?.with_context(|| format!("note {id} not found"))?;
        let mut changed = false;
        let is_str = is_string_note(&ntype, &mime);
        if (cur != 0) != protect {
            let new_title = self.convert_title(&title, protect)?;
            let new_blob = self.convert_blob(blob.clone(), cur != 0, protect, |p| is_str && std::str::from_utf8(p).is_ok())?;
            self.conn.execute("UPDATE notes SET title=?, isProtected=?, blobId=?, dateModified=?, utcDateModified=? WHERE noteId=?",
                params![new_title, protect as i64, new_blob, now_local(), now_utc(), id])?;
            self.record_entity("notes", "noteId", id, "utcDateModified")?;
            if let (Some(o), Some(n)) = (&blob, &new_blob) { if o != n { self.drop_blob_if_unused(o)?; } }
            changed = true;
        }
        // attachments belong to the note and to each of its revisions
        let mut owners = vec![id.to_string()];
        {
            let mut st = self.conn.prepare("SELECT revisionId FROM revisions WHERE noteId=?")?;
            owners.extend(st.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        for (table, pk, owner_col, type_col) in [("attachments", "attachmentId", "ownerId", "''"), ("revisions", "revisionId", "noteId", "type")] {
            let mut rows: Vec<(String, String, i64, Option<String>, String, String)> = vec![];
            for owner in if table == "attachments" { owners.as_slice() } else { &owners[..1] } {
                let mut st = self.conn.prepare(&format!("SELECT {pk},title,isProtected,blobId,mime,{type_col} FROM {table} WHERE {owner_col}=?"))?;
                rows.extend(st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?);
            }
            for (rid, rtitle, rprot, rblob, rmime, rtype) in rows {
                if (rprot != 0) == protect { continue; }
                let t = self.convert_title(&rtitle, protect)?;
                let nb = self.convert_blob(rblob.clone(), rprot != 0, protect, |p| is_string_note(&rtype, &rmime) && std::str::from_utf8(p).is_ok())?;
                self.conn.execute(&format!("UPDATE {table} SET title=?, isProtected=?, blobId=?, utcDateModified=? WHERE {pk}=?"), params![t, protect as i64, nb, now_utc(), rid])?;
                self.record_entity(table, pk, &rid, "utcDateModified")?;
                if let (Some(o), Some(n)) = (&rblob, &nb) { if o != n { self.drop_blob_if_unused(o)?; } }
                changed = true;
            }
        }
        Ok(changed)
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

    fn raw_title(db: &Db, id: &str) -> String { db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [id], |r| r.get(0)).unwrap() }

    fn any_plain(db: &Db, needle: &str) -> bool {
        for (t, c) in [("notes", "title"), ("blobs", "content"), ("revisions", "title"), ("attachments", "title")] {
            let mut st = db.conn.prepare(&format!("SELECT CAST({c} AS TEXT) FROM {t}")).unwrap();
            let rows: Vec<Option<String>> = st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
            if rows.iter().flatten().any(|s| s.contains(needle)) { return true; }
        }
        false
    }

    #[test]
    fn password_lifecycle() {
        let (_d, db) = db();
        assert!(!db.has_password() && !db.is_unlocked());
        db.set_password("hunter2").unwrap();
        assert!(db.has_password() && db.is_unlocked());
        db.lock();
        assert!(!db.is_unlocked());
        assert!(!db.unlock("wrong").unwrap() && !db.is_unlocked());
        assert!(db.unlock("hunter2").unwrap() && db.is_unlocked());
        assert!(db.set_password("again").is_err());
        let key = db.key().unwrap();
        db.change_password("hunter2", "new pass").unwrap();
        db.lock();
        assert!(!db.unlock("hunter2").unwrap());
        assert!(db.unlock("new pass").unwrap());
        assert_eq!(db.key().unwrap(), key, "changing the password keeps the data key");
        // the password options sync like Trilium's
        let synced: i64 = db.conn.query_row("SELECT COUNT(*) FROM options WHERE name IN ('passwordVerificationSalt','passwordDerivedKeySalt','passwordVerificationHash','encryptedDataKey') AND isSynced=1", [], |r| r.get(0)).unwrap();
        assert_eq!(synced, 4);
    }

    #[test]
    fn protected_notes_never_store_plaintext() {
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let n = db.create_note("root", "Secret diary", "text", "text/html", "").unwrap();
        db.set_note_content(&n, "<p>dear diary</p>").unwrap();
        assert!(db.protect_note(&n, true, false).unwrap() == 1);
        assert!(!any_plain(&db, "Secret diary") && !any_plain(&db, "dear diary"));
        assert_eq!(db.note_content(&n).unwrap().unwrap(), "<p>dear diary</p>");
        assert_eq!(db.decrypt_title(&raw_title(&db, &n)), "Secret diary");
        // edits while protected stay encrypted, create a revision, and the revision is protected too
        db.set_note_title(&n, "Renamed secret").unwrap();
        db.set_note_content(&n, "<p>second thoughts</p>").unwrap();
        db.save_revision(&n, "", "manual").unwrap();
        assert!(!any_plain(&db, "Renamed secret") && !any_plain(&db, "second thoughts"));
        let revs = db.revisions(&n).unwrap();
        assert!(!revs.is_empty() && revs.iter().all(|r| r.protected));
        assert_eq!(db.revision_content(&revs[0].id).unwrap().unwrap(), "<p>second thoughts</p>");
        assert_eq!(revs[0].title, "Renamed secret");
        // a new child of a protected note is protected
        let c = db.create_note(&n, "child secret", "text", "text/html", "").unwrap();
        assert!(db.note_protected(&c).unwrap() && !any_plain(&db, "child secret"));
        // attachments
        let a = db.add_attachment(&n, "key.txt", "text/plain", "file", b"attached secret").unwrap();
        assert!(!any_plain(&db, "attached secret") && !any_plain(&db, "key.txt"));
        assert_eq!(db.attachment_bytes(&a).unwrap().unwrap(), b"attached secret");
        assert_eq!(db.attachments(&n).unwrap()[0].title, "key.txt");
        // locked: nothing readable, nothing writable, titles masked
        db.lock();
        assert_eq!(db.decrypt_title(&raw_title(&db, &n)), PLACEHOLDER);
        for e in [db.note_content(&n).unwrap_err(), db.set_note_title(&n, "x").unwrap_err(), db.set_note_content(&n, "x").unwrap_err(),
                  db.create_note(&n, "x", "text", "text/html", "").unwrap_err(), db.add_attachment(&n, "x", "text/plain", "file", b"x").unwrap_err(),
                  db.save_revision(&n, "", "manual").unwrap_err(), db.protect_note(&n, false, false).unwrap_err()] {
            assert!(is_locked_error(&e), "{e}");
        }
        assert_eq!(db.attachments(&n).unwrap()[0].title, PLACEHOLDER);
        // a wrong key reads as damaged data, never as garbage
        db.unlock("pw").unwrap();
        *db.data_key.borrow_mut() = Some(b"another key 1234".to_vec());
        assert!(db.note_content(&n).is_err());
    }

    #[test]
    fn unprotecting_restores_plain_rows_and_survives_a_round_trip() {
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let n = db.create_note("root", "Plain title", "text", "text/html", "").unwrap();
        db.set_note_content(&n, "<p>body</p>").unwrap();
        let a = db.add_attachment(&n, "a.bin", "application/octet-stream", "file", &[0, 159, 146, 150]).unwrap();
        db.save_revision(&n, "", "manual").unwrap();
        assert_eq!(db.protect_note(&n, true, false).unwrap(), 1);
        assert_eq!(db.protect_note(&n, true, false).unwrap(), 0, "already protected");
        assert_eq!(db.attachment_bytes(&a).unwrap().unwrap(), vec![0, 159, 146, 150]);
        assert_eq!(db.protect_note(&n, false, false).unwrap(), 1);
        assert!(!db.note_protected(&n).unwrap());
        assert_eq!(raw_title(&db, &n), "Plain title");
        assert_eq!(db.note_content(&n).unwrap().unwrap(), "<p>body</p>");
        assert_eq!(db.attachment_bytes(&a).unwrap().unwrap(), vec![0, 159, 146, 150]);
        assert!(db.revisions(&n).unwrap().iter().all(|r| !r.protected));
        // recursive protection covers the subtree
        let k = db.create_note(&n, "kid", "text", "text/html", "").unwrap();
        assert_eq!(db.protect_note(&n, true, true).unwrap(), 2);
        assert!(db.note_protected(&k).unwrap());
    }

    #[test]
    fn restoring_a_protected_revision_keeps_the_note_encrypted() {
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let n = db.create_note("root", "T", "text", "text/html", "").unwrap();
        db.set_note_content(&n, "<p>v1 secret</p>").unwrap();
        db.protect_note(&n, true, false).unwrap();
        let r = db.save_revision(&n, "", "manual").unwrap();
        db.set_note_content(&n, "<p>v2 secret</p>").unwrap();
        db.restore_revision(&r).unwrap();
        assert_eq!(db.note_content(&n).unwrap().unwrap(), "<p>v1 secret</p>");
        assert!(!any_plain(&db, "v1 secret") && !any_plain(&db, "v2 secret"));
        // binary notes round-trip too
        let f = db.create_file_note(&n, "pic.png", "image/png", &[1, 2, 3, 255]).unwrap();
        assert_eq!(db.note_bytes(&f).unwrap().unwrap(), vec![1, 2, 3, 255]);
    }

    #[test]
    fn duplicating_protected_notes_keeps_the_copy_encrypted() {
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let plain_parent = db.create_note("root", "Plain parent", "text", "text/html", "").unwrap();
        let n = db.create_note(&plain_parent, "Hidden thing", "text", "text/html", "<p>needle content</p>").unwrap();
        db.add_attachment(&n, "att.txt", "text/plain", "file", b"needle attachment").unwrap();
        db.create_note(&n, "Hidden kid", "text", "text/html", "<p>kid needle</p>").unwrap();
        db.protect_note(&n, true, true).unwrap();
        db.lock();
        assert!(is_locked_error(&db.duplicate_subtree(&n, &plain_parent, true).unwrap_err()));
        db.unlock("pw").unwrap();
        let copy = db.duplicate_subtree(&n, &plain_parent, true).unwrap();
        assert!(db.note_protected(&copy).unwrap());
        assert_eq!(db.note_content(&copy).unwrap().unwrap(), "<p>needle content</p>");
        assert_eq!(db.decrypt_title(&raw_title(&db, &copy)), "Hidden thing (dup)");
        assert_eq!(db.attachments(&copy).unwrap()[0].title, "att.txt");
        assert!(!any_plain(&db, "needle") && !any_plain(&db, "Hidden"));
        let kids: i64 = db.conn.query_row("SELECT COUNT(*) FROM branches WHERE parentNoteId=? AND isDeleted=0", [&copy], |r| r.get(0)).unwrap();
        assert_eq!(kids, 1);
    }

    #[test]
    fn protected_notes_are_searched_like_trilium_only_while_unlocked() {
        use crate::search::{SearchOpts, Searcher};
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let a = db.create_note("root", "Tax Notes", "text", "text/html", "<p>the <b>Zürich</b> account</p>").unwrap();
        let b = db.create_note("root", "Other", "text", "text/html", "<p>nothing</p>").unwrap();
        db.add_attribute(&a, "label", "secretlabel", "", false).unwrap();
        db.protect_note(&a, true, false).unwrap();
        let find = |q: &str| -> Vec<String> {
            let s = Searcher::new(&db).unwrap();
            s.search(&db, q, &SearchOpts::default()).unwrap().hits.into_iter().map(|h| h.note_id).collect()
        };
        for q in ["zurich account", "tax", "\"zürich account\"", "note.content *=* zurich", "tax #secretlabel"] {
            assert!(find(q).contains(&a), "unlocked: {q}");
        }
        assert!(!find("nothing zurich").contains(&a));
        assert!(find("nothing").contains(&b));
        db.lock();
        assert!(!find("zurich").contains(&a) && !find("tax").contains(&a), "locked: neither text nor title are searchable");
        assert!(!find("protected").contains(&a), "a locked title isn't searchable, not even as [protected] (checked against Trilium)");
        assert!(find("#secretlabel").contains(&a), "attributes aren't encrypted");
    }

    #[test]
    fn revision_attachments_and_the_text_index_follow_protection() {
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let n = db.create_note("root", "T", "text", "text/html", "<p>indexed needle body</p>").unwrap();
        let a = db.add_attachment(&n, "att.txt", "text/plain", "file", b"ATTACH_SECRET_1").unwrap();
        db.save_revision(&n, "", "manual").unwrap();
        crate::search::index::ensure_index(&db).unwrap();
        assert!(crate::search::index::notes_containing(&db, "needle").unwrap().contains(&n));
        db.protect_note(&n, true, false).unwrap();
        assert!(!any_plain(&db, "ATTACH_SECRET_1") && !any_plain(&db, "att.txt"), "revision attachments are encrypted too");
        let fts: i64 = db.conn.query_row("SELECT COUNT(*) FROM tui_fts_map", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 0, "the index no longer maps the old plaintext blob");
        let leftover: i64 = db.conn.query_row("SELECT COUNT(*) FROM tui_fts WHERE tui_fts MATCH '\"needle\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(leftover, 0);
        // restoring a revision keeps the attachment readable
        let rev = db.revisions(&n).unwrap()[0].id.clone();
        db.restore_revision(&rev).unwrap();
        let atts = db.attachments(&n).unwrap();
        assert_eq!(atts[0].title, "att.txt");
        assert_eq!(db.attachment_bytes(&atts[0].id).unwrap().unwrap(), b"ATTACH_SECRET_1");
        let _ = a;
        // and the reverse
        db.protect_note(&n, false, false).unwrap();
        let rows: Vec<(String, i64)> = { let mut st = db.conn.prepare("SELECT title,isProtected FROM attachments").unwrap(); st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect() };
        assert!(rows.iter().all(|(t, p)| t == "att.txt" && *p == 0), "{rows:?}");
        // a second run repairs a mixed note instead of returning early
        db.conn.execute("UPDATE attachments SET isProtected=0", []).unwrap();
        db.protect_note(&n, true, false).unwrap();
        db.conn.execute("UPDATE attachments SET isProtected=0, title='att.txt' WHERE rowid=(SELECT MIN(rowid) FROM attachments)", []).unwrap_or(0);
    }

    #[test]
    fn a_linked_but_unsynced_replica_does_not_invent_a_password() {
        let (_d, db) = db();
        db.set_state("documentSecret", "s").unwrap();
        assert!(db.set_password("pw").is_err());
        db.set_state("lastSyncedPull", "5").unwrap();
        assert!(db.set_password("pw").is_ok());
    }

    #[test]
    fn exports_and_templates_never_carry_protected_text_out_in_the_clear() {
        use std::io::{Cursor, Read};
        let (_d, db) = db();
        db.set_password("pw").unwrap();
        let n = db.create_note("root", "Folder", "text", "text/html", "<p>plain body</p>").unwrap();
        let sec = db.create_note(&n, "Secret", "text", "text/html", "<p>hidden</p>").unwrap();
        db.add_attachment(&sec, "SECRETFILE.txt", "text/plain", "file", b"TOPSECRETBYTES").unwrap();
        db.protect_note(&sec, true, false).unwrap();
        let plain_att = db.add_attachment(&n, "ok.txt", "text/plain", "file", b"fine").unwrap();
        let mut buf = Cursor::new(vec![]);
        db.export_zip(&n, &mut buf).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(buf.into_inner())).unwrap();
        let mut all = String::new();
        for i in 0..z.len() { let mut f = z.by_index(i).unwrap(); let name: String = f.name_raw().iter().map(|&b| b as char).collect(); let mut b = vec![]; f.read_to_end(&mut b).unwrap(); all.push_str(&name); all.push_str(&String::from_utf8_lossy(&b)); }
        assert!(!all.contains("SECRETFILE") && !all.contains("TOPSECRETBYTES") && !all.contains("hidden"), "{all}");
        assert!(all.contains("fine"), "ordinary attachments still export");
        assert!(db.source_is_protected(&format!("api/attachments/{}/image/x", db.attachments(&sec).unwrap()[0].id)) && !db.source_is_protected(&format!("api/attachments/{plain_att}/image/x")));
        // a protected template's text lands in a protected note
        let tpl = db.create_note("root", "Tpl", "text", "text/html", "<p>template secret</p>").unwrap();
        db.protect_note(&tpl, true, false).unwrap();
        let target = db.create_note("root", "Target", "text", "text/html", "").unwrap();
        db.apply_template(&target, &tpl).unwrap();
        assert!(db.note_protected(&target).unwrap() && !any_plain(&db, "template secret"));
    }

    #[test]
    fn orphan_blobs_are_swept_with_their_change_rows_and_index_entries() {
        let (_d, db) = db();
        let n = db.create_note("root", "T", "text", "text/html", "<p>sweepneedle</p>").unwrap();
        crate::search::index::ensure_index(&db).unwrap();
        let blob: String = db.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        // the note moves on to other content without our code dropping the old blob (as when the server did it)
        db.conn.execute("UPDATE notes SET blobId=NULL WHERE noteId=?", [&n]).unwrap();
        assert_eq!(db.sweep_orphan_blobs().unwrap(), 1);
        let left: i64 = db.conn.query_row("SELECT (SELECT COUNT(*) FROM blobs WHERE blobId=?1) + (SELECT COUNT(*) FROM entity_changes WHERE entityName='blobs' AND entityId=?1) + (SELECT COUNT(*) FROM tui_fts_map WHERE blobId=?1)", [&blob], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        assert_eq!(db.sweep_orphan_blobs().unwrap(), 0);
    }

    #[test]
    fn notes_named_like_the_archives_own_files_still_export() {
        let (_d, db) = db();
        for name in ["index", "navigation", "style.css", "index.html"] {
            let n = db.create_note("root", name, "text", "text/html", "<p>x</p>").unwrap();
            let mut buf = std::io::Cursor::new(vec![]);
            db.export_zip(&n, &mut buf).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }
}
