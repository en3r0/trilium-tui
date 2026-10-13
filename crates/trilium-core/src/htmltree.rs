//! A small, forgiving HTML/XML tokenizer and tree, used by the OPML/ENEX importers and the static-site exporter.
//! It never fails: whatever it cannot make sense of is kept as text or dropped. Text stays entity-encoded
//! (`Node::Text` holds the raw source), so a parse/serialise round trip does not alter the characters.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Open { name: String, attrs: Vec<(String, Option<String>)>, self_close: bool },
    Close(String),
    /// Raw (still entity-encoded) text.
    Text(String),
    CData(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Raw, entity-encoded text.
    Text(String),
    El(El),
}

#[derive(Debug, Clone, PartialEq)]
pub struct El {
    pub name: String,
    /// Names are lower-cased; values are kept as written (entity-encoded), so they round-trip exactly.
    pub attrs: Vec<(String, Option<String>)>,
    pub kids: Vec<Node>,
    /// Written as `<x ... />` with nothing inside (kept so `<en-media/>` survives a round trip).
    pub self_close: bool,
}

const MAX_DEPTH: usize = 256;

const VOID: &[&str] = &["br", "hr", "img", "input", "meta", "link", "col", "source", "area", "base", "wbr", "embed", "param", "track"];

pub fn is_void(name: &str) -> bool { VOID.contains(&name) }

pub fn escape_text(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;") }
pub fn escape_attr(s: &str) -> String { escape_text(s).replace('"', "&quot;") }

fn attr_out(raw: &str) -> String { raw.replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;") }

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') { return s.to_string(); }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // numeric references decode even without the closing ';' (as browsers do), digits taken greedily
        if let Some(num) = rest.strip_prefix("&#") {
            let (hex, digits) = match num.strip_prefix(['x', 'X']) { Some(h) => (true, h), None => (false, num) };
            let len = digits.bytes().take_while(|b| if hex { b.is_ascii_hexdigit() } else { b.is_ascii_digit() }).count();
            if len > 0 {
                // every digit counts (a browser reads them all), saturating; NUL, surrogates and values past U+10FFFF become U+FFFD
                let radix = if hex { 16 } else { 10 };
                let mut v: u32 = 0;
                for b in digits[..len].bytes() {
                    v = v.saturating_mul(radix).saturating_add((b as char).to_digit(radix).unwrap_or(0));
                    if v > 0x10FFFF { v = 0x110000; }
                }
                let c = char::from_u32(v).filter(|c| *c != '\0').unwrap_or('\u{FFFD}');
                let consumed = 2 + usize::from(hex) + len + usize::from(digits[len..].starts_with(';'));
                out.push(c);
                rest = &rest[consumed..];
                continue;
            }
            out.push('&');
            rest = &rest[1..];
            continue;
        }
        let end = rest.char_indices().take(12).find(|(_, c)| *c == ';').map(|(i, _)| i);
        let decoded = end.and_then(|e| {
            let ch = match &rest[1..e] {
                "amp" => Some('&'), "lt" => Some('<'), "gt" => Some('>'), "quot" => Some('"'), "apos" => Some('\''), "nbsp" => Some('\u{a0}'),
                "colon" => Some(':'), "Tab" => Some('\t'), "NewLine" => Some('\n'),
                _ => None,
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => { out.push(c); rest = &rest[e + 1..]; }
            None => { out.push('&'); rest = &rest[1..]; }
        }
    }
    out.push_str(rest);
    out
}

fn is_name_start(c: char) -> bool { c.is_ascii_alphabetic() || c == '_' }

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

/// Parse a start tag that begins at `rest[0] == '<'`, following the HTML tokenizer rules for where it ends: quotes only
/// matter right after `=`. Returns (bytes consumed incl. `>`, lower-cased name, attributes with raw values, self-closing).
/// `None` when the tag is not terminated (within `limit` characters) or has an unusable name.
fn parse_tag(rest: &str, limit: usize) -> Option<(usize, String, Vec<(String, Option<String>)>, bool)> {
    let bytes = rest.as_bytes();
    let mut i = 1;
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' && bytes[i] != b'>' { i += 1; }
    let name = rest[name_start..i].to_ascii_lowercase();
    if !valid_name(&name) { return None; }
    let mut attrs = vec![];
    let mut self_close = false;
    let end_budget = limit.min(rest.len());
    loop {
        // between attributes
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            self_close = bytes[i] == b'/';
            i += 1;
        }
        if i >= end_budget { return None; }
        if bytes[i] == b'>' { return Some((i + 1, name, attrs, self_close)); }
        self_close = false;
        let n0 = i;
        // a name may start with '=' (per spec) but otherwise ends at whitespace, '/', '>' or '='
        i += 1;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'/' | b'>' | b'=') { i += 1; }
        let aname = rest[n0..i].to_ascii_lowercase();
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
        let mut value = None;
        if j < bytes.len() && bytes[j] == b'=' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                let q = bytes[j];
                let vs = j + 1;
                let ve = rest[vs..].bytes().position(|c| c == q).map(|p| vs + p)?;
                if ve > end_budget { return None; }
                value = Some(rest[vs..ve].to_string());
                j = ve + 1;
            } else {
                let vs = j;
                while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' { j += 1; }
                value = Some(rest[vs..j].to_string());
            }
            i = j;
        }
        // attribute names that could not have been written as plain names are dropped, never emitted
        if valid_name(&aname) { attrs.push((aname, value)); }
    }
}

