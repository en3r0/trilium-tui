//! `.xlsx` → the Univer workbook JSON a Trilium spreadsheet note stores, after Trilium's `parse_from_xlsx.ts`: values (dates as
//! serial numbers), formulas with their cached results, fonts, fills, alignment, borders, number formats, merges, row and column
//! sizes, hyperlinks, data validation and floating images. Not carried over (nor by Trilium): conditional formatting, filters,
//! charts, comments, frozen panes, defined names; rich text is flattened.
use crate::htmltree::{Token, decode_entities, tokenize};
use crate::spreadsheet::{BOOLEAN, DEFAULT_COLS, DEFAULT_ROWS, MAX_COLS, MAX_ROWS, MAX_SHEETS, NUMBER, STRING, num, put_cell, workbook};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::Read;

const FORCE_STRING: i64 = 4;
const HEADER_WIDTH: f64 = 46.0;
const HEADER_HEIGHT: f64 = 20.0;
const MAX_PARTS: usize = 4096;

type Attrs = [(String, Option<String>)];

fn bool_val(attrs: &Attrs) -> bool { !matches!(attr(attrs, "val"), Some("0" | "false")) }

/// Tokens with namespace prefixes dropped from element names (`x:row` reads as `row`), as some writers prefix SpreadsheetML.
fn tok(xml: &str) -> Vec<Token> {
    let local = |n: String| -> String { match n.rsplit_once(':') { Some((_, l)) => l.to_string(), None => n } };
    tokenize(xml, false).into_iter().map(|t| match t {
        Token::Open { name, attrs, self_close } => Token::Open { name: local(name), attrs, self_close },
        Token::Close(name) => Token::Close(local(name)),
        other => other,
    }).collect()
}

/// A relationship-id attribute whatever its prefix (`r:id`, `id`, `ns:id`), or `embed` likewise.
fn attr_ns<'a>(attrs: &'a Attrs, local: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == local || k.ends_with(&format!(":{local}"))).and_then(|(_, v)| v.as_deref())
}

fn attr<'a>(attrs: &'a Attrs, n: &str) -> Option<&'a str> { attrs.iter().find(|(k, _)| k == n).and_then(|(_, v)| v.as_deref()) }
fn attr_dec(attrs: &Attrs, n: &str) -> Option<String> { attr(attrs, n).map(decode_entities) }
fn truthy(v: Option<&str>) -> bool { matches!(v, Some("1" | "true")) }

struct Zip<R: Read + std::io::Seek>(zip::ZipArchive<R>);

impl<R: Read + std::io::Seek> Zip<R> {
    fn text(&mut self, name: &str) -> Option<String> {
        let f = self.0.by_name(name).ok()?;
        if f.size() > 64 << 20 { return None; }
        let mut s = String::new();
        f.take(64 << 20).read_to_string(&mut s).ok()?;
        Some(s)
    }

    fn bytes(&mut self, name: &str, limit: u64) -> Option<Vec<u8>> {
        let f = self.0.by_name(name).ok()?;
        if f.size() > limit { return None; }
        let mut v = vec![];
        f.take(limit).read_to_end(&mut v).ok()?;
        Some(v)
    }
}

/// "AB12" → (row 11, col 27), zero-based.
fn cell_ref(r: &str) -> Option<(usize, usize)> {
    let letters: String = r.chars().take_while(char::is_ascii_alphabetic).collect();
    let digits = &r[letters.len()..];
    if letters.is_empty() || letters.len() > 3 || digits.is_empty() { return None; }
    let col = letters.to_ascii_uppercase().bytes().fold(0usize, |a, b| a * 26 + (b - b'A' + 1) as usize);
    let row = digits.parse::<usize>().ok()?;
    // Excel's limits: 1,048,576 rows, 16,384 columns (XFD)
    if row == 0 || row > MAX_ROWS || col == 0 || col > MAX_COLS { return None; }
    Some((row - 1, col - 1))
}

fn range_ref(r: &str) -> Option<(usize, usize, usize, usize)> {
    let (a, b) = r.split_once(':').unwrap_or((r, r));
    let (r1, c1) = cell_ref(a)?;
    let (r2, c2) = cell_ref(b)?;
    Some((r1.min(r2), r1.max(r2), c1.min(c2), c1.max(c2)))
}

// ------------------------------------------------------------------ number formats

/// exceljs' table of built-in formats (ids without an entry — the locale-specific ones — have no format).
fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "0", 2 => "0.00", 3 => "#,##0", 4 => "#,##0.00", 9 => "0%", 10 => "0.00%", 11 => "0.00E+00", 12 => "# ?/?", 13 => "# ??/??",
        14 => "mm-dd-yy", 15 => "d-mmm-yy", 16 => "d-mmm", 17 => "mmm-yy", 18 => "h:mm AM/PM", 19 => "h:mm:ss AM/PM", 20 => "h:mm", 21 => "h:mm:ss",
        22 => "m/d/yy \"h\":mm", 37 => "#,##0 ;(#,##0)", 38 => "#,##0 ;[Red](#,##0)", 39 => "#,##0.00 ;(#,##0.00)", 40 => "#,##0.00 ;[Red](#,##0.00)",
        45 => "mm:ss", 46 => "[h]:mm:ss", 47 => "mmss.0", 48 => "##0.0E+0", 49 => "@",
        _ => return None,
    })
}

/// exceljs' `isDateFmt`: after dropping `[...]` and `"..."`, any of y m d h M s b.
fn is_date_format(code: &str) -> bool {
    let mut s = String::new();
    let (mut in_q, mut in_b) = (false, false);
    for c in code.chars() {
        match c {
            '"' if !in_b => in_q = !in_q,
            '[' if !in_q => in_b = true,
            ']' if in_b => in_b = false,
            _ if in_q || in_b => {}
            c => s.push(c),
        }
    }
    s.chars().any(|c| matches!(c, 'y' | 'm' | 'd' | 'h' | 'M' | 's' | 'b'))
}

/// Excel serial → JS Date milliseconds (rounded like exceljs) → serial again, so imported dates carry the same float noise.
fn date_roundtrip(v: f64) -> f64 {
    let ms = ((v - 25569.0) * 86_400_000.0).round();
    ms / 86_400_000.0 + 25569.0
}

// ------------------------------------------------------------------ styles

#[derive(Clone, Default, Debug)]
struct Color { argb: Option<String>, theme: Option<u32>, tint: Option<f64> }

fn color_of(attrs: &Attrs) -> Color {
    if let Some(r) = attr(attrs, "rgb") { return Color { argb: Some(r.to_string()), ..Default::default() }; }
    if let Some(t) = attr(attrs, "theme").and_then(|v| v.trim().parse::<u32>().ok()) {
        return Color { theme: Some(t), tint: attr(attrs, "tint").and_then(|v| v.trim().parse().ok()), ..Default::default() };
    }
    Color::default()
}

#[derive(Default, Clone)]
struct Font { name: Option<String>, size: Option<f64>, bold: bool, italic: bool, underline: bool, strike: bool, color: Option<Color>, scheme: bool }

#[derive(Default, Clone)]
struct Border { sides: [Option<(String, Option<Color>)>; 4] } // top, right, bottom, left

#[derive(Default, Clone)]
struct Xf { num_fmt_id: u32, font: Option<usize>, fill: Option<usize>, border: Option<usize>, h: Option<String>, v: Option<String>, wrap: bool, rotation: Option<String> }

#[derive(Default)]
struct Styles { num_fmts: HashMap<u32, String>, fonts: Vec<Font>, fills: Vec<Option<Color>>, borders: Vec<Border>, xfs: Vec<Xf> }

#[derive(PartialEq, Clone, Copy)]
enum Sec { None, NumFmts, Fonts, Fills, Borders, CellXfs, Other }

