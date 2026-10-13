//! OPML (import/export) and Evernote ENEX (import), ported from Trilium's `import/opml.ts`, `import/enex.ts`
//! and `import/enex_converter.ts`. Imported HTML is passed through `sanitize_html` like Trilium does.
use crate::{
    db::Db,
    htmltree::{self, El, Node, Token, decode_entities, escape_attr, escape_text, parse, sanitize_html, serialize},
    transfer::ImportStats,
    write::*,
};
use anyhow::{Context, Result, bail};
use base64::Engine;
use md5::{Digest, Md5};
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex { cell.get_or_init(|| Regex::new(pat).expect("static regex")) }

fn attr_of<'a>(attrs: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
}

fn attr_dec(attrs: &[(String, Option<String>)], name: &str) -> Option<String> { attr_of(attrs, name).map(decode_entities) }

impl Db {
    fn require_note(&self, id: &str) -> Result<()> {
        let e: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [id], |r| r.get(0)).optional()?;
        if e.is_none() { bail!("import target {id} not found"); }
        Ok(())
    }

    // ------------------------------------------------------------------ OPML

    /// Import an OPML 1.0/1.1/2.0 outline under `parent_id`. Top-level outlines become children of the target.
    pub fn import_opml(&self, xml: &str, parent_id: &str) -> Result<ImportStats> {
        self.require_note(parent_id)?;
        let toks = htmltree::tokenize(xml, false);
        let version = toks.iter().find_map(|t| match t {
            Token::Open { name, attrs, .. } if name == "opml" => Some(attr_dec(attrs, "version").unwrap_or_default()),
            _ => None,
        }).context("not an OPML file (no <opml> element)")?;
        if !["1.0", "1.1", "2.0"].contains(&version.as_str()) {
            bail!("Unsupported OPML version {version}, 1.0, 1.1 or 2.0 expected instead.");
        }
        let v2 = version.starts_with('2');
        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let mut stack = vec![parent_id.to_string()];
        let mut touched: HashSet<String> = HashSet::new();
        let mut in_body = false;
        for t in toks {
            match t {
                Token::Open { name, .. } if name == "body" => in_body = true,
                Token::Close(name) if name == "body" => in_body = false,
                Token::Open { name, attrs, self_close } if name == "outline" && in_body => {
                    let text = attr_dec(&attrs, "text").unwrap_or_default();
                    let text = text.as_str();
                    let (title, content) = if v2 {
                        (text.to_string(), attr_dec(&attrs, "_note").unwrap_or_default())
                    } else {
                        let title = attr_dec(&attrs, "title").unwrap_or_default();
                        if title.trim().is_empty() { (text.to_string(), String::new()) } else { (title.clone(), opml_text_to_html(text)) }
                    };
                    let title = if title.trim().is_empty() { "Untitled".to_string() } else { title };
                    let parent = stack.last().cloned().unwrap_or_else(|| parent_id.to_string());
                    let id = self.create_note(&parent, &title, "text", "text/html", &sanitize_html(&content))?;
                    stats.notes += 1;
                    stats.root.get_or_insert_with(|| id.clone());
                    touched.insert(parent);
                    if !self_close { if stack.len() > 100 { bail!("the outline is nested more than 100 levels deep"); } stack.push(id); }
                }
                Token::Close(name) if name == "outline" && in_body && stack.len() > 1 => { stack.pop(); }
                _ => {}
            }
        }
        for p in &touched { self.sort_children_if_needed(p)?; }
        tx.commit()?;
        Ok(stats)
    }

    /// Write the OPML export to `path` (via a hidden `.part` file; removed on failure). Returns the number of notes.
    pub fn export_opml_file(&self, note_id: &str, path: &std::path::Path) -> Result<usize> {
        let xml = self.export_opml(note_id)?;
        let part = path.with_file_name(format!(".{}.part", path.file_name().and_then(|n| n.to_str()).unwrap_or("export")));
        let r = std::fs::write(&part, &xml).and_then(|_| std::fs::rename(&part, path)).with_context(|| format!("writing {}", path.display()));
        if r.is_err() { let _ = std::fs::remove_file(&part); }
        r.map(|_| xml.matches("<outline ").count())
    }

    /// OPML 2.0 export of a note and its subtree (`_note` carries each note's HTML), like Trilium's.
    pub fn export_opml(&self, note_id: &str) -> Result<String> {
        self.require_note(note_id)?;
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">\n<head>\n<title>Trilium export</title>\n</head>\n<body>");
        let prefix: Option<String> = self.conn.query_row(
            "SELECT prefix FROM branches WHERE noteId=? AND isDeleted=0 ORDER BY notePosition LIMIT 1", [note_id], |r| r.get(0)).optional()?.flatten();
        self.opml_outline(note_id, prefix.as_deref(), 0, &mut out)?;
        out.push_str("</body>\n</opml>");
        Ok(out)
    }

    fn opml_outline(&self, note_id: &str, prefix: Option<&str>, depth: usize, out: &mut String) -> Result<()> {
        if depth > 200 { bail!("the tree is nested more than 200 levels deep; it can't be exported as OPML"); }
        if self.label_value(note_id, "excludeFromExport")?.is_some() { return Ok(()); }
        let (title, ntype, mime, protected): (String, String, String, i64) = self.conn.query_row(
            "SELECT title,type,mime,isProtected FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let title = if protected != 0 { "[protected]".to_string() } else { title };
        let title = match prefix.filter(|p| !p.is_empty()) { Some(p) => format!("{p} - {title}"), None => title };
        let content = if protected == 0 && is_string_note(&ntype, &mime) { self.note_content(note_id)?.unwrap_or_default() } else { String::new() };
        out.push_str(&format!("<outline text=\"{}\" _note=\"{}\">\n", xml_attr(&title), xml_attr(&content)));
        let mut st = self.conn.prepare("SELECT noteId,prefix FROM branches WHERE parentNoteId=? AND isDeleted=0 ORDER BY notePosition")?;
        let kids: Vec<(String, Option<String>)> = st.query_map([note_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (k, p) in kids {
            let alive: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [&k], |r| r.get(0)).optional()?;
            if alive.is_some() { self.opml_outline(&k, p.as_deref(), depth + 1, out)?; }
        }
        out.push_str("</outline>");
        Ok(())
    }

    // ------------------------------------------------------------------ ENEX

    /// Import an Evernote export. All notes go under a new note named `root_title` (the file name without `.enex`).
    pub fn import_enex(&self, text: &str, root_title: &str, parent_id: &str) -> Result<ImportStats> {
        self.require_note(parent_id)?;
        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let root = self.create_note(parent_id, root_title, "text", "text/html", "")?;
        stats.notes += 1;
        stats.root = Some(root.clone());

        let mut path: Vec<String> = vec![];
        let mut note: Option<EnexNote> = None;
        let mut by_title: HashMap<String, Option<String>> = HashMap::new();
        let mut created: Vec<(String, String, String, String)> = vec![]; // id, content, created, modified

        for tok in htmltree::tokenize(text, false) {
            match tok {
                Token::Open { name, attrs: _, self_close } => {
                    match name.as_str() {
                        "note" => note = Some(EnexNote::default()),
                        "resource" => if let Some(n) = note.as_mut() { n.resources.push(EnexResource { title: "resource".into(), ..Default::default() }); },
                        "task" => if let Some(n) = note.as_mut() { n.tasks.push(EnexTask::default()); },
                        _ => {}
                    }
                    if !self_close { path.push(name); }
                }
                Token::Close(name) => {
                    // tolerate stray closers: only pop when it matches
                    if path.last() == Some(&name) { path.pop(); }
                    else if let Some(pos) = path.iter().rposition(|p| *p == name) { path.truncate(pos); }
                    if name == "note" {
                        if let Some(n) = note.take() {
                            let id = self.save_enex_note(n, &root, &mut stats)?;
                            by_title.entry(id.3.trim().to_string()).and_modify(|v| *v = None).or_insert(Some(id.0.clone()));
                            created.push((id.0, id.1, id.2.0, id.2.1));
                        }
                    }
                }
                Token::CData(t) => if let (Some(n), Some("content")) = (note.as_mut(), path.last().map(String::as_str)) { n.content.push_str(&t); },
                Token::Text(t) => {
                    let Some(n) = note.as_mut() else { continue };
                    let t = decode_entities(&t);
                    let cur = path.last().map(String::as_str).unwrap_or("");
                    let prev = if path.len() >= 2 { path[path.len() - 2].as_str() } else { "" };
                    match prev {
                        "note-attributes" => {
                            let name = if cur == "source-url" { "pageUrl" } else { cur };
                            n.attributes.push((sanitize_attribute_name(name), t));
                        }
                        "resource-attributes" => if let Some(r) = n.resources.last_mut() {
                            if cur == "file-name" { r.attributes.push(("originalFileName".into(), t.clone())); r.title = t; }
                            else if cur == "source-url" { r.attributes.push(("pageUrl".into(), t)); }
                        },
                        "resource" => if let Some(r) = n.resources.last_mut() {
                            if cur == "data" { r.data.extend(t.chars().filter(|c| !c.is_whitespace())); }
                            else if cur == "mime" { r.mime = Some(t.to_lowercase()); }
                        },
                        "note" => match cur {
                            "title" => n.title = Some(t),
                            "created" => n.created = Some(enex_date(&t)),
                            "updated" => n.updated = Some(enex_date(&t)),
                            "tag" => n.attributes.push((sanitize_attribute_name(&t), String::new())),
                            _ => {}
                        },
                        "task" => if let Some(task) = n.tasks.last_mut() {
                            match cur { "title" => task.title.push_str(&t), "taskStatus" => task.status.push_str(&t), "taskGroupNoteLevelID" => task.group_id.push_str(&t), _ => {} }
                        },
                        _ => {}
                    }
                }
            }
        }
        // second pass: Evernote internal links, now that every title is known
        for (id, content, c, m) in &created {
            let rewritten = rewrite_evernote_links(content, &|t| by_title.get(t).cloned().flatten());
            if &rewritten != content {
                self.write_imported_content(id, &rewritten, c, m)?;
            }
        }
        self.sort_children_if_needed(&root)?;
        self.sort_children_if_needed(parent_id)?;
        tx.commit()?;
        Ok(stats)
    }

    /// Returns (note id, final content, (created, modified), title).
    fn save_enex_note(&self, n: EnexNote, root: &str, stats: &mut ImportStats) -> Result<(String, String, (String, String), String)> {
        let (Some(title), true) = (n.title.clone(), !n.content.is_empty()) else { bail!("Missing title or content for note.") };
        let mut content = extract_enex_content(&n.content, &n.tasks);
        let id = self.create_note(root, &title, "text", "text/html", "")?;
        stats.notes += 1;
        for (name, value) in &n.attributes {
            if name.is_empty() { continue; }
            self.add_attribute(&id, "label", name, value, false)?;
            stats.attributes += 1;
        }
        let created = n.created.clone().unwrap_or_else(now_utc);
        let modified = n.updated.clone().unwrap_or_else(|| created.clone());
        for r in &n.resources {
            if r.data.is_empty() { continue; }
            let Some(bytes) = decode_b64(&r.data) else { continue };
            let hash = Md5::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
            if ONENOTE_CHECKBOX_HASHES.contains(&hash.as_str()) { continue; }
            let media = Regex::new(&format!(r#"<en-media [^>]*hash="{hash}"[^>]*>"#)).expect("hash is hex");
            let mime = r.mime.clone().unwrap_or_else(|| "application/octet-stream".into());
            let mut as_file = || -> Result<()> {
                let att = self.add_attachment(&id, &r.title, &mime, "file", &bytes)?;
                let link = format!(r##"<a class="reference-link" href="#root/{id}?viewMode=attachments&amp;attachmentId={att}">{}</a>"##, escape_text(&r.title));
                content = media.replace_all(&content, regex::NoExpand(&link)).into_owned();
                stats.attachments += 1;
                Ok(())
            };
            if mime.starts_with("image/") {
                let name = if !r.title.is_empty() && r.title != "resource" { r.title.clone() } else { format!("image.{}", &mime[6..]) };
                match self.add_attachment(&id, &name, &mime, "image", &bytes) {
                    Ok(att) => {
                        let link = format!(r#"<img src="api/attachments/{att}/image/{}">"#, encode_component(&name));
                        content = media.replace_all(&content, regex::NoExpand(&link)).into_owned();
                        if !content.contains(&link) { content.push_str(&link); }
                        stats.attachments += 1;
                    }
                    Err(_) => as_file()?,
                }
            } else { as_file()?; }
        }
        let content = sanitize_html(&strip_leftover_enml(&content));
        self.write_imported_content(&id, &content, &created, &modified)?;
        Ok((id, content, (created, modified), title))
    }

    /// Store `content` for a just-created note without a revision, and stamp the original dates.
    fn write_imported_content(&self, id: &str, content: &str, created_utc: &str, modified_utc: &str) -> Result<()> {
        self.store_imported_html(id, content, Some((created_utc, modified_utc))).map(|_| ())
    }

    /// Store `html` as a text note's content (link relations kept in sync, no revision), optionally stamping dates.
    pub(crate) fn store_imported_html(&self, id: &str, html: &str, dates: Option<(&str, &str)>) -> Result<String> {
        let content = self.save_links(id, html, false)?;
        let blob = self.save_blob_for_note(id, content.as_bytes(), true)?;
        let old: Option<String> = self.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [id], |r| r.get(0)).optional()?.flatten();
        match dates {
            Some((c, m)) => self.conn.execute(
                "UPDATE notes SET blobId=?, dateCreated=?, utcDateCreated=?, dateModified=?, utcDateModified=? WHERE noteId=?",
                params![blob, utc_to_local(c), c, utc_to_local(m), m, id])?,
            None => self.conn.execute("UPDATE notes SET blobId=?, dateModified=?, utcDateModified=? WHERE noteId=?", params![blob, now_local(), now_utc(), id])?,
        };
        self.record_entity("notes", "noteId", id, "utcDateModified")?;
        if let Some(o) = old.filter(|o| *o != blob) { self.drop_blob_if_unused(&o)?; }
        Ok(content)
    }
}

const ONENOTE_CHECKBOX_HASHES: [&str; 6] = [
    "74de5d3d1286f01bac98d32a09f601d9", "4a19d3041585e11643e808d68dd3e72f", "8e17580123099ac6515c3634b1f6f9a1",
    "5069b775461e471a47ce04ace6e1c6ae", "7912ee9cec35fc3dba49edb63a9ed158", "3a05f4f006a6eaf2627dae5ed8b8013b",
];

fn opml_text_to_html(text: &str) -> String {
    if text.is_empty() { return String::new(); }
    static NL: OnceLock<Regex> = OnceLock::new();
    format!("<p>{}</p>", re(&NL, r"\r\n|\r|\n").replace_all(&escape_text(text), "</p><p>"))
}

fn xml_attr(s: &str) -> String {
    // newlines and tabs in an attribute would be normalised to spaces by a strict XML parser: write them as references
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;").replace('\n', "&#10;").replace('\r', "&#13;").replace('\t', "&#9;")
}

pub(crate) fn sanitize_attribute_name(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == ':' { c } else { '_' }).collect()
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let e = base64::engine::general_purpose::STANDARD;
    let lenient = base64::engine::general_purpose::STANDARD_NO_PAD;
    e.decode(s).ok().or_else(|| lenient.decode(s.trim_end_matches('=')).ok())
}

/// `20181121T193703Z` or `2013-04-14T16:19:00.000Z` → `2018-11-21 19:37:03.000Z`.
fn enex_date(s: &str) -> String {
    let d: String = s.trim().chars().filter(|c| *c != '-' && *c != ':').collect();
    let ok = d.len() >= 15 && d.is_ascii() && d.as_bytes()[8] == b'T' && d[..8].bytes().all(|b| b.is_ascii_digit()) && d[9..15].bytes().all(|b| b.is_ascii_digit());
    if ok {
        let t = format!("{}-{}-{} {}:{}:{}.000Z", &d[0..4], &d[4..6], &d[6..8], &d[9..11], &d[11..13], &d[13..15]);
        if chrono::NaiveDateTime::parse_from_str(t.trim_end_matches('Z'), "%Y-%m-%d %H:%M:%S%.f").is_ok() { return t; }
    }
    now_utc()
}

fn utc_to_local(utc: &str) -> String {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(utc.trim_end_matches('Z'), "%Y-%m-%d %H:%M:%S%.f").ok()
        .map(|n| chrono::Utc.from_utc_datetime(&n).with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S%.3f%z").to_string())
        .unwrap_or_else(now_local)
}

#[derive(Default)]
struct EnexResource { title: String, data: String, mime: Option<String>, attributes: Vec<(String, String)> }

#[derive(Default, Clone)]
pub struct EnexTask { pub title: String, pub status: String, pub group_id: String }

#[derive(Default)]
struct EnexNote {
    title: Option<String>,
    content: String,
    attributes: Vec<(String, String)>,
    created: Option<String>,
    updated: Option<String>,
    resources: Vec<EnexResource>,
    tasks: Vec<EnexTask>,
}

// ---------------------------------------------------------------- ENML → HTML

fn extract_enex_content(content: &str, tasks: &[EnexTask]) -> String {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static TODO_OFF: OnceLock<Regex> = OnceLock::new();
    static TODO_ON: OnceLock<Regex> = OnceLock::new();
    let mut c = content.to_string();
    if let Some(m) = re(&OPEN, r"<en-note[^>]*>").find(&c) { c = c[m.end()..].to_string(); }
    if let Some(i) = c.rfind("</en-note>") { c.truncate(i); }
    let mut c = c.trim().to_string();
    // OneNote's converted checkboxes carry a known hash
    let tail = r#"( type="[a-z/]*"| width="\d+"| height="\d+")*/>"#;
    c = re(&TODO_OFF, &format!(r#"<en-media alt="To Do( priority [12])?" hash="(74de5d3d1286f01bac98d32a09f601d9|4a19d3041585e11643e808d68dd3e72f|8e17580123099ac6515c3634b1f6f9a1)"{tail}"#)).replace_all(&c, "\u{2610} ").into_owned();
    c = re(&TODO_ON, &format!(r#"<en-media alt="To Do( priority [12])?" hash="(5069b775461e471a47ce04ace6e1c6ae|7912ee9cec35fc3dba49edb63a9ed158|3a05f4f006a6eaf2627dae5ed8b8013b)"{tail}"#)).replace_all(&c, "\u{2611} ").into_owned();
    let mut c = convert_enex_content(&c, tasks);
    // the list-shape workarounds Trilium applies for CKEditor
    for (pat, rep) in [
        (r"<li>\s*<div>", "<li>"), (r"</div>\s*</li>", "</li>"),
        (r"<ul>\s*<ul>", "<ul><li><ul>"), (r"</li>\s*<ul>", "<ul>"), (r"</ul>\s*</ul>", "</ul></li></ul>"), (r"</ul>\s*<li>", "</ul></li><li>"),
        (r"<ol>\s*<ol>", "<ol><li><ol>"), (r"</li>\s*<ol>", "<ol>"), (r"</ol>\s*</ol>", "</ol></li></ol>"), (r"</ol>\s*<li>", "</ol></li><li>"),
    ] {
        c = Regex::new(pat).expect("static regex").replace_all(&c, rep).into_owned();
    }
    sanitize_html(&c)
}

/// Whatever Evernote markup is still present after resources were substituted.
fn strip_leftover_enml(html: &str) -> String {
    static MEDIA: OnceLock<Regex> = OnceLock::new();
    re(&MEDIA, r"</?en-(media|crypt)\b[^>]*>").replace_all(html, "").into_owned()
}

fn style_has(el: &El, name: &str) -> bool {
    el.attr_dec("style").is_some_and(|s| Regex::new(&format!(r"{}\s*:", regex::escape(name))).map(|r| r.is_match(&s)).unwrap_or(false))
}

fn style_var(el: &El, name: &str) -> Option<String> {
    let s = el.attr_dec("style")?;
    Regex::new(&format!(r"{}\s*:\s*([^;]+)", regex::escape(name))).ok()?.captures(&s).map(|c| c[1].trim().to_string())
}

const BLOCK_TAGS: [&str; 22] = ["div", "p", "ul", "ol", "li", "table", "thead", "tbody", "tr", "td", "th", "figure", "pre", "blockquote", "aside", "details", "summary", "h1", "h2", "h3", "h4", "h5"];

fn todo_item(checked: bool, desc: &str, nested: &str) -> String {
    let a = if checked { r#" checked="checked" disabled="disabled""# } else { r#" disabled="disabled""# };
    format!(r#"<li><label class="todo-list__label"><input type="checkbox"{a}><span class="todo-list__label__description">{desc}</span></label>{nested}</li>"#)
}

fn todo_list(items: &[String]) -> String { format!(r#"<ul class="todo-list">{}</ul>"#, items.concat()) }

fn is_en_todo_ul(n: &El) -> bool { n.name == "ul" && style_has(n, "--en-todo") }

fn is_en_todo_div(e: &El) -> bool {
    if e.name != "div" { return false; }
    for k in &e.kids {
        match k {
            Node::El(c) => return c.name == "en-todo",
            Node::Text(t) if !t.trim().is_empty() => return false,
            _ => {}
        }
    }
    false
}

fn build_todo_list(ul: &El) -> String {
    struct Item { checked: bool, desc: String, nested: String }
    let mut items: Vec<Item> = vec![];
    for c in ul.child_els() {
        if c.name == "li" {
            let mut nested = String::new();
            for inner in c.child_els().filter(|e| is_en_todo_ul(e)) { nested.push_str(&build_todo_list(inner)); }
            items.push(Item { checked: style_var(c, "--en-checked").as_deref() == Some("true"), desc: item_description(c), nested });
        } else if is_en_todo_ul(c) {
            let nested = build_todo_list(c);
            match items.last_mut() { Some(p) => p.nested.push_str(&nested), None => items.push(Item { checked: false, desc: String::new(), nested }) }
        }
    }
    todo_list(&items.iter().map(|i| todo_item(i.checked, &i.desc, &i.nested)).collect::<Vec<_>>())
}

fn item_description(li: &El) -> String {
    let els: Vec<&El> = li.child_els().filter(|e| !is_en_todo_ul(e)).collect();
    if els.len() == 1 && els[0].name == "div" { return els[0].inner_html().trim().to_string(); }
    let rest: Vec<Node> = li.kids.iter().filter(|n| !matches!(n, Node::El(e) if is_en_todo_ul(e))).cloned().collect();
    serialize(&rest).trim().to_string()
}

fn block_text(div: &El) -> String {
    let lines: Vec<&El> = div.child_els().filter(|e| e.name == "div").collect();
    if lines.is_empty() { return div.text(); }
    lines.iter().map(|l| { let t = l.text(); if t.trim().is_empty() { String::new() } else { t.trim_end_matches('\n').to_string() } }).collect::<Vec<_>>().join("\n")
}

fn escape_code(s: &str) -> String { escape_text(s) }

pub(crate) const LANGS: &[(&str, &str)] = &[
    ("plaintext", "text/plain"), ("c", "text/x-csrc"), ("clike", "text/x-csrc"), ("csharp", "text/x-csharp"), ("cpp", "text/x-c++src"),
    ("css", "text/css"), ("diff", "text/x-diff"), ("dockerfile", "text/x-dockerfile"), ("go", "text/x-go"), ("groovy", "text/x-groovy"),
    ("haskell", "text/x-haskell"), ("html", "text/html"), ("java", "text/x-java"), ("javascript", "text/javascript"), ("js", "text/javascript"),
    ("json", "application/json"), ("jsonc", "application/json"), ("jsx", "text/jsx"), ("kotlin", "text/x-kotlin"), ("latex", "text/x-latex"),
    ("lua", "text/x-lua"), ("markdown", "text/x-gfm"), ("md", "text/x-gfm"), ("objectivec", "text/x-objectivec"), ("perl", "text/x-perl"), ("php", "text/x-php"),
    ("powershell", "application/x-powershell"), ("python", "text/x-python"), ("py", "text/x-python"), ("ruby", "text/x-ruby"), ("rust", "text/x-rustsrc"),
    ("scss", "text/x-scss"), ("sh", "text/x-sh"), ("bash", "text/x-sh"), ("shell", "text/x-sh"), ("zsh", "text/x-sh"), ("sql", "text/x-sql"),
    ("swift", "text/x-swift"), ("toml", "text/x-toml"), ("typescript", "application/typescript"), ("ts", "application/typescript"),
    ("xml", "text/xml"), ("yaml", "text/x-yaml"), ("yml", "text/x-yaml"),
];

pub(crate) fn code_language(lang: Option<&str>) -> String {
    let Some(l) = lang.filter(|l| !l.is_empty()) else { return "text-x-trilium-auto".into() };
    if l == "mermaid" { return "mermaid".into(); }
    match LANGS.iter().find(|(n, _)| *n == l.to_lowercase()) {
        Some((_, mime)) => Regex::new(r"[\W_]+").expect("static regex").replace_all(&mime.to_lowercase(), "-").into_owned(),
        None => "text-x-trilium-auto".into(),
    }
}

fn nodes_of(html: &str) -> Vec<Node> { parse(html) }

/// Evernote's marker-styled blocks → Trilium/CKEditor markup.
pub fn convert_enex_content(html: &str, tasks: &[EnexTask]) -> String {
    let mut used: HashSet<usize> = HashSet::new();
    let nodes = conv_nodes(parse(html), "", tasks, &mut used);
    let out = serialize(&nodes);
    // any checkbox that is not at the start of a line falls back to a ballot box
    static TODO: OnceLock<Regex> = OnceLock::new();
    re(&TODO, r"<en-todo\b[^>]*>").replace_all(&out, |c: &regex::Captures| if c[0].contains(r#"checked="true""#) { "\u{2611} " } else { "\u{2610} " }).into_owned()
}

fn conv_nodes(nodes: Vec<Node>, parent: &str, tasks: &[EnexTask], used: &mut HashSet<usize>) -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    for n in nodes {
        match n {
            Node::Text(_) => out.push(n),
            Node::El(e) => out.extend(conv_el(e, parent, tasks, used)),
        }
    }
    group_todo_runs(out)
}

fn group_todo_runs(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    let mut run: Vec<El> = vec![];
    let mut pending_ws: Vec<Node> = vec![];
    let flush = |run: &mut Vec<El>, out: &mut Vec<Node>| {
        if run.is_empty() { return; }
        let items: Vec<String> = run.iter().map(|d| {
            let cb = d.child_els().find(|c| c.name == "en-todo");
            let checked = cb.is_some_and(|c| c.attr_dec("checked").as_deref() == Some("true"));
            let rest: Vec<Node> = d.kids.iter().filter(|k| !matches!(k, Node::El(c) if c.name == "en-todo")).cloned().collect();
            todo_item(checked, serialize(&rest).trim(), "")
        }).collect();
        out.extend(nodes_of(&todo_list(&items)));
        run.clear();
    };
    for n in nodes {
        match n {
            Node::El(e) if is_en_todo_div(&e) => { pending_ws.clear(); run.push(e); }
            Node::Text(t) if t.trim().is_empty() && !run.is_empty() => pending_ws.push(Node::Text(t)),
            other => {
                flush(&mut run, &mut out);
                out.append(&mut pending_ws);
                out.push(other);
            }
        }
    }
    flush(&mut run, &mut out);
    out.append(&mut pending_ws);
    out
}

fn conv_el(mut e: El, parent: &str, tasks: &[EnexTask], used: &mut HashSet<usize>) -> Vec<Node> {
    if e.name == "div" && style_has(&e, "--en-task-group") {
        let gid = style_var(&e, "--en-id");
        let mut items = vec![];
        for (i, t) in tasks.iter().enumerate() {
            if used.contains(&i) { continue; }
            if gid.as_ref().is_none_or(|g| *g == t.group_id) { used.insert(i); items.push(todo_item(t.status == "completed", &escape_text(t.title.trim()), "")); }
        }
        return if items.is_empty() { vec![] } else { nodes_of(&todo_list(&items)) };
    }
    if is_en_todo_ul(&e) && parent != "en-todo-ul" { return nodes_of(&build_todo_list(&e)); }
    if e.name == "div" {
        if style_has(&e, "--en-codeblock") {
            let lang = code_language(style_var(&e, "--en-syntaxLanguage").as_deref());
            return nodes_of(&format!(r#"<pre><code class="language-{lang}">{}</code></pre>"#, escape_code(&block_text(&e))));
        }
        if style_has(&e, "--en-mermaidblock") {
            return nodes_of(&format!(r#"<pre><code class="language-mermaid">{}</code></pre>"#, escape_code(&block_text(&e))));
        }
        if style_has(&e, "--en-formulablock") {
            let f = block_text(&e);
            return if f.trim().is_empty() { vec![] } else { nodes_of(&format!(r#"<span class="math-tex">\[{}\]</span>"#, escape_text(f.trim()))) };
        }
        if style_has(&e, "--en-toggle") {
            let summary_el = e.child_els().find(|c| style_has(c, "--en-toggleSummary")).cloned();
            let content_el = e.child_els().find(|c| style_has(c, "--en-toggleContent")).cloned();
            let summary = summary_el.map(|s| serialize(&conv_nodes(s.kids, "div", tasks, used)).trim().to_string()).unwrap_or_default();
            let body = content_el.map(|c| {
                let kids = conv_nodes(c.kids, "div", tasks, used);
                serialize(&kids.into_iter().filter(non_empty_block).collect::<Vec<_>>())
            }).unwrap_or_default();
            let open = if style_var(&e, "--en-isCollapsed").as_deref() == Some("true") { "" } else { " open" };
            return nodes_of(&format!(r#"<details class="trilium-collapsible"{open}><summary>{summary}</summary>{body}</details>"#));
        }
        if style_has(&e, "--en-callout") {
            let emoji = style_var(&e, "--en-emoji");
            let tip = emoji.as_deref() == Some("\u{1f4a1}");
            let mut kids = conv_nodes(std::mem::take(&mut e.kids), "div", tasks, used);
            if let (false, Some(em)) = (tip, emoji.as_ref().filter(|s| !s.is_empty())) {
                let first = kids.iter().position(|n| matches!(n, Node::El(_)));
                match first {
                    Some(i) => match &mut kids[i] {
                        Node::El(f) if f.name == "p" || f.name == "div" => f.kids.insert(0, Node::Text(format!("{} ", escape_text(em)))),
                        _ => kids.insert(0, Node::Text(format!("{} ", escape_text(em)))),
                    },
                    None => kids.insert(0, Node::Text(format!("{} ", escape_text(em)))),
                }
            }
            let cls = if tip { "admonition tip" } else { "admonition note" };
            return vec![Node::El(El::new("aside", &[("class", cls)], kids))];
        }
    }
    let name = e.name.clone();
    let child_parent = if is_en_todo_ul(&e) { "en-todo-ul".to_string() } else { name.clone() };
    e.kids = conv_nodes(std::mem::take(&mut e.kids), &child_parent, tasks, used);
    if name == "div" && parent != "li" && !is_en_todo_div(&e) {
        let has_block = e.child_els().any(|c| BLOCK_TAGS.contains(&c.name.as_str()) || c.name == "hr");
        if has_block { return vec![Node::El(e)]; }
        let blank = e.text().trim().is_empty() && !has_descendant(&e, &["img", "en-media"]);
        if blank { return nodes_of("<p>&nbsp;</p>"); }
        e.name = "p".into();
    }
    vec![Node::El(e)]
}

fn has_descendant(e: &El, names: &[&str]) -> bool {
    e.child_els().any(|c| names.contains(&c.name.as_str()) || has_descendant(c, names))
}

fn non_empty_block(n: &Node) -> bool {
    match n {
        Node::Text(t) => !decode_entities(t).trim().is_empty(),
        Node::El(e) => !e.text().trim().is_empty() || has_descendant(e, &["img", "en-media"]) || e.name == "img",
    }
}

fn rewrite_evernote_links(html: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    if !html.contains("evernote:") { return html.to_string(); }
    static A: OnceLock<Regex> = OnceLock::new();
    static HREF: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    re(&A, r#"(?s)<a\b([^>]*\bhref="evernote:[^"]*"[^>]*)>(.*?)</a>"#).replace_all(html, |c: &regex::Captures| {
        let text = decode_entities(&Regex::new(r"<[^>]*>").expect("static regex").replace_all(&c[2], ""));
        match resolve(text.trim()) {
            Some(id) => {
                let attrs = re(&HREF, r#"href="evernote:[^"]*""#).replace(&c[1], regex::NoExpand(&format!("href=\"#root/{id}\""))).into_owned();
                let attrs = re(&CLASS, r#"\sclass="[^"]*""#).replace(&attrs, "").into_owned();
                format!(r#"<a{attrs} class="reference-link">{}</a>"#, &c[2])
            }
            None => c[0].to_string(),
        }
    }).into_owned()
}

#[allow(dead_code)]
fn unused(_: &str) -> String { escape_attr("") }

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("f.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn children(db: &Db, parent: &str) -> Vec<(String, String)> {
        let mut st = db.conn.prepare("SELECT n.noteId,n.title FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0 ORDER BY b.notePosition").unwrap();
        st.query_map([parent], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    fn content(db: &Db, id: &str) -> String { db.note_content(id).unwrap().unwrap() }
    fn all_ok(db: &Db) { for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); } }

    #[test]
    fn opml_two_versions_import_and_export_round_trips() {
        let (_d, db) = fixture();
        let v2 = r#"<?xml version="1.0"?><opml version="2.0"><head><title>x</title></head><body>
            <outline text="Parent &amp; co" _note="&lt;p&gt;hello &lt;b&gt;there&lt;/b&gt;&lt;/p&gt;&lt;script&gt;x()&lt;/script&gt;">
              <outline text="Child"/>
            </outline><outline text="Second"></outline></body></opml>"#;
        let s = db.import_opml(v2, "root").unwrap();
        assert_eq!(s.notes, 3);
        let top = children(&db, "root");
        assert_eq!(top.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), ["Parent & co", "Second"]);
        assert_eq!(content(&db, &top[0].0), "<p>hello <b>there</b></p>", "script removed");
        assert_eq!(children(&db, &top[0].0)[0].1, "Child");
        // v1: title + text lines → paragraphs; without a title the text is the title
        let v1 = r#"<opml version="1.0"><body><outline title="T1" text="line one&#10;line &lt;two&gt;"/><outline text="only text"/></body></opml>"#;
        let s = db.import_opml(v1, "root").unwrap();
        assert_eq!(s.notes, 2);
        let kids = children(&db, "root");
        let t1 = kids.iter().find(|c| c.1 == "T1").unwrap();
        assert_eq!(content(&db, &t1.0), "<p>line one</p><p>line &lt;two&gt;</p>");
        assert_eq!(content(&db, &kids.iter().find(|c| c.1 == "only text").unwrap().0), "");
        assert!(db.import_opml(r#"<opml version="3.0"><body/></opml>"#, "root").unwrap_err().to_string().contains("Unsupported OPML version"));
        assert!(db.import_opml("plain text", "root").is_err());
        // export → import gives the same tree
        let xml = db.export_opml(&top[0].0).unwrap();
        assert!(xml.contains("<opml version=\"2.0\">") && xml.contains("Parent &amp; co"));
        let s = db.import_opml(&xml, "root").unwrap();
        assert_eq!(s.notes, 2, "the parent and its child");
        all_ok(&db);
    }

    const ENEX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260101T000000Z" application="Evernote" version="11">
  <note>
    <title>First &amp; best</title>
    <created>20200102T030405Z</created>
    <updated>2021-03-04T05:06:07.000Z</updated>
    <tag>important</tag>
    <note-attributes><source-url>https://example.com/page</source-url></note-attributes>
    <content><![CDATA[<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd"><en-note><div>Hello <b>world</b></div><div><br/></div>
<div><en-todo checked="false"/>Write tests</div><div><en-todo checked="true"/>Ship <i>it</i></div>
<div><en-media hash="5d41402abc4b2a76b9719d911017c592" type="text/plain"/></div>
<div style="--en-codeblock:true; --en-syntaxLanguage:c;"><div>void main() {</div><div><br/></div><div>}</div></div>
<div style="--en-callout:true; --en-emoji:🤖;"><div>Robot says hi</div></div>
<a href="evernote:///view/1/s1/guid/guid/">Second</a><script>evil()</script></en-note>]]></content>
    <resource><data encoding="base64">aGVs
bG8=</data><mime>text/plain</mime><resource-attributes><file-name>hello.txt</file-name></resource-attributes></resource>
  </note>
  <note><title>Second</title><content><![CDATA[<en-note><div style="--en-toggle:true; --en-isCollapsed:false"><div style="--en-toggleSummary:true">Sum</div><div style="--en-toggleContent:true"><div>Body</div><div><br/></div></div></div><ul style="--en-todo:true;"><li style="--en-checked:true"><div>One</div></li><ul style="--en-todo:true;"><li style="--en-checked:false"><div>Sub</div></li></ul></ul></en-note>]]></content></note>
</en-export>"#;

    #[test]
    fn enex_imports_notes_attachments_labels_dates_and_links() {
        let (_d, db) = fixture();
        let s = db.import_enex(ENEX, "My notebook", "root").unwrap();
        assert_eq!(s.notes, 3);
        let root = children(&db, "root");
        assert_eq!(root[0].1, "My notebook");
        let kids = children(&db, &root[0].0);
        let first = kids.iter().find(|c| c.1 == "First & best").unwrap().0.clone();
        let second = kids.iter().find(|c| c.1 == "Second").unwrap().0.clone();
        let html = content(&db, &first);
        for want in ["<p>Hello <b>world</b></p>", r#"<ul class="todo-list">"#, r#"<span class="todo-list__label__description">Write tests</span>"#,
            r#"checked="checked""#, r#"Ship <i>it</i>"#, r#"<pre><code class="language-text-x-csrc">void main() {"#, r#"<aside class="admonition note"><p>🤖 Robot says hi</p></aside>"#,
            &format!(r##"class="reference-link" href="#root/{first}?viewMode=attachments&amp;attachmentId="##), &format!(r##"href="#root/{second}""##)] {
            assert!(html.contains(want), "missing {want}\n{html}");
        }
        for bad in ["evil", "--en-", "evernote:", "<en-media", "en-todo"] { assert!(!html.contains(bad), "{bad}\n{html}"); }
        // attachment bytes, labels, dates
        let att: (String, String) = db.conn.query_row("SELECT title,mime FROM attachments WHERE ownerId=? AND isDeleted=0", [&first], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(att, ("hello.txt".into(), "text/plain".into()));
        let label = |n: &str| db.label_value(&first, n).unwrap();
        assert_eq!(label("pageUrl").as_deref(), Some("https://example.com/page"));
        assert!(db.has_label(&first, "important").unwrap());
        let (c, m): (String, String) = db.conn.query_row("SELECT utcDateCreated,utcDateModified FROM notes WHERE noteId=?", [&first], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((c.as_str(), m.as_str()), ("2020-01-02 03:04:05.000Z", "2021-03-04 05:06:07.000Z"));
        // the link became a relation
        let rel: i64 = db.conn.query_row("SELECT COUNT(*) FROM attributes WHERE noteId=? AND name='internalLink' AND value=? AND isDeleted=0", [&first, &second], |r| r.get(0)).unwrap();
        assert_eq!(rel, 1);
        // second note: toggle and nested todo list
        let h2 = content(&db, &second);
        assert!(h2.contains(r#"<details class="trilium-collapsible" open><summary>Sum</summary><p>Body</p></details>"#), "{h2}");
        assert!(h2.contains(r#"<span class="todo-list__label__description">One</span></label><ul class="todo-list">"#), "{h2}");
        all_ok(&db);
    }

    #[test]
    fn enex_edge_cases() {
        let (_d, db) = fixture();
        let e = db.import_enex(r#"<en-export><note><created>20200101T000000Z</created><content><![CDATA[<en-note><div>x</div></en-note>]]></content></note></en-export>"#, "t", "root").unwrap_err();
        assert!(e.to_string().contains("Missing title or content"));
        assert_eq!(children(&db, "root").len(), 0, "a failed import leaves nothing behind");
        // stray tags, no en-note wrapper, empty export, duplicated titles leave links alone
        let s = db.import_enex(r#"<en-export><note><title>Recovered</title><content><![CDATA[<div>No wrapper</div>]]></content></note></unopened></en-export>"#, "bad", "root").unwrap();
        assert_eq!(s.notes, 2);
        let s = db.import_enex("<en-export></en-export>", "empty", "root").unwrap();
        assert_eq!(s.notes, 1);
        let dup = r#"<en-export><note><title>A</title><content><![CDATA[<en-note><a href="evernote://view-note/x">B</a></en-note>]]></content></note>
            <note><title>B</title><content><![CDATA[<en-note>b1</en-note>]]></content></note><note><title>B</title><content><![CDATA[<en-note>b2</en-note>]]></content></note></en-export>"#;
        db.import_enex(dup, "dups", "root").unwrap();
        let root = children(&db, "root").into_iter().find(|c| c.1 == "dups").unwrap();
        let a = children(&db, &root.0).into_iter().find(|c| c.1 == "A").unwrap();
        assert!(content(&db, &a.0).contains("evernote://view-note/x"), "ambiguous link is left unresolved");
        all_ok(&db);
    }

    #[test]
    fn hostile_enex_dates_and_cdata() {
        for bad in ["202é010T0000000", "abcdefghTijklmnopq", "20201399T999999Z", "", "T", "2020"] {
            let d = enex_date(bad);
            assert!(chrono::NaiveDateTime::parse_from_str(d.trim_end_matches('Z'), "%Y-%m-%d %H:%M:%S%.f").is_ok(), "{bad} -> {d}");
        }
        assert_eq!(enex_date("2013-04-14T16:19:00.000Z"), "2013-04-14 16:19:00.000Z");
        let (_d, db) = fixture();
        // a CDATA that is not the note body (resource recognition data) must not leak into the content
        let x = r#"<en-export><note><title>T</title><resource><recognition><![CDATA[<recoIndex/>]]></recognition></resource><content><![CDATA[<en-note>body</en-note>]]></content></note></en-export>"#;
        let s = db.import_enex(x, "r", "root").unwrap();
        let root = s.root.unwrap();
        let kid: String = db.conn.query_row("SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0", [&root], |r| r.get(0)).unwrap();
        assert_eq!(content(&db, &kid), "body");
    }

    #[test]
    fn opml_entities_in_attributes_and_file_export() {
        let (d, db) = fixture();
        let x = r#"<opml version="2.0"><body><outline text="caf&#233; &amp; co &quot;x&quot;" _note="&lt;p title=&quot;a&amp;amp;b&quot;&gt;t&lt;/p&gt;"/></body></opml>"#;
        let s = db.import_opml(x, "root").unwrap();
        let id = s.root.unwrap();
        let t: String = db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [&id], |r| r.get(0)).unwrap();
        assert_eq!(t, "café & co \"x\"");
        assert_eq!(content(&db, &id), r#"<p title="a&amp;b">t</p>"#);
        let out = d.path().join("report.v2");
        assert_eq!(db.export_opml_file(&id, &out).unwrap(), 1);
        assert!(out.exists() && !d.path().join(".report.v2.part").exists());
        assert!(db.export_opml_file(&id, &d.path().join("missing-dir/x.opml")).is_err());
    }

    #[test]
    fn enex_converter_pieces() {
        let c = |h: &str| convert_enex_content(h, &[]);
        assert_eq!(c("<div>a</div><div><br></div>"), "<p>a</p><p>&nbsp;</p>");
        assert_eq!(c(r#"<div><en-todo checked="true"/>x</div>"#), r#"<ul class="todo-list"><li><label class="todo-list__label"><input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">x</span></label></li></ul>"#);
        assert!(c(r#"<p>mid <en-todo checked="true"/> line</p>"#).contains("\u{2611}"));
        let tasks = [EnexTask { title: "Do it".into(), status: "completed".into(), group_id: "g1".into() }, EnexTask { title: "Not yet".into(), status: "open".into(), group_id: "g2".into() }];
        let t = convert_enex_content(r#"<div style="--en-task-group:true; --en-id:g1">Content not supported</div>"#, &tasks);
        assert!(t.contains("Do it") && !t.contains("Not yet") && !t.contains("not supported"), "{t}");
        assert!(c(r#"<div style="--en-callout:true; --en-emoji:💡"><div>tip</div></div>"#).contains("admonition tip"));
        assert!(c(r#"<div style="--en-formulablock:true"><div>e=mc^2</div></div>"#).contains(r"\[e=mc^2\]"));
        assert!(c(r#"<div style="--en-mermaidblock:true"><div>graph TD</div></div>"#).contains("language-mermaid"));
        assert_eq!(code_language(Some("unknown-lang")), "text-x-trilium-auto");
    }
}

/// `toAttributeName`: "Last edited by" → `lastEditedBy`; names without alphanumerics become `unnamed`.
pub(crate) fn to_attribute_name(name: &str) -> String {
    static W: OnceLock<Regex> = OnceLock::new();
    let words: Vec<&str> = re(&W, r"[\p{L}\p{N}]+").find_iter(name).map(|m| m.as_str()).collect();
    if words.is_empty() { return "unnamed".into(); }
    words.iter().enumerate().map(|(i, w)| {
        if i == 0 { w.to_lowercase() } else { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default() }
    }).collect()
}

/// A promoted-attribute definition value (`promoted,single,text,alias=Name`).
pub(crate) fn promoted_definition(alias: &str, label_type: Option<&str>, multi: bool) -> String {
    let safe: String = alias.chars().map(|c| if c.is_control() || c == ',' || c == '=' { ' ' } else { c }).collect();
    format!("promoted,{},{}alias={}", if multi { "multi" } else { "single" }, label_type.map(|t| format!("{t},")).unwrap_or_default(), safe.trim())
}
