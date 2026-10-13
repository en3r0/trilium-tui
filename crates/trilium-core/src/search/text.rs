//! Text helpers shared by search: normalization, tokenization, HTML stripping.
//! Ported from `search/utils/text_utils.ts` and `utils/index.ts::normalize`.
use unicode_normalization::UnicodeNormalization;

/// Lowercase and strip diacritics (`removeDiacritic(str).toLowerCase()`).
pub fn normalize(s: &str) -> String {
    s.nfd().filter(|c| !('\u{0300}'..='\u{036f}').contains(c)).collect::<String>().to_lowercase()
}

fn is_boundary_punct(c: char) -> bool {
    // Unicode categories Pi, Pf, Ps, Pe, Po, Pd; connector punctuation (`_`) and symbols
    // (`+`, `=`, `$`, ...) are deliberately kept, so `c++` and `_private` survive.
    match c {
        '!' | '"' | '#' | '%' | '&' | '\'' | '(' | ')' | '*' | ',' | '-' | '.' | '/' | ':' | ';' | '?' | '@' | '[' | '\\' | ']' | '{' | '}' => true,
        '\u{a1}' | '\u{a7}' | '\u{ab}' | '\u{b6}' | '\u{b7}' | '\u{bb}' | '\u{bf}' => true,
        '\u{2010}'..='\u{2027}' | '\u{2030}'..='\u{205e}' | '\u{3001}'..='\u{3003}' | '\u{3008}'..='\u{3011}' => true,
        _ => false,
    }
}

pub fn strip_word_punctuation(w: &str) -> &str {
    w.trim_matches(is_boundary_punct)
}

/// Words of already-normalized text, punctuation-stripped, empty words dropped.
pub fn tokenize_normalized(n: &str) -> Vec<String> {
    n.split_whitespace().map(strip_word_punctuation).filter(|w| !w.is_empty()).map(String::from).collect()
}

pub fn tokenize(text: &str) -> Vec<String> { tokenize_normalized(&normalize(text)) }

pub fn words_contain_phrase(words: &[String], phrase: &[String]) -> bool {
    !phrase.is_empty() && phrase.len() <= words.len() && words.windows(phrase.len()).any(|w| w == phrase)
}

/// `getAutoMaxEditDistance`
pub fn auto_max_edit_distance(len: usize) -> usize {
    match len { 0..=3 => 0, 4..=6 => 1, _ => 2 }
}

pub fn edit_distance_within(a: &str, b: &str, max: usize) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max { return false; }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        if cur.iter().all(|&x| x > max) { return false; }
        prev = cur;
    }
    prev[b.len()] <= max
}

/// Typo-tolerant word match: some word of `text` within the token's auto edit distance
/// (a word that merely contains the token is a substring match, not a typo).
pub fn fuzzy_match_word(token: &str, text: &str) -> bool {
    let max = auto_max_edit_distance(token.chars().count());
    if max == 0 { return false; }
    text.split_whitespace().any(|w| !(w.len() > token.len() && w.contains(token)) && edit_distance_within(token, w, max))
}

pub fn unescape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';').filter(|&e| e <= 10) {
            Some(e) => {
                let ent = &rest[1..e];
                let rep = match ent {
                    "amp" => Some('&'), "lt" => Some('<'), "gt" => Some('>'), "quot" => Some('"'), "apos" => Some('\''), "nbsp" => Some(' '),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                    _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                    _ => None,
                };
                match rep { Some(c) => { out.push(c); rest = &rest[e + 1..]; } None => { out.push('&'); rest = &rest[1..]; } }
            }
            None => { out.push('&'); rest = &rest[1..]; }
        }
    }
    out.push_str(rest);
    out
}

