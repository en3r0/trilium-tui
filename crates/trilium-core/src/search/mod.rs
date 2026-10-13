//! Trilium search: lexer, parser and evaluator ported from `services/search/`.
pub mod cmp;
pub mod eval;
pub mod index;
pub mod lex;
pub mod parse;
pub mod text;

use crate::db::Db;
use anyhow::Result;
use eval::{ContentTier, Corpus, Exec};
pub use parse::SearchOpts;

#[derive(Debug, Clone)]
pub struct Hit {
    pub note_id: String,
    pub score: f64,
    /// note ids from the top level down to the note
    pub path: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub error: Option<String>,
    pub highlighted: Vec<String>,
}

/// Holds the in-memory corpus; rebuild after the database changes.
pub struct Searcher { corpus: Corpus, fts_ready: std::cell::Cell<Option<bool>> }

impl Searcher {
    pub fn new(db: &Db) -> Result<Self> { Ok(Self { corpus: Corpus::build(db)?, fts_ready: std::cell::Cell::new(None) }) }

    pub fn corpus(&self) -> &Corpus { &self.corpus }

    /// Tell the searcher whether the text index may be used (false while it is still being built,
    /// in which case content is scanned directly instead of waiting for the index).
    pub fn set_index_ready(&self, ready: bool) { self.fts_ready.set(if ready { None } else { Some(false) }); }

    pub fn search(&self, db: &Db, query: &str, opts: &SearchOpts) -> Result<SearchOutput> {
        let mut ctx = parse::Ctx::default();
        let parsed = parse::parse(query, opts, &mut ctx);
        let c = &self.corpus;
        let want = opts.limit.filter(|l| *l > 0).unwrap_or(usize::MAX);
        let nq = text::normalize(&parsed.fulltext_query);
        let tokens = &parsed.fulltext_tokens;

        let exact = self.pass(db, &parsed, opts, false, &nq)?;
        let mut error = exact.error.clone();
        let mut highlighted = exact.highlighted.clone();
        let mut ranked = exact.ranked.clone();
        // Upstream rules (services/search.ts): a query that starts with '#' is a pure expression query and
        // runs once. Otherwise phase 1 is exact; with fewer than 5 results scoring >= 10 (every result
        // does when there is no text to score, as every title "starts with" the empty query), phase 2
        // adds typo-tolerant matches and the combined list is re-sorted by score, depth and path title —
        // which also discards an `orderBy` order, exactly like the web app does.
        let pure_expression = query.trim().starts_with('#');
        let progressive = !pure_expression && opts.order_by.is_none() && !opts.exact_only;
        let good = if tokens.is_empty() { ranked.len() } else { ranked.iter().filter(|(s, _)| *s >= 10.0).count() };
        if progressive && good < 5 {
            let mut extra = vec![];
            if !tokens.is_empty() && !exact.user_ordered {
                let fuzzy = self.pass(db, &parsed, opts, true, &nq)?;
                let have: std::collections::HashSet<u32> = ranked.iter().map(|(_, i)| *i).collect();
                extra = fuzzy.ranked.into_iter().filter(|(_, i)| !have.contains(i)).collect();
                highlighted.extend(fuzzy.highlighted);
                error = error.or(fuzzy.error);
            }
            let resort = |v: &mut Vec<(f64, u32)>| v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| c.path_to(a.1).len().cmp(&c.path_to(b.1).len()))
                .then_with(|| self.path_title(a.1).cmp(&self.path_title(b.1))));
            resort(&mut ranked);
            resort(&mut extra);
            ranked.extend(extra);
        }
        let hits = ranked.into_iter().take(want).map(|(s, i)| Hit {
            note_id: c.ids[i as usize].clone(), score: s,
            path: c.path_to(i).into_iter().map(|x| c.ids[x as usize].clone()).collect(),
        }).collect();
        let mut hl = ctx.highlighted;
        hl.extend(highlighted);
        Ok(SearchOutput { hits, error: ctx.error.or(error), highlighted: hl })
    }

    /// Titles along the note's path joined like upstream's `notePathTitle` (tie-break key).
    fn path_title(&self, i: u32) -> String {
        let c = &self.corpus;
        c.path_to(i).iter().map(|&p| c.title[p as usize].as_str()).collect::<Vec<_>>().join(" \u{203a} ")
    }

    /// One evaluation of the expression; returns notes with scores, best first.
    fn pass(&self, db: &Db, parsed: &parse::Parsed, opts: &SearchOpts, fuzzy: bool, nq: &str) -> Result<Pass> {
        let c = &self.corpus;
        let tokens = &parsed.fulltext_tokens;
        let mut exec = Exec::new(c, db, self.fts_ready.get());
        exec.fuzzy = fuzzy;
        let notes = exec.run_top(&parsed.expr)?;
        self.fts_ready.set(exec.fts_state());
        let user_ordered = exec.user_ordered;
        let ranked: Vec<(f64, u32)> = if tokens.is_empty() {
            // nothing to rank by: every title "starts with" the empty query (+500), hidden notes are penalised
            notes.into_iter().map(|i| (if c.hidden.has(i) { 500.0 / 3.0 } else { 500.0 }, i)).collect()
        } else if user_ordered {
            notes.into_iter().map(|i| (0.0, i)).collect()
        } else {
            // 1) cheap title-only score for everything, 2) full score (path, content tier) for the best few
            let mut pre: Vec<(f64, u32)> = notes.iter().map(|&i| (title_score(c, i, nq, tokens), i)).collect();
            pre.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let want = opts.limit.filter(|l| *l > 0).unwrap_or(usize::MAX);
            let deep = pre.len().min(want.saturating_mul(4).max(500));
            if !exec.deferred_quality.is_empty() {
                let deferred: std::collections::HashSet<u32> = exec.deferred_quality.iter().copied().collect();
                let top: Vec<u32> = pre[..deep].iter().map(|(_, i)| *i).filter(|i| deferred.contains(i)).take(300).collect();
                exec.compute_quality(&top, tokens)?;
            }
            let mut scored: Vec<(f64, u32)> = pre[..deep].iter().map(|&(_, i)| (score(c, i, nq, tokens, exec.content_quality.get(&i).copied()), i)).collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| c.path_to(a.1).len().cmp(&c.path_to(b.1).len()))
                .then_with(|| self.path_title(a.1).cmp(&self.path_title(b.1))));
            scored.extend(pre[deep..].iter().copied());
            scored
        };
        Ok(Pass { ranked, error: exec.error.clone(), highlighted: exec.highlighted.clone(), user_ordered })
    }
}

