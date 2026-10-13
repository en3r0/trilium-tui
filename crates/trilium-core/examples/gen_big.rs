//! Generate a synthetic large database for performance testing: gen_big <db> <notes>
use trilium_core::db::Db;
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let n: usize = a[2].parse()?;
    let db = Db::open(std::path::Path::new(&a[1]))?;
    db.set_state("instanceId", "genbig")?;
    db.conn.execute("INSERT OR IGNORE INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
        VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", [])?;
    let body = "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(20) + "</p>";
    let (mut tops, mut mids): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for i in 0..n {
        let parent = match i % 1000 {
            0..=9 => "root".to_string(),
            10..=109 => tops[i % tops.len().max(1)].clone(),
            _ => mids[i % mids.len().max(1)].clone(),
        };
        let id = db.create_note(&parent, &format!("Note {i} topic{}", i % 97), "text", "text/html", &format!("{body}<p>unique-{i}</p>"))?;
        if i % 10 == 0 { db.add_attribute(&id, "label", "topic", &format!("t{}", i % 97), false)?; }
        if i % 500 == 0 { db.add_attribute(&id, "label", "flagged", "", false)?; }
        match i % 1000 { 0..=9 => tops.push(id), 10..=109 => mids.push(id), _ => {} }
    }
    Ok(())
}
