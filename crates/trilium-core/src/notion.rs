//! Notion HTML-export import, ported from Trilium's `import/notion/`. A Notion export names each page
//! `Title <32-hex id>.html` and keeps its subpages in a sibling `Title/` folder; the hierarchy, page bodies,
//! images and files, cross-page links, timestamps and database properties are rebuilt as Trilium notes.
//! A database becomes a table collection (`book` note, `#viewType=table`) whose rows are its children and whose
//! columns are inheritable promoted-attribute definitions.
use crate::{
    db::Db,
    formats::{promoted_definition, to_attribute_name},
    htmltree::{El, Node, decode_entities, escape_attr, escape_text, find_all_els, find_el, map_els, map_els_pre, parse, sanitize_html, serialize, text_content},
    transfer::{Entry, ImportStats, hex_pair},
};
use anyhow::{Result, bail};
use chrono::TimeZone;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

fn rx(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex { cell.get_or_init(|| Regex::new(pat).expect("static regex")) }

// ------------------------------------------------------------------ ids and paths

pub fn strip_notion_id(s: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"[ -]?[a-z0-9]{32}(\.|$)").replace(&s.replace('-', ""), "$1").into_owned()
}

pub fn get_notion_id(s: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"([a-z0-9]{32})(\?|\.|$)").captures(&s.replace('-', "")).map(|c| c[1].to_string())
}

fn base_name(p: &str) -> &str { p.rsplit('/').next().unwrap_or(p) }
fn dir_name(p: &str) -> &str { p.rfind('/').map_or("", |i| &p[..i]) }
fn remove_ext(n: &str) -> String { static R: OnceLock<Regex> = OnceLock::new(); rx(&R, r"\.[^.]+$").replace(n, "").into_owned() }

fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg { "" | "." => {} ".." => { parts.pop(); } s => parts.push(s) }
    }
    parts.join("/")
}

fn folder_key(folder: &str) -> String { folder.split('/').map(strip_notion_id).collect::<Vec<_>>().join("/") }

/// The folder a page's children live in: its directory plus its own (id-less) title.
fn owned_folder_key(path: &str) -> String {
    let title = strip_notion_id(&remove_ext(base_name(path)));
    let dir = dir_name(path);
    folder_key(&if dir.is_empty() { title } else { format!("{dir}/{title}") })
}

fn parent_folder_key(path: &str) -> String { folder_key(dir_name(path)) }

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = hex_pair(b[i + 1], b[i + 2]) { out.push(v); i += 3; continue; }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn resolve_resource_path(page_path: &str, src: &str) -> String {
    let d = percent_decode(src);
    let base = dir_name(page_path);
    normalize_path(&if base.is_empty() { d } else { format!("{base}/{d}") })
}

/// The Notion id of an internal `.html` link, if it is one.
fn internal_page_id(href: &str) -> Option<String> {
    if href.is_empty() { return None; }
    let d = percent_decode(href);
    let path = d.split(['?', '#']).next().unwrap_or("");
    if !path.to_lowercase().ends_with(".html") && !path.to_lowercase().ends_with(".md") { return None; }
    get_notion_id(path)
}

// ------------------------------------------------------------------ model

#[derive(Clone, Debug)]
struct Prop { name: String, value: String, label_type: Option<String>, multi: bool, kind: Option<&'static str> }

#[derive(Clone, Debug, Default)]
struct Page {
    id: String,
    title: String,
    path: String,
    content: String,
    linked_ids: Vec<String>,
    props: Vec<Prop>,
    is_database: bool,
    created: Option<String>,
    modified: Option<String>,
}

fn classes(e: &El) -> Vec<String> { e.attr_dec("class").unwrap_or_default().split_whitespace().map(String::from).collect() }
fn style_of(e: &El) -> String { e.attr_dec("style").unwrap_or_default() }
fn direct<'a>(e: &'a El, tag: &str) -> Vec<&'a El> { e.child_els().filter(|c| c.name == tag).collect() }
fn is_display_contents(e: &El) -> bool { e.name == "div" && style_of(e).replace(char::is_whitespace, "").contains("display:contents") }
fn inner(e: &El) -> String { e.inner_html() }
fn text_of(e: &El) -> String { e.text() }

// ------------------------------------------------------------------ HTML conversion

fn extract_latex(e: &El) -> Option<String> {
    let mut anns = vec![];
    find_all_els(&e.kids, &|x| x.name == "annotation", &mut anns);
    let a = anns.iter().find(|a| a.attr_dec("encoding").as_deref() == Some("application/x-tex")).or(anns.first())?;
    Some(a.text().trim().to_string())
}

fn convert_math(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "style" { return vec![]; }
        if e.name == "span" && e.has_class("notion-text-equation-token") {
            if let Some(l) = extract_latex(&e) { return parse(&format!("<span class=\"math-tex\">\\({}\\)</span>", escape_text(&l))); }
        }
        if e.name == "figure" && e.has_class("equation") {
            if let Some(l) = extract_latex(&e) { return parse(&format!("<figure><span class=\"math-tex\">\\[{}\\]</span></figure>", escape_text(&l))); }
        }
        vec![Node::El(e)]
    })
}

fn strip_date_prefixes(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |mut e: El| {
        if e.name == "time" && text_of(&e).contains('@') { e.kids = vec![Node::Text(escape_text(&text_of(&e).replace('@', "")))]; }
        vec![Node::El(e)]
    })
}

fn todo_ul(e: &El) -> Option<&El> {
    if e.name == "ul" && e.has_class("to-do-list") { return Some(e); }
    if is_display_contents(e) {
        let els: Vec<&El> = e.child_els().collect();
        if els.len() == 1 && els[0].name == "ul" && els[0].has_class("to-do-list") { return Some(els[0]); }
    }
    None
}

fn build_todo_item(li: &El) -> String {
    let checked = li.child_els().find(|c| c.has_class("checkbox")).is_some_and(|c| c.has_class("checkbox-on"));
    let label = li.child_els().find(|c| c.name == "span" && (c.has_class("to-do-children-checked") || c.has_class("to-do-children-unchecked")));
    let desc = label.map(inner).unwrap_or_default();
    let nested = li.child_els().find(|c| c.has_class("indented")).map(|ind| serialize(&todo_pass(ind.kids.clone())).trim().to_string()).unwrap_or_default();
    let attrs = if checked { " checked=\"checked\" disabled=\"disabled\"" } else { " disabled=\"disabled\"" };
    format!("<li><label class=\"todo-list__label\"><input type=\"checkbox\"{attrs}><span class=\"todo-list__label__description\">{desc}</span></label>{nested}</li>")
}

fn todo_pass(nodes: Vec<Node>) -> Vec<Node> {
    // group maximal runs of adjacent to-do blocks (whitespace between them doesn't break a run)
    let mut out: Vec<Node> = vec![];
    let mut run: Vec<String> = vec![];
    let mut pending_ws: Vec<Node> = vec![];
    let flush = |run: &mut Vec<String>, out: &mut Vec<Node>| {
        if run.is_empty() { return; }
        out.extend(parse(&format!("<ul class=\"todo-list\">{}</ul>", run.concat())));
        run.clear();
    };
    for n in nodes {
        match &n {
            Node::El(e) if todo_ul(e).is_some() => {
                pending_ws.clear();
                for li in direct(todo_ul(e).unwrap(), "li") { run.push(build_todo_item(li)); }
            }
            Node::Text(t) if t.trim().is_empty() && !run.is_empty() => pending_ws.push(n),
            _ => {
                flush(&mut run, &mut out);
                out.append(&mut pending_ws);
                out.push(n);
            }
        }
    }
    flush(&mut run, &mut out);
    out.append(&mut pending_ws);
    // to-do lists may sit inside other blocks too
    out.into_iter().map(|n| match n {
        Node::El(mut e) if !(e.name == "ul" && e.has_class("todo-list")) => { e.kids = todo_pass(std::mem::take(&mut e.kids)); Node::El(e) }
        other => other,
    }).collect()
}

fn convert_toggles(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "ul" && e.has_class("toggle") {
            let mut out = vec![];
            for li in direct(&e, "li") {
                for d in direct(li, "details") {
                    let mut d = d.clone();
                    let cls = d.attr_dec("class").unwrap_or_default();
                    d.set_attr("class", &if cls.is_empty() { "trilium-collapsible".to_string() } else { format!("{cls} trilium-collapsible") });
                    out.push(Node::El(d));
                }
            }
            return out;
        }
        vec![Node::El(e)]
    })
}

fn toggle_heading_tag(summary: &El) -> Option<&'static str> {
    static R: OnceLock<Regex> = OnceLock::new();
    let size = rx(&R, r"font-size:\s*([\d.]+em)").captures(&style_of(summary))?[1].to_string();
    Some(match size.as_str() { "1.875em" => "h2", "1.5em" => "h3", "1.25em" => "h4", "1.125em" => "h5", _ => return None })
}

fn convert_toggle_headings(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "details" {
            if let Some((tag, sum)) = direct(&e, "summary").first().and_then(|s| toggle_heading_tag(s).map(|t| (t, inner(s)))) {
                let mut out = parse(&format!("<{tag}>{}</{tag}>", sum.trim()));
                for k in &e.kids {
                    match k {
                        Node::El(c) if c.name == "summary" => {}
                        Node::El(c) if c.has_class("indented") => out.extend(c.kids.clone()),
                        other => out.push(other.clone()),
                    }
                }
                return out;
            }
        }
        vec![Node::El(e)]
    })
}