fn parse_styles(xml: &str) -> Styles {
    let mut st = Styles::default();
    let mut sec = Sec::None;
    let (mut font, mut fill, mut border, mut xf): (Option<Font>, Option<Option<Color>>, Option<Border>, Option<Xf>) = (None, None, None, None);
    let mut side: Option<usize> = None;
    let mut gradient = false;
    for t in tok(xml) {
        match t {
            Token::Open { name, attrs, self_close } => match name.as_str() {
                "numfmts" => sec = Sec::NumFmts,
                "fonts" => sec = Sec::Fonts,
                "fills" => sec = Sec::Fills,
                "borders" => sec = Sec::Borders,
                "cellxfs" => sec = Sec::CellXfs,
                "cellstylexfs" | "dxfs" | "cellstyles" | "tablestyles" | "colors" | "extlst" => sec = Sec::Other,
                "numfmt" if sec == Sec::NumFmts => {
                    if let (Some(id), Some(code)) = (attr(&attrs, "numfmtid").and_then(|v| v.trim().parse().ok()), attr_dec(&attrs, "formatcode")) { st.num_fmts.insert(id, code); }
                }
                "font" if sec == Sec::Fonts => { if self_close { st.fonts.push(Font::default()); } else { font = Some(Font::default()); } }
                "b" | "i" | "strike" | "u" | "sz" | "name" | "scheme" | "color" if font.is_some() => {
                    let f = font.as_mut().expect("checked");
                    match name.as_str() {
                        // an explicit val="0"/"false" (openpyxl, LibreOffice) switches the property off
                        "b" => f.bold = bool_val(&attrs),
                        "i" => f.italic = bool_val(&attrs),
                        "strike" => f.strike = bool_val(&attrs),
                        "u" => f.underline = attr(&attrs, "val") != Some("none"),
                        "sz" => f.size = attr(&attrs, "val").and_then(|v| v.trim().parse().ok()),
                        "name" => f.name = attr_dec(&attrs, "val"),
                        "scheme" => f.scheme = attr(&attrs, "val").is_some_and(|v| !v.is_empty()),
                        _ => f.color = Some(color_of(&attrs)),
                    }
                }
                "fill" if sec == Sec::Fills => { if self_close { st.fills.push(None); } else { fill = Some(None); gradient = false; } }
                "gradientfill" if fill.is_some() => gradient = true,
                "fgcolor" if fill.is_some() && !gradient => fill = Some(Some(color_of(&attrs))),
                "border" if sec == Sec::Borders => { if self_close { st.borders.push(Border::default()); } else { border = Some(Border::default()); } }
                "top" | "right" | "bottom" | "left" if border.is_some() => {
                    let i = ["top", "right", "bottom", "left"].iter().position(|n| *n == name).expect("listed");
                    if let Some(s) = attr(&attrs, "style").filter(|s| !s.is_empty()) { border.as_mut().expect("checked").sides[i] = Some((s.to_string(), None)); }
                    if !self_close { side = Some(i); }
                }
                "color" if border.is_some() && side.is_some() => {
                    if let Some(Some((_, c))) = side.and_then(|i| border.as_mut().map(|b| &mut b.sides[i])) { *c = Some(color_of(&attrs)); }
                }
                "xf" if sec == Sec::CellXfs => {
                    let x = Xf {
                        num_fmt_id: attr(&attrs, "numfmtid").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
                        font: attr(&attrs, "fontid").and_then(|v| v.trim().parse().ok()),
                        fill: attr(&attrs, "fillid").and_then(|v| v.trim().parse().ok()),
                        border: attr(&attrs, "borderid").and_then(|v| v.trim().parse().ok()),
                        ..Default::default()
                    };
                    if self_close { st.xfs.push(x); } else { xf = Some(x); }
                }
                "alignment" if xf.is_some() => {
                    let x = xf.as_mut().expect("checked");
                    x.h = attr(&attrs, "horizontal").map(String::from);
                    x.v = attr(&attrs, "vertical").map(String::from);
                    x.wrap = truthy(attr(&attrs, "wraptext"));
                    x.rotation = attr(&attrs, "textrotation").map(String::from);
                }
                _ => {}
            },
            Token::Close(name) => match name.as_str() {
                "numfmts" | "fonts" | "fills" | "borders" | "cellxfs" | "cellstylexfs" | "dxfs" => sec = Sec::None,
                "font" => { if let Some(f) = font.take() { st.fonts.push(f); } }
                "fill" => { if let Some(f) = fill.take() { st.fills.push(f); } }
                "border" => { if let Some(b) = border.take() { st.borders.push(b); } side = None; }
                "top" | "right" | "bottom" | "left" => side = None,
                "xf" => { if let Some(x) = xf.take() { st.xfs.push(x); } }
                _ => {}
            },
            _ => {}
        }
    }
    st
}

// Office theme palette as SpreadsheetML indexes it (light/dark swapped at 0/1 and 2/3)
const THEME_COLORS: [&str; 12] = ["#FFFFFF", "#000000", "#E7E6E6", "#44546A", "#4472C4", "#ED7D31", "#A5A5A5", "#FFC000", "#5B9BD5", "#70AD47", "#0563C1", "#954F72"];

fn argb_to_hex(argb: &str) -> Option<String> {
    let s = argb.trim().to_ascii_uppercase();
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    match s.len() {
        8 => if s.starts_with("00") { None } else { Some(format!("#{}", &s[2..])) },
        6 => Some(format!("#{s}")),
        _ => None,
    }
}

fn color_to_rgb(c: &Color) -> Option<String> {
    if let Some(a) = &c.argb { return argb_to_hex(a); }
    let base = THEME_COLORS.get(c.theme? as usize)?;
    Some(apply_tint(base, c.tint.filter(|t| t.is_finite()).unwrap_or(0.0)))
}

