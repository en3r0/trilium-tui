//! Evaluation of parsed search expressions over an in-memory corpus (ports of `search/expressions/*`).
use super::cmp::{Cmp, parse_float};
use super::parse::{Expr, OrderDef, SearchOpts, expand_order_path, prop_name};
use super::text::{normalize, preprocess_content, strip_word_punctuation, tokenize_normalized, words_contain_phrase};
use crate::db::Db;
use anyhow::Result;
use std::collections::{HashMap, HashSet};

// ------------------------------------------------------------------ note sets

#[derive(Clone, Debug)]
pub struct NoteSet { bits: Vec<u64>, n: usize }

impl NoteSet {
    pub fn empty(n: usize) -> Self { Self { bits: vec![0; n.div_ceil(64)], n } }
    pub fn full(n: usize) -> Self {
        let mut s = Self::empty(n);
        for i in 0..n { s.add(i as u32); }
        s
    }
    pub fn add(&mut self, i: u32) { self.bits[(i / 64) as usize] |= 1 << (i % 64); }
    pub fn has(&self, i: u32) -> bool { self.bits[(i / 64) as usize] & (1 << (i % 64)) != 0 }
    pub fn len(&self) -> usize { self.bits.iter().map(|b| b.count_ones() as usize).sum() }
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.n as u32).filter(|i| self.has(*i))
    }
    pub fn intersect(&self, o: &NoteSet) -> NoteSet { NoteSet { bits: self.bits.iter().zip(&o.bits).map(|(a, b)| a & b).collect(), n: self.n } }
    pub fn union(&self, o: &NoteSet) -> NoteSet { NoteSet { bits: self.bits.iter().zip(&o.bits).map(|(a, b)| a | b).collect(), n: self.n } }
    pub fn minus(&self, o: &NoteSet) -> NoteSet { NoteSet { bits: self.bits.iter().zip(&o.bits).map(|(a, b)| a & !b).collect(), n: self.n } }
    pub fn from_iter(n: usize, it: impl IntoIterator<Item = u32>) -> Self { let mut s = Self::empty(n); for i in it { s.add(i); } s }
}

// ------------------------------------------------------------------ corpus

#[derive(Debug, Clone)]
pub struct Attr {
    pub label: bool,
    pub name: String,
    pub name_l: String,
    pub value: String,
    pub value_l: String,
    pub inheritable: bool,
    pub target: Option<u32>,
}

pub struct Corpus {
    pub ids: Vec<String>,
    pub idx: HashMap<String, u32>,
    pub title: Vec<String>,
    pub title_norm: Vec<String>,
    pub ntype: Vec<String>,
    pub mime: Vec<String>,
    pub protected: Vec<bool>,
    pub dates: Vec<[String; 4]>, // dateCreated, dateModified, utcDateCreated, utcDateModified
    pub parents: Vec<Vec<(u32, Option<String>)>>,
    pub children: Vec<Vec<u32>>,
    pub attrs: Vec<Vec<Attr>>,
    by_name: HashMap<(bool, String), Vec<(u32, u32)>>,
    /// target note -> (source note, attr index) for relations
    target_rels: Vec<Vec<(u32, u32)>>,
    pub archived: NoteSet,
    pub hidden: NoteSet,
    flat: Vec<std::cell::OnceCell<String>>,
    /// memoised `getAttributes()`: owned + inherited + template attributes, as (owner note, attr index)
    attr_cache: std::cell::RefCell<HashMap<u32, std::rc::Rc<Vec<(u32, u32)>>>>,
    /// normalized title word -> notes (for typo-tolerant title matching)
    vocab: std::cell::OnceCell<HashMap<String, Vec<u32>>>,
}

const LINK_RELATIONS: &[&str] = &["internallink", "imagelink", "includenotelink", "relationmaplink"];

