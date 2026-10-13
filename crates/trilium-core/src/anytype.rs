//! Anytype JSON ("any-block") export import, ported from Trilium's `import/anytype/`. Each page's block tree becomes
//! HTML (headings, marks, lists, toggles, callouts, quotes, code, tables, math, bookmarks, files, links), collections become
//! table/list/grid/calendar/board collections with promoted columns, and custom relations become labels and file
//! attachments. Pages keep their creation and modification times.
use crate::{
    db::Db,
    formats::{promoted_definition, to_attribute_name},
    htmltree::{El, Node, escape_attr, escape_text, map_els, parse, serialize},
    transfer::{Entry, ImportStats},
    write::new_entity_id,
};
use anyhow::{Result, bail};
use chrono::TimeZone;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn norm_path(p: &str) -> String { p.replace('\\', "/").to_lowercase() }
fn base_name(p: &str) -> &str { p.rsplit('/').next().unwrap_or(p) }

fn s<'a>(v: &'a Value, key: &str) -> &'a str { v.get(key).and_then(Value::as_str).unwrap_or("") }

/// Does the archive look like an Anytype JSON export (`objects/<cid>.pb.json`)?
pub fn looks_like_export(entries: &[Entry]) -> bool {
    entries.iter().any(|e| { let p = norm_path(&e.path); p.starts_with("objects/") && (p.ends_with(".pb.json") || p.ends_with(".pb")) })
}

// ------------------------------------------------------------------ inline text

const MARK_ORDER: [&str; 5] = ["Bold", "Italic", "Underscored", "Strikethrough", "Keyboard"];

fn mark_tag(t: &str) -> Option<&'static str> {
    Some(match t { "Bold" => "strong", "Italic" => "em", "Underscored" => "u", "Strikethrough" => "s", "Keyboard" => "code", _ => return None })
}

fn text_color(n: &str) -> Option<&'static str> {
    Some(match n { "grey" => "#8c9ea5", "yellow" => "#b2a616", "orange" => "#d3720d", "red" => "#e2400c", "pink" => "#ca1b8e", "purple" => "#9e30c4", "blue" => "#3e58eb", "ice" => "#1c8bca", "teal" => "#0caaa3", "lime" => "#64b90f", _ => return None })
}
fn bg_color(n: &str) -> Option<&'static str> {
    Some(match n { "grey" => "#e3e3e3", "yellow" => "#f4eb91", "orange" => "#fcdc9c", "red" => "#fcd1c3", "pink" => "#f8c2e5", "purple" => "#e8d0f1", "blue" => "#cbd2fa", "ice" => "#b2dff9", "teal" => "#a9ebe6", "lime" => "#c5efa3", _ => return None })
}

struct Mark { kind: String, param: String, from: usize, to: usize, href: Option<String> }

type Resolver<'a> = &'a dyn Fn(&str) -> Option<(String, String)>;

fn utf16_slice(u: &[u16], a: usize, b: usize) -> String { String::from_utf16_lossy(&u[a..b]) }

fn render_marked(units: &[u16], marks: &[Value], resolve: Resolver, on_link: &mut dyn FnMut(&str)) -> String {
    let len = units.len();
    let mut applicable: Vec<Mark> = vec![];
    for m in marks {
        let param = s(m, "param").to_string();
        let kind = s(m, "type").to_string();
        let from = (m["range"]["from"].as_i64().unwrap_or(0).max(0) as usize).min(len);
        let to = (m["range"]["to"].as_i64().unwrap_or(0).max(0) as usize).min(len);
        if from >= to { continue; }
        if kind == "Mention" {
            if let Some((id, _)) = (!param.is_empty()).then(|| resolve(&param)).flatten() {
                on_link(&id);
                applicable.push(Mark { kind, param, from, to, href: Some(format!("#root/{id}")) });
            }
            continue;
        }
        let ok = mark_tag(&kind).is_some() || (kind == "TextColor" && text_color(&param).is_some()) || (kind == "BackgroundColor" && bg_color(&param).is_some());
        if ok { applicable.push(Mark { kind, param, from, to, href: None }); }
    }
    // overlapping marks nest tags per segment (quadratic output): more than this many marks in one block are not rendered
    applicable.truncate(400);
    if applicable.is_empty() { return escape_text(&String::from_utf16_lossy(units)); }
    let mut points: Vec<usize> = vec![0, len];
    for m in &applicable { points.push(m.from); points.push(m.to); }
    points.sort_unstable();
    points.dedup();
    let mut html = String::new();
    for w in points.windows(2) {
        let (start, end) = (w[0], w[1]);
        let covering: Vec<&Mark> = applicable.iter().filter(|m| m.from <= start && m.to >= end).collect();
        html.push_str(&wrap_segment(&escape_text(&utf16_slice(units, start, end)), &covering));
    }
    html
}

fn wrap_segment(segment: &str, covering: &[&Mark]) -> String {
    let tc = covering.iter().find(|m| m.kind == "TextColor").and_then(|m| text_color(&m.param));
    let bg = covering.iter().find(|m| m.kind == "BackgroundColor").and_then(|m| bg_color(&m.param));
    let mut style: Vec<String> = vec![];
    if let Some(c) = tc { style.push(format!("color:{c}")); } else if bg.is_some() { style.push("color:#252525".into()); }
    if let Some(b) = bg { style.push(format!("background-color:{b}")); }
    let mut html = if style.is_empty() { segment.to_string() } else { format!("<span style=\"{}\">{segment}</span>", style.join(";")) };
    let mut structural: Vec<&&Mark> = covering.iter().filter(|m| mark_tag(&m.kind).is_some()).collect();
    structural.sort_by_key(|m| MARK_ORDER.iter().position(|k| *k == m.kind));
    for m in structural.iter().rev() { let t = mark_tag(&m.kind).unwrap(); html = format!("<{t}>{html}</{t}>"); }
    if let Some(h) = covering.iter().find(|m| m.kind == "Mention").and_then(|m| m.href.as_ref()) { html = format!("<a class=\"reference-link\" href=\"{h}\">{html}</a>"); }
    html
}

struct Formula { from: usize, to: usize, body: (usize, usize), display: bool }