fn rgb_to_hsl(hex: &str) -> (f64, f64, f64) {
    let n = hex.trim_start_matches('#');
    let ch = |i: usize| u8::from_str_radix(&n[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
    let (r, g, b) = (ch(0), ch(2), ch(4));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min { return (0.0, 0.0, l); }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r { (g - b) / d + if g < b { 6.0 } else { 0.0 } } else if max == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    (h / 6.0, s, l)
}

fn hue_to_channel(p: f64, q: f64, mut t: f64) -> f64 {
    if t < 0.0 { t += 1.0; }
    if t > 1.0 { t -= 1.0; }
    if t < 1.0 / 6.0 { return p + (q - p) * 6.0 * t; }
    if t < 0.5 { return q; }
    if t < 2.0 / 3.0 { return p + (q - p) * (2.0 / 3.0 - t) * 6.0; }
    p
}

fn clamp_channel(v: f64) -> String {
    // JS Math.round rounds halves up
    let byte = (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8;
    format!("{byte:02X}")
}

fn apply_tint(hex: &str, tint: f64) -> String {
    if tint == 0.0 { return hex.to_string(); }
    let (h, s, l) = rgb_to_hsl(hex);
    let lum = if tint < 0.0 { l * (1.0 + tint) } else { l * (1.0 - tint) + tint };
    if s == 0.0 {
        let v = clamp_channel(lum);
        return format!("#{v}{v}{v}");
    }
    let q = if lum < 0.5 { lum * (1.0 + s) } else { lum + s - lum * s };
    let p = 2.0 * lum - q;
    format!("#{}{}{}", clamp_channel(hue_to_channel(p, q, h + 1.0 / 3.0)), clamp_channel(hue_to_channel(p, q, h)), clamp_channel(hue_to_channel(p, q, h - 1.0 / 3.0)))
}

fn border_style_id(s: &str) -> Option<i64> {
    Some(match s {
        "thin" => 1, "hair" => 2, "dotted" => 3, "dashed" => 4, "dashDot" => 5, "dashDotDot" => 6, "double" => 7, "medium" => 8,
        "mediumDashed" => 9, "mediumDashDot" => 10, "mediumDashDotDot" => 11, "slantDashDot" => 12, "thick" => 13,
        _ => return None,
    })
}

impl Styles {
    fn num_fmt(&self, xf: &Xf) -> Option<String> {
        if xf.num_fmt_id == 0 { return None; }
        self.num_fmts.get(&xf.num_fmt_id).cloned().or_else(|| builtin_format(xf.num_fmt_id).map(String::from))
    }

    /// The Univer style of one `cellXfs` entry (`readStyle`), if it carries anything.
    fn style(&self, xf: &Xf) -> Option<Map<String, Value>> {
        let mut s = Map::new();
        if let Some(f) = xf.font.and_then(|i| self.fonts.get(i)) {
            let theme_font = f.scheme;
            if let Some(n) = f.name.as_ref().filter(|n| !n.is_empty() && !theme_font) { s.insert("ff".into(), json!(n)); }
            if let Some(sz) = f.size.filter(|z| z.is_finite() && !(theme_font && *z == 11.0)) { s.insert("fs".into(), num(sz)); }
            if f.bold { s.insert("bl".into(), json!(1)); }
            if f.italic { s.insert("it".into(), json!(1)); }
            if f.underline { s.insert("ul".into(), json!({"s": 1})); }
            if f.strike { s.insert("st".into(), json!({"s": 1})); }
            if let Some(c) = f.color.as_ref().filter(|c| c.theme != Some(1)) {
                if let Some(rgb) = color_to_rgb(c) { s.insert("cl".into(), json!({"rgb": rgb})); }
            }
        }
        if let Some(rgb) = xf.fill.and_then(|i| self.fills.get(i)).and_then(|f| f.as_ref()).and_then(color_to_rgb) { s.insert("bg".into(), json!({"rgb": rgb})); }
        match xf.h.as_deref() { Some("left") => { s.insert("ht".into(), json!(1)); } Some("center") => { s.insert("ht".into(), json!(2)); } Some("right") => { s.insert("ht".into(), json!(3)); } _ => {} }
        match xf.v.as_deref() { Some("top") => { s.insert("vt".into(), json!(1)); } Some("center") => { s.insert("vt".into(), json!(2)); } Some("bottom") => { s.insert("vt".into(), json!(3)); } _ => {} }
        if xf.wrap { s.insert("tb".into(), json!(3)); }
        if let Some(r) = xf.rotation.as_deref().and_then(|r| r.trim().parse::<i64>().ok()) {
            if r == 255 { s.insert("tr".into(), json!({"v": 1})); }
            else if (1..=90).contains(&r) { s.insert("tr".into(), json!({"a": r})); }
            else if (91..=180).contains(&r) { s.insert("tr".into(), json!({"a": 90 - r})); }
        }
        if let Some(b) = xf.border.and_then(|i| self.borders.get(i)) {
            let mut bd = Map::new();
            for (key, idx) in [("t", 0), ("r", 1), ("b", 2), ("l", 3)] {
                let Some((style, color)) = &b.sides[idx] else { continue };
                let Some(id) = border_style_id(style) else { continue };
                let mut side = Map::new();
                side.insert("s".into(), json!(id));
                if let Some(rgb) = color.as_ref().and_then(color_to_rgb) { side.insert("cl".into(), json!({"rgb": rgb})); }
                bd.insert(key.into(), Value::Object(side));
            }
            if !bd.is_empty() { s.insert("bd".into(), Value::Object(bd)); }
        }
        if let Some(code) = self.num_fmt(xf) { s.insert("n".into(), json!({"pattern": code})); }
        if s.is_empty() { None } else { Some(s) }
    }
}

// ------------------------------------------------------------------ shared strings

fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = vec![];
    let (mut cur, mut in_si, mut in_t, mut skip) = (String::new(), false, false, 0u32);
    for t in tok(xml) {
        match t {
            Token::Open { name, self_close, .. } => match name.as_str() {
                "si" => { in_si = true; cur.clear(); if self_close { out.push(String::new()); in_si = false; } }
                "rph" if in_si && !self_close => skip += 1,
                "t" if in_si && skip == 0 && !self_close => in_t = true,
                _ => {}
            },
            Token::Close(name) => match name.as_str() {
                "si" => { out.push(std::mem::take(&mut cur)); in_si = false; }
                "rph" => skip = skip.saturating_sub(1),
                "t" => in_t = false,
                _ => {}
            },
            Token::Text(s) if in_t => cur.push_str(&decode_entities(&s)),
            Token::CData(s) if in_t => cur.push_str(&s),
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------ relationships and paths

fn rels(xml: &str) -> HashMap<String, (String, String)> {
    let mut m = HashMap::new();
    for t in tok(xml) {
        if let Token::Open { name, attrs, .. } = t {
            if name == "relationship" {
                if let (Some(id), Some(target)) = (attr(&attrs, "id"), attr_dec(&attrs, "target")) {
                    m.insert(id.to_string(), (attr(&attrs, "type").unwrap_or("").to_string(), target));
                }
            }
        }
    }
    m
}

/// Resolve a relationship target against the folder of the part that holds it.
fn resolve(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') { vec![] } else { base_dir.split('/').filter(|p| !p.is_empty()).collect() };
    for seg in target.split('/') {
        match seg { "" | "." => {} ".." => { parts.pop(); } s => parts.push(s) }
    }
    parts.join("/")
}

fn rels_path(part: &str) -> String {
    match part.rsplit_once('/') { Some((d, f)) => format!("{d}/_rels/{f}.rels"), None => format!("_rels/{part}.rels") }
}

fn dir_of(part: &str) -> &str { part.rsplit_once('/').map_or("", |(d, _)| d) }

// ------------------------------------------------------------------ worksheets

struct Link { url: Option<String> }

struct Parsed {
    sheet: Map<String, Value>,
    validations: Vec<Value>,
    drawing_rid: Option<String>,
    links: HashMap<(usize, usize), Link>,
    cells: usize,
}

/// Limits for hostile files: Excel's own 32,767-character cell text cap, and workbook-wide caps on cells and text.
const MAX_CELL_CHARS: usize = 32_767;
const MAX_CELLS: usize = 1_000_000;
const MAX_TEXT_BYTES: usize = 128 << 20;
const MAX_IMAGES_PER_SHEET: usize = 200;
const MAX_VALIDATION_RULES: usize = 5_000;
const MAX_VALIDATION_CELLS: usize = 100_000;

struct Ctx<'a> { strings: &'a [String], styles: &'a Styles, style_cache: Vec<Option<Option<Map<String, Value>>>>, date1904: bool, cells: usize, text_bytes: usize, over: bool }

fn clip(s: &str) -> String { if s.len() <= MAX_CELL_CHARS { s.to_string() } else { s.chars().take(MAX_CELL_CHARS).collect() } }

fn js_len(s: &str) -> usize { s.encode_utf16().count() }

fn sanitize_link(url: &str) -> Option<String> {
    let t = url.trim();
    let lower = t.to_ascii_lowercase();
    let ok = (lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:"))
        && t.chars().all(|c| !matches!(c, '\u{0}'..='\u{20}' | '\u{7f}'))
        && t.split_once(':').is_some_and(|(_, rest)| !rest.trim_start_matches('/').is_empty());
    ok.then(|| t.to_string())
}

fn linked_document(text: &str, url: &str, range_id: &str) -> Value {
    let n = js_len(text);
    json!({
        "id": "d", "documentStyle": {},
        "body": {
            "dataStream": format!("{text}\r\n"),
            "textRuns": [{"ts": {}, "st": 0, "ed": n}],
            "paragraphs": [{"startIndex": n}],
            "sectionBreaks": [{"startIndex": n + 1}],
            "customRanges": [{"rangeId": range_id, "rangeType": 0, "startIndex": 0, "endIndex": n as i64 - 1, "properties": {"url": url, "refId": range_id}}],
        }
    })
}

fn a1(row: usize, col: usize) -> String {
    let mut c = col + 1;
    let mut s = String::new();
    while c > 0 { s.insert(0, (b'A' + ((c - 1) % 26) as u8) as char); c = (c - 1) / 26; }
    format!("{s}{}", row + 1)
}

struct Dv { ty: String, op: Option<String>, sqref: String, f1: Option<String>, f2: Option<String> }

fn push_text(cur: &mut Option<Cur>, dvs: &mut [Dv], field: &str, dv_field: &str, rph: u32, s: &str) {
    if let Some(c) = cur.as_mut() {
        match field { "f" => c.f.push_str(s), "v" => c.v.push_str(s), "is" if rph == 0 => c.inline.push_str(s), _ => {} }
    } else if let Some(d) = dvs.last_mut() {
        match dv_field { "f1" => d.f1.get_or_insert_with(String::new).push_str(s), "f2" => d.f2.get_or_insert_with(String::new).push_str(s), _ => {} }
    }
}

fn read_sheet(xml: &str, id: &str, name: &str, hidden: bool, cx: &mut Ctx) -> Parsed {
    let mut cells = Map::new();
    let mut merges: Vec<Value> = vec![];
    let mut row_data = Map::new();
    let mut col_data = Map::new();
    let (mut max_row, mut max_col) = (0usize, 0usize);
    let mut cur: Option<Cur> = None;
    let mut field = "";
    let mut shared_masters: HashMap<String, (String, usize, usize)> = HashMap::new();
    let mut f_shared: Option<String> = None;
    let (mut next_row, mut next_col) = (0usize, 0usize);
    let mut default_row_h: Option<f64> = None;
    let mut default_col_w: Option<f64> = None;
    let mut show_grid = true;
    let mut drawing_rid = None;
    let mut links: HashMap<(usize, usize), Link> = HashMap::new();
    let mut hyperlink_rids: Vec<((usize, usize), Option<String>)> = vec![];
    let mut dvs: Vec<Dv> = vec![];
    let mut dv_field = "";
    let mut rph = 0u32;
    for t in tok(xml) {
        if cx.over { break; }
        match t {
            Token::Open { name, attrs, self_close } => match name.as_str() {
                "sheetformatpr" => {
                    if let Some(h) = attr(&attrs, "defaultrowheight").and_then(|v| v.trim().parse::<f64>().ok()).filter(|h| h.is_finite()) { default_row_h = Some(h); }
                    default_col_w = attr(&attrs, "defaultcolwidth").and_then(|v| v.trim().parse::<f64>().ok()).filter(|w| w.is_finite());
                }
                "sheetview" => if attr(&attrs, "showgridlines").is_some_and(|v| v == "0" || v == "false") { show_grid = false; },
                "drawing" => drawing_rid = attr_ns(&attrs, "id").map(String::from),
                "hyperlink" => {
                    if let Some((r, _, c, _)) = attr(&attrs, "ref").and_then(range_ref) {
                        hyperlink_rids.push(((r, c), attr_ns(&attrs, "id").map(String::from)));
                    }
                }
                "row" => {
                    let r = attr(&attrs, "r").and_then(|v| v.parse::<usize>().ok()).map_or(next_row, |v| v.saturating_sub(1)).min(MAX_ROWS - 1);
                    next_col = 0;
                    next_row = r + 1;
                    let mut meta = Map::new();
                    // exceljs takes any explicit height, custom or not
                    if let Some(h) = attr(&attrs, "ht").and_then(|v| v.trim().parse::<f64>().ok()).filter(|h| h.is_finite()) { meta.insert("h".into(), num(h / 0.75)); }
                    if truthy(attr(&attrs, "hidden")) { meta.insert("hd".into(), json!(1)); }
                    if !meta.is_empty() { row_data.insert(r.to_string(), Value::Object(meta)); }
                    max_row = max_row.max(r);
                }
                "col" => {
                    let (min, max) = (attr(&attrs, "min").and_then(|v| v.parse::<usize>().ok()), attr(&attrs, "max").and_then(|v| v.parse::<usize>().ok()));
                    if let (Some(min), Some(max)) = (min.filter(|m| (1..=MAX_COLS).contains(m)), max.map(|m| m.min(MAX_COLS))) {
                        let mut meta = Map::new();
                        // exceljs gives a column without a width the default 9
                        let w = attr(&attrs, "width").and_then(|v| v.trim().parse::<f64>().ok()).filter(|w| w.is_finite()).unwrap_or(9.0);
                        meta.insert("w".into(), num(w * 7.0 + 5.0));
                        if truthy(attr(&attrs, "hidden")) { meta.insert("hd".into(), json!(1)); }
                        if !meta.is_empty() { for c in min - 1..max { col_data.entry(c.to_string()).or_insert_with(|| Value::Object(meta.clone())); } }
                    }
                }
                "mergecell" => if let Some((r1, r2, c1, c2)) = attr(&attrs, "ref").and_then(range_ref) { merges.push(json!({"startRow": r1, "endRow": r2, "startColumn": c1, "endColumn": c2})); },
                "c" => {
                    let pos = attr(&attrs, "r").and_then(cell_ref).unwrap_or((next_row.saturating_sub(1), next_col.min(MAX_COLS - 1)));
                    next_col = pos.1 + 1;
                    cur = Some(Cur { r: pos.0, c: pos.1, ty: attr(&attrs, "t").unwrap_or("n").to_string(), style: attr(&attrs, "s").and_then(|v| v.parse().ok()), f: String::new(), v: String::new(), inline: String::new(), has_f: false });
                    if self_close { finish_cell(cur.take(), cx, &mut cells, &mut max_row, &mut max_col); }
                }
                "f" if cur.is_some() => {
                    if let Some(c) = cur.as_mut() { c.has_f = true; }
                    f_shared = if attr(&attrs, "t") == Some("shared") { attr(&attrs, "si").map(String::from) } else { None };
                    if self_close { resolve_shared(&mut cur, &f_shared, &shared_masters); } else { field = "f"; }
                }
                "v" if cur.is_some() && !self_close => field = "v",
                "t" if cur.is_some() && !self_close => field = "is",
                "datavalidation" => {
                    dvs.push(Dv { ty: attr(&attrs, "type").filter(|t| !t.is_empty()).unwrap_or("any").to_string(), op: attr(&attrs, "operator").map(String::from), sqref: attr(&attrs, "sqref").unwrap_or("").to_string(), f1: None, f2: None });
                }
                "rph" if cur.is_some() && !self_close => rph += 1,
                "formula1" if !dvs.is_empty() && !self_close => { dv_field = "f1"; if let Some(d) = dvs.last_mut() { d.f1 = Some(String::new()); } }
                "formula2" if !dvs.is_empty() && !self_close => { dv_field = "f2"; if let Some(d) = dvs.last_mut() { d.f2 = Some(String::new()); } }
                _ => {}
            },
            Token::Close(name) => match name.as_str() {
                "f" => { field = ""; resolve_shared(&mut cur, &f_shared, &shared_masters); if let (Some(si), Some(c)) = (f_shared.take(), cur.as_ref()) { if !c.f.is_empty() { shared_masters.entry(si).or_insert((c.f.clone(), c.r, c.c)); } } }
                "v" | "t" => field = "",
                "formula1" | "formula2" => dv_field = "",
                "rph" => rph = rph.saturating_sub(1),
                "c" => finish_cell(cur.take(), cx, &mut cells, &mut max_row, &mut max_col),
                _ => {}
            },
            Token::Text(raw) => { let s = decode_entities(&raw); push_text(&mut cur, &mut dvs, field, dv_field, rph, &s); }
            Token::CData(s) => push_text(&mut cur, &mut dvs, field, dv_field, rph, &s),
        }
    }
    for (pos, rid) in hyperlink_rids { links.insert(pos, Link { url: rid }); }
    let mut sheet = Map::new();
    sheet.insert("id".into(), json!(id));
    sheet.insert("name".into(), json!(name));
    sheet.insert("cellData".into(), Value::Object(cells));
    sheet.insert("rowData".into(), Value::Object(row_data));
    sheet.insert("columnData".into(), Value::Object(col_data));
    sheet.insert("mergeData".into(), Value::Array(merges));
    sheet.insert("rowCount".into(), json!(DEFAULT_ROWS.max(max_row + 1)));
    sheet.insert("columnCount".into(), json!(DEFAULT_COLS.max(max_col + 1)));
    sheet.insert("hidden".into(), json!(u8::from(hidden)));
    sheet.insert("showGridlines".into(), json!(u8::from(show_grid)));
    sheet.insert("rowHeader".into(), json!({"width": 46, "hidden": 0}));
    sheet.insert("columnHeader".into(), json!({"height": 20, "hidden": 0}));
    if let Some(w) = default_col_w { sheet.insert("defaultColumnWidth".into(), num(w * 7.0 + 5.0)); }
    if let Some(h) = default_row_h { sheet.insert("defaultRowHeight".into(), num(h / 0.75)); }
    let validations = build_validations(&dvs.into_iter().map(|d| (d.ty, d.op, d.sqref, d.f1, d.f2)).collect::<Vec<_>>(), id);
    Parsed { sheet, validations, drawing_rid, links, cells: 0 }
}

/// A shared-formula follower (`<f t="shared" si=".."/>`) takes the master's formula slid by the distance between the two cells.
fn resolve_shared(cur: &mut Option<Cur>, si: &Option<String>, masters: &HashMap<String, (String, usize, usize)>) {
    let (Some(c), Some(si)) = (cur.as_mut(), si) else { return };
    if !c.f.is_empty() { return; }
    if let Some((f, mr, mc)) = masters.get(si) { c.f = slide_formula(f, c.r as i64 - *mr as i64, c.c as i64 - *mc as i64); }
}

/// exceljs' `slideFormula`: shift every relative cell reference (functions are left alone).
fn slide_formula(formula: &str, dr: i64, dc: i64) -> String {
    static TOKEN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static CELL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let token = TOKEN.get_or_init(|| regex::Regex::new(r"(?i)(([a-z_\-0-9]*)!)?([a-z0-9_$]{2,})([(])?").expect("static regex"));
    let cell = CELL.get_or_init(|| regex::Regex::new(r"(?i)^([$])?([a-z]+)([$])?([1-9][0-9]*)$").expect("static regex"));
    token.replace_all(formula, |c: &regex::Captures| {
        let whole = c.get(0).map_or("", |m| m.as_str());
        if c.get(4).is_some() { return whole.to_string(); }
        let Some(m) = cell.captures(&c[3]) else { return whole.to_string() };
        let letters = m[2].to_ascii_uppercase();
        if letters.len() > 3 || (letters.len() == 3 && letters.as_str() > "XFD") { return whole.to_string(); }
        let mut col = letters.bytes().fold(0i64, |a, b| a * 26 + (b - b'A' + 1) as i64);
        let Ok(mut row) = m[4].parse::<i64>() else { return whole.to_string() };
        if m.get(1).is_none() { col = col.saturating_add(dc); }
        if m.get(3).is_none() { row = row.saturating_add(dr); }
        // a slide that leaves the sheet leaves the reference as it was
        if !(1..=MAX_COLS as i64).contains(&col) || !(1..=MAX_ROWS as i64).contains(&row) { return whole.to_string(); }
        let mut name = String::new();
        let mut n = col;
        while n > 0 { name.insert(0, (b'A' + ((n - 1) % 26) as u8) as char); n = (n - 1) / 26; }
        format!("{}{}{}{}{}", c.get(1).map_or("", |m| m.as_str()), m.get(1).map_or("", |m| m.as_str()), name, m.get(3).map_or("", |m| m.as_str()), row)
    }).into_owned()
}

fn finish_cell(cur: Option<Cur>, cx: &mut Ctx, cells: &mut Map<String, Value>, max_row: &mut usize, max_col: &mut usize) {
    let Some(c) = cur else { return };
    let (r, col) = (c.r, c.c);
    if let Some(cell) = c.build(cx) {
        *max_row = (*max_row).max(r);
        *max_col = (*max_col).max(col);
        put_cell(cells, r, col, Value::Object(cell));
    }
}

struct Cur { r: usize, c: usize, ty: String, style: Option<usize>, f: String, v: String, inline: String, has_f: bool }

impl Cur {
    /// The cell as Univer stores it, or `None` when it has neither value, formula nor style.
    fn build(self, cx: &mut Ctx) -> Option<Map<String, Value>> {
        let xf = self.style.and_then(|i| cx.styles.xfs.get(i)).cloned();
        let is_date = xf.as_ref().and_then(|x| cx.styles.num_fmt(x)).is_some_and(|f| is_date_format(&f));
        let mut cell = Map::new();
        let formula = self.has_f && !self.f.is_empty();
        if formula { let f = clip(&format!("={}", self.f)); cx.text_bytes += f.len(); cell.insert("f".into(), json!(f)); }
        let put = |cell: &mut Map<String, Value>, v: Value, t: i64| { cell.insert("v".into(), v); cell.insert("t".into(), json!(t)); };
        match self.ty.as_str() {
            "s" => if let Some(s) = self.v.trim().parse::<usize>().ok().and_then(|i| cx.strings.get(i)) { let s = clip(s); cx.text_bytes += s.len(); put(&mut cell, json!(s), STRING); },
            "b" => put(&mut cell, json!(self.v.trim().parse::<i64>().map_or(true, |n| n != 0)), BOOLEAN),
            "str" => if !self.v.is_empty() { let s = clip(&self.v); cx.text_bytes += s.len(); put(&mut cell, json!(s), STRING); },
            "e" => if !self.v.is_empty() { put(&mut cell, json!(clip(&self.v)), FORCE_STRING); },
            "inlineStr" | "inlinestr" => { let s = clip(&self.inline); cx.text_bytes += s.len(); put(&mut cell, json!(s), STRING) }
            "d" => {
                if let Ok(d) = chrono::DateTime::parse_from_rfc3339(self.v.trim()).map(|d| d.timestamp_millis() as f64).or_else(|_| {
                    chrono::NaiveDate::parse_from_str(self.v.trim(), "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis() as f64)
                }) { put(&mut cell, num(d / 86_400_000.0 + 25569.0), NUMBER); }
            }
            _ => if let Ok(n) = self.v.trim().parse::<f64>() {
                if n.is_finite() { put(&mut cell, num(if is_date && !formula { date_roundtrip(if cx.date1904 { n + 1462.0 } else { n }) } else { n }), NUMBER); }
            },
        }
        if let Some(Some(style)) = xf.as_ref().map(|x| {
            let i = self.style.expect("xf came from the style index");
            if cx.style_cache.len() <= i { cx.style_cache.resize(i + 1, None); }
            cx.style_cache[i].get_or_insert_with(|| cx.styles.style(x)).clone()
        }) { cell.insert("s".into(), Value::Object(style)); }
        if cell.is_empty() { return None; }
        cx.cells += 1;
        if cx.cells > MAX_CELLS || cx.text_bytes > MAX_TEXT_BYTES { cx.over = true; }
        Some(cell)
    }
}

// ------------------------------------------------------------------ data validation

type DvRow = (String, Option<String>, String, Option<String>, Option<String>);

/// An inline `"a,b,c"` list → its options; `None` for a range reference.
fn inline_list(formula: &str) -> Option<Vec<String>> {
    if formula.len() < 2 || !formula.starts_with('"') || !formula.ends_with('"') { return None; }
    Some(formula[1..formula.len() - 1].split(',').filter(|o| !o.is_empty()).map(String::from).collect())
}

/// Merge a set of cells into rectangles: each unclaimed cell grows right as far as it can, then down over full-width rows.
fn coalesce(cells: &[(usize, usize)]) -> Vec<(usize, usize, usize, usize)> {
    let present: std::collections::HashSet<(usize, usize)> = cells.iter().copied().collect();
    let mut claimed = std::collections::HashSet::new();
    let mut sorted = cells.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut out = vec![];
    for (row, col) in sorted {
        if claimed.contains(&(row, col)) { continue; }
        let mut end_col = col;
        while present.contains(&(row, end_col + 1)) && !claimed.contains(&(row, end_col + 1)) { end_col += 1; }
        let mut end_row = row;
        while (col..=end_col).all(|c| present.contains(&(end_row + 1, c)) && !claimed.contains(&(end_row + 1, c))) { end_row += 1; }
        for r in row..=end_row { for c in col..=end_col { claimed.insert((r, c)); } }
        out.push((row, end_row, col, end_col));
    }
    out
}

fn build_validations(dvs: &[DvRow], sheet_id: &str) -> Vec<Value> {
    // group the cells of identical rules, in order of first appearance
    let mut groups: Vec<(DvRow, Vec<(usize, usize, usize, usize)>)> = vec![];
    let mut index: HashMap<(String, Option<String>, Option<String>, Option<String>), usize> = HashMap::new();
    for dv in dvs {
        let mut dv = dv.clone();
        // exceljs: comparison types default to "between"; a rule without formulas carries neither formulas nor operator
        if dv.3.is_none() && dv.4.is_none() { dv.1 = None; }
        else if !matches!(dv.0.as_str(), "any" | "list" | "custom") && dv.1.is_none() { dv.1 = Some("between".into()); }
        let rects: Vec<_> = dv.2.split_whitespace().filter_map(range_ref).collect();
        if rects.is_empty() { continue; }
        let key = (dv.0.clone(), dv.1.clone(), dv.3.clone(), dv.4.clone());
        match index.get(&key) {
            Some(&i) => groups[i].1.extend(rects),
            None => { if groups.len() >= MAX_VALIDATION_RULES { continue; } index.insert(key, groups.len()); groups.push((dv, rects)); }
        }
    }
    let mut budget = MAX_VALIDATION_CELLS;
    let mut rules = vec![];
    for ((ty, op, _, f1, f2), rects) in groups {
        let total: usize = rects.iter().map(|(r1, r2, c1, c2)| (r2 - r1 + 1).saturating_mul(c2 - c1 + 1)).fold(0, usize::saturating_add);
        // small rules are merged into rectangles cell by cell (as exceljs' per-cell model does); a sheet-wide budget keeps hostile ones cheap
        let ranges: Vec<(usize, usize, usize, usize)> = if total <= budget {
            budget -= total;
            let cells: Vec<(usize, usize)> = rects.iter().flat_map(|&(r1, r2, c1, c2)| (r1..=r2).flat_map(move |r| (c1..=c2).map(move |c| (r, c)))).collect();
            coalesce(&cells)
        } else { rects };
        let mut rule = Map::new();
        rule.insert("uid".into(), json!(format!("dv-{sheet_id}-{}", rules.len())));
        rule.insert("type".into(), json!(ty));
        rule.insert("ranges".into(), Value::Array(ranges.iter().map(|&(r1, r2, c1, c2)| json!({"startRow": r1, "endRow": r2, "startColumn": c1, "endColumn": c2})).collect()));
        if ty == "list" {
            let Some(raw) = f1 else { continue };
            let formula = match inline_list(&raw) { Some(o) => serde_json::to_string(&o).unwrap_or_default(), None => raw };
            rule.insert("formula1".into(), json!(formula));
        } else {
            if let Some(f) = f1 { rule.insert("formula1".into(), json!(f)); }
            if let Some(f) = f2 { rule.insert("formula2".into(), json!(f)); }
            if let Some(o) = op { rule.insert("operator".into(), json!(o)); }
        }
        rules.push(Value::Object(rule));
    }
    rules
}

// ------------------------------------------------------------------ images

/// Column widths and row heights of a sheet in pixels (explicit sizes over the sheet defaults), built once per sheet.
struct Dims { cols: HashMap<usize, f64>, rows: HashMap<usize, f64>, dcw: f64, drh: f64 }

impl Dims {
    fn from(sheet: &Map<String, Value>) -> Dims {
        let pick = |key: &str, f: &str| -> HashMap<usize, f64> {
            sheet.get(key).and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v[f].as_f64()?))).collect()).unwrap_or_default()
        };
        Dims { cols: pick("columnData", "w"), rows: pick("rowData", "h"),
            dcw: sheet.get("defaultColumnWidth").and_then(Value::as_f64).unwrap_or(88.0), drh: sheet.get("defaultRowHeight").and_then(Value::as_f64).unwrap_or(24.0) }
    }
    fn cw(&self, c: usize) -> f64 { self.cols.get(&c).copied().unwrap_or(self.dcw) }
    fn rh(&self, r: usize) -> f64 { self.rows.get(&r).copied().unwrap_or(self.drh) }
    /// Absolute x of a fractional column point (no header gutter): default widths plus the explicit ones to its left.
    fn col_point(&self, col: f64) -> f64 {
        let c = (col.floor().max(0.0) as usize).min(MAX_COLS);
        c as f64 * self.dcw + self.cols.iter().filter(|(k, _)| **k < c).map(|(_, w)| w - self.dcw).sum::<f64>() + (col - c as f64) * self.cw(c)
    }
    fn row_point(&self, row: f64) -> f64 {
        let r = (row.floor().max(0.0) as usize).min(MAX_ROWS);
        r as f64 * self.drh + self.rows.iter().filter(|(k, _)| **k < r).map(|(_, h)| h - self.drh).sum::<f64>() + (row - r as f64) * self.rh(r)
    }
    fn anchor(&self, col: f64, row: f64) -> Value {
        let (c, r) = (col.floor(), row.floor());
        json!({"row": r as i64, "rowOffset": num((row - r) * self.rh(r as usize)), "column": c as i64, "columnOffset": num((col - c) * self.cw(c as usize))})
    }
}

fn track_at_px(target: f64, size_of: impl Fn(usize) -> f64) -> (usize, f64) {
    if target <= 0.0 { return (0, 0.0); }
    let mut cumulative = 0.0;
    for i in 0..100_000 {
        let size = size_of(i);
        if size <= 0.0 { continue; }
        if cumulative + size > target { return (i, target - cumulative); }
        cumulative += size;
    }
    (0, 0.0)
}

struct Anchor { native_col: f64, col_off: f64, native_row: f64, row_off: f64 }

/// exceljs' fractional `col` / `row` of an anchor (offsets in EMU against the cell's size in EMU).
fn anchor_point(a: &Anchor, col_emu: f64, row_emu: f64) -> (f64, f64) {
    (a.native_col + (col_emu - 1.0).min(a.col_off) / col_emu, a.native_row + (row_emu - 1.0).min(a.row_off) / row_emu)
}

struct SheetGeom { col_custom: HashMap<usize, f64>, row_ht: HashMap<usize, f64> }

fn read_images(z: &mut Zip<std::io::Cursor<&[u8]>>, drawing_part: &str, sheet: &Dims, geom: &SheetGeom, sheet_id: &str, budget: &mut u64) -> Option<Value> {
    let xml = z.text(drawing_part)?;
    let drels = rels(&z.text(&rels_path(drawing_part)).unwrap_or_default());
    let dir = dir_of(drawing_part).to_string();
    let mut data = Map::new();
    let mut order: Vec<Value> = vec![];
    // one anchor at a time
    #[derive(Default)]
    struct An { kind: String, from: Option<Anchor>, to: Option<Anchor>, ext: Option<(f64, f64)>, embed: Option<String>, which: u8 }
    let mut cur: Option<An> = None;
    let mut field = String::new();
    let mut index = 0usize;
    let finish = |a: An, index: usize, budget: &mut u64, data: &mut Map<String, Value>, order: &mut Vec<Value>, z: &mut Zip<std::io::Cursor<&[u8]>>| {
        let Some(embed) = a.embed.as_ref() else { return };
        let Some((_, target)) = drels.get(embed) else { return };
        let path = resolve(&dir, target);
        let mime = match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() { Some("png") => "image/png", Some("jpeg" | "jpg") => "image/jpeg", Some("gif") => "image/gif", _ => return };
        let Some(bytes) = z.bytes(&path, (*budget).min(20 << 20)) else { return };
        if bytes.is_empty() { return; }
        // every image costs at least 1 KiB of the budget, so a drawing can't ask for endless empty ones
        *budget = budget.saturating_sub((bytes.len() as u64).max(1024));
        let Some(from) = a.from.as_ref() else { return };
        let emu = |c: usize, row: bool| -> f64 {
            if row { geom.row_ht.get(&c).map_or(180_000.0, |h| (h * 10_000.0).floor()) } else { geom.col_custom.get(&c).map_or(640_000.0, |w| (w * 10_000.0).floor()) }
        };
        let pt = |an: &Anchor| anchor_point(an, emu(an.native_col as usize, false), emu(an.native_row as usize, true));
        let (tl_c, tl_r) = pt(from);
        let (left, top) = (sheet.col_point(tl_c), sheet.row_point(tl_r));
        let (to_anchor, right, bottom) = if let Some(to) = a.to.as_ref() {
            let (bc, br) = pt(to);
            (sheet.anchor(bc, br), sheet.col_point(bc), sheet.row_point(br))
        } else if let Some((w, h)) = a.ext {
            let (right, bottom) = (left + w, top + h);
            let (c, co) = track_at_px(right, |i| sheet.cw(i));
            let (r, ro) = track_at_px(bottom, |i| sheet.rh(i));
            (json!({"row": r, "rowOffset": num(ro), "column": c, "columnOffset": num(co)}), right, bottom)
        } else { return };
        let orient = json!({"angle": 0, "flipX": false, "flipY": false, "skewX": 0, "skewY": 0});
        let mut sheet_transform = Map::new();
        sheet_transform.insert("from".into(), sheet.anchor(tl_c, tl_r));
        sheet_transform.insert("to".into(), to_anchor);
        for (k, v) in orient.as_object().expect("object") { sheet_transform.insert(k.clone(), v.clone()); }
        let mut transform = Map::new();
        transform.insert("left".into(), num(left + HEADER_WIDTH));
        transform.insert("top".into(), num(top + HEADER_HEIGHT));
        transform.insert("width".into(), num((right - left).max(0.0)));
        transform.insert("height".into(), num((bottom - top).max(0.0)));
        for (k, v) in orient.as_object().expect("object") { transform.insert(k.clone(), v.clone()); }
        let drawing_id = format!("image-{sheet_id}-{index}");
        let drawing = json!({
            "unitId": "imported", "subUnitId": sheet_id, "drawingId": drawing_id, "drawingType": 0, "imageSourceType": "BASE64",
            "source": format!("data:{mime};base64,{}", B64.encode(&bytes)),
            "transform": transform, "sheetTransform": sheet_transform, "axisAlignSheetTransform": sheet_transform,
        });
        data.insert(drawing_id.clone(), drawing);
        order.push(json!(drawing_id));
    };
    for t in tokenize(&xml, false) {
        match t {
            Token::Open { name, attrs, self_close } => {
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                match local.as_str() {
                    "twocellanchor" | "onecellanchor" if !self_close && index < MAX_IMAGES_PER_SHEET => { cur = Some(An { kind: local.clone(), ..Default::default() }); }
                    "from" | "to" if cur.is_some() => { if let Some(a) = cur.as_mut() { a.which = if local == "from" { 1 } else { 2 }; let an = Anchor { native_col: 0.0, col_off: 0.0, native_row: 0.0, row_off: 0.0 }; if local == "from" { a.from = Some(an); } else { a.to = Some(an); } } }
                    "col" | "coloff" | "row" | "rowoff" if cur.is_some() => field = local.clone(),
                    "ext" if cur.as_ref().is_some_and(|a| a.kind == "onecellanchor" && a.which == 0 && a.ext.is_none()) => {
                        let (w, h) = (attr(&attrs, "cx").and_then(|v| v.parse::<f64>().ok()), attr(&attrs, "cy").and_then(|v| v.parse::<f64>().ok()));
                        if let (Some(w), Some(h), Some(a)) = (w, h, cur.as_mut()) { a.ext = Some((w / 9525.0, h / 9525.0)); }
                    }
                    "blip" => { if let Some(a) = cur.as_mut() { a.embed = attr_ns(&attrs, "embed").map(String::from); } }
                    _ => {}
                }
            }
            Token::Text(s) if !field.is_empty() => {
                if let Some(a) = cur.as_mut() {
                    let v = s.trim().parse::<f64>().unwrap_or(0.0);
                    if let Some(an) = if a.which == 1 { a.from.as_mut() } else { a.to.as_mut() } {
                        match field.as_str() { "col" => an.native_col = v, "coloff" => an.col_off = v, "row" => an.native_row = v, "rowoff" => an.row_off = v, _ => {} }
                    }
                }
            }
            Token::Close(name) => {
                let local = name.rsplit(':').next().unwrap_or(&name);
                field.clear();
                match local {
                    "from" | "to" => { if let Some(a) = cur.as_mut() { a.which = 0; } }
                    "twocellanchor" | "onecellanchor" => { if let Some(a) = cur.take() { finish(a, index, budget, &mut data, &mut order, z); index += 1; } }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if order.is_empty() { None } else { Some(json!({"data": data, "order": order})) }
}

// ------------------------------------------------------------------ workbook

/// An `.xlsx` file as a workbook. Errors when the bytes aren't a readable workbook.
pub fn xlsx_to_workbook(bytes: &[u8]) -> Result<String> {
    let mut z = Zip(zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("Unable to parse spreadsheet file.")?);
    if z.0.len() > MAX_PARTS { bail!("Unable to parse spreadsheet file."); }
    let wb = z.text("xl/workbook.xml").context("Unable to parse spreadsheet file.")?;
    let wb_rels = rels(&z.text("xl/_rels/workbook.xml.rels").unwrap_or_default());
    let strings = z.text("xl/sharedStrings.xml").map(|x| shared_strings(&x)).unwrap_or_default();
    let styles = z.text("xl/styles.xml").map(|x| parse_styles(&x)).unwrap_or_default();
    let date1904 = tok(&wb).into_iter().any(|t| matches!(t, Token::Open { name, attrs, .. } if name == "workbookpr" && truthy(attr(&attrs, "date1904"))));
    let mut cx = Ctx { strings: &strings, styles: &styles, style_cache: vec![], date1904, cells: 0, text_bytes: 0, over: false };
    let mut order = vec![];
    let mut sheets = Map::new();
    let mut drawings = Map::new();
    let mut validations = Map::new();
    let mut parsed: HashMap<String, Parsed> = HashMap::new();
    let mut img_budget: u64 = 64 << 20;
    for t in tok(&wb) {
        let Token::Open { name, attrs, .. } = t else { continue };
        if name != "sheet" { continue; }
        if order.len() >= MAX_SHEETS { bail!("too many sheets"); }
        let Some(rid) = attr_ns(&attrs, "id") else { continue };
        let Some((_, target)) = wb_rels.get(rid) else { continue };
        let path = resolve("xl", target);
        let id = format!("sheet-{}", order.len() + 1);
        let hidden = attr(&attrs, "state").is_some_and(|s| s != "visible");
        let sname = attr_dec(&attrs, "name").unwrap_or_else(|| "Sheet".into());
        if !parsed.contains_key(&path) {
            let Some(xml) = z.text(&path) else { continue };
            let before = cx.cells;
            let mut p = read_sheet(&xml, &id, &sname, hidden, &mut cx);
            p.cells = cx.cells - before;
            parsed.insert(path.clone(), p);
        } else {
            // the same part listed again is copied: it counts against the cell budget each time
            cx.cells = cx.cells.saturating_add(parsed[&path].cells);
            if cx.cells > MAX_CELLS { cx.over = true; }
        }
        if cx.over { bail!("this workbook is too large to import"); }
        let p = &parsed[&path];
        let mut sheet = p.sheet.clone();
        sheet.insert("id".into(), json!(id));
        sheet.insert("name".into(), json!(sname));
        sheet.insert("hidden".into(), json!(u8::from(hidden)));
        // hyperlinks: external targets live in the sheet's relationships
        let srels = rels(&z.text(&rels_path(&path)).unwrap_or_default());
        if !p.links.is_empty() {
            if let Some(Value::Object(cells)) = sheet.get_mut("cellData") {
                for ((r, c), link) in &p.links {
                    let url = link.url.as_ref().and_then(|rid| srels.get(rid)).and_then(|(_, t)| sanitize_link(&decode_entities(t)));
                    let Some(url) = url else { continue };
                    let Some(cell) = cells.get_mut(&r.to_string()).and_then(|row| row.get_mut(c.to_string())).and_then(Value::as_object_mut) else { continue };
                    // a linked cell is its text, plus the document that carries the link
                    if cell.contains_key("f") { continue; }
                    let text = match cell.get("v") { Some(Value::String(s)) => s.clone(), Some(Value::Number(n)) => n.to_string(), Some(Value::Bool(b)) => b.to_string(), _ => continue };
                    cell.insert("v".into(), json!(text));
                    cell.insert("t".into(), json!(STRING));
                    if !text.is_empty() { cell.insert("p".into(), linked_document(&text, &url, &format!("link-{}", a1(*r, *c)))); }
                }
            }
        }
        if let Some((_, dt)) = p.drawing_rid.as_ref().and_then(|rid| srels.get(rid)) {
            let dpath = resolve(dir_of(&path), dt);
            let geom = SheetGeom {
                col_custom: sheet["columnData"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, (v["w"].as_f64()? - 5.0) / 7.0))).collect()).unwrap_or_default(),
                row_ht: sheet["rowData"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v["h"].as_f64()? * 0.75))).collect()).unwrap_or_default(),
            };
            let dims = Dims::from(&sheet);
            if let Some(d) = read_images(&mut z, &dpath, &dims, &geom, &id, &mut img_budget) { drawings.insert(id.clone(), d); }
        }
        if !p.validations.is_empty() {
            let re_id: Vec<Value> = p.validations.iter().map(|v| { let mut v = v.clone(); if let Some(o) = v.as_object_mut() { let n = o["uid"].as_str().unwrap_or("").rsplit('-').next().unwrap_or("0").to_string(); o.insert("uid".into(), json!(format!("dv-{id}-{n}"))); } v }).collect();
            validations.insert(id.clone(), Value::Array(re_id));
        }
        sheets.insert(id.clone(), Value::Object(sheet));
        order.push(id);
    }
    if order.is_empty() { bail!("Unable to parse spreadsheet file."); }
    let mut out: Value = serde_json::from_str(&workbook(order, sheets))?;
    let mut resources = vec![];
    if !drawings.is_empty() { resources.push(json!({"name": "SHEET_DRAWING_PLUGIN", "data": Value::Object(drawings).to_string()})); }
    if !validations.is_empty() { resources.push(json!({"name": "SHEET_DATA_VALIDATION_PLUGIN", "data": Value::Object(validations).to_string()})); }
    if !resources.is_empty() { out["workbook"]["resources"] = Value::Array(resources); }
    Ok(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_xlsx() -> Vec<u8> {
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            for (n, c) in [
                ("xl/workbook.xml", r#"<workbook xmlns:r="x"><sheets><sheet name="Data &amp; more" sheetId="1" r:id="rId1"/><sheet name="Hidden" sheetId="2" state="hidden" r:id="rId2"/></sheets></workbook>"#),
                ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#),
                ("xl/sharedStrings.xml", r#"<sst><si><t>Hello</t></si><si><r><t>Rich </t></r><r><t>text</t></r><rPh><t>phonetic</t></rPh></si></sst>"#),
                ("xl/styles.xml", r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="dd/mm/yyyy"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/><xf numFmtId="2"/></cellXfs></styleSheet>"#),
                ("xl/worksheets/sheet1.xml", r#"<worksheet><cols><col min="2" max="3" width="20" customWidth="1" hidden="1"/></cols><sheetData>
                    <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="b"><v>1</v></c><c r="D1" t="inlineStr"><is><t>inline</t></is></c></row>
                    <row r="3" ht="30" customHeight="1" hidden="1"><c r="A3"><v>41</v></c><c r="B3"><f>A3+1</f><v>42</v></c><c r="C3" s="1"><v>45000</v></c><c r="D3" s="2"><v>45001.5</v></c><c r="E3" s="3"><v>3.14</v></c><c r="F3" t="e"><v>#DIV/0!</v></c><c r="G3"/></row>
                    </sheetData><mergeCells><mergeCell ref="A1:B2"/></mergeCells></worksheet>"#),
                ("xl/worksheets/sheet2.xml", r#"<worksheet><sheetData><row r="1"><c r="AA1"><v>7</v></c></row></sheetData></worksheet>"#),
            ] { z.start_file(n, o).unwrap(); z.write_all(c.as_bytes()).unwrap(); }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn hostile_workbooks_stay_bounded() {
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            let sheets: String = (0..300).map(|i| format!(r#"<sheet name="S{i}" sheetId="{i}" r:id="rId1"/>"#)).collect();
            for (n, c) in [
                ("xl/workbook.xml", format!(r#"<workbook xmlns:r="x"><sheets>{sheets}</sheets></workbook>"#)),
                ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#.to_string()),
                ("xl/worksheets/sheet1.xml", r#"<worksheet><cols><col min="18446744073709551615" max="18446744073709551615" width="9"/><col min="1" max="99999999999" hidden="1"/></cols><sheetData>
                    <row r="4000000000"><c r="ZZZZZZZZZZZZZZZZZZZZ1"><v>1</v></c><c r="XFZ5"><v>2</v></c><c><v>3</v></c><c><v>4</v></c></row></sheetData></worksheet>"#.to_string()),
            ] { z.start_file(n, o).unwrap(); z.write_all(c.as_bytes()).unwrap(); }
            z.finish().unwrap();
        }
        assert!(xlsx_to_workbook(&buf.into_inner()).is_err(), "more than 256 sheets is refused");
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            for (n, c) in [
                ("xl/workbook.xml", r#"<workbook xmlns:r="x"><sheets><sheet name="A" r:id="rId1"/><sheet name="B" r:id="rId1"/></sheets></workbook>"#),
                ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#),
                ("xl/worksheets/sheet1.xml", r#"<worksheet><cols><col min="18446744073709551615" max="18446744073709551615" width="9"/><col min="1" max="99999999999" hidden="1"/></cols><sheetData>
                    <row r="4000000000"><c r="ZZZZZZZZZZZZZZZZZZZZ1"><v>1</v></c><c r="XFZ5"><v>2</v></c><c><v>3</v></c><c><v>4</v></c></row></sheetData></worksheet>"#),
            ] { z.start_file(n, o).unwrap(); z.write_all(c.as_bytes()).unwrap(); }
            z.finish().unwrap();
        }
        let w: Value = serde_json::from_str(&xlsx_to_workbook(&buf.into_inner()).unwrap()).unwrap();
        let s = &w["workbook"]["sheets"]["sheet-1"];
        assert!(s["rowCount"].as_u64().unwrap() <= MAX_ROWS as u64 && s["columnCount"].as_u64().unwrap() <= MAX_COLS as u64, "{s}");
        // cells without a reference follow each other along the row instead of overwriting one another
        let row = s["cellData"][(MAX_ROWS - 1).to_string()].as_object().unwrap();
        assert!(row.len() >= 2, "{row:?}");
        assert_eq!(w["workbook"]["sheets"]["sheet-2"]["name"], "B");
        assert_eq!(w["workbook"]["sheets"]["sheet-2"]["id"], "sheet-2");
    }

    #[test]
    fn xlsx_sheets_values_formulas_merges_and_formats() {
        let w: Value = serde_json::from_str(&xlsx_to_workbook(&make_xlsx()).unwrap()).unwrap();
        assert_eq!(w["workbook"]["sheetOrder"], json!(["sheet-1", "sheet-2"]));
        let s1 = &w["workbook"]["sheets"]["sheet-1"];
        assert_eq!(s1["name"], "Data & more");
        let c = |r: usize, k: usize| &s1["cellData"][r.to_string()][k.to_string()];
        assert_eq!(c(0, 0), &json!({"v": "Hello", "t": 1}));
        assert_eq!(c(0, 1)["v"], "Rich text", "phonetic runs are skipped");
        assert_eq!(c(0, 2), &json!({"v": true, "t": 3}));
        assert_eq!(c(0, 3)["v"], "inline");
        assert_eq!(c(2, 1), &json!({"f": "=A3+1", "v": 42, "t": 2}));
        assert_eq!(c(2, 2)["s"], json!({"n": {"pattern": "mm-dd-yy"}}));
        assert_eq!(c(2, 3)["s"], json!({"n": {"pattern": "dd/mm/yyyy"}}));
        assert_eq!(c(2, 4)["s"], json!({"n": {"pattern": "0.00"}}), "every number format is kept");
        assert_eq!(c(2, 5), &json!({"v": "#DIV/0!", "t": 4}), "errors are forced strings");
        assert!(s1["cellData"]["2"].get("6").is_none(), "empty cells are dropped");
        assert_eq!(s1["mergeData"], json!([{"startRow": 0, "endRow": 1, "startColumn": 0, "endColumn": 1}]));
        assert_eq!(s1["rowData"]["2"], json!({"h": 40, "hd": 1}));
        assert_eq!(s1["columnData"]["2"], json!({"w": 145, "hd": 1}));
        assert_eq!(s1["columnData"]["1"], json!({"w": 145, "hd": 1}));
        assert_eq!(s1["hidden"], 0);
        let s2 = &w["workbook"]["sheets"]["sheet-2"];
        assert_eq!(s2["hidden"], 1);
        assert_eq!(s2["cellData"]["0"]["26"]["v"], 7);
        assert!(s2["columnCount"].as_u64().unwrap() >= 27);
        assert!(xlsx_to_workbook(b"not a zip").is_err());
    }

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            for (n, c) in files { z.start_file(*n, o).unwrap(); z.write_all(c.as_bytes()).unwrap(); }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    const WB: &str = r#"<workbook xmlns:r="x"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    const WB_RELS: &str = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;

    #[test]
    fn styles_fonts_fills_borders_alignment_and_theme_colors() {
        let styles = r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/></numFmts>
            <fonts><font><sz val="11"/><color theme="1"/><name val="Calibri"/><scheme val="minor"/></font><font><b/><i/><u/><strike/><sz val="14"/><color rgb="FFFF0000"/><name val="Arial"/></font></fonts>
            <fills><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor theme="4" tint="0.4"/></patternFill></fill></fills>
            <borders><border><left/><right/><top/><bottom/></border><border><left style="thin"><color rgb="FF00FF00"/></left><right style="double"/><top style="dashed"/><bottom style="thick"><color rgb="FF0000FF"/></bottom></border></borders>
            <cellXfs><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="164" fontId="1" fillId="2" borderId="1"><alignment horizontal="center" vertical="center" wrapText="1" textRotation="135"/></xf><xf numFmtId="0" fontId="0" fillId="0" borderId="0"><alignment textRotation="255"/></xf></cellXfs></styleSheet>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>1234.5</v></c><c r="B1" s="2"><v>1</v></c><c r="C1" s="0"><v>2</v></c><c r="D1" s="2"/></row></sheetData></worksheet>"#;
        let w: Value = serde_json::from_str(&xlsx_to_workbook(&zip_of(&[("xl/workbook.xml", WB), ("xl/_rels/workbook.xml.rels", WB_RELS), ("xl/styles.xml", styles), ("xl/worksheets/sheet1.xml", sheet)])).unwrap()).unwrap();
        let c = &w["workbook"]["sheets"]["sheet-1"]["cellData"]["0"];
        assert_eq!(c["0"]["s"], json!({"ff": "Arial", "fs": 14, "bl": 1, "it": 1, "ul": {"s": 1}, "st": {"s": 1}, "cl": {"rgb": "#FF0000"}, "bg": {"rgb": "#8FAADC"},
            "ht": 2, "vt": 2, "tb": 3, "tr": {"a": -45}, "bd": {"t": {"s": 4}, "r": {"s": 7}, "b": {"s": 13, "cl": {"rgb": "#0000FF"}}, "l": {"s": 1, "cl": {"rgb": "#00FF00"}}},
            "n": {"pattern": "\"$\"#,##0.00"}}));
        assert_eq!(c["1"]["s"], json!({"tr": {"v": 1}}), "the default theme font is not carried over");
        assert!(c["2"].get("s").is_none());
        assert_eq!(c["3"], json!({"s": {"tr": {"v": 1}}}), "a styled empty cell is kept");
    }

    #[test]
    fn shared_formulas_slide_and_validation_hyperlinks_and_images_come_across() {
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1"><f t="shared" ref="A1:A3" si="0">B1*$C1+SUM(D1:E1)</f><v>1</v></c><c r="A5" t="inlineStr"><is><t>Link</t></is></c><c r="A6" t="inlineStr"><is><t>Bad</t></is></c></row>
            <row r="3"><c r="A3"><f t="shared" si="0"/><v>3</v></c></row></sheetData>
            <dataValidations count="3"><dataValidation type="list" sqref="B1"><formula1>"a,b"</formula1></dataValidation>
            <dataValidation type="whole" sqref="B2:B3 C2:C3"><formula1>1</formula1><formula2>9</formula2></dataValidation>
            <dataValidation type="list" sqref="B9"/></dataValidations>
            <hyperlinks><hyperlink ref="A5" r:id="rId1"/><hyperlink ref="A6" r:id="rId2"/></hyperlinks><drawing r:id="rId3"/></worksheet>"#;
        let srels = r#"<Relationships><Relationship Id="rId1" Target="https://example.com/x"/><Relationship Id="rId2" Target="javascript:alert(1)"/><Relationship Id="rId3" Target="../drawings/drawing1.xml"/></Relationships>"#;
        let drawing = r#"<xdr:wsDr><xdr:oneCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:ext cx="381000" cy="285750"/>
            <xdr:pic><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:oneCellAnchor></xdr:wsDr>"#;
        let drels = r#"<Relationships><Relationship Id="rId1" Target="../media/p.png"/></Relationships>"#;
        let w: Value = serde_json::from_str(&xlsx_to_workbook(&zip_of(&[("xl/workbook.xml", WB), ("xl/_rels/workbook.xml.rels", WB_RELS), ("xl/worksheets/sheet1.xml", sheet),
            ("xl/worksheets/_rels/sheet1.xml.rels", srels), ("xl/drawings/drawing1.xml", drawing), ("xl/drawings/_rels/drawing1.xml.rels", drels), ("xl/media/p.png", "PNG")])).unwrap()).unwrap();
        let s = &w["workbook"]["sheets"]["sheet-1"];
        assert_eq!(s["cellData"]["0"]["0"]["f"], "=B1*$C1+SUM(D1:E1)");
        assert_eq!(s["cellData"]["2"]["0"]["f"], "=B3*$C3+SUM(D3:E3)", "relative references slide, absolute ones and functions stay");
        // the link document
        let (ok, bad) = (&s["cellData"]["4"]["0"], &s["cellData"]["5"]["0"]);
        assert_eq!(ok["p"]["body"]["customRanges"][0]["properties"]["url"], "https://example.com/x");
        assert_eq!(ok["p"]["body"]["customRanges"][0]["endIndex"], 3);
        assert_eq!((bad["v"].as_str(), bad.get("p")), (Some("Bad"), None), "javascript: links are dropped");
        let res = |name: &str| -> Value { serde_json::from_str(w["workbook"]["resources"].as_array().unwrap().iter().find(|r| r["name"] == name).unwrap()["data"].as_str().unwrap()).unwrap() };
        let dv = res("SHEET_DATA_VALIDATION_PLUGIN");
        assert_eq!(dv["sheet-1"][0], json!({"uid": "dv-sheet-1-0", "type": "list", "ranges": [{"startRow": 0, "endRow": 0, "startColumn": 1, "endColumn": 1}], "formula1": "[\"a\",\"b\"]"}));
        assert_eq!(dv["sheet-1"][1]["ranges"], json!([{"startRow": 1, "endRow": 2, "startColumn": 1, "endColumn": 2}]), "adjacent cells merge into one block");
        assert_eq!(dv["sheet-1"][1]["operator"], "between");
        assert_eq!(dv["sheet-1"].as_array().unwrap().len(), 2, "a list without options is dropped");
        let dr = res("SHEET_DRAWING_PLUGIN");
        let img = &dr["sheet-1"]["data"]["image-sheet-1-0"];
        assert_eq!(img["source"], "data:image/png;base64,UE5H");
        assert_eq!(img["transform"]["width"], 40);
        assert_eq!(img["sheetTransform"]["from"], json!({"row": 2, "rowOffset": 0, "column": 1, "columnOffset": 0}));
        assert_eq!(dr["sheet-1"]["order"], json!(["image-sheet-1-0"]));
    }

    #[test]
    fn review_regressions_prefixes_phonetics_dates_links_and_limits() {
        let wb = r#"<workbook xmlns:r="x"><workbookPr date1904="1"/><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let styles = r#"<styleSheet><fonts><font><b val="0"/><u val="none"/><i/></font></fonts><cellXfs><xf numFmtId="0" fontId="0"/><xf numFmtId="14" fontId="0"/></cellXfs></styleSheet>"#;
        let long = "x".repeat(40_000);
        let sheet = format!(r#"<x:worksheet><x:cols><x:col min="1" max="4000" width="9"/></x:cols><x:sheetData><x:row r="1"><x:c r="A1" t="inlineStr"><x:is><x:t>a</x:t><x:rPh><x:t>PH</x:t></x:rPh></x:is></x:c>
            <x:c r="B1" s="1"><x:v>40000</x:v></x:c><x:c r="C1"><x:v>42</x:v></x:c><x:c r="D1" t="inlineStr"><x:is><x:t>{long}</x:t></x:is></x:c>
            <x:c r="E1" s="0"><x:f>A9223372036854775807+B1</x:f></x:c><x:c r="F1" t="str"><x:v><![CDATA[a&amp;b]]></x:v></x:c></x:row></x:sheetData>
            <x:hyperlinks><x:hyperlink ref="C1" location="S!A1"/></x:hyperlinks></x:worksheet>"#);
        let w: Value = serde_json::from_str(&xlsx_to_workbook(&zip_of(&[("xl/workbook.xml", wb), ("xl/_rels/workbook.xml.rels", WB_RELS), ("xl/styles.xml", styles), ("xl/worksheets/sheet1.xml", &sheet)])).unwrap()).unwrap();
        let s = &w["workbook"]["sheets"]["sheet-1"];
        let c = |k: usize| &s["cellData"]["0"][k.to_string()];
        assert_eq!(c(0)["v"], "a", "phonetic runs of inline strings are skipped, prefixed elements are read");
        assert_eq!(c(1)["v"], 41462, "1904 workbooks are shifted to the 1900 date system");
        assert_eq!(c(2), &json!({"v": 42, "t": 2}), "a link without a target leaves the number alone");
        assert_eq!(c(3)["v"].as_str().unwrap().chars().count(), 32_767, "Excel's cell text limit");
        assert_eq!(c(4)["f"], "=A9223372036854775807+B1");
        assert_eq!(c(5)["v"], "a&amp;b", "CDATA is literal");
        assert_eq!(s["columnData"]["3999"]["w"], 68, "a wide <col> covers every column it names");
        assert_eq!(c(1)["s"], json!({"it": 1, "n": {"pattern": "mm-dd-yy"}}), "val=0 and u=none switch the property off");
        // sliding a shared formula off the sheet leaves the reference alone instead of overflowing
        assert_eq!(slide_formula("A1+$B$2+ZZ1", 5, 1), "B6+$B$2+ZZ1".replace("ZZ1", "AAA6"));
        assert_eq!(slide_formula("A9223372036854775807", 5, 0), "A9223372036854775807");
        assert_eq!(slide_formula("A1", -5, 0), "A1");
    }
}
