//! CSV and XLSX → the Univer workbook JSON a Trilium spreadsheet note stores (`PersistedData`), after Trilium's
//! `parse_from_csv.ts` / `parse_from_xlsx.ts`. CSV becomes one unstyled sheet with conservatively typed values (the
//! XLSX reader lives in `xlsx.rs`).
use serde_json::{Map, Value, json};

pub(crate) const MAX_ROWS: usize = 1_048_576;
pub(crate) const MAX_COLS: usize = 16_384;
pub(crate) const MAX_SHEETS: usize = 256;
pub(crate) const DEFAULT_ROWS: usize = 1000;
pub(crate) const DEFAULT_COLS: usize = 20;
pub(crate) const STRING: i64 = 1;
pub(crate) const NUMBER: i64 = 2;
pub(crate) const BOOLEAN: i64 = 3;

/// RFC 4180 reader: quoted fields, doubled quotes, embedded newlines; CRLF/LF/CR; leading BOM stripped.
pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (mut rows, mut row, mut field) = (vec![], vec![], String::new());
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => { chars.next(); field.push('"'); }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => { row.push(std::mem::take(&mut field)); rows.push(std::mem::take(&mut row)); }
            '\r' => { row.push(std::mem::take(&mut field)); rows.push(std::mem::take(&mut row)); if chars.peek() == Some(&'\n') { chars.next(); } }
            _ => field.push(c),
        }
    }
    row.push(field);
    rows.push(row);
    if rows.last().is_some_and(|l| l.len() == 1 && l[0].is_empty()) { rows.pop(); }
    rows
}

pub(crate) fn workbook(sheet_order: Vec<String>, sheets: Map<String, Value>) -> String {
    json!({"version": 1, "workbook": {"sheetOrder": sheet_order, "styles": {}, "sheets": sheets}}).to_string()
}

/// A JSON number that prints like JavaScript's (`3`, not `3.0`).
pub(crate) fn num(n: f64) -> Value { if n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { json!(n) } }

pub(crate) fn put_cell(cells: &mut Map<String, Value>, r: usize, c: usize, cell: Value) {
    let row = cells.entry(r.to_string()).or_insert_with(|| Value::Object(Map::new()));
    row.as_object_mut().expect("row map").insert(c.to_string(), cell);
}

/// A CSV file as a single-sheet workbook.
pub fn csv_to_workbook(text: &str) -> String {
    let rows = parse_csv(text);
    let mut cells = Map::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if v.is_empty() { continue; }
            let cell = match v.as_str() {
                "TRUE" => json!({"v": true, "t": BOOLEAN}),
                "FALSE" => json!({"v": false, "t": BOOLEAN}),
                _ => match v.parse::<f64>() {
                    // a number only when it reads back exactly (so 007, 1,000, +1, 1e3 stay text)
                    Ok(n) if n.is_finite() && js_number_string(n) == *v => json!({"v": num(n), "t": NUMBER}),
                    _ => json!({"v": v, "t": STRING}),
                },
            };
            put_cell(&mut cells, r, c, cell);
        }
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut sheets = Map::new();
    sheets.insert("sheet-0".into(), json!({"id": "sheet-0", "name": "Sheet1", "cellData": cells, "rowCount": DEFAULT_ROWS.max(rows.len()), "columnCount": DEFAULT_COLS.max(cols)}));
    workbook(vec!["sheet-0".into()], sheets)
}

/// `String(Number)` as JavaScript prints it, for the cases that can round-trip a CSV field.
fn js_number_string(n: f64) -> String {
    if n == 0.0 { return "0".into(); }
    if n.fract() == 0.0 && n.abs() < 1e21 { return format!("{}", n as i128); }
    let s = format!("{n}");
    if s.contains('e') { String::new() } else { s }
}

pub use crate::xlsx::xlsx_to_workbook;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_values_are_typed_conservatively() {
        let w: Value = serde_json::from_str(&csv_to_workbook("\u{feff}name,qty,flag,code,price\n\"Smith, J\",3,TRUE,007,1,000\r\n\"multi\nline\",2.5,FALSE,+1,1e3\n")).unwrap();
        let s = &w["workbook"]["sheets"]["sheet-0"];
        let c = |r: usize, k: usize| &s["cellData"][r.to_string()][k.to_string()];
        assert_eq!(c(0, 0), &json!({"v": "name", "t": 1}));
        assert_eq!(c(1, 0), &json!({"v": "Smith, J", "t": 1}));
        assert_eq!(c(1, 1), &json!({"v": 3, "t": 2}));
        assert_eq!(c(1, 2), &json!({"v": true, "t": 3}));
        assert_eq!(c(1, 3), &json!({"v": "007", "t": 1}), "leading zeros stay text");
        assert_eq!(c(2, 0)["v"], "multi\nline");
        assert_eq!(c(2, 1), &json!({"v": 2.5, "t": 2}));
        assert_eq!(c(2, 3)["t"], 1);
        assert_eq!(c(2, 4)["v"], "1e3");
        assert_eq!(w["workbook"]["sheetOrder"], json!(["sheet-0"]));
        assert!(s["rowCount"].as_u64().unwrap() >= 1000 && s["columnCount"].as_u64().unwrap() >= 20);
        assert_eq!(parse_csv("a,b\n").len(), 1);
        assert_eq!(parse_csv("a,\"\"\"q\"\"\"\r\nz").len(), 2);
    }
}
