//! Randomised stress test of the write path: thousands of random operations, and after every one the
//! change log must still verify (hashes, entity_changes) and the tree must stay structurally sound.
use std::collections::HashSet;
use trilium_core::{db::Db, model::Model};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); self.0 >> 33 }
    fn below(&mut self, n: usize) -> usize { (self.next() as usize) % n.max(1) }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> { if v.is_empty() { None } else { Some(&v[self.below(v.len())]) } }
}

fn setup(path: &std::path::Path) -> Db {
    let db = Db::open(path).unwrap();
    db.set_state("instanceId", "stress").unwrap();
    db.ensure_root().unwrap();
    db.set_option("revisionSnapshotTimeInterval", "0", true).unwrap();
    db
}

fn live_notes(db: &Db) -> Vec<String> {
    let mut st = db.conn.prepare("SELECT noteId FROM notes WHERE isDeleted=0 AND noteId != 'root'").unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

fn live_branches(db: &Db) -> Vec<(String, String, String)> {
    let mut st = db.conn.prepare("SELECT branchId,noteId,parentNoteId FROM branches WHERE isDeleted=0").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect()
}

fn check_invariants(db: &Db, step: usize, op: &str) {
    for (name, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "step {step} after {op}: {name} hashes/change log inconsistent"); }
    let notes: HashSet<String> = { let mut s: HashSet<String> = live_notes(db).into_iter().collect(); s.insert("root".into()); s };
    let branches = live_branches(db);
    for (b, n, p) in &branches {
        assert!(notes.contains(n), "step {step} after {op}: branch {b} points at deleted/missing note {n}");
        assert!(notes.contains(p), "step {step} after {op}: branch {b} has deleted/missing parent {p}");
    }
    let placed: HashSet<&String> = branches.iter().map(|(_, n, _)| n).collect();
    for n in notes.iter().filter(|n| *n != "root") { assert!(placed.contains(n), "step {step} after {op}: live note {n} has no live branch"); }
    // no cycles: every note reaches root
    let m = Model::load(db).unwrap();
    for n in notes.iter().filter(|n| *n != "root") {
        let mut seen = HashSet::new();
        let mut stack = vec![n.clone()];
        let mut reached = false;
        while let Some(x) = stack.pop() {
            if x == "root" { reached = true; break; }
            if seen.insert(x.clone()) { stack.extend(m.parents.get(&x).cloned().unwrap_or_default()); }
        }
        assert!(reached, "step {step} after {op}: note {n} is not reachable from root (cycle or orphan)");
    }
    // deleted notes keep no live attributes
    let stale: i64 = db.conn.query_row("SELECT COUNT(*) FROM attributes a JOIN notes n ON n.noteId=a.noteId WHERE a.isDeleted=0 AND n.isDeleted=1", [], |r| r.get(0)).unwrap();
    assert_eq!(stale, 0, "step {step} after {op}: live attributes on deleted notes");
}

