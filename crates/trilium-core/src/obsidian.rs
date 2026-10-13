//! Obsidian vault import (a zipped or plain folder of Markdown files), ported from Trilium's `import/obsidian/`.
//! Every `.md` file becomes a text note (Markdown rendered with Obsidian's callouts, `==highlights==`, `%%comments%%`,
//! wikilinks and embeds), folders become container notes, front matter becomes labels, files that notes embed or link
//! become attachments of that note, and unreferenced files become file/image notes. `.obsidian/` and other
//! dot-prefixed entries are skipped. Excalidraw drawings become canvas notes (embedded images as attachments titled with their fileId).
use crate::{
    db::Db,
    formats::{promoted_definition, to_attribute_name},
    htmltree::{El, Node, decode_entities, escape_attr, escape_text, map_els, parse, serialize},
    mdhtml::md_to_html,
    transfer::{Entry, ImportStats},
};
use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Does this archive look like an Obsidian vault (an `.obsidian/` folder anywhere)?
pub fn looks_like_vault(entries: &[Entry]) -> bool {
    entries.iter().any(|e| e.path.replace('\\', "/").split('/').any(|s| s == ".obsidian"))
}

/// Approximates JavaScript's `localeCompare` (ICU root collation): letters compare ignoring case and accents first,
/// then accents, then lower case before upper case.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use unicode_normalization::UnicodeNormalization;
    let primary = |s: &str| -> String { s.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).flat_map(char::to_lowercase).collect() };
    primary(a).cmp(&primary(b))
        .then_with(|| a.nfd().collect::<String>().to_lowercase().cmp(&b.nfd().collect::<String>().to_lowercase()))
        .then_with(|| b.cmp(a))
}

/// `LZString.decompressFromBase64` (the compression the Excalidraw plugin uses for drawings).
fn lz_decompress_base64(input: &str) -> Option<String> {
    const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
    let vals: Vec<u32> = input.chars().map(|c| ALPHABET.find(c).map(|p| p as u32)).collect::<Option<_>>()?;
    if vals.is_empty() { return None; }
    let reset: u32 = 32;
    let length = vals.len();
    let mut val = vals[0];
    let mut position = reset;
    let mut index = 1usize;
    let read = |n: u32, val: &mut u32, position: &mut u32, index: &mut usize| -> Option<u32> {
        let mut bits = 0u32;
        for p in 0..n {
            let resb = *val & *position;
            *position >>= 1;
            if *position == 0 { *position = reset; *val = if *index < length { vals[*index] } else { 0 }; *index += 1; }
            if resb > 0 { bits |= 1 << p; }
        }
        Some(bits)
    };
    let mut dict: Vec<Vec<u16>> = vec![vec![0], vec![1], vec![2]];
    let mut enlarge_in = 4u32;
    let mut num_bits = 3u32;
    let first = match read(2, &mut val, &mut position, &mut index)? {
        0 => read(8, &mut val, &mut position, &mut index)?,
        1 => read(16, &mut val, &mut position, &mut index)?,
        _ => return None,
    };
    let mut w: Vec<u16> = vec![first as u16];
    dict.push(w.clone());
    let mut result: Vec<u16> = w.clone();
    let mut dict_units = 0usize;
    loop {
        if index > length + 1 { return None; }
        let c = read(num_bits, &mut val, &mut position, &mut index)?;
        let code = match c {
            0 => { let v = read(8, &mut val, &mut position, &mut index)?; dict.push(vec![v as u16]); enlarge_in -= 1; dict.len() - 1 }
            1 => { let v = read(16, &mut val, &mut position, &mut index)?; dict.push(vec![v as u16]); enlarge_in -= 1; dict.len() - 1 }
            2 => return String::from_utf16(&result).ok(),
            n => n as usize,
        };
        if enlarge_in == 0 { enlarge_in = 1 << num_bits; num_bits += 1; }
        let entry: Vec<u16> = if code < dict.len() { dict[code].clone() } else if code == dict.len() { let mut e = w.clone(); e.push(*w.first()?); e } else { return None };
        dict_units += entry.len();
        result.extend_from_slice(&entry);
        let mut nw = w.clone();
        nw.push(entry[0]);
        dict.push(nw);
        enlarge_in -= 1;
        w = entry;
        if enlarge_in == 0 { enlarge_in = 1 << num_bits; num_bits += 1; }
        // a hostile stream grows both the output and (quadratically) the dictionary: stop at sane sizes
        if result.len() > 16 << 20 || dict_units > 32 << 20 { return None; }
    }
}

/// An Excalidraw-plugin drawing as (canvas note content, `fileId` → embedded vault file) — None when no scene can be read.
fn parse_excalidraw(md: &str) -> Option<(String, Vec<(String, String)>)> {
    static COMPRESSED: OnceLock<Regex> = OnceLock::new();
    static PLAIN: OnceLock<Regex> = OnceLock::new();
    let compressed = COMPRESSED.get_or_init(|| Regex::new(r"```compressed-json\r?\n([\s\S]*?)```").unwrap());
    let plain = PLAIN.get_or_init(|| Regex::new(r"##+\s+Drawing\s*\r?\n```json\r?\n([\s\S]*?)```").unwrap());
    let json = if let Some(c) = compressed.captures(md) { lz_decompress_base64(&c[1].replace(['\r', '\n'], ""))? } else { plain.captures(md)?[1].to_string() };
    let scene: serde_json::Value = serde_json::from_str(&json).ok().filter(serde_json::Value::is_object)?;
    let elements = scene.get("elements").filter(|e| e.is_array()).cloned().unwrap_or(serde_json::json!([]));
    let app_state = scene.get("appState").cloned().unwrap_or(serde_json::json!({}));
    let used: HashSet<String> = elements.as_array().into_iter().flatten().filter_map(|e| e.get("fileId").and_then(|f| f.as_str()).map(String::from)).collect();
    let mut embedded = vec![];
    if !used.is_empty() {
        static LINE: OnceLock<Regex> = OnceLock::new();
        for c in LINE.get_or_init(|| Regex::new(r"(?m)^([0-9a-fA-F]{6,}):\s*\[\[(.+?)\]\]\s*$").unwrap()).captures_iter(md) {
            if used.contains(&c[1]) { embedded.push((c[1].to_string(), c[2].split('|').next().unwrap_or("").trim().to_string())); }
        }
    }
    Some((format!("{{\"type\":\"excalidraw\",\"version\":2,\"elements\":{elements},\"files\":{{}},\"appState\":{app_state}}}"), embedded))
}