fn find_close_tag(src: &str, from: usize, name: &str) -> usize {
    let mut at = from;
    while let Some(p) = src[at..].find("</") {
        let start = at + p;
        let cand = src.get(start + 2..start + 2 + name.len());
        if cand.is_some_and(|c| c.eq_ignore_ascii_case(name)) { return start; }
        at = start + 2;
    }
    src.len()
}

/// Split markup into tokens. `html` makes `<script>`/`<style>` contents raw text.
pub fn tokenize(src: &str, html: bool) -> Vec<Token> {
    let mut out = vec![];
    let mut i = 0;
    let b = src.as_bytes();
    let text_to = |from: usize, to: usize, out: &mut Vec<Token>| { if to > from { out.push(Token::Text(src[from..to].replace('<', "&lt;"))); } };
    let mut failed = false; // a tag scan already ran off the end: later scans are bounded so hostile input stays linear-ish
    let mut text_start = 0;
    // position of the last '>': past it no tag of any kind can close, so scans stay linear on hostile input
    let last_gt = src.rfind('>').unwrap_or(0);
    while i < b.len() {
        if b[i] != b'<' { i += 1; continue; }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            text_to(text_start, i, &mut out);
            i = rest.find("-->").map_or(b.len(), |e| i + e + 3);
        } else if rest.starts_with("<![CDATA[") {
            text_to(text_start, i, &mut out);
            let end = rest.find("]]>");
            let body = &rest[9..end.unwrap_or(rest.len())];
            out.push(Token::CData(body.to_string()));
            i = end.map_or(b.len(), |e| i + e + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            text_to(text_start, i, &mut out);
            if rest.starts_with("<?") { i = rest.find("?>").map_or(b.len(), |e| i + e + 2); }
            else {
                // a DOCTYPE may carry an internal subset in [...]
                let mut depth = 0i32;
                let mut end = rest.len();
                for (k, c) in rest.char_indices() {
                    match c { '[' => depth += 1, ']' => depth -= 1, '>' if depth <= 0 => { end = k + 1; break; } _ => {} }
                }
                i += end;
            }
        } else if rest.starts_with("</") && rest[2..].chars().next().is_some_and(is_name_start) {
            if i > last_gt { i += 1; continue; }
            let Some(end) = rest.find('>') else { i += 1; continue };
            text_to(text_start, i, &mut out);
            out.push(Token::Close(rest[2..end].trim().to_ascii_lowercase()));
            i += end + 1;
        } else if rest[1..].chars().next().is_some_and(is_name_start) {
            if i > last_gt { i += 1; continue; }
            let limit = if failed { 512 } else { usize::MAX };
            let Some((used, name, attrs, self_close)) = parse_tag(rest, limit) else { failed = true; i += 1; continue };
            text_to(text_start, i, &mut out);
            let raw_text = html && !self_close && (name == "script" || name == "style");
            out.push(Token::Open { name: name.clone(), attrs, self_close });
            i += used;
            if raw_text {
                let close = find_close_tag(src, i, &name);
                text_to(i, close, &mut out);
                i = close;
            }
        } else {
            i += 1;
            continue;
        }
        text_start = i;
    }
    text_to(text_start, b.len().min(src.len()), &mut out);
    out
}

