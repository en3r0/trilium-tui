//! Import and export in Trilium's ZIP format (`!!!meta.json` + data files), plus plain files and folders.
//! Export mirrors `services/export/zip.ts` (HTML format); import mirrors `services/import/zip.ts` for the
//! cases that matter: notes, attributes, attachments, clones, positions, and rewriting links/images between
//! relative file paths and note ids.
use crate::{db::Db, write::*};
use anyhow::{Context, Result, bail};
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, Write};
use std::path::Path;

// ------------------------------------------------------------------ metadata (wire format)

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct AttrMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note_id: Option<String>,
    #[serde(rename = "type")] pub kind: String,
    pub name: String,
    #[serde(default)] pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub is_inheritable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub position: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct AttMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub attachment_id: Option<String>,
    pub title: String,
    pub role: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub position: Option<i64>,
    pub data_file_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct NoteMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note_path: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub is_clone: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note_position: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub is_expanded: Option<bool>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")] pub ntype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub data_file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub dir_file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub no_import: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub attributes: Option<Vec<AttrMeta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub attachments: Option<Vec<AttMeta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub children: Option<Vec<NoteMeta>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MetaFile { pub format_version: i64, pub app_version: String, pub files: Vec<NoteMeta> }

#[derive(Debug, Default)]
pub struct ExportStats { pub notes: usize, pub attachments: usize }

#[derive(Debug, Default)]
pub struct ImportStats { pub notes: usize, pub attachments: usize, pub attributes: usize, pub clones: usize, pub root: Option<String> }

#[derive(Debug, Default, Clone)]
pub struct ImportOpts {
    pub preserve_ids: bool,
    /// Force an importer for archives/folders: `obsidian`, `notion`, or `trilium` (the generic importer). `None` detects.
    pub format: Option<String>,
    /// Name of the archive being imported (used to title a vault's root note).
    pub archive_name: Option<String>,
    /// Keep loose Markdown, CSV, ... files as source/file notes instead of converting them to text and spreadsheet notes.
    pub keep_sources: bool,
    /// Set by `import_path` for a lone file (not an archive entry).
    #[doc(hidden)]
    pub single_file: bool,
}

/// One file or directory of an archive / folder being imported.
pub struct Entry { pub path: String, pub data: Vec<u8>, pub is_dir: bool }

// ------------------------------------------------------------------ helpers

/// `sanitize-filename`: strip characters filesystems reject, reserved names, trailing dots/spaces.
pub fn sanitize_filename(name: &str) -> String {
    static RESERVED: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let reserved = RESERVED.get_or_init(|| Regex::new(r"(?i)^(con|prn|aux|nul|com[0-9]|lpt[0-9])(\..*)?$").unwrap());
    let mut s: String = name.chars().filter(|c| !matches!(c, '/' | '?' | '<' | '>' | '\\' | ':' | '*' | '|' | '"') && !c.is_control() && !('\u{80}'..='\u{9f}').contains(c)).collect();
    if s.chars().all(|c| c == '.') { s.clear(); }
    // trailing dots and spaces go first, so that "con " is seen as the reserved name it becomes
    let mut s = s.trim_end_matches(['.', ' ']).to_string();
    if reserved.is_match(&s) { s.clear(); }
    truncate_utf8(&s, 255)
}

fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while !s.is_char_boundary(end) { end -= 1; }
    s[..end].to_string()
}

/// JS `encodeURIComponent`
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

pub(crate) fn hex_pair(a: u8, b: u8) -> Option<u8> {
    Some((char::from(a).to_digit(16)? * 16 + char::from(b).to_digit(16)?) as u8)
}

fn decode_uri_component(s: &str) -> String {
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

fn escape_html(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

pub(crate) fn extname(name: &str) -> String {
    match name.rfind('.') { Some(i) if i > 0 && i + 1 < name.len() => name[i..].to_string(), _ => String::new() }
}

fn unique_name(existing: &mut HashMap<String, u32>, file_name: &str) -> String {
    let lc = file_name.to_lowercase();
    if existing.contains_key(&lc) {
        loop {
            let idx = { let e = existing.get_mut(&lc).unwrap(); let i = *e; *e += 1; i };
            let new_name = format!("{idx}_{lc}");
            if !existing.contains_key(&new_name) { existing.insert(new_name, 1); return format!("{idx}_{file_name}"); }
        }
    }
    existing.insert(lc, 1);
    file_name.to_string()
}

fn code_extension(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "text/x-python" | "text/x-python-script" => "py", "text/x-ruby" => "rb", "text/x-csrc" => "c", "text/x-c++src" => "cpp", "text/x-java" => "java",
        "text/x-csharp" => "cs", "text/x-go" => "go", "text/x-rustsrc" => "rs", "text/x-sh" | "text/x-shellscript" => "sh", "text/x-sql" => "sql",
        "text/x-yaml" | "application/x-yaml" => "yaml", "text/x-markdown" | "text/markdown" | "text/x-gfm" => "md", "text/css" => "css",
        "text/x-php" | "application/x-httpd-php" => "php", "text/x-lua" => "lua", "text/x-perl" => "pl", "text/x-diff" => "diff", "text/xml" | "application/xml" => "xml",
        "text/x-kotlin" => "kt", "text/x-swift" => "swift", "text/typescript" | "application/typescript" => "ts", "text/jsx" => "jsx", "text/x-toml" => "toml",
        _ => return None,
    })
}

/// `mapExtension` of the zip export providers (HTML format).
fn map_extension(ntype: Option<&str>, mime: &str, existing_ext: &str) -> Option<String> {
    if ntype == Some("text") { return Some("html".into()); }
    if mime == "application/x-javascript" || mime == "text/javascript" { return Some("js".into()); }
    if ntype == Some("canvas") || mime == "application/json" { return Some("json".into()); }
    if !existing_ext.is_empty() { return None; }
    let m = mime.to_lowercase();
    let m = m.trim();
    if m == "image/jpg" { return Some("jpg".into()); }
    if m == "text/mermaid" { return Some("txt".into()); }
    if let Some(e) = code_extension(m) { return Some(e.into()); }
    Some(mime_guess::get_mime_extensions_str(m).and_then(|e| e.first()).map(|s| s.to_string()).unwrap_or_else(|| "dat".into()))
}

fn store_uncompressed(mime: &str) -> bool {
    let m = mime.to_lowercase();
    m.starts_with("video/") || matches!(m.as_str(), "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "application/zip" | "application/pdf" | "application/gzip" | "font/woff" | "font/woff2")
}

// ------------------------------------------------------------------ export

struct EMeta {
    note_id: String, note_path: Vec<String>, is_clone: bool, title: String, position: i64, prefix: Option<String>, expanded: bool,
    ntype: String, mime: String, format: Option<String>, data_file: Option<String>, dir_file: Option<String>,
    attributes: Vec<AttrMeta>, attachments: Vec<AttMeta>, children: Vec<usize>,
}

struct Exporter<'a> {
    db: &'a Db,
    markdown: bool,
    arena: Vec<EMeta>,
    by_note: HashMap<String, usize>,
}

