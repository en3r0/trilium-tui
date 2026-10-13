//! Port of `search/services/parse.ts` + `handle_parens.ts`.
use super::cmp::{Cmp, OPERATORS};
use super::lex::{Lexed, Tok};
use super::text::normalize;

pub enum Tree { T(Tok), G(Vec<Tree>) }

pub fn handle_parens(tokens: Vec<Tok>) -> Result<Vec<Tree>, String> {
    let mut stack: Vec<Vec<Tree>> = vec![vec![]];
    for t in tokens {
        match t.token.as_str() {
            "(" => stack.push(vec![]),
            ")" => {
                if stack.len() == 1 { return Err("Did not find matching left parenthesis.".into()); }
                let g = stack.pop().unwrap();
                stack.last_mut().unwrap().push(Tree::G(g));
            }
            _ => stack.last_mut().unwrap().push(Tree::T(t)),
        }
    }
    if stack.len() != 1 { return Err("Did not find matching right parenthesis.".into()); }
    Ok(stack.pop().unwrap())
}

#[derive(Debug, Clone)]
pub struct OrderDef { pub path: Vec<String>, pub desc: bool }

#[derive(Debug)]
pub enum Expr {
    True,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// kind_label: true for labels, false for relations
    AttrExists { label: bool, name: String },
    LabelCmp { name: String, cmp: Cmp },
    RelWhere { name: String, sub: Box<Expr> },
    Prop { prop: &'static str, cmp: Cmp },
    Content { op: String, tokens: Vec<String>, raw: bool, flat_text: bool, fuzzy_fallback: bool },
    FlatText(Vec<String>),
    ChildOf(Box<Expr>),
    ParentOf(Box<Expr>),
    DescendantOf(Box<Expr>),
    Linked { backlinks: bool, sub: Box<Expr> },
    OrderLimit { defs: Vec<OrderDef>, limit: usize, sub: Box<Expr> },
    Ancestor { id: String, depth: Option<String> },
    NotHidden,
}

impl Expr {
    fn and_of(v: Vec<Expr>) -> Expr {
        let mut v: Vec<Expr> = v.into_iter().filter(|e| !matches!(e, Expr::True)).collect();
        match v.len() { 0 => Expr::True, 1 => v.pop().unwrap(), _ => Expr::And(v) }
    }
    fn or_of(mut v: Vec<Expr>) -> Expr {
        match v.len() { 0 => Expr::True, 1 => v.pop().unwrap(), _ => Expr::Or(v) }
    }
}

/// Canonical property names (lowercase key -> property).
pub const PROPS: &[(&str, &str)] = &[
    ("noteid", "noteId"), ("title", "title"), ("type", "type"), ("mime", "mime"), ("isprotected", "isProtected"), ("isarchived", "isArchived"),
    ("datecreated", "dateCreated"), ("datemodified", "dateModified"), ("utcdatecreated", "utcDateCreated"), ("utcdatemodified", "utcDateModified"),
    ("parentcount", "parentCount"), ("childrencount", "childrenCount"), ("attributecount", "attributeCount"), ("labelcount", "labelCount"),
    ("ownedlabelcount", "ownedLabelCount"), ("relationcount", "relationCount"), ("ownedrelationcount", "ownedRelationCount"),
    ("relationcountincludinglinks", "relationCountIncludingLinks"), ("ownedrelationcountincludinglinks", "ownedRelationCountIncludingLinks"),
    ("targetrelationcount", "targetRelationCount"), ("targetrelationcountincludinglinks", "targetRelationCountIncludingLinks"),
    ("contentsize", "contentSize"), ("contentandattachmentssize", "contentAndAttachmentsSize"),
    ("contentandattachmentsandrevisionssize", "contentAndAttachmentsAndRevisionsSize"), ("revisioncount", "revisionCount"),
];

pub fn prop_name(key: &str) -> Option<&'static str> { PROPS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v) }

#[derive(Default, Debug, Clone)]
pub struct SearchOpts {
    pub fast_search: bool,
    pub include_archived: bool,
    pub include_hidden: bool,
    pub ancestor: Option<String>,
    pub ancestor_depth: Option<String>,
    pub order_by: Option<String>,
    pub order_desc: bool,
    pub limit: Option<usize>,
    pub fuzzy_attribute_search: bool,
    /// skip the typo-tolerant second phase
    pub exact_only: bool,
}

#[derive(Default)]
pub struct Ctx {
    pub error: Option<String>,
    pub highlighted: Vec<String>,
    pub original: String,
}

impl Ctx {
    fn err(&mut self, m: impl Into<String>) { if self.error.is_none() { self.error = Some(m.into()); } }
}

type R<T> = Result<T, String>;

fn tok(tokens: &[Tree], i: usize) -> R<&Tok> {
    match tokens.get(i) { Some(Tree::T(t)) => Ok(t), Some(Tree::G(_)) => Err("Unexpected parenthesized group".into()), None => Err("Unexpected end of expression".into()) }
}
fn is_operator(t: Option<&Tree>) -> bool { matches!(t, Some(Tree::T(t)) if OPERATORS.contains(&t.token.as_str())) }

struct P<'a> { ctx: &'a mut Ctx, fuzzy_attrs: bool }

