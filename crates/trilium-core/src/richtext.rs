//! Structural ("edit in place") editing of Trilium note HTML.
//!
//! The note stays an `htmltree` tree. A cursor is `(textblock, unit offset)`, where a *textblock* is a run of inline
//! siblings (a paragraph's content, a list item's own text, ...) and a *unit* is one character, `<br>` or atom (image,
//! note link, ...). Every edit changes only the nodes of the textblock it touches, so everything else serialises
//! byte-for-byte as it was stored. Elements this module does not understand (tables, rules, included notes) are
//! *opaque*: the cursor skips them and they are never altered.
use crate::htmltree::{self, decode_entities, El, Node};

pub const MAX_DEPTH: usize = 48;
/// Larger notes are edited as source (the model re-walks the tree after every edit).
pub const MAX_BYTES: usize = 400_000;
const NBSP: char = '\u{a0}';

const INLINE: &[&str] = &[
    "a", "abbr", "acronym", "b", "bdi", "bdo", "big", "cite", "code", "data", "dfn", "em", "font", "i", "ins", "kbd", "label", "mark", "q", "s", "samp", "small",
    "span", "strike", "strong", "sub", "sup", "time", "tt", "u", "var", "del", "wbr",
];
const OPAQUE: &[&str] = &["table", "hr", "iframe", "svg", "canvas", "video", "audio", "object", "embed", "script", "style", "math", "template", "head", "title", "noscript"];

pub mod mark {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 2;
    pub const UNDERLINE: u16 = 4;
    pub const STRIKE: u16 = 8;
    pub const CODE: u16 = 16;
    pub const HIGHLIGHT: u16 = 32;
    pub const LINK: u16 = 64;
    pub const SUB: u16 = 128;
    pub const SUP: u16 = 256;

    pub fn tag(flag: u16) -> &'static str {
        match flag { BOLD => "strong", ITALIC => "i", UNDERLINE => "u", STRIKE => "s", CODE => "code", HIGHLIGHT => "mark", LINK => "a", SUB => "sub", SUP => "sup", _ => "span" }
    }
}

fn mark_of(name: &str) -> u16 {
    match name {
        "strong" | "b" => mark::BOLD,
        "em" | "i" => mark::ITALIC,
        "u" | "ins" => mark::UNDERLINE,
        "s" | "del" | "strike" => mark::STRIKE,
        "code" | "tt" | "kbd" | "samp" => mark::CODE,
        "mark" => mark::HIGHLIGHT,
        "a" => mark::LINK,
        "sub" => mark::SUB,
        "sup" => mark::SUP,
        _ => 0,
    }
}

fn has_class(e: &El, c: &str) -> bool { e.attrs.iter().any(|(k, v)| k == "class" && v.as_deref().is_some_and(|v| v.split_whitespace().any(|x| x == c))) }
fn attr<'a>(e: &'a El, name: &str) -> Option<&'a str> { e.attrs.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref()) }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind { Text, Break, Atom, Skip, Inline, Opaque, Container }

fn kind_of(n: &Node) -> Kind {
    match n {
        Node::Text(_) => Kind::Text,
        Node::El(e) => {
            let name = e.name.as_str();
            if name == "br" { Kind::Break }
            else if name == "input" { Kind::Skip }
            else if OPAQUE.contains(&name) || has_class(e, "include-note") { Kind::Opaque }
            else if matches!(name, "img" | "en-media") || has_class(e, "math-tex") || has_class(e, "reference-link") { Kind::Atom }
            else if INLINE.contains(&name) { Kind::Inline }
            else { Kind::Container }
        }
    }
}

fn is_block_node(n: &Node) -> bool { matches!(kind_of(n), Kind::Container | Kind::Opaque) }

/// Inline elements that must survive being emptied (a to-do item's text holder), so there is still somewhere to type.
fn keep_empty(e: &El) -> bool { has_class(e, "todo-list__label__description") || has_class(e, "todo-list__label") }

fn dec(s: &str) -> Vec<char> { decode_entities(s).chars().collect() }
fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c { '&' => o.push_str("&amp;"), '<' => o.push_str("&lt;"), '>' => o.push_str("&gt;"), NBSP => o.push_str("&nbsp;"), c => o.push(c) }
    }
    o
}
fn text_node(s: &str) -> Node { Node::Text(enc(s)) }
fn is_spacey(c: char) -> bool { c == ' ' || c == NBSP || c == '\n' || c == '\t' }

fn unit_len(n: &Node) -> usize {
    match n {
        Node::Text(t) => dec(t).len(),
        Node::El(e) => match kind_of(n) {
            Kind::Break | Kind::Atom | Kind::Opaque => 1,
            Kind::Skip => 0,
            _ => e.kids.iter().map(unit_len).sum(),
        },
    }
}

fn is_phantom(nodes: &[Node]) -> bool { matches!(nodes, [Node::Text(t)] if dec(t) == [NBSP]) }

fn visible(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Text(t) => decode_entities(t).chars().any(|c| !c.is_ascii_whitespace()),
        Node::El(e) => match kind_of(n) { Kind::Break | Kind::Atom | Kind::Opaque => true, Kind::Skip => false, _ => visible(&e.kids) },
    })
}

fn kids_at<'a>(nodes: &'a [Node], path: &[usize]) -> Option<&'a [Node]> {
    let mut cur = nodes;
    for &i in path { let Node::El(e) = cur.get(i)? else { return None }; cur = &e.kids; }
    Some(cur)
}
fn kids_at_mut<'a>(nodes: &'a mut Vec<Node>, path: &[usize]) -> Option<&'a mut Vec<Node>> {
    let mut cur = nodes;
    for &i in path { let Some(Node::El(e)) = cur.get_mut(i) else { return None }; cur = &mut e.kids; }
    Some(cur)
}
fn node_at<'a>(nodes: &'a [Node], path: &[usize]) -> Option<&'a Node> {
    let (last, parent) = path.split_last()?;
    kids_at(nodes, parent)?.get(*last)
}
fn node_at_mut<'a>(nodes: &'a mut Vec<Node>, path: &[usize]) -> Option<&'a mut Node> {
    let (last, parent) = path.split_last()?;
    kids_at_mut(nodes, parent)?.get_mut(*last)
}
fn el_at<'a>(nodes: &'a [Node], path: &[usize]) -> Option<&'a El> { if let Node::El(e) = node_at(nodes, path)? { Some(e) } else { None } }

fn whitespace_only(nodes: &[Node]) -> bool { nodes.iter().all(|n| matches!(n, Node::Text(t) if decode_entities(t).chars().all(|c| c.is_ascii_whitespace()))) }

/// Remove the element at `path` if it holds nothing but whitespace, and keep going up while parents become empty too.
fn prune_empty(base: &mut Vec<Node>, mut path: Vec<usize>) {
    while !path.is_empty() {
        if !kids_at(base, &path).is_some_and(whitespace_only) { break; }
        if matches!(node_at(base, &path), Some(Node::El(e)) if matches!(e.name.as_str(), "td" | "th" | "tr" | "tbody" | "thead" | "tfoot" | "table")) { break; }
        let (l, pp) = path.split_last().unwrap();
        let (l, pp) = (*l, pp.to_vec());
        if let Some(k) = kids_at_mut(base, &pp) { if l < k.len() { k.remove(l); } }
        path.pop();
    }
}

/// Split a node sequence at unit offset `k`; elements straddling the cut are closed and re-opened on both sides.
fn split_nodes(nodes: Vec<Node>, mut k: usize) -> (Vec<Node>, Vec<Node>) {
    let mut left = vec![];
    let mut right = vec![];
    let mut rest = nodes.into_iter();
    while let Some(n) = rest.next() {
        let len = unit_len(&n);
        if k > 0 && k >= len { k -= len; left.push(n); continue; }
        if k == 0 { right.push(n); break; }
        match n {
            Node::Text(t) => {
                let chars = dec(&t);
                left.push(text_node(&chars[..k].iter().collect::<String>()));
                right.push(text_node(&chars[k..].iter().collect::<String>()));
            }
            Node::El(e) => {
                let (lk, rk) = split_nodes(e.kids, k);
                left.push(Node::El(El { name: e.name.clone(), attrs: e.attrs.clone(), kids: lk, self_close: false }));
                right.push(Node::El(El { name: e.name, attrs: e.attrs, kids: rk, self_close: false }));
            }
        }
        break;
    }
    right.extend(rest);
    (left, right)
}

/// Remove units `lo..hi` of a node sequence in place; structure that must survive being emptied (a to-do item's holders) stays.
fn delete_units(nodes: Vec<Node>, lo: usize, hi: usize) -> Vec<Node> {
    let mut out = vec![];
    let mut pos = 0;
    for n in nodes {
        let len = unit_len(&n);
        let (s, e) = (pos, pos + len);
        pos = e;
        if len == 0 || e <= lo || s >= hi { out.push(n); continue; }
        let (ls, le) = (lo.saturating_sub(s), hi.min(e) - s);
        let kind = kind_of(&n);
        match n {
            Node::Text(t) => {
                let mut chars = dec(&t);
                chars.drain(ls..le.min(chars.len()));
                if !chars.is_empty() { out.push(text_node(&chars.iter().collect::<String>())); }
            }
            Node::El(mut el) => {
                if matches!(kind, Kind::Break | Kind::Atom | Kind::Opaque) { continue; }
                el.kids = delete_units(std::mem::take(&mut el.kids), ls, le);
                out.push(Node::El(el));
            }
        }
    }
    out
}

/// Apply `f` to a textblock's inline content (inside a to-do item's text holder rather than around it).
fn with_content(mut run: Vec<Node>, f: impl FnOnce(Vec<Node>) -> Vec<Node>) -> Vec<Node> {
    if todo_label(&run).is_some() {
        if let Some(Node::El(label)) = run.first_mut() {
            if let Some(Node::El(span)) = label.kids.iter_mut().find(|k| matches!(k, Node::El(e) if has_class(e, "todo-list__label__description"))) {
                span.kids = f(std::mem::take(&mut span.kids));
                return run;
            }
        }
    }
    f(run)
}

/// A space at the very edge of a block would collapse away in HTML: store it as a non-breaking one.
fn edge_path(nodes: &[Node], first: bool, path: &mut Vec<usize>) -> bool {
    let order: Vec<usize> = if first { (0..nodes.len()).collect() } else { (0..nodes.len()).rev().collect() };
    for i in order {
        match (&nodes[i], kind_of(&nodes[i])) {
            (Node::Text(t), _) if !t.is_empty() => { path.push(i); return true; }
            (Node::El(e), Kind::Inline) => { path.push(i); if edge_path(&e.kids, first, path) { return true; } path.pop(); }
            (Node::El(_), Kind::Skip) | (Node::Text(_), _) => {}
            _ => return false,
        }
    }
    false
}
fn nbsp_edge(nodes: &mut Vec<Node>, first: bool) {
    let mut p = vec![];
    if !edge_path(nodes, first, &mut p) { return; }
    if let Some(Node::Text(t)) = node_at_mut(nodes, &p) {
        if first && t.starts_with(' ') { *t = format!("&nbsp;{}", &t[1..]); }
        else if !first && t.ends_with(' ') { t.pop(); t.push_str("&nbsp;"); }
    }
}
fn fix_halves(l: &mut Vec<Node>, r: &mut Vec<Node>) { nbsp_edge(l, false); nbsp_edge(r, true); }

fn simple_mergeable(a: &El, b: &El) -> bool {
    a.name == b.name && a.attrs == b.attrs && mark_of(&a.name) != 0 && kind_of(&Node::El(El { name: a.name.clone(), attrs: a.attrs.clone(), kids: vec![], self_close: false })) == Kind::Inline
}

/// Drop empty text/inline nodes and merge neighbours that only differ by a split.
fn tidy(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    for n in nodes {
        match n {
            Node::Text(t) => {
                if t.is_empty() { continue; }
                if let Some(Node::Text(p)) = out.last_mut() { p.push_str(&t); } else { out.push(Node::Text(t)); }
            }
            Node::El(mut e) => {
                let kind = kind_of(&Node::El(El { name: e.name.clone(), attrs: e.attrs.clone(), kids: vec![], self_close: false }));
                e.kids = tidy(std::mem::take(&mut e.kids));
                if kind == Kind::Inline && e.kids.is_empty() && !keep_empty(&e) { continue; }
                if let Some(Node::El(p)) = out.last_mut() {
                    if simple_mergeable(p, &e) {
                        p.kids.append(&mut e.kids);
                        p.kids = tidy(std::mem::take(&mut p.kids));
                        continue;
                    }
                }
                out.push(Node::El(e));
            }
        }
    }
    out
}