impl Corpus {
    pub fn build(db: &Db) -> Result<Self> {
        let mut ids = vec![]; let mut idx = HashMap::new();
        let (mut title, mut ntype, mut mime, mut protected, mut dates) = (vec![], vec![], vec![], vec![], vec![]);
        let mut st = db.conn.prepare("SELECT noteId,title,type,mime,isProtected,dateCreated,dateModified,utcDateCreated,utcDateModified FROM notes WHERE isDeleted=0")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?,
            [r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, String>(8)?])))? {
            let (id, t, ty, mi, pr, d) = r?;
            idx.insert(id.clone(), ids.len() as u32);
            // like Trilium: a locked protected note has no searchable title (the stored one is ciphertext)
            let t = if pr != 0 { if db.is_unlocked() { db.decrypt_title(&t) } else { String::new() } } else { t };
            ids.push(id); title.push(t); ntype.push(ty); mime.push(mi); protected.push(pr != 0); dates.push(d);
        }
        let n = ids.len();
        let mut parents = vec![vec![]; n]; let mut children = vec![vec![]; n];
        let mut st = db.conn.prepare("SELECT noteId,parentNoteId,prefix FROM branches WHERE isDeleted=0 ORDER BY notePosition")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))? {
            let (c, p, pre) = r?;
            if let (Some(&ci), Some(&pi)) = (idx.get(&c), idx.get(&p)) { parents[ci as usize].push((pi, pre.filter(|x| !x.is_empty()))); children[pi as usize].push(ci); }
        }
        let mut attrs: Vec<Vec<Attr>> = vec![vec![]; n];
        let mut by_name: HashMap<(bool, String), Vec<(u32, u32)>> = HashMap::new();
        let mut target_rels: Vec<Vec<(u32, u32)>> = vec![vec![]; n];
        let mut st = db.conn.prepare("SELECT noteId,type,name,value,isInheritable FROM attributes WHERE isDeleted=0 ORDER BY position")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0)))? {
            let (nid, kind, name, value, inh) = r?;
            let Some(&ni) = idx.get(&nid) else { continue };
            let label = kind == "label";
            let target = if label { None } else { idx.get(&value).copied() };
            let a = Attr { label, name_l: name.to_lowercase(), name, value_l: value.to_lowercase(), value, inheritable: inh, target };
            let ai = attrs[ni as usize].len() as u32;
            by_name.entry((label, a.name_l.clone())).or_default().push((ni, ai));
            if let Some(t) = target { target_rels[t as usize].push((ni, ai)); }
            attrs[ni as usize].push(a);
        }
        let title_norm = title.iter().map(|t| normalize(t)).collect();
        let mut c = Corpus { ids, idx, title, title_norm, ntype, mime, protected, dates, parents, children, attrs, by_name, target_rels,
            archived: NoteSet::empty(n), hidden: NoteSet::empty(n), flat: (0..n).map(|_| std::cell::OnceCell::new()).collect(), attr_cache: Default::default(), vocab: Default::default() };
        // archived: label with inheritance semantics
        let mut arch = NoteSet::empty(n);
        for &(ni, ai) in c.by_name.get(&(true, "archived".to_string())).map(|v| v.as_slice()).unwrap_or(&[]) {
            let a = &c.attrs[ni as usize][ai as usize];
            for x in c.attr_effect(ni, a, false) { arch.add(x); }
        }
        c.archived = arch;
        // hidden: not reachable from root without passing through _hidden
        let mut reach = NoteSet::empty(n);
        if let Some(&root) = c.idx.get("root") {
            let mut stack = vec![root];
            while let Some(x) = stack.pop() {
                if reach.has(x) { continue; }
                reach.add(x);
                for &ch in &c.children[x as usize] {
                    if c.ids[ch as usize] == "_hidden" { reach.add(ch); } else { stack.push(ch); } // _hidden itself is visible, its subtree is not
                }
            }
        }
        c.hidden = NoteSet::full(n).minus(&reach);
        Ok(c)
    }

    pub fn len(&self) -> usize { self.ids.len() }

    fn vocab(&self) -> &HashMap<String, Vec<u32>> {
        self.vocab.get_or_init(|| {
            let mut m: HashMap<String, Vec<u32>> = HashMap::new();
            for (i, t) in self.title_norm.iter().enumerate() { for w in tokenize_normalized(t) { m.entry(w).or_default().push(i as u32); } }
            m
        })
    }

    /// Notes with a title word within typo distance of `token` (not merely containing it).
    pub fn fuzzy_title_notes(&self, token: &str) -> HashSet<u32> {
        let max = super::text::auto_max_edit_distance(token.chars().count());
        let mut out = HashSet::new();
        if max == 0 { return out; }
        for (w, notes) in self.vocab() {
            if w.len() > token.len() && w.contains(token) { continue; }
            if super::text::edit_distance_within(token, w, max) { out.extend(notes.iter().copied()); }
        }
        out
    }

    fn using_template(&self, note: u32) -> Vec<u32> {
        self.target_rels[note as usize].iter().filter(|(s, ai)| { let a = &self.attrs[*s as usize][*ai as usize]; a.name_l == "template" || a.name_l == "inherit" }).map(|(s, _)| *s).collect()
    }

    fn subtree_incl_templated(&self, root: u32) -> Vec<u32> {
        let mut seen = HashSet::new(); let mut out = vec![]; let mut stack = vec![root];
        while let Some(x) = stack.pop() {
            if self.ids[x as usize] == "_hidden" || !seen.insert(x) { continue; }
            out.push(x);
            stack.extend(self.children[x as usize].iter().copied());
            stack.extend(self.using_template(x));
        }
        out
    }

    pub fn subtree(&self, root: u32) -> Vec<u32> {
        let mut seen = HashSet::new(); let mut out = vec![]; let mut stack = vec![root];
        while let Some(x) = stack.pop() {
            if !seen.insert(x) { continue; }
            out.push(x);
            stack.extend(self.children[x as usize].iter().copied());
        }
        out
    }

    /// The notes an attribute applies to (own note, inheritable subtree, template users).
    fn attr_effect(&self, note: u32, a: &Attr, is_template_label: bool) -> Vec<u32> {
        if a.inheritable && !is_template_label { self.subtree_incl_templated(note) }
        else if !self.using_template(note).is_empty() && !is_template_label {
            let mut v = vec![note]; v.extend(self.using_template(note)); v
        } else { vec![note] }
    }

    pub fn flat_text(&self, n: u32) -> &str {
        self.flat[n as usize].get_or_init(|| {
            let mut s = format!("{} {} {} ", self.ids[n as usize], self.ntype[n as usize], self.mime[n as usize]);
            for (_, pre) in &self.parents[n as usize] { if let Some(p) = pre { s.push_str(p); s.push(' '); } }
            s.push_str(&self.title[n as usize]); s.push(' ');
            for a in self.effective_attrs(n) {
                s.push(if a.label { '#' } else { '~' }); s.push_str(&a.name);
                if !a.value.is_empty() { s.push('='); s.push_str(&a.value); }
                s.push(' ');
            }
            normalize(&s)
        })
    }

    fn title_under(&self, n: u32, prefix: &Option<String>) -> std::borrow::Cow<'_, str> {
        match prefix {
            Some(p) => std::borrow::Cow::Owned(normalize(&format!("{p} - {}", self.title[n as usize]))),
            None => std::borrow::Cow::Borrowed(self.title_norm[n as usize].as_str()),
        }
    }

    pub fn path_to(&self, n: u32) -> Vec<u32> {
        let mut p = vec![n]; let mut cur = n;
        for _ in 0..128 {
            let Some(&(par, _)) = self.parents[cur as usize].iter().find(|(p, _)| self.ids[*p as usize] != "_hidden").or(self.parents[cur as usize].first()) else { break };
            if self.ids[par as usize] == "root" { break; }
            p.push(par); cur = par;
        }
        p.reverse(); p
    }

    pub fn is_descendant_or_self(&self, n: u32, anc: u32) -> bool {
        let (mut stack, mut seen) = (vec![n], HashSet::new());
        while let Some(x) = stack.pop() { if x == anc { return true; } if seen.insert(x) { stack.extend(self.parents[x as usize].iter().map(|(p, _)| *p)); } }
        false
    }

    fn distance_to(&self, n: u32, anc: u32) -> usize {
        if n == anc { return 0; }
        let (mut frontier, mut seen, mut d) = (vec![n], HashSet::new(), 0usize);
        while !frontier.is_empty() && d < 200 {
            d += 1; let mut next = vec![];
            for x in frontier { for (p, _) in &self.parents[x as usize] { if *p == anc { return d; } if seen.insert(*p) { next.push(*p); } } }
            frontier = next;
        }
        999_999
    }

    /// `BNote.__getAttributes`: owned attributes, inheritable ones from parents (recursively), and the
    /// attributes of any `~template` / `~inherit` target (minus the template marker labels).
    fn attrs_ref(&self, n: u32, path: &mut Vec<u32>) -> std::rc::Rc<Vec<(u32, u32)>> {
        if path.contains(&n) { return Default::default(); }
        if let Some(v) = self.attr_cache.borrow().get(&n) { return v.clone(); }
        let mut inherited: Vec<(u32, u32)> = (0..self.attrs[n as usize].len() as u32).map(|ai| (n, ai)).collect();
        path.push(n);
        let id = self.ids[n as usize].as_str();
        if id != "root" && id != "_hidden" {
            for (p, _) in &self.parents[n as usize] {
                let pa = self.attrs_ref(*p, path);
                inherited.extend(pa.iter().copied().filter(|&(o, ai)| self.attrs[o as usize][ai as usize].inheritable));
            }
        }
        let mut from_templates: Vec<(u32, u32)> = vec![];
        for &(o, ai) in &inherited {
            let a = &self.attrs[o as usize][ai as usize];
            if !a.label && (a.name == "template" || a.name == "inherit") {
                if let Some(t) = a.target {
                    from_templates.extend(self.attrs_ref(t, path).iter().copied().filter(|&(to, tai)| {
                        let ta = &self.attrs[to as usize][tai as usize];
                        !(ta.label && (ta.name == "template" || ta.name == "workspaceTemplate"))
                    }));
                }
            }
        }
        path.pop();
        let mut seen = HashSet::new();
        let all: Vec<(u32, u32)> = inherited.into_iter().chain(from_templates).filter(|x| seen.insert(*x)).collect();
        let rc = std::rc::Rc::new(all);
        self.attr_cache.borrow_mut().insert(n, rc.clone());
        rc
    }

    pub fn effective_attrs(&self, n: u32) -> Vec<&Attr> {
        self.attrs_ref(n, &mut vec![]).iter().map(|&(o, ai)| &self.attrs[o as usize][ai as usize]).collect()
    }
}