impl P<'_> {
    fn context(&self, tokens: &[Tree], i: usize) -> String {
        let Ok(t) = tok(tokens, i) else { return String::new() };
        let chars: Vec<char> = self.ctx.original.chars().collect();
        let s = (t.start.max(0) as usize).saturating_sub(20);
        let e = ((t.end.max(0) as usize) + 20).min(chars.len());
        format!("\"{}{}{}\"", if s != 0 { "..." } else { "" }, chars[s.min(e)..e].iter().collect::<String>(), if e != chars.len() { "..." } else { "" })
    }

    fn resolve_constant(&mut self, tokens: &[Tree], i: &mut usize) -> R<Option<String>> {
        let operand = tok(tokens, *i)?.clone();
        if !operand.in_quotes && (operand.token.starts_with('#') || operand.token.starts_with('~') || operand.token == "note") {
            let hint = if operand.token == "note" { format!("\"{}\" is a reserved keyword", operand.token) } else { format!("Cannot compare with \"{}\"", operand.token) };
            let c = self.context(tokens, *i);
            self.ctx.err(format!("{c}: {hint}"));
            return Ok(None);
        }
        if operand.in_quotes || !["now", "today", "month", "year"].contains(&operand.token.as_str()) { return Ok(Some(operand.token)); }
        let mut delta: i64 = 0;
        if *i + 2 < tokens.len() {
            let sign = tok(tokens, *i + 1).map(|t| t.token.clone()).unwrap_or_default();
            if sign == "+" || sign == "-" {
                *i += 2;
                let n: i64 = tok(tokens, *i)?.token.parse().unwrap_or(0);
                delta = if sign == "+" { n } else { -n };
            }
        }
        use chrono::{Datelike, Duration, Local, Months};
        let now = Local::now();
        Ok(Some(match operand.token.as_str() {
            "now" => (now + Duration::seconds(delta)).format("%Y-%m-%d %H:%M:%S").to_string(),
            "today" => (now + Duration::days(delta)).format("%Y-%m-%d").to_string(),
            "month" => { let d = now.date_naive(); let r = if delta >= 0 { d.checked_add_months(Months::new(delta as u32)) } else { d.checked_sub_months(Months::new((-delta) as u32)) }; r.unwrap_or(d).format("%Y-%m").to_string() }
            _ => format!("{}", now.year() as i64 + delta),
        }))
    }

    fn parse_note_property(&mut self, tokens: &[Tree], i: &mut usize) -> R<Option<Expr>> {
        if tok(tokens, *i)?.token != "." { self.ctx.err("Expected \".\" to separate field path"); return Ok(None); }
        *i += 1;
        let name = tok(tokens, *i)?.token.clone();
        match name.as_str() {
            "content" | "rawcontent" => {
                let raw = name == "rawcontent";
                *i += 1;
                if !is_operator(tokens.get(*i)) {
                    let got = tok(tokens, *i).map(|t| t.token.clone()).unwrap_or_default();
                    let c = self.context(tokens, *i);
                    self.ctx.err(format!("After content expected operator, but got \"{got}\" in {c}"));
                    return Ok(None);
                }
                let op = tok(tokens, *i)?.token.clone();
                *i += 1;
                let val = tok(tokens, *i)?.token.clone();
                Ok(Some(Expr::Content { op, tokens: vec![val], raw, flat_text: false, fuzzy_fallback: false }))
            }
            "parents" | "children" | "ancestors" => {
                *i += 1;
                let Some(e) = self.parse_note_property(tokens, i)? else { return Ok(None) };
                Ok(Some(match name.as_str() { "parents" => Expr::ChildOf(Box::new(e)), "children" => Expr::ParentOf(Box::new(e)), _ => Expr::DescendantOf(Box::new(e)) }))
            }
            "links" | "backlinks" => {
                *i += 1;
                let Some(e) = self.parse_note_property(tokens, i)? else { return Ok(None) };
                Ok(Some(Expr::Linked { backlinks: name == "backlinks", sub: Box::new(e) }))
            }
            "labels" | "relations" => {
                if tok(tokens, *i + 1)?.token != "." {
                    let got = tok(tokens, *i + 1)?.token.clone();
                    let c = self.context(tokens, *i);
                    self.ctx.err(format!("Expected \".\" to separate field path, got \"{got}\" in {c}"));
                    return Ok(None);
                }
                *i += 2;
                let n = tok(tokens, *i)?.token.clone();
                if name == "labels" { self.parse_label(tokens, i, &n) } else { self.parse_relation(tokens, i, &n) }
            }
            "text" => {
                let op = tok(tokens, *i + 1)?.token.clone();
                if op != "*=*" {
                    let c = self.context(tokens, *i);
                    self.ctx.err(format!("Virtual attribute \"note.text\" supports only *=* operator, instead given \"{op}\" in {c}"));
                    return Ok(None);
                }
                *i += 2;
                let v = tok(tokens, *i)?.token.clone();
                Ok(Some(Expr::Or(vec![Expr::Prop { prop: "title", cmp: Cmp::build("*=*", &v).unwrap() }, Expr::Content { op: "*=*".into(), tokens: vec![v], raw: false, flat_text: false, fuzzy_fallback: false }])))
            }
            _ => {
                if let Some(prop) = prop_name(&name) {
                    let op = tok(tokens, *i + 1)?.token.clone();
                    *i += 2;
                    let Some(val) = self.resolve_constant(tokens, i)? else { self.ctx.err("Unresolved constant operand."); return Ok(None) };
                    return match Cmp::build(&op, &val) {
                        Some(cmp) => Ok(Some(Expr::Prop { prop, cmp })),
                        None => { self.ctx.err(format!("Unknown operator '{op}'")); Ok(None) }
                    };
                }
                let c = self.context(tokens, *i);
                self.ctx.err(format!("Unrecognized note property \"{name}\" in {c}"));
                Ok(None)
            }
        }
    }

    fn parse_label(&mut self, tokens: &[Tree], i: &mut usize, name: &str) -> R<Option<Expr>> {
        self.ctx.highlighted.push(name.into());
        if *i + 2 < tokens.len() && is_operator(tokens.get(*i + 1)) {
            let mut op = tok(tokens, *i + 1)?.token.clone();
            *i += 2;
            let Some(val) = self.resolve_constant(tokens, i)? else { return Ok(None) };
            self.ctx.highlighted.push(val.clone());
            if self.fuzzy_attrs && op == "=" { op = "*=*".into(); }
            return match Cmp::build(&op, &val) {
                Some(cmp) => Ok(Some(Expr::LabelCmp { name: name.to_lowercase(), cmp })),
                None => { let c = self.context(tokens, i.saturating_sub(1)); self.ctx.err(format!("Can't find operator '{op}' in {c}")); Ok(None) }
            };
        }
        Ok(Some(Expr::AttrExists { label: true, name: name.to_lowercase() }))
    }

    fn parse_relation(&mut self, tokens: &[Tree], i: &mut usize, name: &str) -> R<Option<Expr>> {
        self.ctx.highlighted.push(name.into());
        if *i + 2 < tokens.len() && tok(tokens, *i + 1).is_ok_and(|t| t.token == ".") {
            *i += 1;
            let Some(e) = self.parse_note_property(tokens, i)? else { return Ok(None) };
            return Ok(Some(Expr::RelWhere { name: name.to_lowercase(), sub: Box::new(e) }));
        } else if *i + 2 < tokens.len() && is_operator(tokens.get(*i + 1)) {
            let c = self.context(tokens, *i);
            self.ctx.err(format!("Relation can be compared only with property, e.g. ~relation.title=hello in {c}"));
            return Ok(None);
        }
        Ok(Some(Expr::AttrExists { label: false, name: name.to_lowercase() }))
    }

    fn parse_attribute(&mut self, tokens: &[Tree], i: &mut usize, token: &str) -> R<Option<Expr>> {
        let is_label = token.starts_with('#');
        let mut name = &token[1..];
        let negated = name.starts_with('!');
        if negated { name = &name[1..]; }
        let sub = if is_label { self.parse_label(tokens, i, name)? } else { self.parse_relation(tokens, i, name)? };
        Ok(sub.map(|e| if negated { Expr::Not(Box::new(e)) } else { e }))
    }

    fn get_expression(&mut self, tokens: &[Tree], level: usize) -> R<Option<Expr>> {
        if tokens.is_empty() { return Ok(None); }
        let mut exprs: Vec<Expr> = vec![];
        let mut op: Option<String> = None;
        let mut i = 0usize;
        macro_rules! aggregate {
            () => {{
                let e = std::mem::take(&mut exprs);
                match op.as_deref() { None | Some("and") => Expr::and_of(e), Some("or") => Expr::or_of(e), Some(o) => return Err(format!("Unrecognized op={o}")) }
            }};
        }
        while i < tokens.len() {
            match &tokens[i] {
                Tree::G(g) => {
                    if let Some(e) = self.get_expression(g, level + 1)? { exprs.push(e); }
                    if op.is_none() && exprs.len() > 1 { op = Some("and".into()); }
                    i += 1;
                    continue;
                }
                Tree::T(t) => {
                    let token = t.token.clone();
                    if token == "#" || token == "~" { i += 1; continue; }
                    if token.starts_with('#') || token.starts_with('~') {
                        if let Some(a) = self.parse_attribute(tokens, &mut i, &token)? { exprs.push(a); }
                    } else if token == "orderby" || token == "limit" {
                        if level != 0 { self.ctx.err("orderBy can appear only on the top expression level"); i += 1; continue; }
                        let (defs, limit) = self.parse_order_limit(tokens, &mut i)?;
                        let sub = aggregate!();
                        return Ok(Some(Expr::OrderLimit { defs, limit, sub: Box::new(sub) }));
                    } else if token == "not" {
                        i += 1;
                        match tokens.get(i) {
                            Some(Tree::G(g)) => {
                                let Some(e) = self.get_expression(g, level + 1)? else { return Ok(None) };
                                exprs.push(Expr::Not(Box::new(e)));
                            }
                            other => {
                                let got = match other { Some(Tree::T(t)) => t.token.clone(), _ => String::new() };
                                self.ctx.err(format!("not keyword should be followed by sub-expression in parenthesis, got {got} instead"));
                                i += 1;
                                continue;
                            }
                        }
                    } else if token == "note" {
                        i += 1;
                        let Some(e) = self.parse_note_property(tokens, &mut i)? else { return Ok(None) };
                        exprs.push(e);
                        i += 1;
                        if op.is_none() && exprs.len() > 1 { op = Some("and".into()); }
                        continue;
                    } else if token == "and" || token == "or" {
                        match &op {
                            None => op = Some(token),
                            Some(o) if *o != token => self.ctx.err("Mixed usage of AND/OR - always use parenthesis to group AND/OR expressions."),
                            _ => {}
                        }
                    } else if OPERATORS.contains(&token.as_str()) {
                        self.ctx.err(format!("Misplaced operator \"{token}\""));
                    } else {
                        self.ctx.err(format!("Full-text search terms must come before attribute filters; found \"{token}\""));
                    }
                }
            }
            if op.is_none() && exprs.len() > 1 { op = Some("and".into()); }
            i += 1;
        }
        Ok(Some(aggregate!()))
    }

    fn parse_order_limit(&mut self, tokens: &[Tree], i: &mut usize) -> R<(Vec<OrderDef>, usize)> {
        let mut defs = vec![];
        let mut limit = 0usize;
        if tok(tokens, *i)?.token == "orderby" {
            loop {
                let mut path = vec![];
                let mut desc = false;
                loop {
                    *i += 1;
                    path.push(tok(tokens, *i)?.token.clone());
                    *i += 1;
                    if !(*i < tokens.len() && tok(tokens, *i)?.token == ".") { break; }
                }
                if *i < tokens.len() {
                    let t = tok(tokens, *i)?.token.clone();
                    if t == "asc" || t == "desc" { desc = t == "desc"; *i += 1; }
                }
                let def = OrderDef { path: path.iter().map(|p| p.to_lowercase()).collect(), desc };
                if let Some(e) = validate_order(&def) { self.ctx.err(e); }
                defs.push(def);
                if !(*i < tokens.len() && tok(tokens, *i).is_ok_and(|t| t.token == ",")) { break; }
            }
        }
        if *i < tokens.len() && tok(tokens, *i).is_ok_and(|t| t.token == "limit") {
            limit = tok(tokens, *i + 1)?.token.parse().unwrap_or(0);
        }
        Ok((defs, limit))
    }
}

