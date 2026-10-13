//! Google Keep (Takeout) import, ported from Trilium's `import/keep/`. Every note JSON becomes a text note directly under
//! one "Google Keep import" root: text or checklist body, colour, original timestamps, attachments appended to the body.
//! Keep has no hierarchy; labels and the pinned/archived/trashed flags are not carried over (as in Trilium).
use crate::{
    db::Db,
    htmltree::{El, Node, escape_text, map_els, parse, serialize},
    transfer::{Entry, ImportStats},
};
use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

fn base_name(p: &str) -> &str { p.rsplit('/').next().unwrap_or(p) }

/// Does this archive look like a Keep Takeout (note JSON with Keep's timestamp fields)?
pub fn looks_like_takeout(entries: &[Entry]) -> bool {
    entries.iter().filter(|e| !e.is_dir && e.path.to_lowercase().ends_with(".json")).take(8).any(|e| {
        let head = String::from_utf8_lossy(&e.data[..e.data.len().min(4096)]).into_owned();
        head.contains("\"userEditedTimestampUsec\"") || (head.contains("\"isTrashed\"") && head.contains("\"color\""))
    })
}

fn is_bold(style: &str) -> bool {
    static W: OnceLock<Regex> = OnceLock::new();
    match W.get_or_init(|| Regex::new(r"font-weight:\s*(\d+)").unwrap()).captures(style) {
        Some(c) => c[1].parse::<u32>().unwrap_or(0) >= 600,
        None => Regex::new(r"font-weight:\s*bold").unwrap().is_match(style),
    }
}

fn is_underline(style: &str) -> bool {
    static U: OnceLock<Regex> = OnceLock::new();
    U.get_or_init(|| Regex::new(r"text-decoration:\s*[^;]*underline").unwrap()).is_match(style)
}

/// Keep's Docs-style spans → `<strong>`/`<i>`/`<u>`; presentational attributes dropped.
fn convert_html(html: &str) -> Vec<Node> {
    map_els(parse(html), &mut |mut e: El| {
        if e.name == "span" {
            let style = e.attr_dec("style").unwrap_or_default();
            let mut kids = e.kids;
            if is_underline(&style) { kids = vec![Node::El(El::new("u", &[], kids))]; }
            if style.contains("font-style:") && Regex::new(r"font-style:\s*italic").unwrap().is_match(&style) { kids = vec![Node::El(El::new("i", &[], kids))]; }
            if is_bold(&style) { kids = vec![Node::El(El::new("strong", &[], kids))]; }
            return kids;
        }
        e.remove_attr("style");
        e.remove_attr("dir");
        vec![Node::El(e)]
    })
}

fn convert_inline(html: &str) -> String {
    let nodes = convert_html(html);
    let els: Vec<&El> = nodes.iter().filter_map(|n| if let Node::El(e) = n { Some(e) } else { None }).collect();
    if els.len() == 1 { els[0].inner_html() } else { serialize(&nodes) }
}

fn color_hex(c: &str) -> Option<&'static str> {
    Some(match c.to_uppercase().as_str() {
        "RED" => "#ff6d3f", "ORANGE" => "#ff9b00", "YELLOW" => "#ffda00", "GREEN" => "#95d641", "TEAL" => "#1ce8b5", "BLUE" => "#3fc3ff",
        "CERULEAN" => "#82b1ff", "PURPLE" => "#b388ff", "PINK" => "#f8bbd0", "BROWN" => "#d7ccc8", "GRAY" => "#b8c4c9", _ => return None,
    })
}

fn usec_to_utc(v: &serde_json::Value) -> Option<String> {
    let usec = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    chrono::DateTime::from_timestamp_micros(usec).map(|d| d.format("%Y-%m-%d %H:%M:%S%.3fZ").to_string())
}

/// "2026-06-21T11_14_14.438+03_00" → "2026-06-21 11:14:14".
fn untitled_title(name: &str) -> String {
    static T: OnceLock<Regex> = OnceLock::new();
    match T.get_or_init(|| Regex::new(r"^(\d{4}-\d{2}-\d{2})T(\d{2})_(\d{2})_(\d{2})").unwrap()).captures(name) {
        Some(c) => format!("{} {}:{}:{}", &c[1], &c[2], &c[3], &c[4]),
        None if name.is_empty() => "Untitled".into(),
        None => name.to_string(),
    }
}

