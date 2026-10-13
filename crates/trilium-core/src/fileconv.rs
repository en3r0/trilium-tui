//! Conversions the generic importer applies to loose files (Trilium's `single.ts` / `zip.ts`): Markdown (with YAML front
//! matter as labels) and plain text become text notes, CSV/XLSX become spreadsheet notes, Mermaid and Excalidraw files get
//! their own note types. Everything else is left to the caller (code, images, files).
use crate::{formats::to_attribute_name, htmltree::escape_text, mdhtml::md_to_html, obsidian::parse_frontmatter};

pub struct Converted {
    pub ntype: &'static str,
    pub mime: String,
    pub content: String,
    pub labels: Vec<(String, String)>,
}

/// Front matter values → labels: a list gives one label per item, a nested mapping is skipped.
fn frontmatter_labels(data: &serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> {
    fn flat(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Null => out.push(String::new()),
            serde_json::Value::Array(a) => for x in a { flat(x, out); },
            serde_json::Value::Bool(b) => out.push(b.to_string()),
            serde_json::Value::Number(n) => out.push(n.to_string()),
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Object(_) => {}
        }
    }
    let mut labels = vec![];
    for (k, v) in data {
        let mut vals = vec![];
        flat(v, &mut vals);
        let name = to_attribute_name(k);
        for val in vals { labels.push((name.clone(), val)); }
    }
    labels
}

/// Text of a file: UTF-16 with a byte-order mark is decoded, UTF-8 loses its BOM; anything else is not text.
pub fn decode_text(data: &[u8]) -> Option<String> {
    if let Some(rest) = data.strip_prefix(&[0xff, 0xfe]).or_else(|| data.strip_prefix(&[0xfe, 0xff])) {
        let le = data[0] == 0xff;
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return String::from_utf16(&units).ok();
    }
    let data = data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data);
    if let Ok(s) = std::str::from_utf8(data) { return Some(s.to_string()); }
    // not UTF-8: legacy 8-bit and CJK text is guessed (Latin-1/Windows-125x, Shift-JIS, GBK, KOI8, ...); binary data is not text
    if data.contains(&0) { return None; }
    let control = data.iter().filter(|b| **b < 0x20 && !matches!(**b, b'\n' | b'\r' | b'\t' | 0x0c)).count();
    if control * 50 > data.len().max(1) { return None; }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(&data[..data.len().min(1 << 20)], data.len() <= 1 << 20);
    let enc = det.guess(None, chardetng::Utf8Detection::Deny);
    let (text, _, had_errors) = enc.decode(data);
    (!had_errors).then(|| text.into_owned())
}

/// `convertTextToHtml`: escape, line breaks, blank lines split paragraphs.
pub fn text_to_html(text: &str) -> String {
    let mut t = escape_text(text);
    t = t.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "<br>");
    let re = regex::Regex::new(r"<br>\s*<br>").expect("static regex");
    t = re.replace_all(&t, "</p><p>").into_owned();
    format!("<p>{t}</p>")
}