/// `ValueExtractor.validate` after the `#x` / `~x` shorthand has been expanded.
pub fn expand_order_path(path: &[String]) -> Vec<String> {
    let first = path[0].clone();
    if let Some(l) = first.strip_prefix('#') { let mut v = vec!["note".to_string(), "labels".into(), l.into()]; v.extend_from_slice(&path[1..]); v }
    else if let Some(r) = first.strip_prefix('~') { let mut v = vec!["note".to_string(), "relations".into(), r.into()]; v.extend_from_slice(&path[1..]); v }
    else { path.to_vec() }
}

fn validate_order(def: &OrderDef) -> Option<String> {
    let p = expand_order_path(&def.path);
    if p[0] != "note" { return Some(format!("property specifier must start with 'note', but starts with '{}'", p[0])); }
    let mut i = 1;
    while i < p.len() {
        let el = p[i].as_str();
        if el == "labels" {
            if i != p.len() - 2 { return Some("label is a terminal property specifier and must be at the end".into()); }
            i += 1;
        } else if el == "relations" {
            if i >= p.len().saturating_sub(2) { return Some("relation name or property name is missing".into()); }
            i += 1;
        } else if prop_name(el).is_some() || el == "random" {
            if i != p.len() - 1 { return Some(format!("{el} is a terminal property specifier and must be at the end")); }
        } else if !["parents", "children"].contains(&el) {
            return Some(format!("Unrecognized property specifier {el}"));
        }
        i += 1;
    }
    None
}

