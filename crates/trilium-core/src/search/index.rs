//! Persistent substring index over note text: an FTS5 trigram table keyed by blob id.
//! Blobs are content-addressed (the id is a hash of the content), so an entry can never go stale:
//! changed content means a new blob, which simply gets indexed on the next `ensure_index`.
use super::text::preprocess_content;
use crate::db::Db;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashSet;

/// Index version = fingerprint of what `preprocess_content` produces for fixed probe inputs, so any change to
/// the preprocessing rebuilds stale indexes automatically (no manual version bump to forget).
fn index_version() -> String {
    let probes: [(&str, &str, &str); 4] = [
        ("<p>A&amp;B</p><b>x</b>y<a href=\"u\">l</a></a><br>Z", "text", "text/html"),
        ("{\"elements\":[{\"type\":\"text\",\"text\":\"Hi There\"}]}", "canvas", "application/json"),
        ("{\"nodeData\":{\"topic\":\"Root\",\"children\":[{\"topic\":\"Kid\"}]}}", "mindMap", "application/json"),
        ("Caf\u{e9} CODE", "code", "text/plain"),
    ];
    let joined = probes.iter().map(|(c, t, m)| preprocess_content(c, t, m, false)).collect::<Vec<_>>().join("\u{1}");
    format!("v1-{}", crate::hash::hash10(&joined))
}
const TYPES: &str = "('text','code','mermaid','canvas','mindMap','spreadsheet','llmChat')";
const MAX_BYTES: i64 = 2 * 1024 * 1024;

/// True when FTS5 with the trigram tokenizer is available in this SQLite build.
pub fn ensure_schema(db: &Db) -> bool {
    db.conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tui_fts_map (id INTEGER PRIMARY KEY, blobId TEXT UNIQUE NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS tui_fts USING fts5(text, tokenize='trigram');").is_ok()
}

/// Index every searchable blob that isn't indexed yet. Returns how many were added.
/// Works in batches so a first build on a large database doesn't hold the write lock for long.
pub fn ensure_index(db: &Db) -> Result<usize> {
    if !ensure_schema(db) { return Ok(0); }
    let version = index_version();
    if db.state("ftsIndexVersion")?.as_deref() != Some(version.as_str()) {
        db.conn.execute_batch("DELETE FROM tui_fts; DELETE FROM tui_fts_map;")?;
        db.set_state("ftsIndexVersion", &version)?;
    }
    let mut total = 0;
    loop {
        let batch: Vec<(String, String, String)> = {
            let mut st = db.conn.prepare(&format!(
                "SELECT n.blobId, n.type, n.mime FROM notes n WHERE n.isDeleted=0 AND n.isProtected=0 AND n.blobId IS NOT NULL AND n.type IN {TYPES}
                 AND NOT EXISTS (SELECT 1 FROM tui_fts_map m WHERE m.blobId=n.blobId) GROUP BY n.blobId LIMIT 1000"))?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        if batch.is_empty() { return Ok(total); }
        let tx = db.begin()?;
        for (blob, ntype, mime) in &batch {
            let content: Option<rusqlite::types::Value> = db.conn.query_row(
                "SELECT CASE WHEN LENGTH(content) < ?2 THEN content END FROM blobs WHERE blobId=?1", params![blob, MAX_BYTES], |r| r.get(0)).ok();
            let text = match content {
                Some(rusqlite::types::Value::Text(s)) => s,
                Some(rusqlite::types::Value::Blob(b)) => String::from_utf8_lossy(&b).into_owned(),
                _ => String::new(), // too large or missing: map it anyway so it isn't retried
            };
            let text = preprocess_content(&text, ntype, mime, false);
            db.conn.execute("INSERT OR IGNORE INTO tui_fts_map(blobId) VALUES(?)", [blob])?;
            let id: i64 = db.conn.query_row("SELECT id FROM tui_fts_map WHERE blobId=?", [blob], |r| r.get(0))?;
            if !text.is_empty() { db.conn.execute("INSERT INTO tui_fts(rowid, text) VALUES(?,?)", params![id, text])?; }
        }
        tx.commit()?;
        total += batch.len();
    }
}

/// Drop index rows of blobs no note references any more.
pub fn prune_index(db: &Db) -> Result<usize> {
    if !ensure_schema(db) { return Ok(0); }
    let tx = db.begin()?;
    db.conn.execute("DELETE FROM tui_fts WHERE rowid IN (SELECT id FROM tui_fts_map WHERE blobId NOT IN (SELECT blobId FROM notes WHERE blobId IS NOT NULL))", [])?;
    let n = db.conn.execute("DELETE FROM tui_fts_map WHERE blobId NOT IN (SELECT blobId FROM notes WHERE blobId IS NOT NULL)", [])?;
    tx.commit()?;
    Ok(n)
}

/// Note ids whose indexed text contains `token` as a substring (token must be >= 3 characters).
pub fn notes_containing(db: &Db, token: &str) -> Result<HashSet<String>> {
    let q = format!("\"{}\"", token.replace('"', "\"\""));
    let mut st = db.conn.prepare(
        "SELECT n.noteId FROM tui_fts f JOIN tui_fts_map m ON m.id=f.rowid JOIN notes n ON n.blobId=m.blobId
         WHERE f.text MATCH ?1 AND n.isDeleted=0 AND n.isProtected=0")?;
    Ok(st.query_map([q], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_is_stable_and_stale_indexes_are_rebuilt() {
        assert_eq!(index_version(), index_version());
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.create_note("root", "n", "text", "text/html", "<p>needle</p>").unwrap();
        assert_eq!(ensure_index(&db).unwrap(), 1); // the note (root has no content blob)
        assert_eq!(ensure_index(&db).unwrap(), 0, "nothing new to index");
        assert_eq!(notes_containing(&db, "needle").unwrap().len(), 1);
        db.set_state("ftsIndexVersion", "old-version").unwrap(); // simulate an index from older preprocessing
        assert!(ensure_index(&db).unwrap() >= 1, "stale index rebuilt");
        assert_eq!(notes_containing(&db, "needle").unwrap().len(), 1);
    }
}