fn run(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = setup(&dir.path().join("s.db"));
    let mut rng = Rng(seed);
    let words = ["alpha", "beta", "gamma", "delta", "ünï", "日本語", "x&y", "<tag>"];
    for step in 0..steps {
        let notes = live_notes(&db);
        let parent = rng.pick(&notes).cloned().filter(|_| rng.below(4) != 0).unwrap_or_else(|| "root".into());
        let note = rng.pick(&notes).cloned();
        let branches = live_branches(&db);
        let op = rng.below(21);
        let w = words[rng.below(words.len())];
        let name = match op {
            0 | 1 | 2 => { db.create_note(&parent, &format!("{w} {step}"), "text", "text/html", &format!("<p>{w} body {step}</p>")).map(|_| ()).map_err(|e| e.to_string()).ok(); "create" }
            3 => { if let Some(n) = &note { let _ = db.set_note_title(n, &format!("{w} renamed {step}")); } "rename" }
            4 | 5 => { if let Some(n) = &note { let link = rng.pick(&notes).map(|t| format!(r##"<a href="#root/{t}">l</a>"##)).unwrap_or_default(); let _ = db.set_note_content(n, &format!("<p>{w} {step}</p>{link}")); } "set content" }
            6 => { if let Some((b, ..)) = rng.pick(&branches) { let _ = db.delete_branch(b); } "delete" }
            7 => { if let Some((b, ..)) = rng.pick(&branches) { let _ = db.move_branch_up(b); } "move up" }
            8 => { if let Some((b, ..)) = rng.pick(&branches) { let _ = db.move_branch_down(b); } "move down" }
            9 => { if let Some((b, ..)) = rng.pick(&branches) { let _ = db.move_branch_in(b); } "move in" }
            10 => { if let Some((b, ..)) = rng.pick(&branches) { let _ = db.move_branch_out(b); } "move out" }
            11 => { if let Some(n) = &note { let _ = db.clone_note(n, &parent, None); } "clone" }
            12 => { if let (Some((b, ..)), Some(t)) = (rng.pick(&branches), rng.pick(&notes)) { let _ = db.move_branch_to(b, t); } "move to" }
            13 => { if let Some(n) = &note { let _ = db.add_attribute(n, if rng.below(2) == 0 { "label" } else { "relation" }, "k", &if rng.below(2) == 0 { w.to_string() } else { rng.pick(&notes).cloned().unwrap_or_default() }, rng.below(2) == 0); } "add attribute" }
            14 => { if let Some(n) = &note { let rows = db.attribute_rows(n).unwrap(); if let Some(a) = rng.pick(&rows) { if a.owner == *n { let _ = if rng.below(2) == 0 { db.delete_attribute(&a.id) } else { db.set_attribute_value(&a.id, w) }; } } } "edit attribute" }
            15 => { if let Some(n) = &note { let _ = db.duplicate_subtree(n, &parent, rng.below(2) == 0); } "duplicate" }
            16 => { if let Some(n) = &note { if let Ok(revs) = db.revisions(n) { if let Some(r) = rng.pick(&revs) { let _ = db.restore_revision(&r.id); } } } "restore revision" }
            18 => { let bytes: Vec<u8> = (0..rng.below(300)).map(|i| (i * 7 + step) as u8).collect(); let _ = db.create_file_note(&parent, &format!("file{step}.bin"), "application/octet-stream", &bytes); "create file note" }
            19 => { if let Some(n) = &note { let _ = db.set_note_content(n, &format!("append {w}")); let _ = db.set_note_type(n, if rng.below(2) == 0 { "code" } else { "text" }, if rng.below(2) == 0 { "text/plain" } else { "text/html" }); } "retype" }
            20 => { if let Some(n) = &note { let _ = db.move_branch_to(&db.first_branch_of(n).ok().flatten().unwrap_or_default(), "NO_SUCH_PARENT"); } "move to missing parent" }
            _ => { if let Some(n) = &note { let _ = db.add_attachment(n, &format!("f{step}.bin"), "application/octet-stream", "file", &[step as u8, 0, 255]); } "attach" }
        };
        check_invariants(&db, step, name);
    }
    // finally the whole tree survives an export/import round trip with consistent bookkeeping
    let top = db.create_note("root", "everything", "text", "text/html", "").unwrap();
    for n in live_notes(&db).iter().filter(|n| **n != top).take(40) { let _ = db.clone_note(n, &top, None); }
    let mut buf = std::io::Cursor::new(vec![]);
    db.export_zip(&top, &mut buf).unwrap();
    let target = db.create_note("root", "import target", "text", "text/html", "").unwrap();
    let entries = trilium_core::transfer::read_zip(std::io::Cursor::new(buf.into_inner())).unwrap();
    db.import_entries(entries, &target, &Default::default()).unwrap();
    check_invariants(&db, steps, "export/import");
}

#[test]
fn random_operations_keep_the_change_log_and_tree_sound() {
    for seed in [1u64, 7, 42, 20261012, 987654321] { run(seed, 250); }
}