/// Build a tree; unmatched close tags are ignored and unclosed elements are closed at the end.
pub fn parse(src: &str) -> Vec<Node> {
    let mut stack: Vec<El> = vec![El { name: String::new(), attrs: vec![], kids: vec![], self_close: false }];
    fn close_top(stack: &mut Vec<El>) {
        let el = stack.pop().unwrap();
        stack.last_mut().unwrap().kids.push(Node::El(el));
    }
    for t in tokenize(src, true) {
        match t {
            Token::Text(s) => stack.last_mut().unwrap().kids.push(Node::Text(s)),
            Token::CData(s) => stack.last_mut().unwrap().kids.push(Node::Text(escape_text(&s))),
            Token::Open { name, attrs, self_close } => {
                let el = El { name: name.clone(), attrs, kids: vec![], self_close };
                // nesting beyond this is flattened so recursion (here and in every consumer) stays bounded
                if self_close || is_void(&name) || stack.len() > MAX_DEPTH { stack.last_mut().unwrap().kids.push(Node::El(el)); }
                else { stack.push(el); }
            }
            Token::Close(name) => {
                if let Some(pos) = stack.iter().rposition(|e| e.name == name).filter(|p| *p > 0) {
                    while stack.len() > pos { close_top(&mut stack); }
                }
            }
        }
    }
    while stack.len() > 1 { close_top(&mut stack); }
    stack.pop().unwrap().kids
}

pub fn serialize(nodes: &[Node]) -> String {
    let mut out = String::new();
    for n in nodes { write_node(n, &mut out); }
    out
}

fn write_node(n: &Node, out: &mut String) {
    match n {
        Node::Text(t) => out.push_str(t),
        Node::El(e) => {
            out.push('<');
            out.push_str(&e.name);
            for (k, v) in &e.attrs {
                out.push(' ');
                out.push_str(k);
                if let Some(v) = v { out.push_str("=\""); out.push_str(&attr_out(v)); out.push('"'); }
            }
            if e.kids.is_empty() && (e.self_close && !is_void(&e.name)) { out.push_str(" />"); return; }
            out.push('>');
            if is_void(&e.name) { return; }
            for k in &e.kids { write_node(k, out); }
            out.push_str("</");
            out.push_str(&e.name);
            out.push('>');
        }
    }
}

/// Rewrite every element bottom-up (children first); `f` returns what replaces it (nothing, the element, or several nodes).
pub fn map_els(nodes: Vec<Node>, f: &mut dyn FnMut(El) -> Vec<Node>) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        match n {
            Node::Text(_) => out.push(n),
            Node::El(mut e) => {
                e.kids = map_els(std::mem::take(&mut e.kids), f);
                out.extend(f(e));
            }
        }
    }
    out
}