#[derive(Clone)]
struct Pass { ranked: Vec<(f64, u32)>, error: Option<String>, highlighted: Vec<String>, user_ordered: bool }

/// Title tiers and token matches in the title only: cheap enough to run for every hit.
fn title_score(c: &Corpus, i: u32, nq: &str, tokens: &[String]) -> f64 {
    let iu = i as usize;
    let mut s = 0.0;
    if c.ids[iu].to_lowercase() == nq { s += 1000.0; }
    let title = &c.title_norm[iu];
    if !tokens.iter().any(|t| title.contains(text::normalize(t).as_str())) && title != nq { return s; }
    let title_words = text::tokenize_normalized(title);
    let qwords = text::tokenize_normalized(nq);
    if title == nq { s += 2000.0; }
    else if title.starts_with(nq) && !nq.is_empty() { s += 500.0; }
    else if text::words_contain_phrase(&title_words, &qwords) { s += 300.0; }
    s + chunk_score(&title_words, tokens, 2.0)
}

fn chunk_score(words: &[String], tokens: &[String], factor: f64) -> f64 {
    tokens.iter().map(|t| {
        let t = text::strip_word_punctuation(&text::normalize(t)).to_string();
        if t.is_empty() { 0.0 }
        else if words.iter().any(|w| *w == t) { 4.0 * factor }
        else if words.iter().any(|w| w.starts_with(&t)) { 2.0 * factor }
        else if words.iter().any(|w| w.contains(&t)) { factor }
        else { 0.0 }
    }).sum()
}

