//! Local Trilium-schema SQLite database and the "apply remote change" logic
//! (port of `sync_update.ts::updateNormalEntity`).
use crate::hash;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as Sql};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

const SCHEMA: &str = include_str!("schema.sql");
/// Highest Trilium database version (migration number) this build has been verified against.
pub const KNOWN_DB_VERSION: i64 = 241;
/// Tables a sync change may touch (also guards the dynamic SQL below).
const ENTITY_TABLES: &[(&str, &str)] = &[
    ("notes", "noteId"), ("branches", "branchId"), ("attributes", "attributeId"),
    ("attachments", "attachmentId"), ("revisions", "revisionId"), ("blobs", "blobId"),
    ("options", "name"), ("etapi_tokens", "etapiTokenId"),
];

pub fn primary_key(entity: &str) -> Option<&'static str> {
    ENTITY_TABLES.iter().find(|(t, _)| *t == entity).map(|(_, pk)| *pk)
}

pub struct Db {
    pub conn: Connection,
    depth: std::cell::Cell<u32>,
    /// The protected-session data key (None = locked); never written to disk.
    pub(crate) data_key: std::cell::RefCell<Option<Vec<u8>>>,
}

/// Nestable transaction (SAVEPOINT based): the outermost one is a real transaction, inner ones
/// roll back only their own work. Dropping without `commit()` rolls back.
pub struct Tx<'a> {
    db: &'a Db,
    name: String,
    done: bool,
}

impl Tx<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        self.db.depth.set(self.db.depth.get().saturating_sub(1));
        let r = if self.name.is_empty() { self.db.conn.execute_batch("COMMIT") } else { self.db.conn.execute_batch(&format!("RELEASE {}", self.name)) };
        if let Err(e) = r {
            // a failed commit must not leave a half-open transaction behind
            let _ = self.db.conn.execute_batch(&Self::rollback_sql(&self.name));
            return Err(e.into());
        }
        Ok(())
    }

    fn rollback_sql(name: &str) -> String {
        if name.is_empty() { "ROLLBACK".into() } else { format!("ROLLBACK TO {name}; RELEASE {name}") }
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.db.depth.set(self.db.depth.get().saturating_sub(1));
            let _ = self.db.conn.execute_batch(&Self::rollback_sql(&self.name));
        }
    }
}