/// Like `map_els` but top-down: `f` may replace an element (`Ok`, its replacement is not visited again) or decline (`Err`,
/// the element is kept and its children are visited).
pub fn map_els_pre(nodes: Vec<Node>, f: &mut dyn FnMut(El) -> Result<Vec<Node>, El>) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        match n {
            Node::Text(_) => out.push(n),
            Node::El(e) => match f(e) {
                Ok(r) => out.extend(r),
                Err(mut e) => { e.kids = map_els_pre(std::mem::take(&mut e.kids), f); out.push(Node::El(e)); }
            },
        }
    }
    out
}

/// First element (document order, depth-first) satisfying `pred`.
pub fn find_el<'a>(nodes: &'a [Node], pred: &dyn Fn(&El) -> bool) -> Option<&'a El> {
    for n in nodes {
        if let Node::El(e) = n {
            if pred(e) { return Some(e); }
            if let Some(f) = find_el(&e.kids, pred) { return Some(f); }
        }
    }
    None
}

/// Every element satisfying `pred`, in document order.
pub fn find_all_els<'a>(nodes: &'a [Node], pred: &dyn Fn(&El) -> bool, out: &mut Vec<&'a El>) {
    for n in nodes {
        if let Node::El(e) = n {
            if pred(e) { out.push(e); }
            find_all_els(&e.kids, pred, out);
        }
    }
}

impl El {
    pub fn has_class(&self, c: &str) -> bool { self.attr_dec("class").is_some_and(|v| v.split_whitespace().any(|x| x == c)) }
    pub fn remove_attr(&mut self, name: &str) { self.attrs.retain(|(k, _)| k != name); }

    pub fn new(name: &str, attrs: &[(&str, &str)], kids: Vec<Node>) -> El {
        El { name: name.into(), attrs: attrs.iter().map(|(k, v)| (k.to_string(), Some(escape_attr(v)))).collect(), kids, self_close: false }
    }
    /// The attribute value as written (still entity-encoded).
    pub fn attr(&self, name: &str) -> Option<&str> { self.attrs.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref()) }
    pub fn attr_dec(&self, name: &str) -> Option<String> { self.attr(name).map(decode_entities) }
    pub fn has_attr(&self, name: &str) -> bool { self.attrs.iter().any(|(k, _)| k == name) }
    /// `value` is plain text; it is stored entity-encoded.
    pub fn set_attr(&mut self, name: &str, value: &str) {
        let v = escape_attr(value);
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some(a) => a.1 = Some(v),
            None => self.attrs.push((name.to_string(), Some(v))),
        }
    }
    pub fn inner_html(&self) -> String { serialize(&self.kids) }
    pub fn text(&self) -> String { text_content(&self.kids) }
    pub fn child_els(&self) -> impl Iterator<Item = &El> { self.kids.iter().filter_map(|n| if let Node::El(e) = n { Some(e) } else { None }) }
}

/// Decoded text of the nodes; `<br>` reads as a newline.
pub fn text_content(nodes: &[Node]) -> String {
    let mut out = String::new();
    fn walk(n: &Node, out: &mut String) {
        match n {
            Node::Text(t) => out.push_str(&decode_entities(t)),
            Node::El(e) if e.name == "br" => out.push('\n'),
            Node::El(e) => for k in &e.kids { walk(k, out); },
        }
    }
    for n in nodes { walk(n, &mut out); }
    out
}

// ---------------------------------------------------------------- sanitising

/// Elements whose whole subtree goes: script-like and raw-text containers (sanitize-html's `nonTextTags` plus embedding and
/// foreign-content elements). Everything else that isn't allowed (`form`, `fieldset`, `picture`, `video`, ...) is unwrapped, keeping its text.
const DROP_WITH_CONTENT: &[&str] = &["script", "style", "iframe", "object", "embed", "applet", "frame", "frameset", "svg", "math", "animate", "set", "animatemotion", "animatetransform", "foreignobject", "head", "title", "noscript", "template", "textarea", "option", "track", "area", "base", "link", "meta"];