fn unwrap_flag(nodes: Vec<Node>, flag: u16) -> Vec<Node> {
    let mut out = vec![];
    for n in nodes {
        let k = kind_of(&n);
        match n {
            Node::El(mut e) if k == Kind::Inline => {
                e.kids = unwrap_flag(std::mem::take(&mut e.kids), flag);
                if mark_of(&e.name) == flag { out.extend(e.kids); } else { out.push(Node::El(e)); }
            }
            other => out.push(other),
        }
    }
    out
}

fn new_el(name: &str, attrs: &[(&str, &str)], kids: Vec<Node>) -> Node {
    Node::El(El { name: name.into(), attrs: attrs.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect(), kids, self_close: false })
}
fn phantom() -> Vec<Node> { vec![Node::Text("&nbsp;".into())] }

fn find_checkbox(nodes: &[Node]) -> Option<bool> {
    for n in nodes {
        if let Node::El(e) = n {
            if e.name == "input" && attr(e, "type").is_some_and(|t| t.eq_ignore_ascii_case("checkbox")) { return Some(e.attrs.iter().any(|(k, _)| k == "checked")); }
            if let Some(c) = find_checkbox(&e.kids) { return Some(c); }
        }
    }
    None
}
fn toggle_input(nodes: &mut [Node]) -> bool {
    for n in nodes {
        if let Node::El(e) = n {
            if e.name == "input" && attr(e, "type").is_some_and(|t| t.eq_ignore_ascii_case("checkbox")) {
                if e.attrs.iter().any(|(k, _)| k == "checked") { e.attrs.retain(|(k, _)| k != "checked"); } else { let at = e.attrs.iter().position(|(k, _)| k == "type").map_or(e.attrs.len(), |p| p + 1);
                    e.attrs.insert(at, ("checked".into(), Some("checked".into()))); }
                return true;
            }
            if toggle_input(&mut e.kids) { return true; }
        }
    }
    false
}

fn todo_label(run: &[Node]) -> Option<&El> {
    match run { [Node::El(e)] if e.name == "label" && has_class(e, "todo-list__label") => Some(e), _ => None }
}

fn atom_info(e: &El) -> AtomInfo {
    let src = (e.name == "img").then(|| attr(e, "src").map(decode_entities)).flatten();
    let href = (e.name == "a").then(|| attr(e, "href").map(decode_entities)).flatten();
    AtomInfo { label: atom_label(e), src, href }
}

fn atom_label(e: &El) -> String {
    match e.name.as_str() {
        "img" => {
            let alt = attr(e, "alt").filter(|a| !a.is_empty()).map(|a| decode_entities(a))
                .or_else(|| attr(e, "src").and_then(|s| s.rsplit('/').next()).map(decode_entities)).unwrap_or_else(|| "image".into());
            format!("[image: {alt}]")
        }
        "en-media" => "[media]".into(),
        _ if has_class(e, "math-tex") => "[math]".into(),
        _ if has_class(e, "reference-link") => { let t = htmltree::text_content(&e.kids); if t.trim().is_empty() { "[note link]".into() } else { t } }
        _ => "[object]".into(),
    }
}

// ------------------------------------------------------------------ segments

#[derive(Clone, Debug)]
pub enum SegKind { Text(Vec<char>), Atom(AtomInfo), Br, Anchor }

/// An unbreakable inline object: how to show it, plus the picture source or link target it carries.
#[derive(Clone, Debug)]
pub struct AtomInfo { pub label: String, pub src: Option<String>, pub href: Option<String> }

#[derive(Clone, Debug)]
pub struct Seg {
    /// Index path from the textblock's parent: first entry indexes the parent's kids.
    pub path: Vec<usize>,
    pub kind: SegKind,
    pub marks: u16,
    /// Depth (index into `path`) of the enclosing `<a>`.
    pub link: Option<usize>,
}

impl Seg {
    fn len(&self) -> usize { match &self.kind { SegKind::Text(c) => c.len(), SegKind::Atom (_) | SegKind::Br => 1, SegKind::Anchor => 0 } }
}

fn build_segs(run: &[Node], base: usize) -> Vec<Seg> {
    let mut out = vec![];
    if is_phantom(run) { return out; }
    let mut path = vec![];
    for (i, n) in run.iter().enumerate() {
        path.push(base + i);
        walk_seg(n, &mut path, 0, None, &mut out);
        path.pop();
    }
    out
}