fn build_content(n: &serde_json::Value) -> String {
    if let Some(list) = n["listContent"].as_array().filter(|l| !l.is_empty()) {
        let items: String = list.iter().filter(|i| i["text"].as_str().is_some_and(|t| !t.is_empty()) || i["textHtml"].as_str().is_some_and(|t| !t.is_empty())).map(|i| {
            let attrs = if i["isChecked"].as_bool() == Some(true) { " checked=\"checked\" disabled=\"disabled\"" } else { " disabled=\"disabled\"" };
            let label = match i["textHtml"].as_str().filter(|t| !t.is_empty()) { Some(h) => convert_inline(h), None => escape_text(i["text"].as_str().unwrap_or("")) };
            format!("<li><label class=\"todo-list__label\"><input type=\"checkbox\"{attrs}><span class=\"todo-list__label__description\">{label}</span></label></li>")
        }).collect();
        return if items.is_empty() { String::new() } else { format!("<ul class=\"todo-list\">{items}</ul>") };
    }
    if let Some(h) = n["textContentHtml"].as_str().filter(|t| !t.is_empty()) { return serialize(&convert_html(h)); }
    if let Some(t) = n["textContent"].as_str().filter(|t| !t.is_empty()) {
        return t.split('\n').map(|l| format!("<p>{}</p>", escape_text(l))).collect();
    }
    String::new()
}