/// Trilium's `SANITIZER_DEFAULT_ALLOWED_TAGS`, minus the form controls and media tags dropped above.
const ALLOWED_TAGS: &[&str] = &[
    "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "p", "a", "ul", "ol", "li", "b", "i", "strong", "em", "u", "strike", "s", "del", "abbr", "code", "hr", "br", "div",
    "table", "thead", "caption", "tbody", "tfoot", "tr", "th", "td", "pre", "section", "img", "figure", "figcaption", "span", "label", "input", "details", "summary",
    "address", "aside", "footer", "header", "hgroup", "main", "nav", "dl", "dt", "menu", "bdi", "bdo", "dfn", "kbd", "mark", "q", "time", "var", "wbr", "ins",
    "en-media", "acronym", "article", "big", "cite", "col", "colgroup", "data", "dd", "rp", "samp", "small", "sub", "sup", "tt",
];

const ALLOWED_SCHEMES: &[&str] = &["http", "https", "ftp", "ftps", "mailto", "data", "evernote", "file", "facetime", "gemini", "git", "gopher", "imap", "irc", "irc6", "jabber", "jar", "lastfm",
    "ldap", "ldaps", "magnet", "message", "mumble", "nfs", "onenote", "pop", "rmi", "s3", "sftp", "skype", "sms", "spotify", "steam", "svn", "udp", "view-source", "vlc", "vnc", "ws", "wss",
    "xmpp", "jdbc", "slack", "tel", "smb", "zotero", "geo", "logseq", "mid", "obsidian", "bookends", "highlights"];

fn allowed_attr(tag: &str, attr: &str) -> bool {
    if attr.starts_with("data-") { return true; }
    if ["class", "style", "title", "src", "href", "hash", "disabled", "align", "alt", "center"].contains(&attr) { return true; }
    match tag {
        "a" | "li" | "h2" => attr == "id",
        "ol" => attr == "start" || attr == "reversed",
        "details" => attr == "open",
        "input" => attr == "type" || attr == "checked",
        "img" => attr == "width" || attr == "height",
        "td" => attr == "colspan" || attr == "rowspan",
        "th" => attr == "colspan" || attr == "rowspan" || attr == "scope",
        _ => false,
    }
}

fn unsafe_url(v: &str) -> bool {
    let cleaned: String = v.chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    cleaned.starts_with("javascript:") || cleaned.starts_with("vbscript:")
        || (cleaned.starts_with("data:") && (!cleaned.starts_with("data:image/") || cleaned.starts_with("data:image/svg")))
}

fn url_allowed(v: &str) -> bool {
    if unsafe_url(v) { return false; }
    // an entity we could not decode might become a scheme character in a browser: refuse it before the first '/', '?' or '#'
    let head = v.split(['/', '?', '#']).next().unwrap_or("");
    if head.contains('&') { return false; }
    let cleaned: String = v.chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match cleaned.split_once(':') {
        Some((scheme, _)) if !scheme.contains(['/', '?', '#']) => ALLOWED_SCHEMES.contains(&scheme),
        _ => true,
    }
}

fn color_ok(v: &str) -> bool {
    static C: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    C.get_or_init(|| regex::Regex::new(r"(?i)^(#(0x)?[0-9a-f]+|rgb\(\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*\d{1,3}\s*\)|hsl\(\s*\d{1,3}\s*,\s*\d{1,3}%\s*,\s*\d{1,3}%\s*\))$").unwrap()).is_match(v.trim())
}
fn size_ok(v: &str) -> bool {
    static S: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    S.get_or_init(|| regex::Regex::new(r"^\d+\.?\d*(?:px|em|%)$").unwrap()).is_match(v.trim())
}
fn border_style_ok(v: &str) -> bool { matches!(v.trim(), "none" | "hidden" | "dotted" | "dashed" | "solid" | "double" | "groove" | "ridge" | "inset" | "outset") }