/// Inline `$…$` / `$$…$$` runs (same delimiter rules as the Markdown renderer), as UTF-16 offsets.
fn find_formulas(u: &[u16]) -> Vec<Formula> {
    let ch = |i: usize| u.get(i).copied();
    let mut out = vec![];
    let mut i = 0;
    while i < u.len() {
        if u[i] == b'$' as u16 && (i == 0 || (u[i - 1] != b'\\' as u16 && u[i - 1] != b'$' as u16)) {
            if ch(i + 1) == Some(b'$' as u16) && ch(i + 2) != Some(b'$' as u16) {
                let mut j = i + 2;
                let mut found = None;
                while j < u.len() {
                    if u[j] == b'$' as u16 { if ch(j + 1) == Some(b'$' as u16) && ch(j + 2) != Some(b'$' as u16) && j > i + 2 { found = Some(j); } break; }
                    if u[j] == b'\n' as u16 && ch(j + 1) == Some(b'\n' as u16) { break; }
                    j += 1;
                }
                if let Some(j) = found { out.push(Formula { from: i, to: j + 2, body: (i + 2, j), display: true }); i = j + 2; continue; }
            } else if let Some(n) = ch(i + 1).filter(|n| !is_space(*n) && *n != b'$' as u16) {
                let _ = n;
                let mut j = i + 1;
                let mut found = None;
                while j < u.len() && u[j] != b'\n' as u16 {
                    if u[j] == b'$' as u16 { found = Some(j); break; }
                    j += 1;
                }
                if let Some(j) = found.filter(|j| *j > i + 1) {
                    let before = u[j - 1];
                    let after = ch(j + 1);
                    if !is_space(before) && before != b'$' as u16 && after != Some(b'$' as u16) && !after.is_some_and(|a| (b'0' as u16..=b'9' as u16).contains(&a)) {
                        out.push(Formula { from: i, to: j + 1, body: (i + 1, j), display: false });
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

fn is_space(c: u16) -> bool { char::from_u32(c as u32).is_some_and(char::is_whitespace) }

fn quote_fix(s: String) -> String { s.replace("&quot;", "\"") }

pub(crate) fn render_inline(text: &str, marks: &[Value], resolve: Resolver, on_link: &mut dyn FnMut(&str)) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let formulas = find_formulas(&units);
    if formulas.is_empty() { return render_marked(&units, marks, resolve, on_link); }
    let mut html = String::new();
    let mut cursor = 0;
    let slice = |start: usize, end: usize, html: &mut String, on_link: &mut dyn FnMut(&str)| {
        if end <= start { return; }
        let sub: Vec<Value> = marks.iter().filter_map(|m| {
            let f = m["range"]["from"].as_i64().unwrap_or(0) - start as i64;
            let t = m["range"]["to"].as_i64().unwrap_or(0) - start as i64;
            if t > 0 && f < (end - start) as i64 { let mut m = m.clone(); m["range"] = serde_json::json!({"from": f, "to": t}); Some(m) } else { None }
        }).collect();
        html.push_str(&render_marked(&units[start..end], &sub, resolve, on_link));
    };
    for f in &formulas {
        slice(cursor, f.from, &mut html, on_link);
        let body = quote_fix(escape_text(&utf16_slice(&units, f.body.0, f.body.1)));
        html.push_str(&if f.display { format!("<span class=\"math-tex\">\\[ {body} \\]</span>") } else { format!("<span class=\"math-tex\">\\( {body} \\)</span>") });
        cursor = f.to;
    }
    slice(cursor, units.len(), &mut html, on_link);
    html
}

// ------------------------------------------------------------------ block tree

#[derive(Clone, Copy, PartialEq)]
enum ListKind { Ul, Ol, Task }

fn list_kind(b: &Value) -> Option<ListKind> {
    match b["text"]["style"].as_str() { Some("Marked") => Some(ListKind::Ul), Some("Numbered") => Some(ListKind::Ol), Some("Checkbox") => Some(ListKind::Task), _ => None }
}

fn tag_for_style(style: &str) -> &'static str {
    match style { "Header1" | "ToggleHeader1" => "h2", "Header2" | "ToggleHeader2" => "h3", "Header3" | "ToggleHeader3" => "h4", _ => "p" }
}

fn children(b: &Value) -> Vec<String> { b["childrenIds"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect() }

struct Renderer<'a> {
    by_id: HashMap<String, &'a Value>,
    visited: HashSet<String>,
    /// nesting of `sequence` calls: a flat block list can chain thousands of parents and must not overflow the stack
    depth: usize,
    links: Vec<String>,
    files: Vec<String>,
    resolve: Resolver<'a>,
}

impl Renderer<'_> {
    fn inline(&mut self, b: &Value) -> String {
        let marks = b["text"]["marks"]["marks"].as_array().cloned().unwrap_or_default();
        let resolve = self.resolve;
        let mut found = vec![];
        let html = render_inline(s(&b["text"], "text"), &marks, resolve, &mut |id| found.push(id.to_string()));
        for f in found { if !self.links.contains(&f) { self.links.push(f); } }
        html
    }

    fn sequence(&mut self, ids: &[String]) -> String {
        if self.depth >= 64 { return String::new(); }
        self.depth += 1;
        let r = self.sequence_inner(ids);
        self.depth -= 1;
        r
    }

    fn sequence_inner(&mut self, ids: &[String]) -> String {
        let mut parts = String::new();
        let mut i = 0;
        while i < ids.len() {
            let id = &ids[i];
            let block = if id == "header" || self.visited.contains(id) { None } else { self.by_id.get(id).copied() };
            let Some(block) = block else { i += 1; continue };
            if let Some(kind) = list_kind(block) {
                let mut run: Vec<&Value> = vec![];
                while i < ids.len() {
                    let cand = if self.visited.contains(&ids[i]) { None } else { self.by_id.get(&ids[i]).copied() };
                    match cand { Some(c) if list_kind(c) == Some(kind) => { self.visited.insert(ids[i].clone()); run.push(c); i += 1; } _ => break }
                }
                let body: String = run.iter().map(|it| self.item(kind, it)).collect();
                parts.push_str(&match kind { ListKind::Task => format!("<ul class=\"todo-list\">{body}</ul>"), ListKind::Ol => format!("<ol>{body}</ol>"), ListKind::Ul => format!("<ul>{body}</ul>") });
            } else {
                self.visited.insert(id.clone());
                parts.push_str(&self.leaf(block));
                i += 1;
            }
        }
        parts
    }

    fn item(&mut self, kind: ListKind, item: &Value) -> String {
        let text = self.inline(item);
        let nested = self.sequence(&children(item));
        if kind == ListKind::Task {
            let cb = format!("<input type=\"checkbox\"{}disabled=\"disabled\">", if item["text"]["checked"].as_bool() == Some(true) { " checked=\"checked\" " } else { "" });
            return format!("<li><label class=\"todo-list__label\">{cb}<span class=\"todo-list__label__description\">{text}</span></label>{nested}</li>");
        }
        format!("<li>{text}{nested}</li>")
    }

    fn leaf(&mut self, b: &Value) -> String {
        if b.get("div").is_some() { return "<hr>".into(); }
        if let Some(l) = b.get("link") {
            let Some((id, title)) = l["targetBlockId"].as_str().and_then(|t| (self.resolve)(t)) else { return String::new() };
            if !self.links.contains(&id) { self.links.push(id.clone()); }
            return format!("<p><a class=\"reference-link\" href=\"#root/{id}\">{}</a></p>", escape_text(&title));
        }
        if let Some(bm) = b.get("bookmark") {
            for h in ["faviconHash", "imageHash"] { if let Some(x) = bm[h].as_str().filter(|x| !x.is_empty()) { self.files.push(x.to_string()); } }
            return render_bookmark(bm);
        }
        if let Some(f) = b.get("file") {
            let Some(target) = f["targetObjectId"].as_str().filter(|t| !t.is_empty()) else { return String::new() };
            self.files.push(target.to_string());
            if f["type"].as_str() == Some("Image") { return format!("<figure class=\"image\"><img src=\"{}\"></figure>", escape_attr(target)); }
            let name = s(f, "name").trim();
            let name = if name.is_empty() { target } else { name };
            return format!("<p><a class=\"anytype-file\" href=\"{}\">{}</a></p>", escape_attr(target), escape_text(name));
        }
        if let Some(l) = b.get("latex") {
            let text = s(l, "text");
            if text.trim().is_empty() { return String::new(); }
            let esc = quote_fix(escape_text(text));
            return if l["processor"].as_str() == Some("Mermaid") { format!("<pre><code class=\"language-mermaid\">{esc}</code></pre>") } else { format!("<span class=\"math-tex\">\\[ {esc} \\]</span>") };
        }
        if b.get("table").is_some() { return self.table(b); }
        let raw = s(&b["text"], "text").to_string();
        let style = s(&b["text"], "style").to_string();
        let kids = children(b);
        match style.as_str() {
            "Toggle" => { let t = self.inline(b); return format!("<details class=\"trilium-collapsible\"><summary>{t}</summary>{}</details>", self.sequence(&kids)); }
            "Quote" => {
                let first = if raw.trim().is_empty() { String::new() } else { format!("<p>{}</p>", self.inline(b)) };
                return format!("<blockquote>{first}{}</blockquote>", self.sequence(&kids));
            }
            "Callout" => {
                let emoji = s(&b["text"], "iconEmoji").to_string();
                let ty = if emoji.is_empty() { "tip" } else { "note" };
                let lead: Vec<String> = [escape_text(&emoji), self.inline(b)].into_iter().filter(|x| !x.is_empty()).collect();
                let first = if lead.is_empty() { String::new() } else { format!("<p>{}</p>", lead.join(" ")) };
                return format!("<aside class=\"admonition {ty}\">{first}{}</aside>", self.sequence(&kids));
            }
            _ => {}
        }
        let mut html = String::new();
        if !raw.trim().is_empty() && style != "Title" && style != "Description" {
            if style == "Code" {
                let lang = b["fields"]["lang"].as_str();
                html = format!("<pre><code class=\"language-{}\">{}</code></pre>", crate::formats::code_language(lang), quote_fix(escape_text(&raw)));
            } else {
                let tag = tag_for_style(&style);
                html = format!("<{tag}>{}</{tag}>", self.inline(b));
            }
        }
        html + &self.sequence(&kids)
    }

    fn table(&mut self, table: &Value) -> String {
        let kids = children(table);
        let col_ids = kids.first().and_then(|id| self.by_id.get(id)).map(|b| children(b)).unwrap_or_default();
        let row_ids = kids.get(1).and_then(|id| self.by_id.get(id)).map(|b| children(b)).unwrap_or_default();
        if col_ids.is_empty() || row_ids.is_empty() { return String::new(); }
        let (mut head, mut body) = (String::new(), String::new());
        for rid in &row_ids {
            let Some(row) = self.by_id.get(rid).copied() else { continue };
            let is_header = row["tableRow"]["isHeader"].as_bool() == Some(true);
            let mut by_col: HashMap<String, &Value> = HashMap::new();
            for cid in children(row) { if let (Some(cell), Some(col)) = (self.by_id.get(&cid).copied(), cid.strip_prefix(&format!("{rid}-"))) { by_col.insert(col.to_string(), cell); } }
            let (tag, scope) = if is_header { ("th", " scope=\"col\"") } else { ("td", "") };
            let cells: String = col_ids.iter().map(|c| {
                let content = by_col.get(c).map(|cell| self.inline(cell)).unwrap_or_default();
                format!("<{tag}{scope}>{content}</{tag}>")
            }).collect();
            let r = format!("<tr>{cells}</tr>");
            if is_header { head.push_str(&r); } else { body.push_str(&r); }
        }
        let thead = if head.is_empty() { String::new() } else { format!("<thead>{head}</thead>") };
        let tbody = if body.is_empty() { String::new() } else { format!("<tbody>{body}</tbody>") };
        format!("<figure class=\"table\"><table>{thead}{tbody}</table></figure>")
    }
}

fn render_bookmark(bm: &Value) -> String {
    let url = s(bm, "url").trim();
    if url.is_empty() { return String::new(); }
    let mut attrs = format!("data-url=\"{}\" data-embed-type=\"opengraph\"", escape_attr(url));
    for (k, a) in [("title", "data-title"), ("description", "data-description"), ("faviconHash", "data-favicon"), ("imageHash", "data-image")] {
        let v = s(bm, k).trim();
        if !v.is_empty() { attrs.push_str(&format!(" {a}=\"{}\"", escape_attr(v))); }
    }
    format!("<section class=\"link-embed\" {attrs}></section>")
}

/// (html, ids of linked notes, ids of inline file objects)
fn extract_content<'a>(blocks: &'a [Value], root_id: &str, resolve: Resolver<'a>) -> (String, Vec<String>, Vec<String>) {
    let by_id: HashMap<String, &Value> = blocks.iter().filter_map(|b| b["id"].as_str().map(|i| (i.to_string(), b))).collect();
    let root = (!root_id.is_empty()).then(|| by_id.get(root_id).copied()).flatten().or(blocks.first());
    let Some(root) = root else { return (String::new(), vec![], vec![]) };
    let mut r = Renderer { by_id, visited: HashSet::new(), depth: 0, links: vec![], files: vec![], resolve };
    let html = r.sequence(&children(root));
    let mut files = vec![];
    for f in r.files { if !files.contains(&f) { files.push(f); } }
    (html, r.links, files)
}

// ------------------------------------------------------------------ properties and collections

#[derive(Clone)]
struct Relation { name: String, format: i64, include_time: bool }
#[derive(Clone)]
struct FileObj { title: String, mime: String, source: String }
#[derive(Clone)]
struct Column { name: String, label_type: &'static str, alias: String, multi: bool }

struct Collection { view_type: &'static str, group_by: Option<String>, members: Vec<String>, columns: Vec<Column> }

struct Page {
    id: String,
    title: String,
    content: String,
    #[allow(dead_code)]
    link_ids: Vec<String>,
    created: Option<String>,
    modified: Option<String>,
    props: Vec<(String, String)>,
    file_refs: Vec<String>,
    inline_files: Vec<String>,
    collection: Option<Collection>,
}

fn is_custom_key(k: &str) -> bool { k.len() >= 16 && k.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

/// (label type, multi, option-backed)
fn property_mapping(r: &Relation) -> Option<(&'static str, bool, bool)> {
    Some(match r.format {
        0 | 1 => ("text", false, false),
        2 => ("number", false, false),
        3 => ("text", false, true),
        4 => (if r.include_time { "datetime" } else { "date" }, false, false),
        6 => ("boolean", false, false),
        7 => ("url", false, false),
        8 => ("email", false, false),
        9 => ("phone", false, false),
        11 => ("text", true, true),
        _ => return None,
    })
}

fn format_property(raw: &Value, label_type: &str) -> Option<String> {
    if raw.is_null() { return None; }
    match label_type {
        "boolean" => match raw { Value::Bool(b) => Some(b.to_string()), Value::String(s) if s == "true" || s == "false" => Some(s.clone()), _ => None },
        "date" | "datetime" => {
            let secs = raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.parse().ok()))?;
            let local = chrono::Local.timestamp_opt(secs as i64, 0).single()?;
            Some(local.format(if label_type == "datetime" { "%Y-%m-%dT%H:%M" } else { "%Y-%m-%d" }).to_string())
        }
        "number" => { let n = raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()))?; n.is_finite().then(|| if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }) }
        _ => {
            let v = match raw { Value::String(s) => s.clone(), other => other.to_string() };
            if v.trim().is_empty() { return None; }
            Some(match label_type { "email" => v.strip_prefix("mailto:").unwrap_or(&v).to_string(), "phone" => v.strip_prefix("tel:").unwrap_or(&v).to_string(), _ => v })
        }
    }
}