impl Exporter<'_> {
    fn data_file_name(&self, ntype: Option<&str>, mime: &str, base: &str, existing: &mut HashMap<String, u32>) -> String {
        let name = if base.trim().is_empty() { "note".to_string() } else { base.trim().to_string() };
        let existing_ext = extname(&name).to_lowercase();
        let new_ext = if self.markdown && ntype == Some("text") { Some("md".to_string()) } else { map_extension(ntype, mime, &existing_ext) };
        let ext = match new_ext { Some(e) if existing_ext != format!(".{}", e.to_lowercase()) => format!(".{e}"), _ => String::new() };
        let base = truncate_utf8(&sanitize_filename(&name), 255 - ext.len());
        unique_name(existing, &format!("{base}{ext}"))
    }

    fn create_meta(&mut self, note_id: &str, prefix: Option<String>, position: i64, expanded: bool, parent_path: &[String], existing: &mut HashMap<String, u32>, depth: usize) -> Result<Option<usize>> {
        if depth > 100 { bail!("tree too deep"); }
        let db = self.db;
        let (title, ntype, mime, protected): (String, String, String, i64) = db.conn.query_row(
            "SELECT title,type,mime,isProtected FROM notes WHERE noteId=? AND isDeleted=0", [note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        if db.label_value(note_id, "excludeFromExport")?.is_some() { return Ok(None); }
        let title = if protected != 0 { "[protected]".to_string() } else { title };
        let complete = match prefix.as_deref() { Some(p) if !p.is_empty() => format!("{p} - {title}"), _ => title.clone() };
        let mut base = sanitize_filename(&complete);
        if base.is_empty() { base = "note".to_string(); }
        if base.chars().count() > 200 { base = base.chars().take(200).collect(); }
        let mut path = parent_path.to_vec();
        path.push(note_id.to_string());

        if let Some(_first) = self.by_note.get(note_id) {
            let file = unique_name(existing, &format!("{base}.clone.{}", if self.markdown { "md" } else { "html" }));
            self.arena.push(EMeta { note_id: note_id.into(), note_path: path, is_clone: true, title, position, prefix, expanded, ntype: "text".into(), mime: "text/html".into(),
                format: Some(if self.markdown { "markdown" } else { "html" }.into()), data_file: Some(file), dir_file: None, attributes: vec![], attachments: vec![], children: vec![] });
            return Ok(Some(self.arena.len() - 1));
        }
        let attributes: Vec<AttrMeta> = {
            let mut st = db.conn.prepare("SELECT type,name,value,isInheritable,position FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
            st.query_map([note_id], |r| Ok(AttrMeta { note_id: None, kind: r.get(0)?, name: r.get(1)?, value: r.get(2)?, is_inheritable: Some(r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0), position: Some(r.get(4)?) }))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter().filter(|a| !(a.kind == "relation" && (a.name == "internalLink" || a.name == "imageLink"))).collect()
        };
        let idx = self.arena.len();
        self.arena.push(EMeta { note_id: note_id.into(), note_path: path.clone(), is_clone: false, title, position, prefix, expanded, ntype: ntype.clone(), mime: mime.clone(),
            format: (ntype == "text").then(|| if self.markdown { "markdown" } else { "html" }.to_string()), data_file: None, dir_file: None, attributes, attachments: vec![], children: vec![] });
        self.by_note.insert(note_id.to_string(), idx);

        let mut kids: Vec<(String, Option<String>, i64, bool)> = {
            let mut st = db.conn.prepare("SELECT noteId,prefix,notePosition,isExpanded FROM branches WHERE parentNoteId=? AND isDeleted=0 AND noteId != '_hidden' ORDER BY notePosition, branchId")?;
            st.query_map([note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0)))?.collect::<rusqlite::Result<_>>()?
        };
        kids.retain(|(k, ..)| db.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [k], |r| r.get::<_, i64>(0)).is_ok());
        let content_len: i64 = db.conn.query_row("SELECT COALESCE(LENGTH(b.content),0) FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId=?", [note_id], |r| r.get(0))?;
        if protected == 0 && (content_len > 0 || kids.is_empty()) {
            let file = self.data_file_name(Some(&ntype), &mime, &base, existing);
            self.arena[idx].data_file = Some(file);
        }
        // a protected note's attachments, and any protected attachment, stay out of exports (their text would be decrypted into the archive)
        let atts: Vec<_> = if protected != 0 { vec![] } else { db.attachments(note_id)?.into_iter().filter(|a| !db.attachment_protected(&a.id)).collect() };
        let mut metas = vec![];
        let mut sorted = atts.clone();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        for a in sorted {
            let file = self.data_file_name(None, &a.mime, &format!("{base}_{}", a.title), existing);
            let position: i64 = db.conn.query_row("SELECT position FROM attachments WHERE attachmentId=?", [&a.id], |r| r.get(0)).unwrap_or(0);
            metas.push(AttMeta { attachment_id: Some(a.id), title: a.title, role: a.role, mime: a.mime, position: Some(position), data_file_name: file });
        }
        self.arena[idx].attachments = metas;
        if !kids.is_empty() {
            let dir = unique_name(existing, &base);
            self.arena[idx].dir_file = Some(dir);
            let mut child_existing = HashMap::new();
            for (kid, pre, pos, exp) in kids {
                if let Some(c) = self.create_meta(&kid, pre.filter(|p| !p.is_empty()), pos, exp, &path, &mut child_existing, depth + 1)? { self.arena[idx].children.push(c); }
            }
        }
        Ok(Some(idx))
    }

    /// `getNoteTargetUrl`: relative URL from one exported note's file to another's.
    fn target_url(&self, target: &str, source: usize, file_override: Option<&str>) -> Option<String> {
        let tm = &self.arena[*self.by_note.get(target)?];
        let mut tp: Vec<&String> = tm.note_path.iter().collect();
        let mut sp: Vec<&String> = self.arena[source].note_path.iter().collect();
        while tp.len() > 1 && sp.len() > 1 && tp[0] == sp[0] { tp.remove(0); sp.remove(0); }
        let mut url = "../".repeat(sp.len() - 1);
        for id in &tp[..tp.len() - 1] {
            if let Some(m) = self.by_note.get(*id).map(|i| &self.arena[*i]) { if let Some(d) = &m.dir_file { url.push_str(&encode_uri_component(d)); url.push('/'); } }
        }
        let last = &self.arena[*self.by_note.get(tp[tp.len() - 1])?];
        url.push_str(&encode_uri_component(file_override.or(last.data_file.as_deref()).or(last.dir_file.as_deref()).unwrap_or("")));
        Some(url)
    }

    fn rewrite_links(&self, content: &str, source: usize) -> String {
        static IMG: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        static ATT_IMG: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        static ATT_HREF: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        static NOTE_HREF: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let img = IMG.get_or_init(|| Regex::new(r#"src="[^"]*api/images/([a-zA-Z0-9_]+)/[^"]*""#).unwrap());
        let att_img = ATT_IMG.get_or_init(|| Regex::new(r#"(src|data-image|data-favicon)="[^"]*api/attachments/([a-zA-Z0-9_]+)/image/[^"]*""#).unwrap());
        let att_href = ATT_HREF.get_or_init(|| Regex::new(r##"href="[^"]*#root[^"]*attachmentId=([a-zA-Z0-9_]+)/?""##).unwrap());
        let note_href = NOTE_HREF.get_or_init(|| Regex::new(r##"href="[^"]*#root[a-zA-Z0-9_/]*/([a-zA-Z0-9_]+)[^"]*""##).unwrap());
        let me = &self.arena[source];
        let find_att = |id: &str| me.attachments.iter().find(|a| a.attachment_id.as_deref() == Some(id)).map(|a| a.data_file_name.clone());
        let c = img.replace_all(content, |caps: &regex::Captures| {
            let id = &caps[1];
            // an embedded image note: its raw file is the data file (image/file notes keep their bytes there)
            match self.target_url(id, source, None) { Some(u) => format!("src=\"{u}\""), None => caps[0].to_string() }
        });
        let c = att_img.replace_all(&c, |caps: &regex::Captures| match find_att(&caps[2]) { Some(u) => format!("{}=\"{}\"", &caps[1], u), None => caps[0].to_string() });
        let c = att_href.replace_all(&c, |caps: &regex::Captures| match find_att(&caps[1]) { Some(u) => format!("href=\"{u}\""), None => caps[0].to_string() });
        let c = note_href.replace_all(&c, |caps: &regex::Captures| match self.target_url(&caps[1], source, None) { Some(u) => format!("href=\"{u}\""), None => caps[0].to_string() });
        c.into_owned()
    }

    /// Markdown form of a text note: links rewritten to relative files, `# Title` on top.
    fn prepare_markdown(&self, title: &str, content: &str, idx: usize) -> String {
        let html = self.rewrite_links(content, idx);
        let md = crate::htmlmd::html_to_markdown(&html);
        if md.trim().is_empty() || md.starts_with("# ") { md } else { format!("# {title}\r\n{md}") }
    }

    fn prepare_text(&self, title: &str, content: &str, idx: usize) -> String {
        let mut c = content.replace(|_: char| false, "");
        c = Regex::new(r#" data-list-item-id="[^"]*""#).unwrap().replace_all(&c, "").into_owned();
        if !c.chars().take(100).collect::<String>().to_lowercase().contains("<html") {
            let css = format!("{}style.css", "../".repeat(self.arena[idx].note_path.len().saturating_sub(1)));
            let t = escape_html(title);
            c = format!("<html>\n<head>\n    <meta charset=\"utf-8\">\n    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n    <link rel=\"stylesheet\" href=\"{css}\">\n    <base target=\"_parent\">\n    <title data-trilium-title>{t}</title>\n</head>\n<body>\n    <div class=\"content\">\n    <h1 data-trilium-h1>{t}</h1>\n\n    <div class=\"ck-content\">{c}</div>\n    </div>\n</body>\n</html>");
        }
        self.rewrite_links(&c, idx)
    }

    fn to_meta(&self, idx: usize) -> NoteMeta {
        let m = &self.arena[idx];
        if m.is_clone {
            return NoteMeta { note_id: Some(m.note_id.clone()), note_path: Some(m.note_path.clone()), is_clone: Some(true), title: Some(m.title.clone()), prefix: m.prefix.clone(),
                note_position: Some(m.position),
                data_file_name: m.data_file.clone(), ntype: Some("text".into()), format: Some(if self.markdown { "markdown" } else { "html" }.into()), ..Default::default() };
        }
        NoteMeta {
            note_id: Some(m.note_id.clone()), note_path: Some(m.note_path.clone()), is_clone: Some(false), title: Some(m.title.clone()), note_position: Some(m.position),
            prefix: m.prefix.clone(), is_expanded: Some(m.expanded), ntype: Some(m.ntype.clone()), mime: Some(m.mime.clone()), format: m.format.clone(),
            data_file_name: m.data_file.clone(), dir_file_name: m.dir_file.clone(),
            attributes: Some(m.attributes.iter().filter(|a| a.kind != "relation" || self.by_note.contains_key(&a.value) || a.value == "root" || a.value.starts_with('_')).cloned().collect()),
            attachments: Some(m.attachments.clone()),
            children: if m.children.is_empty() { None } else { Some(m.children.iter().map(|c| self.to_meta(*c)).collect()) },
            ..Default::default()
        }
    }
}

pub(crate) fn add_file<W: Write + Seek>(zip: &mut zip::ZipWriter<W>, name: &str, data: &[u8], mime: &str) -> Result<()> {
    // names come from titles and ids that can be hostile: never a path that climbs out of or starts outside the archive
    if name.starts_with('/') || name.contains('\\') || name.split('/').any(|seg| seg == "..") { anyhow::bail!("refusing to write {name:?} into an archive"); }
    let method = if store_uncompressed(mime) { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
    zip.start_file(name, zip::write::SimpleFileOptions::default().compression_method(method).large_file(data.len() as u64 > 3_900_000_000))?;
    zip.write_all(data)?;
    Ok(())
}

impl Db {
    /// Export a note and its subtree as a Trilium-compatible ZIP (HTML format).
    /// Export to a file without ever clobbering an existing one on failure: written beside it, then renamed into place.
    fn export_file_as(&self, note_id: &str, path: &Path, markdown: bool) -> Result<ExportStats> {
        let part = path.with_file_name(format!(".{}.part", path.file_name().and_then(|n| n.to_str()).unwrap_or("export")));
        let result = (|| -> Result<ExportStats> {
            let f = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
            let mut w = std::io::BufWriter::new(f);
            let st = self.export_zip_as(note_id, &mut w, markdown)?;
            w.flush()?;
            w.get_ref().sync_all()?; // contents on disk before the name points at them
            std::fs::rename(&part, path).with_context(|| format!("moving the export to {}", path.display()))?;
            Ok(st)
        })();
        if result.is_err() { let _ = std::fs::remove_file(&part); }
        result
    }

    pub fn export_zip<W: Write + Seek>(&self, note_id: &str, out: W) -> Result<ExportStats> { self.export_zip_as(note_id, out, false) }

    /// Like `export_zip_file`, but text notes are written as Markdown (`.md`) instead of HTML.
    pub fn export_markdown_file(&self, note_id: &str, path: &Path) -> Result<ExportStats> { self.export_file_as(note_id, path, true) }

    pub fn export_zip_file(&self, note_id: &str, path: &Path) -> Result<ExportStats> { self.export_file_as(note_id, path, false) }

    pub fn export_zip_as<W: Write + Seek>(&self, note_id: &str, out: W, markdown: bool) -> Result<ExportStats> {
        let mut ex = Exporter { db: self, markdown, arena: vec![], by_note: HashMap::new() };
        let (prefix, position, expanded): (Option<String>, i64, bool) = self.conn.query_row(
            "SELECT prefix,notePosition,isExpanded FROM branches WHERE noteId=? AND isDeleted=0 ORDER BY notePosition LIMIT 1", [note_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0))).optional()?.unwrap_or((None, 10, false));
        let mut existing: HashMap<String, u32> = HashMap::from([("navigation".to_string(), 0), ("index".to_string(), 1)]);
        // the archive's own files can't be taken by a note called "index" or "navigation" (a zip can't hold two of one name)
        for reserved in ["navigation.html", "index.html", "style.css", "!!!meta.json"] { existing.entry(reserved.to_string()).or_insert(1); }
        let root = ex.create_meta(note_id, prefix.filter(|p| !p.is_empty()), position, expanded, &[], &mut existing, 0)?.context("this note is excluded from export")?;

        let mut files = vec![serde_json::to_value(ex.to_meta(root))?];
        for f in ["navigation.html", "index.html", "style.css"].into_iter().filter(|_| !markdown) { files.push(serde_json::json!({"noImport": true, "dataFileName": f})); }
        // Trilium writes `"prefix": null` for notes without one
        fn null_prefix(v: &mut Value) {
            if let Value::Object(o) = v {
                if o.contains_key("noteId") && !o.contains_key("prefix") { o.insert("prefix".into(), Value::Null); }
                if let Some(Value::Array(ch)) = o.get_mut("children") { for c in ch { null_prefix(c); } }
            }
        }
        for f in files.iter_mut() { null_prefix(f); }
        let meta = serde_json::json!({"formatVersion": 2, "appVersion": env!("CARGO_PKG_VERSION"), "files": files});

        let mut zip = zip::ZipWriter::new(out);
        add_file(&mut zip, "!!!meta.json", serde_json::to_string_pretty(&meta)?.replace("  ", "\t").as_bytes(), "application/json")?;
        let mut stats = ExportStats::default();
        self.save_meta(&ex, root, "", &mut zip, &mut stats)?;
        // extra files that make the archive browsable without Trilium
        if !markdown {
        let nav = format!("<html><head><meta charset=\"utf-8\"><link rel=\"stylesheet\" href=\"style.css\"></head><body><ul>{}</ul></body></html>", nav_html(&ex, root, root));
        add_file(&mut zip, "navigation.html", nav.as_bytes(), "text/html")?;
        let index = "<html><head><meta charset=\"utf-8\"><link rel=\"stylesheet\" href=\"style.css\"></head><frameset cols=\"25%,75%\"><frame name=\"navigation\" src=\"navigation.html\"><frame name=\"detail\" src=\"\"></frameset></html>";
        add_file(&mut zip, "index.html", index.as_bytes(), "text/html")?;
        add_file(&mut zip, "style.css", b"body { font-family: sans-serif; max-width: 50em; margin: 1em auto; line-height: 1.5 }\npre, code { background: #f4f4f4 }\n", "text/css")?;
        }
        zip.finish()?;
        Ok(stats)
    }

    fn save_meta<W: Write + Seek>(&self, ex: &Exporter, idx: usize, prefix: &str, zip: &mut zip::ZipWriter<W>, stats: &mut ExportStats) -> Result<()> {
        let m = &ex.arena[idx];
        stats.notes += 1;
        if m.is_clone {
            let url = ex.target_url(&m.note_id, idx, None).unwrap_or_default();
            let c = if ex.markdown { ex.prepare_markdown(&m.title, &format!("<p>This is a clone of a note. Go to its <a href=\"{url}\">primary location</a>.</p>"), idx) }
                else { ex.prepare_text(&m.title, &format!("<p>This is a clone of a note. Go to its <a href=\"{url}\">primary location</a>.</p>"), idx) };
            add_file(zip, &format!("{prefix}{}", m.data_file.as_deref().unwrap_or("clone.html")), c.as_bytes(), "text/html")?;
            return Ok(());
        }
        if let Some(df) = &m.data_file {
            let raw: Vec<u8> = self.conn.query_row("SELECT b.content FROM notes n LEFT JOIN blobs b ON b.blobId=n.blobId WHERE n.noteId=?", [&m.note_id], |r| r.get::<_, Option<rusqlite::types::Value>>(0))?
                .map(|v| match v { rusqlite::types::Value::Text(s) => s.into_bytes(), rusqlite::types::Value::Blob(b) => b, _ => vec![] }).unwrap_or_default();
            let data = if m.ntype == "text" && ex.markdown { ex.prepare_markdown(&m.title, &String::from_utf8_lossy(&raw), idx).into_bytes() }
                else if m.ntype == "text" { ex.prepare_text(&m.title, &String::from_utf8_lossy(&raw), idx).into_bytes() } else { raw };
            add_file(zip, &format!("{prefix}{df}"), &data, &m.mime)?;
        }
        for a in &m.attachments {
            if let Some(id) = &a.attachment_id {
                if let Some(bytes) = self.attachment_bytes(id)? { add_file(zip, &format!("{prefix}{}", a.data_file_name), &bytes, &a.mime)?; stats.attachments += 1; }
            }
        }
        if !m.children.is_empty() {
            let dir = format!("{prefix}{}", m.dir_file.as_deref().unwrap_or("dir"));
            zip.add_directory(format!("{dir}/"), zip::write::SimpleFileOptions::default())?;
            for c in &m.children { self.save_meta(ex, *c, &format!("{dir}/"), zip, stats)?; }
        }
        Ok(())
    }
}

fn nav_html(ex: &Exporter, root: usize, idx: usize) -> String {
    let m = &ex.arena[idx];
    let title = escape_html(&match m.prefix.as_deref() { Some(p) if !p.is_empty() => format!("{p} - {}", m.title), _ => m.title.clone() });
    let mut h = String::from("<li>");
    match (&m.data_file, ex.target_url(&m.note_id, root, None)) {
        (Some(_), Some(u)) if !m.is_clone => h.push_str(&format!("<a href=\"{u}\" target=\"detail\">{title}</a>")),
        _ => h.push_str(&title),
    }
    if !m.children.is_empty() { h.push_str("<ul>"); for c in &m.children { h.push_str(&nav_html(ex, root, *c)); } h.push_str("</ul>"); }
    h.push_str("</li>");
    h
}

// ------------------------------------------------------------------ import

fn dirname(p: &str) -> String { match p.rfind('/') { Some(i) => p[..i].to_string(), None => ".".to_string() } }
fn basename(p: &str) -> String { p.rsplit('/').next().unwrap_or(p).to_string() }
fn strip_ext(p: &str) -> String { let e = extname(&basename(p)); p[..p.len() - e.len()].to_string() }

const KNOWN_TYPES: &[&str] = &["text", "code", "file", "image", "search", "book", "relationMap", "render", "canvas", "mermaid", "mindMap", "noteMap", "webView", "launcher", "doc", "aiChat", "spreadsheet", "llmChat"];

/// Mime type and note type for a file name (`mimeService.getType`, simplified).
pub fn type_for_file(name: &str) -> (String, String) {
    let ext = extname(name).to_lowercase();
    let mime = match ext.as_str() {
        ".md" | ".markdown" => "text/x-markdown".to_string(),
        ".html" | ".htm" => "text/html".to_string(),
        ".txt" => "text/plain".to_string(),
        ".js" | ".mjs" => "application/javascript;env=frontend".to_string(),
        ".ts" => "text/typescript".to_string(),
        ".py" => "text/x-python".to_string(), ".rs" => "text/x-rustsrc".to_string(), ".sh" => "text/x-sh".to_string(),
        ".json" => "application/json".to_string(), ".css" => "text/css".to_string(), ".xml" => "text/xml".to_string(),
        ".yaml" | ".yml" => "text/x-yaml".to_string(), ".toml" => "text/x-toml".to_string(), ".sql" => "text/x-sql".to_string(),
        ".c" => "text/x-csrc".to_string(), ".cpp" | ".cc" | ".h" | ".hpp" => "text/x-c++src".to_string(), ".java" => "text/x-java".to_string(),
        ".go" => "text/x-go".to_string(), ".rb" => "text/x-ruby".to_string(), ".php" => "text/x-php".to_string(), ".lua" => "text/x-lua".to_string(),
        _ => mime_guess::from_path(name).first().map(|m| m.essence_str().to_string()).unwrap_or_else(|| "application/octet-stream".into()),
    };
    let ntype = if mime == "text/html" { "text" }
        else if mime.starts_with("image/") && mime != "image/svg+xml" { "image" }
        else if mime.starts_with("text/") || mime.starts_with("application/javascript") || mime == "application/json" || mime == "image/svg+xml" { "code" }
        else { "file" };
    (ntype.to_string(), mime)
}

struct Importer<'a> {
    db: &'a Db,
    opts: &'a ImportOpts,
    id_map: HashMap<String, String>,
    att_map: HashMap<String, String>,
    stats: ImportStats,
    parents_touched: HashSet<String>,
    /// archive entries by path
    files: HashMap<String, Vec<u8>>,
    meta: Option<MetaFile>,
    attributes: Vec<(String, AttrMeta)>,
    clones: Vec<(String, String, Option<String>, Option<i64>)>, // old note id, new parent id, prefix, position
    /// text notes made from loose files (note id, archive path), for resolving their relative links afterwards
    text_notes: Vec<(String, String)>,
}

impl Importer<'_> {
    fn new_id(&mut self, old: &str) -> String {
        if old.trim().is_empty() { return "empty_note_id".into(); }
        if self.opts.preserve_ids { return old.to_string(); }
        self.id_map.entry(old.to_string()).or_insert_with(new_entity_id).clone()
    }
    /// The new id of a note that is part of this import (never allocates one).
    fn lookup(&self, old: &str) -> Option<String> {
        if self.opts.preserve_ids { return Some(old.to_string()); }
        self.id_map.get(old).cloned()
    }

    fn new_att_id(&mut self, old: &str) -> String {
        if self.opts.preserve_ids { return old.to_string(); }
        self.att_map.entry(old.to_string()).or_insert_with(new_entity_id).clone()
    }

    /// `getMeta(filePath)`: resolve an archive path to (note meta, attachment meta).
    fn resolve(&self, path: &str) -> (Option<&NoteMeta>, Option<&AttMeta>) {
        let Some(mf) = &self.meta else { return (None, None) };
        let mut children: &[NoteMeta] = &mf.files;
        let mut cursor: Option<&NoteMeta> = None;
        let segs: Vec<&str> = path.split(['/', '\\']).collect();
        for (i, seg) in segs.iter().enumerate() {
            let seg = seg.to_string();
            match children.iter().find(|f| f.data_file_name.as_deref() == Some(&seg) || f.dir_file_name.as_deref() == Some(&seg)) {
                Some(f) => { cursor = Some(f); children = f.children.as_deref().unwrap_or(&[]); }
                None => {
                    // an attachment file of one of the notes at this level
                    if i == segs.len() - 1 { return self.find_attachment(&seg, if i == 0 { &mf.files } else { children }, cursor); }
                    return (None, None);
                }
            }
        }
        (cursor, None)
    }

    fn find_attachment<'b>(&'b self, file: &str, siblings: &'b [NoteMeta], parent: Option<&'b NoteMeta>) -> (Option<&'b NoteMeta>, Option<&'b AttMeta>) {
        let level: &[NoteMeta] = match parent { Some(p) => p.children.as_deref().unwrap_or(&[]), None => siblings };
        for n in level.iter().chain(parent.into_iter()) {
            for a in n.attachments.as_deref().unwrap_or(&[]) { if a.data_file_name == file { return (Some(n), Some(a)); } }
        }
        (None, None)
    }

    /// `getEntityIdFromRelativeUrl`: (note id, optional (attachment id, title, is rendered-note image)).
    fn target_of(&mut self, url: &str, file_path: &str) -> Option<(String, Option<(String, String)>)> {
        let mut url = url.to_string();
        let abs = if !url.starts_with('/') {
            while url.starts_with("./") { url = url[2..].to_string(); }
            let mut abs = dirname(file_path);
            while url.starts_with("../") { abs = dirname(&abs); url = url[3..].to_string(); }
            if abs == "." { abs.clear(); }
            if abs.is_empty() { url } else { format!("{abs}/{url}") }
        } else { url[1..].to_string() };
        let (note, att) = self.resolve(&abs);
        let note_id = note?.note_id.clone()?;
        let att = att.cloned();
        let nid = self.new_id(&note_id);
        match att {
            Some(a) if a.attachment_id.is_some() => { let aid = self.new_att_id(a.attachment_id.as_deref().unwrap()); Some((nid, Some((aid, a.title)))) }
            _ => Some((nid, None)),
        }
    }

    fn process_text(&mut self, content: &str, title: &str, file_path: &str, meta: Option<&NoteMeta>) -> String {
        let mut c = content.to_string();
        // Trilium's own wrapper tags
        for pat in [r"(?i)<h1 data-trilium-h1>([^<]*)</h1>", r"(?i)<title data-trilium-title>([^<]*)</title>"] { c = Regex::new(pat).unwrap().replace_all(&c, "").into_owned(); }
        c = Regex::new(r#"(?ms)<div class="ck-content">(.*)</div>"#).unwrap().replace_all(&c, "$1").into_owned();
        c = Regex::new(r#"(?ms)<div class="content">(.*)</div>"#).unwrap().replace_all(&c, "$1").into_owned();
        // a leading <h1> equal to the title is the page heading, other h1 become h2
        let t = title.trim().to_string();
        c = Regex::new(r"(?i)<h1>([^<]*)</h1>").unwrap().replace_all(&c, |caps: &regex::Captures| if caps[1].trim() == t { String::new() } else { format!("<h2>{}</h2>", &caps[1]) }).into_owned();
        c = Regex::new(r"(?is)<html.*?<body[^>]*>").unwrap().replace_all(&c, "").into_owned();
        c = Regex::new(r"(?is)</body>.*</html>").unwrap().replace_all(&c, "").into_owned();

        static ABS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let abs_re = ABS.get_or_init(|| Regex::new(r"(?i)^(?:[a-z]+:)?//").unwrap());
        let is_abs = |u: &str| abs_re.is_match(u);
        let src_re = Regex::new(r#"(src|data-image|data-favicon)="([^"]*)""#).unwrap();
        let mut out = String::new();
        let mut last = 0;
        for caps in src_re.captures_iter(&c.clone()) {
            let m = caps.get(0).unwrap();
            out.push_str(&c[last..m.start()]);
            last = m.end();
            let (attr, url) = (&caps[1], &caps[2]);
            if url.starts_with("data:image") { out.push_str(m.as_str()); continue; }
            let url = decode_uri_component(url).trim().to_string();
            if is_abs(&url) { out.push_str(m.as_str()); continue; }
            match self.target_of(&url, file_path) {
                Some((_, Some((aid, atitle)))) => out.push_str(&format!("{attr}=\"api/attachments/{aid}/image/{}\"", encode_uri_component(&if atitle.is_empty() { basename(&url) } else { atitle }))),
                Some((nid, None)) if attr == "src" => out.push_str(&format!("src=\"api/images/{nid}/{}\"", basename(&url))),
                _ => out.push_str(m.as_str()),
            }
        }
        out.push_str(&c[last..]);
        c = out;

        let href_re = Regex::new(r#"href="([^"]*)""#).unwrap();
        let mut out = String::new();
        let mut last = 0;
        for caps in href_re.captures_iter(&c.clone()) {
            let m = caps.get(0).unwrap();
            out.push_str(&c[last..m.start()]);
            last = m.end();
            let url = decode_uri_component(&caps[1]).trim().to_string();
            if url.starts_with('#') || is_abs(&url) || url.starts_with("mailto:") { out.push_str(m.as_str()); continue; }
            match self.target_of(&url, file_path) {
                Some((nid, Some((aid, _)))) => out.push_str(&format!("href=\"#root/{nid}?viewMode=attachments&attachmentId={aid}\"")),
                Some((nid, None)) => out.push_str(&format!("href=\"#root/{nid}\"")),
                None => out.push_str(m.as_str()),
            }
        }
        out.push_str(&c[last..]);
        c = out;

        let att_re = Regex::new(r#"data-attachment-id="([a-zA-Z0-9_]+)""#).unwrap();
        let ids: Vec<String> = att_re.captures_iter(&c).map(|x| x[1].to_string()).collect();
        for id in ids { let n = self.new_att_id(&id); c = c.replace(&format!("data-attachment-id=\"{id}\""), &format!("data-attachment-id=\"{n}\"")); }
        if let Some(m) = meta {
            for a in m.attributes.iter().flatten().filter(|a| a.kind == "relation" && a.name == "includeNoteLink") {
                // only notes that are part of this import move to new ids ("root" or notes outside it stay as written)
                if let Some(n) = self.lookup(&a.value) { c = c.replace(&format!("data-note-id=\"{}\"", a.value), &format!("data-note-id=\"{n}\"")); }
            }
        }
        // like Trilium's safe import (on by default): imported HTML goes through the allow-list sanitiser
        crate::htmltree::sanitize_html(c.trim())
    }

    /// Create one note from metadata + data file; returns its new id.
    fn create_from_meta(&mut self, meta: &NoteMeta, parent: &str, prefix_path: &str) -> Result<Option<String>> {
        if meta.no_import == Some(true) { return Ok(None); }
        let old_id = meta.note_id.clone().unwrap_or_else(new_entity_id);
        if meta.is_clone == Some(true) {
            self.clones.push((old_id, parent.to_string(), meta.prefix.clone().filter(|p| !p.is_empty()), meta.note_position));
            return Ok(None);
        }
        let id = self.new_id(&old_id);
        let title = meta.title.clone().unwrap_or_else(|| "Untitled".into());
        let ntype = meta.ntype.clone().filter(|t| KNOWN_TYPES.contains(&t.as_str())).unwrap_or_else(|| "text".into());
        let mime = meta.mime.clone().unwrap_or_else(|| "text/html".into());
        let file_path = meta.data_file_name.as_ref().map(|f| format!("{prefix_path}{f}"));
        let raw = file_path.as_ref().and_then(|p| self.files.get(p).cloned());
        let string_note = is_string_note(&ntype, &mime);
        let position = meta.note_position;
        if let (Some(raw), true) = (&raw, string_note) {
            let mut text = String::from_utf8_lossy(raw).into_owned();
            if ntype == "text" && meta.format.as_deref() == Some("markdown") { text = crate::mdhtml::md_to_html(&text, &title, false); }
            if ntype == "text" || ntype == "book" { text = self.process_text(&text, &title, file_path.as_deref().unwrap_or(""), Some(meta)); }
            else if ntype == "relationMap" {
                for a in meta.attributes.iter().flatten().filter(|a| a.kind == "relation" && a.name == "relationMapLink") { if let Some(n) = self.lookup(&a.value) { text = text.replace(&a.value, &n); } }
            }
            self.db.insert_note_with_id(&id, parent, &title, &ntype, &mime, &text, meta.prefix.as_deref().filter(|p| !p.is_empty()), position)?;
        } else {
            self.db.insert_note_with_id(&id, parent, &title, &ntype, &mime, "", meta.prefix.as_deref().filter(|p| !p.is_empty()), position)?;
            if let Some(raw) = raw { self.db.set_note_bytes(&id, &raw)?; }
        }
        if meta.is_expanded == Some(true) { self.db.conn.execute("UPDATE branches SET isExpanded=1 WHERE branchId=?", [format!("{parent}_{id}")])?; }
        self.parents_touched.insert(parent.to_string());
        self.stats.notes += 1;
        for a in meta.attributes.iter().flatten() {
            let mut a = a.clone();
            if a.kind == "label-definition" { a.kind = "label".into(); a.name = format!("label:{}", a.name); }
            else if a.kind == "relation-definition" { a.kind = "label".into(); a.name = format!("relation:{}", a.name); }
            if a.kind != "label" && a.kind != "relation" { continue; }
            if a.kind == "relation" && ["internalLink", "imageLink", "relationMapLink", "includeNoteLink"].contains(&a.name.as_str()) { continue; }
            self.attributes.push((id.clone(), a));
        }
        for att in meta.attachments.iter().flatten() {
            let Some(bytes) = self.files.get(&format!("{prefix_path}{}", att.data_file_name)).cloned() else { continue };
            let aid = att.attachment_id.as_deref().map(|o| self.new_att_id(o)).unwrap_or_else(new_entity_id);
            let protected = self.db.note_protected(&id)?;
            let blob = self.db.save_blob_for(protected, &bytes, false)?;
            let title = if protected { self.db.encrypt_title(&att.title)? } else { att.title.clone() };
            self.db.conn.execute(
                "INSERT OR REPLACE INTO attachments(attachmentId,ownerId,role,mime,title,isProtected,position,blobId,dateModified,utcDateModified,isDeleted) VALUES(?,?,?,?,?,?,?,?,?,?,0)",
                params![aid, id, att.role, att.mime, title, protected as i64, att.position.unwrap_or(10), blob, now_local(), now_utc()])?;
            self.db.record_entity("attachments", "attachmentId", &aid, "utcDateModified")?;
            self.stats.attachments += 1;
        }
        if !meta.children.as_deref().unwrap_or(&[]).is_empty() {
            let dir = format!("{prefix_path}{}/", meta.dir_file_name.as_deref().unwrap_or(""));
            for c in meta.children.as_deref().unwrap_or(&[]) { self.create_from_meta(c, &id, &dir)?; }
        }
        Ok(Some(id))
    }
}

impl Db {
    /// Import a `.zip` (Trilium export or any folder zipped), a directory or a single file under `parent_id`.
    pub fn import_path(&self, path: &Path, parent_id: &str, opts: &ImportOpts) -> Result<ImportStats> {
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
        if path.is_file() && matches!(ext.as_str(), "opml" | "enex") {
            let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
            if meta.len() > 1024 * 1024 * 1024 { bail!("{} is larger than 1 GB", path.display()); }
            let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let text = String::from_utf8_lossy(&bytes);
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            if ext == "opml" { return self.import_opml(text, parent_id); }
            let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("Evernote import");
            return self.import_enex(text, name, parent_id);
        }
        let is_zip = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        let entries = if is_zip {
            read_zip(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?)?
        } else if path.is_dir() {
            let mut v = vec![];
            read_dir(path, path.file_name().and_then(|n| n.to_str()).unwrap_or("import"), &mut v)?;
            v
        } else {
            {
                let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
                if !meta.is_file() { bail!("{} is not a regular file", path.display()); }
                if meta.len() > 1024 * 1024 * 1024 { bail!("{} is larger than 1 GB", path.display()); }
                vec![Entry { path: path.file_name().and_then(|n| n.to_str()).unwrap_or("file").into(), data: std::fs::read(path)?, is_dir: false }]
            }
        };
        let mut opts = opts.clone();
        opts.single_file = !is_zip && !path.is_dir();
        if opts.archive_name.is_none() { opts.archive_name = path.file_name().and_then(|n| n.to_str()).map(String::from); }
        self.import_entries(entries, parent_id, &opts)
    }

    pub fn import_entries(&self, entries: Vec<Entry>, parent_id: &str, opts: &ImportOpts) -> Result<ImportStats> {
        let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)).optional()?;
        if exists.is_none() { bail!("import target {parent_id} not found"); }
        let entries: Vec<Entry> = entries.into_iter().filter(|e| {
            let b = basename(&e.path);
            !(e.path.starts_with("__MACOSX/") || b == ".DS_Store" || b.starts_with("._") || b.is_empty())
        }).collect();
        let is_trilium = entries.iter().any(|e| e.path == "!!!meta.json");
        match opts.format.as_deref() {
            Some("obsidian") => return self.import_obsidian(entries, parent_id, opts.archive_name.as_deref()),
            Some("notion") => return self.import_notion(entries, parent_id),
            Some("keep") => return self.import_keep(entries, parent_id),
            Some("anytype") => return self.import_anytype(entries, parent_id, opts.archive_name.as_deref()),
            Some("trilium") | Some("plain") => {}
            Some(other) => bail!("unknown import format '{other}' (obsidian, notion, keep, anytype or trilium)"),
            None if !is_trilium && crate::obsidian::looks_like_vault(&entries) => return self.import_obsidian(entries, parent_id, opts.archive_name.as_deref()),
            None if !is_trilium && crate::notion::looks_like_export(&entries) => return self.import_notion(entries, parent_id),
            None if !is_trilium && crate::anytype::looks_like_export(&entries) => return self.import_anytype(entries, parent_id, opts.archive_name.as_deref()),
            None if !is_trilium && crate::keep::looks_like_takeout(&entries) => return self.import_keep(entries, parent_id),
            None => {}
        }
        let tx = self.begin()?;
        let mut files = HashMap::new();
        // a Trilium archive is read through its metadata, so the bytes can move into `files`; only a plain folder
        // import needs them in both places
        let entries = if entries.iter().any(|e| e.path == "!!!meta.json") {
            for e in entries { if !e.is_dir { files.insert(e.path, e.data); } }
            vec![]
        } else {
            for e in &entries { if !e.is_dir { files.insert(e.path.clone(), e.data.clone()); } }
            entries
        };
        let meta: Option<MetaFile> = files.get("!!!meta.json").map(|b| serde_json::from_slice(b)).transpose().context("reading !!!meta.json")?;
        let mut imp = Importer { db: self, opts, id_map: HashMap::new(), att_map: HashMap::new(), stats: ImportStats::default(), parents_touched: HashSet::new(),
            files, meta: None, attributes: vec![], clones: vec![], text_notes: vec![] };
        if let Some(mf) = meta {
            // pre-assign every id so links and relations can refer to notes created later
            fn walk(n: &NoteMeta, imp: &mut Importer) {
                if n.no_import == Some(true) { return; }
                if let (Some(id), Some(false) | None) = (&n.note_id, n.is_clone) { imp.new_id(id); }
                for c in n.children.iter().flatten() { walk(c, imp); }
            }
            for f in &mf.files { walk(f, &mut imp); }
            let files_meta = mf.files.clone();
            imp.meta = Some(mf);
            let mut first = None;
            for f in &files_meta {
                if let Some(id) = imp.create_from_meta(f, parent_id, "")? { first.get_or_insert(id); }
            }
            imp.stats.root = first;
            // clones become extra placements once every note exists
            for (old, parent, prefix, pos) in std::mem::take(&mut imp.clones) {
                let Some(nid) = imp.lookup(&old) else { continue };
                let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [&nid], |r| r.get(0)).optional()?;
                if exists.is_some() {
                    let bid = format!("{parent}_{nid}");
                    let pos = match pos { Some(p) => p, None => self.conn.query_row("SELECT COALESCE(MAX(notePosition),0)+10 FROM branches WHERE parentNoteId=? AND isDeleted=0", [&parent], |r| r.get(0))? };
                    self.conn.execute("INSERT OR REPLACE INTO branches(branchId,noteId,parentNoteId,notePosition,prefix,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,?,?,0,0,?)", params![bid, nid, parent, pos, prefix, now_utc()])?;
                    self.record_entity("branches", "branchId", &bid, "utcDateModified")?;
                    imp.stats.clones += 1;
                }
            }
        } else {
            plain_import(&mut imp, &entries, parent_id)?;
        }
        // attributes last: relation targets may be created after their sources
        for (note, a) in std::mem::take(&mut imp.attributes) {
            let value = if a.kind == "relation" {
                if let Some(n) = imp.lookup(&a.value) { n }
                else if a.value == "root" || a.value.starts_with('_') { a.value.clone() }
                else {
                    // a note outside the import: keep the relation if that note exists here, otherwise drop it
                    let exists: Option<i64> = self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [&a.value], |r| r.get(0)).optional()?;
                    if exists.is_some() { a.value.clone() } else { continue }
                }
            } else { a.value.clone() };
            let id = new_entity_id();
            self.conn.execute("INSERT INTO attributes(attributeId,noteId,type,name,value,position,utcDateModified,isDeleted,isInheritable) VALUES(?,?,?,?,?,?,?,0,?)",
                params![id, note, a.kind, a.name, value, a.position.unwrap_or(10), now_utc(), a.is_inheritable.unwrap_or(false) as i64])?;
            self.record_entity("attributes", "attributeId", &id, "utcDateModified")?;
            imp.stats.attributes += 1;
        }
        // keep content-derived relations (internalLink etc.) in sync for imported text notes
        let new_ids: Vec<String> = imp.id_map.values().cloned().collect();
        for id in new_ids {
            let row: Option<(String, String)> = self.conn.query_row("SELECT type,mime FROM notes WHERE noteId=? AND isDeleted=0", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((t, m)) = row {
                let md = t == "code" && matches!(m.as_str(), "text/markdown" | "text/x-markdown" | "text/x-gfm");
                if t == "text" || md { if let Some(c) = self.note_content(&id)? { self.save_links(&id, &c, md)?; } }
            }
        }
        for p in &imp.parents_touched { self.sort_children_if_needed(p)?; }
        let stats = std::mem::take(&mut imp.stats);
        tx.commit()?;
        Ok(stats)
    }
}

/// Folder / archive without Trilium metadata: directories become notes, files become text/code/image/file notes.
fn plain_import(imp: &mut Importer, entries: &[Entry], parent_id: &str) -> Result<()> {
    // a path thousands of folders deep would cost quadratic time and leave a tree nothing can export
    if entries.iter().any(|e| e.path.matches('/').count() > 64) { bail!("the folders are nested more than 64 levels deep"); }
    // Notes are created in archive order (as Trilium's zip importer does), a folder when first met.
    let mut dirs: Vec<String> = entries.iter().filter(|e| e.is_dir).map(|e| e.path.trim_end_matches('/').to_string()).collect();
    for e in entries.iter().filter(|e| !e.is_dir) { let d = dirname(&e.path); if d != "." { dirs.push(d); } }
    let mut all: HashSet<String> = HashSet::new();
    for d in dirs { let mut cur = d.clone(); loop { all.insert(cur.clone()); let p = dirname(&cur); if p == "." { break; } cur = p; } }
    let mut ordered: Vec<String> = all.iter().cloned().collect();
    ordered.sort_by_key(|d| (d.matches('/').count(), d.clone()));
    let rank = |p: &str| match extname(p).to_lowercase().as_str() { ".md" | ".markdown" => 0, ".html" | ".htm" => 1, _ => 2 };
    let mut files_by_stem: HashMap<String, &Entry> = HashMap::new();
    for e in entries.iter().filter(|e| !e.is_dir).filter(|e| matches!(extname(&e.path).to_lowercase().as_str(), ".html" | ".htm" | ".md" | ".markdown" | ".txt")) {
        let stem = strip_ext(&e.path);
        if files_by_stem.get(&stem).is_none_or(|old| rank(&e.path) < rank(&old.path)) { files_by_stem.insert(stem, e); }
    }
    // "Foo.md" next to a folder "Foo" is that folder's own text
    let absorbed: HashSet<String> = files_by_stem.iter().filter(|(stem, _)| all.contains(*stem)).map(|(_, e)| e.path.clone()).collect();
    let single = imp.opts.single_file;
    let mut dir_notes: HashMap<String, String> = HashMap::new();

    for e in entries {
        let path = e.path.trim_end_matches('/');
        let parent_dir = if e.is_dir { dirname(path) } else { dirname(&e.path) };
        let mut chain: Vec<String> = vec![];
        let mut cur = if e.is_dir { path.to_string() } else { parent_dir.clone() };
        while cur != "." && !cur.is_empty() && !dir_notes.contains_key(&cur) { chain.push(cur.clone()); cur = dirname(&cur); }
        for d in chain.into_iter().rev() { make_dir_note(imp, &mut dir_notes, &files_by_stem, parent_id, &d)?; }
        if e.is_dir || absorbed.contains(&e.path) { continue; }
        let d = dirname(&e.path);
        let parent = if d == "." { parent_id.to_string() } else { dir_notes.get(&d).cloned().unwrap_or_else(|| parent_id.to_string()) };
        make_file_note(imp, e, &parent, single)?;
    }
    imp.stats.root = ordered.first().and_then(|d| dir_notes.get(d).cloned());
    resolve_plain_links(imp)?;
    Ok(())
}

/// Relative links and images between the imported files point at the notes they became.
fn resolve_plain_links(imp: &mut Importer) -> Result<()> {
    let db = imp.db;
    static HREF: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = HREF.get_or_init(|| Regex::new(r#"(href|src)="([^"]*)""#).unwrap());
    let image_ids: HashSet<String> = imp.id_map.iter().filter(|(k, _)| k.starts_with("file:")).filter(|(_, id)| db.conn.query_row("SELECT type FROM notes WHERE noteId=?", [id], |r| r.get::<_, String>(0)).is_ok_and(|t| t == "image")).map(|(_, id)| id.clone()).collect();
    for (id, path) in std::mem::take(&mut imp.text_notes) {
        let Some(html) = db.note_content(&id)? else { continue };
        if !html.contains("href=\"") && !html.contains("src=\"") { continue; }
        let folder = dirname(&path);
        let out = re.replace_all(&html, |c: &regex::Captures| {
            let raw = &c[2];
            let url = decode_uri_component(raw.split(['#', '?']).next().unwrap_or(""));
            if url.is_empty() || url.contains("://") || url.starts_with("mailto:") || url.starts_with('/') || url.starts_with("data:") { return c[0].to_string(); }
            let mut parts: Vec<&str> = if folder == "." { vec![] } else { folder.split('/').collect() };
            for seg in url.split('/') { match seg { "" | "." => {} ".." => { if parts.pop().is_none() { return c[0].to_string(); } } s => parts.push(s) } }
            let target = parts.join("/");
            let found = imp.id_map.get(&format!("file:{target}")).or_else(|| imp.id_map.get(&format!("dir:{target}"))).cloned();
            match found {
                Some(tid) if &c[1] == "src" && image_ids.contains(&tid) => format!("src=\"api/images/{tid}/{}\"", encode_uri_component(&basename(&target))),
                Some(tid) if &c[1] == "href" => format!("href=\"#root/{tid}\""),
                _ => c[0].to_string(),
            }
        }).into_owned();
        if out != html { db.store_imported_html(&id, &out, None)?; }
    }
    Ok(())
}

fn make_dir_note(imp: &mut Importer, dir_notes: &mut HashMap<String, String>, files_by_stem: &HashMap<String, &Entry>, parent_id: &str, d: &str) -> Result<()> {
    let db = imp.db;
    let parent = match dir_notes.get(&dirname(d)) { Some(p) if dirname(d) != "." => p.clone(), _ => parent_id.to_string() };
    let id = new_entity_id();
    let title = basename(d);
    let mut labels: Vec<(String, String)> = vec![];
    let (ntype, mime, content) = match files_by_stem.get(d) {
        Some(f) => {
            let conv = if imp.opts.keep_sources { None } else { crate::fileconv::convert(&basename(&f.path), &title, &f.data, false) };
            match conv {
                Some(c) => { labels = c.labels; let content = if c.ntype == "text" { imp.process_text(&c.content, &title, &f.path, None) } else { c.content }; (c.ntype.to_string(), c.mime, content) }
                None => { let (t, m) = type_for_file(&f.path); let c = String::from_utf8_lossy(&f.data).into_owned(); let c = if t == "text" && !c.is_empty() { imp.process_text(&c, &title, d, None) } else { c }; (t, m, c) }
            }
        }
        None => ("text".into(), "text/html".into(), String::new()),
    };
    db.insert_note_with_id(&id, &parent, &title, &ntype, &mime, &content, None, None)?;
    for (n, v) in labels { db.add_attribute(&id, "label", &n, &v, false)?; imp.stats.attributes += 1; }
    if ntype == "text" && !content.is_empty() { if let Some(f) = files_by_stem.get(d) { imp.text_notes.push((id.clone(), f.path.clone())); } }
    imp.id_map.insert(format!("dir:{d}"), id.clone());
    imp.parents_touched.insert(parent);
    imp.stats.notes += 1;
    dir_notes.insert(d.to_string(), id);
    Ok(())
}

fn make_file_note(imp: &mut Importer, f: &Entry, parent: &str, single: bool) -> Result<()> {
    let db = imp.db;
    let name = basename(&f.path);
    let (ntype, mime) = type_for_file(&name);
    let ext = extname(&name);
    let title = if ntype == "text" || matches!(ext.to_lowercase().as_str(), ".md" | ".markdown") { name[..name.len() - ext.len()].to_string() } else { name.clone() };
    let id = new_entity_id();
    let ext_title = if ext.is_empty() { name.clone() } else { name[..name.len() - ext.len()].to_string() };
    if let Some(c) = (!imp.opts.keep_sources).then(|| crate::fileconv::convert(&name, &ext_title, &f.data, single)).flatten() {
        let content = if c.ntype == "text" { imp.process_text(&c.content, &ext_title, &f.path, None) } else { c.content.clone() };
        db.insert_note_with_id(&id, parent, &ext_title, c.ntype, &c.mime, &content, None, None)?;
        if c.ntype == "text" { imp.text_notes.push((id.clone(), f.path.clone())); }
        for (n, v) in c.labels { db.add_attribute(&id, "label", &n, &v, false)?; imp.stats.attributes += 1; }
    } else if ntype == "image" || ntype == "file" || crate::fileconv::decode_text(&f.data).is_none() {
        let ntype = if ntype == "image" { "image" } else { "file" };
        db.insert_note_with_id(&id, parent, &name, ntype, &mime, "", None, None)?;
        db.set_note_bytes(&id, &f.data)?;
    } else {
        let text = crate::fileconv::decode_text(&f.data).unwrap_or_default();
        let text = if ntype == "text" { imp.process_text(&text, &title, &f.path, None) } else { text };
        db.insert_note_with_id(&id, parent, &title, &ntype, &mime, &text, None, None)?;
        if ntype == "text" { imp.text_notes.push((id.clone(), f.path.clone())); }
    }
    imp.id_map.insert(format!("file:{}", f.path), id);
    imp.parents_touched.insert(parent.to_string());
    imp.stats.notes += 1;
    Ok(())
}

pub fn read_zip<R: Read + Seek>(r: R) -> Result<Vec<Entry>> { read_zip_budget(r, &mut ZIP_BUDGET.clone()) }

pub(crate) const ZIP_BUDGET: u64 = 1024 * 1024 * 1024;

/// Like `read_zip`, drawing the uncompressed size from a shared `budget` (nested archives must not each get a fresh one).
pub(crate) fn read_zip_budget<R: Read + Seek>(r: R, budget: &mut u64) -> Result<Vec<Entry>> {
    let mut z = zip::ZipArchive::new(r).context("not a valid zip file")?;
    let mut out = vec![];
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let name = f.name()?.replace('\\', "/");
        // never trust archive paths
        if name.starts_with('/') || name.split('/').any(|s| s == "..") { continue; }
        if f.is_dir() { out.push(Entry { path: name.trim_end_matches('/').to_string(), data: vec![], is_dir: true }); continue; }
        // header sizes are attacker-controlled: budget what is actually read, and never pre-allocate by claim
        if f.size() > *budget { bail!("archive is larger than 2 GB uncompressed"); }
        *budget -= f.size();
        let mut data = Vec::with_capacity((f.size() as usize).min(16 * 1024 * 1024));
        let declared = f.size();
        let got = (&mut f).take(declared.saturating_add(1)).read_to_end(&mut data)?;
        if got as u64 > declared { bail!("archive entry '{name}' is larger than its header claims"); }
        out.push(Entry { path: name, data, is_dir: false });
    }
    Ok(out)
}

fn read_dir(dir: &Path, rel: &str, out: &mut Vec<Entry>) -> Result<()> {
    out.push(Entry { path: rel.to_string(), data: vec![], is_dir: true });
    let mut items: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for it in items {
        let name = it.file_name().to_string_lossy().into_owned();
        let p = it.path();
        let relp = format!("{rel}/{name}");
        let ft = it.file_type()?;
        // an Obsidian vault is recognised by its `.obsidian` folder; keep that marker and the property-type registry
        if name == ".obsidian" && ft.is_dir() {
            out.push(Entry { path: relp.clone(), data: vec![], is_dir: true });
            if let Ok(bytes) = std::fs::read(p.join("types.json")) { if bytes.len() < 1 << 20 { out.push(Entry { path: format!("{relp}/types.json"), data: bytes, is_dir: false }); } }
            continue;
        }
        if name.starts_with('.') { continue; }
        if ft.is_symlink() { continue; }
        if ft.is_dir() { read_dir(&p, &relp, out)?; } else { out.push(Entry { path: relp, data: std::fs::read(&p)?, is_dir: false }); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use std::io::Cursor;

    #[test]
    fn percent_decoding_survives_multibyte_text() {
        assert_eq!(decode_uri_component("a%日x"), "a%日x");
        assert_eq!(decode_uri_component("a%20b%e6%97%a5"), "a b日");
        assert_eq!(decode_uri_component("100%"), "100%");
    }

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn png() -> Vec<u8> {
        let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8 * 30, y as u8 * 30, 7]));
        let mut out = Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn titles(db: &Db, parent: &str) -> Vec<String> {
        let m = Model::load(db).unwrap();
        m.children.get(parent).map(|c| c.iter().map(|b| m.display_title(b)).collect()).unwrap_or_default()
    }

    fn find(db: &Db, title: &str, under: &str) -> String {
        let m = Model::load(db).unwrap();
        m.children[under].iter().map(|b| b.note_id.clone()).find(|id| m.title_of(id) == title).unwrap_or_else(|| panic!("no '{title}' under {under}"))
    }

    fn build_source(db: &Db) -> (String, String, String) {
        let top = db.create_note("root", "Export me", "text", "text/html", "<p>top</p>").unwrap();
        let a = db.create_note(&top, "Child A", "text", "text/html", "").unwrap();
        let b = db.create_note(&top, "Child B", "text", "text/html", "<p>b body</p>").unwrap();
        let att = db.add_attachment(&a, "pic.png", "image/png", "image", &png()).unwrap();
        db.set_note_content(&a, &format!(r##"<p>A links to <a href="#root/{top}/{b}">B</a> and shows <img src="api/attachments/{att}/image/pic.png"></p>"##)).unwrap();
        db.add_attribute(&a, "label", "tag", "hello", true).unwrap();
        db.add_attribute(&a, "relation", "friend", &b, false).unwrap();
        db.add_attribute(&b, "label", "done", "", false).unwrap();
        db.clone_note(&b, &a, Some("pre")).unwrap();
        db.create_note(&top, "code.py", "code", "text/x-python", "print('hi')\n").unwrap();
        db.create_file_note(&top, "photo.png", "image/png", &png()).unwrap();
        db.create_note(&top, "Same", "text", "text/html", "one").unwrap();
        db.create_note(&top, "Same", "text", "text/html", "two").unwrap();
        db.create_note(&top, "wei/rd:na*me?", "text", "text/html", "x").unwrap();
        (top, a, b)
    }

    #[test]
    fn filename_helpers() {
        assert_eq!(sanitize_filename("a/b:c*d?e\"f<g>h|i"), "abcdefghi");
        assert_eq!(sanitize_filename("con.txt"), "");
        assert_eq!(sanitize_filename("trailing. "), "trailing");
        assert_eq!(sanitize_filename(".."), "");
        assert_eq!(encode_uri_component("a b/ü.html"), "a%20b%2F%C3%BC.html");
        assert_eq!(decode_uri_component("a%20b%2F%C3%BC"), "a b/ü");
        let mut ex = HashMap::new();
        assert_eq!(unique_name(&mut ex, "Same.html"), "Same.html");
        assert_eq!(unique_name(&mut ex, "same.html"), "1_same.html");
        assert_eq!(map_extension(Some("code"), "text/x-python", "").as_deref(), Some("py"));
        assert_eq!(map_extension(Some("file"), "application/pdf", "").as_deref(), Some("pdf"));
        assert_eq!(map_extension(Some("image"), "image/jpg", ".jpeg"), None, "an existing extension is kept");
    }

    #[test]
    fn export_has_trilium_layout() {
        let (_d, db) = fixture();
        let (top, _a, _b) = build_source(&db);
        let mut buf = Cursor::new(vec![]);
        let st = db.export_zip(&top, &mut buf).unwrap();
        assert!(st.notes >= 8 && st.attachments == 1, "{st:?}");
        let entries = read_zip(Cursor::new(buf.into_inner())).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names[0], "!!!meta.json");
        for expect in ["Export me.html", "Export me/Child A.html", "Export me/code.py", "Export me/photo.png", "Export me/Same.html", "Export me/1_Same.html", "navigation.html", "index.html", "style.css"] {
            assert!(names.contains(&expect), "missing {expect} in {names:?}");
        }
        assert!(names.iter().any(|n| n.ends_with("pic.png") && n.starts_with("Export me/")), "attachment file: {names:?}");
        assert!(names.iter().any(|n| n.contains(".clone.html")), "clone placeholder: {names:?}");
        let meta: MetaFile = serde_json::from_slice(&entries[0].data).unwrap();
        assert_eq!(meta.format_version, 2);
        assert_eq!(meta.files[0].title.as_deref(), Some("Export me"));
        assert!(meta.files[1..].iter().all(|f| f.no_import == Some(true)));
        let page = String::from_utf8(entries.iter().find(|e| e.path == "Export me/Child A.html").unwrap().data.clone()).unwrap();
        assert!(page.contains("<h1 data-trilium-h1>Child A</h1>") && page.contains("../style.css"), "{page}");
        assert!(page.contains(r#"href="Child%20B.html""#) || page.contains("Child%20B.html"), "links point at files: {page}");
        assert!(!page.contains(&top), "no raw note paths survive: {page}");
    }

    #[test]
    fn export_import_round_trip_preserves_structure_content_and_links() {
        let (_d, db) = fixture();
        let (top, a, b) = build_source(&db);
        let mut buf = Cursor::new(vec![]);
        db.export_zip(&top, &mut buf).unwrap();
        let target = db.create_note("root", "Imported here", "text", "text/html", "").unwrap();
        let st = db.import_entries(read_zip(Cursor::new(buf.into_inner())).unwrap(), &target, &ImportOpts::default()).unwrap();
        assert!(st.notes >= 8 && st.attachments == 1 && st.clones == 1, "{st:?}");
        let top2 = find(&db, "Export me", &target);
        assert_ne!(top2, top, "imported notes get fresh ids");
        assert_eq!(db.note_content(&top2).unwrap().unwrap(), "<p>top</p>");
        assert_eq!(titles(&db, &top2), ["Child A", "Child B", "code.py", "photo.png", "Same", "Same", "wei/rd:na*me?"], "same order and original (unsanitised) titles");
        let a2 = find(&db, "Child A", &top2);
        let b2 = find(&db, "Child B", &top2);
        assert!(titles(&db, &a2).contains(&"pre - Child B".to_string()), "the clone is re-created with its prefix: {:?}", titles(&db, &a2));
        assert_eq!(db.note_content(&b2).unwrap().unwrap(), "<p>b body</p>");
        // attachments and the link/image rewiring inside text
        let atts = db.attachments(&a2).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(db.attachment_bytes(&atts[0].id).unwrap().unwrap(), png());
        let body = db.note_content(&a2).unwrap().unwrap();
        assert!(body.contains(&format!("#root/{b2}")) && body.contains(&format!("api/attachments/{}/image/pic.png", atts[0].id)), "{body}");
        assert!(!body.contains(&a) && !body.contains(&b) && !body.contains(&top), "no old ids: {body}");
        // attributes: values kept, relation retargeted to the imported copy, inheritable flag kept
        let rows = db.attribute_rows(&a2).unwrap();
        assert!(rows.iter().any(|r| r.name == "tag" && r.value == "hello" && r.inheritable));
        assert!(rows.iter().any(|r| r.name == "friend" && r.value == b2), "{rows:?}");
        // content-derived link relations were rebuilt for the new ids
        assert!(db.backlinks(&b2).unwrap().iter().any(|l| l.source == a2 && l.relation == "internalLink"));
        // code and image/file notes keep type, mime and exact bytes
        let c = find(&db, "code.py", &top2);
        let (t, m): (String, String) = db.conn.query_row("SELECT type,mime FROM notes WHERE noteId=?", [&c], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t.as_str(), m.as_str()), ("code", "text/x-python"));
        assert_eq!(db.note_content(&c).unwrap().unwrap(), "print('hi')\n");
        let ph = find(&db, "photo.png", &top2);
        assert_eq!(db.note_bytes(&ph).unwrap().unwrap(), png());
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn markdown_export_is_readable_and_imports_back() {
        let (_d, db) = fixture();
        let (top, _a, _b) = build_source(&db);
        db.set_note_content(&top, "<h2>Intro</h2><p>Some <strong>bold</strong> text.</p><ul><li>one</li><li>two</li></ul>").unwrap();
        let mut buf = Cursor::new(vec![]);
        db.export_zip_as(&top, &mut buf, true).unwrap();
        let entries = read_zip(Cursor::new(buf.clone().into_inner())).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(names.contains(&"Export me.md") && names.contains(&"Export me/Child A/pre - Child B.md") && names.contains(&"Export me/Child A.md"), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("navigation.html") || n.ends_with("style.css")), "{names:?}");
        let text = |p: &str| String::from_utf8(entries.iter().find(|e| e.path == p).unwrap().data.clone()).unwrap();
        assert_eq!(text("Export me.md"), "# Export me\r\n## Intro\n\nSome **bold** text.\n\n*   one\n*   two\n");
        let a_md = text("Export me/Child A.md");
        assert!(a_md.contains("[B](Child%20B.md)") && a_md.contains("![](Child%20A_pic.png)") || a_md.contains("![](") , "{a_md}");
        assert!(a_md.starts_with("# Child A"), "{a_md}");
        // importing the Markdown archive gives HTML notes with working links again
        let target = db.create_note("root", "From markdown", "text", "text/html", "").unwrap();
        db.import_entries(entries, &target, &ImportOpts::default()).unwrap();
        let top2 = find(&db, "Export me", &target);
        let body = db.note_content(&top2).unwrap().unwrap();
        assert!(body.contains("<h2>Intro</h2>") && body.contains("<strong>bold</strong>") && body.contains("<ul><li>one</li><li>two</li></ul>") && !body.contains("<h1"), "{body}");
        let a2 = find(&db, "Child A", &top2);
        let b2 = find(&db, "Child B", &a2);
        assert!(db.note_content(&a2).unwrap().unwrap().contains(&format!("#root/{b2}")), "{}", db.note_content(&a2).unwrap().unwrap());
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn vault_and_notion_archives_are_detected_and_forced_formats_work() {
        let (d, db) = fixture();
        let zip_path = d.path().join("My Vault.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            for (n, c) in [(".obsidian/app.json", "{}"), ("Note.md", "---\ntags: [x]\n---\nHello [[Other]]"), ("Other.md", "o")] {
                z.start_file(n, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(c.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        let st = db.import_path(&zip_path, "root", &ImportOpts::default()).unwrap();
        let root = st.root.unwrap();
        assert_eq!(db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [&root], |r| r.get::<_, String>(0)).unwrap(), "My Vault");
        let note = find(&db, "Note", &root);
        assert!(db.has_label(&note, "x").unwrap());
        // forcing the generic importer treats the same archive as plain files
        db.import_path(&zip_path, "root", &ImportOpts { format: Some("trilium".into()), keep_sources: true, ..Default::default() }).unwrap();
        let plain: i64 = db.conn.query_row("SELECT COUNT(*) FROM notes WHERE type='code' AND title='Note' AND isDeleted=0", [], |r| r.get(0)).unwrap();
        assert_eq!(plain, 1, "forcing the generic importer keeps the Markdown as a source note");
        assert!(db.import_path(&zip_path, "root", &ImportOpts { format: Some("bogus".into()), ..Default::default() }).is_err());
    }

    #[test]
    fn an_obsidian_vault_folder_is_detected_too() {
        let (d, db) = fixture();
        let v = d.path().join("vault");
        std::fs::create_dir_all(v.join(".obsidian")).unwrap();
        std::fs::write(v.join(".obsidian/types.json"), r#"{"types":{"rating":"number"}}"#).unwrap();
        std::fs::write(v.join(".obsidian/workspace.json"), "{}").unwrap();
        std::fs::write(v.join("A.md"), "---\nrating: 4\n---\nsee [[B]] ==hl==").unwrap();
        std::fs::write(v.join("B.md"), "b").unwrap();
        let st = db.import_path(&v, "root", &ImportOpts::default()).unwrap();
        let a = find(&db, "A", &st.root.unwrap());
        let html = db.note_content(&a).unwrap().unwrap();
        assert!(html.contains("#root/") && html.contains("background-color") && !html.contains("[["), "{html}");
        assert_eq!(db.label_value(&a, "label:rating").unwrap().as_deref(), Some("promoted,single,number,alias=rating"));
    }

    #[test]
    fn latin1_and_windows_text_files_become_notes() {
        let (d, db) = fixture();
        let src = d.path().join("old");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("notes.txt"), b"caf\xe9 au lait, avec du texte fran\xe7ais").unwrap();
        std::fs::write(src.join("page.html"), b"<html><body><p>Gr\xfc\xdfe aus M\xfcnchen und andere Orte</p></body></html>").unwrap();
        std::fs::write(src.join("script.py"), b"# caf\xe9\nprint('na\xefve')\n").unwrap();
        std::fs::write(src.join("blob.dat"), [0u8, 159, 146, 150, 0, 7]).unwrap();
        let st = db.import_path(&src, "root", &ImportOpts::default()).unwrap();
        let top = st.root.unwrap();
        let ty = |t: &str| -> (String, String) { let id = find(&db, t, &top); db.conn.query_row("SELECT type,mime FROM notes WHERE noteId=?", [&id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap() };
        assert_eq!(ty("notes.txt").0, "code");
        assert_eq!(db.note_content(&find(&db, "notes.txt", &top)).unwrap().unwrap(), "caf\u{e9} au lait, avec du texte fran\u{e7}ais");
        assert!(db.note_content(&find(&db, "page", &top)).unwrap().unwrap().contains("Gr\u{fc}\u{df}e aus M\u{fc}nchen"));
        assert_eq!(db.note_content(&find(&db, "script.py", &top)).unwrap().unwrap(), "# caf\u{e9}\nprint('na\u{ef}ve')\n");
        assert_eq!(ty("blob.dat").0, "file");
    }

    #[test]
    fn converted_files_keep_working_links_and_utf16_text_is_text() {
        let (d, db) = fixture();
        let src = d.path().join("docs");
        std::fs::create_dir_all(src.join("Foo")).unwrap();
        std::fs::write(src.join("Foo.md"), "folder text").unwrap();
        std::fs::write(src.join("Foo.txt"), "other text").unwrap();
        std::fs::write(src.join("Foo/in.md"), "inner").unwrap();
        std::fs::write(src.join("Bar.md"), "[foo](Foo/in.md) ![i](pic.png)").unwrap();
        std::fs::write(src.join("pic.png"), png()).unwrap();
        let mut utf16: Vec<u8> = vec![0xff, 0xfe];
        for u in "<p>caf\u{e9} \u{65e5}\u{672c}</p>".encode_utf16() { utf16.extend(u.to_le_bytes()); }
        std::fs::write(src.join("wide.html"), &utf16).unwrap();
        std::fs::write(src.join("code.py"), b"\xef\xbb\xbfprint(1)\n").unwrap();
        let st = db.import_path(&src, "root", &ImportOpts::default()).unwrap();
        let top = st.root.unwrap();
        let foo = find(&db, "Foo", &top);
        assert_eq!(db.note_content(&foo).unwrap().unwrap(), "<p>folder text</p>", "Foo.md wins over Foo.txt as the folder's text");
        assert_eq!(titles(&db, &foo), ["in"]);
        let bar = find(&db, "Bar", &top);
        let html = db.note_content(&bar).unwrap().unwrap();
        let inner = find(&db, "in", &foo);
        assert!(html.contains(&format!("#root/{inner}")) && html.contains("api/images/"), "{html}");
        let wide = find(&db, "wide", &top);
        assert!(db.note_content(&wide).unwrap().unwrap().contains("caf\u{e9} \u{65e5}\u{672c}"));
        assert_eq!(db.note_content(&find(&db, "code.py", &top)).unwrap().unwrap(), "print(1)\n", "BOM dropped");
    }

    #[test]
    fn plain_folders_and_files_import() {
        let (d, db) = fixture();
        let src = d.path().join("notes");
        std::fs::create_dir_all(src.join("Projects/Alpha")).unwrap();
        std::fs::write(src.join("Projects/Alpha/readme.md"), "# Alpha\n\nsee [beta](../Beta/todo.md)").unwrap();
        std::fs::create_dir_all(src.join("Projects/Beta")).unwrap();
        std::fs::write(src.join("Projects/Beta/todo.txt"), "- one\n- two\n").unwrap();
        std::fs::write(src.join("Projects/page.html"), "<html><body><h1>page</h1><p>hello</p></body></html>").unwrap();
        std::fs::write(src.join("Projects/pic.png"), png()).unwrap();
        std::fs::write(src.join("Projects/blob.bin"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(src.join("Projects/.hidden"), "x").unwrap();
        let st = db.import_path(&src, "root", &ImportOpts::default()).unwrap();
        assert!(st.notes >= 8, "{st:?}");
        let top = find(&db, "notes", "root");
        let projects = find(&db, "Projects", &top);
        let alpha = find(&db, "Alpha", &projects);
        let readme = find(&db, "readme", &alpha);
        let (t, m): (String, String) = db.conn.query_row("SELECT type,mime FROM notes WHERE noteId=?", [&readme], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t.as_str(), m.as_str()), ("text", "text/html"), "markdown becomes a text note, like Trilium's importer");
        assert!(db.note_content(&readme).unwrap().unwrap().contains("<p>see <a href=\"../Beta/todo.md\">beta</a></p>"));
        // ... unless the sources are to be kept as they are
        let kept = db.import_path(&src, "root", &ImportOpts { keep_sources: true, ..Default::default() }).unwrap();
        let top2 = kept.root.unwrap_or_default();
        let _ = top2;
        let (t2, m2): (String, String) = db.conn.query_row("SELECT type,mime FROM notes WHERE title='readme' AND type='code' AND isDeleted=0", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t2.as_str(), m2.as_str()), ("code", "text/x-markdown"));
        let page = find(&db, "page", &projects);
        let html = db.note_content(&page).unwrap().unwrap();
        assert_eq!(html, "<p>hello</p>", "html wrapper and the title heading are stripped: {html}");
        let pic = find(&db, "pic.png", &projects);
        assert_eq!(db.note_bytes(&pic).unwrap().unwrap(), png());
        let blob = find(&db, "blob.bin", &projects);
        let bt: String = db.conn.query_row("SELECT type FROM notes WHERE noteId=?", [&blob], |r| r.get(0)).unwrap();
        assert_eq!(bt, "file");
        assert!(!titles(&db, &projects).iter().any(|t| t.contains("hidden")));
        // single file import
        let one = d.path().join("single.md");
        std::fs::write(&one, "just text").unwrap();
        let before = titles(&db, "root").len();
        db.import_path(&one, "root", &ImportOpts::default()).unwrap();
        assert_eq!(titles(&db, "root").len(), before + 1);
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn hostile_archives_are_contained() {
        let (_d, db) = fixture();
        let mut z = zip::ZipWriter::new(Cursor::new(vec![]));
        z.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"x").unwrap();
        z.start_file("/abs.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"x").unwrap();
        z.start_file("ok/fine.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"fine").unwrap();
        let bytes = z.finish().unwrap().into_inner();
        let entries = read_zip(Cursor::new(bytes)).unwrap();
        assert_eq!(entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["ok/fine.txt"]);
        db.import_entries(entries, "root", &ImportOpts::default()).unwrap();
        assert!(db.import_path(Path::new("/nonexistent.zip"), "root", &ImportOpts::default()).is_err());
        assert!(db.import_entries(vec![], "nosuchparent", &ImportOpts::default()).is_err());
    }

    #[test]
    fn review_regressions_names_depth_and_sanitised_imports() {
        use std::io::Cursor;
        let (_d, db) = fixture();
        // titles that differ only by case or look like generated names, and ones that sanitise to nothing
        let top = db.create_note("root", "Top", "text", "text/html", "<p>t</p>").unwrap();
        for t in ["Same", "same", "1_same", "???", "con ", ".."] {
            let k = db.create_note(&top, t, "text", "text/html", "<p>x</p>").unwrap();
            db.create_note(&k, "child", "text", "text/html", "<p>y</p>").unwrap();
        }
        let mut buf = Cursor::new(vec![]);
        db.export_zip(&top, &mut buf).unwrap_or_else(|e| panic!("{e:#}"));
        let mut md = Cursor::new(vec![]);
        db.export_zip_as(&top, &mut md, true).unwrap_or_else(|e| panic!("{e:#}"));
        // a zip import is sanitised like every other HTML import
        let entries = vec![Entry { path: "p.html".into(), data: br#"<p onclick="evil()">x</p><script>alert(1)</script><a href="javascript:alert(1)">l</a>"#.to_vec(), is_dir: false }];
        db.import_entries(entries, &top, &ImportOpts::default()).unwrap();
        let id: String = db.conn.query_row("SELECT noteId FROM notes WHERE title='p' AND isDeleted=0", [], |r| r.get(0)).unwrap();
        let c = db.note_content(&id).unwrap().unwrap();
        assert!(!c.contains("onclick") && !c.contains("<script") && !c.contains("javascript:"), "{c}");
        // absurdly deep folder paths are refused up front
        let deep = format!("{}f.txt", "d/".repeat(70));
        assert!(db.import_entries(vec![Entry { path: deep, data: b"x".to_vec(), is_dir: false }], &top, &ImportOpts::default()).is_err());
        assert_eq!(sanitize_filename("con "), "");
    }
}