fn style_allowed(tag: &str, prop: &str, v: &str) -> bool {
    let v = v.trim();
    match prop {
        "color" | "background-color" | "--tn-color" | "--tn-background" => return color_ok(v),
        "margin-left" | "padding-left" => return size_ok(v),
        "text-align" => return matches!(v, "left" | "center" | "right" | "justify"),
        "list-style-type" => return matches!(v, "disc" | "circle" | "square" | "decimal" | "decimal-leading-zero" | "lower-latin" | "upper-latin" | "lower-alpha" | "upper-alpha" | "lower-roman" | "upper-roman" | "none"),
        _ => {}
    }
    match (tag, prop) {
        ("figure", "float") => matches!(v, "left" | "right" | "none"),
        ("figure", "width" | "height" | "--include-note-width" | "--include-note-height") => size_ok(v),
        ("img", "width" | "height") => size_ok(v),
        ("img", "aspect-ratio") => { static A: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new(); A.get_or_init(|| regex::Regex::new(r"^\d+(\.\d+)?/\d+(\.\d+)?$").unwrap()).is_match(v) }
        ("col", "width") => size_ok(v),
        ("table" | "td" | "th", "border-color" | "--tn-border-color") => color_ok(v) || v == "transparent",
        ("table" | "td" | "th", "border-style") => border_style_ok(v),
        ("table" | "td" | "th", "border-width") => size_ok(v),
        ("table" | "td" | "th", "border") => {
            static B: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            B.get_or_init(|| regex::Regex::new(r"(?i)^\d+\.?\d*(?:px|em|%)\s*(none|hidden|dotted|dashed|solid|double|groove|ridge|inset|outset)\s*(#(0x)?[0-9a-f]+|rgb\(\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*\d{1,3}\s*\)|hsl\(\s*\d{1,3}\s*,\s*\d{1,3}%\s*,\s*\d{1,3}%\s*\))$").unwrap()).is_match(v)
        }
        _ => false,
    }
}

fn clean_style(tag: &str, style: &str) -> String {
    style.split(';').filter_map(|d| {
        let (p, v) = d.split_once(':')?;
        let (p, v) = (p.trim().to_ascii_lowercase(), v.trim());
        (!v.is_empty() && !v.contains(['\\', '<', '>']) && style_allowed(tag, &p, v)).then(|| format!("{p}:{v}"))
    }).collect::<Vec<_>>().join(";")
}

fn clean(nodes: Vec<Node>, rename: &[(String, String)]) -> Vec<Node> {
    let mut out = vec![];
    for n in nodes {
        match n {
            Node::Text(_) => out.push(n),
            Node::El(mut e) => {
                if DROP_WITH_CONTENT.contains(&e.name.as_str()) { continue; }
                let kids = clean(std::mem::take(&mut e.kids), rename);
                if !ALLOWED_TAGS.contains(&e.name.as_str()) { out.extend(kids); continue; }
                let orig = e.name.clone();
                e.attrs.retain(|(k, _)| allowed_attr(&orig, k));
                let mut keep = vec![];
                for (k, v) in std::mem::take(&mut e.attrs) {
                    let dec = v.as_deref().map(decode_entities);
                    match (k.as_str(), dec) {
                        ("href" | "src", Some(d)) if !url_allowed(&d) => {}
                        ("style", Some(d)) => { let c = clean_style(&orig, &d); if !c.is_empty() { keep.push((k, Some(escape_attr(&c)))); } }
                        ("class", Some(d)) if d.trim().is_empty() => {}
                        ("open" | "disabled" | "checked" | "reversed", Some(d)) if d.is_empty() => keep.push((k, None)),
                        (_, v2) => keep.push((k, v2.map(|_| v.unwrap_or_default()))),
                    }
                }
                e.attrs = keep;
                if let Some((_, to)) = rename.iter().find(|(from, _)| *from == e.name) { e.name = to.clone(); }
                e.kids = kids;
                out.push(Node::El(e));
            }
        }
    }
    out
}