// ------------------------------------------------------------------ execution

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum ContentTier { Fuzzy = 0, Substring = 1, WordPrefix, ExactWord, Proximity, ExactPhrase }

pub struct Exec<'a> {
    pub c: &'a Corpus,
    pub db: &'a Db,
    pub highlighted: Vec<String>,
    pub content_quality: HashMap<u32, ContentTier>,
    pub error: Option<String>,
    db_cache: HashMap<&'static str, HashMap<u32, f64>>,
    order: Option<Vec<u32>>,
    /// an explicit orderBy ran, so relevance sorting must not override it
    pub user_ordered: bool,
    /// notes matched through the index whose content-match quality is computed lazily for the top results
    pub deferred_quality: Vec<u32>,
    fts_ok: Option<bool>,
    /// progressive phase 2: typo-tolerant matching in titles and (small corpora) content
    pub fuzzy: bool,
}

impl<'a> Exec<'a> {
    pub fn new(c: &'a Corpus, db: &'a Db, fts_ok: Option<bool>) -> Self { Self { c, db, highlighted: vec![], content_quality: HashMap::new(), error: None, db_cache: HashMap::new(), order: None, user_ordered: false, deferred_quality: vec![], fts_ok, fuzzy: false } }

    pub fn run(&mut self, e: &Expr, input: &NoteSet) -> Result<NoteSet> {
        let c = self.c;
        let n = c.len();
        Ok(match e {
            Expr::True => input.clone(),
            Expr::And(v) => { let mut cur = input.clone(); for x in v { cur = self.run(x, &cur)?; } cur }
            Expr::Or(v) => { let mut out = NoteSet::empty(n); for x in v { out = out.union(&self.run(x, input)?); } out }
            Expr::Not(x) => input.minus(&self.run(x, input)?),
            Expr::NotHidden => input.intersect(&c.hidden),
            Expr::AttrExists { label, name } => {
                let tmpl = *label && (name == "template" || name == "workspacetemplate");
                let mut out = NoteSet::empty(n);
                for &(ni, ai) in c.by_name.get(&(*label, name.clone())).map(|v| v.as_slice()).unwrap_or(&[]) {
                    for x in c.attr_effect(ni, &c.attrs[ni as usize][ai as usize], tmpl) { out.add(x); }
                }
                out.intersect(input)
            }
            Expr::LabelCmp { name, cmp } => {
                let mut out = NoteSet::empty(n);
                for &(ni, ai) in c.by_name.get(&(true, name.clone())).map(|v| v.as_slice()).unwrap_or(&[]) {
                    let a = &c.attrs[ni as usize][ai as usize];
                    if input.has(ni) && cmp.matches(&a.value_l) { for x in c.attr_effect(ni, a, false) { out.add(x); } }
                }
                out
            }
            Expr::RelWhere { name, sub } => {
                let mut out = NoteSet::empty(n);
                for &(ni, ai) in c.by_name.get(&(false, name.clone())).map(|v| v.as_slice()).unwrap_or(&[]) {
                    let a = &c.attrs[ni as usize][ai as usize];
                    if let (true, Some(t)) = (input.has(ni), a.target) {
                        let r = self.run(sub, &NoteSet::from_iter(n, [t]))?;
                        if r.has(t) { for x in c.attr_effect(ni, a, false) { out.add(x); } }
                    }
                }
                out.intersect(input)
            }
            Expr::Prop { prop, cmp } if matches!(*prop, "isArchived" | "isProtected") => {
                // boolean properties only take two values: evaluate the comparator twice, not per note
                let set = if *prop == "isArchived" { c.archived.clone() } else { NoteSet::from_iter(n, (0..n as u32).filter(|&i| c.protected[i as usize])) };
                let mut out = NoteSet::empty(n);
                if cmp.matches("true") { out = out.union(&input.intersect(&set)); }
                if cmp.matches("false") { out = out.union(&input.minus(&set)); }
                out
            }
            Expr::Prop { prop, cmp } => {
                let mut out = NoteSet::empty(n);
                // exact/word title comparisons can only match when the normalized title contains the value
                let title_prefilter = *prop == "title" && matches!(cmp.op(), "word=" | "=") && !cmp.norm().is_empty();
                for i in input.iter() {
                    if title_prefilter && !c.title_norm[i as usize].contains(cmp.norm()) { continue; }
                    if cmp.matches(&self.prop_value(prop, i)?) { out.add(i); }
                }
                out
            }
            Expr::FlatText(tokens) => self.flat_text_match(tokens, input),
            Expr::Content { op, tokens, raw, flat_text, fuzzy_fallback } => {
                let exact = self.content_match(op, tokens, *raw, *flat_text, input)?;
                if *fuzzy_fallback && self.fuzzy { let extra = self.fuzzy_content(tokens, input, &exact)?; exact.union(&extra) } else { exact }
            }
            Expr::ChildOf(sub) => {
                let mut subin = NoteSet::empty(n);
                for i in input.iter() { for (p, _) in &c.parents[i as usize] { subin.add(*p); } }
                let r = self.run(sub, &subin)?;
                let mut out = NoteSet::empty(n);
                for p in r.iter() { for &ch in &c.children[p as usize] { if input.has(ch) { out.add(ch); } } }
                out
            }
            Expr::ParentOf(sub) => {
                let mut subin = NoteSet::empty(n);
                for i in input.iter() { for &ch in &c.children[i as usize] { subin.add(ch); } }
                let r = self.run(sub, &subin)?;
                let mut out = NoteSet::empty(n);
                for ch in r.iter() { for (p, _) in &c.parents[ch as usize] { if input.has(*p) { out.add(*p); } } }
                out
            }
            Expr::DescendantOf(sub) => {
                let r = self.run(sub, &NoteSet::full(n))?;
                let mut tree = NoteSet::empty(n);
                for x in r.iter() { for d in c.subtree(x) { tree.add(d); } }
                input.intersect(&tree)
            }
            Expr::Linked { backlinks, sub } => {
                let linked = |this: &Self, note: u32| -> Vec<u32> {
                    let mut v = vec![];
                    if !*backlinks {
                        for a in &this.c.attrs[note as usize] { if !a.label { if let Some(t) = a.target { if is_link(&a.name_l) { v.push(t); } } } }
                    } else {
                        for &(s, ai) in &this.c.target_rels[note as usize] { if is_link(&this.c.attrs[s as usize][ai as usize].name_l) { v.push(s); } }
                    }
                    v
                };
                let mut subin = NoteSet::empty(n);
                for i in input.iter() { for x in linked(self, i) { subin.add(x); } }
                let matched = self.run(sub, &subin)?;
                NoteSet::from_iter(n, input.iter().filter(|&i| linked(self, i).iter().any(|x| matched.has(*x))))
            }
            Expr::Ancestor { id, depth } => {
                let Some(&a) = c.idx.get(id) else { return Ok(NoteSet::empty(n)) };
                let tree = NoteSet::from_iter(n, c.subtree(a)).intersect(input);
                let Some(d) = depth else { return Ok(tree) };
                let want: usize = d.get(2..).and_then(|s| s.parse().ok()).unwrap_or(0);
                let test: Box<dyn Fn(usize) -> bool> = match d.get(..2) {
                    Some("eq") => Box::new(move |x| x == want), Some("gt") => Box::new(move |x| x > want), Some("lt") => Box::new(move |x| x < want),
                    _ => return Ok(tree),
                };
                NoteSet::from_iter(n, tree.iter().filter(|&i| test(c.distance_to(i, a))))
            }
            Expr::OrderLimit { defs, limit, sub } => {
                let set = self.run(sub, input)?;
                // a nested orderBy has already fixed an order; keep it unless this level sorts itself
                let mut notes: Vec<u32> = match self.order.take() {
                    Some(v) => v.into_iter().filter(|&i| set.has(i)).collect(),
                    None => set.iter().collect(),
                };
                if !defs.is_empty() {
                    self.user_ordered = true;
                    let keys: Vec<Vec<Option<String>>> = defs.iter().map(|d| notes.iter().map(|&i| self.extract(d, i)).collect()).collect();
                    let pos: HashMap<u32, usize> = notes.iter().enumerate().map(|(k, &i)| (i, k)).collect();
                    notes.sort_by(|a, b| {
                        for (di, d) in defs.iter().enumerate() {
                            let r = compare_values(keys[di][pos[a]].as_deref(), keys[di][pos[b]].as_deref());
                            if r != std::cmp::Ordering::Equal { return if d.desc { r.reverse() } else { r }; }
                        }
                        std::cmp::Ordering::Equal
                    });
                }
                if *limit > 0 { notes.truncate(*limit); }
                let out = NoteSet::from_iter(n, notes.iter().copied());
                self.order = Some(notes);
                out
            }
        })
    }

