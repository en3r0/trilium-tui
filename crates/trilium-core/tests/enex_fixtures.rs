//! Evernote export samples taken from Trilium's own importer test-suite, run against the Rust importer.
use trilium_core::db::Db;

fn fixture(name: &str) -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("e.db")).unwrap();
    db.set_state("instanceId", "t").unwrap();
    db.ensure_root().unwrap();
    let path = format!("{}/tests/fixtures/enex/{name}.enex", env!("CARGO_MANIFEST_DIR"));
    let stats = db.import_path(std::path::Path::new(&path), "root", &Default::default()).unwrap();
    let root = stats.root.unwrap();
    (dir, db, root)
}

fn child(db: &Db, parent: &str, title: &str) -> String {
    db.conn.query_row(
        "SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0 AND trim(n.title)=?",
        [parent, title], |r| r.get(0)).unwrap_or_else(|_| panic!("no child '{title}'"))
}

fn text(db: &Db, id: &str) -> String { db.note_content(id).unwrap().unwrap() }

#[test]
fn strays_before_the_first_note_are_ignored() {
    let (_d, db, root) = fixture("Stray elements");
    let n = child(&db, &root, "After strays");
    assert!(text(&db, &n).contains("Body"));
    let atts: i64 = db.conn.query_row("SELECT COUNT(*) FROM attachments WHERE ownerId=?", [&n], |r| r.get(0)).unwrap();
    let attrs: i64 = db.conn.query_row("SELECT COUNT(*) FROM attributes WHERE noteId=? AND isDeleted=0", [&n], |r| r.get(0)).unwrap();
    assert_eq!((atts, attrs), (0, 0));
}

#[test]
fn internal_links_resolve_by_title_unless_ambiguous() {
    let (_d, db, root) = fixture("Internal links");
    let (first, second) = (child(&db, &root, "First Note"), child(&db, &root, "Second Note"));
    let c = text(&db, &first);
    assert!(c.contains(&format!(r##"href="#root/{second}""##)) && c.contains(r#"class="reference-link""#) && !c.contains("evernote://"), "{c}");
    assert!(text(&db, &second).contains(&format!(r##"href="#root/{first}""##)));
    let rel: i64 = db.conn.query_row("SELECT COUNT(*) FROM attributes WHERE noteId=? AND name='internalLink' AND value=? AND isDeleted=0", [&first, &second], |r| r.get(0)).unwrap();
    assert_eq!(rel, 1);

    let (_d, db, root) = fixture("Duplicate titles");
    let (linker, target) = (child(&db, &root, "Linker"), child(&db, &root, "Target"));
    let c = text(&db, &linker);
    assert!(c.contains(r#"href="evernote://view-note/dddddddd-dddd-dddd-dddd-dddddddddddd""#) && c.contains(&format!(r##"href="#root/{target}""##)), "{c}");

    let (_d, db, root) = fixture("Internal links whitespace");
    let (linker, padded) = (child(&db, &root, "Linker"), child(&db, &root, "Padded Note"));
    let c = text(&db, &linker);
    assert!(c.contains(&format!(r##"href="#root/{padded}""##)) && !c.contains("evernote://"), "{c}");
}

#[test]
fn legacy_checkboxes_become_todo_lists() {
    let (_d, db, root) = fixture("Legacy checkboxes");
    let kids: Vec<String> = db.conn.prepare("SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0").unwrap()
        .query_map([&root], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    let c = text(&db, &kids[0]);
    assert!(c.contains(r#"<ul class="todo-list">"#) && !c.contains('\u{2610}') && !c.contains('\u{2611}'), "{c}");
    assert!(c.contains(r#"<span class="todo-list__label__description">Write tests</span>"#), "{c}");
    assert!(c.contains(r#"<span class="todo-list__label__description">Ship <i>it</i></span>"#), "{c}");
    assert!(c.contains(r#"checked="checked""#));
}

#[test]
fn attachments_labels_and_images_from_samples() {
    let (_d, db, root) = fixture("File with attachments");
    let n = child(&db, &root, "TEST1");
    let atts: Vec<(String, String)> = db.conn.prepare("SELECT title,mime FROM attachments WHERE ownerId=? AND isDeleted=0 ORDER BY position").unwrap()
        .query_map([&n], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(atts.len(), 2, "{atts:?}");
    let c = text(&db, &n);
    assert_eq!(c.matches("class=\"reference-link\" href=\"#root/").count(), 2, "{c}");
    let no_children: i64 = db.conn.query_row("SELECT COUNT(*) FROM branches WHERE parentNoteId=? AND isDeleted=0", [&n], |r| r.get(0)).unwrap();
    assert_eq!(no_children, 0);

    let (_d, db, root) = fixture("Note metadata");
    let kids: Vec<String> = db.conn.prepare("SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0").unwrap()
        .query_map([&root], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    let n = &kids[0];
    assert_eq!(db.label_value(n, "pageUrl").unwrap().as_deref(), Some("https://triliumnotes.org/page"));
    assert!(db.has_label(n, "important").unwrap() && db.has_label(n, "archive").unwrap());
    assert!(text(&db, n).contains("<img"));

    let (_d, db, _root) = fixture("Formatting");
    for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
}