fn view_type(t: &str) -> &'static str { match t { "List" => "list", "Gallery" => "grid", "Calendar" => "calendar", "Kanban" => "board", _ => "table" } }

fn columns_from_keys(keys: &[String], rels: &HashMap<String, Relation>) -> Vec<Column> {
    let mut cols: Vec<Column> = vec![];
    for k in keys {
        if !is_custom_key(k) { continue; }
        let Some(info) = rels.get(k) else { continue };
        let Some((lt, multi, _)) = property_mapping(info) else { continue };
        let name = to_attribute_name(&info.name);
        if !cols.iter().any(|c| c.name == name) { cols.push(Column { name, label_type: lt, alias: info.name.clone(), multi }); }
    }
    cols
}

fn utc_from_secs(v: &Value) -> Option<String> {
    let s = v.as_f64().filter(|s| s.is_finite() && *s > 0.0)?;
    chrono::DateTime::from_timestamp(s as i64, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S%.3fZ").to_string())
}

fn page_title(details: &Value) -> String { let n = s(details, "name").trim(); if n.is_empty() { "Untitled".into() } else { n.to_string() } }

fn is_page(snap: &Value) -> bool {
    if s(snap, "sbType") != "Page" { return false; }
    let d = &snap["snapshot"]["data"]["details"];
    d.get("layout").and_then(Value::as_i64).or_else(|| d.get("resolvedLayout").and_then(Value::as_i64)).unwrap_or(0) == 0
}

fn is_collection_object(snap: &Value) -> bool {
    s(snap, "sbType") == "Page" && snap["snapshot"]["data"]["blocks"].as_array().is_some_and(|bs| bs.iter().any(|b| b["dataview"]["isCollection"].as_bool() == Some(true)))
}

fn sniff_image(b: &[u8]) -> Option<(&'static str, &'static str)> {
    Some(if b.starts_with(b"\x89PNG") { ("image/png", "png") } else if b.starts_with(&[0xff, 0xd8, 0xff]) { ("image/jpeg", "jpg") } else if b.starts_with(b"GIF8") { ("image/gif", "gif") }
        else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { ("image/webp", "webp") } else if b.starts_with(&[0, 0, 1, 0]) { ("image/x-icon", "ico") } else if b.starts_with(b"BM") { ("image/bmp", "bmp") }
        else { return None })
}

fn safe_host(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?', '#']).next().unwrap_or("");
    let h: String = host.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).collect();
    if h.is_empty() { "site".into() } else { h }
}

