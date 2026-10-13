//! Evernote ENEX export: a note and its descendants as one flat notebook file. Text notes become well-formed
//! ENML (images and attachments are embedded as `<resource>`s referenced by `<en-media>`, to-do lists become
//! `<en-todo>` lines, code blocks use Evernote's code-block markup), labels without a value become tags and
//! `#pageUrl` becomes the source URL. Notes whose content can't be shown in Evernote (protected) are left out.
use crate::{
    db::Db,
    htmltree::{Node, decode_entities, escape_attr as escape_attr_raw, escape_text, parse},
    write::is_string_note,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use md5::{Digest, Md5};
use regex::Regex;
use rusqlite::OptionalExtension;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

/// XML-safe attribute value: control characters (illegal in XML 1.0) are dropped.
fn escape_attr(s: &str) -> String { escape_attr_raw(&s.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t' | '\r')).collect::<String>()) }

const ENML_TAGS: &[&str] = &["a", "abbr", "acronym", "address", "b", "bdo", "big", "blockquote", "br", "caption", "center", "cite", "code", "col", "colgroup", "dd", "del", "dfn", "div", "dl", "dt", "em", "font", "h1", "h2", "h3", "h4", "h5", "h6", "hr", "i", "img", "ins", "kbd", "li", "ol", "p", "pre", "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "u", "ul", "var"];

struct Resource { bytes: Vec<u8>, mime: String, name: String, hash: String }

#[derive(Default)]
struct Ctx { resources: Vec<Resource> }

impl Ctx {
    /// Register a file; returns the `<en-media>` markup referring to it.
    fn media(&mut self, bytes: Vec<u8>, mime: &str, name: &str, extra: &str) -> String {
        let hash: String = Md5::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if !self.resources.iter().any(|r| r.hash == hash && r.name == name) {
            self.resources.push(Resource { bytes, mime: mime.to_string(), name: name.to_string(), hash: hash.clone() });
        }
        format!("<en-media type=\"{}\" hash=\"{hash}\"{extra}/>", escape_attr(mime))
    }
}

const DROP: &[&str] = &["script", "style", "head", "meta", "link", "base", "form", "input", "iframe", "object", "embed", "applet", "button", "select", "textarea", "frame", "frameset", "noscript", "template", "title"];
const KEEP_ATTRS: &[&str] = &["href", "alt", "title", "style", "colspan", "rowspan", "width", "height", "align", "dir", "lang", "start"];
const VOID_OUT: &[&str] = &["br", "hr", "img", "col"];

fn has_class(e: &crate::htmltree::El, c: &str) -> bool { e.attr_dec("class").is_some_and(|v| v.split_whitespace().any(|x| x == c)) }

fn xml_text(raw: &str) -> String {
    escape_text(&decode_entities(raw)).replace('\u{a0}', "&#160;").chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t' | '\r')).collect()
}

fn clean_style(s: &str) -> String {
    s.split(';').map(str::trim).filter(|d| !d.is_empty() && !d.starts_with("--")).collect::<Vec<_>>().join("; ")
}

fn write_nodes(db: &Db, nodes: &[Node], out: &mut String, ctx: &mut Ctx, depth: usize) {
    for n in nodes { write_node(db, n, out, ctx, depth); }
}

fn write_node(db: &Db, n: &Node, out: &mut String, ctx: &mut Ctx, depth: usize) {
    if depth > 200 { return; }
    let e = match n { Node::Text(t) => { out.push_str(&xml_text(t)); return; } Node::El(e) => e };
    let name = e.name.as_str();
    if DROP.contains(&name) || name.starts_with("en-") { return; }
    match name {
        "img" => {
            let src = e.attr_dec("src").unwrap_or_default();
            let found = if db.source_is_protected(&src) { None } else { db.image_source_bytes(&src).ok().flatten() };
            match found {
                Some((fname, mime, bytes)) => {
                    let extra: String = ["width", "height"].iter().filter_map(|a| e.attr_dec(a).filter(|v| v.chars().all(|c| c.is_ascii_digit())).map(|v| format!(" {a}=\"{v}\""))).collect();
                    out.push_str(&ctx.media(bytes, &mime, &fname, &extra));
                }
                None if src.starts_with("http://") || src.starts_with("https://") => {
                    out.push_str(&format!("<img src=\"{}\"{}/>", escape_attr(&src), e.attr_dec("alt").map(|a| format!(" alt=\"{}\"", escape_attr(&a))).unwrap_or_default()));
                }
                None => {}
            }
        }
        "a" => {
            let href = e.attr_dec("href").unwrap_or_default();
            static ATT: OnceLock<Regex> = OnceLock::new();
            let att = ATT.get_or_init(|| Regex::new(r"attachmentId=([A-Za-z0-9_]+)").unwrap()).captures(&href).map(|c| c[1].to_string());
            if let Some(aid) = att {
                if let (Ok(Some(bytes)), Ok(Some((mime, title)))) = (if db.attachment_protected(&aid) { Ok(None) } else { db.attachment_bytes(&aid) }, db.conn.query_row("SELECT mime,title FROM attachments WHERE attachmentId=? AND isDeleted=0", [&aid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()) {
                    out.push_str(&ctx.media(bytes, &mime, &title, ""));
                    return;
                }
            }
            let external = href.starts_with("http://") || href.starts_with("https://") || href.starts_with("mailto:");
            if external {
                out.push_str(&format!("<a href=\"{}\">", escape_attr(&href)));
                write_nodes(db, &e.kids, out, ctx, depth + 1);
                out.push_str("</a>");
            } else {
                write_nodes(db, &e.kids, out, ctx, depth + 1);
            }
        }
        "pre" => {
            let code = e.child_els().find(|c| c.name == "code");
            let lang = code.and_then(|c| c.attr_dec("class")).and_then(|c| c.split_whitespace().find_map(|x| x.strip_prefix("language-").map(String::from)));
            let text = code.map_or_else(|| e.text(), |c| c.text());
            out.push_str(&code_block(&text, lang.as_deref()));
        }
        "ul" | "ol" if has_class(e, "todo-list") => todo_list(db, e, out, ctx, depth),
        "li" | "ul" | "ol" if false => {}
        "aside" if has_class(e, "admonition") => {
            out.push_str("<blockquote>");
            write_nodes(db, &e.kids, out, ctx, depth + 1);
            out.push_str("</blockquote>");
        }
        "details" => {
            for k in &e.kids {
                match k {
                    Node::El(s) if s.name == "summary" => { out.push_str("<div><b>"); write_nodes(db, &s.kids, out, ctx, depth + 1); out.push_str("</b></div>"); }
                    other => write_node(db, other, out, ctx, depth + 1),
                }
            }
        }
        "figure" | "section" | "main" | "article" | "header" | "footer" | "nav" | "label" | "summary" | "colgroup" | "center" | "font" | "abbr" | "time" | "cite" | "q" | "kbd" | "var" | "samp" | "dl" | "dd" | "dt" => {
            if name == "section" && (has_class(e, "include-note") || has_class(e, "link-embed")) { return; }
            write_nodes(db, &e.kids, out, ctx, depth + 1);
        }
        "span" if has_class(e, "math-tex") => out.push_str(&xml_text(&e.text())),
        "span" | "mark" => {
            let style = clean_style(&e.attr_dec("style").unwrap_or_default());
            let style = if name == "mark" && style.is_empty() { "background-color: rgb(255, 239, 158)".to_string() } else { style };
            if style.is_empty() { write_nodes(db, &e.kids, out, ctx, depth + 1); } else {
                out.push_str(&format!("<span style=\"{}\">", escape_attr(&style)));
                write_nodes(db, &e.kids, out, ctx, depth + 1);
                out.push_str("</span>");
            }
        }
        _ if !ENML_TAGS.contains(&name) => write_nodes(db, &e.kids, out, ctx, depth + 1),
        _ => {
            out.push('<');
            out.push_str(name);
            for (k, v) in &e.attrs {
                if !KEEP_ATTRS.contains(&k.as_str()) || (name != "a" && k == "href") { continue; }
                let v = decode_entities(v.as_deref().unwrap_or(""));
                let v = if k == "style" { clean_style(&v) } else { v };
                if v.is_empty() && k == "style" { continue; }
                out.push_str(&format!(" {k}=\"{}\"", escape_attr(&v)));
            }
            if VOID_OUT.contains(&name) { out.push_str("/>"); return; }
            out.push('>');
            write_nodes(db, &e.kids, out, ctx, depth + 1);
            out.push_str(&format!("</{name}>"));
        }
    }
}

fn todo_list(db: &Db, ul: &crate::htmltree::El, out: &mut String, ctx: &mut Ctx, depth: usize) {
    for li in ul.child_els().filter(|c| c.name == "li") {
        let label = li.child_els().find(|c| c.name == "label");
        let checked = label.is_some_and(|l| l.child_els().any(|c| c.name == "input" && c.has_attr("checked")));
        out.push_str(&format!("<div><en-todo checked=\"{checked}\"/>"));
        if let Some(d) = label.and_then(|l| l.child_els().find(|c| has_class(c, "todo-list__label__description"))) { write_nodes(db, &d.kids, out, ctx, depth + 1); }
        out.push_str("</div>");
        for nested in li.child_els().filter(|c| c.name == "ul" || c.name == "ol") {
            out.push_str("<div style=\"padding-left: 40px\">");
            todo_list(db, nested, out, ctx, depth + 1);
            out.push_str("</div>");
        }
    }
}

/// Evernote's code-block markup (it is what `--en-codeblock` in Trilium's ENEX importer reads back).
fn code_block(text: &str, lang: Option<&str>) -> String {
    let l = lang.map(|l| l.strip_prefix("text-x-").unwrap_or(l)).filter(|l| *l != "trilium-auto").unwrap_or("");
    let lines: String = text.trim_end_matches('\n').split('\n').map(|ln| if ln.is_empty() { "<div><br/></div>".to_string() } else { format!("<div>{}</div>", xml_text(ln)) }).collect();
    format!("<div style=\"box-sizing: border-box; padding: 8px; font-family: Monaco, Menlo, Consolas, 'Courier New', monospace; font-size: 12px; color: rgb(51, 51, 51); border-radius: 4px; background-color: rgb(251, 250, 248); border: 1px solid rgba(0, 0, 0, 0.15); --en-codeblock:true; --en-syntaxLanguage:{l};\">{lines}</div>")
}

fn enex_time(utc: &str) -> String {
    let digits: String = utc.chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    if digits.len() == 14 { format!("{}T{}Z", &digits[..8], &digits[8..]) } else { "19700101T000000Z".into() }
}

impl Db {
    /// Export `note_id` and its subtree to an ENEX file (written via a `.part` file). Returns the number of notes.
    pub fn export_enex_file(&self, note_id: &str, path: &Path) -> Result<usize> {
        let part = path.with_file_name(format!(".{}.part", path.file_name().and_then(|n| n.to_str()).unwrap_or("export")));
        let r = (|| -> Result<usize> {
            let f = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
            let mut w = std::io::BufWriter::new(f);
            let n = self.export_enex(note_id, &mut w)?;
            w.flush()?;
            w.get_ref().sync_all()?;
            std::fs::rename(&part, path).with_context(|| format!("moving the export to {}", path.display()))?;
            Ok(n)
        })();
        if r.is_err() { let _ = std::fs::remove_file(&part); }
        r
    }

    pub fn export_enex<W: Write>(&self, note_id: &str, mut w: W) -> Result<usize> {
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| r.get(0)).optional()?;
        anyhow::ensure!(exists.is_some(), "note {note_id} not found");
        write!(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE en-export SYSTEM \"http://xml.evernote.com/pub/evernote-export4.dtd\">\n<en-export export-date=\"{}\" application=\"Evernote\" version=\"10.0\">\n", enex_time(&crate::write::now_utc()))?;
        let mut seen = HashSet::new();
        let mut count = 0;
        self.enex_walk(note_id, 0, &mut seen, &mut w, &mut count)?;
        w.write_all(b"</en-export>\n")?;
        Ok(count)
    }

    fn enex_walk<W: Write>(&self, id: &str, depth: usize, seen: &mut HashSet<String>, w: &mut W, count: &mut usize) -> Result<()> {
        if depth > 200 { bail!("the tree is nested more than 200 levels deep; it can't be exported as ENEX"); }
        if !seen.insert(id.to_string()) || self.label_value(id, "excludeFromExport")?.is_some() { return Ok(()); }
        let row: Option<(String, String, String, i64, String, String)> = self.conn.query_row(
            "SELECT title,type,mime,isProtected,utcDateCreated,utcDateModified FROM notes WHERE noteId=? AND isDeleted=0", [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
        let Some((title, ntype, mime, protected, created, modified)) = row else { return Ok(()) };
        if protected == 0 {
            self.enex_note(id, &title, &ntype, &mime, &created, &modified, w)?;
            *count += 1;
        }
        let mut st = self.conn.prepare("SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0 ORDER BY notePosition")?;
        let kids: Vec<String> = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for k in kids { self.enex_walk(&k, depth + 1, seen, w, count)?; }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn enex_note<W: Write>(&self, id: &str, title: &str, ntype: &str, mime: &str, created: &str, modified: &str, w: &mut W) -> Result<()> {
        let mut ctx = Ctx::default();
        let mut body = String::new();
        match ntype {
            "text" | "book" => {
                let html = self.note_content(id)?.unwrap_or_default();
                write_nodes(self, &parse(&html), &mut body, &mut ctx, 0);
            }
            "image" | "file" => {
                if let Some(bytes) = self.note_bytes(id)? { body.push_str(&format!("<div>{}</div>", ctx.media(bytes, mime, title, ""))); }
            }
            _ if is_string_note(ntype, mime) => body.push_str(&code_block(&self.note_content(id)?.unwrap_or_default(), None)),
            _ => {}
        }
        // attachments the text doesn't show
        for a in self.attachments(id)?.into_iter().filter(|a| !self.attachment_protected(&a.id)) {
            if let Some(bytes) = self.attachment_bytes(&a.id)? {
                let tag = ctx.media(bytes, &a.mime, &a.title, "");
                if !body.contains(&tag) { body.push_str(&format!("<div>{tag}</div>")); }
            }
        }
        let mut st = self.conn.prepare("SELECT name,value FROM attributes WHERE noteId=? AND type='label' AND isDeleted=0 ORDER BY position")?;
        let labels: Vec<(String, String)> = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        write!(w, "<note>\n<title>{}</title>\n<created>{}</created>\n<updated>{}</updated>\n", xml_text(title), enex_time(created), enex_time(modified))?;
        for (n, v) in &labels {
            if v.is_empty() && !n.contains(':') && !matches!(n.as_str(), "sorted" | "archived" | "excludeFromExport" | "iconClass" | "cssClass" | "calendarRoot") {
                writeln!(w, "<tag>{}</tag>", xml_text(n))?;
            }
        }
        if let Some((_, url)) = labels.iter().find(|(n, v)| n == "pageUrl" && !v.is_empty()) {
            writeln!(w, "<note-attributes><source-url>{}</source-url></note-attributes>", xml_text(url))?;
        }
        write!(w, "<content><![CDATA[<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n<!DOCTYPE en-note SYSTEM \"http://xml.evernote.com/pub/enml2.dtd\"><en-note>{}</en-note>]]></content>\n", body.replace("]]>", "]]]]><![CDATA[>"))?;
        for r in &ctx.resources {
            writeln!(w, "<resource>")?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&r.bytes);
            write!(w, "<data encoding=\"base64\">")?;
            for chunk in b64.as_bytes().chunks(76) { w.write_all(chunk)?; w.write_all(b"\n")?; }
            writeln!(w, "</data>\n<mime>{}</mime>\n<resource-attributes><file-name>{}</file-name></resource-attributes>\n</resource>", xml_text(&r.mime), xml_text(&r.name))?;
        }
        writeln!(w, "</note>")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htmltree::{Token, tokenize};

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("x.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn well_formed(xml: &str) {
        let mut stack: Vec<String> = vec![];
        for t in tokenize(xml, false) {
            match t {
                Token::Open { name, self_close, .. } => if !self_close { stack.push(name) },
                Token::Close(name) => assert_eq!(stack.pop().as_deref(), Some(name.as_str()), "mismatched </{name}> in {xml}"),
                _ => {}
            }
        }
        assert!(stack.is_empty(), "unclosed {stack:?}");
    }

    #[test]
    fn enex_export_is_well_formed_and_imports_back() {
        let (d, db) = fixture();
        let top = db.create_note("root", "Notebook & <co>", "text", "text/html", "").unwrap();
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let n1 = db.create_note(&top, "First \u{e9}", "text", "text/html", "").unwrap();
        let img = db.create_file_note(&top, "pic.png", "image/png", &png).unwrap();
        let att = db.add_attachment(&n1, "doc.txt", "text/plain", "file", b"attached!").unwrap();
        db.set_note_content(&n1, &format!(r##"<h2>Head</h2><p class="x">Hi <b>there</b><br>line &nbsp;two <mark>hl</mark> <a href="https://example.org/?a=1&amp;b=2">ext</a> <a href="#root/{top}">internal</a></p>
            <p><img src="api/images/{img}/pic.png"> <a class="reference-link" href="#root/{n1}?viewMode=attachments&amp;attachmentId={att}">doc.txt</a></p>
            <ul class="todo-list"><li><label class="todo-list__label"><input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">done</span></label></li></ul>
            <pre><code class="language-text-x-python">x = 1 &lt; 2

print(x)</code></pre><script>evil()</script><table><tr><td colspan="2">c</td></tr></table>"##)).unwrap();
        db.add_attribute(&n1, "label", "project", "", false).unwrap();
        db.add_attribute(&n1, "label", "pageUrl", "https://example.org/page", false).unwrap();
        db.create_note(&top, "Source", "code", "text/x-rust", "fn main() { println!(\"<hi>\"); }\n").unwrap();
        let secret = db.create_note(&top, "Secret", "text", "text/html", "x").unwrap();
        db.add_attribute(&secret, "label", "excludeFromExport", "", false).unwrap();

        let out = d.path().join("nb.enex");
        let n = db.export_enex_file(&top, &out).unwrap();
        assert_eq!(n, 4, "notebook note, first, picture, source");
        let xml = std::fs::read_to_string(&out).unwrap();
        for bad in ["evil", "<script", "class=\"x\"", "#root", "Secret", "<input"] { assert!(!xml.contains(bad), "{bad}\n{xml}"); }
        for want in ["<title>First \u{e9}</title>", "<title>Notebook &amp; &lt;co&gt;</title>", "<tag>project</tag>", "<source-url>https://example.org/page</source-url>",
            "<en-todo checked=\"true\"/>done", "--en-codeblock:true; --en-syntaxLanguage:python;", "<br/>", "<td colspan=\"2\">", "<a href=\"https://example.org/?a=1&amp;b=2\">ext</a>"] {
            assert!(xml.contains(want), "missing {want}\n{xml}");
        }
        // content CDATA bodies are well-formed XML
        for c in xml.split("<![CDATA[").skip(1) { well_formed(c.split("]]>").next().unwrap().split("<en-note>").nth(1).map(|b| format!("<en-note>{b}")).unwrap().as_str()); }

        // read it back with our own importer
        let target = db.create_note("root", "Back", "text", "text/html", "").unwrap();
        let st = db.import_path(&out, &target, &Default::default()).unwrap();
        assert_eq!(st.notes, 5);
        let root = st.root.unwrap();
        let first: String = db.conn.query_row("SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND n.title=? AND b.isDeleted=0", [&root, "First \u{e9}"], |r| r.get(0)).unwrap();
        let html = db.note_content(&first).unwrap().unwrap();
        assert!(html.contains("<h2>Head</h2>") && html.contains("todo-list") && html.contains("language-text-x-csrc") == false && html.contains("<pre><code class=\"language-text-x-python\">x = 1 &lt; 2"), "{html}");
        assert!(db.has_label(&first, "project").unwrap() && db.label_value(&first, "pageUrl").unwrap().as_deref() == Some("https://example.org/page"));
        let titles: Vec<String> = db.attachments(&first).unwrap().into_iter().map(|a| a.title).collect();
        assert!(titles.contains(&"doc.txt".to_string()) && titles.contains(&"pic.png".to_string()), "{titles:?}");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn reviewer_enex_cases() {
        let (_d, db) = fixture();
        let n = db.create_note("root", "dups", "text", "text/html", "<p title=\"a\u{1}b\">x</p><custom-tag foo=\"1\">kept text</custom-tag>").unwrap();
        db.add_attachment(&n, "a.txt", "text/plain", "file", b"same").unwrap();
        db.add_attachment(&n, "b.txt", "text/plain", "file", b"same").unwrap();
        let mut buf = vec![];
        db.export_enex(&n, &mut buf).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        assert!(xml.contains("<file-name>a.txt</file-name>") && xml.contains("<file-name>b.txt</file-name>"), "{xml}");
        assert!(!xml.contains('\u{1}') && xml.contains("kept text") && !xml.contains("custom-tag"), "{xml}");
    }

    #[test]
    fn hostile_content_does_not_panic_or_break_the_file() {
        let (_d, db) = fixture();
        let n = db.create_note("root", "t]]>t", "text", "text/html", "<p>a]]>b \u{1} \u{0}</p><div>".repeat(3000).as_str()).unwrap();
        let mut buf = vec![];
        db.export_enex(&n, &mut buf).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        assert!(xml.ends_with("</en-export>\n"));
        assert!(db.export_enex("missing", &mut vec![]).is_err());
    }
}