/// `stripTags` of the content preprocessor: tags that imply text separation become a space, inline
/// formatting tags (b, strong, em, i, span, ...) vanish without one so a formatted word stays whole,
/// opening `<a ...>` tags are kept (URLs stay searchable) and `</a>` is removed.
pub fn strip_tags(html: &str) -> String {
    const INLINE: &[&str] = &["b", "strong", "em", "i", "span", "big", "small", "font", "sub", "sup"];
    let mut out = String::with_capacity(html.len());
    let mut chars = html.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '<' { out.push(c); continue; }
        // find the end of the tag
        let Some(rel) = html[i..].find('>') else { out.push_str(&html[i..]); break; };
        let tag = &html[i..i + rel + 1];
        let inner = tag[1..tag.len() - 1].trim_start_matches('/').trim_start();
        let name: String = inner.chars().take_while(|ch| ch.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        let closing = tag[1..].starts_with('/');
        if name.is_empty() { out.push_str(tag); }                       // not a tag (e.g. "a < b >")
        else if name == "a" { if !closing { out.push_str(tag); } }       // keep opening anchors
        else if INLINE.contains(&name.as_str()) { /* dropped without a space */ }
        else { out.push(' '); }
        // skip past the tag
        while chars.peek().is_some_and(|(j, _)| *j < i + rel + 1) { chars.next(); }
    }
    out
}

fn canvas_text(content: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else { return String::new() };
    let Some(els) = v.get("elements").and_then(|e| e.as_array()) else { return String::new() };
    let texts: Vec<&str> = els.iter().filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|e| e.get("text").and_then(|t| t.as_str())).filter(|t| !t.is_empty()).collect();
    normalize(&texts.join(" "))
}

fn mindmap_text(content: &str) -> String {
    fn collect(n: &serde_json::Value, out: &mut Vec<String>) {
        out.push(n.get("topic").and_then(|t| t.as_str()).unwrap_or("undefined").to_string());
        if let Some(ch) = n.get("children").and_then(|c| c.as_array()) { for c in ch { collect(c, out); } }
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else { return String::new() };
    let mut out = vec![];
    if let Some(root) = v.get("nodedata") { collect(root, &mut out); }
    normalize(&out.join(", "))
}

/// `preprocessContent` for the content types we search (link-title and icon injection are not done).
pub fn preprocess_content(raw: &str, ntype: &str, mime: &str, raw_mode: bool) -> String {
    let mut content = normalize(raw);
    if ntype == "text" && mime == "text/html" {
        if !raw_mode { content = unescape_html(&strip_tags(&content)); }
        content = content.replace("&nbsp;", " ");
    } else if ntype == "mindMap" && mime == "application/json" {
        content = mindmap_text(&content); // upstream parses the already lower-cased JSON (so the key is "nodedata")
    } else if ntype == "canvas" && mime == "application/json" {
        content = canvas_text(&content);
    }
    content.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_and_tokens() {
        assert_eq!(normalize("Ktorý Čaj"), "ktory caj");
        assert_eq!(tokenize("(Sync), c++ \"Books\" _x"), ["sync", "c++", "books", "_x"]);
        assert!(words_contain_phrase(&tokenize("a big red fox"), &tokenize("big red")));
        assert!(!words_contain_phrase(&tokenize("a big red fox"), &tokenize("red big")));
    }
    #[test]
    fn html_to_text_matches_striptags() {
        assert_eq!(preprocess_content("<p>Caf&eacute; A</p><p>B &amp; C&nbsp;D</p>", "text", "text/html", false), "caf&eacute; a  b & c d");
        assert_eq!(preprocess_content("<p>one <b>bo</b>ld <a href=\"http://x.y/z\">link</a></p>", "text", "text/html", false), "one bold <a href=\"http://x.y/z\">link".replace("<a href=\"http://x.y/z\">", "<a href=\"http://x.y/z\">"));
        assert_eq!(preprocess_content(r#"{"elements":[{"type":"text","text":"Hello"},{"type":"rect"},{"type":"text","text":"World"}]}"#, "canvas", "application/json", false), "hello world");
        assert_eq!(preprocess_content(r#"{"nodeData":{"topic":"Root","children":[{"topic":"Kid"}]}}"#, "mindMap", "application/json", false), "root, kid");
        assert_eq!(preprocess_content("<b>x</b>", "text", "text/html", true), "<b>x</b>");
        assert_eq!(unescape_html("&#65;&#x42;&lt;&bogus;"), "AB<&bogus;");
    }
    #[test]
    fn fuzzy() {
        assert!(fuzzy_match_word("combinef", "all combined here"));
        assert!(!fuzzy_match_word("sync", "async code"), "substring is not a typo");
        assert!(!fuzzy_match_word("abc", "abd"), "3-letter tokens never fuzzy-match");
    }
}