impl Db {
    /// Import an Anytype JSON export under `parent_id`. `archive_name` titles the root of a collection-scoped export.
    pub fn import_anytype(&self, entries: Vec<Entry>, parent_id: &str, archive_name: Option<&str>) -> Result<ImportStats> {
        let exists: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)))?;
        anyhow::ensure!(exists.is_some(), "import target {parent_id} not found");
        // Anytype's Markdown export is a plain folder of Markdown files with a files/ directory
        let has = |f: &dyn Fn(&str) -> bool| entries.iter().any(|e| !e.is_dir && f(&norm_path(&e.path)));
        if !has(&|p| p.starts_with("objects/")) && has(&|p| p.ends_with(".md")) {
            return self.import_obsidian(entries, parent_id, archive_name);
        }
        let (mut objects, mut relations, mut options, mut file_objs): (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) = Default::default();
        let mut files: HashMap<String, Vec<u8>> = HashMap::new();
        let (mut pb_count, mut md_count) = (0, 0);
        for e in entries {
            if e.is_dir { continue; }
            let p = norm_path(&e.path);
            if p.starts_with("files/") { files.insert(p, e.data); continue; }
            if p.ends_with(".pb") {
                // Protobuf export: decode into the same snapshot shape as the JSON export
                let kind = ["objects", "relationsoptions", "relations", "filesobjects"].into_iter().find(|k| p.starts_with(&format!("{k}/")));
                let Some(kind) = kind else { continue };
                pb_count += 1;
                let (bucket, k) = match kind { "objects" => (&mut objects, "objects"), "relations" => (&mut relations, "relations"), "relationsoptions" => (&mut options, "relationsOptions"), _ => (&mut file_objs, "filesObjects") };
                if let Some(v) = crate::anytype_pb::to_snapshot(&e.data, k) { bucket.push(v); }
                continue;
            }
            if p.ends_with(".md") { md_count += 1; continue; }
            let bucket = if p.starts_with("objects/") && p.ends_with(".pb.json") { &mut objects }
                else if p.starts_with("relationsoptions/") && p.ends_with(".pb.json") { &mut options }
                else if p.starts_with("filesobjects/") && p.ends_with(".pb.json") { &mut file_objs }
                else if p.starts_with("relations/") && p.ends_with(".pb.json") { &mut relations } else { continue };
            if let Ok(v) = serde_json::from_slice::<Value>(&e.data) { bucket.push(v); }
        }
        let page_snaps: Vec<&Value> = objects.iter().filter(|o| is_page(o) || is_collection_object(o)).collect();
        if page_snaps.is_empty() {
            if pb_count > 0 { bail!("Could not read this Anytype Protobuf export (no readable pages). Export from Anytype as \"JSON\" (Any-Block) instead."); }
            if md_count > 0 { bail!("No Anytype objects found among the Markdown files."); }
        }
        let details_of = |o: &Value| o["snapshot"]["data"]["details"].clone();
        let rels: HashMap<String, Relation> = relations.iter().filter_map(|r| { let d = &r["snapshot"]["data"]["details"]; d["relationKey"].as_str().map(|k| (k.to_string(), Relation { name: s(d, "name").to_string(), format: d["relationFormat"].as_i64().unwrap_or(-1), include_time: d["relationFormatIncludeTime"].as_bool() == Some(true) })) }).collect();
        let opts: HashMap<String, String> = options.iter().filter_map(|o| { let d = &o["snapshot"]["data"]["details"]; d["id"].as_str().map(|i| (i.to_string(), s(d, "name").to_string())) }).collect();
        let fobjs: HashMap<String, FileObj> = file_objs.iter().filter_map(|o| {
            let d = &o["snapshot"]["data"]["details"];
            let id = d["id"].as_str()?;
            let source = s(d, "source").to_string();
            let mut title = base_name(&norm_path(&source)).to_string();
            if title.is_empty() { title = format!("{}{}", d["name"].as_str().unwrap_or("file"), d["fileExt"].as_str().filter(|e| !e.is_empty()).map(|e| format!(".{e}")).unwrap_or_default()); }
            let mime = mime_guess::from_path(&title).first().map(|m| m.to_string()).unwrap_or_else(|| s(d, "fileMimeType").to_string());
            Some((id.to_string(), FileObj { title, mime, source }))
        }).collect();

        // collection-scoped export: every page was created inside one collection that isn't itself in the archive
        let object_ids: HashSet<String> = objects.iter().filter_map(|o| o["snapshot"]["data"]["details"]["id"].as_str().map(String::from)).collect();
        let collection_export = archive_name.is_some() && !page_snaps.is_empty() && {
            let ctx = s(&details_of(page_snaps[0]), "createdInContext").to_string();
            !ctx.is_empty() && !object_ids.contains(&ctx) && page_snaps.iter().all(|p| s(&details_of(p), "createdInContext") == ctx)
        };
        let root_title = if collection_export {
            let b = base_name(&archive_name.unwrap_or("").replace('\\', "/")).to_string();
            let t = b.rsplit_once('.').map_or(b.as_str(), |(a, _)| a).trim().to_string();
            Some(if t.is_empty() { b } else { t })
        } else { None };
        let root_columns = collection_export.then(|| {
            let keys: Vec<String> = page_snaps.iter().flat_map(|p| details_of(p).as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()).collect();
            columns_from_keys(&keys, &rels)
        });

        // every page gets its note id up front so forward links resolve
        let mut targets: HashMap<String, (String, String)> = HashMap::new();
        for p in &page_snaps { let d = details_of(p); if let Some(cid) = d["id"].as_str() { targets.insert(cid.to_string(), (new_entity_id(), page_title(&d))); } }
        let resolve = |cid: &str| targets.get(cid).cloned();

        let parse_object = |snap: &Value| -> Page {
            let data = &snap["snapshot"]["data"];
            let d = &data["details"];
            let id = s(d, "id").to_string();
            let blocks: Vec<Value> = data["blocks"].as_array().cloned().unwrap_or_default();
            let (content, link_ids, inline_files) = extract_content(&blocks, &id, &resolve);
            // custom properties and file relations
            let mut props = vec![];
            let mut file_refs = vec![];
            if let Some(obj) = d.as_object() {
                for (k, raw) in obj {
                    if !is_custom_key(k) { continue; }
                    let Some(info) = rels.get(k) else { continue };
                    if info.format == 5 { for f in raw.as_array().into_iter().flatten() { if let Some(f) = f.as_str() { file_refs.push(f.to_string()); } } continue; }
                    let Some((lt, _, option_backed)) = property_mapping(info) else { continue };
                    let name = to_attribute_name(&info.name);
                    if option_backed {
                        for o in raw.as_array().into_iter().flatten() { if let Some(n) = o.as_str().and_then(|i| opts.get(i)).filter(|n| !n.trim().is_empty()) { props.push((name.clone(), n.clone())); } }
                    } else if let Some(v) = format_property(raw, lt) { props.push((name, v)); }
                }
            }
            let collection = blocks.iter().find_map(|b| b.get("dataview")).filter(|dv| dv["isCollection"].as_bool() == Some(true)).map(|dv| {
                let view = dv["views"].as_array().and_then(|v| v.first());
                let visible: Vec<String> = view.and_then(|v| v["relations"].as_array()).into_iter().flatten().filter(|r| r["isVisible"].as_bool() == Some(true)).filter_map(|r| r["key"].as_str().map(String::from)).collect();
                let group_by = view.and_then(|v| v["groupRelationKey"].as_str()).filter(|k| !k.is_empty()).and_then(|k| rels.get(k)).map(|r| to_attribute_name(&r.name));
                Collection { view_type: view_type(view.map_or("", |v| s(v, "type"))), group_by, members: d["links"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(), columns: columns_from_keys(&visible, &rels) }
            });
            Page { id, title: page_title(d), content, link_ids, created: utc_from_secs(&d["createdDate"]), modified: utc_from_secs(&d["lastModifiedDate"]), props, file_refs, inline_files, collection }
        };
        let pages: Vec<Page> = page_snaps.iter().map(|p| parse_object(p)).collect();
        let excluded_file_refs: HashSet<String> = if collection_export {
            objects.iter().filter(|o| !is_page(o) && !is_collection_object(o)).flat_map(|o| { let p = parse_object(o); p.file_refs.into_iter().chain(p.inline_files).collect::<Vec<_>>() }).collect()
        } else { HashSet::new() };

        // ---- create notes
        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let title = root_title.unwrap_or_else(|| "Anytype import".into());
        let root = if root_columns.is_some() { self.create_note(parent_id, &title, "book", "", "")? } else { self.create_note(parent_id, &title, "text", "text/html", "")? };
        stats.notes += 1;
        stats.root = Some(root.clone());
        if let Some(cols) = &root_columns { self.apply_collection_view(&root, "table", cols, None)?; }
        self.add_attribute(&root, "label", "iconClass", "bx bx-import", false)?;

        let collection_page_ids: HashSet<&String> = pages.iter().filter(|p| p.collection.is_some()).map(|p| &p.id).collect();
        let mut primary: HashMap<String, String> = HashMap::new();
        for p in &pages { for m in p.collection.iter().flat_map(|c| &c.members) { if !collection_page_ids.contains(m) { primary.entry(m.clone()).or_insert_with(|| p.id.clone()); } } }

        let mut notes: HashMap<String, String> = HashMap::new(); // anytype id → note id
        let mut order: Vec<&Page> = pages.iter().collect();
        order.sort_by_key(|p| if p.collection.is_some() { 0 } else { 1 });
        for page in order {
            let note_id = targets.get(&page.id).map(|t| t.0.clone()).unwrap_or_else(new_entity_id);
            let parent = primary.get(&page.id).and_then(|c| notes.get(c)).cloned().unwrap_or_else(|| root.clone());
            match &page.collection {
                Some(c) => {
                    self.insert_note_with_id(&note_id, &parent, &page.title, "book", "", "", None, None)?;
                    self.apply_collection_view(&note_id, c.view_type, &c.columns, c.group_by.as_deref())?;
                }
                None => { self.insert_note_with_id(&note_id, &parent, &page.title, "text", "text/html", "", None, None)?; }
            }
            stats.notes += 1;
            for (n, v) in &page.props { self.add_attribute(&note_id, "label", n, v, false)?; stats.attributes += 1; }
            // file-property files first (reference links prepended), then the body's inline files
            let mut file_links = String::new();
            for fid in &page.file_refs {
                let Some(info) = fobjs.get(fid) else { continue };
                let Some(bytes) = files.get(&norm_path(&info.source)) else { continue };
                let aid = self.add_attachment(&note_id, &info.title, &mime_or_octet(&info.mime), "file", bytes)?;
                stats.attachments += 1;
                file_links.push_str(&format!("<p><a class=\"reference-link\" href=\"#root/{note_id}?viewMode=attachments&amp;attachmentId={aid}\">{}</a></p>", escape_text(&info.title)));
            }
            let body = format!("{file_links}{}", if page.collection.is_some() { "" } else { page.content.as_str() });
            let body = self.apply_inline_files(&note_id, &body, &fobjs, &files, &mut stats)?;
            let dates = match (page.created.as_deref(), page.modified.as_deref()) {
                (None, None) => None,
                (c, m) => { let c = c.or(m).unwrap_or_default().to_string(); let m = m.map(String::from).unwrap_or_else(|| c.clone()); Some((c, m)) }
            };
            self.store_imported_html(&note_id, &body, dates.as_ref().map(|(c, m)| (c.as_str(), m.as_str())))?;
            notes.insert(page.id.clone(), note_id);
        }

        // files dropped straight into a collection
        let mut file_members: Vec<String> = vec![];
        for p in &pages { for m in p.collection.iter().flat_map(|c| &c.members) { if !notes.contains_key(m) && fobjs.contains_key(m) && !file_members.contains(m) { file_members.push(m.clone()); } } }
        for fid in file_members {
            let (Some(info), true) = (fobjs.get(&fid), true) else { continue };
            let Some(bytes) = files.get(&norm_path(&info.source)) else { continue };
            let parent = primary.get(&fid).and_then(|c| notes.get(c)).cloned().unwrap_or_else(|| root.clone());
            let id = self.file_member_note(&parent, info, bytes)?;
            stats.notes += 1;
            notes.insert(fid, id);
        }
        if collection_export {
            let mut referenced: HashSet<String> = excluded_file_refs;
            for p in &pages { referenced.extend(p.file_refs.iter().cloned()); referenced.extend(p.inline_files.iter().cloned()); }
            let mut ids: Vec<&String> = fobjs.keys().collect();
            ids.sort();
            for fid in ids {
                if notes.contains_key(fid) || referenced.contains(fid) { continue; }
                let info = &fobjs[fid];
                if let Some(bytes) = files.get(&norm_path(&info.source)) { let id = self.file_member_note(&root, info, bytes)?; stats.notes += 1; notes.insert(fid.clone(), id); }
            }
        }
        // membership: clone into every collection that isn't the primary parent
        for p in &pages {
            let Some(coll_note) = p.collection.as_ref().and_then(|_| notes.get(&p.id)) else { continue };
            for m in p.collection.iter().flat_map(|c| &c.members) {
                if primary.get(m) == Some(&p.id) { continue; }
                if let Some(member) = notes.get(m) { self.clone_note(member, coll_note, None)?; stats.clones += 1; }
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    fn file_member_note(&self, parent: &str, info: &FileObj, bytes: &[u8]) -> Result<String> {
        let mime = mime_or_octet(&info.mime);
        let id = self.create_file_note(parent, &info.title, &mime, bytes)?;
        if !mime.starts_with("image/") { self.add_attribute(&id, "label", "originalFileName", &info.title, false)?; }
        Ok(id)
    }

    fn apply_collection_view(&self, note: &str, view: &str, columns: &[Column], group_by: Option<&str>) -> Result<()> {
        self.add_attribute(note, "label", "viewType", view, false)?;
        match (group_by, view) {
            (Some(g), "board") => { self.add_attribute(note, "label", "board:groupBy", g, false)?; }
            (Some(g), "calendar") => { self.add_attribute(note, "label", "calendar:startDate", g, true)?; }
            _ => {}
        }
        for c in columns {
            self.add_attribute(note, "label", &format!("label:{}", c.name), &promoted_definition(&c.alias, Some(c.label_type), c.multi), true)?;
        }
        Ok(())
    }

    /// Resolve the file placeholders a page body carries into attachments of `note_id`.
    fn apply_inline_files(&self, note_id: &str, html: &str, fobjs: &HashMap<String, FileObj>, files: &HashMap<String, Vec<u8>>, stats: &mut ImportStats) -> Result<String> {
        if !html.contains("<img") && !html.contains("anytype-file") && !html.contains("link-embed") { return Ok(html.to_string()); }
        let bytes_of = |id: &str| fobjs.get(id).and_then(|i| files.get(&norm_path(&i.source)));
        let mut err: Option<anyhow::Error> = None;
        let mut saved = 0usize;
        let out = map_els(parse(html), &mut |mut e: El| -> Vec<Node> {
            match e.name.as_str() {
                "section" if e.has_class("link-embed") => {
                    let url = e.attr_dec("data-url").unwrap_or_default();
                    for (attr, role, suffix) in [("data-favicon", "favicon", ""), ("data-image", "coverImage", "-preview")] {
                        let Some(tid) = e.attr_dec(attr).filter(|t| !t.is_empty()) else { continue };
                        let pic = bytes_of(&tid).and_then(|b| sniff_image(b).map(|(m, x)| (b, m, x)));
                        match pic {
                            Some((b, mime, ext)) => {
                                let name = format!("{}{suffix}.{ext}", safe_host(&url));
                                match self.add_attachment(note_id, &name, mime, role, b) {
                                    Ok(aid) => { saved += 1; e.set_attr(attr, &format!("api/attachments/{aid}/image/{name}")); }
                                    Err(x) => { err.get_or_insert(x); e.remove_attr(attr); }
                                }
                            }
                            None => e.remove_attr(attr),
                        }
                    }
                    vec![Node::El(e)]
                }
                "figure" if e.child_els().any(|c| c.name == "img") => {
                    // an image whose bytes aren't in the export is dropped together with its figure
                    let img = e.child_els().find(|c| c.name == "img").cloned().unwrap();
                    let tid = img.attr_dec("src").unwrap_or_default();
                    if fobjs.contains_key(&tid) || !tid.is_empty() && bytes_of(&tid).is_some() {
                        match bytes_of(&tid) {
                            Some(b) => {
                                let info = &fobjs[&tid];
                                let title = if info.title.is_empty() { "image".to_string() } else { info.title.clone() };
                                let mime = mime_guess::from_path(&title).first().map(|m| m.to_string()).filter(|m| m.starts_with("image/")).unwrap_or_else(|| "image/png".into());
                                match self.add_attachment(note_id, &title, &mime, "image", b) {
                                    Ok(aid) => { saved += 1; let mut im = img; im.set_attr("src", &format!("api/attachments/{aid}/image/{}", enc(&title))); e.kids = vec![Node::El(im)]; return vec![Node::El(e)]; }
                                    Err(x) => { err.get_or_insert(x); return vec![]; }
                                }
                            }
                            None => return vec![],
                        }
                    }
                    vec![]
                }
                "a" if e.has_class("anytype-file") => {
                    e.remove_attr("class");
                    let tid = e.attr_dec("href").unwrap_or_default();
                    match (fobjs.get(&tid), bytes_of(&tid)) {
                        (Some(info), Some(b)) => match self.add_attachment(note_id, &info.title, &mime_or_octet(&info.mime), "file", b) {
                            Ok(aid) => { saved += 1; e.set_attr("href", &format!("#root/{note_id}?viewMode=attachments&attachmentId={aid}")); e.set_attr("class", "reference-link"); }
                            Err(x) => { err.get_or_insert(x); e.remove_attr("href"); }
                        },
                        _ => e.remove_attr("href"),
                    }
                    vec![Node::El(e)]
                }
                _ => vec![Node::El(e)],
            }
        });
        stats.attachments += saved;
        match err { Some(e) => Err(e), None => Ok(serialize(&out)) }
    }
}

fn mime_or_octet(m: &str) -> String { if m.is_empty() { "application/octet-stream".into() } else { m.to_string() } }

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() { if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); } }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("a.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn e(path: &str, v: &Value) -> Entry { Entry { path: path.into(), data: serde_json::to_vec(v).unwrap(), is_dir: false } }

    fn text(id: &str, style: &str, t: &str, marks: Value, kids: &[&str]) -> Value {
        json!({"id": id, "childrenIds": kids, "text": {"text": t, "style": style, "marks": {"marks": marks}}})
    }

    #[test]
    fn inline_marks_formulas_and_utf16_offsets() {
        let r = |_: &str| None;
        let none = &mut |_: &str| {};
        let marks = vec![json!({"range":{"from":0,"to":5},"type":"Bold"}), json!({"range":{"from":3,"to":8},"type":"Italic"}), json!({"range":{"from":0,"to":2},"type":"TextColor","param":"red"}), json!({"range":{"from":2,"to":4},"type":"BackgroundColor","param":"blue"})];
        assert_eq!(render_inline("abcdefgh <&>", &marks, &r, none),
            "<strong><span style=\"color:#e2400c\">ab</span></strong><strong><span style=\"color:#252525;background-color:#cbd2fa\">c</span></strong><strong><em><span style=\"color:#252525;background-color:#cbd2fa\">d</span></em></strong><strong><em>e</em></strong><em>fgh</em> &lt;&amp;&gt;");
        // offsets count UTF-16 units: the emoji is two
        let m = vec![json!({"range":{"from":3,"to":4},"type":"Bold"})];
        assert_eq!(render_inline("a😀b", &m, &r, none), "a😀<strong>b</strong>");
        assert_eq!(render_inline("cost $5 and $6, math $x^2$ and $$y<z$$", &[], &r, none), "cost $5 and $6, math <span class=\"math-tex\">\\( x^2 \\)</span> and <span class=\"math-tex\">\\[ y&lt;z \\]</span>");
        let mention = vec![json!({"range":{"from":0,"to":4},"type":"Mention","param":"cidB"})];
        let resolve = |c: &str| (c == "cidB").then(|| ("NOTEB".to_string(), "B".to_string()));
        let mut seen = vec![];
        assert_eq!(render_inline("Page x", &mention, &resolve, &mut |id| seen.push(id.to_string())), "<a class=\"reference-link\" href=\"#root/NOTEB\">Page</a> x");
        assert_eq!(seen, ["NOTEB"]);
    }

    #[test]
    fn export_with_pages_collections_properties_files_and_links() {
        let (_d, db) = fixture();
        let mut ticked = text("cb", "Checkbox", "todo", json!([]), &[]);
        ticked["text"]["checked"] = json!(true);
        let mut code = text("code", "Code", "let x = 1 < 2;", json!([]), &[]);
        code["fields"] = json!({"lang": "rust"});
        let mut call = text("call", "Callout", "careful", json!([]), &[]);
        call["text"]["iconEmoji"] = json!("⚠️");
        let blocks = vec![
            json!({"id":"cidA","childrenIds":["h","p","b1","b2","n1","cb","tg","code","div","lk","img","doc","tbl","bm","ltx","quote","call","empty"]}),
            json!({"id":"header","childrenIds":["title"]}),
            text("h", "Header1", "Heading", json!([]), &[]),
            text("p", "Paragraph", "Hello **no** bold", json!([{"range":{"from":0,"to":5},"type":"Bold"}]), &[]),
            text("b1", "Marked", "one", json!([]), &["b1c"]), text("b1c", "Marked", "child", json!([]), &[]), text("b2", "Marked", "two", json!([]), &[]),
            text("n1", "Numbered", "first", json!([]), &[]),
            ticked,
            text("tg", "Toggle", "Toggle title", json!([]), &["tgc"]), text("tgc", "Paragraph", "hidden", json!([]), &[]),
            code,
            json!({"id":"div","div":{"style":"Line"}}),
            json!({"id":"lk","link":{"targetBlockId":"cidB","style":"Page"}}),
            json!({"id":"img","file":{"targetObjectId":"fileImg","type":"Image","name":"pic.png","state":"Done"}}),
            json!({"id":"doc","file":{"targetObjectId":"fileDoc","type":"PDF","name":"doc.pdf"}}),
            json!({"id":"tbl","table":{},"childrenIds":["cols","rows"]}), json!({"id":"cols","childrenIds":["c1","c2"]}), json!({"id":"rows","childrenIds":["r1","r2"]}),
            json!({"id":"r1","tableRow":{"isHeader":true},"childrenIds":["r1-c1","r1-c2"]}), json!({"id":"r2","tableRow":{},"childrenIds":["r2-c1"]}),
            text("r1-c1", "Paragraph", "H1", json!([]), &[]), text("r1-c2", "Paragraph", "H2", json!([]), &[]), text("r2-c1", "Paragraph", "cell", json!([]), &[]),
            json!({"id":"bm","bookmark":{"url":"https://example.org/x","title":"Ex","description":"D","faviconHash":"fileFav"}}),
            json!({"id":"ltx","latex":{"text":"graph TD","processor":"Mermaid"}}),
            text("quote", "Quote", "quoted", json!([]), &[]),
            call,
            json!({"id":"missing","file":{"type":"Image"}}),
            json!({"id":"empty","file":{"targetObjectId":"gone","type":"Image"}}),
        ];
        let page_a = json!({"sbType":"Page","snapshot":{"data":{"details":{"id":"cidA","name":"Alpha","createdDate":1700000000,"lastModifiedDate":1700000500,
            "6a0b1c2d3e4f5a6b7c8d9e0f":["Open"],"6a0b1c2d3e4f5a6b7c8d9e10":3,"6a0b1c2d3e4f5a6b7c8d9e11":["opt1","opt2"],"6a0b1c2d3e4f5a6b7c8d9e12":["fileDoc"],"name_not_custom":"x"},
            "blocks": blocks}}});
        let page_b = json!({"sbType":"Page","snapshot":{"data":{"details":{"id":"cidB","name":"Beta"},"blocks":[{"id":"cidB","childrenIds":["m"]}, text("m","Paragraph","see Alpha",json!([{"range":{"from":4,"to":9},"type":"Mention","param":"cidA"}]),&[])]}}});
        let coll = json!({"sbType":"Page","snapshot":{"data":{"details":{"id":"cidC","name":"Tasks","layout":14,"links":["cidB","fileMember"]},"blocks":[{"id":"cidC","childrenIds":["dv"]},
            {"id":"dv","dataview":{"isCollection":true,"views":[{"type":"Kanban","groupRelationKey":"6a0b1c2d3e4f5a6b7c8d9e0f","relations":[{"key":"6a0b1c2d3e4f5a6b7c8d9e0f","isVisible":true},{"key":"6a0b1c2d3e4f5a6b7c8d9e10","isVisible":true},{"key":"6a0b1c2d3e4f5a6b7c8d9eff","isVisible":true}]}]}}]}}});
        let rel = |key: &str, name: &str, fmt: i64| json!({"sbType":"STRelation","snapshot":{"data":{"details":{"relationKey":key,"name":name,"relationFormat":fmt}}}});
        let opt = |id: &str, name: &str| json!({"snapshot":{"data":{"details":{"id":id,"name":name}}}});
        let fo = |id: &str, src: &str| json!({"snapshot":{"data":{"details":{"id":id,"source":src,"name":"n","fileMimeType":"text/plain"}}}});
        let entries = vec![
            e("objects/a.pb.json", &page_a), e("objects/b.pb.json", &page_b), e("objects/c.pb.json", &coll),
            e("relations/s.pb.json", &rel("6a0b1c2d3e4f5a6b7c8d9e0f", "Status", 3)), e("relations/n.pb.json", &rel("6a0b1c2d3e4f5a6b7c8d9e10", "Story points", 2)),
            e("relations/t.pb.json", &rel("6a0b1c2d3e4f5a6b7c8d9e11", "Labels", 11)), e("relations/f.pb.json", &rel("6a0b1c2d3e4f5a6b7c8d9e12", "Docs", 5)),
            e("relationsOptions/o1.pb.json", &opt("opt1", "red")), e("relationsOptions/o2.pb.json", &opt("opt2", "blue")), e("relationsOptions/o3.pb.json", &opt("Open", "Open")),
            e("filesObjects/f1.pb.json", &fo("fileImg", "files\\pic.png")), e("filesObjects/f2.pb.json", &fo("fileDoc", "files/doc.pdf")), e("filesObjects/f3.pb.json", &fo("fileFav", "files/fav.png")), e("filesObjects/f4.pb.json", &fo("fileMember", "files/member.txt")),
            Entry { path: "files/pic.png".into(), data: b"\x89PNGpic".to_vec(), is_dir: false }, Entry { path: "files/doc.pdf".into(), data: b"%PDF".to_vec(), is_dir: false },
            Entry { path: "files/fav.png".into(), data: b"\x89PNGfav".to_vec(), is_dir: false }, Entry { path: "files/member.txt".into(), data: b"member".to_vec(), is_dir: false },
        ];
        assert!(looks_like_export(&entries));
        let st = db.import_anytype(entries, "root", None).unwrap();
        let root = st.root.unwrap();
        let kid = |p: &str, t: &str| -> String { db.conn.query_row("SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND n.title=? AND b.isDeleted=0", [p, t], |r| r.get(0)).unwrap_or_else(|_| panic!("no {t} under {p}")) };
        let alpha = kid(&root, "Alpha");
        let tasks = kid(&root, "Tasks");
        let beta = kid(&tasks, "Beta");
        let html = db.note_content(&alpha).unwrap().unwrap();
        for want in ["<h2>Heading</h2>", "<p><strong>Hello</strong> **no** bold</p>", "<ul><li>one<ul><li>child</li></ul></li><li>two</li></ul>", "<ol><li>first</li></ol>",
            "<ul class=\"todo-list\"><li><label class=\"todo-list__label\"><input type=\"checkbox\" checked=\"checked\" disabled=\"disabled\">", "<details class=\"trilium-collapsible\"><summary>Toggle title</summary><p>hidden</p></details>",
            "<pre><code class=\"language-text-x-rustsrc\">let x = 1 &lt; 2;</code></pre>", "<hr>", &format!("<a class=\"reference-link\" href=\"#root/{beta}\">Beta</a>"),
            "<figure class=\"table\"><table><thead><tr><th scope=\"col\">H1</th><th scope=\"col\">H2</th></tr></thead><tbody><tr><td>cell</td><td></td></tr></tbody></table></figure>",
            "<section class=\"link-embed\" data-url=\"https://example.org/x\" data-embed-type=\"opengraph\" data-title=\"Ex\" data-description=\"D\" data-favicon=\"api/attachments/", "<pre><code class=\"language-mermaid\">graph TD</code></pre>",
            "<blockquote><p>quoted</p></blockquote>", "<aside class=\"admonition note\"><p>⚠️ careful</p></aside>", "<figure class=\"image\"><img src=\"api/attachments/", ">doc.pdf</a>"] {
            assert!(html.contains(want), "missing {want}\n{html}");
        }
        assert!(!html.contains("gone") && !html.contains("anytype-file"), "unresolvable image dropped, no placeholders left: {html}");
        // properties
        assert_eq!(db.label_value(&alpha, "status").unwrap().as_deref(), Some("Open"));
        assert_eq!(db.label_value(&alpha, "storyPoints").unwrap().as_deref(), Some("3"));
        let labels: Vec<String> = db.conn.prepare("SELECT value FROM attributes WHERE noteId=? AND name='labels' AND isDeleted=0 ORDER BY position").unwrap().query_map([&alpha], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(labels, ["red", "blue"]);
        assert!(db.label_value(&alpha, "name_not_custom").unwrap().is_none());
        // attachments: image, file property + inline file, favicon
        let atts: Vec<(String, String)> = db.attachments(&alpha).unwrap().into_iter().map(|a| (a.title, a.role)).collect();
        assert!(atts.contains(&("pic.png".into(), "image".into())) && atts.iter().filter(|a| a.0 == "doc.pdf").count() == 2 && atts.contains(&("example.org.png".into(), "favicon".into())), "{atts:?}");
        // dates
        let (c, m): (String, String) = db.conn.query_row("SELECT utcDateCreated,utcDateModified FROM notes WHERE noteId=?", [&alpha], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((c.as_str(), m.as_str()), ("2023-11-14 22:13:20.000Z", "2023-11-14 22:21:40.000Z"));
        // the mention in Beta links back, the relation exists
        assert!(db.note_content(&beta).unwrap().unwrap().contains(&format!("<a class=\"reference-link\" href=\"#root/{alpha}\">Alpha</a>")));
        assert!(db.backlinks(&alpha).unwrap().iter().any(|b| b.source == beta));
        // collection: board view grouped by Status, promoted columns (visible & supported only), members under it
        let ty: String = db.conn.query_row("SELECT type FROM notes WHERE noteId=?", [&tasks], |r| r.get(0)).unwrap();
        assert_eq!(ty, "book");
        assert_eq!(db.label_value(&tasks, "viewType").unwrap().as_deref(), Some("board"));
        assert_eq!(db.label_value(&tasks, "board:groupBy").unwrap().as_deref(), Some("status"));
        assert_eq!(db.label_value(&tasks, "label:status").unwrap().as_deref(), Some("promoted,single,text,alias=Status"));
        assert_eq!(db.label_value(&tasks, "label:storyPoints").unwrap().as_deref(), Some("promoted,single,number,alias=Story points"));
        assert!(db.label_value(&tasks, "label:labels").unwrap().is_none());
        let members: Vec<String> = db.conn.prepare("SELECT n.title FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 ORDER BY b.notePosition").unwrap().query_map([&tasks], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(members, ["Beta", "member.txt"]);
        assert_eq!(db.conn.query_row("SELECT type FROM notes WHERE title='member.txt'", [], |r| r.get::<_, String>(0)).unwrap(), "file");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn wrong_export_shapes_and_collection_scoped_exports() {
        let (_d, db) = fixture();
        assert!(db.import_anytype(vec![Entry { path: "objects/x.pb".into(), data: vec![1], is_dir: false }], "root", None).unwrap_err().to_string().contains("Protobuf"));
        // the Markdown export is a plain folder of Markdown files
        let md = db.import_anytype(vec![Entry { path: "Page.md".into(), data: b"# Page\n\nhello".to_vec(), is_dir: false }], "root", Some("Space export.zip")).unwrap();
        assert_eq!(db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [md.root.as_ref().unwrap()], |r| r.get::<_, String>(0)).unwrap(), "Space export");
        let mk = |id: &str, name: &str| json!({"sbType":"Page","snapshot":{"data":{"details":{"id":id,"name":name,"createdInContext":"absentColl","6a0b1c2d3e4f5a6b7c8d9e0f":"v"},"blocks":[{"id":id,"childrenIds":[]}]}}});
        let rel = json!({"snapshot":{"data":{"details":{"relationKey":"6a0b1c2d3e4f5a6b7c8d9e0f","name":"Note","relationFormat":1}}}});
        let st = db.import_anytype(vec![e("objects/a.pb.json", &mk("a", "A")), e("objects/b.pb.json", &mk("b", "B")), e("relations/r.pb.json", &rel)], "root", Some("exports/My Tasks.zip")).unwrap();
        let root = st.root.unwrap();
        let (t, ty): (String, String) = db.conn.query_row("SELECT title,type FROM notes WHERE noteId=?", [&root], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t.as_str(), ty.as_str()), ("My Tasks", "book"));
        assert_eq!(db.label_value(&root, "label:note").unwrap().as_deref(), Some("promoted,single,text,alias=Note"));
        // hostile input never panics
        let _ = db.import_anytype(vec![e("objects/z.pb.json", &json!({"sbType":"Page","snapshot":{"data":{"details":{"id":"z","name":"Z"},"blocks":[{"id":"z","childrenIds":["z","q"]},{"id":"q","childrenIds":["q"],"text":{"text":"x😀y$$","marks":{"marks":[{"range":{"from":-5,"to":999},"type":"Bold"},{"range":{"from":2,"to":1}}]}}}]}}}))], "root", None).unwrap();
    }
}