#[derive(Default, Debug)]
pub struct ApplyStats {
    pub applied: u32,
    pub skipped_known: u32,
    pub local_newer: u32,
    pub erased: u32,
    /// Remote changes dropped because a newer, not-yet-pushed local change to the same entity wins
    /// (last writer wins, as in Trilium). (entityName, entityId)
    pub conflicts: Vec<(String, String)>,
    /// Unpushed local edits that an equal-or-newer remote change replaced (last writer wins). For notes the replaced
    /// content is kept so it can be saved aside. (entityName, entityId, title, content)
    pub overwritten: Vec<(String, String, String, Option<Vec<u8>>)>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let fresh = !path.exists();
        #[cfg(unix)]
        if fresh {
            // the file will hold the sync secret: it must never exist with a looser mode, not even for a moment (the -wal/-shm inherit it)
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).with_context(|| format!("creating {}", path.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // deleted rows (a note's old plaintext after it gets protected) are overwritten, not left in free pages
        conn.pragma_update(None, "secure_delete", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // empty file (fresh, touched, or left by a crash right after creation): lay down the schema
        let has_notes: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='notes'", [], |r| r.get::<_, i64>(0)).optional()?.is_some();
        if fresh || !has_notes {
            let empty: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
            if empty == 0 { conn.execute_batch(SCHEMA)?; }
        }
        // Our own bookkeeping; Trilium ignores unknown tables.
        conn.execute_batch("CREATE TABLE IF NOT EXISTS tui_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        Ok(Self { conn, depth: std::cell::Cell::new(0), data_key: Default::default() })
    }

    /// Server's database version as last seen at sync login (0 = never synced).
    pub fn server_db_version(&self) -> i64 {
        self.state("serverDbVersion").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// Writes are refused when the server runs a newer Trilium database than this build knows,
    /// because our hashing/bookkeeping rules might no longer match.
    pub fn ensure_writable(&self) -> Result<()> {
        let v = self.server_db_version();
        if v > KNOWN_DB_VERSION {
            bail!("the server's Trilium database (version {v}) is newer than this build supports ({KNOWN_DB_VERSION}); read-only until trilium-tui is updated");
        }
        Ok(())
    }

    /// Copy the database to `<name>.bak-YYYY-MM-DD.db` next to it, at most once per day, keeping the
    /// newest `keep` copies. Uses SQLite's own `VACUUM INTO`, which is consistent while other connections write.
    pub fn backup_daily(&self, db_path: &Path, keep: usize) -> Result<Option<std::path::PathBuf>> {
        let stem = db_path.file_stem().and_then(|s| s.to_str()).unwrap_or("replica");
        let dir = db_path.parent().unwrap_or(Path::new("."));
        let today = chrono::Local::now().format("%Y-%m-%d");
        let target = dir.join(format!("{stem}.bak-{today}.db"));
        if target.exists() { return Ok(None); }
        #[cfg(unix)]
        { // VACUUM INTO accepts an empty target: make it private before the secret lands in it
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&target)?;
        }
        if let Err(e) = self.conn.execute("VACUUM INTO ?", [target.to_string_lossy().as_ref()]) {
            let _ = std::fs::remove_file(&target); // a partial file would block today's backup
            return Err(e.into());
        }
        #[cfg(unix)]
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o600))?; // contains the sync secret
        let mut baks: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| Self::is_backup_name(stem, n))).collect();
        baks.sort();
        while baks.len() > keep { let _ = std::fs::remove_file(baks.remove(0)); }
        Ok(Some(target))
    }

    /// Is `name` one of this database's automatic backups, exactly `<stem>.bak-YYYY-MM-DD.db`?
    fn is_backup_name(stem: &str, name: &str) -> bool {
        name.strip_prefix(&format!("{stem}.bak-")).and_then(|r| r.strip_suffix(".db"))
            .is_some_and(|date| date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok())
    }

    /// The automatic daily backups next to this database (`<name>.bak-YYYY-MM-DD.db`; other files are never touched).
    pub fn backups(&self) -> Vec<std::path::PathBuf> {
        let Some(db_path) = self.conn.path().map(std::path::PathBuf::from) else { return vec![] };
        let stem = db_path.file_stem().and_then(|s| s.to_str()).unwrap_or("replica").to_string();
        let Ok(rd) = std::fs::read_dir(db_path.parent().unwrap_or(Path::new("."))) else { return vec![] };
        let mut v: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| Self::is_backup_name(&stem, n))).collect();
        v.sort();
        v
    }

    /// Delete every automatic backup (after protecting notes, older copies still hold their plaintext). Returns how many went.
    pub fn delete_backups(&self) -> usize { self.backups().into_iter().filter(|p| std::fs::remove_file(p).is_ok()).count() }

    pub(crate) fn begin(&self) -> Result<Tx<'_>> {
        self.ensure_writable()?;
        let d = self.depth.get() + 1;
        // the outermost transaction takes the write lock up front (waiting out the sync thread's commit via busy_timeout)
        // instead of failing instantly with SQLITE_BUSY when it upgrades from a read
        let name = if d == 1 { String::new() } else { format!("sp{d}") };
        self.conn.execute_batch(&if d == 1 { "BEGIN IMMEDIATE".to_string() } else { format!("SAVEPOINT {name}") })?;
        self.depth.set(d);
        Ok(Tx { db: self, name, done: false })
    }

    pub fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM tui_state WHERE key=?", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO tui_state(key,value) VALUES(?,?)", [key, value])?;
        Ok(())
    }

    /// Apply one pulled batch atomically. `source_instance` is the sync server's instanceId.
    pub fn apply_changes(&mut self, source_instance: &str, records: &[Value]) -> Result<ApplyStats> {
        let local_instance = self.state("instanceId")?.unwrap_or_default();
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut stats = ApplyStats::default();
        for rec in records {
            let ec = &rec["entityChange"];
            let name = ec["entityName"].as_str().context("entityName")?;
            let id = ec["entityId"].as_str().context("entityId")?;
            let change_id = ec["changeId"].as_str();
            // old servers have entries with an empty changeId (thousands of them in long-lived databases): they are not unique, so never a duplicate marker
            if let Some(cid) = change_id.filter(|c| !c.is_empty()) {
                let seen: Option<i64> = tx.query_row("SELECT 1 FROM entity_changes WHERE changeId=?", [cid], |r| r.get(0)).optional()?;
                if seen.is_some() { stats.skipped_known += 1; continue; }
            }
            let local: Option<String> = tx.query_row(
                "SELECT utcDateChanged FROM entity_changes WHERE entityName=? AND entityId=?",
                [name, id], |r| r.get(0)).optional()?;
            let remote_date = ec["utcDateChanged"].as_str().unwrap_or("");
            // is our current version an edit the server has not seen yet, and does it differ from the remote one?
            let unpushed_local = if local.is_some() && name != "note_reordering" {
                let last_pushed: i64 = tx.query_row("SELECT value FROM tui_state WHERE key='lastSyncedPush'", [], |r| r.get::<_, String>(0)).optional()?.and_then(|s| s.parse().ok()).unwrap_or(0);
                let (our_id, our_hash, our_instance): (i64, String, String) = tx.query_row(
                    "SELECT id,hash,instanceId FROM entity_changes WHERE entityName=? AND entityId=?", [name, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                our_instance != source_instance && our_id > last_pushed && our_hash != ec["hash"].as_str().unwrap_or("")
            } else { false };
            // last writer wins; equal timestamps take the remote (sync_update.ts: `localECIsOlderOrSameAsRemote` needs both
            // dates to be non-empty). A note_reordering is always applied, whatever the dates (updateNoteReordering).
            let remote_older_or_same = local.as_deref().is_some_and(|l| !l.is_empty() && !remote_date.is_empty() && l <= remote_date);
            if name != "note_reordering" && local.is_some() && !remote_older_or_same {
                stats.local_newer += 1;
                if unpushed_local { stats.conflicts.push((name.to_string(), id.to_string())); }
                // our side is newer: make sure the server hears about it again (putEntityChangeForOtherInstances),
                // otherwise a server that lost it (restored backup) would never get it back
                let (lh, le): (String, i64) = tx.query_row("SELECT hash,isErased FROM entity_changes WHERE entityName=? AND entityId=?", [name, id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                if lh != ec["hash"].as_str().unwrap_or("") || le != as_int(&ec["isErased"]) {
                    requeue_entity_change(&tx, &local_instance, name, id)?;
                }
                continue;
            }
            if unpushed_local {
                let (title, blob): (String, Option<String>) = if name == "notes" {
                    tx.query_row("SELECT title,blobId FROM notes WHERE noteId=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or_default()
                } else { Default::default() };
                let content: Option<Vec<u8>> = match blob {
                    Some(b) => tx.query_row("SELECT content FROM blobs WHERE blobId=?", [b], |r| r.get::<_, rusqlite::types::Value>(0)).optional()?
                        .map(|v| match v { rusqlite::types::Value::Text(t) => t.into_bytes(), rusqlite::types::Value::Blob(b) => b, _ => vec![] }),
                    None => None,
                };
                stats.overwritten.push((name.to_string(), id.to_string(), title, content));
            }

            if name == "note_reordering" {
                if let Some(map) = rec["entity"].as_object() {
                    for (branch, pos) in map {
                        tx.execute("UPDATE branches SET notePosition=? WHERE branchId=?", params![pos.as_i64(), branch])?;
                    }
                }
            } else {
                // an entity type this build doesn't know (a newer server) is passed over rather than stalling every pull
                let Some(&(table, pk)) = ENTITY_TABLES.iter().find(|(t, _)| *t == name) else { stats.skipped_known += 1; continue };
                if ec["isErased"].as_i64() == Some(1) || ec["isErased"].as_bool() == Some(true) {
                    let in_use = table == "blobs" && ["notes", "attachments", "revisions"].iter().any(|t| tx.query_row(&format!("SELECT 1 FROM {t} WHERE blobId=? LIMIT 1"), [id], |r| r.get::<_, i64>(0)).optional().ok().flatten().is_some());
                    // sync_update.ts eraseEntity: only these six are ever erased (options / tokens: logged and left alone)
                    let erasable = ["notes", "branches", "attributes", "revisions", "attachments", "blobs"].contains(&table);
                    if erasable && !in_use { tx.execute(&format!("DELETE FROM {table} WHERE {pk}=?"), [id])?; }
                    stats.erased += 1;
                } else if let Some(row) = rec.get("entity").and_then(Value::as_object) {
                    // one malformed record must not block the rest of the batch (and every retry): leave it out and report it
                    if let Err(e) = replace_row(&tx, table, row) {
                        stats.conflicts.push((name.to_string(), format!("{id}: remote record not applied ({e:#})")));
                        continue;
                    }
                } else if table != "options" {
                    bail!("empty entity row for {name} {id}");
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO entity_changes(entityName,entityId,hash,isErased,changeId,componentId,instanceId,isSynced,utcDateChanged)
                 VALUES(?,?,?,?,?,?,?,?,?)",
                params![name, id, ec["hash"].as_str().unwrap_or(""), as_int(&ec["isErased"]),
                    change_id.unwrap_or(""), ec["componentId"].as_str().unwrap_or("NA"), source_instance,
                    as_int(&ec["isSynced"]), remote_date],
            )?;
            stats.applied += 1;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Keep what a newer remote change replaced: one file per note under `<db dir>/overwritten/`, plus a line in
    /// `conflicts.log`. Returns the directory when anything was written.
    pub fn save_overwritten(&self, items: &[(String, String, String, Option<Vec<u8>>)]) -> Result<Option<std::path::PathBuf>> {
        use std::io::Write;
        if items.is_empty() { return Ok(None); }
        let Some(dir) = self.conn.path().map(std::path::PathBuf::from).and_then(|p| p.parent().map(|d| d.to_path_buf())) else { return Ok(None) };
        let out = dir.join("overwritten");
        std::fs::create_dir_all(&out)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("conflicts.log"))?;
        for (entity, id, title, content) in items {
            let safe_title: String = title.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            writeln!(log, "{stamp}\toverwritten-by-remote\t{entity}\t{id}\t{safe_title}")?;
            if let Some(bytes) = content {
                let mut path = out.join(format!("{id}-{stamp}.txt"));
                let mut n = 1;
                while path.exists() { path = out.join(format!("{id}-{stamp}-{n}.txt")); n += 1; }
                let mut oo = std::fs::OpenOptions::new();
                oo.create(true).write(true).truncate(true);
                #[cfg(unix)]
                { use std::os::unix::fs::OpenOptionsExt; oo.mode(0o600); }
                oo.open(path)?.write_all(bytes)?;
            }
        }
        Ok(Some(out))
    }

    /// Recompute every entity hash from the stored rows and compare with `entity_changes.hash`.
    /// Returns entityName -> (matching, mismatching). Attachments accept either null/undefined rendering.
    pub fn recompute_hashes(&self) -> Result<BTreeMap<String, (u32, u32)>> {
        let mut out: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        let mut tally = |name: &str, ok: bool| {
            let e = out.entry(name.to_string()).or_default();
            if ok { e.0 += 1 } else { e.1 += 1 }
        };
        for &(table, pk) in ENTITY_TABLES {
            if table == "blobs" || table == "etapi_tokens" { continue; }
            let has_deleted = table != "revisions" && table != "attachments_" && table != "options";
            let mut stmt = self.conn.prepare(&format!("SELECT * FROM {table}"))?;
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let rows = stmt.query_map([], |r| {
                let mut m = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    m.insert(c.clone(), match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                        rusqlite::types::ValueRef::Real(f) => Value::from(f),
                        rusqlite::types::ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                        rusqlite::types::ValueRef::Blob(_) => Value::Null,
                    });
                }
                Ok(Value::Object(m))
            })?;
            for row in rows {
                let row = row?;
                let id = row[pk].as_str().unwrap_or_default().to_string();
                let stored: Option<String> = self.conn.query_row(
                    "SELECT hash FROM entity_changes WHERE entityName=? AND entityId=?", [table, &id], |r| r.get(0)).optional()?;
                let Some(stored) = stored else { tally(table, false); continue };
                let deleted = has_deleted && row["isDeleted"].as_i64() == Some(1);
                // A NULL column may have been hashed as "undefined" when the entity was first created
                // (see docs/P0-findings.md §10), so also try the row with its NULLs removed.
                let mut stripped = row.clone();
                stripped.as_object_mut().unwrap().retain(|_, v| !v.is_null());
                let ok = [(&row, hash::Missing::Null), (&stripped, hash::Missing::Undefined)].iter().any(|(r, m)| {
                    [false, deleted].iter().any(|d| hash::entity_hash(table, r, *d, *m).as_deref() == Some(&stored))
                });
                tally(table, ok);
            }
        }
        let mut stmt = self.conn.prepare("SELECT blobId, content, textRepresentation FROM blobs")?;
        let rows = stmt.query_map([], |r| {
            let content: Vec<u8> = match r.get_ref(1)? {
                rusqlite::types::ValueRef::Blob(b) | rusqlite::types::ValueRef::Text(b) => b.to_vec(),
                _ => Vec::new(),
            };
            Ok((r.get::<_, String>(0)?, content, r.get::<_, Option<String>>(2)?))
        })?;
        for row in rows {
            let (id, content, text) = row?;
            let stored: Option<String> = self.conn.query_row(
                "SELECT hash FROM entity_changes WHERE entityName='blobs' AND entityId=?", [&id], |r| r.get(0)).optional()?;
            tally("blobs", stored.as_deref() == Some(&hash::blob_hash(&id, &content, text.as_deref())));
        }
        Ok(out)
    }

    /// Per-entity, per-sector hashes exactly as `content_hash.ts::getEntityHashes`.
    pub fn sector_hashes(&self) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let mut stmt = self.conn.prepare(
            "SELECT entityName, entityId, hash, isErased FROM entity_changes WHERE isSynced=1 AND entityName != 'note_reordering'
             ORDER BY entityId")?;
        let mut acc: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
            let (name, id, h, erased) = row?;
            let sector: String = id.chars().take(1).collect();
            acc.entry(name).or_default().entry(sector).or_default().push_str(&format!("{h}{erased}"));
        }
        for sectors in acc.values_mut() {
            for v in sectors.values_mut() { *v = hash::hash_full(v); }
        }
        Ok(acc)
    }
}

/// Re-queue an existing entity change so the next push sends it again (entity_changes.ts `requeueEntityChange`):
/// new changeId, stamped with our own instance (changes stamped with the server's id are never pushed back to it),
/// new id so it sorts after `lastSyncedPush`.
fn requeue_entity_change(conn: &Connection, local_instance: &str, name: &str, id: &str) -> Result<()> {
    if local_instance.is_empty() { return Ok(()); }
    let row: Option<(String, i64, String, i64, String)> = conn.query_row(
        "SELECT hash,isErased,componentId,isSynced,utcDateChanged FROM entity_changes WHERE entityName=? AND entityId=?",
        [name, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    let Some((hash, erased, component, synced, date)) = row else { return Ok(()) };
    conn.execute(
        "INSERT OR REPLACE INTO entity_changes(entityName,entityId,hash,isErased,changeId,componentId,instanceId,isSynced,utcDateChanged) VALUES(?,?,?,?,?,?,?,?,?)",
        params![name, id, hash, erased, crate::write::new_entity_id(), component, local_instance, synced, date])?;
    Ok(())
}

impl Db {
    /// Queue every entity change of one hash sector for sending again (entity_changes.ts `addEntityChangesForSector`);
    /// for blobs also the notes, attachments and revisions that point at them. Returns how many were queued.
    pub fn requeue_sector(&mut self, entity: &str, sector: &str) -> Result<usize> {
        let local_instance = self.instance_id()?;
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut targets: Vec<(String, String)> = Vec::new();
        if entity == "blobs" {
            for (table, pk) in [("notes", "noteId"), ("attachments", "attachmentId"), ("revisions", "revisionId")] {
                let mut st = tx.prepare(&format!(
                    "SELECT dep.entityName, dep.entityId FROM entity_changes o JOIN {table} t ON t.blobId=o.entityId
                     JOIN entity_changes dep ON dep.entityName='{table}' AND dep.entityId=t.{pk}
                     WHERE o.entityName='blobs' AND SUBSTR(o.entityId,1,1)=?"))?;
                targets.extend(st.query_map([sector], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?);
            }
        }
        {
            let mut st = tx.prepare("SELECT entityName, entityId FROM entity_changes WHERE entityName=? AND SUBSTR(entityId,1,1)=?")?;
            targets.extend(st.query_map([entity, sector], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        for (n, i) in &targets { requeue_entity_change(&tx, &local_instance, n, i)?; }
        tx.commit()?;
        Ok(targets.len())
    }
}

fn as_int(v: &Value) -> i64 {
    match v { Value::Bool(b) => *b as i64, Value::Number(n) => n.as_i64().unwrap_or(0), _ => 0 }
}

thread_local! { static COLUMNS: std::cell::RefCell<std::collections::HashMap<String, std::collections::HashSet<String>>> = Default::default(); }

/// Columns our schema has for `table`. A newer server may send columns we don't know; storing them would fail the
/// whole batch on every pull, so they are skipped (writes are already refused while the server is newer).
fn known_columns(tx: &rusqlite::Transaction, table: &str) -> Result<std::collections::HashSet<String>> {
    if let Some(c) = COLUMNS.with(|m| m.borrow().get(table).cloned()) { return Ok(c); }
    let mut st = tx.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let cols: std::collections::HashSet<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    COLUMNS.with(|m| m.borrow_mut().insert(table.to_string(), cols.clone()));
    Ok(cols)
}

thread_local! { static INT_COLUMNS: std::cell::RefCell<std::collections::HashMap<String, std::collections::HashSet<String>>> = Default::default(); }

/// Columns declared INTEGER: a text value there would make later reads fail (`Model::load` reads them as numbers).
fn integer_columns(tx: &rusqlite::Transaction, table: &str) -> Result<std::collections::HashSet<String>> {
    if let Some(c) = INT_COLUMNS.with(|m| m.borrow().get(table).cloned()) { return Ok(c); }
    let mut st = tx.prepare(&format!("SELECT name FROM pragma_table_info('{table}') WHERE upper(type) LIKE '%INT%'"))?;
    let cols: std::collections::HashSet<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    INT_COLUMNS.with(|m| m.borrow_mut().insert(table.to_string(), cols.clone()));
    Ok(cols)
}

fn replace_row(tx: &rusqlite::Transaction, table: &str, row: &serde_json::Map<String, Value>) -> Result<()> {
    let known = known_columns(tx, table)?;
    let ints = integer_columns(tx, table)?;
    let mut cols = Vec::new();
    let mut vals: Vec<Sql> = Vec::new();
    for (k, v) in row {
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { bail!("bad column name {k}"); }
        if !known.contains(k.as_str()) { continue; }
        cols.push(format!("\"{k}\""));
        vals.push(match v {
            Value::Null => Sql::Null,
            Value::Bool(b) => Sql::Integer(*b as i64),
            Value::Number(n) => n.as_i64().map(Sql::Integer).unwrap_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0))),
            // blob content travels base64-encoded; Trilium stores it as a BLOB
            // sync_update.ts preProcessContent: an empty buffer is stored as the empty string, not as an empty BLOB
            Value::String(s) if table == "blobs" && k == "content" => match B64.decode(s)? { b if b.is_empty() => Sql::Text(String::new()), b => Sql::Blob(b) },
            Value::String(s) if ints.contains(k.as_str()) => match s.trim().parse::<i64>() { Ok(n) => Sql::Integer(n), Err(_) => bail!("{table}.{k} must be a number, got {s:?}") },
            Value::String(s) => Sql::Text(s.clone()),
            other => bail!("unexpected JSON in {table}.{k}: {other}"),
        });
    }
    let sql = format!("INSERT OR REPLACE INTO {table}({}) VALUES({})", cols.join(","), vec!["?"; cols.len()].join(","));
    tx.execute(&sql, params_from_iter(vals)).with_context(|| format!("writing {table}"))?;
    Ok(())
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use serde_json::json;

    fn db() -> (tempfile::TempDir, Db, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let db = Db::open(&path).unwrap();
        db.set_state("instanceId", "local").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db, path)
    }

    #[test]
    fn newer_server_database_makes_replica_read_only() {
        let (_d, db, _p) = db();
        db.create_note("root", "ok", "text", "text/html", "").unwrap();
        db.set_state("serverDbVersion", &(KNOWN_DB_VERSION + 1).to_string()).unwrap();
        let e = db.create_note("root", "nope", "text", "text/html", "").unwrap_err().to_string();
        assert!(e.contains("read-only"), "{e}");
        db.set_state("serverDbVersion", &KNOWN_DB_VERSION.to_string()).unwrap();
        db.create_note("root", "fine again", "text", "text/html", "").unwrap();
    }

    #[test]
    fn daily_backup_is_created_once_rotated_and_restorable() {
        let (dir, db, path) = db();
        let n = db.create_note("root", "keep me", "text", "text/html", "x").unwrap();
        let first = db.backup_daily(&path, 3).unwrap().expect("first backup");
        assert!(db.backup_daily(&path, 3).unwrap().is_none(), "only once per day");
        // old backups are pruned to `keep`
        for d in ["2020-01-01", "2020-01-02", "2020-01-03"] { std::fs::write(dir.path().join(format!("s.bak-{d}.db")), b"old").unwrap(); }
        std::fs::remove_file(&first).unwrap();
        db.backup_daily(&path, 3).unwrap().unwrap();
        let baks = std::fs::read_dir(dir.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".bak-")).count();
        assert_eq!(baks, 3);
        let restored = Db::open(&first).unwrap();
        assert_eq!(restored.note_content(&n).unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn unpushed_local_edit_newer_than_remote_is_reported_as_conflict() {
        let (_d, mut db, _p) = db();
        let n = db.create_note("root", "mine", "text", "text/html", "").unwrap();
        // remote change for the same note, older than our local edit and different
        let rec = json!({"entityChange": {"entityName": "notes", "entityId": n, "hash": "remotehash", "isErased": 0, "changeId": "remotechg001",
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2001-01-01 00:00:00.000Z"},
            "entity": {"noteId": n, "title": "theirs", "isProtected": 0, "type": "text", "mime": "text/html", "isDeleted": 0,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": "2001-01-01 00:00:00.000Z", "utcDateModified": "2001-01-01 00:00:00.000Z"}});
        let stats = db.apply_changes("server", &[rec]).unwrap();
        assert_eq!(stats.conflicts, vec![("notes".to_string(), n.clone())]);
        let title: String = db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        assert_eq!(title, "mine", "local newer edit is kept");
    }

    #[test]
    fn newer_remote_change_replaces_an_unpushed_edit_but_keeps_the_old_text() {
        let (d, mut db, _p) = db();
        let n = db.create_note("root", "mine", "text", "text/html", "<p>my unsynced words</p>").unwrap();
        let rec = |date: &str, hash: &str| json!({"entityChange": {"entityName": "notes", "entityId": n, "hash": hash, "isErased": 0, "changeId": format!("chg{hash}"),
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": date},
            "entity": {"noteId": n, "title": "theirs", "isProtected": 0, "type": "text", "mime": "text/html", "isDeleted": 0, "blobId": null,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": "2001-01-01 00:00:00.000Z", "utcDateModified": "2001-01-01 00:00:00.000Z"}});
        let stats = db.apply_changes("server", &[rec("2999-01-01 00:00:00.000Z", "remote1")]).unwrap();
        assert_eq!(stats.overwritten.len(), 1);
        assert_eq!(stats.overwritten[0].2, "mine");
        assert_eq!(stats.overwritten[0].3.as_deref(), Some(&b"<p>my unsynced words</p>"[..]));
        let out = db.save_overwritten(&stats.overwritten).unwrap().expect("saved");
        let files: Vec<_> = std::fs::read_dir(&out).unwrap().collect();
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(files[0].as_ref().unwrap().path()).unwrap(), b"<p>my unsynced words</p>");
        assert!(std::fs::read_to_string(d.path().join("conflicts.log")).unwrap().contains("overwritten-by-remote"));
        // once our change counts as pushed, a later remote change is ordinary and not reported
        let last: i64 = db.conn.query_row("SELECT MAX(id) FROM entity_changes", [], |r| r.get(0)).unwrap();
        db.set_state("lastSyncedPush", &last.to_string()).unwrap();
        let stats = db.apply_changes("server", &[rec("2999-02-01 00:00:00.000Z", "remote2")]).unwrap();
        assert!(stats.overwritten.is_empty() && stats.conflicts.is_empty());
    }

    fn note_rec(id: &str, hash: &str, date: &str, change: &str) -> Value {
        json!({"entityChange": {"entityName": "notes", "entityId": id, "hash": hash, "isErased": 0, "changeId": change,
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": date},
            "entity": {"noteId": id, "title": id, "isProtected": 0, "type": "text", "mime": "text/html", "isDeleted": 0, "blobId": null,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": date, "utcDateModified": date}})
    }

    fn ec_row(db: &Db, name: &str, id: &str) -> (i64, String, String, String) {
        db.conn.query_row("SELECT id,changeId,instanceId,hash FROM entity_changes WHERE entityName=? AND entityId=?", [name, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
    }

    #[test]
    fn a_newer_local_entry_that_differs_is_queued_for_the_server_again() {
        // sync_update.ts: putEntityChangeForOtherInstances, new changeId and our own instance so that push sends it
        let (_d, mut db, _p) = db();
        db.apply_changes("server", &[note_rec("n1", "h-new", "2031-01-01 00:00:00.000Z", "c-new")]).unwrap();
        let (id0, change0, inst0, _) = ec_row(&db, "notes", "n1");
        assert_eq!(inst0, "server");
        // an older, different version arrives (server restored from a backup)
        let st = db.apply_changes("server", &[note_rec("n1", "h-old", "2030-01-01 00:00:00.000Z", "c-old")]).unwrap();
        assert_eq!((st.applied, st.local_newer), (0, 1));
        let (id1, change1, inst1, hash1) = ec_row(&db, "notes", "n1");
        assert_eq!((inst1.as_str(), hash1.as_str()), ("local", "h-new"), "ours stays, stamped with our instance");
        assert_ne!(change1, change0);
        assert!(id1 > id0, "a new id, after lastSyncedPush");
        // the same hash on both sides needs no re-announcement
        db.apply_changes("server", &[note_rec("n1", "h-new", "2030-01-01 00:00:00.000Z", "c-same")]).unwrap();
        assert_eq!(ec_row(&db, "notes", "n1").0, id1);
    }

    #[test]
    fn a_note_reordering_is_applied_whatever_the_dates() {
        let (_d, mut db, _p) = db();
        db.conn.execute("INSERT INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES('p_a','a','p',10,NULL,0,0,'x')", []).unwrap();
        db.conn.execute("INSERT INTO entity_changes(entityName,entityId,hash,isErased,changeId,componentId,instanceId,isSynced,utcDateChanged)
            VALUES('note_reordering','p','N/A',0,'local-reorder','x','local',1,'2099-01-01 00:00:00.000Z')", []).unwrap();
        let rec = json!({"entityChange": {"entityName": "note_reordering", "entityId": "p", "hash": "N/A", "isErased": 0, "changeId": "remote-reorder",
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2020-01-01 00:00:00.000Z"}, "entity": {"p_a": 50}});
        let st = db.apply_changes("server", &[rec]).unwrap();
        assert_eq!((st.applied, st.local_newer), (1, 0));
        let pos: i64 = db.conn.query_row("SELECT notePosition FROM branches WHERE branchId='p_a'", [], |r| r.get(0)).unwrap();
        assert_eq!(pos, 50);
    }

    #[test]
    fn an_entry_without_a_date_is_not_overwritten_like_trilium() {
        // localECIsOlderOrSameAsRemote needs both dates; otherwise the local entry stands
        let (_d, mut db, _p) = db();
        db.apply_changes("server", &[note_rec("n2", "h1", "2030-01-01 00:00:00.000Z", "c1")]).unwrap();
        db.conn.execute("UPDATE entity_changes SET utcDateChanged='' WHERE entityId='n2'", []).unwrap();
        let st = db.apply_changes("server", &[note_rec("n2", "h2", "2031-01-01 00:00:00.000Z", "c2")]).unwrap();
        assert_eq!((st.applied, st.local_newer), (0, 1));
    }

    #[test]
    fn requeueing_a_sector_restamps_its_entries_and_a_blobs_dependants() {
        let (_d, mut db, _p) = db();
        let mut a = note_rec("xa1", "ha", "2030-01-01 00:00:00.000Z", "ca");
        a["entity"]["blobId"] = json!("xblob1");
        let blob = json!({"entityChange": {"entityName": "blobs", "entityId": "xblob1", "hash": "hb", "isErased": 0, "changeId": "cb",
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"},
            "entity": {"blobId": "xblob1", "content": "", "dateModified": "x", "utcDateModified": "2030-01-01 00:00:00.000Z"}});
        db.apply_changes("server", &[a, blob, note_rec("yb1", "hc", "2030-01-01 00:00:00.000Z", "cc")]).unwrap();
        let before = ec_row(&db, "notes", "xa1");
        let n = db.requeue_sector("blobs", "x").unwrap();
        assert_eq!(n, 2, "the blob and the note that points at it");
        let after = ec_row(&db, "notes", "xa1");
        assert_eq!(after.2, "local");
        assert!(after.0 > before.0 && after.1 != before.1);
        assert_eq!(ec_row(&db, "blobs", "xblob1").2, "local");
        assert_eq!(ec_row(&db, "notes", "yb1").2, "server", "another sector is left alone");
    }

    #[test]
    fn an_erase_of_an_option_is_recorded_but_deletes_nothing() {
        let (_d, mut db, _p) = db();
        db.conn.execute("INSERT INTO options(name,value,isSynced,utcDateModified) VALUES('keepme','v',1,'x')", []).unwrap();
        let rec = json!({"entityChange": {"entityName": "options", "entityId": "keepme", "hash": "", "isErased": 1, "changeId": "e1",
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"}});
        db.apply_changes("server", &[rec]).unwrap();
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM options WHERE name='keepme'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn an_empty_pulled_blob_is_stored_like_trilium_does() {
        let (_d, mut db, _p) = db();
        let rec = |id: &str, content: &str| json!({"entityChange": {"entityName": "blobs", "entityId": id, "hash": "h", "isErased": 0, "changeId": format!("c-{id}"),
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"},
            "entity": {"blobId": id, "content": content, "dateModified": "x", "utcDateModified": "2030-01-01 00:00:00.000Z"}});
        db.apply_changes("server", &[rec("emptyblob", ""), rec("fullblob", "aGk=")]).unwrap();
        let ty = |id: &str| -> String { db.conn.query_row("SELECT typeof(content) FROM blobs WHERE blobId=?", [id], |r| r.get(0)).unwrap() };
        assert_eq!((ty("emptyblob").as_str(), ty("fullblob").as_str()), ("text", "blob"));
    }

    #[test]
    fn records_with_an_empty_change_id_are_all_applied() {
        // long-lived servers hold thousands of old entity changes with changeId ""; they must not shadow each other
        let (_d, mut db, _p) = db();
        let rec = |id: &str| json!({"entityChange": {"entityName": "notes", "entityId": id, "hash": "h", "isErased": 0, "changeId": "",
            "componentId": "x", "instanceId": "", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"},
            "entity": {"noteId": id, "title": id, "isProtected": 0, "type": "text", "mime": "text/html", "isDeleted": 0, "blobId": null,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": "2030-01-01 00:00:00.000Z", "utcDateModified": "2030-01-01 00:00:00.000Z"}});
        let stats = db.apply_changes("server", &[rec("emptyA"), rec("emptyB"), rec("emptyC")]).unwrap();
        assert_eq!(stats.skipped_known, 0);
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM notes WHERE noteId LIKE 'empty%'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn unknown_columns_from_a_newer_server_are_skipped_not_fatal() {
        let (_d, mut db, _p) = db();
        let rec = json!({"entityChange": {"entityName": "notes", "entityId": "newnote", "hash": "h", "isErased": 0, "changeId": "chgX",
            "componentId": "x", "instanceId": "server", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"},
            "entity": {"noteId": "newnote", "title": "from the future", "isProtected": 0, "type": "text", "mime": "text/html", "isDeleted": 0, "blobId": null,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": "2030-01-01 00:00:00.000Z", "utcDateModified": "2030-01-01 00:00:00.000Z", "someNewColumn": "v9"}});
        db.apply_changes("server", &[rec]).unwrap();
        let t: String = db.conn.query_row("SELECT title FROM notes WHERE noteId='newnote'", [], |r| r.get(0)).unwrap();
        assert_eq!(t, "from the future");
    }

    #[test]
    #[cfg(unix)]
    fn a_new_database_and_its_wal_are_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let (d, db, p) = db();
        db.set_state("documentSecret", "s3cret").unwrap();
        for f in ["s.db", "s.db-wal", "s.db-shm"] {
            let path = d.path().join(f);
            if path.exists() { assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0, "{f} must not be group/world accessible"); }
        }
        let _ = p;
    }

    #[test]
    fn only_dated_automatic_backups_are_listed_and_deleted() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("s.db")).unwrap();
        for f in ["s.bak-2026-01-01.db", "s.bak-2026-02-30.db", "s.bak-mine-before-upgrade.db", "s.bak-2026-01-01.db-wal", "other.bak-2026-01-01.db", "s.bak-2026-1-1.db"] {
            std::fs::write(d.path().join(f), b"x").unwrap();
        }
        let names: Vec<String> = db.backups().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["s.bak-2026-01-01.db"]);
        assert_eq!(db.delete_backups(), 1);
        assert!(d.path().join("s.bak-mine-before-upgrade.db").exists() && d.path().join("other.bak-2026-01-01.db").exists());
    }

    #[test]
    fn malformed_and_unknown_remote_records_do_not_block_the_batch() {
        let (_d, mut db, _p) = db();
        let rec = |name: &str, id: &str, hash: &str, entity: Value| json!({"entityChange": {"entityName": name, "entityId": id, "changeId": format!("c-{id}"), "hash": hash, "isErased": 0,
            "componentId": "x", "isSynced": 1, "utcDateChanged": "2030-01-01 00:00:00.000Z"}, "entity": entity});
        let recs = vec![
            rec("widgets", "w1", "h", json!({"widgetId": "w1"})),
            rec("notes", "bad1", "h", json!({"noteId": "bad1", "title": "t", "isProtected": "abc", "type": "text", "mime": "text/html", "isDeleted": 0,
                "dateCreated": "x", "dateModified": "x", "utcDateCreated": "x", "utcDateModified": "x"})),
            rec("options", "good", "h", json!({"name": "good", "value": "v", "isSynced": 1, "utcDateModified": "2030-01-01 00:00:00.000Z"})),
        ];
        let st = db.apply_changes("server", &recs).unwrap();
        assert_eq!(st.applied, 1, "only the good record is applied");
        assert!(st.conflicts.iter().any(|(e, m)| e == "notes" && m.contains("isProtected")), "{:?}", st.conflicts);
        let n: i64 = db.conn.query_row("SELECT COUNT(*) FROM notes WHERE noteId='bad1'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "the malformed note never reached the table");
        assert!(crate::model::Model::load(&db).is_ok());
        let v: String = db.conn.query_row("SELECT value FROM options WHERE name='good'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "v");
    }

    #[test]
    fn a_blob_the_server_may_have_dropped_is_announced_again_when_reused() {
        let (_d, db, _p) = db();
        let n = db.create_note("root", "T", "text", "text/html", "<p>v1</p>").unwrap();
        let blob: String = db.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        db.set_note_content(&n, "<p>v2</p>").unwrap();
        // v1's blob was dropped as unused; pretend its change row is old and the server forgot it
        db.conn.execute("INSERT OR IGNORE INTO blobs(blobId,content,dateModified,utcDateModified) VALUES(?,?,'x','2000-01-01 00:00:00.000Z')", params![blob, "<p>v1</p>"]).unwrap();
        db.conn.execute("UPDATE entity_changes SET utcDateChanged='2000-01-01 00:00:00.000Z' WHERE entityName='blobs' AND entityId=?", [&blob]).unwrap();
        let before: String = db.conn.query_row("SELECT changeId FROM entity_changes WHERE entityName='blobs' AND entityId=?", [&blob], |r| r.get(0)).unwrap_or_default();
        db.set_note_content(&n, "<p>v1</p>").unwrap();
        let (after, date): (String, String) = db.conn.query_row("SELECT changeId,utcDateChanged FROM entity_changes WHERE entityName='blobs' AND entityId=?", [&blob], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_ne!(after, before, "re-used orphan blob gets a fresh change");
        assert!(date.as_str() > "2020", "{date}");
    }

    #[test]
    fn a_remote_erase_keeps_a_blob_a_local_row_still_uses() {
        let (_d, mut db, _p) = db();
        let n = db.create_note("root", "T", "text", "text/html", "<p>keep</p>").unwrap();
        let blob: String = db.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [&n], |r| r.get(0)).unwrap();
        let rec = json!({"entityChange": {"entityName": "blobs", "entityId": blob, "changeId": "erase-1", "hash": "", "isErased": 1, "componentId": "x", "isSynced": 1, "utcDateChanged": "2099-01-01 00:00:00.000Z"}});
        db.apply_changes("server", &[rec]).unwrap();
        assert_eq!(db.note_content(&n).unwrap().unwrap(), "<p>keep</p>");
    }
}