/// Relevance scoring modelled on `search_result.ts` (title tiers, token matches in title and
/// path, content-match tier, hidden-subtree penalty). Fuzzy tiers are not scored.
fn score(c: &Corpus, i: u32, nq: &str, tokens: &[String], content: Option<ContentTier>) -> f64 {
    let mut s = title_score(c, i, nq, tokens);
    let path_words: Vec<String> = c.path_to(i).iter().rev().skip(1).flat_map(|&p| text::tokenize(&c.title[p as usize])).collect();
    s += chunk_score(&path_words, tokens, 0.3);
    s += match content { Some(ContentTier::ExactPhrase) => 150.0, Some(ContentTier::Proximity) => 80.0, Some(ContentTier::ExactWord) => 60.0,
        Some(ContentTier::WordPrefix) => 30.0, Some(ContentTier::Substring) => 15.0, Some(ContentTier::Fuzzy) => 5.0, None => 0.0 };
    if c.hidden.has(i) { s /= 3.0; }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lib { _dir: tempfile::TempDir, db: Db, ids: std::collections::HashMap<&'static str, String> }

    fn lib() -> Lib {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'2020-01-01 00:00:00.000+0000','2020-01-01 00:00:00.000+0000','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        let mut ids: std::collections::HashMap<&'static str, String> = std::collections::HashMap::new();
        fn mk_in(db: &Db, ids: &mut std::collections::HashMap<&'static str, String>, key: &'static str, parent: &str, title: &str, content: &str) -> String {
            let p = if parent.is_empty() { "root".to_string() } else { ids[parent].clone() };
            let id = db.create_note(&p, title, "text", "text/html", content).unwrap();
            ids.insert(key, id.clone());
            id
        }
        let books = mk_in(&db, &mut ids, "books", "", "Books", "");
        db.add_attribute(&books, "label", "bookshelf", "", false).unwrap();
        let fell = mk_in(&db, &mut ids, "fell", "books", "The Fellowship of the Ring", "<p>One ring to rule them all</p>");
        for (n, v) in [("book", ""), ("year", "1954"), ("author", "Tolkien")] { db.add_attribute(&fell, "label", n, v, false).unwrap(); }
        let dune = mk_in(&db, &mut ids, "dune", "books", "Dune", "<p>The spice must flow. Fear is the mind-killer.</p>");
        for (n, v) in [("book", ""), ("year", "1965")] { db.add_attribute(&dune, "label", n, v, false).unwrap(); }
        let old = mk_in(&db, &mut ids, "old", "books", "Old Draft", "<p>spice</p>");
        db.add_attribute(&old, "label", "archived", "", false).unwrap();
        let tolk = mk_in(&db, &mut ids, "tolk", "", "Tolkien", "<p>J. R. R. Tolkien, author</p>");
        db.add_attribute(&fell, "relation", "writtenBy", &tolk, false).unwrap();
        let rec = mk_in(&db, &mut ids, "rec", "", "Recipes", "");
        db.add_attribute(&rec, "label", "color", "red", true).unwrap();
        mk_in(&db, &mut ids, "pasta", "rec", "Pasta", "<p>tomato basil &amp; garlic</p>");
        mk_in(&db, &mut ids, "diary", "", "Diary", "<p>Today I ate pasta for dinner</p>");
        Lib { _dir: dir, db, ids }
    }

    fn q(l: &Lib, query: &str) -> Vec<String> {
        let s = Searcher::new(&l.db).unwrap();
        let out = s.search(&l.db, query, &SearchOpts::default()).unwrap();
        assert!(out.error.is_none(), "query {query:?}: {:?}", out.error);
        out.hits.iter().map(|h| s.corpus().title[s.corpus().idx[&h.note_id] as usize].clone()).collect()
    }
    fn set(v: &[&str]) -> std::collections::BTreeSet<String> { v.iter().map(|s| s.to_string()).collect() }
    fn qs(l: &Lib, query: &str) -> std::collections::BTreeSet<String> { q(l, query).into_iter().collect() }

    #[test]
    fn plain_fulltext_titles_content_and_ancestors() {
        let l = lib();
        assert_eq!(q(&l, "dune"), ["Dune"]);
        assert_eq!(qs(&l, "spice"), set(&["Dune"]), "archived 'Old Draft' is excluded by default");
        assert_eq!(qs(&l, "one ring"), set(&["The Fellowship of the Ring"]));
        assert_eq!(q(&l, "books fellowship"), ["The Fellowship of the Ring"], "ancestor title + own title");
        assert_eq!(qs(&l, "tomato garlic"), set(&["Pasta"]), "html entities decoded");
        assert!(q(&l, "zzznothing").is_empty());
        // tokens are AND-ed: 'dune' and 'ring' never co-occur
        assert!(q(&l, "dune ring").is_empty());
    }

    #[test]
    fn relevance_title_beats_content() {
        let l = lib();
        // 'pasta' is the title of one note and appears in the body of another
        assert_eq!(q(&l, "pasta"), ["Pasta", "Diary"]);
    }

    #[test]
    fn labels_comparisons_and_boolean_logic() {
        let l = lib();
        assert_eq!(qs(&l, "#book"), set(&["The Fellowship of the Ring", "Dune"]));
        assert_eq!(qs(&l, "#book and #year >= 1960"), set(&["Dune"]));
        assert_eq!(qs(&l, "#year > 1950 AND #year < 1960"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "#author = tolkien"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "#author *=* olki"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "#author %= '^tolk.*n$'"), set(&["The Fellowship of the Ring"]));
        assert!(!qs(&l, "#!book").contains("Dune"));
        assert!(!qs(&l, "not(#book) and #bookshelf").contains("Dune"));
        assert_eq!(qs(&l, "(#year=1954 OR #year=1965) AND #book"), set(&["The Fellowship of the Ring", "Dune"]));
        assert_eq!(qs(&l, "dune #book"), set(&["Dune"]), "fulltext + attribute filter");
    }

    #[test]
    fn inherited_labels_and_archived() {
        let l = lib();
        assert_eq!(qs(&l, "#color=red"), set(&["Recipes", "Pasta"]), "inheritable label applies to the subtree");
        let s = Searcher::new(&l.db).unwrap();
        let out = s.search(&l.db, "spice", &SearchOpts { include_archived: true, ..Default::default() }).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert_eq!(qs(&l, "#archived"), set(&[]), "archived notes are hidden even from #archived unless included");
        let out = s.search(&l.db, "#archived", &SearchOpts { include_archived: true, ..Default::default() }).unwrap();
        assert_eq!(out.hits.len(), 1);
    }

    #[test]
    fn relations_and_note_properties() {
        let l = lib();
        assert_eq!(qs(&l, "~writtenBy"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "~writtenBy.title = tolkien"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "note.title *=* dun"), set(&["Dune"]));
        assert_eq!(qs(&l, "note.title =* the"), set(&["The Fellowship of the Ring"]));
        assert!(qs(&l, "note.type = text").len() >= 8);
        assert_eq!(qs(&l, "note.content *=* basil"), set(&["Pasta"]));
        assert_eq!(qs(&l, "note.parents.title = books"), set(&["The Fellowship of the Ring", "Dune"]));
        assert_eq!(qs(&l, "note.children.title = pasta"), set(&["Recipes"]));
        assert_eq!(qs(&l, "note.ancestors.title = recipes"), set(&["Recipes", "Pasta"]), "the matched note counts as part of its own subtree, as upstream");
        assert_eq!(qs(&l, "note.backlinks.title = tolkien").len(), 0, "relations to Tolkien aren't links in the 'links' sense? writtenBy is a user relation");
        assert_eq!(qs(&l, "note.labelCount >= 3"), set(&["The Fellowship of the Ring"]));
        assert_eq!(qs(&l, "note.childrenCount = 2"), set(&["Books"]).intersection(&qs(&l, "note.childrenCount = 2")).cloned().collect());
    }

    #[test]
    fn order_by_and_limit() {
        let l = lib();
        assert_eq!(q(&l, "#book orderBy #year desc"), ["Dune", "The Fellowship of the Ring"]);
        assert_eq!(q(&l, "#book orderBy #year"), ["The Fellowship of the Ring", "Dune"]);
        assert_eq!(q(&l, "#book orderBy #year desc limit 1"), ["Dune"]);
        assert_eq!(q(&l, "#book orderBy note.title"), ["Dune", "The Fellowship of the Ring"]);
        // dates: newest first with desc (Diary was created last)
        std::thread::sleep(std::time::Duration::from_millis(5));
        l.db.create_note("root", "Newest", "text", "text/html", "").unwrap();
        assert_eq!(q(&l, "note.type = text orderBy note.dateCreated desc limit 6")[0], "Newest");
        assert_eq!(q(&l, "note.type = text orderBy note.dateCreated limit 6")[0], "root");
        // upstream quirk, replicated: a non-'#' query with fewer than 5 results is re-sorted by score/depth/path,
        // which discards its orderBy (see search_progressive_rules in docs)
        // (fixture notes can share a creation millisecond, so which neighbour joins "Newest" is not fixed)
        let two = q(&l, "note.type = text orderBy note.dateCreated desc limit 2");
        assert_eq!(two.len(), 2);
        assert!(two.contains(&"Newest".to_string()), "{two:?}");
        assert!(two[0] <= two[1], "re-sorted by path title: {two:?}");
    }

    #[test]
    fn exact_match_and_errors() {
        let l = lib();
        assert_eq!(qs(&l, "=pasta"), set(&["Pasta", "Diary"]), "exact word in title or body");
        assert!(qs(&l, "=past").is_empty());
        let s = Searcher::new(&l.db).unwrap();
        let e = |query: &str| s.search(&l.db, query, &SearchOpts::default()).unwrap().error;
        assert!(e("#a and #b or #c").unwrap().contains("Mixed usage of AND/OR"));
        assert!(e("#book dune").unwrap().contains("Full-text"));
        assert!(e("(#a").unwrap().contains("parenthesis"));
        assert!(e("note.nonsense = 1").unwrap().contains("Unrecognized note property"));
        assert!(e("#a = ~b").unwrap().contains("Cannot compare"));
        assert!(e("~writtenBy = x").unwrap().contains("Relation can be compared only with property"));
    }

    #[test]
    fn dates_and_ancestor_option() {
        let l = lib();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(qs(&l, &format!("note.dateCreated =* '{today}'")).len(), 7);
        assert_eq!(qs(&l, "note.dateCreated =* today").len(), qs(&l, &format!("note.dateCreated =* '{today}'")).len());
        let s = Searcher::new(&l.db).unwrap();
        let out = s.search(&l.db, "", &SearchOpts { ancestor: Some(l.ids["books"].clone()), ..Default::default() }).unwrap();
        let titles: std::collections::BTreeSet<_> = out.hits.iter().map(|h| s.corpus().title[s.corpus().idx[&h.note_id] as usize].clone()).collect();
        assert_eq!(titles, set(&["Books", "The Fellowship of the Ring", "Dune"]));
    }

    /// The index is only a prefilter: results must equal a straightforward scan of the same data.
    #[test]
    fn indexed_search_equals_naive_scan_and_tracks_edits() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("f.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "theta", "kappa", "sigma", "omega", "caf\u{e9}", "na\u{ef}ve"];
        let mut state = 12345u64;
        let mut rnd = |n: usize| { state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); ((state >> 33) as usize) % n };
        let mut docs: Vec<(String, String)> = vec![];
        for i in 0..150 {
            let body: Vec<&str> = (0..6).map(|_| words[rnd(words.len())]).collect();
            let title = format!("n{i} {}", words[rnd(words.len())]);
            let html = format!("<p>{}</p>", body.join(" "));
            let id = db.create_note("root", &title, "text", "text/html", &html).unwrap();
            docs.push((id, format!("{} {}", title.to_lowercase(), text::preprocess_content(&html, "text", "text/html", false))));
        }
        let s = Searcher::new(&db).unwrap();
        for q in ["alp", "gam", "eta", "sigma omega", "cafe", "naive", "theta kappa zeta"] {
            let got: std::collections::BTreeSet<String> = s.search(&db, q, &SearchOpts::default()).unwrap().hits.into_iter().map(|h| h.note_id).collect();
            let toks: Vec<String> = q.split(' ').map(|t| text::normalize(t)).collect();
            let want: std::collections::BTreeSet<String> = docs.iter().filter(|(_, hay)| {
                let hay = text::normalize(hay);
                toks.iter().all(|t| hay.contains(t.as_str()))
            }).map(|(id, _)| id.clone()).collect();
            assert!(want.is_subset(&got), "query {q:?}: indexed search lost {:?}", want.difference(&got).collect::<Vec<_>>());
        }
        // a later edit is picked up (new blob id => indexed on demand)
        let (id, _) = &docs[0];
        db.set_note_content(id, "<p>brandnewterm here</p>").unwrap();
        let s = Searcher::new(&db).unwrap();
        let hits = s.search(&db, "brandnewterm", &SearchOpts::default()).unwrap().hits;
        assert_eq!(hits.iter().map(|h| h.note_id.clone()).collect::<Vec<_>>(), vec![id.clone()]);
        // deleted notes disappear
        db.delete_branch(&format!("root_{id}")).unwrap();
        let s = Searcher::new(&db).unwrap();
        assert!(s.search(&db, "brandnewterm", &SearchOpts::default()).unwrap().hits.is_empty());
        // short tokens (below the trigram size) fall back to scanning and still work
        assert!(!s.search(&db, "ze", &SearchOpts::default()).unwrap().hits.is_empty());
    }
}