impl Db {
    /// Import a Google Keep Takeout archive under `parent_id`.
    pub fn import_keep(&self, entries: Vec<Entry>, parent_id: &str) -> Result<ImportStats> {
        let exists: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)))?;
        anyhow::ensure!(exists.is_some(), "import target {parent_id} not found");
        let mut notes: Vec<(String, serde_json::Value)> = vec![];
        let mut binaries: HashMap<String, Vec<u8>> = HashMap::new();
        for e in entries {
            if e.is_dir { continue; }
            let lower = e.path.to_lowercase();
            if lower.ends_with(".json") {
                if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_slice::<serde_json::Value>(&e.data) { notes.push((e.path.clone(), v)); }
            } else if !lower.ends_with(".html") && !lower.ends_with(".txt") {
                binaries.insert(base_name(&e.path).to_string(), e.data);
            }
        }
        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let root = self.create_note(parent_id, "Google Keep import", "text", "text/html", "")?;
        self.add_attribute(&root, "label", "iconClass", "bx bx-import", false)?;
        stats.notes += 1;
        stats.root = Some(root.clone());
        for (path, n) in &notes {
            let stem = base_name(path).rsplit_once('.').map_or(base_name(path), |(s, _)| s);
            let title = n["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from).unwrap_or_else(|| untitled_title(stem));
            let id = self.create_note(&root, &title, "text", "text/html", "")?;
            stats.notes += 1;
            if let Some(hex) = n["color"].as_str().and_then(color_hex) { self.add_attribute(&id, "label", "color", hex, false)?; stats.attributes += 1; }
            let mut html = build_content(n);
            for a in n["attachments"].as_array().into_iter().flatten() {
                let Some(file) = a["filePath"].as_str().map(base_name).filter(|f| !f.is_empty()) else { continue };
                let Some(bytes) = binaries.get(file) else { continue };
                let mime = mime_guess::from_path(file).first().map(|m| m.to_string()).or_else(|| a["mimetype"].as_str().map(String::from)).unwrap_or_else(|| "application/octet-stream".into());
                if mime.starts_with("image/") {
                    let aid = self.add_attachment(&id, file, &mime, "image", bytes)?;
                    html.push_str(&format!("<p><img src=\"api/attachments/{aid}/image/{}\"></p>", encode_component(file)));
                } else {
                    let aid = self.add_attachment(&id, file, &mime, "file", bytes)?;
                    html.push_str(&format!("<p><a class=\"reference-link\" href=\"#root/{id}?viewMode=attachments&attachmentId={aid}\">{}</a></p>", escape_text(file)));
                }
                stats.attachments += 1;
            }
            let html = crate::htmltree::sanitize_html(&html);
            let created = usec_to_utc(&n["createdTimestampUsec"]);
            let modified = usec_to_utc(&n["userEditedTimestampUsec"]);
            let dates = match (created.as_deref(), modified.as_deref()) {
                (None, None) => None,
                (c, m) => { let c = c.or(m).unwrap_or_default().to_string(); let m = m.map(String::from).unwrap_or_else(|| c.clone()); Some((c, m)) }
            };
            self.store_imported_html(&id, &html, dates.as_ref().map(|(c, m)| (c.as_str(), m.as_str())))?;
        }
        tx.commit()?;
        Ok(stats)
    }
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() { if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); } }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("k.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn e(path: &str, data: &[u8]) -> Entry { Entry { path: path.into(), data: data.to_vec(), is_dir: false } }

    #[test]
    fn takeout_notes_checklists_colours_dates_and_attachments() {
        let (_d, db) = fixture();
        let text = br#"{"title":"Shopping","color":"YELLOW","isTrashed":false,"createdTimestampUsec":1600000000000000,"userEditedTimestampUsec":1600000100000000,
            "textContentHtml":"<p dir=\"ltr\" style=\"x\"><span style=\"font-weight:700;\">bold</span> and <span style=\"font-style:italic;text-decoration:underline;\">it</span></p><script>x</script>",
            "attachments":[{"filePath":"pic.png","mimetype":"image/png"},{"filePath":"memo.m4a","mimetype":"audio/mp4"},{"filePath":"gone.png"}]}"#;
        let list = br#"{"title":"","color":"DEFAULT","userEditedTimestampUsec":1700000000000000,"listContent":[{"text":"milk","isChecked":true},{"text":"","textHtml":""},{"text":"eggs","textHtml":"<p><span style=\"font-weight:bold\">free</span> eggs</p>","isChecked":false}]}"#;
        let plain = br#"{"title":"Plain","textContent":"line one\n<b>two</b>"}"#;
        let entries = vec![
            e("Takeout/Keep/Shopping.json", text), e("Takeout/Keep/Shopping.html", b"<html>dup</html>"), e("Takeout/Keep/2026-06-21T11_14_14.438+03_00.json", list),
            e("Takeout/Keep/Plain.json", plain), e("Takeout/Keep/pic.png", b"\x89PNG"), e("Takeout/Keep/memo.m4a", b"audio"), e("Takeout/Keep/Labels.txt", b"x"), e("Takeout/Keep/notes.json", b"[1,2]"),
        ];
        assert!(looks_like_takeout(&entries));
        let st = db.import_keep(entries, "root").unwrap();
        let root = st.root.unwrap();
        assert_eq!(st.notes, 4);
        let kid = |t: &str| -> String { db.conn.query_row("SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND n.title=? AND b.isDeleted=0", [&root, t], |r| r.get(0)).unwrap_or_else(|_| panic!("no {t}")) };
        let shop = kid("Shopping");
        let html = db.note_content(&shop).unwrap().unwrap();
        for want in ["<p><strong>bold</strong> and <i><u>it</u></i></p>", "api/attachments/", "viewMode=attachments"] { assert!(html.contains(want), "{want}\n{html}"); }
        assert!(!html.contains("dir=") && !html.contains("script") && !html.contains("style"), "{html}");
        assert_eq!(db.label_value(&shop, "color").unwrap().as_deref(), Some("#ffda00"));
        let (c, m): (String, String) = db.conn.query_row("SELECT utcDateCreated,utcDateModified FROM notes WHERE noteId=?", [&shop], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((c.as_str(), m.as_str()), ("2020-09-13 12:26:40.000Z", "2020-09-13 12:28:20.000Z"));
        let atts: Vec<(String, String)> = db.attachments(&shop).unwrap().into_iter().map(|a| (a.title, a.role)).collect();
        assert_eq!(atts, [("pic.png".to_string(), "image".to_string()), ("memo.m4a".to_string(), "file".to_string())]);
        let checklist = kid("2026-06-21 11:14:14");
        let lh = db.note_content(&checklist).unwrap().unwrap();
        assert!(lh.contains(r#"<input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">milk</span>"#) && lh.contains("<strong>free</strong> eggs") && lh.matches("<li>").count() == 2, "{lh}");
        assert!(db.label_value(&checklist, "color").unwrap().is_none());
        assert_eq!(db.note_content(&kid("Plain")).unwrap().unwrap(), "<p>line one</p><p>&lt;b&gt;two&lt;/b&gt;</p>");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }
}
