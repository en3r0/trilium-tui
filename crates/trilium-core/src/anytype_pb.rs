//! Anytype Protobuf export (`objects/*.pb`) → the JSON snapshot shape the JSON importer reads.
//!
//! A small protobuf wire decoder plus the field numbers of Anytype's `SnapshotWithType` / `Block` / `Struct` messages.
//! The numbers come from Anytype's published `.proto` files as remembered, not from a sample export; to keep a wrong guess
//! from producing silent garbage, `to_snapshot` only returns a snapshot when it holds a coherent block tree (the root
//! block's id equals the object's id and its children resolve) and the importer refuses everything else with a message
//! asking for the JSON export.
use serde_json::{Map, Value, json};
#[cfg(test)]
use crate::{db::Db, transfer::Entry};

#[derive(Debug, Clone)]
enum Wire<'a> { Varint(u64), Fixed64(u64), Bytes(&'a [u8]) }

fn read_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let byte = *b.get(*i)?;
        *i += 1;
        if shift >= 64 { return None; }
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 { return Some(v); }
        shift += 7;
    }
}

/// Decode one message level into (field number, value) pairs. Malformed input yields `None`.
fn fields(b: &[u8]) -> Option<Vec<(u32, Wire<'_>)>> {
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let key = read_varint(b, &mut i)?;
        let field = u32::try_from(key >> 3).ok().filter(|f| *f > 0)?;
        let v = match key & 7 {
            0 => Wire::Varint(read_varint(b, &mut i)?),
            1 => { let s = b.get(i..i + 8)?; i += 8; Wire::Fixed64(u64::from_le_bytes(s.try_into().ok()?)) }
            2 => { let n = usize::try_from(read_varint(b, &mut i)?).ok()?; let s = b.get(i..i.checked_add(n)?)?; i += n; Wire::Bytes(s) }
            5 => { b.get(i..i + 4)?; i += 4; continue; }
            _ => return None,
        };
        out.push((field, v));
    }
    Some(out)
}