pub struct Parsed { pub expr: Expr, pub fulltext_query: String, pub fulltext_tokens: Vec<String> }

pub fn parse(query: &str, opts: &SearchOpts, ctx: &mut Ctx) -> Parsed {
    ctx.original = query.to_string();
    let lexed: Lexed = super::lex::lex(query);
    let tokens: Vec<String> = lexed.fulltext_tokens.iter().map(|t| normalize_keep_case(&t.token)).collect();
    ctx.highlighted.extend(tokens.iter().cloned());

    let expression = match handle_parens(lexed.expression_tokens) {
        Ok(tree) => {
            let mut p = P { ctx, fuzzy_attrs: opts.fuzzy_attribute_search };
            match p.get_expression(&tree, 0) { Ok(e) => e, Err(m) => { p.ctx.err(m); Some(Expr::True) } }
        }
        Err(m) => { ctx.err(m); Some(Expr::True) }
    };

    let fulltext = if tokens.is_empty() { None } else if opts.fast_search {
        Some(Expr::FlatText(tokens.clone()))
    } else if lexed.leading_operator == "=" {
        let phrase = tokens.join(" ");
        Some(Expr::Or(vec![
            Expr::Prop { prop: "title", cmp: Cmp::build("word=", &phrase).unwrap() },
            Expr::Content { op: "=".into(), tokens: tokens.clone(), raw: false, flat_text: false, fuzzy_fallback: false },
            Expr::Content { op: "=".into(), tokens: tokens.clone(), raw: false, flat_text: true, fuzzy_fallback: false },
        ]))
    } else {
        Some(Expr::Or(vec![Expr::FlatText(tokens.clone()), Expr::Content { op: "*=*".into(), tokens: tokens.clone(), raw: false, flat_text: true, fuzzy_fallback: true }]))
    };

    let ancestor = match &opts.ancestor {
        Some(a) if a != "root" => Some(Expr::Ancestor { id: a.clone(), depth: opts.ancestor_depth.clone() }),
        _ if !opts.include_hidden => Some(Expr::Not(Box::new(Expr::NotHidden))),
        _ => None,
    };
    let archived = if opts.include_archived { None } else { Some(Expr::Prop { prop: "isArchived", cmp: Cmp::build("=", "false").unwrap() }) };
    let mut exp = Expr::and_of([archived, ancestor, fulltext, expression].into_iter().flatten().collect());

    if let (Some(l), None) = (opts.limit.filter(|l| *l > 0), &opts.order_by) {
        exp = Expr::OrderLimit { defs: vec![], limit: l, sub: Box::new(exp) };
    }
    if let Some(ob) = opts.order_by.as_ref().filter(|o| *o != "relevancy") {
        exp = Expr::OrderLimit { defs: vec![OrderDef { path: vec!["note".into(), ob.to_lowercase()], desc: opts.order_desc }], limit: opts.limit.unwrap_or(0), sub: Box::new(exp) };
    }
    Parsed { expr: exp, fulltext_query: lexed.fulltext_query, fulltext_tokens: tokens }
}

/// `removeDiacritic` only (the lexer already lowercased).
fn normalize_keep_case(s: &str) -> String { normalize(s) }
