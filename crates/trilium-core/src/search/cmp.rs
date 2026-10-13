//! Comparators (`build_comparator.ts`).
use super::text::{auto_max_edit_distance, fuzzy_match_word, normalize, strip_word_punctuation, tokenize_normalized};
use regex::Regex;

#[derive(Debug, Clone)]
pub struct Cmp {
    op: String,
    /// compared value, lowercased
    val: String,
    norm: String,
    num: Option<f64>,
    regex: Option<Regex>,
}

pub const OPERATORS: &[&str] = &["=", "!=", "*=*", "*=", "=*", ">", ">=", "<", "<=", "%=", "~=", "~*"];

/// JS `parseFloat`: leading numeric prefix, NaN otherwise.
pub fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let mut end = 0;
    let b = t.as_bytes();
    if end < b.len() && (b[end] == b'+' || b[end] == b'-') { end += 1; }
    let digits_start = end;
    while end < b.len() && b[end].is_ascii_digit() { end += 1; }
    if end < b.len() && b[end] == b'.' { end += 1; while end < b.len() && b[end].is_ascii_digit() { end += 1; } }
    if end == digits_start || (end == digits_start + 1 && b[digits_start] == b'.') { return None; }
    t[..end].parse().ok()
}

impl Cmp {
    /// `None` for unknown operators (the caller reports the error).
    pub fn build(op: &str, compared: &str) -> Option<Self> {
        let val = compared.to_lowercase();
        let known = OPERATORS.contains(&op) || op == "word=";
        if !known { return None; }
        let regex = if op == "%=" { Regex::new(&val).ok() } else { None };
        let num = if matches!(op, ">" | ">=" | "<" | "<=") { val.trim().parse::<f64>().ok().or_else(|| if val.trim().is_empty() { Some(0.0) } else { None }) } else { None };
        let norm = normalize(&val);
        Some(Self { op: op.into(), val, norm, num, regex })
    }

    /// Normalized compared value, for prefilters that must never reject a possible match.
    pub fn norm(&self) -> &str { &self.norm }
    pub fn op(&self) -> &str { &self.op }

    pub fn matches(&self, v: &str) -> bool {
        if let Some(n) = self.num {
            let x = parse_float(v);
            return match self.op.as_str() {
                ">" => x.is_some_and(|x| x > n), ">=" => x.is_some_and(|x| x >= n),
                "<" => x.is_some_and(|x| x < n), _ => x.is_some_and(|x| x <= n),
            };
        }
        match self.op.as_str() {
            "=" => normalize(v) == self.norm,
            "!=" => normalize(v) != self.norm,
            "word=" => {
                if v.is_empty() { return false; }
                let nv = normalize(v);
                if self.norm.contains(' ') { return nv.contains(&self.norm); }
                let w = strip_word_punctuation(&self.norm);
                tokenize_normalized(&nv).iter().any(|x| x == w)
            }
            ">" => v > self.val.as_str(), ">=" => v >= self.val.as_str(),
            "<" => v < self.val.as_str(), "<=" => v <= self.val.as_str(),
            "*=" => !v.is_empty() && v.ends_with(&self.val),
            "=*" => !v.is_empty() && v.starts_with(&self.val),
            "*=*" => !v.is_empty() && v.contains(&self.val),
            "%=" => !v.is_empty() && self.regex.as_ref().is_some_and(|r| r.is_match(v)),
            "~=" | "~*" => {
                if v.is_empty() || self.val.is_empty() { return false; }
                if self.val.chars().count() < 3 { return v.contains(&self.val); }
                let nv = normalize(v);
                if nv.contains(&self.norm) { return true; }
                if self.op == "~=" { tokenize_normalized(&nv).iter().any(|w| fuzzy_match_word(&self.norm, w)) } else { fuzzy_match_word(&self.norm, &nv) }
            }
            _ => false,
        }
    }
}

#[allow(dead_code)]
fn _keep(_: usize) { let _ = auto_max_edit_distance; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn string_numeric_and_regex() {
        assert!(Cmp::build("=", "Vienna").unwrap().matches("vienna"));
        assert!(!Cmp::build("=", "vienna").unwrap().matches("vienna austria"));
        assert!(Cmp::build("*=*", "ien").unwrap().matches("vienna"));
        assert!(Cmp::build("=*", "vi").unwrap().matches("vienna") && Cmp::build("*=", "na").unwrap().matches("vienna"));
        assert!(Cmp::build(">=", "2020").unwrap().matches("2021") && !Cmp::build(">=", "2020").unwrap().matches("2019"));
        assert!(!Cmp::build(">", "5").unwrap().matches("abc"));
        assert!(Cmp::build(">", "2020-01-01").unwrap().matches("2021-03-04"), "non-numeric compared value compares as text");
        assert!(Cmp::build("%=", "^v.*a$").unwrap().matches("vienna"));
        assert!(Cmp::build("word=", "books").unwrap().matches("(Books) of Fire"));
        assert!(Cmp::build("~=", "combinef").unwrap().matches("all combined here"));
        assert!(Cmp::build("??", "x").is_none());
        assert_eq!(parse_float("12px"), Some(12.0));
        assert_eq!(parse_float("x"), None);
    }
}