fn drop_toc_and_unwrap(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "nav" && e.has_class("table_of_contents") { return vec![]; }
        if is_display_contents(&e) { return e.kids; }
        vec![Node::El(e)]
    })
}

fn convert_inline_databases(nodes: Vec<Node>) -> Vec<Node> {
    map_els_pre(nodes, &mut |e: El| {
        if e.name == "div" && e.has_class("collection-content") {
            if let Some(id) = get_notion_id(&e.attr_dec("id").unwrap_or_default()) {
                return Ok(parse(&format!("<section class=\"include-note\" data-notion-id=\"{id}\" data-box-size=\"medium\">&nbsp;</section>")));
            }
        }
        Err(e)
    })
}

fn mergeable(e: &El) -> bool { (e.name == "ul" && e.has_class("bulleted-list")) || (e.name == "ol" && e.has_class("numbered-list")) }

fn merge_lists(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    for n in nodes {
        if let Node::El(e) = &n {
            if mergeable(e) {
                if let Some(Node::El(prev)) = out.iter_mut().rev().find(|x| matches!(x, Node::El(_))) {
                    if prev.name == e.name && mergeable(prev) { prev.kids.extend(e.kids.clone()); continue; }
                }
            }
        }
        out.push(n);
    }
    out.into_iter().map(|n| match n {
        Node::El(mut e) => {
            e.kids = merge_lists(std::mem::take(&mut e.kids));
            if mergeable(&e) { for a in ["class", "start", "type", "id"] { e.remove_attr(a); } }
            Node::El(e)
        }
        other => other,
    }).collect()
}

fn column_width(c: &El) -> Option<f64> {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"width:\s*([\d.]+)%").captures(&style_of(c))?[1].parse().ok()
}

fn round2(v: f64) -> f64 { (v * 100.0).round() / 100.0 }

fn pure_wrapper_list(col: &El) -> Option<&El> {
    let meaningful: Vec<&El> = col.child_els().filter(|c| !(c.name == "p" && text_of(c).trim().is_empty())).collect();
    (meaningful.len() == 1 && meaningful[0].has_class("column-list")).then(|| meaningful[0])
}

fn cell_content(col: &El) -> String {
    let els: Vec<&El> = col.child_els().collect();
    if els.len() == 1 && els[0].name == "p" { return inner(els[0]); }
    col.kids.iter().map(|n| match n {
        Node::El(e) if e.has_class("column-list") => column_list_figure(e),
        other => serialize(std::slice::from_ref(other)),
    }).collect()
}

fn flatten_columns(list: &El, parent_width: f64) -> Vec<(f64, String)> {
    let cols: Vec<&El> = direct(list, "div").into_iter().filter(|c| c.has_class("column")).collect();
    let widths: Vec<f64> = cols.iter().map(|c| column_width(c).unwrap_or(100.0 / cols.len() as f64)).collect();
    let total = widths.iter().sum::<f64>().max(f64::MIN_POSITIVE);
    let mut leaves = vec![];
    for (i, col) in cols.iter().enumerate() {
        let share = parent_width * (widths[i] / total);
        match pure_wrapper_list(col) {
            Some(w) => leaves.extend(flatten_columns(w, share)),
            None => leaves.push((share, cell_content(col))),
        }
    }
    leaves
}

fn column_list_figure(list: &El) -> String {
    let cells: String = flatten_columns(list, 100.0).iter().map(|(w, c)| format!("<td style=\"border-color:transparent;width:{}%;\">{c}</td>", round2(*w))).collect();
    format!("<figure class=\"table\"><table style=\"border-color:transparent;\"><tbody><tr>{cells}</tr></tbody></table></figure>")
}

fn convert_columns(nodes: Vec<Node>) -> Vec<Node> {
    map_els_pre(nodes, &mut |e: El| {
        if e.name == "div" && e.has_class("column-list") { return Ok(parse(&column_list_figure(&e))); }
        Err(e)
    })
}

fn convert_tables(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |mut e: El| {
        if e.name == "table" && e.has_class("simple-table") {
            fn strip(n: &mut Vec<Node>, in_head: bool, in_body: bool) {
                for k in n.iter_mut() {
                    if let Node::El(c) = k {
                        if ["table", "thead", "tbody", "tr", "th", "td"].contains(&c.name.as_str()) { for a in ["class", "id", "style"] { c.remove_attr(a); } }
                        let (h, b) = (in_head || c.name == "thead", in_body || c.name == "tbody");
                        if c.name == "th" {
                            if h { c.set_attr("scope", "col"); } else if b { c.set_attr("scope", "row"); }
                        }
                        strip(&mut c.kids, h, b);
                    }
                }
            }
            for a in ["class", "id", "style"] { e.remove_attr(a); }
            strip(&mut e.kids, false, false);
            return vec![Node::El(El::new("figure", &[("class", "table")], vec![Node::El(e)]))];
        }
        vec![Node::El(e)]
    })
}

fn convert_images(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |mut e: El| {
        if e.name == "figure" && e.has_class("image") {
            if let Some(img) = find_el(&e.kids, &|x| x.name == "img").cloned() {
                let mut img = img;
                img.remove_attr("style");
                e.remove_attr("id");
                e.remove_attr("data-notion-image");
                e.kids = vec![Node::El(img)];
            }
        }
        vec![Node::El(e)]
    })
}

fn convert_attachments(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "figure" {
            let anchor = find_el(&e.kids, &|x| x.name == "div" && x.has_class("source")).and_then(|d| find_el(&d.kids, &|x| x.name == "a")).cloned();
            if let Some(mut a) = anchor {
                a.set_attr("class", "notion-attachment");
                return vec![Node::El(El::new("p", &[], vec![Node::El(a)]))];
            }
        }
        vec![Node::El(e)]
    })
}

fn convert_code(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |mut e: El| {
        if e.name == "script" || e.name == "link" { return vec![]; }
        if e.name == "pre" {
            if let Some(Node::El(code)) = e.kids.iter_mut().find(|k| matches!(k, Node::El(c) if c.name == "code")) {
                let prism = Regex::new(r"language-(\S+)").unwrap().captures(&code.attr_dec("class").unwrap_or_default()).map(|c| c[1].to_string());
                code.remove_attr("style");
                code.set_attr("class", &format!("language-{}", crate::formats::code_language(prism.as_deref())));
                e.remove_attr("id");
                e.remove_attr("class");
            }
        }
        vec![Node::El(e)]
    })
}

fn convert_callouts(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "figure" && e.has_class("callout") {
            let divs = direct(&e, "div");
            let icon = divs.iter().find(|d| style_of(d).contains("font-size"));
            let content = divs.iter().find(|d| style_of(d).replace(char::is_whitespace, "").contains("width:100%")).or(divs.last());
            let emoji = icon.and_then(|i| find_el(&i.kids, &|x| x.name == "span" && x.has_class("icon"))).map(|s| text_of(s).trim().to_string()).unwrap_or_default();
            let tip = emoji == "\u{1f4a1}";
            let mut kids = content.map(|c| c.kids.clone()).unwrap_or_default();
            if !tip && !emoji.is_empty() && content.is_some() {
                let first_is_el = matches!(kids.first(), Some(Node::El(_)));
                match kids.first_mut() {
                    Some(Node::El(f)) if f.name == "p" => f.kids.insert(0, Node::Text(format!("{} ", escape_text(&emoji)))),
                    Some(Node::El(f)) if ["p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "blockquote", "pre", "figure", "table", "aside", "details"].contains(&f.name.as_str()) && first_is_el => {
                        kids.insert(0, Node::El(El::new("p", &[], vec![Node::Text(escape_text(&emoji))])));
                    }
                    _ => kids.insert(0, Node::Text(format!("{} ", escape_text(&emoji)))),
                }
            }
            return vec![Node::El(El::new("aside", &[("class", if tip { "admonition tip" } else { "admonition note" })], kids))];
        }
        vec![Node::El(e)]
    })
}

fn convert_bookmarks(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "figure" {
            if let Some(url) = find_el(&e.kids, &|x| x.name == "a" && x.has_class("bookmark")).and_then(|a| a.attr_dec("href")) {
                let q = |pred: &dyn Fn(&El) -> bool| find_el(&e.kids, pred).map(|x| text_of(x).trim().to_string()).filter(|s| !s.is_empty());
                let attr_of = |cls: &str| find_el(&e.kids, &|x| x.name == "img" && x.has_class(cls)).and_then(|x| x.attr_dec("src")).filter(|s| !s.is_empty());
                let mut s = El::new("section", &[("class", "link-embed")], vec![]);
                s.set_attr("data-url", &url);
                s.set_attr("data-embed-type", "opengraph");
                if let Some(t) = q(&|x| x.has_class("bookmark-title")) { s.set_attr("data-title", &t); }
                if let Some(t) = q(&|x| x.has_class("bookmark-description")) { s.set_attr("data-description", &t); }
                if let Some(t) = attr_of("bookmark-icon") { s.set_attr("data-favicon", &t); }
                if let Some(t) = attr_of("bookmark-image") { s.set_attr("data-image", &t); }
                return vec![Node::El(s)];
            }
        }
        vec![Node::El(e)]
    })
}

fn flatten_over_white(rgba: (u8, u8, u8, f64)) -> String {
    let b = |c: u8| (c as f64 * rgba.3 + 255.0 * (1.0 - rgba.3)).round() as u8;
    format!("rgb({}, {}, {})", b(rgba.0), b(rgba.1), b(rgba.2))
}

