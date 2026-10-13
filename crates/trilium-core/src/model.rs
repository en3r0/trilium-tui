//! In-memory read model of the note tree, loaded from the local database.
//! Titles and branches for even very large databases fit comfortably in memory, which
//! keeps navigation, jump-to-note and tree rendering free of SQL round trips.
use crate::db::Db;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub ntype: String,
    pub mime: String,
    pub protected: bool,
    pub utc_modified: String,
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub id: String,
    pub note_id: String,
    pub parent_id: String,
    pub position: i64,
    pub prefix: Option<String>,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub struct Attribute {
    pub kind: String, // "label" | "relation"
    pub name: String,
    pub value: String,
    pub inheritable: bool,
}

#[derive(Default)]
pub struct Model {
    pub notes: HashMap<String, Note>,
    /// parent note id -> child branches sorted by position
    pub children: HashMap<String, Vec<Branch>>,
    /// child note id -> parent note ids
    pub parents: HashMap<String, Vec<String>>,
    /// note ids carrying an `#archived` label (hidden unless "show archived")
    pub archived: std::collections::HashSet<String>,
    /// branch id -> parent note id
    pub branch_parent: HashMap<String, String>,
}

pub const ROOT: &str = "root";

impl Model {
    pub fn load(db: &Db) -> Result<Self> {
        let mut m = Model::default();
        let mut stmt = db.conn.prepare("SELECT noteId,title,type,mime,isProtected,utcDateModified FROM notes WHERE isDeleted=0")?;
        for n in stmt.query_map([], |r| Ok(Note {
            id: r.get(0)?, title: r.get(1)?, ntype: r.get(2)?, mime: r.get(3)?,
            protected: r.get::<_, i64>(4)? != 0, utc_modified: r.get(5)?,
        }))? {
            let mut n = n?;
            if n.protected { n.title = db.decrypt_title(&n.title); }
            m.notes.insert(n.id.clone(), n);
        }
        let mut stmt = db.conn.prepare(
            "SELECT branchId,noteId,parentNoteId,notePosition,prefix,isExpanded FROM branches WHERE isDeleted=0 ORDER BY notePosition")?;
        for b in stmt.query_map([], |r| Ok(Branch {
            id: r.get(0)?, note_id: r.get(1)?, parent_id: r.get(2)?, position: r.get(3)?,
            prefix: r.get(4)?, expanded: r.get::<_, i64>(5)? != 0,
        }))? {
            let b = b?;
            if !m.notes.contains_key(&b.note_id) { continue; } // dangling branch
            m.branch_parent.insert(b.id.clone(), b.parent_id.clone());
            m.parents.entry(b.note_id.clone()).or_default().push(b.parent_id.clone());
            m.children.entry(b.parent_id.clone()).or_default().push(b);
        }
        let mut stmt = db.conn.prepare("SELECT DISTINCT noteId FROM attributes WHERE isDeleted=0 AND type='label' AND name='archived'")?;
        for id in stmt.query_map([], |r| r.get::<_, String>(0))? { m.archived.insert(id?); }
        Ok(m)
    }

    pub fn title_of(&self, id: &str) -> &str {
        self.notes.get(id).map(|n| n.title.as_str()).unwrap_or("?")
    }

    pub fn display_title(&self, b: &Branch) -> String {
        let t = self.title_of(&b.note_id);
        match b.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("{p} - {t}"),
            _ => t.to_string(),
        }
    }

    pub fn has_children(&self, id: &str, show_archived: bool) -> bool {
        self.children.get(id).is_some_and(|c| c.iter().any(|b| show_archived || !self.archived.contains(&b.note_id)))
    }

    /// First root-to-note path (list of note ids, excluding "root"), preferring the first parent chain.
    pub fn path_to(&self, id: &str) -> Option<Vec<String>> {
        let mut path = vec![id.to_string()];
        let mut cur = id.to_string();
        for _ in 0..256 {
            if cur == ROOT { path.reverse(); return Some(path); }
            let parent = self.parents.get(&cur)?.first()?.clone();
            if parent != ROOT { path.push(parent.clone()); }
            cur = parent;
        }
        None // cycle or absurd depth
    }
}

impl Db {
    pub fn note_attributes(&self, note_id: &str) -> Result<Vec<Attribute>> {
        let mut stmt = self.conn.prepare(
            "SELECT type,name,value,isInheritable FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
        let rows = stmt.query_map([note_id], |r| Ok(Attribute {
            kind: r.get(0)?, name: r.get(1)?, value: r.get(2)?, inheritable: r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
        }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Quick-search: every whitespace-separated token must appear in the title or the content
    /// (case-insensitive). Title hits rank first. Content scan is a plain LIKE for now.
    pub fn quick_search(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let tokens: Vec<String> = query.split_whitespace().map(|t| format!("%{}%", t.replace('%', "\\%").replace('_', "\\_"))).collect();
        if tokens.is_empty() { return Ok(vec![]); }
        let title_cond = tokens.iter().map(|_| "n.title LIKE ? ESCAPE '\\'").collect::<Vec<_>>().join(" AND ");
        let any_cond = tokens.iter().map(|_| "(n.title LIKE ? ESCAPE '\\' OR CAST(b.content AS TEXT) LIKE ? ESCAPE '\\')").collect::<Vec<_>>().join(" AND ");
        let sql = format!(
            "SELECT n.noteId, CASE WHEN {title_cond} THEN 0 ELSE 1 END AS rank FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId
             WHERE n.isDeleted=0 AND n.isProtected=0 AND ({any_cond}) ORDER BY rank, n.title LIMIT {limit}");
        let mut params: Vec<&str> = tokens.iter().map(String::as_str).collect();
        for t in &tokens { params.push(t); params.push(t); }
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_search_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("m.db")).unwrap();
        db.set_state("instanceId", "x").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        let a = db.create_note("root", "Alpha", "text", "text/html", "<p>needle here</p>").unwrap();
        let b = db.create_note(&a, "Beta", "text", "text/html", "nothing").unwrap();
        let m = Model::load(&db).unwrap();
        assert_eq!(m.path_to(&b).unwrap(), vec![a.clone(), b.clone()]);
        assert_eq!(m.children["root"].len(), 1);
        assert_eq!(db.quick_search("needle", 10).unwrap(), vec![a.clone()]);
        assert_eq!(db.quick_search("beta", 10).unwrap(), vec![b]);
        assert!(db.quick_search("zzz", 10).unwrap().is_empty());
    }
}
