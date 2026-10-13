//! bench <db>: time corpus build and a range of searches.
use std::time::Instant;
use trilium_core::{db::Db, search::{SearchOpts, Searcher}};
fn main() -> anyhow::Result<()> {
    let db = Db::open(std::path::Path::new(&std::env::args().nth(1).unwrap()))?;
    let t = Instant::now(); let s = Searcher::new(&db)?; println!("corpus build: {:?} ({} notes)", t.elapsed(), s.corpus().len());
    for q in ["topic42", "unique-99999", "zzzznotfound", "note 5000", "lorem ipsum", "#flagged", "#topic = t42", "#topic=t42 and #flagged",
        "#topic *=* 4 orderBy note.title limit 5", "note.title *=* 'note 9999'", "note.content *=* unique-12345", "#flagged lorem", "=topic3"] {
        let t = Instant::now();
        let o = s.search(&db, q, &SearchOpts { limit: Some(50), ..Default::default() })?;
        println!("{:>8.1?}  {:>5} hits  {q:?} {}", t.elapsed(), o.hits.len(), o.error.map(|e| format!("ERROR {e}")).unwrap_or_default());
    }
    Ok(())
}