    /// Evaluate the top-level expression; an `orderBy`/`limit` anywhere in it fixes the result order.
    pub fn run_top(&mut self, e: &Expr) -> Result<Vec<u32>> {
        let all = NoteSet::full(self.c.len());
        self.order = None;
        let set = self.run(e, &all)?;
        Ok(match self.order.take() {
            Some(v) => v.into_iter().filter(|&i| set.has(i)).collect(),
            None => set.iter().collect(),
        })
    }

    fn extract(&mut self, d: &OrderDef, note: u32) -> Option<String> {
        let path = expand_order_path(&d.path);
        let c = self.c;
        let mut cur = note;
        let mut i = 1; // path[0] == "note"
        while i < path.len() {
            match path[i].as_str() {
                "labels" => { i += 1; let name = &path[i]; return c.effective_attrs(cur).into_iter().find(|a| a.label && a.name_l == *name).map(|a| a.value.clone()); }
                "relations" => { i += 1; let name = &path[i]; cur = c.effective_attrs(cur).into_iter().find(|a| !a.label && a.name_l == *name).and_then(|a| a.target)?; }
                "parents" => cur = c.parents[cur as usize].first()?.0,
                "children" => cur = *c.children[cur as usize].first()?,
                "random" => return Some(format!("{:.9}", rand::random::<f64>())),
                p => return prop_name(p).and_then(|pn| self.prop_value(pn, cur).ok()),
            }
            i += 1;
        }
        None
    }

