use trilium_core::{db::Db, search::{SearchOpts, Searcher}};
fn main() -> anyhow::Result<()> {
    let db = Db::open(std::path::Path::new(&std::env::args().nth(1).unwrap()))?;
    let s = Searcher::new(&db)?;
    for q in ["note.type = text orderBy note.dateCreated desc", "note.type = text orderBy note.dateCreated desc limit 3", "note.type = text orderBy note.dateCreated"] {
        let o = s.search(&db, q, &SearchOpts { limit: Some(4), ..Default::default() })?;
        println!("{q}");
        for h in &o.hits { let i = s.corpus().idx[&h.note_id] as usize; println!("   {} {}", s.corpus().dates[i][0], s.corpus().title[i]); }
    }
    Ok(())
}