/// Convert a loose file, or `None` when it should be imported as it is. `single` marks a lone file import (plain text
/// becomes a text note there; inside an archive it stays a source note).
pub fn convert(file_name: &str, title: &str, data: &[u8], single: bool) -> Option<Converted> {
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    let text = || decode_text(data).unwrap_or_else(|| String::from_utf8_lossy(data).into_owned());
    Some(match ext.as_str() {
        "md" | "markdown" | "mdx" => {
            let fm = parse_frontmatter(&text());
            Converted { ntype: "text", mime: "text/html".into(), content: md_to_html(&fm.body, title, false), labels: frontmatter_labels(&fm.data) }
        }
        "txt" if single => Converted { ntype: "text", mime: "text/html".into(), content: crate::htmltree::sanitize_html(&text_to_html(&text())), labels: vec![] },
        "csv" => Converted { ntype: "spreadsheet", mime: "text/x-spreadsheet".into(), content: crate::spreadsheet::csv_to_workbook(&text()), labels: if single { vec![("originalFileName".into(), file_name.into())] } else { vec![] } },
        "xlsx" => Converted { ntype: "spreadsheet", mime: "text/x-spreadsheet".into(), content: crate::spreadsheet::xlsx_to_workbook(data).ok()?, labels: if single { vec![("originalFileName".into(), file_name.into())] } else { vec![] } },
        "triliumsheet" => Converted { ntype: "spreadsheet", mime: "text/x-spreadsheet".into(), content: text(), labels: vec![] },
        "mmd" | "mermaid" => Converted { ntype: "mermaid", mime: "text/vnd.mermaid".into(), content: text(), labels: vec![] },
        "excalidraw" => Converted { ntype: "canvas", mime: "application/json".into(), content: text(), labels: vec![] },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_encodings_are_decoded_and_binary_is_not_text() {
        assert_eq!(decode_text(b"caf\xe9 cr\xe8me, na\xefve fa\xe7ade \xfcber").as_deref(), Some("caf\u{e9} cr\u{e8}me, na\u{ef}ve fa\u{e7}ade \u{fc}ber"));
        // Windows-1252 curly quotes and euro sign
        assert_eq!(decode_text(b"\x93quoted\x94 costs 5\x80 and it\x92s fine, with plenty of ordinary english text around").unwrap().chars().next(), Some('\u{201c}'));
        // Shift-JIS Japanese
        assert_eq!(decode_text(b"\x93\xfa\x96\x7b\x8c\xea\x82\xcc\x83\x65\x83\x58\x83\x67\x82\xc5\x82\xb7\x81\x42").as_deref(), Some("\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30b9}\u{30c8}\u{3067}\u{3059}\u{3002}"));
        assert_eq!(decode_text(b"plain \xc3\xa9").as_deref(), Some("plain \u{e9}"));
        assert!(decode_text(&[0x89, b'P', b'N', b'G', 0, 1, 2, 0xff]).is_none(), "NUL bytes mean binary");
        assert!(decode_text(&[0x80, 0x81, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0b]).is_none(), "mostly control characters");
        assert_eq!(convert("old.md", "T", b"# T\n\ncaf\xe9 au lait, avec du texte fran\xe7ais", false).unwrap().content, "<p>caf\u{e9} au lait, avec du texte fran\u{e7}ais</p>");
    }

    #[test]
    fn markdown_text_csv_and_special_types() {
        let c = convert("Note.md", "Note", b"---\ntags: [a, b]\ntitle: T\nempty:\nnested:\n  k: v\n---\n# Note\n\nHello **you**", false).unwrap();
        assert_eq!((c.ntype, c.mime.as_str()), ("text", "text/html"));
        assert_eq!(c.content, "<p>Hello <strong>you</strong></p>");
        assert_eq!(c.labels, [("tags".into(), "a".into()), ("tags".into(), "b".into()), ("title".into(), "T".into()), ("empty".into(), String::new())]);
        assert!(convert("a.txt", "a", b"x", false).is_none());
        assert_eq!(convert("a.txt", "a", b"one\r\ntwo\n\nthree <b>", true).unwrap().content, "<p>one<br>two</p><p>three &lt;b&gt;</p>");
        assert_eq!(convert("t.csv", "t", b"a,b\n1,2", true).unwrap().ntype, "spreadsheet");
        assert_eq!(convert("d.mmd", "d", b"graph TD", false).unwrap().mime, "text/vnd.mermaid");
        assert_eq!(convert("d.excalidraw", "d", b"{}", false).unwrap().ntype, "canvas");
        assert!(convert("x.xlsx", "x", b"junk", false).is_none(), "an unreadable workbook stays a plain file");
        assert!(convert("a.rs", "a", b"fn main(){}", true).is_none());
    }
}