    fn db_prop(&mut self, prop: &'static str) -> Result<&HashMap<u32, f64>> {
        if !self.db_cache.contains_key(prop) {
            let sql = match prop {
                "contentSize" => "SELECT n.noteId, COALESCE(LENGTH(b.content),0) FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.isDeleted=0",
                "contentAndAttachmentsSize" => "SELECT n.noteId, COALESCE(LENGTH(b.content),0) + COALESCE((SELECT SUM(LENGTH(ab.content)) FROM attachments a JOIN blobs ab ON ab.blobId=a.blobId WHERE a.ownerId=n.noteId AND a.isDeleted=0),0) FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.isDeleted=0",
                "contentAndAttachmentsAndRevisionsSize" => "SELECT n.noteId, COALESCE(LENGTH(b.content),0) + COALESCE((SELECT SUM(LENGTH(ab.content)) FROM attachments a JOIN blobs ab ON ab.blobId=a.blobId WHERE a.ownerId=n.noteId AND a.isDeleted=0),0) + COALESCE((SELECT SUM(LENGTH(rb.content)) FROM revisions r JOIN blobs rb ON rb.blobId=r.blobId WHERE r.noteId=n.noteId),0) FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.isDeleted=0",
                _ => "SELECT noteId, COUNT(*) FROM revisions GROUP BY noteId",
            };
            let mut m = HashMap::new();
            let mut st = self.db.conn.prepare(sql)?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))? {
                let (id, v) = r?;
                if let Some(&i) = self.c.idx.get(&id) { m.insert(i, v); }
            }
            self.db_cache.insert(prop, m);
        }
        Ok(&self.db_cache[prop])
    }

    fn prop_value(&mut self, prop: &'static str, i: u32) -> Result<String> {
        let c = self.c;
        let iu = i as usize;
        let rel_count = |attrs: &[&Attr], links: bool| attrs.iter().filter(|a| !a.label && (links || !LINK_RELATIONS.contains(&a.name_l.as_str()))).count();
        Ok(match prop {
            "noteId" => c.ids[iu].clone(),
            "title" => c.title[iu].to_lowercase(),
            "type" => c.ntype[iu].to_lowercase(),
            "mime" => c.mime[iu].to_lowercase(),
            "isProtected" => c.protected[iu].to_string(),
            "isArchived" => c.archived.has(i).to_string(),
            "dateCreated" => c.dates[iu][0].to_lowercase(), "dateModified" => c.dates[iu][1].to_lowercase(),
            "utcDateCreated" => c.dates[iu][2].to_lowercase(), "utcDateModified" => c.dates[iu][3].to_lowercase(),
            "parentCount" => c.parents[iu].len().to_string(),
            "childrenCount" => c.children[iu].len().to_string(),
            "attributeCount" => c.effective_attrs(i).len().to_string(),
            "labelCount" => c.effective_attrs(i).iter().filter(|a| a.label).count().to_string(),
            "ownedLabelCount" => c.attrs[iu].iter().filter(|a| a.label).count().to_string(),
            "relationCount" => rel_count(&c.effective_attrs(i), false).to_string(),
            "relationCountIncludingLinks" => rel_count(&c.effective_attrs(i), true).to_string(),
            "ownedRelationCount" => rel_count(&c.attrs[iu].iter().collect::<Vec<_>>(), false).to_string(),
            "ownedRelationCountIncludingLinks" => rel_count(&c.attrs[iu].iter().collect::<Vec<_>>(), true).to_string(),
            "targetRelationCount" => c.target_rels[iu].iter().filter(|(s, ai)| !LINK_RELATIONS.contains(&c.attrs[*s as usize][*ai as usize].name_l.as_str())).count().to_string(),
            "targetRelationCountIncludingLinks" => c.target_rels[iu].len().to_string(),
            "contentSize" | "contentAndAttachmentsSize" | "contentAndAttachmentsAndRevisionsSize" | "revisionCount" =>
                format!("{}", self.db_prop(prop)?.get(&i).copied().unwrap_or(0.0)),
            _ => String::new(),
        })
    }

    // ---------------------------------------------------------------- fulltext

    fn flat_text_match(&mut self, tokens: &[String], input: &NoteSet) -> NoteSet {
        let c = self.c;
        let n = c.len();
        let mut toks: Vec<String> = vec![];
        for t in tokens { let nt = normalize(t); if !toks.contains(&nt) { toks.push(nt); } }
        let mut out = NoteSet::empty(n);
        if toks.is_empty() || toks.len() > 63 { return out; }
        let full_mask: u64 = (1u64 << toks.len()) - 1;
        let own_mask = |note: u32| -> u64 {
            let mut m = 0u64;
            for (k, t) in toks.iter().enumerate() {
                let ty = c.ntype[note as usize].to_lowercase(); let mi = c.mime[note as usize].to_lowercase();
                if ty.contains(t.as_str()) || mi.contains(t.as_str()) { m |= 1 << k; }
                for a in &c.attrs[note as usize] { if normalize(&a.name).contains(t.as_str()) || normalize(&a.value).contains(t.as_str()) { m |= 1 << k; } }
            }
            m
        };
        let fuzzy = self.fuzzy;
        let title_mask = |note: u32, prefix: &Option<String>| -> u64 {
            let title = c.title_under(note, prefix);
            toks.iter().enumerate().filter(|(_, t)| title.contains(t.as_str()) || (fuzzy && super::text::fuzzy_match_word(t, &title))).fold(0u64, |m, (k, _)| m | (1 << k))
        };
        let fuzzy_cands: Vec<HashSet<u32>> = if fuzzy { toks.iter().map(|t| c.fuzzy_title_notes(t)).collect() } else { vec![] };
        // walk towards the root collecting the remaining tokens along one path (memoised: the answer
        // for (note, remaining) is the same whichever descendant asked)
        fn up(c: &Corpus, note: u32, remaining: u64, memo: &mut HashMap<(u32, u64), bool>, own: &dyn Fn(u32) -> u64, title: &dyn Fn(u32, &Option<String>) -> u64) -> bool {
            if remaining == 0 { return true; }
            if c.parents[note as usize].is_empty() || c.ids[note as usize] == "root" { return false; }
            if let Some(&r) = memo.get(&(note, remaining)) { return r; }
            memo.insert((note, remaining), false); // cycle guard
            let found_attr = own(note) & remaining;
            let mut res = false;
            for (p, pre) in &c.parents[note as usize] {
                let found = found_attr | (title(note, pre) & remaining);
                if up(c, *p, remaining & !found, memo, own, title) { res = true; break; }
            }
            memo.insert((note, remaining), res);
            res
        }
        let mut memo: HashMap<(u32, u64), bool> = HashMap::new();
        let single = toks.len() == 1;
        for note in input.iter() {
            if single && c.ids[note as usize].to_lowercase() == toks[0] { out.add(note); continue; }
            // candidate gate (cheap): some token must be in the note's own flat text
            let flat = c.flat_text(note);
            if !toks.iter().any(|t| flat.contains(t.as_str())) && !fuzzy_cands.iter().any(|s| s.contains(&note)) { continue; }
            let attr_found = own_mask(note);
            for (p, pre) in &c.parents[note as usize] {
                let found = attr_found | title_mask(note, pre);
                if found != 0 {
                    if up(c, *p, full_mask & !found, &mut memo, &own_mask, &title_mask) { out.add(note); break; }
                }
            }
        }
        out
    }

    /// Content search. Like Trilium, protected notes are searched only while the protected session is unlocked (their
    /// text is decrypted in memory for the scan); locked ones never match on content.
    fn content_match(&mut self, op: &str, tokens: &[String], raw: bool, flat_text: bool, input: &NoteSet) -> Result<NoteSet> {
        let c = self.c;
        let n = c.len();
        let prot = NoteSet::from_iter(n, input.iter().filter(|&i| c.protected[i as usize]));
        if prot.iter().next().is_none() { return self.content_match_inner(op, tokens, raw, flat_text, input, false); }
        let plain = NoteSet::from_iter(n, input.iter().filter(|&i| !c.protected[i as usize]));
        let out = self.content_match_inner(op, tokens, raw, flat_text, &plain, false)?;
        if !self.db.is_unlocked() { return Ok(out); }
        Ok(out.union(&self.content_match_inner(op, tokens, raw, flat_text, &prot, true)?))
    }

    fn content_match_inner(&mut self, op: &str, tokens: &[String], raw: bool, flat_text: bool, input: &NoteSet, protected_pass: bool) -> Result<NoteSet> {
        const ALLOWED: &[&str] = &["=", "!=", "*=*", "*=", "=*", "%=", "~=", "~*"];
        let c = self.c;
        let n = c.len();
        if !ALLOWED.contains(&op) {
            self.error.get_or_insert(format!("Note content can be searched only with operators: {}, operator {op} given.", ALLOWED.join(", ")));
            return Ok(input.clone());
        }
        for t in tokens { if !self.highlighted.contains(t) { self.highlighted.push(t.clone()); } }
        // Indexed prefilter: substring candidates from the trigram index instead of reading every note.
        let mut input = input;
        let narrowed;
        let mut extra_hits: Option<NoteSet> = None;
        if !protected_pass && !raw && matches!(op, "*=*" | "=") && !tokens.is_empty() && tokens.iter().all(|t| t.chars().count() >= 3) && self.fts_available() {
            let per_token: Vec<NoteSet> = tokens.iter().map(|t| -> Result<NoteSet> {
                let nt = if op == "*=*" { t.clone() } else { normalize(t) };
                let mut s = NoteSet::from_iter(n, super::index::notes_containing(self.db, &nt)?.iter().filter_map(|id| c.idx.get(id).copied()));
                if flat_text {
                    for i in input.iter() { if c.flat_text(i).contains(nt.as_str()) { s.add(i); } }
                }
                Ok(s)
            }).collect::<Result<_>>()?;
            if op == "*=*" {
                // every token is in the content or (for the default fulltext) in the note's flat text
                let mut cur = input.clone();
                for s in &per_token { cur = cur.intersect(s); }
                self.deferred_quality.extend(cur.iter());
                return Ok(cur);
            }
            // exact word/phrase matching: the index narrows the notes whose CONTENT needs the exact check;
            // matches in the flat text (title, attributes, ...) are decided in memory
            let mut cand = input.clone();
            for t in tokens.iter() {
                let nt = normalize(t);
                let s = NoteSet::from_iter(n, super::index::notes_containing(self.db, &nt)?.iter().filter_map(|id| c.idx.get(id).copied()));
                cand = cand.intersect(&s);
            }
            if flat_text {
                let mut hits = NoteSet::empty(n);
                for i in input.iter() { if contains_exact_phrase(tokens, c.flat_text(i)) { hits.add(i); } }
                extra_hits = Some(hits);
            }
            narrowed = cand;
            input = &narrowed;
        }
        let regex = if op == "%=" { tokens.first().and_then(|t| regex::RegexBuilder::new(t).multi_line(true).dot_matches_new_line(true).build().ok()) } else { None };
        let fuzzy = |t: &str, content: &str| -> bool {
            let nt = normalize(t); let nc = normalize(content);
            nc.contains(&nt) || super::text::fuzzy_match_word(&nt, &nc)
        };
        let mut out = NoteSet::empty(n);
        let mut check = |i: u32, ntype: &str, mime: &str, raw_content: &str, exec: &mut Exec| {
            let content = preprocess_content(raw_content, ntype, mime, raw);
            let flat = if flat_text { c.flat_text(i) } else { "" };
            let (matched, negation) = if tokens.len() == 1 {
                let t = &tokens[0];
                match op {
                    "=" => (contains_exact_word(t, &content) || (flat_text && contains_exact_phrase(&[t.clone()], flat)), false),
                    "!=" => (!contains_exact_word(t, &content) && !(flat_text && contains_exact_phrase(&[t.clone()], flat)), true),
                    "*=" => (content.ends_with(t.as_str()), false),
                    "=*" => (content.starts_with(t.as_str()), false),
                    "*=*" => (content.contains(t.as_str()), false),
                    "%=" => (regex.as_ref().is_some_and(|r| r.is_match(&content)), false),
                    _ => (fuzzy(t, &content), false),
                }
            } else if op == "=" || op == "!=" {
                let m = contains_exact_phrase(tokens, &content) || (flat_text && contains_exact_phrase(tokens, flat));
                if op == "=" { (m, false) } else { (!m, true) }
            } else if op == "~=" || op == "~*" {
                (tokens.iter().all(|t| fuzzy(t, &content)), false)
            } else {
                (tokens.iter().all(|t| { let nt = normalize(t); content.contains(&nt) || (flat_text && flat.contains(t.as_str())) }), false)
            };
            if matched {
                out.add(i);
                if !negation { exec.record_quality(i, tokens, &content); }
            }
        };
        if protected_pass {
            // decrypt each protected note's text for this scan only (nothing is indexed or cached)
            let mut st = self.db.conn.prepare("SELECT noteId, type, mime, content FROM (SELECT n.noteId, n.type, n.mime, b.content FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.isProtected=1 AND n.isDeleted=0 AND n.type IN ('text','code','mermaid','canvas','mindMap','spreadsheet','llmChat') AND LENGTH(b.content) < 4194304)")?;
            let rows: Vec<(String, String, String, rusqlite::types::Value)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, ty, mi, v) in rows {
                let Some(&i) = c.idx.get(&id) else { continue };
                if !input.has(i) { continue; }
                let Ok(bytes) = self.db.plain_bytes(true, v) else { continue };
                check(i, &ty, &mi, &String::from_utf8_lossy(&bytes), self);
            }
        }
        // iterate content rows
        let in_len = if protected_pass { 0 } else { input.len() };
        let types = "('text','code','mermaid','canvas','mindMap','spreadsheet','llmChat')";
        let base = format!("SELECT n.noteId, n.type, n.mime, b.content FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.type IN {types} AND n.isDeleted=0 AND n.isProtected=0 AND LENGTH(b.content) < 2097152");
        let to_s = |v: rusqlite::types::Value| -> String { match v { rusqlite::types::Value::Text(s) => s, rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(), _ => String::new() } };
        if in_len > 0 && in_len <= 20000 {
            let ids: Vec<u32> = input.iter().collect();
            for chunk in ids.chunks(500) {
                let ph = vec!["?"; chunk.len()].join(",");
                let mut st = self.db.conn.prepare(&format!("{base} AND n.noteId IN ({ph})"))?;
                let params: Vec<&str> = chunk.iter().map(|&i| c.ids[i as usize].as_str()).collect();
                let rows: Vec<(String, String, String, String)> = st.query_map(rusqlite::params_from_iter(params), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, to_s(r.get(3)?))))?.collect::<rusqlite::Result<_>>()?;
                for (id, ty, mi, content) in rows { if let Some(&i) = c.idx.get(&id) { check(i, &ty, &mi, &content, self); } }
            }
        } else if in_len > 0 {
            let mut st = self.db.conn.prepare(&base)?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let id: String = r.get(0)?;
                let Some(&i) = c.idx.get(&id) else { continue };
                if !input.has(i) { continue; }
                let (ty, mi): (String, String) = (r.get(1)?, r.get(2)?);
                let content = to_s(r.get(3)?);
                check(i, &ty, &mi, &content, self);
            }
        }
 if let Some(h) = extra_hits.take() { out = out.union(&h); }
        // notes without content rows can still match "!=" (nothing contains the token)
        if op == "!=" {
            for i in input.iter() { if !out.has(i) && !c.ntype[i as usize].is_empty() && content_less(&c.ntype[i as usize]) { out.add(i); } }
        }
        Ok(out)
    }

    pub fn fts_state(&self) -> Option<bool> { self.fts_ok }

    /// Typo-tolerant body matching: every token is a substring of the content or fuzzy-matches one of
    /// its words. Reads content, so it only runs on small corpora (see `FUZZY_CONTENT_BUDGET`).
    fn fuzzy_content(&mut self, tokens: &[String], input: &NoteSet, exact: &NoteSet) -> Result<NoteSet> {
        const FUZZY_CONTENT_BUDGET: i64 = 25 * 1024 * 1024;
        let c = self.c;
        let n = c.len();
        let mut out = NoteSet::empty(n);
        if tokens.iter().all(|t| super::text::auto_max_edit_distance(t.chars().count()) == 0) { return Ok(out); }
        let bytes: i64 = if self.fts_available() {
            self.db.conn.query_row("SELECT COALESCE(SUM(LENGTH(text)),0) FROM tui_fts", [], |r| r.get(0)).unwrap_or(i64::MAX)
        } else { i64::MAX };
        if bytes > FUZZY_CONTENT_BUDGET { return Ok(out); }
        let unlocked = self.db.is_unlocked();
        let mut st = self.db.conn.prepare(&format!("SELECT n.noteId, n.type, n.mime, b.content, n.isProtected FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.type IN ('text','code','mermaid','canvas','mindMap') AND n.isDeleted=0 AND n.isProtected IN (0{}) AND LENGTH(b.content) < 2097152", if unlocked { ",1" } else { "" }))?;
        let mut rows = st.query([])?;
        let nts: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let Some(&i) = c.idx.get(&id) else { continue };
            if !input.has(i) || exact.has(i) { continue; }
            let (ty, mi): (String, String) = (r.get(1)?, r.get(2)?);
            let protected = r.get::<_, i64>(4)? != 0;
            let value = r.get::<_, rusqlite::types::Value>(3)?;
            let raw: String = if protected { match self.db.plain_bytes(true, value) { Ok(b) => String::from_utf8_lossy(&b).into_owned(), Err(_) => continue } }
                else { match value { rusqlite::types::Value::Text(s) => s, rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(), _ => continue } };
            let content = preprocess_content(&raw, &ty, &mi, false);
            if nts.iter().all(|t| content.contains(t.as_str()) || super::text::fuzzy_match_word(t, &content)) {
                out.add(i);
                self.content_quality.entry(i).or_insert(ContentTier::Fuzzy);
            }
        }
        Ok(out)
    }

    fn fts_available(&mut self) -> bool {
        if self.fts_ok.is_none() {
            let ok = super::index::ensure_schema(self.db) && super::index::ensure_index(self.db).is_ok();
            self.fts_ok = Some(ok);
        }
        self.fts_ok == Some(true)
    }

    /// Content-match tier for notes matched through the index (computed only for the top results).
    pub fn compute_quality(&mut self, notes: &[u32], tokens: &[String]) -> Result<()> {
        let c = self.c;
        for chunk in notes.chunks(300) {
            let ph = vec!["?"; chunk.len()].join(",");
            let mut st = self.db.conn.prepare(&format!("SELECT n.noteId, n.type, n.mime, b.content FROM notes n JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId IN ({ph}) AND n.isProtected IN (0{})", if self.db.is_unlocked() { ",1" } else { "" }))?;
            let params: Vec<&str> = chunk.iter().map(|&i| c.ids[i as usize].as_str()).collect();
            let rows: Vec<(String, String, String, rusqlite::types::Value)> = st.query_map(rusqlite::params_from_iter(params), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, ty, mi, v) in rows {
                let protected = c.idx.get(&id).is_some_and(|&i| c.protected[i as usize]);
                let raw = if protected { match self.db.plain_bytes(true, v) { Ok(b) => String::from_utf8_lossy(&b).into_owned(), Err(_) => continue } }
                    else { match v { rusqlite::types::Value::Text(s) => s, rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(), _ => String::new() } };
                if let Some(&i) = c.idx.get(&id) { let content = preprocess_content(&raw, &ty, &mi, false); self.record_quality(i, tokens, &content); }
            }
        }
        Ok(())
    }

    fn record_quality(&mut self, i: u32, tokens: &[String], content: &str) {
        let words = tokenize_normalized(content);
        let qw: Vec<String> = tokens.iter().flat_map(|t| super::text::tokenize(t)).collect();
        if qw.is_empty() { return; }
        let tier = if qw.len() > 1 && words_contain_phrase(&words, &qw) { ContentTier::ExactPhrase }
            else if qw.iter().all(|q| words.iter().any(|w| w == q)) { if qw.len() > 1 { ContentTier::Proximity } else { ContentTier::ExactWord } }
            else if qw.iter().all(|q| words.iter().any(|w| w.starts_with(q.as_str()))) { ContentTier::WordPrefix }
            else { ContentTier::Substring };
        let e = self.content_quality.entry(i).or_insert(tier);
        if tier > *e { *e = tier; }
    }
}