fn walk_seg(n: &Node, path: &mut Vec<usize>, marks: u16, link: Option<usize>, out: &mut Vec<Seg>) {
    match (kind_of(n), n) {
        (Kind::Text, Node::Text(t)) => {
            let chars = dec(t);
            if !chars.is_empty() { out.push(Seg { path: path.clone(), kind: SegKind::Text(chars), marks, link }); }
        }
        (Kind::Break, _) => out.push(Seg { path: path.clone(), kind: SegKind::Br, marks, link }),
        (Kind::Atom, Node::El(e)) => out.push(Seg { path: path.clone(), kind: SegKind::Atom(atom_info(e)), marks, link }),
        (Kind::Opaque, _) => out.push(Seg { path: path.clone(), kind: SegKind::Atom(AtomInfo { label: "[object]".into(), src: None, href: None }), marks, link }),
        (Kind::Inline | Kind::Container, Node::El(e)) => {
            let m = marks | mark_of(&e.name);
            let l = if e.name == "a" { Some(path.len() - 1) } else { link };
            if e.kids.is_empty() {
                out.push(Seg { path: path.clone(), kind: SegKind::Anchor, marks: m, link: l });
            } else {
                for (i, k) in e.kids.iter().enumerate() {
                    path.push(i);
                    walk_seg(k, path, m, l, out);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

fn segs_len(segs: &[Seg]) -> usize { segs.iter().map(Seg::len).sum() }

fn unit_char(segs: &[Seg], k: usize) -> Option<char> {
    let mut pos = 0;
    for s in segs {
        let l = s.len();
        if k < pos + l {
            return Some(match &s.kind { SegKind::Text(c) => c[k - pos], SegKind::Br => '\n', _ => '\u{fffc}' });
        }
        pos += l;
    }
    None
}

fn same_link(a: &Seg, b: &Seg) -> bool {
    match (a.link, b.link) { (Some(x), Some(y)) => a.path[..=x] == b.path[..=y], _ => false }
}

/// One cursor step of a textblock, with what the view needs to draw it.
#[derive(Clone, Debug)]
pub struct Unit {
    pub ch: char,
    pub label: Option<String>,
    /// picture source of an image, link target of a note link
    pub src: Option<String>,
    pub href: Option<String>,
    pub marks: u16,
    pub br: bool,
}

// ------------------------------------------------------------------ outline

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Path of the element holding the run (empty = the note's top level).
    pub parent: Vec<usize>,
    pub start: usize,
    pub end: usize,
    pub pre: bool,
    /// An opaque element (table, rule, ...) sits between the previous textblock and this one.
    pub after_opaque: bool,
    /// Path of the table cell (`td`/`th`) this textblock lives in.
    pub cell: Option<Vec<usize>>,
}

#[derive(Clone, Debug)]
pub enum Anc { Quote, Li { ordinal: Option<usize>, first: bool } }

#[derive(Clone, Debug)]
pub struct BlockInfo { pub block: Block, pub anc: Vec<Anc>, pub heading: u8, pub checkbox: Option<bool> }

#[derive(Clone, Debug)]
pub struct TableRow { pub header: bool, pub cells: Vec<Vec<Item>> }

#[derive(Clone, Debug)]
pub enum Item { Block(usize), Opaque { html: String, anc: Vec<Anc> }, Table(Vec<TableRow>) }

#[derive(Clone, Debug, Default)]
pub struct Outline { pub blocks: Vec<BlockInfo>, pub items: Vec<Item> }

#[derive(Clone)]
enum Frame { Quote, Li { ordinal: Option<usize>, emitted: bool } }

struct Walk { blocks: Vec<BlockInfo>, items: Vec<Item>, frames: Vec<Frame>, after_opaque: bool, cell: Option<Vec<usize>> }

fn heading_of(name: &str) -> u8 { match name { "h1" => 1, "h2" => 2, "h3" => 3, "h4" => 4, "h5" => 5, "h6" => 6, _ => 0 } }

/// One table row: where it is (relative to the table's kids), whether it is a header, and which of its kids are cells.
struct RowShape { tr: Vec<usize>, header: bool, cells: Vec<usize> }

fn shape_rows(kids: &[Node], base: &[usize], in_head: bool, rows: &mut Vec<RowShape>) -> Option<()> {
    for (i, n) in kids.iter().enumerate() {
        match n {
            Node::Text(t) if decode_entities(t).chars().all(|c| c.is_ascii_whitespace()) => {}
            Node::El(e) => {
                let mut path = base.to_vec();
                path.push(i);
                match e.name.as_str() {
                    "thead" | "tbody" | "tfoot" => shape_rows(&e.kids, &path, in_head || e.name == "thead", rows)?,
                    "colgroup" | "col" => {}
                    "tr" => {
                        let mut cells = vec![];
                        let mut all_th = true;
                        for (ci, c) in e.kids.iter().enumerate() {
                            match c {
                                Node::Text(t) if decode_entities(t).chars().all(|c| c.is_ascii_whitespace()) => {}
                                Node::El(ce) if ce.name == "td" || ce.name == "th" => { all_th &= ce.name == "th"; cells.push(ci); }
                                _ => return None,
                            }
                        }
                        if cells.is_empty() || cells.len() > 16 { return None; }
                        rows.push(RowShape { tr: path, header: in_head || all_th, cells });
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(())
}

/// A table whose cells can be edited in place: plain rows of cells, no nested tables.
fn table_shape(t: &El) -> Option<Vec<RowShape>> {
    fn has_table(nodes: &[Node]) -> bool { nodes.iter().any(|n| matches!(n, Node::El(e) if e.name == "table" || has_table(&e.kids))) }
    if has_table(&t.kids) { return None; }
    let mut rows = vec![];
    shape_rows(&t.kids, &[], false, &mut rows)?;
    if rows.is_empty() { None } else { Some(rows) }
}

impl Walk {
    fn table(&mut self, t: &El, shape: &[RowShape], path: &mut Vec<usize>) {
        let mut rows = vec![];
        for rs in shape {
            let Some(Node::El(tr)) = kids_at(&t.kids, &rs.tr[..rs.tr.len() - 1]).and_then(|k| k.get(*rs.tr.last().unwrap())) else { continue };
            let mut cells = vec![];
            for &ci in &rs.cells {
                let Node::El(cell) = &tr.kids[ci] else { continue };
                let mut cell_path = path.clone();
                cell_path.extend(&rs.tr);
                cell_path.push(ci);
                let at = self.items.len();
                self.cell = Some(cell_path.clone());
                self.container(&cell.kids, &mut cell_path, &cell.name, false, 0);
                self.cell = None;
                cells.push(self.items.split_off(at));
            }
            rows.push(TableRow { header: rs.header, cells });
        }
        self.items.push(Item::Table(rows));
    }

    fn anc(&self, markers: bool) -> Vec<Anc> {
        self.frames.iter().map(|f| match f {
            Frame::Quote => Anc::Quote,
            Frame::Li { ordinal, emitted } => Anc::Li { ordinal: *ordinal, first: markers && !*emitted },
        }).collect()
    }

    fn emit(&mut self, kids: &[Node], path: &[usize], start: usize, end: usize, pre: bool, heading: u8) {
        let anc = self.anc(true);
        let checkbox = find_checkbox(&kids[start..end]);
        for f in &mut self.frames { if let Frame::Li { emitted, .. } = f { *emitted = true; } }
        self.items.push(Item::Block(self.blocks.len()));
        self.blocks.push(BlockInfo { block: Block { parent: path.to_vec(), start, end, pre, after_opaque: self.after_opaque, cell: self.cell.clone() }, anc, heading, checkbox });
        self.after_opaque = false;
    }

    fn container(&mut self, kids: &[Node], path: &mut Vec<usize>, name: &str, pre: bool, heading: u8) {
        if path.len() > MAX_DEPTH { return; }
        if !kids.iter().any(is_block_node) {
            self.emit(kids, path, 0, kids.len(), pre, heading);
            return;
        }
        let ordered = name == "ol";
        let mut ord = 1usize;
        let mut i = 0;
        while i < kids.len() {
            match kind_of(&kids[i]) {
                Kind::Opaque => {
                    if let (Node::El(e), None) = (&kids[i], &self.cell) {
                        if e.name == "table" {
                            if let Some(shape) = table_shape(e) {
                                path.push(i);
                                self.table(e, &shape, path);
                                path.pop();
                                self.after_opaque = true;
                                i += 1;
                                continue;
                            }
                        }
                    }
                    self.items.push(Item::Opaque { html: htmltree::serialize(&kids[i..=i]), anc: self.anc(false) });
                    self.after_opaque = true;
                    i += 1;
                }
                Kind::Container => {
                    let Node::El(e) = &kids[i] else { unreachable!() };
                    let pushed = match e.name.as_str() {
                        "blockquote" => { self.frames.push(Frame::Quote); true }
                        "li" => { self.frames.push(Frame::Li { ordinal: ordered.then_some(ord), emitted: false }); ord += 1; true }
                        _ => false,
                    };
                    path.push(i);
                    self.container(&e.kids, path, &e.name, pre || e.name == "pre", heading_of(&e.name));
                    path.pop();
                    if pushed { self.frames.pop(); }
                    i += 1;
                }
                _ => {
                    let mut j = i;
                    while j < kids.len() && !is_block_node(&kids[j]) { j += 1; }
                    if visible(&kids[i..j]) { self.emit(kids, path, i, j, pre, heading); }
                    i = j;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ cursor & document

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Cursor { pub block: usize, pub off: usize }

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LinkTarget { Href(String), Image(String) }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListKind { Bullet, Numbered, Todo }

#[derive(Clone, Debug)]
pub struct Doc { pub nodes: Vec<Node> }

/// Same elements, attributes (compared decoded) and text; only the spelling of void tags (`<br />` / `<br>`) and the whitespace inside tags may differ.
fn same_tokens(a: &[htmltree::Token], b: &[htmltree::Token]) -> bool {
    use htmltree::Token;
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| match (x, y) {
        (Token::Open { name: n1, attrs: a1, self_close: s1 }, Token::Open { name: n2, attrs: a2, self_close: s2 }) => {
            n1 == n2 && (htmltree::is_void(n1) || s1 == s2) && a1.len() == a2.len()
                && a1.iter().zip(a2).all(|((k1, v1), (k2, v2))| k1 == k2 && v1.as_deref().map(decode_entities) == v2.as_deref().map(decode_entities))
        }
        _ => x == y,
    })
}

/// Why a note cannot be edited in place: it is too big or deep, or parsing and writing it again would change more than markup spelling.
pub fn unsupported_reason(html: &str) -> Option<String> {
    if html.len() > MAX_BYTES { return Some(format!("it is large ({} KB)", html.len() / 1000)); }
    if html.contains("<!") || html.contains("<?") { return Some("it contains comments or other markup that would not be preserved".into()); }
    let nodes = htmltree::parse(html);
    fn depth(nodes: &[Node]) -> usize { nodes.iter().map(|n| if let Node::El(e) = n { 1 + depth(&e.kids) } else { 0 }).max().unwrap_or(0) }
    if depth(&nodes) > MAX_DEPTH { return Some("it is nested very deeply".into()); }
    let out = htmltree::serialize(&nodes);
    if out != html && !same_tokens(&htmltree::tokenize(html, true), &htmltree::tokenize(&out, true)) { return Some("its HTML would not be preserved (unclosed or mis-nested tags)".into()); }
    None
}

impl Doc {
    pub fn parse(html: &str) -> Doc {
        let mut nodes = htmltree::parse(html);
        if nodes.is_empty() { nodes = vec![new_el("p", &[], phantom())]; }
        Doc { nodes }
    }

    pub fn html(&self) -> String { htmltree::serialize(&self.nodes) }

    pub fn outline(&self) -> Outline {
        let mut w = Walk { blocks: vec![], items: vec![], frames: vec![], after_opaque: false, cell: None };
        w.container(&self.nodes, &mut vec![], "", false, 0);
        Outline { blocks: w.blocks, items: w.items }
    }

    fn run<'a>(&'a self, b: &Block) -> &'a [Node] {
        kids_at(&self.nodes, &b.parent).map(|k| &k[b.start.min(k.len())..b.end.min(k.len())]).unwrap_or(&[])
    }

    pub fn block_len(&self, b: &Block) -> usize { segs_len(&build_segs(self.run(b), b.start)) }

    pub fn units(&self, b: &Block) -> Vec<Unit> {
        let mut out = vec![];
        for s in build_segs(self.run(b), b.start) {
            match s.kind {
                SegKind::Text(cs) => out.extend(cs.into_iter().map(|ch| Unit { ch, label: None, src: None, href: None, marks: s.marks, br: false })),
                SegKind::Atom(a) => out.push(Unit { ch: '\u{fffc}', label: Some(a.label), src: a.src, href: a.href, marks: s.marks, br: false }),
                SegKind::Br => out.push(Unit { ch: '\n', label: None, src: None, href: None, marks: s.marks, br: true }),
                SegKind::Anchor => {}
            }
        }
        out
    }

    pub fn block_text(&self, b: &Block) -> String { self.units(b).iter().map(|u| if u.br { '\n' } else if u.label.is_some() { ' ' } else { u.ch }).collect() }

    /// Byte offset in `html()` of a cursor position (found by planting a private-use marker character there).
    pub fn html_offset(&self, c: Cursor) -> usize {
        const MARK: char = '\u{E000}';
        let mut d = self.clone();
        d.insert_text(c, &MARK.to_string());
        d.html().find(MARK).unwrap_or(0)
    }

    /// The cursor for a byte offset in `html` (an offset inside a tag or entity moves to its end/start). Pair of `html_offset`.
    pub fn cursor_at_html_offset(html: &str, off: usize) -> Cursor {
        const MARK: char = '\u{E000}';
        let mut off = off.min(html.len());
        while !html.is_char_boundary(off) { off -= 1; }
        let before = &html[..off];
        if let (Some(lt), gt) = (before.rfind('<'), before.rfind('>')) {
            if gt.is_none_or(|g| g < lt) { off = html[off..].find('>').map_or(html.len(), |e| off + e + 1); }
        }
        if let Some(amp) = html[..off].rfind('&') {
            if !html[amp..off].contains([';', ' ', '<']) && off - amp < 12 { off = amp; }
        }
        let marked = format!("{}{MARK}{}", &html[..off], &html[off..]);
        let (plain, with) = (Doc::parse(html), Doc::parse(&marked));
        let (a, b) = (plain.outline(), with.outline());
        for (i, bi) in b.blocks.iter().enumerate() {
            if let Some(at) = with.block_text(&bi.block).chars().position(|ch| ch == MARK) {
                // a marker between blocks makes a block of its own: the cursor then starts the block that follows
                return if a.blocks.len() == b.blocks.len() { Cursor { block: i, off: at } } else { plain.clamp(Cursor { block: i, off: 0 }, &a) };
            }
        }
        Cursor::default()
    }

    pub fn clamp(&self, c: Cursor, ol: &Outline) -> Cursor {
        if ol.blocks.is_empty() { return Cursor::default(); }
        let block = c.block.min(ol.blocks.len() - 1);
        Cursor { block, off: c.off.min(self.block_len(&ol.blocks[block].block)) }
    }

    /// Replace a textblock's run through `f`; the run is tidied and an emptied paragraph/heading/item gets CKEditor's `&nbsp;` filler.
    fn edit_run(&mut self, b: &Block, f: impl FnOnce(Vec<Node>) -> Vec<Node>) -> Option<()> {
        let kids = kids_at_mut(&mut self.nodes, &b.parent)?;
        if b.end > kids.len() { return None; }
        let run: Vec<Node> = kids.drain(b.start..b.end).collect();
        let mut new = tidy(f(run));
        let whole = b.start == 0 && kids.is_empty();
        if new.is_empty() && whole && !b.parent.is_empty() {
            let name = el_at(&self.nodes, &b.parent).map(|e| e.name.clone()).unwrap_or_default();
            if matches!(name.as_str(), "p" | "li" | "td" | "th" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") { new = phantom(); }
        }
        let kids = kids_at_mut(&mut self.nodes, &b.parent)?;
        kids.splice(b.start..b.start, new);
        Some(())
    }

    // -------------------------------------------------------------- text

    pub fn insert_text(&mut self, c: Cursor, text: &str) -> Cursor {
        if text.is_empty() { return c; }
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        let b = bi.block.clone();
        let Some(kids) = kids_at_mut(&mut self.nodes, &b.parent) else { return c };
        if b.end > kids.len() { return c; }
        let before_len = kids.len();
        let phantom_run = is_phantom(&kids[b.start..b.end]);
        let segs = build_segs(&kids[b.start..b.end], b.start);
        let total = segs_len(&segs);
        let off = c.off.min(total);
        let prev_ch = if off > 0 { unit_char(&segs, off - 1) } else { None };
        let next_ch = unit_char(&segs, off);
        let prepared = if b.pre { text.to_string() } else { prepare_text(text, prev_ch, next_ch) };
        let inserted = prepared.chars().count();
        let first = prepared.chars().next().unwrap_or(' ');

        if phantom_run {
            kids[b.start] = text_node(&prepared);
            return Cursor { block: c.block, off: inserted };
        }

        enum Target { InText(Vec<usize>, usize), After(Vec<usize>), Before(Vec<usize>), Into(Vec<usize>), Run }
        let mut pos = 0;
        let (mut inside, mut prev, mut next, mut anchor) = (None, None, None, None);
        for (i, s) in segs.iter().enumerate() {
            let (st, en, l) = (pos, pos + s.len(), s.len());
            if l > 0 && st < off && off < en { inside = Some((i, off - st)); break; }
            if l > 0 && en == off { prev = Some(i); }
            if l > 0 && st == off && next.is_none() { next = Some(i); }
            if l == 0 && st == off && anchor.is_none() { anchor = Some(i); }
            pos = en;
        }
        let is_text = |i: &usize| matches!(segs[*i].kind, SegKind::Text(_));
        let target = if let Some((i, ix)) = inside {
            Target::InText(segs[i].path.clone(), ix)
        } else {
            match (prev.filter(is_text), next.filter(is_text)) {
                (Some(pi), nx) => {
                    let ps = &segs[pi];
                    let SegKind::Text(pc) = &ps.kind else { unreachable!() };
                    if let Some(d) = ps.link.filter(|_| !nx.is_some_and(|ni| same_link(ps, &segs[ni]))) {
                        match nx { Some(ni) => Target::InText(segs[ni].path.clone(), 0), None => Target::After(ps.path[..=d].to_vec()) }
                    } else {
                        Target::InText(ps.path.clone(), pc.len())
                    }
                }
                (None, Some(ni)) => {
                    let ns = &segs[ni];
                    match ns.link { Some(d) => Target::Before(ns.path[..=d].to_vec()), None => Target::InText(ns.path.clone(), 0) }
                }
                (None, None) => {
                    if let Some(a) = anchor { Target::Into(segs[a].path.clone()) }
                    else if let Some(p) = prev { Target::After(segs[p].path.clone()) }
                    else if let Some(n) = next { Target::Before(segs[n].path.clone()) }
                    else { Target::Run }
                }
            }
        };
        let start = b.start;
        let new_text = prepared.clone();
        match target {
            Target::InText(path, ix) => {
                if let Some(Node::Text(t)) = node_at_mut(kids, &path) {
                    let mut chars = dec(t);
                    let at = ix.min(chars.len());
                    chars.splice(at..at, prepared.chars());
                    // a filler nbsp typed at the end of a word becomes an ordinary space once more text follows it
                    if at >= 2 && chars[at - 1] == NBSP && !is_spacey(chars[at - 2]) && !is_spacey(first) && !b.pre { chars[at - 1] = ' '; }
                    *t = enc(&chars.iter().collect::<String>());
                }
            }
            Target::After(path) => { if let Some((l, p)) = path.split_last() { if let Some(k) = kids_at_mut(kids, p) { k.insert((*l + 1).min(k.len()), text_node(&new_text)); } } }
            Target::Before(path) => { if let Some((l, p)) = path.split_last() { if let Some(k) = kids_at_mut(kids, p) { k.insert((*l).min(k.len()), text_node(&new_text)); } } }
            Target::Into(path) => { if let Some(Node::El(e)) = node_at_mut(kids, &path) { e.kids.push(text_node(&new_text)); } }
            Target::Run => kids.insert(start.min(kids.len()), text_node(&new_text)),
        }
        let grown = kids.len() - before_len;
        let end = b.end + grown;
        let run: Vec<Node> = kids.drain(b.start..end).collect();
        kids.splice(b.start..b.start, tidy(run));
        Cursor { block: c.block, off: off + inserted }
    }

    /// Delete units `lo..hi` of one textblock.
    fn delete_in_block(&mut self, block: usize, lo: usize, hi: usize) {
        if lo >= hi { return; }
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(block) else { return };
        let _ = self.edit_run(&bi.block.clone(), |run| if is_phantom(&run) { run } else { delete_units(run, lo, hi) });
    }

    pub fn delete_range(&mut self, a: Cursor, b: Cursor) -> Cursor {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == b { return a; }
        let ol = self.outline();
        if b.block >= ol.blocks.len() { return a; }
        if a.block == b.block {
            self.delete_in_block(a.block, a.off, b.off);
            return a;
        }
        // a selection reaching into another table cell (or out of the table) is not deleted: cells keep their structure
        if ol.blocks[a.block..=b.block].iter().any(|bi| bi.block.cell != ol.blocks[a.block].block.cell) { return a; }
        let len_a = self.block_len(&ol.blocks[a.block].block);
        // whole blocks strictly between, last first so earlier paths stay valid
        for i in (a.block + 1..b.block).rev() {
            let blk = ol.blocks[i].block.clone();
            if let Some(k) = kids_at_mut(&mut self.nodes, &blk.parent) {
                if blk.end <= k.len() { k.drain(blk.start..blk.end); }
            }
            if !blk.parent.is_empty() { prune_empty(&mut self.nodes, blk.parent.clone()); }
        }
        // the blocks between are gone, so b's block now directly follows a's
        self.delete_in_block(a.block + 1, 0, b.off);
        self.delete_in_block(a.block, a.off, len_a);
        // refused (an opaque element in between, ...) leaves two blocks, which is still a correct result
        let _ = self.merge_next(a.block);
        a
    }

    pub fn delete_back(&mut self, c: Cursor) -> Cursor {
        if c.off > 0 {
            self.delete_in_block(c.block, c.off - 1, c.off);
            return Cursor { block: c.block, off: c.off - 1 };
        }
        if c.block == 0 { return c; }
        self.merge_next(c.block - 1).unwrap_or(c)
    }

    pub fn delete_forward(&mut self, c: Cursor) -> Cursor {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        if c.off < self.block_len(&bi.block) {
            self.delete_in_block(c.block, c.off, c.off + 1);
            return c;
        }
        if c.block + 1 >= ol.blocks.len() { return c; }
        self.merge_next(c.block).map(|_| c).unwrap_or(c)
    }

    /// Join textblock `ai + 1` onto the end of `ai`. `None` (and no change) when that is not safe.
    fn merge_next(&mut self, ai: usize) -> Option<Cursor> {
        let ol = self.outline();
        let (a, b) = (ol.blocks.get(ai)?.block.clone(), ol.blocks.get(ai + 1)?.block.clone());
        if b.after_opaque || a.cell != b.cell { return None; }
        let a_len = self.block_len(&a);
        let mut nodes = self.nodes.clone();
        let mut moved: Vec<Node> = {
            let bk = kids_at_mut(&mut nodes, &b.parent)?;
            if b.end > bk.len() { return None; }
            bk.drain(b.start..b.end).collect()
        };
        if is_phantom(&moved) { moved.clear(); }
        // an `li` left holding more than its text (a nested list) travels along into the previous item, or the merge is refused
        let mut leftover: Vec<Node> = vec![];
        let mut remove_b_el = false;
        if !b.parent.is_empty() {
            let name = el_at(&nodes, &b.parent)?.name.clone();
            let rest = kids_at(&nodes, &b.parent)?;
            if !whitespace_only(rest) {
                if name == "li" {
                    let a_in_li = !a.parent.is_empty() && el_at(&nodes, &a.parent).is_some_and(|e| e.name == "li");
                    if !a_in_li { return None; }
                    leftover = std::mem::take(kids_at_mut(&mut nodes, &b.parent)?);
                    remove_b_el = true;
                }
            } else { remove_b_el = true; }
        }
        {
            let ak = kids_at_mut(&mut nodes, &a.parent)?;
            if a.end > ak.len() { return None; }
            let mut at = a.end;
            if is_phantom(&ak[a.start..a.end]) { ak.drain(a.start..a.end); at = a.start; }
            let n = moved.len();
            ak.splice(at..at, moved);
            let run: Vec<Node> = ak.drain(a.start..at + n).collect();
            ak.splice(a.start..a.start, tidy(run));
            if !leftover.is_empty() { ak.extend(leftover); }
        }
        if remove_b_el {
            // paths into `nodes` before A (and A's own ancestors) are unaffected by removing something after them
            let mut p = b.parent.clone();
            // b's element was emptied (or its kids moved): drop it, then any parents this empties
            if let Some((l, pp)) = p.split_last() {
                let (l, pp) = (*l, pp.to_vec());
                if let Some(k) = kids_at_mut(&mut nodes, &pp) { if l < k.len() { k.remove(l); } }
                p.pop();
            }
            prune_empty(&mut nodes, p);
        }
        self.nodes = nodes;
        Some(Cursor { block: ai, off: a_len })
    }

    // -------------------------------------------------------------- structure

    /// Enter. Returns the new cursor (start of the following textblock).
    pub fn split(&mut self, c: Cursor) -> Cursor {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        let b = bi.block.clone();
        let len = self.block_len(&b);
        let off = c.off.min(len);
        if b.pre { return self.insert_text(Cursor { block: c.block, off }, "\n"); }
        let parent_el = if b.parent.is_empty() { None } else { el_at(&self.nodes, &b.parent).cloned() };
        let whole = parent_el.as_ref().is_some_and(|e| b.start == 0 && b.end == e.kids.len());
        let name = parent_el.as_ref().map(|e| e.name.clone()).unwrap_or_default();
        let next = Cursor { block: c.block + 1, off: 0 };

        if name == "li" {
            let li = parent_el.unwrap();
            let run: Vec<Node> = li.kids[b.start..b.end].to_vec();
            let todo = todo_label(&run).is_some();
            if len == 0 && !self.is_nested_li(&b) { return self.lift_li(c).unwrap_or(c); }
            if len == 0 { return self.outdent(c).unwrap_or(c); }
            let (l, r) = if todo { split_todo(&run, off) } else { split_nodes(run, off) };
            let (mut l, mut r) = (tidy(l), tidy(r));
            if !todo { fix_halves(&mut l, &mut r); if l.is_empty() { l = phantom(); } if r.is_empty() { r = phantom(); } }
            let mut lk: Vec<Node> = li.kids[..b.start].to_vec();
            lk.extend(l);
            let mut rk = r;
            rk.extend(li.kids[b.end..].iter().cloned());
            let left = Node::El(El { kids: lk, ..li.clone() });
            let right = Node::El(El { kids: rk, ..li });
            let (idx, lp) = b.parent.split_last().map(|(i, p)| (*i, p.to_vec())).unwrap();
            if let Some(list) = kids_at_mut(&mut self.nodes, &lp) { list[idx] = left; list.insert(idx + 1, right); }
            return next;
        }
        if whole && matches!(name.as_str(), "p" | "div" | "dt" | "dd" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
            let el = parent_el.unwrap();
            let run = el.kids.clone();
            let (l, r) = split_nodes(run, off);
            let (mut l, mut r) = (tidy(l), tidy(r));
            fix_halves(&mut l, &mut r);
            if l.is_empty() { l = phantom(); }
            if r.is_empty() { r = phantom(); }
            let heading = heading_of(&name) > 0;
            let (rname, rattrs) = if heading && off == len { ("p".to_string(), vec![]) } else { (el.name.clone(), el.attrs.iter().filter(|(k, _)| k != "id").cloned().collect()) };
            let left = Node::El(El { kids: l, ..el.clone() });
            let right = Node::El(El { name: rname, attrs: rattrs, kids: r, self_close: false });
            let (idx, pp) = b.parent.split_last().map(|(i, p)| (*i, p.to_vec())).unwrap();
            if let Some(k) = kids_at_mut(&mut self.nodes, &pp) { k[idx] = left; k.insert(idx + 1, right); }
            return next;
        }
        let wrap = !whole || matches!(name.as_str(), "" | "blockquote" | "section" | "article" | "aside" | "header" | "footer" | "main" | "nav" | "details" | "body");
        if wrap {
            let Some(kids) = kids_at_mut(&mut self.nodes, &b.parent) else { return c };
            let run: Vec<Node> = kids.drain(b.start..b.end).collect();
            let (l, r) = split_nodes(run, off);
            let (mut l, mut r) = (tidy(l), tidy(r));
            fix_halves(&mut l, &mut r);
            if l.is_empty() { l = phantom(); }
            if r.is_empty() { r = phantom(); }
            kids.splice(b.start..b.start, [new_el("p", &[], l), new_el("p", &[], r)]);
            return next;
        }
        self.soft_break(c)
    }

    /// Shift+Enter: a `<br>` at the cursor.
    pub fn soft_break(&mut self, c: Cursor) -> Cursor {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        let off = c.off.min(self.block_len(&bi.block));
        let br = Node::El(El { name: "br".into(), attrs: vec![], kids: vec![], self_close: false });
        let _ = self.edit_run(&bi.block.clone(), |run| {
            let run = if is_phantom(&run) { vec![] } else { run };
            let (l, r) = split_nodes(run, off);
            let mut out = l;
            out.push(br);
            out.extend(r);
            out
        });
        Cursor { block: c.block, off: off + 1 }
    }

    fn is_nested_li(&self, b: &Block) -> bool {
        // the item is at b.parent, its list one level up; nested when that list sits inside another item
        let n = b.parent.len();
        n >= 3 && el_at(&self.nodes, &b.parent[..n - 2]).is_some_and(|e| e.name == "li")
    }

    fn li_context(&self, c: Cursor) -> Option<(Block, Vec<usize>, usize)> {
        let ol = self.outline();
        let b = ol.blocks.get(c.block)?.block.clone();
        if b.parent.len() < 2 { return None; }
        if el_at(&self.nodes, &b.parent)?.name != "li" { return None; }
        let (idx, list_path) = b.parent.split_last().map(|(i, p)| (*i, p.to_vec()))?;
        if !matches!(el_at(&self.nodes, &list_path)?.name.as_str(), "ul" | "ol") { return None; }
        Some((b, list_path, idx))
    }

    /// Tab on a list item: nest it under the previous item.
    pub fn indent(&mut self, c: Cursor) -> Option<()> {
        let (_, list_path, idx) = self.li_context(c)?;
        let list = kids_at_mut(&mut self.nodes, &list_path)?;
        let prev = (0..idx).rev().find(|i| matches!(&list[*i], Node::El(e) if e.name == "li"))?;
        let li = list.remove(idx);
        let list_name = el_at(&self.nodes, &list_path).map(|e| e.name.clone())?;
        let list_attrs: Vec<(String, Option<String>)> = el_at(&self.nodes, &list_path).map(|e| e.attrs.clone())?;
        let list = kids_at_mut(&mut self.nodes, &list_path)?;
        let Node::El(prev_li) = &mut list[prev] else { return None };
        let nested = prev_li.kids.iter().rposition(|n| matches!(n, Node::El(e) if matches!(e.name.as_str(), "ul" | "ol")));
        match nested {
            Some(i) => { if let Node::El(nl) = &mut prev_li.kids[i] { nl.kids.push(li); } }
            None => prev_li.kids.push(Node::El(El { name: list_name, attrs: list_attrs, kids: vec![li], self_close: false })),
        }
        Some(())
    }

    /// Shift+Tab on a nested list item: move it out one level (the items after it become its children).
    pub fn outdent(&mut self, c: Cursor) -> Option<Cursor> {
        let (_, list_path, idx) = self.li_context(c)?;
        let (outer_idx, outer_li_path) = list_path.split_last().map(|(i, p)| (*i, p.to_vec()))?;
        if el_at(&self.nodes, &outer_li_path)?.name != "li" { return None; }
        let (oi, grand_path) = outer_li_path.split_last().map(|(i, p)| (*i, p.to_vec()))?;
        let list = kids_at_mut(&mut self.nodes, &list_path)?;
        let mut tail: Vec<Node> = list.drain(idx..).collect();
        let mut li = tail.remove(0);
        let followers: Vec<Node> = tail.into_iter().filter(|n| matches!(n, Node::El(e) if e.name == "li")).collect();
        let list_empty = whitespace_only(list);
        let (lname, lattrs) = { let e = el_at(&self.nodes, &list_path)?; (e.name.clone(), e.attrs.clone()) };
        if !followers.is_empty() {
            if let Node::El(e) = &mut li {
                match e.kids.iter().rposition(|n| matches!(n, Node::El(x) if matches!(x.name.as_str(), "ul" | "ol"))) {
                    Some(i) => { if let Node::El(nl) = &mut e.kids[i] { nl.kids.extend(followers); } }
                    None => e.kids.push(Node::El(El { name: lname, attrs: lattrs, kids: followers, self_close: false })),
                }
            }
        }
        if list_empty {
            if let Some(k) = kids_at_mut(&mut self.nodes, &outer_li_path) { k.remove(outer_idx); }
        }
        let grand = kids_at_mut(&mut self.nodes, &grand_path)?;
        grand.insert(oi + 1, li);
        Some(c)
    }

    /// Take a list item out of its list into a paragraph (Enter on an empty item, or toggling the list off).
    pub fn lift_li(&mut self, c: Cursor) -> Option<Cursor> {
        let (b, list_path, idx) = self.li_context(c)?;
        let (list_idx, lp) = list_path.split_last().map(|(i, p)| (*i, p.to_vec()))?;
        let list_el = el_at(&self.nodes, &list_path)?.clone();
        let li = match list_el.kids.get(idx)? { Node::El(e) => e.clone(), _ => return None };
        let run: Vec<Node> = li.kids[b.start..b.end].to_vec();
        let content = match todo_label(&run) {
            Some(label) => label.kids.iter().find_map(|k| match k { Node::El(e) if e.name == "span" => Some(e.kids.clone()), _ => None }).unwrap_or_default(),
            None => run,
        };
        let content = if content.is_empty() { phantom() } else { content };
        let others: Vec<Node> = li.kids.iter().enumerate().filter(|(i, _)| *i < b.start || *i >= b.end).map(|(_, n)| n.clone()).collect();
        let before: Vec<Node> = list_el.kids[..idx].to_vec();
        let after: Vec<Node> = list_el.kids[idx + 1..].to_vec();
        let mut repl: Vec<Node> = vec![];
        if before.iter().any(|n| matches!(n, Node::El(_))) { repl.push(Node::El(El { kids: before, ..list_el.clone() })); }
        repl.push(new_el("p", &[], content));
        repl.extend(others.into_iter().filter(|n| matches!(n, Node::El(_))));
        if after.iter().any(|n| matches!(n, Node::El(_))) { repl.push(Node::El(El { kids: after, ..list_el })); }
        let k = kids_at_mut(&mut self.nodes, &lp)?;
        k.splice(list_idx..=list_idx, repl);
        Some(c)
    }

    /// Turn the paragraph/heading holding the cursor into a list item (or a list item back into a paragraph).
    pub fn toggle_list(&mut self, c: Cursor, kind: ListKind) -> Option<Cursor> {
        if self.li_context(c).is_some() { return self.lift_li(c); }
        let ol = self.outline();
        let b = ol.blocks.get(c.block)?.block.clone();
        let el = el_at(&self.nodes, &b.parent)?.clone();
        if !matches!(el.name.as_str(), "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") || b.start != 0 || b.end != el.kids.len() { return None; }
        let run = if is_phantom(&el.kids) { vec![] } else { el.kids.clone() };
        let li_kids = match kind {
            ListKind::Todo => vec![new_el("label", &[("class", "todo-list__label")], vec![
                new_el("input", &[("type", "checkbox"), ("disabled", "disabled")], vec![]),
                new_el("span", &[("class", "todo-list__label__description")], run),
            ])],
            _ => if run.is_empty() { phantom() } else { run },
        };
        let (lname, lattrs): (&str, &[(&str, &str)]) = match kind { ListKind::Bullet => ("ul", &[]), ListKind::Numbered => ("ol", &[]), ListKind::Todo => ("ul", &[("class", "todo-list")]) };
        let li = new_el("li", &[], li_kids);
        let (idx, pp) = b.parent.split_last().map(|(i, p)| (*i, p.to_vec()))?;
        let sib = kids_at_mut(&mut self.nodes, &pp)?;
        let list = new_el(lname, lattrs, vec![li]);
        sib[idx] = list;
        Some(c)
    }

    /// Rename the paragraph/heading holding the cursor (`p`, `h1`..`h6`).
    pub fn set_block_tag(&mut self, c: Cursor, tag: &str) -> bool {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return false };
        let b = &bi.block;
        let ok = el_at(&self.nodes, &b.parent).is_some_and(|e| matches!(e.name.as_str(), "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") && b.start == 0 && b.end == e.kids.len());
        if !ok { return false; }
        if let Some((i, pp)) = b.parent.split_last() {
            if let Some(Node::El(e)) = kids_at_mut(&mut self.nodes, pp).and_then(|k| k.get_mut(*i)) { e.name = tag.into(); }
        }
        true
    }

    /// Wrap the paragraph in a block quote.
    pub fn quote(&mut self, c: Cursor) -> bool {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return false };
        let b = &bi.block;
        if !el_at(&self.nodes, &b.parent).is_some_and(|e| e.name == "p") { return false; }
        let Some((i, pp)) = b.parent.split_last() else { return false };
        let Some(k) = kids_at_mut(&mut self.nodes, pp) else { return false };
        let p = k.remove(*i);
        k.insert(*i, new_el("blockquote", &[], vec![p]));
        true
    }

    pub fn toggle_checkbox(&mut self, c: Cursor) -> bool {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return false };
        let b = bi.block.clone();
        let Some(kids) = kids_at_mut(&mut self.nodes, &b.parent) else { return false };
        b.end <= kids.len() && toggle_input(&mut kids[b.start..b.end])
    }

    // -------------------------------------------------------------- inline formatting

    fn range_in_block(a: Cursor, b: Cursor, i: usize, len: usize) -> (usize, usize) {
        (if i == a.block { a.off } else { 0 }, if i == b.block { b.off.min(len) } else { len })
    }

    /// Marks that every text unit in the selection carries.
    pub fn marks_in(&self, a: Cursor, b: Cursor) -> u16 {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let ol = self.outline();
        let mut all = u16::MAX;
        let mut any = false;
        for i in a.block..=b.block.min(ol.blocks.len().saturating_sub(1)) {
            let units = self.units(&ol.blocks[i].block);
            let (lo, hi) = Self::range_in_block(a, b, i, units.len());
            for u in units.iter().take(hi).skip(lo).filter(|u| u.label.is_none() && !u.br) { all &= u.marks; any = true; }
        }
        if any { all } else { 0 }
    }

    /// Toggle one inline format (`mark::*`) over the selection: remove it if the whole selection has it, add it otherwise.
    pub fn toggle_mark(&mut self, a: Cursor, b: Cursor, flag: u16) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == b { return; }
        let add = self.marks_in(a, b) & flag == 0;
        self.apply_mark(a, b, flag, add.then(|| (mark::tag(flag).to_string(), vec![])));
    }

    /// Force one inline format on or off over the selection (what a pending Ctrl+B at the caret applies to the text typed next).
    pub fn set_mark(&mut self, a: Cursor, b: Cursor, flag: u16, on: bool) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a != b { self.apply_mark(a, b, flag, on.then(|| (mark::tag(flag).to_string(), vec![]))); }
    }

    /// Set (`Some(href)`) or clear (`None`) a link over the selection.
    pub fn set_link(&mut self, a: Cursor, b: Cursor, href: Option<&str>) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == b { return; }
        let wrap = href.filter(|h| !h.is_empty()).map(|h| ("a".to_string(), vec![("href".to_string(), Some(htmltree::escape_attr(h)))]));
        self.apply_mark(a, b, mark::LINK, wrap);
    }

    fn apply_mark(&mut self, a: Cursor, b: Cursor, flag: u16, wrap: Option<(String, Vec<(String, Option<String>)>)>) {
        let ol = self.outline();
        for i in a.block..=b.block.min(ol.blocks.len().saturating_sub(1)) {
            let blk = ol.blocks[i].block.clone();
            let len = self.block_len(&blk);
            let (lo, hi) = Self::range_in_block(a, b, i, len);
            if lo >= hi { continue; }
            let wrap = wrap.clone();
            let _ = self.edit_run(&blk, |run| {
                if is_phantom(&run) { return run; }
                with_content(run, |run| {
                    let (l, rest) = split_nodes(run, lo);
                    let (m, r) = split_nodes(rest, hi - lo);
                    let m = unwrap_flag(m, flag);
                    let mut out = l;
                    match wrap {
                        Some((name, attrs)) if !m.is_empty() => out.push(Node::El(El { name, attrs, kids: m, self_close: false })),
                        _ => out.extend(m),
                    }
                    out.extend(r);
                    out
                })
            });
        }
    }

    /// The link around the cursor: (start, end, href).
    pub fn link_at(&self, c: Cursor) -> Option<(Cursor, Cursor, String)> {
        let ol = self.outline();
        let b = &ol.blocks.get(c.block)?.block;
        let segs = build_segs(self.run(b), b.start);
        let mut pos = 0;
        let mut hit: Option<(Vec<usize>, usize, usize)> = None;
        for s in &segs {
            let (st, en) = (pos, pos + s.len());
            pos = en;
            let Some(d) = s.link else { continue };
            let lp = s.path[..=d].to_vec();
            if let Some((p, _, e)) = &mut hit { if *p == lp { *e = en; continue; } else if c.off <= *e { break; } }
            if hit.as_ref().is_some_and(|(_, _, e)| c.off <= *e) { break; }
            hit = Some((lp, st, en));
        }
        let (lp, st, en) = hit.filter(|(_, st, en)| *st <= c.off && c.off <= *en)?;
        let href = el_at(kids_at(&self.nodes, &b.parent)?, &lp).and_then(|e| attr(e, "href")).map(decode_entities)?;
        Some((Cursor { block: c.block, off: st }, Cursor { block: c.block, off: en }, href))
    }

    // -------------------------------------------------------------- links & images

    /// Put an inline node at the cursor.
    fn insert_inline(&mut self, c: Cursor, node: Node, units: usize) -> Cursor {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        let off = c.off.min(self.block_len(&bi.block));
        let _ = self.edit_run(&bi.block.clone(), |run| {
            let run = if is_phantom(&run) { vec![] } else { run };
            with_content(run, |run| {
                let (l, r) = split_nodes(run, off);
                let mut out = l;
                out.push(node);
                out.extend(r);
                out
            })
        });
        Cursor { block: c.block, off: off + units }
    }

    /// A link to another note, shown with its title (Trilium keeps `reference-link` titles current).
    pub fn insert_note_link(&mut self, c: Cursor, note_id: &str, title: &str) -> Cursor {
        let shown = if title.trim().is_empty() { note_id } else { title };
        let a = new_el("a", &[("class", "reference-link"), ("href", &format!("#root/{note_id}"))], vec![text_node(shown)]);
        self.insert_inline(c, a, 1)
    }

    /// A link whose text is `text`, at the cursor.
    pub fn insert_link_text(&mut self, c: Cursor, href: &str, text: &str) -> Cursor {
        let a = new_el("a", &[("href", &htmltree::escape_attr(href))], vec![text_node(text)]);
        self.insert_inline(c, a, text.chars().count())
    }

    /// A picture at the cursor: as its own block (`<figure class="image">`, like Trilium's editor) when the cursor is in a plain
    /// paragraph, inline elsewhere. Returns where typing continues.
    pub fn insert_image(&mut self, c: Cursor, src: &str) -> Cursor {
        let ol = self.outline();
        let Some(bi) = ol.blocks.get(c.block) else { return c };
        let b = bi.block.clone();
        let off = c.off.min(self.block_len(&b));
        let img = new_el("img", &[("src", src)], vec![]);
        let el = if b.parent.is_empty() { None } else { el_at(&self.nodes, &b.parent).cloned() };
        match el {
            Some(el) if matches!(el.name.as_str(), "p" | "div") && b.start == 0 && b.end == el.kids.len() => {
                let run = if is_phantom(&el.kids) { vec![] } else { el.kids.clone() };
                let (l, r) = split_nodes(run, off);
                let (l, mut r) = (tidy(l), tidy(r));
                let mut repl = vec![];
                let has_left = !l.is_empty();
                if has_left { repl.push(Node::El(El { kids: l, ..el.clone() })); }
                repl.push(new_el("figure", &[("class", "image")], vec![img]));
                if r.is_empty() { r = phantom(); }
                repl.push(Node::El(El { kids: r, ..el }));
                if let Some((i, pp)) = b.parent.split_last() {
                    if let Some(k) = kids_at_mut(&mut self.nodes, pp) { k.splice(*i..=*i, repl); }
                }
                Cursor { block: c.block + usize::from(has_left) + 1, off: 0 }
            }
            _ => self.insert_inline(c, img, 1),
        }
    }

    /// What Ctrl+O / Ctrl+Enter acts on at the cursor: a link target, or a picture.
    pub fn target_at(&self, c: Cursor) -> Option<LinkTarget> {
        if let Some((_, _, h)) = self.link_at(c) { return Some(LinkTarget::Href(h)); }
        let ol = self.outline();
        let units = self.units(&ol.blocks.get(c.block)?.block);
        for k in [Some(c.off), c.off.checked_sub(1)].into_iter().flatten() {
            if let Some(u) = units.get(k) {
                if let Some(h) = &u.href { return Some(LinkTarget::Href(h.clone())); }
                if let Some(s) = &u.src { return Some(LinkTarget::Image(s.clone())); }
            }
        }
        None
    }

    // -------------------------------------------------------------- autoformat

    /// After a space was typed at the start of a paragraph (`- `, `1. `, `# `, `[ ] `, `> `), turn the paragraph into what it asked for.
    pub fn autoformat(&mut self, c: Cursor) -> Option<Cursor> {
        let ol = self.outline();
        let b = ol.blocks.get(c.block)?.block.clone();
        if c.off < 2 || !el_at(&self.nodes, &b.parent).is_some_and(|e| e.name == "p") || !b.parent.is_empty() && self.li_context(c).is_some() { return None; }
        let units = self.units(&b);
        if units.get(c.off - 1).map(|u| u.ch) != Some(' ') && units.get(c.off - 1).map(|u| u.ch) != Some(NBSP) { return None; }
        let prefix: String = units[..c.off - 1].iter().map(|u| u.ch).collect();
        if units[..c.off - 1].iter().any(|u| u.label.is_some() || u.br) { return None; }
        enum To { List(ListKind, bool), Heading(u8), Quote }
        let to = match prefix.as_str() {
            "*" | "-" => To::List(ListKind::Bullet, false),
            "[]" | "[ ]" => To::List(ListKind::Todo, false),
            "[x]" | "[X]" => To::List(ListKind::Todo, true),
            ">" => To::Quote,
            p if p.len() >= 2 && p.ends_with('.') && p[..p.len() - 1].chars().all(|ch| ch.is_ascii_digit()) => To::List(ListKind::Numbered, false),
            p if (1..=6).contains(&p.len()) && p.chars().all(|ch| ch == '#') => To::Heading(p.len() as u8),
            _ => return None,
        };
        // drop the trigger text, keeping a filler if nothing else is left
        let at = Cursor { block: c.block, off: 0 };
        self.delete_in_block(c.block, 0, c.off);
        match to {
            To::Heading(n) => { self.set_block_tag(at, &format!("h{n}")); }
            To::Quote => { self.quote(at); }
            To::List(kind, checked) => {
                self.toggle_list(at, kind)?;
                if checked { self.toggle_checkbox(at); }
            }
        }
        Some(at)
    }

    // -------------------------------------------------------------- text queries

    pub fn selected_text(&self, a: Cursor, b: Cursor) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let ol = self.outline();
        let mut parts = vec![];
        for i in a.block..=b.block.min(ol.blocks.len().saturating_sub(1)) {
            let text: Vec<char> = self.block_text(&ol.blocks[i].block).chars().collect();
            let (lo, hi) = Self::range_in_block(a, b, i, text.len());
            parts.push(text[lo.min(text.len())..hi.min(text.len())].iter().collect::<String>());
        }
        parts.join("\n")
    }

    pub fn end_cursor(&self) -> Cursor {
        let ol = self.outline();
        match ol.blocks.last() { Some(b) => Cursor { block: ol.blocks.len() - 1, off: self.block_len(&b.block) }, None => Cursor::default() }
    }

    /// First case-insensitive match at or after `from`, wrapping once: (start, end).
    pub fn find(&self, q: &str, from: Cursor) -> Option<(Cursor, Cursor)> {
        let fold = |s: &str| -> Vec<char> { s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect() };
        let q = fold(q);
        if q.is_empty() { return None; }
        let ol = self.outline();
        let n = ol.blocks.len();
        for step in 0..=n {
            let i = (from.block + step) % n.max(1);
            if i >= n { break; }
            let text = fold(&self.block_text(&ol.blocks[i].block));
            let start = if step == 0 { from.off } else { 0 };
            let end_limit = if step == n { from.off } else { text.len() };
            if text.len() < q.len() { continue; }
            for s in start..=(text.len() - q.len()) {
                if s + q.len() > end_limit.max(s + q.len()).min(text.len()) { break; }
                if text[s..s + q.len()] == q[..] {
                    return Some((Cursor { block: i, off: s }, Cursor { block: i, off: s + q.len() }));
                }
            }
        }
        None
    }
}

fn split_todo(run: &[Node], off: usize) -> (Vec<Node>, Vec<Node>) {
    let Some(label) = todo_label(run) else { return split_nodes(run.to_vec(), off) };
    let (inputs, content): (Vec<Node>, Vec<Node>) = label.kids.iter().cloned().partition(|n| matches!(n, Node::El(e) if e.name == "input"));
    let (l, r) = split_nodes(content, off);
    let mut lk = inputs;
    lk.extend(l);
    let mut rk = vec![new_el("input", &[("type", "checkbox"), ("disabled", "disabled")], vec![])];
    rk.extend(r);
    (vec![Node::El(El { kids: lk, ..label.clone() })], vec![Node::El(El { kids: rk, ..label.clone() })])
}

/// Typed/pasted text as it must be stored: spaces that would collapse in HTML become non-breaking ones, tabs become spaces.
fn prepare_text(text: &str, prev: Option<char>, next: Option<char>) -> String {
    let chars: Vec<char> = text.replace('\t', "    ").replace("\r\n", " ").chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    let mut out = String::with_capacity(text.len());
    let mut before = prev;
    for (i, &ch) in chars.iter().enumerate() {
        let after = chars.get(i + 1).copied().or(next);
        if ch == ' ' {
            let sp_before = before.is_none_or(is_spacey);
            let sp_after = after.is_none_or(is_spacey);
            out.push(if sp_before || sp_after { NBSP } else { ' ' });
        } else { out.push(ch); }
        before = out.chars().last();
    }
    out
}

/// Start of the word before the cursor (crossing into the previous textblock at a block start).
pub fn word_back(doc: &Doc, c: Cursor) -> Cursor {
    let ol = doc.outline();
    let Some(bi) = ol.blocks.get(c.block) else { return c };
    if c.off == 0 {
        return if c.block == 0 { c } else { Cursor { block: c.block - 1, off: doc.block_len(&ol.blocks[c.block - 1].block) } };
    }
    let t: Vec<char> = doc.block_text(&bi.block).chars().collect();
    let mut i = c.off.min(t.len());
    while i > 0 && !t[i - 1].is_alphanumeric() { i -= 1; }
    while i > 0 && t[i - 1].is_alphanumeric() { i -= 1; }
    Cursor { block: c.block, off: i }
}

/// End of the word after the cursor (crossing into the next textblock at a block end).
pub fn word_forward(doc: &Doc, c: Cursor) -> Cursor {
    let ol = doc.outline();
    let Some(bi) = ol.blocks.get(c.block) else { return c };
    let t: Vec<char> = doc.block_text(&bi.block).chars().collect();
    if c.off >= t.len() {
        return if c.block + 1 >= ol.blocks.len() { c } else { Cursor { block: c.block + 1, off: 0 } };
    }
    let mut i = c.off;
    while i < t.len() && !t[i].is_alphanumeric() { i += 1; }
    while i < t.len() && t[i].is_alphanumeric() { i += 1; }
    Cursor { block: c.block, off: i }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(h: &str) -> Doc { Doc::parse(h) }
    fn at(block: usize, off: usize) -> Cursor { Cursor { block, off } }

    #[test]
    fn untouched_html_serialises_identically_and_odd_markup_is_refused() {
        let h = "<h2>Title</h2><p>a <strong>b</strong>&nbsp;c</p>\r\n<ul><li>x<ul><li>y</li></ul></li></ul><figure class=\"table\"><table><tr><td>1</td></tr></table></figure>";
        assert_eq!(doc(h).html(), h);
        assert!(unsupported_reason(h).is_none());
        assert!(unsupported_reason("<p>a</p><!-- c -->").is_some());
        assert!(unsupported_reason("<p>a<br />b<img src=\"x\" /></p><hr />").is_none(), "only the spelling of void tags differs");
        assert!(unsupported_reason("<p>a<b>b</p>").is_some(), "unclosed tags would be rewritten");
        assert!(unsupported_reason("<p class=\"x\"\n   id=\"y\">a</p>").is_none());
        assert!(unsupported_reason(&"x".repeat(MAX_BYTES + 1)).is_some());
        assert_eq!(doc("").html(), "<p>&nbsp;</p>");
    }

    #[test]
    fn outline_finds_textblocks_and_skips_opaque_elements() {
        let d = doc("<p>one</p><hr><ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>");
        let ol = d.outline();
        let texts: Vec<String> = ol.blocks.iter().map(|b| d.block_text(&b.block)).collect();
        assert_eq!(texts, ["one", "a", "b", "c"]);
        assert!(ol.blocks[0].block.after_opaque == false && ol.blocks[1].block.after_opaque);
        assert_eq!(ol.items.iter().filter(|i| matches!(i, Item::Opaque { .. })).count(), 1);
    }

    #[test]
    fn typing_goes_into_the_right_text_node_and_keeps_everything_else() {
        let mut d = doc("<p>ab<strong>cd</strong>ef</p><p>keep</p>");
        let c = d.insert_text(at(0, 2), "X");
        assert_eq!(d.html(), "<p>abX<strong>cd</strong>ef</p><p>keep</p>");
        assert_eq!(c, at(0, 3));
        d.insert_text(at(0, 5), "Y");
        assert_eq!(d.html(), "<p>abX<strong>cdY</strong>ef</p><p>keep</p>", "typing at the end of bold continues the bold");
        d.insert_text(at(0, 6), "Z");
        assert_eq!(d.html(), "<p>abX<strong>cdYZ</strong>ef</p><p>keep</p>", "the end of a bold run keeps growing it");
        d.insert_text(at(0, 2), "Q");
        assert_eq!(d.html(), "<p>abQX<strong>cdYZ</strong>ef</p><p>keep</p>");
    }

    #[test]
    fn typing_at_the_end_of_a_link_leaves_the_link() {
        let mut d = doc("<p>go <a href=\"http://x\">here</a></p>");
        d.insert_text(at(0, 7), "!");
        assert_eq!(d.html(), "<p>go <a href=\"http://x\">here</a>!</p>");
        let mut d = doc("<p><a href=\"http://x\">here</a> end</p>");
        d.insert_text(at(0, 2), "-");
        assert_eq!(d.html(), "<p><a href=\"http://x\">he-re</a> end</p>");
    }

    #[test]
    fn spaces_that_would_collapse_become_nbsp_like_ckeditor() {
        let mut d = doc("<p>ab</p>");
        let c = d.insert_text(at(0, 2), " ");
        assert_eq!(d.html(), "<p>ab&nbsp;</p>");
        d.insert_text(c, "c");
        assert_eq!(d.html(), "<p>ab c</p>", "the filler becomes an ordinary space once a letter follows");
        let mut d = doc("<p>a b</p>");
        d.insert_text(at(0, 1), " ");
        assert_eq!(d.html(), "<p>a&nbsp; b</p>");
    }

    #[test]
    fn typing_into_an_empty_paragraph_replaces_the_filler() {
        let mut d = doc("<p>&nbsp;</p>");
        assert_eq!(d.block_len(&d.outline().blocks[0].block), 0);
        d.insert_text(at(0, 0), "hi");
        assert_eq!(d.html(), "<p>hi</p>");
        let mut d = doc("<p></p>");
        d.insert_text(at(0, 0), "hi");
        assert_eq!(d.html(), "<p>hi</p>");
        let mut d = doc("");
        d.insert_text(at(0, 0), "x");
        assert_eq!(d.html(), "<p>x</p>");
    }

    #[test]
    fn entities_survive_in_text_that_is_touched() {
        let mut d = doc("<p>a &lt; b &amp; c</p>");
        d.insert_text(at(0, 0), "x");
        assert_eq!(d.html(), "<p>xa &lt; b &amp; c</p>");
    }

    #[test]
    fn backspace_and_delete_inside_and_across_blocks() {
        let mut d = doc("<p>abc</p><p>def</p>");
        assert_eq!(d.delete_back(at(0, 2)), at(0, 1));
        assert_eq!(d.html(), "<p>ac</p><p>def</p>");
        assert_eq!(d.delete_back(at(1, 0)), at(0, 1 + 1));
        assert_eq!(d.html(), "<p>acdef</p>");
        let mut d = doc("<p>abc</p><p>def</p>");
        d.delete_forward(at(0, 3));
        assert_eq!(d.html(), "<p>abcdef</p>");
        let mut d = doc("<p>a<strong>b</strong></p>");
        d.delete_back(at(0, 2));
        assert_eq!(d.html(), "<p>a</p>", "an emptied inline element goes away");
        let mut d = doc("<p>a</p>");
        d.delete_back(at(0, 1));
        assert_eq!(d.html(), "<p>&nbsp;</p>", "an emptied paragraph keeps CKEditor's filler");
    }

    #[test]
    fn deleting_a_selection_across_several_blocks_joins_the_ends() {
        let mut d = doc("<p>one two</p><p>middle</p><h2>three four</h2><p>tail</p>");
        let c = d.delete_range(at(0, 4), at(2, 6));
        assert_eq!(c, at(0, 4));
        assert_eq!(d.html(), "<p>one four</p><p>tail</p>");
    }

    const TABLE: &str = "<p>a</p><figure class=\"table\"><table><thead><tr><th>H1</th><th>H2</th></tr></thead><tbody><tr><td>c1</td><td>&nbsp;</td></tr></tbody></table></figure><p>b</p>";

    #[test]
    fn table_cells_are_textblocks_in_a_grid_outline() {
        let d = doc(TABLE);
        let ol = d.outline();
        let texts: Vec<String> = ol.blocks.iter().map(|b| d.block_text(&b.block)).collect();
        assert_eq!(texts, ["a", "H1", "H2", "c1", "", "b"]);
        let Item::Table(rows) = ol.items.iter().find(|i| matches!(i, Item::Table(_))).unwrap() else { unreachable!() };
        assert_eq!(rows.len(), 2);
        assert!(rows[0].header && !rows[1].header);
        assert_eq!(rows[1].cells.len(), 2);
        assert!(ol.blocks[1].block.cell.is_some() && ol.blocks[0].block.cell.is_none() && ol.blocks[5].block.cell.is_none());
        // tables with nested tables or odd children stay opaque
        let d = doc("<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>");
        assert!(d.outline().items.iter().any(|i| matches!(i, Item::Opaque { .. })));
        let d = doc("<table><caption>c</caption><tr><td>x</td></tr></table>");
        assert!(d.outline().items.iter().any(|i| matches!(i, Item::Opaque { .. })));
    }

    #[test]
    fn typing_in_cells_changes_only_that_cell_and_enter_makes_a_line_break() {
        let mut d = doc(TABLE);
        d.insert_text(at(3, 2), "!");
        assert!(d.html().contains("<td>c1!</td><td>&nbsp;</td>"), "{}", d.html());
        d.insert_text(at(4, 0), "new");
        assert!(d.html().contains("<td>c1!</td><td>new</td>"), "{}", d.html());
        let c = d.split(at(3, 1));
        assert_eq!(c, at(3, 2));
        assert!(d.html().contains("<td>c<br>1!</td>"), "{}", d.html());
        d.delete_back(at(3, 2));
        d.delete_back(at(3, 1));
        d.delete_back(at(3, 0));
        d.delete_forward(at(3, 0));
        d.delete_forward(at(3, 0));
        assert!(d.html().contains("<td>&nbsp;</td><td>new</td>"), "an emptied cell keeps its filler: {}", d.html());
    }

    #[test]
    fn edits_never_cross_cell_borders_or_remove_cells() {
        let mut d = doc(TABLE);
        let h = d.html();
        assert_eq!(d.delete_back(at(2, 0)), at(2, 0), "Backspace at a cell's start");
        assert_eq!(d.delete_forward(at(1, 2)), at(1, 2), "Delete at a cell's end");
        assert_eq!(d.delete_back(at(5, 0)), at(5, 0), "Backspace right after the table");
        assert_eq!(d.delete_forward(at(0, 1)), at(0, 1), "Delete right before the table");
        d.delete_range(at(0, 0), at(5, 1));
        d.delete_range(at(1, 0), at(3, 1));
        assert_eq!(d.html(), h);
        let mut d = doc("<table><tr><td><p>one</p><p>two</p></td></tr></table>");
        d.delete_back(at(1, 0));
        assert_eq!(d.html(), "<table><tr><td><p>onetwo</p></td></tr></table>", "paragraphs inside one cell still join");
    }

    #[test]
    fn merging_never_crosses_a_table_or_swallows_a_nested_list_into_a_paragraph() {
        let mut d = doc("<p>a</p><table><tr><td>t</td></tr></table><p>b</p>");
        let h = d.html();
        d.delete_back(at(1, 0));
        assert_eq!(d.html(), h);
        let mut d = doc("<p>para</p><ul><li>one<ul><li>nested</li></ul></li></ul>");
        let h = d.html();
        d.delete_back(at(1, 0));
        assert_eq!(d.html(), h, "joining a parent item that still has a nested list into a paragraph is refused");
    }

    #[test]
    fn backspace_at_the_start_of_a_list_item_joins_the_previous_item() {
        let mut d = doc("<ul><li>one</li><li>two</li></ul>");
        let c = d.delete_back(at(1, 0));
        assert_eq!(c, at(0, 3));
        assert_eq!(d.html(), "<ul><li>onetwo</li></ul>");
        let mut d = doc("<ul><li>one</li><li>two<ul><li>sub</li></ul></li></ul>");
        d.delete_back(at(1, 0));
        assert_eq!(d.html(), "<ul><li>onetwo<ul><li>sub</li></ul></li></ul>");
    }

    #[test]
    fn enter_splits_paragraphs_and_headings() {
        let mut d = doc("<p>hello <strong>big world</strong></p>");
        let c = d.split(at(0, 9));
        assert_eq!(c, at(1, 0));
        assert_eq!(d.html(), "<p>hello <strong>big</strong></p><p><strong>&nbsp;world</strong></p>");
        let mut d = doc("<h2>Title</h2>");
        d.split(at(0, 5));
        assert_eq!(d.html(), "<h2>Title</h2><p>&nbsp;</p>", "Enter at the end of a heading starts a paragraph");
        let mut d = doc("<h2>Title</h2>");
        d.split(at(0, 2));
        assert_eq!(d.html(), "<h2>Ti</h2><h2>tle</h2>");
        let mut d = doc("<p>a</p>");
        d.split(at(0, 0));
        assert_eq!(d.html(), "<p>&nbsp;</p><p>a</p>");
    }

    #[test]
    fn enter_in_lists_splits_items_exits_on_empty_and_keeps_nested_lists_with_the_second_half() {
        let mut d = doc("<ul><li>ab<ul><li>sub</li></ul></li></ul>");
        let c = d.split(at(0, 1));
        assert_eq!(c, at(1, 0));
        assert_eq!(d.html(), "<ul><li>a</li><li>b<ul><li>sub</li></ul></li></ul>");
        let mut d = doc("<ul><li>a</li><li>&nbsp;</li><li>c</li></ul>");
        d.split(at(1, 0));
        assert_eq!(d.html(), "<ul><li>a</li></ul><p>&nbsp;</p><ul><li>c</li></ul>");
        let mut d = doc("<ul><li>a<ul><li>&nbsp;</li></ul></li></ul>");
        d.split(at(1, 0));
        assert_eq!(d.html(), "<ul><li>a</li><li>&nbsp;</li></ul>", "Enter on an empty nested item moves it out one level");
    }

    #[test]
    fn enter_in_a_todo_item_makes_a_fresh_unchecked_item() {
        let todo = |t: &str, chk: bool| format!("<li><label class=\"todo-list__label\"><input type=\"checkbox\"{} disabled=\"disabled\"><span class=\"todo-list__label__description\">{t}</span></label></li>", if chk { " checked=\"checked\"" } else { "" });
        let mut d = doc(&format!("<ul class=\"todo-list\">{}</ul>", todo("ab", true)));
        d.split(at(0, 1));
        assert_eq!(d.html(), format!("<ul class=\"todo-list\">{}{}</ul>", todo("a", true), todo("b", false)));
        assert!(d.toggle_checkbox(at(1, 0)));
        assert_eq!(d.html(), format!("<ul class=\"todo-list\">{}{}</ul>", todo("a", true), todo("b", true)));
        d.insert_text(at(0, 1), "z");
        assert!(d.html().contains(">az</span>"));
    }

    #[test]
    fn typing_into_an_emptied_todo_item_works() {
        let mut d = doc("<ul class=\"todo-list\"><li><label class=\"todo-list__label\"><input type=\"checkbox\" disabled=\"disabled\"><span class=\"todo-list__label__description\">a</span></label></li></ul>");
        d.delete_back(at(0, 1));
        assert!(d.html().contains("<span class=\"todo-list__label__description\"></span>"), "{}", d.html());
        d.insert_text(at(0, 0), "q");
        assert!(d.html().contains(">q</span>"), "{}", d.html());
    }

    #[test]
    fn soft_break_and_pre() {
        let mut d = doc("<p>ab</p>");
        let c = d.soft_break(at(0, 1));
        assert_eq!(c, at(0, 2));
        assert_eq!(d.html(), "<p>a<br>b</p>");
        let mut d = doc("<pre><code class=\"language-plain\">ab</code></pre>");
        d.split(at(0, 1));
        assert_eq!(d.html(), "<pre><code class=\"language-plain\">a\nb</code></pre>");
        d.insert_text(at(0, 2), "  ");
        assert!(d.html().contains("a\n  b"));
    }

    #[test]
    fn inline_formats_toggle_on_and_off_without_fragmenting() {
        let mut d = doc("<p>one two three</p>");
        d.toggle_mark(at(0, 4), at(0, 7), mark::BOLD);
        assert_eq!(d.html(), "<p>one <strong>two</strong> three</p>");
        d.toggle_mark(at(0, 4), at(0, 7), mark::BOLD);
        assert_eq!(d.html(), "<p>one two three</p>");
        d.toggle_mark(at(0, 0), at(0, 3), mark::ITALIC);
        d.toggle_mark(at(0, 0), at(0, 7), mark::ITALIC);
        assert_eq!(d.html(), "<p><i>one two</i> three</p>", "a partly formatted selection gets the format everywhere");
        d.toggle_mark(at(0, 2), at(0, 4), mark::ITALIC);
        assert_eq!(d.html(), "<p><i>on</i>e <i>two</i> three</p>");
    }

    #[test]
    fn formats_apply_across_blocks() {
        let mut d = doc("<p>one</p><p>two</p>");
        d.toggle_mark(at(0, 1), at(1, 2), mark::UNDERLINE);
        assert_eq!(d.html(), "<p>o<u>ne</u></p><p><u>tw</u>o</p>");
        assert_eq!(d.marks_in(at(0, 1), at(1, 2)) & mark::UNDERLINE, mark::UNDERLINE);
    }

    #[test]
    fn links_can_be_set_found_and_removed() {
        let mut d = doc("<p>see docs now</p>");
        d.set_link(at(0, 4), at(0, 8), Some("http://a.b/?x=1&y=2"));
        assert_eq!(d.html(), "<p>see <a href=\"http://a.b/?x=1&amp;y=2\">docs</a> now</p>");
        let (s, e, href) = d.link_at(at(0, 6)).unwrap();
        assert_eq!((s, e, href.as_str()), (at(0, 4), at(0, 8), "http://a.b/?x=1&y=2"));
        assert!(d.link_at(at(0, 1)).is_none());
        d.set_link(s, e, None);
        assert_eq!(d.html(), "<p>see docs now</p>");
    }

    #[test]
    fn tab_and_shift_tab_move_list_items() {
        let mut d = doc("<ul><li>a</li><li>b</li><li>c</li></ul>");
        d.indent(at(1, 0)).unwrap();
        assert_eq!(d.html(), "<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>");
        d.indent(at(2, 0)).unwrap();
        assert_eq!(d.html(), "<ul><li>a<ul><li>b</li><li>c</li></ul></li></ul>");
        assert!(doc("<ul><li>a</li></ul>").indent(at(0, 0)).is_none(), "the first item cannot nest");
        let mut d = doc("<ul><li>a<ul><li>b</li><li>c</li></ul></li></ul>");
        d.outdent(at(1, 0)).unwrap();
        assert_eq!(d.html(), "<ul><li>a</li><li>b<ul><li>c</li></ul></li></ul>");
        assert!(doc("<ul><li>a</li></ul>").outdent(at(0, 0)).is_none());
    }

    #[test]
    fn lists_headings_and_quotes_from_paragraphs() {
        let mut d = doc("<p>a</p><p>b</p>");
        d.toggle_list(at(0, 0), ListKind::Bullet).unwrap();
        d.toggle_list(at(1, 0), ListKind::Numbered).unwrap();
        assert_eq!(d.html(), "<ul><li>a</li></ul><ol><li>b</li></ol>");
        d.toggle_list(at(0, 0), ListKind::Bullet).unwrap();
        assert_eq!(d.html(), "<p>a</p><ol><li>b</li></ol>");
        let mut d = doc("<p>a</p>");
        assert!(d.set_block_tag(at(0, 0), "h3"));
        assert_eq!(d.html(), "<h3>a</h3>");
        assert!(d.quote(at(0, 0)) == false, "only paragraphs are quoted");
        let mut d = doc("<p>a</p>");
        assert!(d.quote(at(0, 0)));
        assert_eq!(d.html(), "<blockquote><p>a</p></blockquote>");
    }

    #[test]
    fn markdown_style_triggers_convert_the_paragraph() {
        for (typed, want) in [("- ", "<ul><li>text</li></ul>"), ("1. ", "<ol><li>text</li></ol>"), ("## ", "<h2>text</h2>"), ("> ", "<blockquote><p>text</p></blockquote>"),
            ("[ ] ", "<ul class=\"todo-list\"><li><label class=\"todo-list__label\"><input type=\"checkbox\" disabled=\"disabled\"><span class=\"todo-list__label__description\">text</span></label></li></ul>")] {
            let mut d = doc("<p>text</p>");
            let c = d.insert_text(at(0, 0), typed);
            d.autoformat(c).unwrap_or_else(|| panic!("{typed}"));
            assert_eq!(d.html(), want, "{typed}");
        }
        let mut d = doc("<p>x - </p>");
        assert!(d.autoformat(at(0, 4)).is_none());
        let mut d = doc("<p>a -</p>");
        let c = d.insert_text(at(0, 3), " ");
        assert!(d.autoformat(c).is_none(), "only at the very start");
    }

    #[test]
    fn atoms_are_single_units_and_survive_edits_around_them() {
        let mut d = doc("<p>a<img src=\"x.png\" alt=\"pic\">b</p>");
        let u = d.units(&d.outline().blocks[0].block);
        assert_eq!(u.len(), 3);
        assert_eq!(u[1].label.as_deref(), Some("[image: pic]"));
        d.insert_text(at(0, 2), "Z");
        assert_eq!(d.html(), "<p>a<img src=\"x.png\" alt=\"pic\">Zb</p>");
        d.delete_back(at(0, 2));
        assert_eq!(d.html(), "<p>aZb</p>", "backspace after the image removes the image");
    }

    #[test]
    fn note_links_url_links_and_images_insert_where_the_cursor_is() {
        let mut d = doc("<p>see  now</p>");
        let c = d.insert_note_link(at(0, 4), "abc123", "My <note>");
        assert_eq!(d.html(), "<p>see <a class=\"reference-link\" href=\"#root/abc123\">My &lt;note&gt;</a> now</p>");
        assert_eq!(c, at(0, 5));
        assert_eq!(d.units(&d.outline().blocks[0].block)[4].label.as_deref(), Some("My <note>"));
        assert_eq!(d.target_at(at(0, 5)), Some(LinkTarget::Href("#root/abc123".into())));
        assert_eq!(d.target_at(at(0, 4)), Some(LinkTarget::Href("#root/abc123".into())));
        assert_eq!(d.target_at(at(0, 0)), None);
        let c = d.insert_link_text(at(0, 0), "http://x/?a=1&b=2", "site");
        assert_eq!(c, at(0, 4));
        assert!(d.html().starts_with("<p><a href=\"http://x/?a=1&amp;b=2\">site</a>see "), "{}", d.html());
        assert_eq!(d.target_at(at(0, 2)), Some(LinkTarget::Href("http://x/?a=1&b=2".into())));
        // pictures: their own block in a paragraph, inline elsewhere
        let mut d = doc("<p>before after</p>");
        let c = d.insert_image(at(0, 7), "api/attachments/A1/image/pic.png");
        assert_eq!(d.html(), "<p>before </p><figure class=\"image\"><img src=\"api/attachments/A1/image/pic.png\"></figure><p>after</p>");
        assert_eq!(c, at(2, 0));
        assert_eq!(d.target_at(at(1, 0)), Some(LinkTarget::Image("api/attachments/A1/image/pic.png".into())));
        let mut d = doc("<p>&nbsp;</p>");
        let c = d.insert_image(at(0, 0), "api/images/N/x.png");
        assert_eq!(d.html(), "<figure class=\"image\"><img src=\"api/images/N/x.png\"></figure><p>&nbsp;</p>");
        assert_eq!(c, at(1, 0));
        d.insert_text(c, "hi");
        assert!(d.html().ends_with("<p>hi</p>"));
        let mut d = doc("<ul><li>item</li></ul>");
        let c = d.insert_image(at(0, 4), "api/images/N/x.png");
        assert_eq!(d.html(), "<ul><li>item<img src=\"api/images/N/x.png\"></li></ul>");
        assert_eq!(c, at(0, 5));
        let mut d = doc("<p>x</p>");
        d.insert_image(at(0, 0), "a.png");
        assert!(d.html().starts_with("<figure class=\"image\"><img src=\"a.png\"></figure><p>x</p>"), "{}", d.html());
    }

    #[test]
    fn cursor_positions_map_to_html_offsets_and_back() {
        let h = "<h2>Head</h2>\r\n<p>one <strong>two</strong>&nbsp;x</p><table><tr><td>cell</td></tr></table><p>&nbsp;</p>";
        let d = doc(h);
        let ol = d.outline();
        for (i, bi) in ol.blocks.iter().enumerate() {
            for off in 0..=d.block_len(&bi.block) {
                let c = at(i, off);
                let o = d.html_offset(c);
                let back = Doc::cursor_at_html_offset(h, o);
                assert_eq!(d.html_offset(back), o, "{c:?} -> {o} -> {back:?}");
            }
        }
        assert!(h[d.html_offset(at(1, 4))..].starts_with("<strong>"), "after \"one \" comes the bold tag");
        // inside a tag or entity, and between blocks
        assert_eq!(Doc::cursor_at_html_offset(h, 2), at(0, 0), "inside <h2> lands at its text start");
        assert_eq!(Doc::cursor_at_html_offset(h, h.find("Head").unwrap() + 2), at(0, 2));
        assert_eq!(Doc::cursor_at_html_offset(h, h.find("&nbsp;x").unwrap() + 3), at(1, 7), "inside an entity is before it");
        assert_eq!(Doc::cursor_at_html_offset(h, h.find("\r\n").unwrap() + 1), at(1, 0), "between blocks starts the next block");
    }

    #[test]
    fn find_is_case_insensitive_and_wraps() {
        let d = doc("<p>Hello World</p><p>say hello</p>");
        assert_eq!(d.find("hello", at(0, 0)), Some((at(0, 0), at(0, 5))));
        assert_eq!(d.find("hello", at(0, 5)), Some((at(1, 4), at(1, 9))));
        assert_eq!(d.find("hello", at(1, 9)), Some((at(0, 0), at(0, 5))));
        assert_eq!(d.find("nope", at(0, 0)), None);
    }

    #[test]
    fn selected_text_and_words() {
        let d = doc("<p>one two</p><p>three</p>");
        assert_eq!(d.selected_text(at(0, 4), at(1, 3)), "two\nthr");
        assert_eq!(word_back(&d, at(0, 7)), at(0, 4));
        assert_eq!(word_forward(&d, at(0, 0)), at(0, 3));
        assert_eq!(word_forward(&d, at(0, 7)), at(1, 0));
        assert_eq!(word_back(&d, at(1, 0)), at(0, 7));
    }

    #[test]
    fn mixed_content_text_directly_in_a_container_is_wrapped_on_enter() {
        let mut d = doc("hello world");
        let c = d.split(at(0, 5));
        assert_eq!(c, at(1, 0));
        assert_eq!(d.html(), "<p>hello</p><p>&nbsp;world</p>");
        let mut d = doc("<blockquote>quoted<p>x</p></blockquote>");
        d.split(at(0, 3));
        assert_eq!(d.html(), "<blockquote><p>quo</p><p>ted</p><p>x</p></blockquote>");
    }

    /// Opt-in check against exported real notes: `RICH_NOTES_DIR=/path cargo test -p trilium-core real_notes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_notes_round_trip_and_survive_every_operation() {
        let Ok(dir) = std::env::var("RICH_NOTES_DIR") else { return };
        let (mut total, mut refused, mut reasons) = (0, 0, std::collections::BTreeMap::<String, usize>::new());
        let mut ops = 0usize;
        let mut cosmetic = 0usize;
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let html = std::fs::read_to_string(e.path()).unwrap();
            total += 1;
            if let Some(r) = unsupported_reason(&html) {
                refused += 1;
                *reasons.entry(r).or_default() += 1;
                continue;
            }
            let base = Doc::parse(&html);
            if base.html() != html && !html.is_empty() { cosmetic += 1; }
            let ol = base.outline();
            for (i, bi) in ol.blocks.iter().enumerate() {
                let len = base.block_len(&bi.block);
                for off in [0, len / 2, len] {
                    let c = at(i, off);
                    let mut d = base.clone(); d.insert_text(c, "x y"); ops += 1;
                    let mut d = base.clone(); d.split(c); ops += 1;
                    let mut d = base.clone(); d.soft_break(c); ops += 1;
                    let mut d = base.clone(); d.delete_back(c); ops += 1;
                    let mut d = base.clone(); d.delete_forward(c); ops += 1;
                    let mut d = base.clone(); d.indent(c); d.outdent(c); ops += 1;
                    let mut d = base.clone(); d.toggle_list(c, ListKind::Todo); ops += 1;
                    let mut d = base.clone(); d.set_block_tag(c, "h2"); d.quote(c); ops += 1;
                    let mut d = base.clone(); d.toggle_checkbox(c); ops += 1;
                    let mut d = base.clone(); d.autoformat(c); ops += 1;
                    let mut d = base.clone(); d.toggle_mark(at(i, 0), at(i, off), mark::BOLD); d.toggle_mark(at(i, 0), at(i, off), mark::BOLD); ops += 1;
                    let mut d = base.clone(); d.set_link(at(i, 0), at(i, off), Some("http://x")); ops += 1;
                    let _ = d.link_at(c);
                    let end = d.end_cursor();
                    let mut d = base.clone(); d.delete_range(c, end); ops += 1;
                    let reparsed = Doc::parse(&d.html());
                    assert!(d.html().is_empty() || reparsed.html() == d.html(), "an edited note must itself round-trip ({} block {i} off {off}: {:?} vs {:?})", e.path().display(), reparsed.html(), d.html());
                }
            }
            let end = base.end_cursor();
            let mut d = base.clone(); d.delete_range(at(0, 0), end); ops += 1;
            assert!(d.html().is_empty() || Doc::parse(&d.html()).html() == d.html(), "{}", e.path().display());
        }
        eprintln!("{total} notes, {refused} refused, {cosmetic} editable with void-tag respelling, {ops} operations");
        for (r, n) in reasons { eprintln!("  {n:>4}  {r}"); }
    }
}