fn get<'a>(f: &'a [(u32, Wire<'a>)], n: u32) -> Option<&'a Wire<'a>> { f.iter().find(|(k, _)| *k == n).map(|(_, v)| v) }
fn bytes_of<'a>(f: &'a [(u32, Wire<'a>)], n: u32) -> Vec<&'a [u8]> { f.iter().filter_map(|(k, v)| match (k, v) { (k, Wire::Bytes(b)) if *k == n => Some(*b), _ => None }).collect() }
fn string(f: &[(u32, Wire)], n: u32) -> String { match get(f, n) { Some(Wire::Bytes(b)) => String::from_utf8_lossy(b).into_owned(), _ => String::new() } }
fn int(f: &[(u32, Wire)], n: u32) -> i64 { match get(f, n) { Some(Wire::Varint(v)) => *v as i64, _ => 0 } }
fn boolean(f: &[(u32, Wire)], n: u32) -> bool { int(f, n) != 0 }
fn strings(f: &[(u32, Wire)], n: u32) -> Vec<String> { bytes_of(f, n).into_iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect() }

// ---- google.protobuf.Struct / Value

fn value(b: &[u8], depth: usize) -> Value {
    let Some(f) = fields(b) else { return Value::Null };
    if depth > 32 { return Value::Null; }
    match f.first() {
        Some((1, _)) => Value::Null,
        Some((2, Wire::Fixed64(bits))) => {
            let n = f64::from_bits(*bits);
            // whole numbers stay integers (layouts, formats, timestamps), as in the JSON export
            if n.is_finite() && n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null) }
        }
        Some((3, Wire::Bytes(s))) => Value::String(String::from_utf8_lossy(s).into_owned()),
        Some((4, Wire::Varint(v))) => Value::Bool(*v != 0),
        Some((5, Wire::Bytes(s))) => strukt(s, depth + 1),
        Some((6, Wire::Bytes(l))) => Value::Array(fields(l).unwrap_or_default().iter().filter_map(|(k, v)| match (k, v) { (1, Wire::Bytes(x)) => Some(value(x, depth + 1)), _ => None }).collect()),
        _ => Value::Null,
    }
}

fn strukt(b: &[u8], depth: usize) -> Value {
    let mut m = Map::new();
    for e in bytes_of(&fields(b).unwrap_or_default(), 1) {
        let Some(kv) = fields(e) else { continue };
        let key = string(&kv, 1);
        let val = match get(&kv, 2) { Some(Wire::Bytes(v)) => value(v, depth), _ => Value::Null };
        if !key.is_empty() { m.insert(key, val); }
    }
    Value::Object(m)
}

// ---- enums (by number) → the names the JSON export uses

fn pick(n: i64, names: &[&str]) -> Value { names.get(n as usize).map_or(Value::Null, |s| json!(s)) }

fn range(b: &[u8]) -> Value { let f = fields(b).unwrap_or_default(); json!({"from": int(&f, 1), "to": int(&f, 2)}) }

fn text_block(b: &[u8]) -> Value {
    let f = fields(b).unwrap_or_default();
    const STYLES: &[&str] = &["Paragraph", "Header1", "Header2", "Header3", "Header4", "Quote", "Code", "Title", "Checkbox", "Marked", "Numbered", "Toggle", "Description", "Callout", "ToggleHeader1", "ToggleHeader2", "ToggleHeader3"];
    const MARKS: &[&str] = &["Strikethrough", "Keyboard", "Italic", "Bold", "Underscored", "Link", "TextColor", "BackgroundColor", "Mention", "Emoji", "Object"];
    let marks: Vec<Value> = get(&f, 3).and_then(|w| if let Wire::Bytes(m) = w { fields(m) } else { None }).unwrap_or_default().iter().filter_map(|(k, v)| match (k, v) {
        (1, Wire::Bytes(m)) => { let mf = fields(m).unwrap_or_default(); Some(json!({"range": bytes_of(&mf, 1).first().map_or(Value::Null, |r| range(r)), "type": pick(int(&mf, 2), MARKS), "param": string(&mf, 3)})) }
        _ => None,
    }).collect();
    json!({"text": string(&f, 1), "style": pick(int(&f, 2), STYLES), "marks": {"marks": marks}, "checked": boolean(&f, 4), "iconEmoji": string(&f, 6)})
}

fn block(b: &[u8]) -> Option<Value> {
    let f = fields(b)?;
    let id = string(&f, 1);
    if id.is_empty() { return None; }
    let mut o = Map::new();
    o.insert("id".into(), json!(id));
    o.insert("childrenIds".into(), json!(strings(&f, 4)));
    if let Some(Wire::Bytes(s)) = get(&f, 2) { let st = strukt(s, 0); if st.as_object().is_some_and(|m| !m.is_empty()) { o.insert("fields".into(), st); } }
    let sub = |n: u32| -> Option<Vec<(u32, Wire)>> { bytes_of(&f, n).first().and_then(|x| fields(x)) };
    // content oneof (11..=24)
    if let Some(x) = bytes_of(&f, 12).first() { o.insert("text".into(), text_block(x)); }
    if let Some(x) = sub(13) {
        const TYPES: &[&str] = &["None", "File", "Image", "Video", "Audio", "PDF"];
        const STATES: &[&str] = &["Empty", "Uploading", "Done", "Error"];
        o.insert("file".into(), json!({"name": string(&x, 2), "type": pick(int(&x, 3), TYPES), "mime": string(&x, 4), "state": pick(int(&x, 7), STATES), "targetObjectId": string(&x, 9)}));
    }
    if let Some(x) = sub(15) { o.insert("div".into(), json!({"style": pick(int(&x, 1), &["Line", "Dots"])})); }
    if let Some(x) = sub(16) { o.insert("bookmark".into(), json!({"url": string(&x, 1), "title": string(&x, 2), "description": string(&x, 3), "imageHash": string(&x, 4), "faviconHash": string(&x, 5), "targetObjectId": string(&x, 7)})); }
    if let Some(x) = sub(18) { o.insert("link".into(), json!({"targetBlockId": string(&x, 1), "style": pick(int(&x, 2), &["Page", "Dataview", "Dashboard", "Archive"])})); }
    if let Some(x) = sub(22) { o.insert("latex".into(), json!({"text": string(&x, 1), "processor": if int(&x, 2) == 2 { json!("Mermaid") } else { json!("Latex") }})); }
    if let Some(x) = sub(23) { o.insert("tableRow".into(), json!({"isHeader": boolean(&x, 1)})); }
    if bytes_of(&f, 24).first().is_some() { o.insert("table".into(), json!({})); }
    Some(Value::Object(o))
}

/// An object's `SnapshotWithType` message as the JSON export shape; `None` when it doesn't hold a coherent page.
pub fn to_snapshot(b: &[u8], kind: &str) -> Option<Value> {
    let top = fields(b)?;
    let snapshot = fields(bytes_of(&top, 2).first()?)?;
    let data = fields(bytes_of(&snapshot, 1).first()?)?;
    // details: the one Struct-shaped message; blocks: the repeated messages whose first field is a string id
    let details = bytes_of(&data, 2).first().map(|s| strukt(s, 0))?;
    let blocks: Vec<Value> = bytes_of(&data, 1).into_iter().filter_map(block).collect();
    let id = details["id"].as_str()?.to_string();
    let objects_types = strings(&data, 4);
    let sb_type = match (kind, int(&top, 1)) { ("objects", 16) => "Page", ("objects", _) => "Other", ("relations", _) => "STRelation", ("relationsOptions", _) => "STRelationOption", _ => "Other" };
    if kind == "objects" && sb_type == "Page" {
        // coherence check: the root block is the object itself and its children exist
        let root = blocks.iter().find(|b| b["id"] == json!(id))?;
        let ids: std::collections::HashSet<&str> = blocks.iter().filter_map(|b| b["id"].as_str()).collect();
        let kids = root["childrenIds"].as_array()?;
        if !kids.is_empty() && !kids.iter().any(|k| k.as_str().is_some_and(|k| ids.contains(k))) { return None; }
    }
    let mut details = details;
    // a collection's dataview flag isn't decoded: it is the collection layout instead
    let is_collection = details["resolvedLayout"].as_i64() == Some(14) || details["layout"].as_i64() == Some(14);
    let mut blocks = blocks;
    if is_collection { blocks.push(json!({"id": "dataview-collection", "dataview": {"isCollection": true, "views": []}})); }
    if let Some(o) = details.as_object_mut() { o.entry("id").or_insert(json!(id)); }
    Some(json!({"sbType": sb_type, "snapshot": {"data": {"blocks": blocks, "details": details, "objectTypes": objects_types}}}))
}

#[cfg(test)]
pub(crate) mod enc {
    //! Test-side protobuf encoder.
    pub fn varint(mut v: u64, out: &mut Vec<u8>) { loop { let b = (v & 0x7f) as u8; v >>= 7; if v == 0 { out.push(b); break; } out.push(b | 0x80); } }
    pub fn key(f: u32, w: u32, out: &mut Vec<u8>) { varint(u64::from(f) << 3 | u64::from(w), out); }
    pub fn bytes(f: u32, b: &[u8], out: &mut Vec<u8>) { key(f, 2, out); varint(b.len() as u64, out); out.extend_from_slice(b); }
    pub fn int(f: u32, v: u64, out: &mut Vec<u8>) { key(f, 0, out); varint(v, out); }
    pub fn string(f: u32, s: &str, out: &mut Vec<u8>) { bytes(f, s.as_bytes(), out); }
    pub fn value_str(s: &str) -> Vec<u8> { let mut o = vec![]; string(3, s, &mut o); o }
    pub fn value_num(n: f64) -> Vec<u8> { let mut o = vec![]; key(2, 1, &mut o); o.extend_from_slice(&n.to_bits().to_le_bytes()); o }
    pub fn value_list(items: &[Vec<u8>]) -> Vec<u8> { let mut l = vec![]; for i in items { bytes(1, i, &mut l); } let mut o = vec![]; bytes(6, &l, &mut o); o }
    pub fn strukt(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut o = vec![];
        for (k, v) in entries { let mut e = vec![]; string(1, k, &mut e); bytes(2, v, &mut e); bytes(1, &e, &mut o); }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::enc;

    fn text(t: &str, style: u64, marks: &[(u64, u64, u64, &str)]) -> Vec<u8> {
        let mut m = vec![];
        for (from, to, ty, param) in marks {
            let mut r = vec![]; enc::int(1, *from, &mut r); enc::int(2, *to, &mut r);
            let mut mk = vec![]; enc::bytes(1, &r, &mut mk); enc::int(2, *ty, &mut mk); enc::string(3, param, &mut mk);
            enc::bytes(1, &mk, &mut m);
        }
        let mut o = vec![]; enc::string(1, t, &mut o); enc::int(2, style, &mut o); enc::bytes(3, &m, &mut o);
        o
    }
    fn block_msg(id: &str, kids: &[&str], content: Option<(u32, Vec<u8>)>) -> Vec<u8> {
        let mut o = vec![]; enc::string(1, id, &mut o);
        for k in kids { enc::string(4, k, &mut o); }
        if let Some((f, c)) = content { enc::bytes(f, &c, &mut o); }
        o
    }
    fn snapshot(sb: u64, blocks: Vec<Vec<u8>>, details: Vec<u8>) -> Vec<u8> {
        let mut data = vec![]; for b in &blocks { enc::bytes(1, b, &mut data); } enc::bytes(2, &details, &mut data);
        let mut snap = vec![]; enc::bytes(1, &data, &mut snap);
        let mut top = vec![]; enc::int(1, sb, &mut top); enc::bytes(2, &snap, &mut top);
        top
    }

    #[test]
    fn wire_decoder_reads_structs_values_and_rejects_garbage() {
        let d = enc::strukt(&[("id", enc::value_str("x")), ("n", enc::value_num(2.5)), ("list", enc::value_list(&[enc::value_str("a"), enc::value_num(1.0)]))]);
        let v = super::strukt(&d, 0);
        assert_eq!(v, json!({"id": "x", "n": 2.5, "list": ["a", 1]}));
        assert_eq!(super::strukt(&enc::strukt(&[("layout", enc::value_num(14.0))]), 0)["layout"].as_i64(), Some(14));
        for junk in [&[0xffu8; 12][..], &[0x0a, 0xff, 0x01][..], &[0x08][..], &[0x07][..], &[][..]] { let _ = to_snapshot(junk, "objects"); let _ = fields(junk); }
        assert!(to_snapshot(&[0x12, 0x00], "objects").is_none());
    }

    #[test]
    fn a_page_snapshot_becomes_the_json_shape_and_imports() {
        let blocks = vec![
            block_msg("pageid", &["h", "p"], None),
            block_msg("h", &[], Some((12, text("Heading", 1, &[])))),
            block_msg("p", &[], Some((12, text("Hello bold", 0, &[(6, 10, 3, "")])))),
        ];
        let details = enc::strukt(&[("id", enc::value_str("pageid")), ("name", enc::value_str("Proto page")), ("createdDate", enc::value_num(1700000000.0))]);
        let page = snapshot(16, blocks, details);
        let json = to_snapshot(&page, "objects").unwrap();
        assert_eq!(json["sbType"], "Page");
        assert_eq!(json["snapshot"]["data"]["blocks"][2]["text"]["marks"]["marks"][0]["type"], "Bold");
        // a snapshot whose tree doesn't hold together is refused instead of imported as garbage
        let bad = snapshot(16, vec![block_msg("other", &["nope"], None)], enc::strukt(&[("id", enc::value_str("pageid"))]));
        assert!(to_snapshot(&bad, "objects").is_none());

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("p.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        let st = db.import_anytype(vec![Entry { path: "objects/a.pb".into(), data: page, is_dir: false }], "root", None).unwrap();
        let note: String = db.conn.query_row("SELECT n.noteId FROM branches b JOIN notes n ON n.noteId=b.noteId WHERE b.parentNoteId=? AND n.title='Proto page'", [st.root.unwrap()], |r| r.get(0)).unwrap();
        assert_eq!(db.note_content(&note).unwrap().unwrap(), "<h2>Heading</h2><p>Hello <strong>bold</strong></p>");
        assert!(db.import_anytype(vec![Entry { path: "objects/a.pb".into(), data: bad, is_dir: false }], "root", None).unwrap_err().to_string().contains("JSON"));
    }
}