fn content_less(t: &str) -> bool { matches!(t, "book" | "image" | "file" | "render" | "search" | "relationMap" | "noteMap" | "webView" | "launcher" | "doc" | "aiChat") }

fn is_link(name_l: &str) -> bool {
    let n = name_l.strip_prefix("disabled:").unwrap_or(name_l);
    LINK_RELATIONS.contains(&n) || !BUILTIN_RELATIONS.contains(&n)
}

/// Built-in relation names (lower-cased), from `commons/lib/builtin_attributes.ts`.
const BUILTIN_RELATIONS: &[&str] = &["board:cardredirectto", "boardcardredirectto", "internallink", "imagelink", "relationmaplink", "includenotelink", "runonnotecreation", "runonnotetitlechange", "runonnotechange", "runonnotecontentchange", "runonnotedeletion", "runonbranchcreation", "runonbranchchange", "runonbranchdeletion", "runonchildnotecreation", "runonattributecreation", "runonattributechange", "map:image", "template", "template:newnotedefaultparent", "inherit", "datetemplate", "weektemplate", "monthtemplate", "quartertemplate", "yeartemplate", "widget", "rendernote", "searchscript", "ancestor", "target", "script", "hoistednote", "sharecss", "sharelogo", "shareopengraphimage", "sharejs", "sharehtml", "sharetemplate", "sharefavicon"];