fn mark_style(cls: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let c = rx(&R, r"highlight-([a-z]+)(_background)?\b").captures(cls)?;
    let name = &c[1];
    if name == "default" { return None; }
    let bg = c.get(2).is_some();
    let text: [(&str, (u8, u8, u8, f64)); 9] = [("gray", (125, 122, 117, 1.0)), ("brown", (159, 118, 90, 1.0)), ("orange", (210, 123, 45, 1.0)), ("yellow", (203, 148, 52, 1.0)), ("teal", (80, 148, 110, 1.0)), ("blue", (56, 125, 201, 1.0)), ("purple", (154, 107, 180, 1.0)), ("pink", (193, 76, 138, 1.0)), ("red", (207, 81, 72, 1.0))];
    let back: [(&str, (u8, u8, u8, f64)); 9] = [("gray", (42, 28, 0, 0.07)), ("brown", (139, 46, 0, 0.086)), ("orange", (224, 101, 1, 0.129)), ("yellow", (211, 168, 0, 0.137)), ("teal", (0, 100, 45, 0.09)), ("blue", (0, 124, 215, 0.094)), ("purple", (102, 0, 178, 0.078)), ("pink", (197, 0, 93, 0.086)), ("red", (223, 22, 0, 0.094))];
    let table: &[(&str, (u8, u8, u8, f64))] = if bg { &back } else { &text };
    let (_, rgba) = table.iter().find(|(n, _)| *n == name)?;
    Some(format!("{}:{}", if bg { "background-color" } else { "color" }, flatten_over_white(*rgba)))
}

fn convert_colors(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "mark" {
            return match mark_style(&e.attr_dec("class").unwrap_or_default()) {
                Some(style) => vec![Node::El(El::new("span", &[("style", &style)], e.kids))],
                None => e.kids,
            };
        }
        vec![Node::El(e)]
    })
}

fn convert_link_to_page(nodes: Vec<Node>) -> Vec<Node> {
    map_els(nodes, &mut |e: El| {
        if e.name == "figure" && e.has_class("link-to-page") {
            if let Some(a) = find_el(&e.kids, &|x| x.name == "a").cloned() { return vec![Node::El(El::new("p", &[], vec![Node::El(a)]))]; }
        }
        vec![Node::El(e)]
    })
}

pub(crate) fn convert_notion_html(html: &str) -> String {
    let mut n = parse(html);
    n = convert_math(n);
    n = strip_date_prefixes(n);
    n = todo_pass(n);
    n = convert_toggles(n);
    n = convert_toggle_headings(n);
    n = drop_toc_and_unwrap(n);
    n = convert_inline_databases(n);
    n = merge_lists(n);
    n = convert_columns(n);
    n = convert_tables(n);
    n = convert_images(n);
    n = convert_attachments(n);
    n = convert_code(n);
    n = convert_callouts(n);
    n = convert_bookmarks(n);
    n = convert_colors(n);
    n = convert_link_to_page(n);
    serialize(&n)
}

// ------------------------------------------------------------------ properties and dates

fn parse_notion_date(text: &str) -> Option<chrono::NaiveDateTime> {
    let t = text.trim().replace('@', "");
    let t = t.trim();
    for f in ["%Y-%m-%dT%H:%M:%S%.fZ", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, f) { return Some(d); }
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(t) { return Some(d.naive_local()); }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") { return d.and_hms_opt(0, 0, 0); }
    for f in ["%B %d, %Y %I:%M %p", "%B %d, %Y %H:%M", "%b %d, %Y %I:%M %p", "%B %d, %Y %I:%M%p"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, f) { return Some(d); }
    }
    for f in ["%B %d, %Y", "%b %d, %Y", "%m/%d/%Y", "%d/%m/%Y"] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(t, f) { return d.and_hms_opt(0, 0, 0); }
    }
    None
}

fn time_value(t: &El) -> Option<String> {
    let v = t.attr_dec("datetime").unwrap_or_else(|| text_of(t)).replace('@', "");
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn local_to_utc(d: chrono::NaiveDateTime) -> String {
    chrono::Local.from_local_datetime(&d).earliest().map(|l| l.with_timezone(&chrono::Utc)).unwrap_or_else(|| chrono::Utc.from_utc_datetime(&d)).format("%Y-%m-%d %H:%M:%S%.3fZ").to_string()
}

fn to_number(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() { return None; }
    let n: String = t.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    if !n.is_empty() && n.parse::<f64>().is_ok_and(f64::is_finite) { Some(n) } else { Some(t.to_string()) }
}

fn date_prop(name: &str, text: &str) -> Option<Prop> {
    let d = parse_notion_date(text)?;
    let has_time = Regex::new(r"\d{1,2}:\d{2}").unwrap().is_match(text);
    Some(if has_time { Prop { name: name.into(), value: d.format("%Y-%m-%dT%H:%M").to_string(), label_type: Some("datetime".into()), multi: false, kind: None } }
        else { Prop { name: name.into(), value: d.format("%Y-%m-%d").to_string(), label_type: Some("date".into()), multi: false, kind: None } })
}

fn user_names(cell: &El) -> Vec<String> {
    let mut users = vec![];
    find_all_els(&cell.kids, &|x| x.name == "span" && x.has_class("user"), &mut users);
    users.iter().filter_map(|u| {
        let mut u = (*u).clone();
        u.kids = map_els(u.kids, &mut |e: El| if e.has_class("user-icon") { vec![] } else { vec![Node::El(e)] });
        let t = u.text().trim().to_string();
        (!t.is_empty()).then_some(t)
    }).collect()
}

fn checkbox_prop(name: &str, cell: &El) -> Option<Prop> {
    let cb = find_el(&cell.kids, &|x| x.name == "div" && x.has_class("checkbox"))?;
    Some(Prop { name: name.into(), value: if cb.has_class("checkbox-on") { "true" } else { "false" }.into(), label_type: Some("boolean".into()), multi: false, kind: None })
}

fn extract_properties(root: &[Node]) -> Vec<Prop> {
    let mut props = vec![];
    let mut tables = vec![];
    find_all_els(root, &|x| x.name == "table" && x.has_class("properties"), &mut tables);
    for t in tables {
        let mut rows = vec![];
        find_all_els(&t.kids, &|x| x.name == "tr" && x.has_class("property-row"), &mut rows);
        for row in rows {
            let name = find_el(&row.kids, &|x| x.name == "th").map(|th| text_of(th).trim().to_string()).unwrap_or_default();
            let Some(cell) = find_el(&row.kids, &|x| x.name == "td") else { continue };
            if name.is_empty() { continue; }
            let ty = classes(row).iter().find_map(|c| c.strip_prefix("property-row-").map(String::from)).unwrap_or_default();
            let single = |v: String, lt: &str| Prop { name: name.clone(), value: v, label_type: Some(lt.into()), multi: false, kind: None };
            match ty.as_str() {
                "text" | "select" | "status" | "place" => { let v = text_of(cell).trim().to_string(); if !v.is_empty() { props.push(single(v, "text")); } }
                "number" => if let Some(v) = to_number(&text_of(cell)) { props.push(single(v, "number")); },
                "auto_increment_id" => { let v = text_of(cell).trim().to_string(); if !v.is_empty() { let lt = if v.chars().all(|c| c.is_ascii_digit()) { "number" } else { "text" }; props.push(single(v, lt)); } }
                "multi_select" => {
                    let mut opts = vec![];
                    find_all_els(&cell.kids, &|x| x.name == "span" && x.has_class("selected-value"), &mut opts);
                    for o in opts { let v = text_of(o).trim().to_string(); if !v.is_empty() { props.push(Prop { name: name.clone(), value: v, label_type: Some("text".into()), multi: true, kind: None }); } }
                }
                "url" | "email" | "phone_number" => {
                    if let Some(h) = find_el(&cell.kids, &|x| x.name == "a").and_then(|a| a.attr_dec("href")).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) {
                        let (v, lt) = match ty.as_str() {
                            "email" => (h.strip_prefix("mailto:").unwrap_or(&h).to_string(), "email"),
                            "phone_number" => (h.strip_prefix("tel:").unwrap_or(&h).to_string(), "phone"),
                            _ => (h, "url"),
                        };
                        props.push(single(v, lt));
                    }
                }
                "date" => if let Some(time) = find_el(&cell.kids, &|x| x.name == "time") {
                    let text = text_of(time);
                    let mut parts = text.split('\u{2192}').map(str::trim);
                    let (start, end) = (parts.next().unwrap_or(""), parts.next().filter(|e| !e.is_empty()));
                    let start_text = if end.is_some() { start.to_string() } else { time_value(time).unwrap_or_else(|| start.to_string()) };
                    props.extend(date_prop(&name, &start_text));
                    if let Some(e) = end { props.extend(date_prop(&format!("{name} end"), e)); }
                },
                "checkbox" => props.extend(checkbox_prop(&name, cell)),
                "formula" | "rollup" => {
                    if let Some(p) = checkbox_prop(&name, cell) { props.push(p); }
                    else {
                        let t = text_of(cell).trim().to_string();
                        if !t.is_empty() {
                            let numeric = !Regex::new(r"[\p{L}/:]").unwrap().is_match(&t) && { let core: String = t.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '+' | '-')).collect(); Regex::new(r"^[+-]?(\d+\.?\d*|\.\d+)$").unwrap().is_match(&core) };
                            if numeric { props.push(single(t.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '+' | '-')).collect(), "number")); } else { props.push(single(t, "text")); }
                        }
                    }
                }
                "person" => for v in user_names(cell) { props.push(Prop { name: name.clone(), value: v, label_type: Some("text".into()), multi: true, kind: None }); },
                "created_by" | "last_edited_by" => if let Some(v) = user_names(cell).into_iter().next() { props.push(single(v, "text")); },
                "relation" => {
                    let mut anchors = vec![];
                    find_all_els(&cell.kids, &|x| x.name == "a", &mut anchors);
                    for a in anchors { if let Some(id) = a.attr_dec("href").and_then(|h| internal_page_id(&h)) { props.push(Prop { name: name.clone(), value: id, label_type: None, multi: true, kind: Some("relation") }); } }
                }
                "file" => {
                    let mut anchors = vec![];
                    find_all_els(&cell.kids, &|x| x.name == "a", &mut anchors);
                    for a in anchors { if let Some(h) = a.attr_dec("href").filter(|h| !h.is_empty()) { props.push(Prop { name: name.clone(), value: h, label_type: None, multi: true, kind: Some("file") }); } }
                }
                _ => {}
            }
        }
    }
    props
}