fn canvas_color(c: Option<&str>) -> &'static str {
    // Obsidian's six preset colours
    match c { Some("1") => "#e03131", Some("2") => "#e8590c", Some("3") => "#f08c00", Some("4") => "#2f9e44", Some("5") => "#1098ad", Some("6") => "#7048e8", _ => "#1e1e1e" }
}

/// An Obsidian JSON Canvas as an Excalidraw scene: each node is a rectangle with its text, edges are arrows.
fn canvas_to_excalidraw(json: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(json).ok().filter(serde_json::Value::is_object)?;
    let base = |id: &str, kind: &str, x: f64, y: f64, w: f64, h: f64, color: &str| serde_json::json!({
        "id": id, "type": kind, "x": x, "y": y, "width": w, "height": h, "angle": 0, "strokeColor": color, "backgroundColor": "transparent", "fillStyle": "solid",
        "strokeWidth": 2, "strokeStyle": "solid", "roughness": 1, "opacity": 100, "groupIds": [], "frameId": null, "roundness": null, "seed": 1, "version": 1,
        "versionNonce": 1, "isDeleted": false, "boundElements": null, "updated": 1, "link": null, "locked": false });
    let mut elements: Vec<serde_json::Value> = vec![];
    let mut boxes: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    for n in doc["nodes"].as_array().into_iter().flatten() {
        let (Some(id), Some(x), Some(y)) = (n["id"].as_str(), n["x"].as_f64(), n["y"].as_f64()) else { continue };
        let (w, h) = (n["width"].as_f64().unwrap_or(250.0), n["height"].as_f64().unwrap_or(60.0));
        let color = canvas_color(n["color"].as_str());
        boxes.insert(id.to_string(), (x, y, w, h));
        let kind = n["type"].as_str().unwrap_or("text");
        let label = match kind {
            "file" => n["file"].as_str().map(|f| format!("{}{}", f, n["subpath"].as_str().unwrap_or(""))).unwrap_or_default(),
            "link" => n["url"].as_str().unwrap_or("").to_string(),
            "group" => n["label"].as_str().unwrap_or("").to_string(),
            _ => n["text"].as_str().unwrap_or("").to_string(),
        };
        let mut rect = base(&format!("{id}-box"), "rectangle", x, y, w, h, color);
        rect["roundness"] = serde_json::json!({"type": 3});
        if kind == "group" { rect["strokeStyle"] = serde_json::json!("dashed"); }
        // only web links are carried over (a canvas link node could hold a javascript: URL)
        if kind == "link" && n["url"].as_str().is_some_and(|u| { let l = u.trim().to_ascii_lowercase(); l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:") }) { rect["link"] = n["url"].clone(); }
        if !label.is_empty() && kind != "group" {
            rect["boundElements"] = serde_json::json!([{"id": format!("{id}-text"), "type": "text"}]);
            let mut t = base(&format!("{id}-text"), "text", x + 8.0, y + 8.0, w - 16.0, (h - 16.0).max(20.0), color);
            let o = t.as_object_mut().expect("text element");
            for (k, v) in [("text", serde_json::json!(label)), ("originalText", serde_json::json!(label)), ("fontSize", serde_json::json!(16)), ("fontFamily", serde_json::json!(1)),
                ("textAlign", serde_json::json!("left")), ("verticalAlign", serde_json::json!("top")), ("baseline", serde_json::json!(14)), ("containerId", serde_json::json!(format!("{id}-box"))),
                ("lineHeight", serde_json::json!(1.25)), ("autoResize", serde_json::json!(true))] { o.insert(k.into(), v); }
            elements.push(rect);
            elements.push(t);
        } else {
            elements.push(rect);
            if kind == "group" && !label.is_empty() {
                let mut t = base(&format!("{id}-label"), "text", x, y - 26.0, w.max(60.0), 22.0, color);
                let o = t.as_object_mut().expect("text element");
                for (k, v) in [("text", serde_json::json!(label)), ("originalText", serde_json::json!(label)), ("fontSize", serde_json::json!(16)), ("fontFamily", serde_json::json!(1)),
                    ("textAlign", serde_json::json!("left")), ("verticalAlign", serde_json::json!("top")), ("baseline", serde_json::json!(14)), ("containerId", serde_json::Value::Null), ("lineHeight", serde_json::json!(1.25))] { o.insert(k.into(), v); }
                elements.push(t);
            }
        }
    }
    let anchor = |b: (f64, f64, f64, f64), side: Option<&str>| -> (f64, f64) {
        match side { Some("top") => (b.0 + b.2 / 2.0, b.1), Some("bottom") => (b.0 + b.2 / 2.0, b.1 + b.3), Some("left") => (b.0, b.1 + b.3 / 2.0), Some("right") => (b.0 + b.2, b.1 + b.3 / 2.0), _ => (b.0 + b.2 / 2.0, b.1 + b.3 / 2.0) }
    };
    for e in doc["edges"].as_array().into_iter().flatten() {
        let (Some(id), Some(from), Some(to)) = (e["id"].as_str(), e["fromNode"].as_str(), e["toNode"].as_str()) else { continue };
        let (Some(fb), Some(tb)) = (boxes.get(from), boxes.get(to)) else { continue };
        let (a, b) = (anchor(*fb, e["fromSide"].as_str()), anchor(*tb, e["toSide"].as_str()));
        let mut arrow = base(id, "arrow", a.0, a.1, (b.0 - a.0).abs(), (b.1 - a.1).abs(), canvas_color(e["color"].as_str()));
        let o = arrow.as_object_mut().expect("arrow element");
        o.insert("points".into(), serde_json::json!([[0, 0], [b.0 - a.0, b.1 - a.1]]));
        o.insert("startBinding".into(), serde_json::json!({"elementId": format!("{from}-box"), "focus": 0, "gap": 8}));
        o.insert("endBinding".into(), serde_json::json!({"elementId": format!("{to}-box"), "focus": 0, "gap": 8}));
        o.insert("startArrowhead".into(), serde_json::Value::Null);
        o.insert("endArrowhead".into(), if e["toEnd"].as_str() == Some("none") { serde_json::Value::Null } else { serde_json::json!("arrow") });
        o.insert("roundness".into(), serde_json::json!({"type": 2}));
        elements.push(arrow);
        if let Some(label) = e["label"].as_str().filter(|l| !l.is_empty()) {
            let mut t = base(&format!("{id}-label"), "text", (a.0 + b.0) / 2.0 - 40.0, (a.1 + b.1) / 2.0 - 10.0, 80.0, 20.0, "#1e1e1e");
            let o = t.as_object_mut().expect("text element");
            for (k, v) in [("text", serde_json::json!(label)), ("originalText", serde_json::json!(label)), ("fontSize", serde_json::json!(14)), ("fontFamily", serde_json::json!(1)),
                ("textAlign", serde_json::json!("center")), ("verticalAlign", serde_json::json!("middle")), ("baseline", serde_json::json!(12)), ("containerId", serde_json::Value::Null), ("lineHeight", serde_json::json!(1.25))] { o.insert(k.into(), v); }
            elements.push(t);
        }
    }
    Some(serde_json::json!({"type": "excalidraw", "version": 2, "elements": elements, "files": {}, "appState": {}}).to_string())
}

fn basename(p: &str) -> &str { p.rsplit('/').next().unwrap_or(p) }
fn parent_folder(p: &str) -> &str { p.rfind('/').map_or("", |i| &p[..i]) }
fn strip_md(p: &str) -> &str { p.strip_suffix(".md").unwrap_or(p) }
fn is_ignored(p: &str) -> bool { p.split('/').any(|s| s.starts_with('.')) }
fn is_markdown(p: &str) -> bool { p.to_lowercase().ends_with(".md") }
fn is_special(p: &str) -> bool { let l = p.to_lowercase(); l.ends_with(".canvas") || l.ends_with(".base") }

fn decode_percent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = crate::transfer::hex_pair(b[i + 1], b[i + 2]) { out.push(v); i += 3; continue; }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The prefix before the shallowest `.obsidian/`, else a single wrapper folder shared by everything, else none.
fn detect_vault_root(paths: &[String]) -> String {
    let mut root: Option<String> = None;
    for p in paths {
        let segs: Vec<&str> = p.split('/').collect();
        if let Some(i) = segs.iter().position(|s| *s == ".obsidian") {
            let prefix = if i == 0 { String::new() } else { format!("{}/", segs[..i].join("/")) };
            if root.as_ref().is_none_or(|r| prefix.len() < r.len()) { root = Some(prefix); }
        }
    }
    if let Some(r) = root { return r; }
    if let Some(first) = paths.first().and_then(|p| p.split('/').next()) {
        if paths.iter().all(|p| p.starts_with(&format!("{first}/"))) { return format!("{first}/"); }
    }
    String::new()
}

struct VaultNote { path: String, title: String, markdown: String }

pub(crate) struct Frontmatter { pub(crate) body: String, pub(crate) data: serde_json::Map<String, serde_json::Value> }

pub(crate) fn parse_frontmatter(md: &str) -> Frontmatter {
    static FM: OnceLock<Regex> = OnceLock::new();
    let re = FM.get_or_init(|| Regex::new(r"^---\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)").unwrap());
    let md = md.strip_prefix('\u{feff}').unwrap_or(md);
    let Some(m) = re.captures(md) else { return Frontmatter { body: md.to_string(), data: Default::default() } };
    let whole = m.get(0).map_or(0, |x| x.end());
    match serde_yaml_ng::from_str::<serde_json::Value>(&m[1]) {
        Ok(serde_json::Value::Object(map)) => Frontmatter { body: md[whole..].to_string(), data: map },
        // a non-mapping block is stripped but carries no data; malformed YAML leaves the note untouched
        Ok(_) => Frontmatter { body: md[whole..].to_string(), data: Default::default() },
        Err(_) => Frontmatter { body: md.to_string(), data: Default::default() },
    }
}

fn scalars(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Array(a) => a.iter().flat_map(scalars).collect(),
        serde_json::Value::Bool(b) => vec![b.to_string()],
        serde_json::Value::Number(n) => vec![n.to_string()],
        serde_json::Value::String(s) if !s.trim().is_empty() => vec![s.clone()],
        _ => vec![],
    }
}

/// Front matter → labels: tags and aliases specially, presentation keys dropped, everything else a value label
/// plus a promoted definition typed from `.obsidian/types.json`.
fn map_frontmatter(data: &serde_json::Map<String, serde_json::Value>, types: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut labels = vec![];
    for (key, raw) in data {
        let lower = key.to_lowercase();
        if matches!(lower.as_str(), "cssclasses" | "publish" | "permalink") { continue; }
        let values = scalars(raw);
        if lower == "tags" {
            for v in values { labels.push((to_attribute_name(v.trim_start_matches('#')), String::new())); }
            continue;
        }
        if lower == "aliases" {
            for v in values { labels.push(("alias".into(), v)); }
            continue;
        }
        let ty = types.get(key.as_str()).map(String::as_str);
        let name = to_attribute_name(key);
        for v in &values {
            let v = if ty == Some("datetime") {
                static T: OnceLock<Regex> = OnceLock::new();
                T.get_or_init(|| Regex::new(r"T(\d{2}:\d{2})(?::\d{2})?.*$").unwrap()).replace(v, "T$1").into_owned()
            } else { v.clone() };
            labels.push((name.clone(), v));
        }
        let label_type = match ty { Some("number") => "number", Some("checkbox") => "boolean", Some("date") => "date", Some("datetime") => "datetime", _ => "text" };
        labels.push((format!("label:{name}"), promoted_definition(key, Some(label_type), ty == Some("multitext") || values.len() > 1)));
    }
    labels
}

struct AttIndex { by_path: HashMap<String, Vec<u8>>, by_base: HashMap<String, Vec<String>> }

impl AttIndex {
    fn new(by_path: HashMap<String, Vec<u8>>) -> Self {
        let mut by_base: HashMap<String, Vec<String>> = HashMap::new();
        for p in by_path.keys() { by_base.entry(basename(p).to_lowercase()).or_default().push(p.clone()); }
        AttIndex { by_path, by_base }
    }
    fn resolve(&self, name: &str) -> Option<(String, &Vec<u8>)> {
        if let Some(b) = self.by_path.get(name) { return Some((name.to_string(), b)); }
        match self.by_base.get(&basename(name).to_lowercase()) {
            Some(v) if v.len() == 1 => self.by_path.get(&v[0]).map(|b| (v[0].clone(), b)),
            _ => None,
        }
    }
}

/// The vault file a `src`/`href` names (percent-decoded, `#heading` and `|size` peeled off), or None.
fn attachment_ref(value: &str) -> Option<(String, Option<String>)> {
    let value = decode_entities(value);
    static EXT: OnceLock<Regex> = OnceLock::new();
    if EXT.get_or_init(|| Regex::new(r"(?i)^(https?:|data:|mailto:|tel:|#)").unwrap()).is_match(&value) { return None; }
    let mut r = decode_percent(value.strip_prefix('/').unwrap_or(&value));
    let mut width = None;
    if let Some(p) = r.find('|') {
        static SZ: OnceLock<Regex> = OnceLock::new();
        if let Some(c) = SZ.get_or_init(|| Regex::new(r"^(\d+)(?:x\d+)?$").unwrap()).captures(r[p + 1..].trim()) { width = Some(c[1].to_string()); }
        r.truncate(p);
    }
    if let Some(h) = r.find('#') { r.truncate(h); }
    let r = r.replace('\\', "/").trim().to_string();
    (!r.is_empty()).then_some((r, width))
}

fn split_wikilink(inner: &str) -> (String, Option<String>) {
    let mut target = inner.to_string();
    let mut alias = None;
    if let Some(p) = target.find('|') { alias = Some(target[p + 1..].trim().to_string()); target.truncate(p); }
    if let Some(h) = target.find('#') { target.truncate(h); }
    (target.trim().to_string(), alias)
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

impl Db {
    /// Import an Obsidian vault from archive/folder entries under `parent_id`.
    pub fn import_obsidian(&self, entries: Vec<Entry>, parent_id: &str, archive_name: Option<&str>) -> Result<ImportStats> {
        let exists: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row("SELECT 1 FROM notes WHERE noteId=? AND isDeleted=0", [parent_id], |r| r.get(0)))?;
        anyhow::ensure!(exists.is_some(), "import target {parent_id} not found");
        let all: Vec<(String, Vec<u8>)> = entries.into_iter().filter(|e| !e.is_dir).map(|e| (e.path.replace('\\', "/"), e.data)).collect();
        let paths: Vec<String> = all.iter().map(|(p, _)| p.clone()).collect();
        let vault_root = detect_vault_root(&paths);
        let strip = |p: &str| p.strip_prefix(vault_root.as_str()).unwrap_or(p).to_string();

        let mut types: HashMap<String, String> = HashMap::new();
        let mut notes: Vec<VaultNote> = vec![];
        let mut attachments: HashMap<String, Vec<u8>> = HashMap::new();
        let mut specials: Vec<(String, Vec<u8>)> = vec![]; // .canvas whiteboards and .base databases
        for (p, data) in all {
            let lower = p.to_lowercase();
            if lower == ".obsidian/types.json" || lower.ends_with("/.obsidian/types.json") {
                if let Ok(serde_json::Value::Object(o)) = serde_json::from_slice::<serde_json::Value>(&data).map(|v| v.get("types").cloned().unwrap_or_default()) {
                    for (k, v) in o { if let Some(s) = v.as_str() { types.insert(k, s.to_string()); } }
                }
            }
            if is_ignored(&p) { continue; }
            let rel = strip(&p);
            if is_ignored(&rel) { continue; }
            if is_markdown(&rel) {
                let base = basename(&rel);
                let title = if base.to_lowercase().ends_with(".excalidraw.md") { base[..base.len() - ".excalidraw.md".len()].to_string() } else { base[..base.len() - 3].to_string() };
                notes.push(VaultNote { path: rel, title, markdown: String::from_utf8_lossy(&data).into_owned() });
            } else if is_special(&rel) {
                specials.push((rel, data));
            } else {
                attachments.insert(rel, data);
            }
        }
        notes.sort_by(|a, b| locale_cmp(&a.path, &b.path));

        let tx = self.begin()?;
        let mut stats = ImportStats::default();
        let root_title = if !vault_root.is_empty() { basename(vault_root.trim_end_matches('/')).to_string() }
            else { archive_name.map(|n| basename(&n.replace('\\', "/")).trim_end_matches(".zip").trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Obsidian import".into()) };
        let root = self.create_note(parent_id, &root_title, "text", "text/html", "")?;
        self.add_attribute(&root, "label", "iconClass", "bx bx-import", false)?;
        stats.notes += 1;
        stats.root = Some(root.clone());

        let index = AttIndex::new(attachments);
        let mut folders: HashMap<String, String> = HashMap::new();
        let mut consumed: HashSet<String> = HashSet::new();
        let mut created: Vec<(String, String, String)> = vec![]; // note id, vault path, html

        for vn in &notes {
            let parent = self.ensure_folder(parent_folder(&vn.path), &root, &mut folders, &mut stats)?;
            if vn.path.to_lowercase().ends_with(".excalidraw.md") {
                if let Some((content, embedded)) = parse_excalidraw(&vn.markdown) {
                    let id = self.create_note(&parent, &vn.title, "canvas", "application/json", &content)?;
                    stats.notes += 1;
                    for (file_id, reference) in embedded {
                        if let Some((path, bytes)) = index.resolve(&reference) {
                            let mime = mime_guess::from_path(basename(&path)).first_or_octet_stream().to_string();
                            if mime.starts_with("image/") {
                                consumed.insert(path);
                                self.add_attachment(&id, &file_id, &mime, "image", bytes)?;
                                stats.attachments += 1;
                            }
                        }
                    }
                    created.push((id, vn.path.replace(".excalidraw.md", ""), String::new()));
                    continue;
                }
            }
            let fm = parse_frontmatter(&vn.markdown);
            let html = md_to_html(&fm.body, &vn.title, true);
            let id = self.create_note(&parent, &vn.title, "text", "text/html", "")?;
            stats.notes += 1;
            for (n, v) in map_frontmatter(&fm.data, &types) {
                if n.is_empty() { continue; }
                self.add_attribute(&id, "label", &n, &v, false)?;
                stats.attributes += 1;
            }
            let html = self.apply_attachments(&id, &html, &index, &mut consumed, &mut stats)?;
            created.push((id, vn.path.clone(), html));
        }

        // whiteboards become Excalidraw canvas notes, Bases keep their YAML as a source note
        specials.sort_by(|a, b| locale_cmp(&a.0, &b.0));
        for (path, data) in &specials {
            let parent = self.ensure_folder(parent_folder(path), &root, &mut folders, &mut stats)?;
            let name = basename(path);
            let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
            let text = String::from_utf8_lossy(data).into_owned();
            let id = match (path.to_lowercase().ends_with(".canvas"), canvas_to_excalidraw(&text)) {
                (true, Some(scene)) => self.create_note(&parent, stem, "canvas", "application/json", &scene)?,
                _ => self.create_note(&parent, stem, "code", if path.to_lowercase().ends_with(".base") { "text/x-yaml" } else { "application/json" }, &text)?,
            };
            stats.notes += 1;
            created.push((id, path.clone(), String::new()));
        }

        // second pass: wikilinks and note embeds, now that every note has an id
        let mut by_path: HashMap<String, String> = HashMap::new();
        let mut by_name: HashMap<String, Option<String>> = HashMap::new();
        for (id, path, _) in &created {
            let key = strip_md(&path.to_lowercase()).to_string();
            by_path.entry(key.clone()).or_insert_with(|| id.clone());
            by_name.entry(basename(&key).to_string()).and_modify(|v| *v = None).or_insert(Some(id.clone()));
        }
        let resolve = |target: &str| -> Option<String> {
            if target.is_empty() { return None; }
            let key = strip_md(&target.to_lowercase()).to_string();
            by_path.get(&key).cloned().or_else(|| by_name.get(basename(&key)).cloned().flatten())
        };
        for (id, path, html) in &created {
            if html.is_empty() { continue; }
            // plain Markdown links to other notes (`[x](../Other%20note.md)`), relative to this note's folder
            let folder = parent_folder(path).to_string();
            let rel = |href: &str| -> Option<String> {
                let h = decode_percent(href.split(['#', '?']).next().unwrap_or(""));
                if h.is_empty() || h.contains("://") || h.starts_with("mailto:") || h.starts_with('/') { return None; }
                let mut parts: Vec<&str> = if folder.is_empty() { vec![] } else { folder.split('/').collect() };
                for seg in h.split('/') { match seg { "" | "." => {} ".." => { parts.pop()?; } s => parts.push(s) } }
                by_path.get(&strip_md(&parts.join("/").to_lowercase()).to_string()).cloned()
            };
            let resolved = resolve_links(html, &resolve, &rel);
            self.store_imported_html(id, &resolved, None)?;
        }

        // orphans: files nothing embedded or linked
        let mut orphans: Vec<&String> = index.by_path.keys().filter(|p| !consumed.contains(*p)).collect();
        orphans.sort_by(|a, b| locale_cmp(a, b));
        for p in orphans {
            let parent = self.ensure_folder(parent_folder(p), &root, &mut folders, &mut stats)?;
            let name = basename(p);
            let mime = mime_guess::from_path(name).first_or_octet_stream().to_string();
            let id = self.create_file_note(&parent, name, &mime, &index.by_path[p])?;
            self.add_attribute(&id, "label", "originalFileName", name, false)?;
            stats.notes += 1;
        }
        tx.commit()?;
        Ok(stats)
    }

    fn ensure_folder(&self, folder: &str, root: &str, folders: &mut HashMap<String, String>, stats: &mut ImportStats) -> Result<String> {
        if folder.is_empty() { return Ok(root.to_string()); }
        if let Some(id) = folders.get(folder) { return Ok(id.clone()); }
        let parent = self.ensure_folder(parent_folder(folder), root, folders, stats)?;
        let id = self.create_note(&parent, basename(folder), "text", "text/html", "")?;
        stats.notes += 1;
        folders.insert(folder.to_string(), id.clone());
        Ok(id)
    }

    /// Save embedded/linked vault files as attachments of `note_id` and point the HTML at them.
    fn apply_attachments(&self, note_id: &str, html: &str, index: &AttIndex, consumed: &mut HashSet<String>, stats: &mut ImportStats) -> Result<String> {
        if index.by_path.is_empty() || (!html.contains("<img") && !html.contains("<a")) { return Ok(html.to_string()); }
        let mut err: Option<anyhow::Error> = None;
        let mut saved = 0usize;
        fn save_file(db: &Db, note_id: &str, path: &str, bytes: &[u8], saved: &mut usize, err: &mut Option<anyhow::Error>) -> Option<(String, String)> {
            let name = basename(path).to_string();
            let mime = mime_guess::from_path(&name).first_or_octet_stream().to_string();
            match db.add_attachment(note_id, &name, &mime, "file", bytes) {
                Ok(a) => { *saved += 1; Some((a, name)) }
                Err(e) => { err.get_or_insert(e); None }
            }
        }
        let out = map_els(parse(html), &mut |mut e: El| -> Vec<Node> {
            match e.name.as_str() {
                "img" => {
                    let Some((name, width)) = e.attr("src").and_then(attachment_ref) else { return vec![Node::El(e)] };
                    let Some((path, bytes)) = index.resolve(&name) else { return vec![Node::El(e)] };
                    consumed.insert(path.clone());
                    let mime = mime_guess::from_path(basename(&path)).first_or_octet_stream().to_string();
                    if mime.starts_with("image/") {
                        match self.add_attachment(note_id, basename(&path), &mime, "image", bytes) {
                            Ok(aid) => {
                                saved += 1;
                                e.set_attr("src", &format!("api/attachments/{aid}/image/{}", encode_component(basename(&path))));
                                if let Some(w) = width { e.set_attr("width", &w); }
                                return vec![Node::El(e)];
                            }
                            Err(x) => { err.get_or_insert(x); return vec![Node::El(e)]; }
                        }
                    }
                    match save_file(self, note_id, &path, bytes, &mut saved, &mut err) {
                        Some((aid, title)) => parse(&format!("<a class=\"reference-link\" href=\"#root/{note_id}?viewMode=attachments&amp;attachmentId={aid}\">{}</a>", escape_text(&title))),
                        None => vec![Node::El(e)],
                    }
                }
                "a" => {
                    let Some((name, _)) = e.attr("href").and_then(attachment_ref) else { return vec![Node::El(e)] };
                    let Some((path, bytes)) = index.resolve(&name) else { return vec![Node::El(e)] };
                    consumed.insert(path.clone());
                    if let Some((aid, _)) = save_file(self, note_id, &path, bytes, &mut saved, &mut err) {
                        e.set_attr("href", &format!("#root/{note_id}?viewMode=attachments&attachmentId={aid}"));
                        e.set_attr("class", "reference-link");
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

/// Wikilinks → internal links (alias keeps its text), unresolved ones → plain text, `![[Note]]` → include-note.
fn resolve_links(html: &str, resolve: &dyn Fn(&str) -> Option<String>, rel: &dyn Fn(&str) -> Option<String>) -> String {
    if !html.contains("href=\"") && !html.contains("src=\"/") { return html.to_string(); }
    let out = map_els(parse(html), &mut |mut e: El| -> Vec<Node> {
        if e.name == "a" && !e.has_class("reference-link") {
            if let Some(id) = e.attr_dec("href").and_then(|h| rel(&h)) { e.set_attr("href", &format!("#root/{id}")); }
            return vec![Node::El(e)];
        }
        if e.name == "a" && e.has_class("reference-link") {
            let Some(href) = e.attr_dec("href").filter(|h| h.starts_with('/')) else { return vec![Node::El(e)] };
            let (target, alias) = split_wikilink(&decode_percent(&href[1..]));
            return match resolve(&target) {
                Some(id) => {
                    e.set_attr("href", &format!("#root/{id}"));
                    if let Some(a) = alias { e.remove_attr("class"); e.kids = vec![Node::Text(escape_text(&a))]; }
                    vec![Node::El(e)]
                }
                None => vec![Node::Text(escape_text(&alias.unwrap_or(target)))],
            };
        }
        if e.name == "img" {
            let Some(src) = e.attr_dec("src").filter(|s| s.starts_with('/')) else { return vec![Node::El(e)] };
            let (target, _) = split_wikilink(&decode_percent(&src[1..]));
            return match resolve(&target) {
                Some(id) => parse(&format!("<section class=\"include-note\" data-note-id=\"{}\" data-box-size=\"medium\">&nbsp;</section>", escape_attr(&id))),
                None => {
                    let l = target.to_lowercase();
                    if l.ends_with(".base") || l.ends_with(".canvas") { vec![] } else { vec![Node::El(e)] }
                }
            };
        }
        vec![Node::El(e)]
    });
    serialize(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("o.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn e(path: &str, data: &[u8]) -> Entry { Entry { path: path.into(), data: data.to_vec(), is_dir: false } }

    fn title_of(db: &Db, id: &str) -> String { db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [id], |r| r.get(0)).unwrap() }
    fn kids(db: &Db, p: &str) -> Vec<(String, String)> {
        let mut st = db.conn.prepare("SELECT n.noteId,n.title FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0 ORDER BY b.notePosition").unwrap();
        st.query_map([p], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
    fn child(db: &Db, p: &str, t: &str) -> String { kids(db, p).into_iter().find(|k| k.1 == t).unwrap_or_else(|| panic!("no {t}: {:?}", kids(db, p))).0 }

    fn vault() -> Vec<Entry> {
        vec![
            e("My Vault/.obsidian/app.json", b"{}"),
            e("My Vault/.obsidian/types.json", br#"{"types":{"rating":"number","due":"date","done":"checkbox"}}"#),
            e("My Vault/Home.md", b"---\ntags: [project, 'big idea']\naliases: [Start]\nrating: 4\ndue: 2026-01-02\ndone: false\nauthor: Me\ncssclasses: [wide]\n---\n# Home\n\nSee [[Projects/Plan|the plan]] and [[Missing]] and [[plan]].\n\n![[Projects/Plan]]\n\n![diagram](assets/diagram.png) ![[assets/diagram.png|200]] [spec](docs/spec.pdf)\n\n> [!info] Heads up\n> careful\n\n==marked== %%secret%%\n"),
            e("My Vault/Projects/Plan.md", b"# Plan\n\nBack to [[Home]].\n- [ ] step\n"),
            e("My Vault/Other/Plan.md", b"duplicate name\n"),
            e("My Vault/assets/diagram.png", b"\x89PNG-diagram"),
            e("My Vault/docs/spec.pdf", b"%PDF-spec"),
            e("My Vault/assets/unused.jpg", b"jpg-bytes"),
            e("My Vault/.trash/old.md", b"trashed"),
            e("My Vault/Board.canvas", b"{}"),
            e("My Vault/Drawing.excalidraw.md", b"---\nexcalidraw-plugin: parsed\n---\ncompressed-json"),
        ]
    }

    #[test]
    fn vault_structure_links_attachments_and_front_matter() {
        let (_d, db) = fixture();
        assert!(looks_like_vault(&vault()));
        let st = db.import_obsidian(vault(), "root", Some("ignored.zip")).unwrap();
        let r = st.root.unwrap();
        assert_eq!(title_of(&db, &r), "My Vault", "named after the wrapper folder");
        let names: Vec<String> = kids(&db, &r).into_iter().map(|k| k.1).collect();
        for n in ["Home", "Projects", "Other", "Drawing"] { assert!(names.contains(&n.to_string()), "{names:?}"); }
        assert!(!names.iter().any(|n| n.starts_with('.')), "dot folders are skipped: {names:?}");
        assert!(names.contains(&"Board".to_string()), "the whiteboard is imported: {names:?}");
        let home = child(&db, &r, "Home");
        let plan = child(&db, &child(&db, &r, "Projects"), "Plan");
        let html = db.note_content(&home).unwrap().unwrap();
        for want in [format!("<a href=\"#root/{plan}\">the plan</a>"), format!("data-note-id=\"{plan}\""), "Missing".into(), r#"<aside class="admonition note"><p><strong>Heads up</strong></p><p>careful</p></aside>"#.into(),
            r#"<span style="background-color:#e5e64d;--tn-background:#e5e64d;">marked</span>"#.into(), "spec".into()] {
            assert!(html.contains(&want), "missing {want}\n{html}");
        }
        assert!(!html.contains("secret") && !html.contains("[[") && !html.contains("<h1"), "{html}");
        // [[plan]] is ambiguous (two notes called Plan) → left as text; [[Missing]] → text
        assert!(!html.contains("href=\"/") , "{html}");
        // images and files became attachments of Home
        let atts = db.attachments(&home).unwrap();
        let mut titles: Vec<(String, String)> = atts.iter().map(|a| (a.title.clone(), a.role.clone())).collect();
        titles.sort();
        assert_eq!(titles, [("diagram.png".to_string(), "image".to_string()), ("diagram.png".to_string(), "image".to_string()), ("spec.pdf".to_string(), "file".to_string())], "{titles:?}");
        assert!(html.contains("width=\"200\"") && html.contains("api/attachments/"), "{html}");
        // the unreferenced image is a standalone image note; embedded ones are not duplicated as notes
        let assets = child(&db, &r, "assets");
        assert_eq!(kids(&db, &assets).iter().map(|k| k.1.clone()).collect::<Vec<_>>(), ["unused.jpg"]);
        // front matter
        assert!(db.has_label(&home, "project").unwrap() && db.has_label(&home, "bigIdea").unwrap());
        assert_eq!(db.label_value(&home, "alias").unwrap().as_deref(), Some("Start"));
        assert_eq!(db.label_value(&home, "rating").unwrap().as_deref(), Some("4"));
        assert_eq!(db.label_value(&home, "label:rating").unwrap().as_deref(), Some("promoted,single,number,alias=rating"));
        assert_eq!(db.label_value(&home, "label:done").unwrap().as_deref(), Some("promoted,single,boolean,alias=done"));
        assert_eq!(db.label_value(&home, "label:author").unwrap().as_deref(), Some("promoted,single,text,alias=author"));
        assert!(db.label_value(&home, "cssclasses").unwrap().is_none());
        // relations came from the links
        let rels: Vec<String> = db.backlinks(&plan).unwrap().into_iter().map(|b| format!("{}:{}", b.relation, b.source == home)).collect();
        assert!(rels.contains(&"internalLink:true".to_string()) && rels.contains(&"includeNoteLink:true".to_string()), "{rels:?}");
        // an Excalidraw file without a readable scene falls back to an ordinary text note
        let drawing = child(&db, &r, "Drawing");
        assert!(db.note_content(&drawing).unwrap().unwrap().contains("compressed-json"));
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn canvases_become_excalidraw_scenes_and_bases_keep_their_yaml() {
        let canvas = r##"{"nodes":[{"id":"a","type":"text","x":0,"y":0,"width":200,"height":80,"text":"Idea","color":"4"},{"id":"b","type":"file","x":300,"y":0,"width":200,"height":80,"file":"Notes/Plan.md"},
            {"id":"c","type":"link","x":0,"y":200,"width":200,"height":80,"url":"https://example.org"},{"id":"g","type":"group","x":-20,"y":-40,"width":600,"height":340,"label":"Group"},{"x":1}],
            "edges":[{"id":"e1","fromNode":"a","toNode":"b","fromSide":"right","toSide":"left","label":"leads to"},{"id":"bad","fromNode":"a","toNode":"missing"}]}"##;
        let (_d, db) = fixture();
        let st = db.import_obsidian(vec![e("V/.obsidian/x", b""), e("V/Maps/Board.canvas", canvas.as_bytes()), e("V/Maps/Broken.canvas", b"nope"), e("V/Table.base", b"views:\n  - type: table\n"), e("V/Note.md", b"![[Board.canvas]] and ![[Table.base]]")], "root", None).unwrap();
        let root = st.root.unwrap();
        let maps = child(&db, &root, "Maps");
        let board = child(&db, &maps, "Board");
        let (t, m): (String, String) = db.conn.query_row("SELECT type,mime FROM notes WHERE noteId=?", [&board], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t.as_str(), m.as_str()), ("canvas", "application/json"));
        let scene: serde_json::Value = serde_json::from_str(&db.note_content(&board).unwrap().unwrap()).unwrap();
        let ids: Vec<&str> = scene["elements"].as_array().unwrap().iter().filter_map(|x| x["id"].as_str()).collect();
        for want in ["a-box", "a-text", "b-box", "c-box", "g-box", "g-label", "e1", "e1-label"] { assert!(ids.contains(&want), "{want} in {ids:?}"); }
        assert!(!ids.contains(&"bad"), "an edge to a missing node is dropped");
        let arrow = scene["elements"].as_array().unwrap().iter().find(|x| x["id"] == "e1").unwrap();
        assert_eq!(arrow["points"], serde_json::json!([[0, 0], [100.0, 0.0]]));
        assert_eq!(arrow["startBinding"]["elementId"], "a-box");
        assert_eq!(scene["elements"][0]["strokeColor"], "#2f9e44");
        // an unreadable canvas is kept as its source
        let broken = child(&db, &maps, "Broken");
        assert_eq!(db.note_content(&broken).unwrap().unwrap(), "nope");
        let table = child(&db, &root, "Table");
        assert_eq!(db.note_content(&table).unwrap().unwrap(), "views:\n  - type: table\n");
        // embeds of the whiteboard and the base resolve to include-notes
        let note = db.note_content(&child(&db, &root, "Note")).unwrap().unwrap();
        assert!(note.contains(&format!("data-note-id=\"{board}\"")) && note.contains(&format!("data-note-id=\"{table}\"")), "{note}");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn relative_markdown_links_between_notes_are_resolved() {
        let (_d, db) = fixture();
        let st = db.import_obsidian(vec![e("Space/Home.md", b"[a](Sub/Page%20two.md#frag) [b](../x.md) [c](https://e.org/a.md) [d](Sub/missing.md)"), e("Space/Sub/Page two.md", b"[back](../Home.md)"), e("Space/Sub/p.png", b"\x89PNGx")], "root", None).unwrap();
        let root = st.root.unwrap();
        let home = child(&db, &root, "Home");
        let two = child(&db, &child(&db, &root, "Sub"), "Page two");
        let h = db.note_content(&home).unwrap().unwrap();
        assert!(h.contains(&format!("<a href=\"#root/{two}\">a</a>")) && h.contains("href=\"../x.md\"") && h.contains("https://e.org/a.md") && h.contains("Sub/missing.md"), "{h}");
        assert!(db.note_content(&two).unwrap().unwrap().contains(&format!("#root/{home}")));
    }

    #[test]
    fn lz_bombs_and_climbing_links_are_contained() {
        // a stream of the repeated 'a' code decompresses to ever longer strings; the output must stay capped
        let packed = lz_compress_base64_for_tests(&"a".repeat(200_000));
        let out = lz_decompress_base64(&packed);
        assert!(out.as_ref().is_none_or(|s| s.len() <= 16 << 20));
        let (_d, db) = fixture();
        let st = db.import_obsidian(vec![e("V/.obsidian/x", b""), e("V/A.md", b"[up](../../x.md) [ok](B.md)"), e("V/B.md", b"b"), e("V/x.md", b"x")], "root", None).unwrap();
        let root = st.root.unwrap();
        let a = db.note_content(&child(&db, &root, "A")).unwrap().unwrap();
        assert!(a.contains("href=\"../../x.md\"") && a.contains(&format!("#root/{}", child(&db, &root, "B"))), "{a}");
    }

    #[test]
    fn excalidraw_drawings_become_canvas_notes() {
        // LZString.compressToBase64(JSON.stringify({type:"excalidraw",elements:[{id:"a",type:"image",fileId:"abcdef12"}],appState:{theme:"dark"}}))
        let scene = r#"{"type":"excalidraw","elements":[{"id":"a","type":"image","fileId":"abcdef12"}],"appState":{"theme":"dark"}}"#;
        let packed = lz_compress_base64_for_tests(scene);
        assert_eq!(lz_decompress_base64(&packed).as_deref(), Some(scene), "round trip");
        let md = format!("---\nexcalidraw-plugin: parsed\ntags: [excalidraw]\n---\n# Excalidraw Data\n## Embedded Files\nabcdef12: [[pic.png]]\n\n%%\n## Drawing\n```compressed-json\n{packed}\n```\n%%");
        let (_d, db) = fixture();
        let st = db.import_obsidian(vec![e("V/.obsidian/x", b""), e("V/Art/Drawing.excalidraw.md", md.as_bytes()), e("V/pic.png", b"\x89PNGx"), e("V/Note.md", b"see [[Drawing]]")], "root", None).unwrap();
        let art = child(&db, &st.root.unwrap(), "Art");
        let d = child(&db, &art, "Drawing");
        let (t, m): (String, String) = db.conn.query_row("SELECT type,mime FROM notes WHERE noteId=?", [&d], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((t.as_str(), m.as_str()), ("canvas", "application/json"));
        let content: serde_json::Value = serde_json::from_str(&db.note_content(&d).unwrap().unwrap()).unwrap();
        assert_eq!(content["type"], "excalidraw");
        assert_eq!(content["elements"][0]["fileId"], "abcdef12");
        assert_eq!(content["appState"]["theme"], "dark");
        let atts = db.attachments(&d).unwrap();
        assert_eq!((atts[0].title.as_str(), atts[0].role.as_str()), ("abcdef12", "image"));
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM notes WHERE title='pic.png' AND isDeleted=0", [], |r| r.get::<_, i64>(0)).unwrap(), 0, "embedded image isn't also an orphan note");
        for junk in ["!!!", "", "AAAA", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "gA=="] { let _ = lz_decompress_base64(junk); }
    }

    #[test]
    fn lz_string_output_from_the_real_library_decompresses() {
        // produced by lz-string's compressToBase64 for a JSON with non-ASCII text and a long run
        let expected = format!(r#"{{"type":"excalidraw","elements":[{{"id":"é日本語","text":"{}日"}}],"appState":{{"a":[1,2,3]}}}}"#, "x".repeat(500));
        assert_eq!(lz_decompress_base64("N4IgLgngDgpiBcIYA8DGBDANgSwCYCd0B3EAGiUxgFsYA7MAZwQG1Q8EQBLwU9NAac0DyosuBRgOycRMlTpM2XPkLFS5StVr1GzVu07de/QcNHjJydxABfALrl0UKAGUw6MHHih0LAIykATKQBmKwsLIA===").as_deref(), Some(expected.as_str()));
    }

    /// Test-only LZString.compressToBase64 (the reference algorithm), to round-trip the decompressor.
    fn lz_compress_base64_for_tests(input: &str) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
        let units: Vec<u16> = input.encode_utf16().collect();
        let mut dict: std::collections::HashMap<Vec<u16>, u32> = Default::default();
        let mut to_create: std::collections::HashSet<Vec<u16>> = Default::default();
        let (mut w, mut enlarge, mut dict_size, mut num_bits) = (Vec::<u16>::new(), 2u32, 3u32, 2u32);
        let mut bits: Vec<bool> = vec![];
        fn put(bits: &mut Vec<bool>, value: u32, n: u32) { for i in 0..n { bits.push(value >> i & 1 == 1); } }
        for &c in &units {
            let cs = vec![c];
            if !dict.contains_key(&cs) { dict.insert(cs.clone(), dict_size); dict_size += 1; to_create.insert(cs.clone()); }
            let mut wc = w.clone();
            wc.push(c);
            if dict.contains_key(&wc) { w = wc; continue; }
            if to_create.remove(&w) {
                let v = w[0] as u32;
                if v < 256 { put(&mut bits, 0, num_bits); put(&mut bits, v, 8); } else { put(&mut bits, 1, num_bits); put(&mut bits, v, 16); }
                enlarge -= 1; if enlarge == 0 { enlarge = 1 << num_bits; num_bits += 1; }
            } else { put(&mut bits, dict[&w], num_bits); }
            enlarge -= 1; if enlarge == 0 { enlarge = 1 << num_bits; num_bits += 1; }
            dict.insert(wc, dict_size); dict_size += 1;
            w = vec![c];
        }
        if !w.is_empty() {
            if to_create.remove(&w) {
                let v = w[0] as u32;
                if v < 256 { put(&mut bits, 0, num_bits); put(&mut bits, v, 8); } else { put(&mut bits, 1, num_bits); put(&mut bits, v, 16); }
                enlarge -= 1; if enlarge == 0 { enlarge = 1 << num_bits; num_bits += 1; }
            } else { put(&mut bits, dict[&w], num_bits); }
            enlarge -= 1; if enlarge == 0 { num_bits += 1; }
        }
        put(&mut bits, 2, num_bits);
        let mut out = String::new();
        for chunk in bits.chunks(6) {
            let mut v = 0usize;
            for (i, b) in chunk.iter().enumerate() { if *b { v |= 1 << (5 - i); } }
            out.push(A[v] as char);
        }
        while out.len() % 4 != 0 { out.push('='); }
        out
    }

    #[test]
    fn zip_without_obsidian_folder_and_hostile_input() {
        let (_d, db) = fixture();
        let st = db.import_obsidian(vec![e("a.md", b"hello [[b]]"), e("sub/b.md", b"---\nbad: [unclosed\n---\nbody"), e("../evil.md", b"x"), e("c.png", b"png")], "root", Some("dir/My Notes.zip")).unwrap();
        assert_eq!(title_of(&db, &st.root.clone().unwrap()), "My Notes");
        let r = st.root.unwrap();
        let a = child(&db, &r, "a");
        let b = child(&db, &child(&db, &r, "sub"), "b");
        assert!(db.note_content(&a).unwrap().unwrap().contains(&format!("#root/{b}")));
        assert!(db.note_content(&b).unwrap().unwrap().contains("bad: [unclosed"), "malformed front matter is left in the note");
        assert!(db.import_obsidian(vec![e("x.md", b"x")], "nope", None).is_err());
        let _ = db.import_obsidian(vec![e("é.md", "---\nk: v\n---\n$é$ ==ü== [[é]] ![[é]] %%".as_bytes()), e("n.md", b"[[")], "root", None).unwrap();
    }
}