fn contains_exact_word(token: &str, content: &str) -> bool {
    let nt = normalize(token);
    if nt.contains(' ') { return words_contain_phrase(&super::text::tokenize(content), &super::text::tokenize(&nt)); }
    let needle = strip_word_punctuation(&nt).to_string();
    tokenize_normalized(&normalize(content)).iter().any(|w| *w == needle)
}

fn contains_exact_phrase(tokens: &[String], content: &str) -> bool {
    let phrase = tokens.iter().map(|t| normalize(t)).collect::<Vec<_>>().join(" ");
    if !phrase.contains(' ') { return tokenize_normalized(&normalize(content)).iter().any(|w| *w == strip_word_punctuation(&phrase)); }
    words_contain_phrase(&super::text::tokenize(content), &super::text::tokenize(&phrase))
}

/// `compareValues`: dates chronologically, numbers numerically, else text; empty values tie.
pub fn compare_values(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        // a missing value sorts after every present one in ascending order (upstream: compareSortValues)
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => {
            if x.is_empty() && y.is_empty() { return Equal; }
            let is_date = |s: &str| s.len() >= 8 && s.as_bytes()[..4].iter().all(u8::is_ascii_digit) && s.as_bytes()[4] == b'-';
            if is_date(x) && is_date(y) { return x.cmp(y); }
            if let (Some(p), Some(q)) = (parse_float(x), parse_float(y)) { return p.partial_cmp(&q).unwrap_or(Equal); }
            x.cmp(y)
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &Cmp, _: &SearchOpts) {}