fn row_date(root: &[Node], class: &str) -> Option<String> {
    let row = find_el(root, &|x| x.name == "tr" && x.has_class(class))?;
    let t = find_el(&row.kids, &|x| x.name == "time")?;
    parse_notion_date(&time_value(t)?).map(local_to_utc)
}

fn parse_page(path: &str, html: &str) -> Option<Page> {
    let root = parse(html);
    let body = find_el(&root, &|x| x.name == "body");
    let id = body.and_then(|b| b.child_els().find_map(|c| get_notion_id(&c.attr_dec("id").unwrap_or_default()))).or_else(|| get_notion_id(base_name(path)))?;
    let title = find_el(&root, &|x| x.name == "title").map(|t| text_of(t).trim().to_string()).filter(|t| !t.is_empty())
        .or_else(|| Some(strip_notion_id(&remove_ext(base_name(path)))).filter(|t| !t.is_empty())).unwrap_or_else(|| "Untitled".into());
    let content = find_el(&root, &|x| x.has_class("page-body")).map(|pb| sanitize_html(&convert_notion_html(&inner(pb)))).unwrap_or_default();
    let mut anchors = vec![];
    find_all_els(&root, &|x| x.name == "a", &mut anchors);
    // structural references only: link-to-page blocks and a rendered collection table
    let mut structural: Vec<&El> = vec![];
    find_all_els(&root, &|x| (x.name == "figure" && x.has_class("link-to-page")) || x.has_class("collection-content"), &mut structural);
    let mut linked = vec![];
    for s in structural { let mut a = vec![]; find_all_els(&s.kids, &|x| x.name == "a", &mut a); for x in a { if let Some(i) = x.attr_dec("href").and_then(|h| internal_page_id(&h)) { linked.push(i); } } }
    Some(Page { id, title, path: path.into(), content, linked_ids: linked, props: extract_properties(&root), created: row_date(&root, "property-row-created_time"), modified: row_date(&root, "property-row-last_edited_time"), is_database: false })
}

/// Minimal CSV reader (quotes, doubled quotes, embedded newlines).
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = vec![];
    let mut row: Vec<String> = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.strip_prefix('\u{feff}').unwrap_or(text).chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') => if chars.peek() == Some(&'"') { chars.next(); field.push('"'); } else { quoted = false; },
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\n') => { row.push(std::mem::take(&mut field)); rows.push(std::mem::take(&mut row)); }
            (false, '\r') => {}
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() { row.push(field); rows.push(row); }
    rows
}

/// Is this archive a Notion HTML export (pages named `Title <id>.html`)?
pub fn looks_like_export(entries: &[Entry]) -> bool {
    // HTML export (`Title <id>.html`) or the Markdown & CSV export (`Title <id>.md` / `.csv`)
    let html: Vec<&Entry> = entries.iter().filter(|e| { let l = e.path.to_lowercase(); !e.is_dir && (l.ends_with(".html") || l.ends_with(".md") || l.ends_with(".csv")) && base_name(&e.path) != "index.html" }).collect();
    !html.is_empty() && html.iter().filter(|e| get_notion_id(base_name(&e.path)).is_some()).count() * 2 >= html.len()
        && !entries.iter().any(|e| e.path.split('/').any(|s| s == ".obsidian"))
}

struct Parsed { pages: Vec<Page>, resources: HashMap<String, Vec<u8>>, csv_paths: Vec<String>, csv_cols: HashMap<String, Vec<String>>, md_count: usize, csv_row_titles: HashSet<String>, md_files: Vec<(String, Vec<u8>)>, csv_rows: HashMap<String, Vec<Vec<String>>> }

fn read_entries(entries: Vec<Entry>, depth: usize, p: &mut Parsed, budget: &mut u64) {
    for e in entries {
        if e.is_dir { continue; }
        let path = e.path.replace('\\', "/");
        let lower = path.to_lowercase();
        if lower.ends_with(".zip") && !path.contains('/') {
            if depth < 2 { if let Ok(inner) = crate::transfer::read_zip_budget(std::io::Cursor::new(e.data), budget) { read_entries(inner, depth + 1, p, budget); } }
        } else if lower.ends_with(".html") {
            if base_name(&path) == "index.html" { continue; }
            if let Some(pg) = parse_page(&path, &String::from_utf8_lossy(&e.data)) { p.pages.push(pg); }
        } else if lower.ends_with("_all.csv") {
            // Notion writes every database twice (`Name <id>.csv` and `Name <id>_all.csv`); one is enough
        } else if lower.ends_with(".csv") {
            p.csv_paths.push(path.clone());
            let rows = parse_csv(&String::from_utf8_lossy(&e.data));
            let header = rows.first().cloned().unwrap_or_default();
            p.csv_cols.insert(owned_folder_key(&path), header.iter().map(|c| to_attribute_name(c.trim())).collect());
            for r in rows.iter().skip(1) { if let Some(t) = r.first().map(|t| t.trim()).filter(|t| !t.is_empty()) { p.csv_row_titles.insert(t.to_string()); } }
            p.csv_rows.insert(owned_folder_key(&path), rows);
        } else {
            if lower.ends_with(".md") { p.md_count += 1; p.md_files.push((path.clone(), e.data.clone())); }
            p.resources.insert(normalize_path(&path), e.data);
        }
    }
}