/// Trilium's import sanitiser (`sanitizeHtml`): only the allowed tags, attributes, styles and URL schemes survive, scripts and
/// frames go entirely, other unknown tags are unwrapped, and a document that uses `<h1>` has its headings pushed down a level
/// (h1→h2, and h2→h3 if it had h2s too, …). Scripts, event handlers and `javascript:` / HTML `data:` URLs can't get through.
pub fn sanitize_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut rename = vec![];
    for i in 1..6 {
        if lower.contains(&format!("<h{i}")) { rename.push((format!("h{i}"), format!("h{}", i + 1))); } else { break; }
    }
    serialize(&clean(parse(html), &rename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_text_and_structure() {
        let h = r#"<p class="a">x &amp; y &lt; z<br><img src="a.png" alt='q"'><en-media hash="h" /></p>"#;
        assert_eq!(serialize(&parse(h)), r#"<p class="a">x &amp; y &lt; z<br><img src="a.png" alt="q&quot;"><en-media hash="h" /></p>"#);
    }

    #[test]
    fn tolerates_broken_markup() {
        assert_eq!(serialize(&parse("<p>a<b>bold</p>tail</i>")), "<p>a<b>bold</b></p>tail");
        assert_eq!(serialize(&parse("1 < 2 and <3")), "1 &lt; 2 and &lt;3");
        assert!(!parse("<div attr=\"unterminated").is_empty());
    }

    #[test]
    fn tokenizer_handles_cdata_doctype_and_comments() {
        let t = tokenize("<?xml version=\"1.0\"?><!DOCTYPE a [<!ENTITY x \"y\">]><a><!-- c --><![CDATA[<en-note>x</en-note>]]></a>", false);
        assert_eq!(t, vec![
            Token::Open { name: "a".into(), attrs: vec![], self_close: false },
            Token::CData("<en-note>x</en-note>".into()),
            Token::Close("a".into()),
        ]);
    }

    #[test]
    fn sanitiser_removes_active_content_only() {
        let dirty = r#"<p onclick="x()">hi <a href=" JaVa&#10;script:alert(1)">l</a><script>alert(1)</script><iframe src="//e"></iframe><img src="data:text/html;base64,AA"><img src="data:image/png;base64,AA"><b>ok</b></p>"#;
        let clean = sanitize_html(dirty);
        for bad in ["onclick", "script", "iframe", "javascript", "text/html"] { assert!(!clean.to_lowercase().contains(bad), "{bad}: {clean}"); }
        assert!(clean.contains("data:image/png") && clean.contains("<b>ok</b>") && clean.contains("hi "));
    }

    #[test]
    fn hostile_input_is_contained() {
        // tag names end at '/', so slash tricks can't hide a tag from the sanitiser
        for dirty in ["<img/src=x/onerror=alert(1)>", "<svg/onload=alert(1)>", "<script/x>alert(1)</script/x>", "<img src=a\" onerror=alert(1)>",
                      "<b><img src=x onerror=alert(1);//", "<a href=\"jav&Tab;ascript&colon;alert(1)\">x</a>", "<a href='JAVASCRIPT&#58;alert(1)'>x</a>"] {
            let c = sanitize_html(dirty).to_lowercase();
            assert!(!c.contains("<script"), "{dirty} -> {c}");
            // whatever remains parses as text only: no tag in the output carries an event handler
            for n in parse(&c) { if let Node::El(e) = n {
                assert!(!e.attrs.iter().any(|(k, v)| k.starts_with("on") || v.as_deref().is_some_and(|v| unsafe_url(&decode_entities(v)))), "{dirty} -> {c}");
                assert!(e.name != "script" && e.name != "svg", "{dirty} -> {c}");
            } }
        }
        // truncated markup with multibyte characters cannot panic
        for s in ["</aé", "</a😀", "<é", "<a é", "<!-- é", "<![CDATA[é", "<?é"] { let _ = parse(s); let _ = tokenize(s, false); }
        // deep nesting is flattened, not recursed into
        let deep = "<div>".repeat(50_000) + "x" + &"</div>".repeat(50_000);
        assert!(serialize(&parse(&deep)).contains('x'));
        assert!(sanitize_html(&deep).contains('x'));
        // unterminated tags and scripts stay fast
        let t = std::time::Instant::now();
        let _ = parse(&"<a ".repeat(40_000));
        let _ = parse(&"<script></script>".repeat(20_000));
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }

    #[test]
    fn reviewer_bypasses_are_closed() {
        for dirty in [r#"<p data-a"><script src=//e/x.js x=">hi</p>"#, r#"<p data-a"><iframe src=//e/x.js x=">hi</p>"#, "<a href=\"javascript&#58alert(1)\">x</a>",
                      "<a href=\"javascript&#x3a alert(1)\">x</a>", "<a href=\"java&#115cript:alert(1)\">x</a>", "<a href=\"jav&#x09;ascript:alert(1)\">x</a>", "<img src=\"x\"onerror=alert(1)>", "<b/onclick=alert(1)>x</b>"] {
            let c = sanitize_html(dirty).to_lowercase();
            for bad in ["<script", "<iframe", "javascript", "onerror", "onclick"] {
                assert!(!c.contains(bad), "{dirty} -> {c}");
            }
            // nothing in the output may re-parse into an element with a handler or a script
            fn check(nodes: &[Node], dirty: &str) {
                for n in nodes { if let Node::El(e) = n {
                    assert!(e.name != "script" && e.name != "iframe" && !e.attrs.iter().any(|(k, _)| k.starts_with("on")), "{dirty}");
                    check(&e.kids, dirty);
                } }
            }
            check(&parse(&c), dirty);
        }
        // attribute and element names are always plain
        let c = sanitize_html("<p data-a\"b='x' data-ok=1>t</p>");
        assert!(!c.contains('"') || c.matches('"').count() % 2 == 0, "{c}");
        let t = std::time::Instant::now();
        let _ = parse(&"</a".repeat(200_000));
        let _ = parse(&"<a \"".repeat(100_000));
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }

    #[test]
    fn entities_survive_round_trips() {
        let h = r#"<a title="&copy; caf&eacute; &amp; &quot;q&quot;" href="x?a=1&b=2">t</a>"#;
        assert_eq!(serialize(&parse(h)), r#"<a title="&copy; caf&eacute; &amp; &quot;q&quot;" href="x?a=1&b=2">t</a>"#);
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &#65;&#x42; &bogus; &"), "a & b AB &bogus; &");
        assert_eq!(text_content(&parse("<p>a<br>b &lt;i&gt;</p>")), "a\nb <i>");
    }

    #[test]
    fn long_numeric_entities_cannot_hide_a_scheme_and_form_text_survives() {
        for href in ["javascript&#x0000000003A;PWN(1)", "javascript&#0000000058;PWN(1)", "javascript&#x3A;x", "jav&#x61;script:x"] {
            let out = sanitize_html(&format!(r#"<a href="{href}">x</a><img src="{href}">"#));
            assert!(!out.to_lowercase().contains("javascript") && !out.contains("PWN") , "{href} -> {out}");
        }
        assert_eq!(decode_entities("&#x0000000003A;&#0000000065;&#xD800;&#0;&#x110000;"), ":A\u{FFFD}\u{FFFD}\u{FFFD}");
        assert_eq!(sanitize_html("<form><p>Hello world</p></form><fieldset>kept</fieldset><picture><img src=\"a.png\"></picture><video><p>fallback</p></video>"), "<p>Hello world</p>kept<img src=\"a.png\"><p>fallback</p>");
        assert!(!sanitize_html("<script>alert(1)</script><textarea>t</textarea>x").contains("alert"));
    }
}