impl Db {
    /// Import a Notion HTML export (a zip's entries) under `parent_id`.
    pub fn import_notion(&self, entries: Vec<Entry>, parent_id: &str) -> Result<ImportStats> {
        let exists: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)))?;
        anyhow::ensure!(exists.is_some(), "import target {parent_id} not found");
        let mut parsed = Parsed { pages: vec![], resources: HashMap::new(), csv_paths: vec![], csv_cols: HashMap::new(), md_count: 0, csv_row_titles: HashSet::new(), md_files: vec![], csv_rows: HashMap::new() };
        let mut budget = crate::transfer::ZIP_BUDGET;
        read_entries(entries, 0, &mut parsed, &mut budget);
        let Parsed { mut pages, resources, csv_paths, csv_cols, md_count, csv_row_titles, md_files, csv_rows } = parsed;
        if pages.is_empty() && md_count > 0 {
            // Notion's "Markdown & CSV" export: pages are .md files, databases are .csv files
            pages = md_pages(&md_files, &csv_paths, &csv_rows, &resources);
            if pages.is_empty() && csv_paths.is_empty() { bail!("No Notion pages found: the Markdown files have no Notion ids in their names. Import the folder as plain Markdown files instead."); }
        }
        if pages.len() > 1 && pages.iter().all(|p| !p.path.contains('/')) {
            let ids: HashSet<&String> = pages.iter().map(|p| &p.id).collect();
            let flat_sub = pages.iter().any(|p| p.linked_ids.iter().any(|i| *i != p.id && ids.contains(i)));
            let flat_row = pages.iter().any(|p| csv_row_titles.contains(&p.title));
            if flat_sub || flat_row {
                bail!("This Notion export has no subpage folders, so the page hierarchy can't be rebuilt. Export again with \"Create folders for subpages\" enabled.");
            }
        }
        resolve_database_containers(&mut pages, &csv_paths);
        reconcile_date_columns(&mut pages);

        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let root = self.create_note(parent_id, "Notion import", "text", "text/html", "")?;
        self.add_attribute(&root, "label", "iconClass", "bx bx-import", false)?;
        stats.notes += 1;
        stats.root = Some(root.clone());

        let mut by_folder: HashMap<String, String> = HashMap::new();
        let mut target_by_id: HashMap<String, (String, String)> = HashMap::new();
        let mut created: Vec<(String, String, usize)> = vec![]; // note id, content, page index
        let mut order: Vec<usize> = (0..pages.len()).collect();
        order.sort_by_key(|i| pages[*i].path.split('/').count());

        for i in order {
            let page = &pages[i];
            let parent = by_folder.get(&parent_folder_key(&page.path)).cloned().unwrap_or_else(|| root.clone());
            let id = if page.is_database { self.create_note(&parent, &page.title, "book", "", "")? } else { self.create_note(&parent, &page.title, "text", "text/html", "")? };
            stats.notes += 1;
            by_folder.insert(owned_folder_key(&page.path), id.clone());
            target_by_id.insert(page.id.clone(), (id.clone(), page.title.clone()));
            if page.is_database { self.add_attribute(&id, "label", "viewType", "table", false)?; stats.attributes += 1; }
            // own property values: files become attachments, relations wait for pass two, the rest are labels
            let mut file_links = String::new();
            for p in &page.props {
                match p.kind {
                    Some("file") => {
                        let rp = resolve_resource_path(&page.path, &p.value);
                        if let Some(bytes) = resources.get(&rp) {
                            let name = base_name(&rp).to_string();
                            let mime = mime_guess::from_path(&name).first_or_octet_stream().to_string();
                            let aid = self.add_attachment(&id, &name, &mime, "file", bytes)?;
                            stats.attachments += 1;
                            file_links.push_str(&format!("<p><a class=\"reference-link\" href=\"#root/{id}?viewMode=attachments&amp;attachmentId={aid}\">{}</a></p>", escape_text(&name)));
                        }
                    }
                    Some(_) => {}
                    None => { self.add_attribute(&id, "label", &to_attribute_name(&p.name), &p.value, false)?; stats.attributes += 1; }
                }
            }
            let body = format!("{file_links}{}", page.content);
            let body = self.rewrite_notion_images(&id, &body, &page.path, &resources, &mut stats)?;
            let body = self.rewrite_notion_attachments(&id, &body, &page.path, &resources, &mut stats)?;
            created.push((id, body, i));
        }

        apply_database_schemas(self, &pages, &by_folder, &csv_cols, &mut stats)?;

        // second pass: cross-page links, inline database includes, relations, original timestamps
        for (id, body, i) in &created {
            let page = &pages[*i];
            let body = rewrite_links(body, &target_by_id);
            let body = rewrite_collection_includes(&body, &target_by_id);
            let dates = match (page.created.as_deref(), page.modified.as_deref()) {
                (None, None) => None,
                (c, m) => { let c = c.or(m).unwrap_or_default(); Some((c.to_string(), m.or(Some(c)).unwrap_or_default().to_string())) }
            };
            self.store_imported_html(id, &body, dates.as_ref().map(|(c, m)| (c.as_str(), m.as_str())))?;
            for p in page.props.iter().filter(|p| p.kind == Some("relation")) {
                if let Some((target, _)) = target_by_id.get(&p.value) {
                    self.add_attribute(id, "relation", &to_attribute_name(&p.name), target, false)?;
                    stats.attributes += 1;
                }
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    fn rewrite_notion_images(&self, note_id: &str, html: &str, page_path: &str, res: &HashMap<String, Vec<u8>>, stats: &mut ImportStats) -> Result<String> {
        if !html.contains("<img") { return Ok(html.to_string()); }
        let mut err = None;
        let mut saved = 0;
        let out = map_els(parse(html), &mut |mut e: El| {
            if e.name == "img" {
                if let Some(src) = e.attr_dec("src").filter(|s| !s.is_empty()) {
                    let rp = resolve_resource_path(page_path, &src);
                    if let Some(bytes) = res.get(&rp) {
                        let name = base_name(&rp).to_string();
                        let mime = mime_guess::from_path(&name).first_or_octet_stream().to_string();
                        let mime = if mime.starts_with("image/") { mime } else { "image/png".into() };
                        match self.add_attachment(note_id, &name, &mime, "image", bytes) {
                            Ok(aid) => { saved += 1; e.set_attr("src", &format!("api/attachments/{aid}/image/{}", percent_encode(&name))); }
                            Err(x) => { err.get_or_insert(x); }
                        }
                    }
                }
            }
            vec![Node::El(e)]
        });
        stats.attachments += saved;
        match err { Some(e) => Err(e), None => Ok(serialize(&out)) }
    }

    fn rewrite_notion_attachments(&self, note_id: &str, html: &str, page_path: &str, res: &HashMap<String, Vec<u8>>, stats: &mut ImportStats) -> Result<String> {
        if !html.contains("notion-attachment") { return Ok(html.to_string()); }
        let mut err = None;
        let mut saved = 0;
        let out = map_els(parse(html), &mut |mut e: El| {
            if e.name == "a" && e.has_class("notion-attachment") {
                e.remove_attr("class");
                let href = e.attr_dec("href").unwrap_or_default();
                let rp = resolve_resource_path(page_path, &href);
                if let Some(bytes) = res.get(&rp) {
                    let title = { let t = text_of(&e).trim().to_string(); if t.is_empty() { base_name(&rp).to_string() } else { t } };
                    let mime = mime_guess::from_path(base_name(&rp)).first_or_octet_stream().to_string();
                    match self.add_attachment(note_id, &title, &mime, "file", bytes) {
                        Ok(aid) => { saved += 1; e.set_attr("href", &format!("#root/{note_id}?viewMode=attachments&attachmentId={aid}")); e.set_attr("class", "reference-link"); }
                        Err(x) => { err.get_or_insert(x); }
                    }
                }
            }
            vec![Node::El(e)]
        });
        stats.attachments += saved;
        match err { Some(e) => Err(e), None => Ok(serialize(&out)) }
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() { if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); } }
    out
}

/// Column types inferred from a CSV's values (the Markdown & CSV export carries no types): number, date(time), checkbox, else text.
fn infer_column(values: &[&str]) -> &'static str {
    let vals: Vec<&str> = values.iter().copied().filter(|v| !v.trim().is_empty()).collect();
    if vals.is_empty() { return "text"; }
    if vals.iter().all(|v| v.trim().parse::<f64>().is_ok_and(f64::is_finite)) { return "number"; }
    if vals.iter().all(|v| matches!(v.trim(), "Yes" | "No")) { return "boolean"; }
    let dates: Vec<&&str> = vals.iter().filter(|v| v.contains(char::is_numeric)).collect();
    if dates.len() == vals.len() && vals.iter().all(|v| v.split('\u{2192}').all(|p| parse_notion_date(p).is_some())) {
        return if vals.iter().any(|v| Regex::new(r"\d{1,2}:\d{2}").unwrap().is_match(v)) { "datetime" } else { "date" };
    }
    "text"
}

fn md_pages(md_files: &[(String, Vec<u8>)], csv_paths: &[String], csv_rows: &HashMap<String, Vec<Vec<String>>>, resources: &HashMap<String, Vec<u8>>) -> Vec<Page> {
    let db_keys: HashSet<String> = csv_paths.iter().map(|p| owned_folder_key(p)).collect();
    let mut col_types: HashMap<(String, usize), &'static str> = HashMap::new();
    for (key, rows) in csv_rows {
        let ncols = rows.first().map_or(0, Vec::len);
        for c in 1..ncols {
            let vals: Vec<&str> = rows.iter().skip(1).filter_map(|r| r.get(c).map(String::as_str)).collect();
            col_types.insert((key.clone(), c), infer_column(&vals));
        }
    }
    let mut pages = vec![];
    let mut used_rows: HashSet<(String, usize)> = HashSet::new();
    for (path, bytes) in md_files {
        let Some(id) = get_notion_id(base_name(path)) else { continue };
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut lines = text.lines().peekable();
        let mut title = None;
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() { lines.next(); continue; }
            if let Some(t) = l.strip_prefix("# ") { title = Some(t.trim().to_string()); lines.next(); }
            break;
        }
        let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| { let t = strip_notion_id(&remove_ext(base_name(path))); if t.is_empty() { "Untitled".into() } else { t } });
        let parent_key = parent_folder_key(path);
        let is_row = db_keys.contains(&parent_key);
        // a database row starts with `Name: value` property lines, then a blank line
        let mut md_props: Vec<(String, String)> = vec![];
        let mut rest: Vec<&str> = vec![];
        if is_row {
            while lines.peek().is_some_and(|l| l.trim().is_empty()) { lines.next(); }
            while let Some(l) = lines.peek() {
                if l.trim().is_empty() { break; }
                // with a CSV at hand only its column names count as properties; a body line like "Note: ..." stays body text
                let header = csv_rows.get(&parent_key).and_then(|r| r.first());
                match l.split_once(": ") {
                    Some((k, v)) if !k.trim().is_empty() && k.len() < 80 && header.is_none_or(|h| h.iter().any(|c| c.trim() == k.trim())) => { md_props.push((k.trim().to_string(), v.trim().to_string())); lines.next(); }
                    _ => break,
                }
            }
        }
        rest.extend(lines);
        let body = rest.join("\n");
        // properties: prefer the database's CSV row (it is the typed source), else the lines found in the page
        let mut props: Vec<Prop> = vec![];
        // rows sharing a title: take the unused one that agrees best with the page's own property lines
        let csv_row = csv_rows.get(&parent_key).and_then(|rows| {
            let header = &rows[0];
            let score = |r: &Vec<String>| md_props.iter().filter(|(k, v)| header.iter().position(|h| h.trim() == k).and_then(|i| r.get(i)).is_some_and(|c| c.trim() == v)).count();
            let best = rows.iter().enumerate().skip(1)
                .filter(|(i, r)| r.first().map(|t| t.trim()) == Some(title.as_str()) && !used_rows.contains(&(parent_key.clone(), *i)))
                .max_by_key(|(i, r)| (score(r), std::cmp::Reverse(*i)))?;
            used_rows.insert((parent_key.clone(), best.0));
            Some((header, best.1))
        });
        let source: Vec<(usize, String, String)> = match csv_row {
            Some((header, row)) => header.iter().enumerate().skip(1).filter_map(|(i, h)| row.get(i).map(|v| (i, h.trim().to_string(), v.trim().to_string()))).collect(),
            None => md_props.iter().enumerate().map(|(i, (k, v))| (i + 1, k.clone(), v.clone())).collect(),
        };
        for (i, name, value) in source {
            if value.is_empty() || name.is_empty() { continue; }
            if value.contains("notion.so/") || value.contains(".md)") {
                for part in value.split(", ") { if let Some(rid) = get_notion_id(part.rsplit_once('(').map_or(part, |(_, u)| u).trim_end_matches(')')) { props.push(Prop { name: name.clone(), value: rid, label_type: None, multi: true, kind: Some("relation") }); } }
                continue;
            }
            let ty = col_types.get(&(parent_key.clone(), i)).copied().unwrap_or("text");
            let prop = match ty {
                "number" => Prop { name, value, label_type: Some("number".into()), multi: false, kind: None },
                "boolean" => Prop { name, value: (value == "Yes").to_string(), label_type: Some("boolean".into()), multi: false, kind: None },
                "date" | "datetime" => {
                    let mut parts = value.split('\u{2192}').map(str::trim);
                    let start = parts.next().unwrap_or("");
                    let end = parts.next();
                    if let Some(p) = date_prop(&name, start) { props.push(p); }
                    if let Some(p) = end.and_then(|e| date_prop(&format!("{name} end"), e)) { props.push(p); }
                    continue;
                }
                _ => Prop { name, value, label_type: Some("text".into()), multi: false, kind: None },
            };
            props.push(prop);
        }
        // links to other pages and to bundled files
        let html = crate::mdhtml::md_to_html(&body, &title, false);
        let mut linked = vec![];
        let nodes = map_els(parse(&html), &mut |mut e: El| {
            if e.name == "a" {
                if let Some(href) = e.attr_dec("href") {
                    // a link to a database's CSV stands for the database itself
                    if percent_decode(&href).split(['?', '#']).next().unwrap_or("").to_lowercase().ends_with(".csv") {
                        if let Some(did) = get_notion_id(&percent_decode(&href)) {
                            return parse(&format!("<section class=\"include-note\" data-notion-id=\"{did}\" data-box-size=\"medium\">&nbsp;</section>"));
                        }
                    }
                    if let Some(pid) = internal_page_id(&href) { linked.push(pid); }
                    else if !href.starts_with('#') && !href.contains("://") && resources.contains_key(&resolve_resource_path(path, &href)) { e.set_attr("class", "notion-attachment"); }
                }
            }
            vec![Node::El(e)]
        });
        pages.push(Page { id, title, path: path.clone(), content: sanitize_html(&serialize(&nodes)), linked_ids: linked, props, is_database: false, created: None, modified: None });
    }
    pages
}

fn resolve_database_containers(pages: &mut Vec<Page>, csv_paths: &[String]) {
    let db_keys: HashSet<String> = csv_paths.iter().map(|p| owned_folder_key(p)).collect();
    let mut owned: HashSet<String> = pages.iter().map(|p| owned_folder_key(&p.path)).collect();
    for p in pages.iter_mut() { if db_keys.contains(&owned_folder_key(&p.path)) { p.is_database = true; p.content.clear(); } }
    for csv in csv_paths {
        let key = owned_folder_key(csv);
        if owned.contains(&key) { continue; }
        owned.insert(key);
        let title = strip_notion_id(&remove_ext(base_name(csv)));
        pages.push(Page { id: get_notion_id(base_name(csv)).unwrap_or_default(), title: if title.is_empty() { "Database".into() } else { title }, path: csv.clone(), is_database: true, ..Default::default() });
    }
}

fn reconcile_date_columns(pages: &mut [Page]) {
    let key = |path: &str, name: &str| format!("{} {}", parent_folder_key(path), to_attribute_name(name));
    let with_time: HashSet<String> = pages.iter().flat_map(|p| p.props.iter().filter(|x| x.label_type.as_deref() == Some("datetime")).map(|x| key(&p.path, &x.name)).collect::<Vec<_>>()).collect();
    for p in pages.iter_mut() {
        let path = p.path.clone();
        for prop in p.props.iter_mut() {
            let is_date = matches!(prop.label_type.as_deref(), Some("date" | "datetime"));
            if is_date && with_time.contains(&key(&path, &prop.name)) {
                prop.label_type = Some("datetime".into());
                if !prop.value.contains('T') { prop.value.push_str("T00:00"); }
            }
        }
    }
}

fn apply_database_schemas(db: &Db, pages: &[Page], by_folder: &HashMap<String, String>, csv_cols: &HashMap<String, Vec<String>>, stats: &mut ImportStats) -> Result<()> {
    let mut schemas: Vec<(String, Vec<Prop>)> = vec![];
    for p in pages {
        let fk = parent_folder_key(&p.path);
        if !by_folder.contains_key(&fk) || p.props.is_empty() { continue; }
        let idx = match schemas.iter().position(|(k, _)| *k == fk) { Some(i) => i, None => { schemas.push((fk.clone(), vec![])); schemas.len() - 1 } };
        for prop in &p.props {
            if prop.kind == Some("file") { continue; }
            if !schemas[idx].1.iter().any(|x| to_attribute_name(&x.name) == to_attribute_name(&prop.name)) { schemas[idx].1.push(prop.clone()); }
        }
    }
    for (fk, mut props) in schemas {
        let Some(container) = by_folder.get(&fk) else { continue };
        if let Some(cols) = csv_cols.get(&fk) {
            let idx: HashMap<&String, usize> = cols.iter().enumerate().map(|(i, c)| (c, i)).collect();
            let sort_key = |p: &Prop| -> (usize, usize) {
                if let Some(i) = idx.get(&to_attribute_name(&p.name)) { return (*i, 0); }
                if let Some(base) = p.name.strip_suffix(" end") { if let Some(i) = idx.get(&to_attribute_name(base)) { return (*i, 1); } }
                (usize::MAX, 0)
            };
            props.sort_by_key(|p| sort_key(p));
        }
        for p in props {
            let kind = if p.kind == Some("relation") { "relation" } else { "label" };
            db.add_attribute(container, "label", &format!("{kind}:{}", to_attribute_name(&p.name)), &promoted_definition(&p.name, p.label_type.as_deref(), p.multi), true)?;
            stats.attributes += 1;
        }
    }
    Ok(())
}

/// Page links → internal links (reference links when the text is the target's title).
fn rewrite_links(html: &str, targets: &HashMap<String, (String, String)>) -> String {
    if !html.contains(".html") && !html.contains(".md") { return html.to_string(); }
    serialize(&map_els(parse(html), &mut |mut e: El| {
        if e.name == "a" {
            if let Some((id, title)) = e.attr_dec("href").and_then(|h| internal_page_id(&h)).and_then(|i| targets.get(&i)) {
                e.set_attr("href", &format!("#root/{id}"));
                if text_of(&e).trim() == title.trim() { e.set_attr("class", "reference-link"); }
            }
        }
        vec![Node::El(e)]
    }))
}

fn rewrite_collection_includes(html: &str, targets: &HashMap<String, (String, String)>) -> String {
    if !html.contains("data-notion-id") { return html.to_string(); }
    serialize(&map_els(parse(html), &mut |mut e: El| {
        if e.name == "section" && e.has_class("include-note") {
            if let Some(nid) = e.attr_dec("data-notion-id") {
                e.remove_attr("data-notion-id");
                return match targets.get(&nid) {
                    Some((id, _)) => { e.set_attr("data-note-id", id); vec![Node::El(e)] }
                    None => vec![],
                };
            }
        }
        vec![Node::El(e)]
    }))
}

#[allow(dead_code)]
fn _unused() -> String { escape_attr(&decode_entities(&text_content(&[]))) }

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "0123456789abcdef0123456789abcdef";
    const ID_B: &str = "fedcba9876543210fedcba9876543210";
    const ID_DB: &str = "aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbb";
    const ID_ROW: &str = "11111111111111112222222222222222";

    fn page(id: &str, title: &str, body: &str, props: &str) -> String {
        format!("<html><head><title>{title}</title><style>@import katex</style></head><body><article id=\"{id}\" class=\"page sans\"><header><h1 class=\"page-title\">{title}</h1><table class=\"properties\"><tbody>{props}</tbody></table></header><div class=\"page-body\">{body}</div></article></body></html>")
    }

    fn e(path: &str, data: &str) -> Entry { Entry { path: path.into(), data: data.as_bytes().to_vec(), is_dir: false } }

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("n.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn kids(db: &Db, p: &str) -> Vec<(String, String)> {
        let mut st = db.conn.prepare("SELECT n.noteId,n.title FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0 ORDER BY b.notePosition").unwrap();
        st.query_map([p], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
    fn child(db: &Db, p: &str, t: &str) -> String { kids(db, p).into_iter().find(|k| k.1 == t).unwrap_or_else(|| panic!("no {t}: {:?}", kids(db, p))).0 }

    #[test]
    fn ids_and_paths() {
        assert_eq!(strip_notion_id(&format!("Page Title {ID_A}.html")), "Page Title.html");
        assert_eq!(get_notion_id(&format!("Folder/Sub {ID_B}.html?x=1")).as_deref(), Some(ID_B));
        assert_eq!(owned_folder_key(&format!("Top {ID_A}/Child {ID_B}.html")), "Top/Child");
        assert_eq!(parent_folder_key(&format!("Top {ID_A}/Child {ID_B}.html")), "Top");
        assert_eq!(resolve_resource_path("A/B/page.html", "../img%20x.png"), "A/img x.png");
        assert_eq!(internal_page_id(&format!("Some%20Page%20{ID_A}.html")).as_deref(), Some(ID_A));
        assert!(internal_page_id("https://example.org/x.html").is_none());
    }

    #[test]
    fn converter_blocks() {
        let h = convert_notion_html(r##"
          <div style="display:contents"><ul class="to-do-list"><li><div class="checkbox checkbox-on"></div> <span class="to-do-children-checked">done</span></li></ul></div>
          <div style="display:contents"><ul class="to-do-list"><li><div class="checkbox checkbox-off"></div> <span class="to-do-children-unchecked">open</span></li></ul></div>
          <ul class="bulleted-list"><li>a</li></ul><ul class="bulleted-list"><li>b</li></ul><p>x</p><ol class="numbered-list" start="1"><li>n</li></ol>
          <ul class="toggle"><li><details open=""><summary>Sum</summary><p>inside</p></details></li></ul>
          <figure class="callout" style="white-space:pre-wrap;display:flex"><div style="font-size:1.5em"><span class="icon">⚠️</span></div><div style="width:100%">warn</div></figure>
          <figure class="callout"><div style="font-size:1.5em"><span class="icon">💡</span></div><div style="width:100%">idea</div></figure>
          <pre class="code" id="x"><code class="language-javascript">let a = 1 &lt; 2;</code></pre>
          <table class="simple-table" id="t"><thead><tr id="r"><th id="h">H</th></tr></thead><tbody><tr><th>R</th><td style="x">1</td></tr></tbody></table>
          <figure class="image" data-notion-image="a.png"><a href="a.png"><img src="a.png" style="width:100px"></a></figure>
          <figure><div class="source"><a href="doc.pdf">doc.pdf</a></div></figure>
          <figure><a class="bookmark source" href="https://x.org"><div class="bookmark-info"><div class="bookmark-title">T</div></div></a></figure>
          <p><mark class="highlight-red">r</mark> <mark class="highlight-blue_background">b</mark> <mark class="highlight-default">d</mark></p>
          <figure class="link-to-page"><a href="Sub%20fedcba9876543210fedcba9876543210.html">Sub</a></figure>
          <div class="column-list"><div class="column" style="width:25%"><p>left</p></div><div class="column" style="width:75%"><p>right</p></div></div>
          <nav class="table_of_contents"><a href="#x">toc</a></nav>
          <span class="notion-text-equation-token"><span class="katex"><annotation encoding="application/x-tex">x^2</annotation></span></span>"##);
        for want in [
            r#"<ul class="todo-list"><li><label class="todo-list__label"><input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">done</span></label></li><li><label class="todo-list__label"><input type="checkbox" disabled="disabled"><span class="todo-list__label__description">open</span></label></li></ul>"#,
            "<ul><li>a</li><li>b</li></ul>", "<ol><li>n</li></ol>", r#"<details open="" class="trilium-collapsible"><summary>Sum</summary><p>inside</p></details>"#,
            r#"<aside class="admonition note">⚠️ warn</aside>"#, r#"<aside class="admonition tip">idea</aside>"#,
            r#"<pre><code class="language-text-javascript">let a = 1 &lt; 2;</code></pre>"#,
            r#"<figure class="table"><table><thead><tr><th scope="col">H</th></tr></thead><tbody><tr><th scope="row">R</th><td>1</td></tr></tbody></table></figure>"#,
            r#"<figure class="image"><img src="a.png"></figure>"#, r#"<p><a href="doc.pdf" class="notion-attachment">doc.pdf</a></p>"#,
            r#"<section class="link-embed" data-url="https://x.org" data-embed-type="opengraph" data-title="T"></section>"#,
            r#"<span style="color:rgb(207, 81, 72)">r</span>"#, r#"<span style="background-color:rgb(231, 243, 251)">b</span>"#, "d</p>",
            r#"<p><a href="Sub%20fedcba9876543210fedcba9876543210.html">Sub</a></p>"#,
            r#"<td style="border-color:transparent;width:25%;">left</td><td style="border-color:transparent;width:75%;">right</td>"#, r#"<span class="math-tex">\(x^2\)</span>"#,
        ] {
            assert!(h.contains(want), "missing {want}\n{h}");
        }
        assert!(!h.contains("toc") && !h.contains("display:contents") && !h.contains("katex"), "{h}");
    }

    #[test]
    fn properties_and_dates() {
        let rows = r#"
          <tr class="property-row property-row-select"><th><span class="icon"></span>Status</th><td><span class="selected-value">Open</span></td></tr>
          <tr class="property-row property-row-number"><th>Cost</th><td>$1,200.50</td></tr>
          <tr class="property-row property-row-multi_select"><th>Tags</th><td><span class="selected-value">a</span><span class="selected-value">b</span></td></tr>
          <tr class="property-row property-row-checkbox"><th>Done</th><td><div class="checkbox checkbox-on"></div></td></tr>
          <tr class="property-row property-row-date"><th>When</th><td><time>@June 24, 2026 10:30 AM → June 25, 2026 11:00 AM</time></td></tr>
          <tr class="property-row property-row-email"><th>Mail</th><td><a href="mailto:a@b.c">a@b.c</a></td></tr>
          <tr class="property-row property-row-relation"><th>Rel</th><td><a href="Other%20fedcba9876543210fedcba9876543210.html">Other</a></td></tr>
          <tr class="property-row property-row-created_time"><th>Created</th><td><time datetime="2026-01-02T03:04:00">January 2, 2026 3:04 AM</time></td></tr>"#;
        let root = parse(&page(ID_A, "T", "", rows));
        let props = extract_properties(&root);
        let get = |n: &str| props.iter().filter(|p| p.name == n).map(|p| (p.value.clone(), p.label_type.clone().unwrap_or_default(), p.multi)).collect::<Vec<_>>();
        assert_eq!(get("Status"), [("Open".into(), "text".into(), false)]);
        assert_eq!(get("Cost"), [("1200.50".into(), "number".into(), false)]);
        assert_eq!(get("Tags").len(), 2);
        assert_eq!(get("Done"), [("true".into(), "boolean".into(), false)]);
        assert_eq!(get("When"), [("2026-06-24T10:30".into(), "datetime".into(), false)]);
        assert_eq!(get("When end"), [("2026-06-25T11:00".into(), "datetime".into(), false)]);
        assert_eq!(get("Mail"), [("a@b.c".into(), "email".into(), false)]);
        assert_eq!(props.iter().find(|p| p.kind == Some("relation")).unwrap().value, ID_B);
        assert!(row_date(&root, "property-row-created_time").unwrap().starts_with("2026-01-0"));
        assert_eq!(parse_csv("a,\"b,c\",d\n\"x\"\"y\",2,\"multi\nline\"\n"), [vec!["a", "b,c", "d"], vec!["x\"y", "2", "multi\nline"]]);
    }

    #[test]
    fn full_export_structure_links_databases_and_files() {
        let (_d, db) = fixture();
        let top = page(ID_A, "Top page", &format!(r#"<p>See <a href="Top%20page/Sub%20{ID_B}.html">Sub</a> and <a href="Top%20page/Sub%20{ID_B}.html">custom text</a>.</p><figure class="image"><a href="Top%20page/pic.png"><img src="Top%20page/pic.png"></a></figure><figure><div class="source"><a href="Top%20page/doc.txt">doc.txt</a></div></figure><div class="collection-content" id="{ID_DB}"><table class="collection-content"></table></div>"#),
            r#"<tr class="property-row property-row-last_edited_time"><th>Edited</th><td><time datetime="2025-05-06T07:08:00">May 6, 2025 7:08 AM</time></td></tr>"#);
        let sub = page(ID_B, "Sub", "<p>child body</p>", "");
        let db_page = page(ID_DB, "Tasks", "<table class=\"collection-content\"><tr><td>rendered table</td></tr></table>", "");
        let row = page(ID_ROW, "Task one", "<p>row body</p>", r#"<tr class="property-row property-row-select"><th>Status</th><td>Open</td></tr><tr class="property-row property-row-number"><th>Points</th><td>3</td></tr>"#);
        let entries = vec![
            e(&format!("Top page {ID_A}.html"), &top), e(&format!("Top page/Sub {ID_B}.html"), &sub),
            Entry { path: "Top page/pic.png".into(), data: b"\x89PNG".to_vec(), is_dir: false }, e("Top page/doc.txt", "plain"),
            e(&format!("Tasks {ID_DB}.html"), &db_page), e(&format!("Tasks {ID_DB}.csv"), "Name,Status,Points\nTask one,Open,3\n"),
            e(&format!("Tasks/Task one {ID_ROW}.html"), &row), e("index.html", "<html></html>"),
        ];
        assert!(looks_like_export(&entries));
        let st = db.import_notion(entries, "root").unwrap();
        let r = st.root.unwrap();
        let top_id = child(&db, &r, "Top page");
        let sub_id = child(&db, &top_id, "Sub");
        let html = db.note_content(&top_id).unwrap().unwrap();
        assert!(html.contains(&format!("<a href=\"#root/{sub_id}\" class=\"reference-link\">Sub</a>")), "{html}");
        assert!(html.contains(&format!("<a href=\"#root/{sub_id}\">custom text</a>")), "{html}");
        let tasks = child(&db, &r, "Tasks");
        assert!(html.contains(&format!("data-note-id=\"{tasks}\"")), "inline database include resolved: {html}");
        let atts: Vec<(String, String)> = db.attachments(&top_id).unwrap().into_iter().map(|a| (a.title, a.role)).collect();
        assert!(atts.contains(&("pic.png".into(), "image".into())) && atts.contains(&("doc.txt".into(), "file".into())), "{atts:?}");
        assert!(html.contains("api/attachments/") && html.contains("viewMode=attachments"), "{html}");
        let (c, m): (String, String) = db.conn.query_row("SELECT utcDateCreated,utcDateModified FROM notes WHERE noteId=?", [&top_id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(c.starts_with("2025-05-0") && m.starts_with("2025-05-0"), "{c} {m}");
        // database: a table collection with inheritable promoted columns in CSV order; rows carry the values
        let (ntype,): (String,) = db.conn.query_row("SELECT type FROM notes WHERE noteId=?", [&tasks], |r| Ok((r.get(0)?,))).unwrap();
        assert_eq!(ntype, "book");
        assert_eq!(db.label_value(&tasks, "viewType").unwrap().as_deref(), Some("table"));
        assert_eq!(db.note_content(&tasks).unwrap().unwrap_or_default(), "", "the rendered table body is dropped");
        assert_eq!(db.label_value(&tasks, "label:status").unwrap().as_deref(), Some("promoted,single,text,alias=Status"));
        assert_eq!(db.label_value(&tasks, "label:points").unwrap().as_deref(), Some("promoted,single,number,alias=Points"));
        let inh: i64 = db.conn.query_row("SELECT isInheritable FROM attributes WHERE noteId=? AND name='label:status' AND isDeleted=0", [&tasks], |r| r.get(0)).unwrap();
        assert_eq!(inh, 1);
        let row_id = child(&db, &tasks, "Task one");
        assert_eq!(db.label_value(&row_id, "status").unwrap().as_deref(), Some("Open"));
        assert_eq!(db.label_value(&row_id, "points").unwrap().as_deref(), Some("3"));
        assert_eq!(db.note_content(&row_id).unwrap().unwrap(), "<p>row body</p>");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn markdown_and_csv_export() {
        let (_d, db) = fixture();
        let top = format!("# Top page\n\nSome **text** with a [link to sub](Top%20page/Sub%20{ID_B}.md) and a [file](Top%20page/doc.txt).\n\n![pic](Top%20page/pic.png)\n\n- [x] done\n\n<details><summary>Toggle</summary>\n\nhidden\n\n</details>\n\n[Tasks]({ID_DB}.csv)\n");
        let sub = "# Sub\n\nchild body\n".to_string();
        let db_page = format!("# Tasks\n\n[Tasks]({ID_DB}.csv)\n");
        let row1 = "# Task one\n\nStatus: Open\nPoints: 3\nDue: June 24, 2026\nDone: Yes\nRelated: Task two (https://www.notion.so/Task-two-33333333333333334444444444444444)\n\nrow body\n".to_string();
        let row2 = "# Task two\n\nStatus: Done\nPoints: 5\nDue: June 30, 2026\nDone: No\n\nsecond\n".to_string();
        let csv = "Name,Status,Points,Due,Done,Related\nTask one,Open,3,\"June 24, 2026\",Yes,\"Task two (https://www.notion.so/Task-two-33333333333333334444444444444444)\"\nTask two,Done,5,\"June 30, 2026\",No,\n";
        let entries = vec![
            e(&format!("Top page {ID_A}.md"), &top), e(&format!("Top page/Sub {ID_B}.md"), &sub), Entry { path: "Top page/pic.png".into(), data: b"\x89PNGpic".to_vec(), is_dir: false }, e("Top page/doc.txt", "document"),
            e(&format!("Tasks {ID_DB}.md"), &db_page), e(&format!("Tasks {ID_DB}.csv"), csv), e(&format!("Tasks {ID_DB}_all.csv"), csv),
            e(&format!("Tasks/Task one {ID_ROW}.md"), &row1), e(&format!("Tasks/Task two 33333333333333334444444444444444.md"), &row2),
        ];
        assert!(looks_like_export(&entries));
        let st = db.import_notion(entries, "root").unwrap();
        let r = st.root.unwrap();
        let top_id = child(&db, &r, "Top page");
        let sub_id = child(&db, &top_id, "Sub");
        let html = db.note_content(&top_id).unwrap().unwrap();
        assert!(html.contains(&format!("<a href=\"#root/{sub_id}\" class=\"reference-link\">link to sub</a>")) || html.contains(&format!("#root/{sub_id}")), "{html}");
        assert!(html.contains("<strong>text</strong>") && html.contains("api/attachments/") && html.contains("viewMode=attachments") && html.contains("todo-list"), "{html}");
        assert!(html.contains("<details class=\"trilium-collapsible\"><summary>Toggle</summary>"), "{html}");
        let atts: Vec<(String, String)> = db.attachments(&top_id).unwrap().into_iter().map(|a| (a.title, a.role)).collect();
        assert!(atts.contains(&("pic.png".into(), "image".into())) && atts.contains(&("file".into(), "file".into())), "{atts:?}");
        // the database is a table collection and its rows carry typed values from the CSV
        let tasks = child(&db, &r, "Tasks");
        assert_eq!(db.label_value(&tasks, "viewType").unwrap().as_deref(), Some("table"));
        assert_eq!(db.label_value(&tasks, "label:points").unwrap().as_deref(), Some("promoted,single,number,alias=Points"));
        assert_eq!(db.label_value(&tasks, "label:due").unwrap().as_deref(), Some("promoted,single,date,alias=Due"));
        assert_eq!(db.label_value(&tasks, "label:done").unwrap().as_deref(), Some("promoted,single,boolean,alias=Done"));
        assert_eq!(db.label_value(&tasks, "label:status").unwrap().as_deref(), Some("promoted,single,text,alias=Status"));
        assert!(db.label_value(&tasks, "relation:related").unwrap().is_some());
        let one = child(&db, &tasks, "Task one");
        let two = child(&db, &tasks, "Task two");
        assert_eq!(db.label_value(&one, "status").unwrap().as_deref(), Some("Open"));
        assert_eq!(db.label_value(&one, "points").unwrap().as_deref(), Some("3"));
        assert_eq!(db.label_value(&one, "due").unwrap().as_deref(), Some("2026-06-24"));
        assert_eq!(db.label_value(&one, "done").unwrap().as_deref(), Some("true"));
        assert_eq!(db.label_value(&two, "done").unwrap().as_deref(), Some("false"));
        let rel: String = db.conn.query_row("SELECT value FROM attributes WHERE noteId=? AND name='related' AND type='relation' AND isDeleted=0", [&one], |r| r.get(0)).unwrap();
        assert_eq!(rel, two);
        assert_eq!(db.note_content(&one).unwrap().unwrap(), "<p>row body</p>", "property lines are not part of the body");
        assert_eq!(kids_titles(&db, &r), ["Tasks", "Top page"], "the _all.csv duplicate adds nothing");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn markdown_csv_duplicate_titles_and_property_lookalikes() {
        let (_d, db) = fixture();
        let csv = "Name,Status,Count\nAlpha,Open,1\nAlpha,Closed,3\nGamma,Open,2\n";
        let entries = vec![
            e(&format!("Tasks {ID_DB}.csv"), csv), e(&format!("Tasks {ID_DB}.md"), "# Tasks\n"),
            e("Tasks/Alpha 11111111111111111111111111111111.md", "# Alpha\n\nStatus: Open\nCount: 1\n\nfirst"),
            e("Tasks/Alpha 22222222222222222222222222222222.md", "# Alpha\n\nStatus: Closed\nCount: 3\n\nsecond"),
            e("Tasks/Gamma 33333333333333333333333333333333.md", "# Gamma\n\nNote: this is the first body line\n\nrest"),
        ];
        let st = db.import_notion(entries, "root").unwrap();
        let tasks = child(&db, &st.root.unwrap(), "Tasks");
        let rows: Vec<(String, String, String)> = kids(&db, &tasks).into_iter().map(|(id, t)| (t, db.label_value(&id, "status").unwrap().unwrap_or_default(), db.note_content(&id).unwrap().unwrap())).collect();
        assert_eq!(rows[0], ("Alpha".into(), "Open".into(), "<p>first</p>".into()));
        assert_eq!(rows[1], ("Alpha".into(), "Closed".into(), "<p>second</p>".into()));
        assert_eq!(rows[2].2, "<p>Note: this is the first body line</p><p>rest</p>", "a body line is not a property");
    }

    fn kids_titles(db: &Db, p: &str) -> Vec<String> { let mut v: Vec<String> = kids(db, p).into_iter().map(|k| k.1).collect(); v.sort(); v }

    #[test]
    fn wrong_export_shapes_are_explained() {
        let (_d, db) = fixture();
        let err = db.import_notion(vec![e("Page.md", "# hi")], "root").unwrap_err().to_string();
        assert!(err.contains("no Notion ids"), "{err}");
        let flat = vec![e(&format!("A {ID_A}.html"), &page(ID_A, "A", &format!("<figure class=\"link-to-page\"><a href=\"B%20{ID_B}.html\">B</a></figure>"), "")), e(&format!("B {ID_B}.html"), &page(ID_B, "B", "", ""))];
        assert!(db.import_notion(flat, "root").unwrap_err().to_string().contains("subpage folders"));
        assert!(db.import_notion(vec![e("x.html", "<html")], "nope").is_err());
        // hostile pages never panic and never keep scripts
        let evil = page(ID_A, "Evil", "<script>alert(1)</script><p onclick=\"x()\">a</p>", "");
        let st = db.import_notion(vec![e(&format!("Evil {ID_A}.html"), &evil), e("weird.html", "<<<<>>>"), e(&format!("é {ID_B}.html"), "<body>\u{0}")], "root").unwrap();
        let n = child(&db, &st.root.unwrap(), "Evil");
        let c = db.note_content(&n).unwrap().unwrap();
        assert!(!c.contains("script") && !c.contains("onclick"), "{c}");
    }
}
