//! Markdown → Trilium/CKEditor HTML, following Trilium's `markdown_renderer.ts`: fenced code with
//! mime-style language classes, task lists as `todo-list`, tables in `<figure class="table">`, callouts as
//! admonitions, `$…$` / `$$…$$` math spans, `==highlight==`, `[[wikilinks]]` and `![[embeds]]` (left as
//! `href="/target"` / `src="/target"` for the importer to resolve), and the leading title heading dropped.
use crate::htmltree::{decode_entities, escape_text, sanitize_html};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

const HIGHLIGHT_STYLE: &str = "background-color:#e5e64d;--tn-background:#e5e64d;";

fn rx(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex { cell.get_or_init(|| Regex::new(pat).expect("static regex")) }

/// Obsidian's extra callout types, mapped onto the five admonition kinds Trilium has.
fn callout_type(t: &str, obsidian: bool) -> Option<&'static str> {
    Some(match t {
        "note" => "note", "tip" => "tip", "important" => "important", "caution" => "caution", "warning" => "warning",
        _ if !obsidian => return None,
        "abstract" | "summary" | "tldr" | "info" | "todo" | "quote" | "cite" => "note",
        "hint" | "success" | "check" | "done" | "question" | "help" | "faq" => "tip",
        "example" => "important",
        "attention" => "warning",
        "failure" | "fail" | "missing" | "danger" | "error" | "bug" => "caution",
        _ => return None,
    })
}

struct Shield { map: HashMap<String, String>, orig: HashMap<String, String>, n: usize }

impl Shield {
    fn new() -> Self { Shield { map: HashMap::new(), orig: HashMap::new(), n: 0 } }
    fn put(&mut self, kind: &str, value: String) -> String { self.put_with(kind, value, None) }
    /// `orig` is the source text, used instead of `value` when the placeholder ends up inside an HTML tag (a link destination).
    fn put_with(&mut self, kind: &str, value: String, orig: Option<String>) -> String {
        self.n += 1;
        let key = format!("\u{E000}{kind}{}\u{E001}", self.n);
        if let Some(o) = orig { self.orig.insert(key.clone(), o); }
        self.map.insert(key.clone(), value);
        key
    }
    fn restore(&self, text: &str) -> String {
        if self.map.is_empty() { return text.to_string(); }
        static ANY: OnceLock<Regex> = OnceLock::new();
        static BLOCK: OnceLock<Regex> = OnceLock::new();
        // a block formula that is a paragraph by itself stands alone, as in Trilium's renderer
        let mut text = rx(&BLOCK, r"<p>(\x{E000}B\d+\x{E001})</p>").replace_all(text, "$1").into_owned();
        // values can contain other placeholders (code inside math, ...): resolve until none are left
        for _ in 0..4 {
            if !text.contains('\u{E000}') { break; }
            let src = text.clone();
            text = rx(&ANY, r"\x{E000}(?:C|F|W|H|B)\d+\x{E001}").replace_all(&src, |c: &regex::Captures| {
                let whole = c.get(0).unwrap();
                let before = &src[..whole.start()];
                let in_tag = match (before.rfind('<'), before.rfind('>')) { (Some(l), Some(g)) => l > g, (Some(_), None) => true, _ => false };
                match (in_tag, self.orig.get(&c[0]), self.map.get(&c[0])) {
                    (true, Some(o), _) => escape_attr(o),
                    (_, _, Some(v)) => v.clone(),
                    _ => String::new(),
                }
            }).into_owned();
        }
        text.replace(['\u{E000}', '\u{E001}'], "")
    }
}

fn escape_formula(f: &str) -> String { f.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;") }

/// Replace `$$…$$` and `$…$` formulas with placeholders (GitHub/Pandoc rules: prices like `$5 and $6` stay text).
fn shield_math(text: &str, sh: &mut Shield) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let at = |k: usize| chars.get(k).copied();
    while i < chars.len() {
        let c = chars[i];
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        if c == '$' && prev != Some('\\') && prev != Some('$') {
            if at(i + 1) == Some('$') && at(i + 2) != Some('$') {
                // block: up to the next lone $$ with no blank line inside
                let mut j = i + 2;
                let mut found = None;
                while j < chars.len() {
                    if chars[j] == '$' && at(j + 1) == Some('$') && at(j + 2) != Some('$') && chars[j - 1] != '$' { found = Some(j); break; }
                    if chars[j] == '$' { break; }
                    if chars[j] == '\n' && at(j + 1) == Some('\n') { break; }
                    j += 1;
                }
                if let Some(j) = found.filter(|j| *j > i + 2) {
                    let f: String = chars[i + 2..j].iter().collect();
                    out.push_str(&sh.put_with("B", format!("<span class=\"math-tex\">\\[{}\\]</span>", escape_formula(&f)), Some(format!("$${f}$$"))));
                    i = j + 2;
                    continue;
                }
            } else if at(i + 1).is_some_and(|n| !n.is_whitespace() && n != '$') {
                let mut j = i + 1;
                let mut found = None;
                while j < chars.len() && chars[j] != '\n' {
                    if chars[j] == '$' { found = Some(j); break; }
                    j += 1;
                }
                if let Some(j) = found {
                    let before = chars[j - 1];
                    let after = at(j + 1);
                    if !before.is_whitespace() && before != '$' && after != Some('$') && !after.is_some_and(|a| a.is_ascii_digit()) {
                        let f: String = chars[i + 1..j].iter().collect();
                        out.push_str(&sh.put_with("F", format!("<span class=\"math-tex\">\\({}\\)</span>", escape_formula(&f)), Some(format!("${f}$"))));
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Markdown source → HTML for a note whose title is `title` (a matching leading `# title` is dropped).
pub fn md_to_html(md: &str, title: &str, obsidian: bool) -> String {
    let md = md.replace(['\u{E000}', '\u{E001}'], "\u{FFFD}");
    let md = md.as_str();
    let mut sh = Shield::new();
    // 1. code is shielded so nothing below touches it
    static FENCE: OnceLock<Regex> = OnceLock::new();
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let text = rx(&FENCE, r"(?ms)^[ \t]*(?:>[ \t]*)*(```|~~~)[^\n]*\n.*?^[ \t]*(?:>[ \t]*)*(```|~~~)[ \t]*$")
        .replace_all(md, |c: &regex::Captures| sh.put("C", c[0].to_string())).into_owned();
    let text = rx(&INLINE, r"`[^`\n]+`").replace_all(&text, |c: &regex::Captures| sh.put("C", c[0].to_string())).into_owned();
    // 2. things marked/pulldown don't know
    let text = shield_math(&text, &mut sh);
    static EMBED: OnceLock<Regex> = OnceLock::new();
    static WIKI: OnceLock<Regex> = OnceLock::new();
    static HILITE: OnceLock<Regex> = OnceLock::new();
    static COMMENT: OnceLock<Regex> = OnceLock::new();
    let text = if obsidian { rx(&COMMENT, r"(?s)%%.*?%%").replace_all(&text, "").into_owned() } else { text };
    let text = rx(&EMBED, r"!\[\[([^\]]+?)\]\]").replace_all(&text, |c: &regex::Captures| sh.put_with("W", format!("<img src=\"/{}\">", escape_attr(c[1].trim())), Some(c[0].to_string()))).into_owned();
    let text = rx(&WIKI, r"\[\[([^\]]+?)\]\]").replace_all(&text, |c: &regex::Captures| {
        let inner = c[1].trim();
        sh.put_with("W", format!("<a class=\"reference-link\" href=\"/{}\">{}</a>", escape_attr(inner), escape_text(inner)), Some(c[0].to_string()))
    }).into_owned();
    let text = rx(&HILITE, r"==(\S|\S[^\n]*?\S)==").replace_all(&text, |c: &regex::Captures| {
        format!("{}{}{}", sh.put_with("H", format!("<span style=\"{HIGHLIGHT_STYLE}\">"), Some("==".into())), &c[1], sh.put_with("H", "</span>".into(), Some("==".into())))
    }).into_owned();
    // a lone ~ is "approximately", not strikethrough
    let text = literal_tildes(&text);
    let text = sh.restore_code(&text);

    // 3. parse
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES);
    let html = render(Parser::new_ext(&text, opts), obsidian);
    let html = sh.restore(&html);
    let html = demote_headings(&html, title);
    // CKEditor writes a trailing semicolon after the last style declaration; sanitising drops it
    static STYLE: OnceLock<Regex> = OnceLock::new();
    let out = rx(&STYLE, r#"(<[a-z][^<>]*\sstyle="[^"]*[^";])""#).replace_all(&sanitize_html(&html), "$1;\"").into_owned();
    add_collapsible_class(&out)
}

impl Shield {
    /// Put back only code placeholders (the rest stay shielded through parsing).
    fn restore_code(&self, text: &str) -> String {
        static C: OnceLock<Regex> = OnceLock::new();
        rx(&C, r"\x{E000}C\d+\x{E001}").replace_all(text, |c: &regex::Captures| self.map.get(&c[0]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned()
    }
}

fn escape_attr(s: &str) -> String { escape_text(s).replace('"', "&quot;") }

fn literal_tildes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let lone = c == '~' && (i == 0 || chars[i - 1] != '~') && chars.get(i + 1) != Some(&'~') && (i == 0 || chars[i - 1] != '\\');
        if lone { out.push('\\'); }
        out.push(c);
    }
    out
}

fn render<'a>(events: impl Iterator<Item = Event<'a>>, obsidian: bool) -> String {
    let mut bufs: Vec<String> = vec![String::new()];
    let mut pending = String::new();
    let mut list_stack: Vec<(bool, Option<u64>, bool)> = vec![]; // (ordered, start, any_task)
    let mut item_task: Vec<Option<bool>> = vec![];
    let mut code: Option<(String, String)> = None; // (language, text)
    let mut in_head = false;
    let mut aligns: Vec<pulldown_cmark::Alignment> = vec![];
    let mut col = 0usize;
    let mut link_depth = 0usize;
    let mut table_has_body = false;
    let mut footnotes: Vec<(String, String)> = vec![]; // (label, body html)
    let mut footnote_refs: Vec<String> = vec![];
    let mut footnote_names: Vec<String> = vec![];
    let mut image_stack: Vec<(String, String)> = vec![];
    macro_rules! out { ($($a:tt)*) => { bufs.last_mut().unwrap().push_str(&format!($($a)*)) }; }
    for ev in events {
        if let Some((_, t)) = code.as_mut() {
            match ev {
                Event::Text(s) => { t.push_str(&s); continue; }
                Event::End(TagEnd::CodeBlock) => {}
                _ => continue,
            }
        }
        // adjacent text events (escapes split a run) are one text for autolinking
        if let Event::Text(s) = &ev { pending.push_str(s); continue; }
        if !pending.is_empty() {
            let esc = escape_text(&pending);
            let html = if link_depth == 0 && image_stack.is_empty() { autolink(&esc) } else { esc };
            bufs.last_mut().unwrap().push_str(&html);
            pending.clear();
        }
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out!("<p>"),
                Tag::Heading { level, .. } => out!("<h{}>", level as u8),
                Tag::BlockQuote(_) => bufs.push(String::new()),
                Tag::CodeBlock(kind) => {
                    let lang = match kind { CodeBlockKind::Fenced(info) => info.split(|c: char| c.is_whitespace() || c == '{').next().unwrap_or("").to_string(), _ => String::new() };
                    code = Some((lang, String::new()));
                }
                Tag::List(start) => { list_stack.push((start.is_some(), start, false)); bufs.push(String::new()); }
                Tag::Item => { item_task.push(None); bufs.push(String::new()); }
                Tag::FootnoteDefinition(name) => { footnote_names.push(name.to_string()); bufs.push(String::new()); }
                Tag::Table(a) => { aligns = a; out!("<figure class=\"table\"><table>") }
                Tag::TableHead => { in_head = true; table_has_body = false; col = 0; out!("<thead><tr>") }
                Tag::TableRow => { if !table_has_body { table_has_body = true; out!("<tbody>"); } col = 0; out!("<tr>") }
                Tag::TableCell => {
                    let al = match aligns.get(col) { Some(pulldown_cmark::Alignment::Left) => " align=\"left\"", Some(pulldown_cmark::Alignment::Right) => " align=\"right\"", Some(pulldown_cmark::Alignment::Center) => " align=\"center\"", _ => "" };
                    col += 1;
                    out!("<{}{al}>", if in_head { "th" } else { "td" })
                }
                Tag::Emphasis => out!("<em>"),
                Tag::Strong => out!("<strong>"),
                Tag::Strikethrough => out!("<del>"),
                Tag::Link { dest_url, title, .. } => {
                    let t = if title.is_empty() { String::new() } else { format!(" title=\"{}\"", escape_attr(&title)) };
                    link_depth += 1;
                    out!("<a href=\"{}\"{t}>", escape_attr(&dest_url));
                }
                Tag::Image { dest_url, title, .. } => { image_stack.push((dest_url.to_string(), title.to_string())); bufs.push(String::new()); }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph => out!("</p>"),
                TagEnd::Heading(level) => out!("</h{}>", level as u8),
                TagEnd::BlockQuote(_) => {
                    let body = bufs.pop().unwrap_or_default();
                    out!("{}", callout_or_quote(&body, obsidian));
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = code.take() {
                        if !text.is_empty() {
                            let ck = crate::formats::code_language(if lang.is_empty() { None } else { Some(lang.as_str()) });
                            let body = escape_text(&text).replace("&quot;", "\"");
                            out!("<pre><code class=\"language-{ck}\">{}</code></pre>", body.strip_suffix('\n').unwrap_or(&body));
                        }
                    }
                }
                TagEnd::List(_) => {
                    let inner = bufs.pop().unwrap_or_default();
                    let (ordered, start, task) = list_stack.pop().unwrap_or((false, None, false));
                    if ordered { match start { Some(n) if n != 1 => out!("<ol start=\"{n}\">{inner}</ol>"), _ => out!("<ol>{inner}</ol>") } }
                    else if task { out!("<ul class=\"todo-list\">{inner}</ul>") }
                    else { out!("<ul>{inner}</ul>") }
                }
                TagEnd::Item => {
                    let body = bufs.pop().unwrap_or_default();
                    match item_task.pop().flatten() {
                        Some(checked) => {
                            if let Some(l) = list_stack.last_mut() { l.2 = true; }
                            let (desc, rest) = split_task_body(&body);
                            let checked = if checked { " checked=\"checked\"" } else { "" };
                            out!("<li><label class=\"todo-list__label\"><input type=\"checkbox\"{checked} disabled=\"disabled\"><span class=\"todo-list__label__description\">{desc}</span></label>{rest}</li>");
                        }
                        None => out!("<li>{}</li>", body.trim_end()),
                    }
                }
                TagEnd::FootnoteDefinition => {
                    let body = bufs.pop().unwrap_or_default();
                    let name = footnote_names.pop().unwrap_or_default();
                    footnotes.push((name, body.trim().to_string()));
                }
                TagEnd::Table => { if table_has_body { out!("</tbody>"); } out!("</table></figure>") }
                TagEnd::TableHead => { in_head = false; out!("</tr></thead>") }
                TagEnd::TableRow => out!("</tr>"),
                TagEnd::TableCell => out!("{}", if in_head { "</th>" } else { "</td>" }),
                TagEnd::Emphasis => out!("</em>"),
                TagEnd::Strong => out!("</strong>"),
                TagEnd::Strikethrough => out!("</del>"),
                TagEnd::Link => { link_depth = link_depth.saturating_sub(1); out!("</a>") }
                TagEnd::Image => {
                    let alt_html = bufs.pop().unwrap_or_default();
                    let (src, title) = image_stack.pop().unwrap_or_default();
                    let alt = decode_entities(&strip_tags(&alt_html));
                    let a = if alt.is_empty() { String::new() } else { format!(" alt=\"{}\"", escape_attr(&alt)) };
                    let t = if title.is_empty() { String::new() } else { format!(" title=\"{}\"", escape_attr(&title)) };
                    out!("<img src=\"{}\"{a}{t}>", escape_attr(&src));
                }
                _ => {}
            },
            Event::Code(s) => out!("<code>{}</code>", escape_text(&s)),
            Event::Html(s) | Event::InlineHtml(s) => out!("{s}"),
            Event::SoftBreak => out!("\n"),
            Event::HardBreak => out!("<br>"),
            Event::Rule => out!("<hr>"),
            Event::TaskListMarker(c) => { if let Some(t) = item_task.last_mut() { *t = Some(c); } }
            Event::FootnoteReference(n) => {
                let idx = match footnote_refs.iter().position(|x| *x == *n) { Some(i) => i, None => { footnote_refs.push(n.to_string()); footnote_refs.len() - 1 } };
                let slug = footnote_slug(&n);
                out!("<sup><a id=\"footnote-ref-{slug}\" href=\"#footnote-{slug}\" data-footnote-ref>{}</a></sup>", idx + 1);
            }
            _ => {}
        }
    }
    if !pending.is_empty() { let esc = escape_text(&pending); bufs.last_mut().unwrap().push_str(&if link_depth == 0 && image_stack.is_empty() { autolink(&esc) } else { esc }); }
    let mut html = bufs.concat();
    if !footnotes.is_empty() {
        footnotes.sort_by_key(|(name, _)| footnote_refs.iter().position(|r| r == name).unwrap_or(usize::MAX));
        let items: String = footnotes.iter().map(|(name, body)| {
            let slug = footnote_slug(name);
            let back = format!(" <a href=\"#footnote-ref-{slug}\" data-footnote-backref>\u{21a9}</a>");
            let body = match body.rfind("</p>") { Some(i) => format!("{}{back}{}", &body[..i], &body[i..]), None => format!("{body}{back}") };
            format!("<li id=\"footnote-{slug}\">{body}</li>")
        }).collect();
        html.push_str(&format!("<section class=\"footnotes\" data-footnotes><h2 id=\"footnote-label\" class=\"sr-only\">Footnotes</h2><ol>{items}</ol></section>"));
    }
    html
}

/// GFM autolinks: bare `http(s)://…` and `www.…` addresses in running text.
fn autolink(escaped: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    if !escaped.contains("://") && !escaped.contains("www.") { return escaped.to_string(); }
    rx(&URL, r#"(?:https?://|www\.)[^\s<"]+"#).replace_all(escaped, |c: &regex::Captures| {
        let mut url = c[0].to_string();
        let mut tail = String::new();
        while url.ends_with(['.', ',', ';', ':', '!', '?', '*', '_', '~', '\'', '"']) || (url.ends_with(')') && url.matches('(').count() < url.matches(')').count()) {
            tail.insert(0, url.pop().unwrap());
        }
        if url.ends_with("&amp") || url.contains("&amp;&") { return c[0].to_string(); }
        let href = if url.starts_with("www.") { format!("http://{url}") } else { url.clone() };
        format!("<a href=\"{href}\">{url}</a>{tail}")
    }).into_owned()
}

/// `<details>` in Markdown carries no class; Trilium's editor expects `trilium-collapsible`.
fn add_collapsible_class(html: &str) -> String {
    if !html.contains("<details") { return html.to_string(); }
    static D: OnceLock<Regex> = OnceLock::new();
    rx(&D, r"<details\b([^>]*)>").replace_all(html, |c: &regex::Captures| {
        let attrs = &c[1];
        if attrs.contains("trilium-collapsible") { c[0].to_string() }
        else if let Some(i) = attrs.find(" class=\"") { format!("<details{} class=\"trilium-collapsible {}>", &attrs[..i], &attrs[i + 8..]) }
        else { format!("<details class=\"trilium-collapsible\"{attrs}>") }
    }).into_owned()
}

fn footnote_slug(name: &str) -> String { name.chars().map(|c| if c.is_whitespace() { '-' } else { c }).collect::<String>().to_lowercase() }

fn strip_tags(s: &str) -> String { static T: OnceLock<Regex> = OnceLock::new(); rx(&T, r"<[^>]*>").replace_all(s, "").into_owned() }

/// First block of a task item is its label; everything after it (nested lists, more paragraphs) follows the label.
fn split_task_body(body: &str) -> (String, String) {
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    let body = body.trim();
    if let Some(inner) = body.strip_prefix("<p>") {
        if let Some(end) = inner.find("</p>") {
            return (inner[..end].trim().to_string(), inner[end + 4..].trim().to_string());
        }
    }
    match rx(&BLOCK, r"<(ul|ol|pre|p|figure|blockquote|aside|table)\b").find(body) {
        Some(m) => (body[..m.start()].trim().to_string(), body[m.start()..].to_string()),
        None => (body.to_string(), String::new()),
    }
}

fn callout_or_quote(body: &str, obsidian: bool) -> String {
    static MARK: OnceLock<Regex> = OnceLock::new();
    let Some(m) = rx(&MARK, r"^<p>\[!([a-zA-Z]+)\]([-+]?)[ \t]*").captures(body) else { return format!("<blockquote>{body}</blockquote>") };
    let Some(kind) = callout_type(&m[1].to_lowercase(), obsidian) else { return format!("<blockquote>{body}</blockquote>") };
    let rest = &body[m.get(0).map_or(0, |x| x.end())..];
    let nl = rest.find('\n');
    let pend = rest.find("</p>");
    let (title, inner) = match (pend, nl) {
        (Some(p), n) if n.is_none_or(|n| p < n) => (rest[..p].to_string(), rest[p + 4..].to_string()),
        (_, Some(n)) => (rest[..n].to_string(), format!("<p>{}", &rest[n + 1..])),
        _ => (rest.to_string(), String::new()),
    };
    let title_html = if title.trim().is_empty() { String::new() } else { format!("<p><strong>{}</strong></p>", title.trim()) };
    let inside = format!("{title_html}{}", inner.trim());
    format!("<aside class=\"admonition {kind}\">{}</aside>", if inside.is_empty() { "&nbsp;" } else { &inside })
}

/// Strip a leading `<h1>` that repeats the title; if an `<h1>` remains, shift every heading down a level.
fn demote_headings(html: &str, title: &str) -> String {
    static H1: OnceLock<Regex> = OnceLock::new();
    static SUB: OnceLock<Regex> = OnceLock::new();
    static H1B: OnceLock<Regex> = OnceLock::new();
    let h1 = rx(&H1, r"(?is)<h1[^>]*>(.*?)</h1>");
    let mut done = false;
    let html = h1.replace(html, |c: &regex::Captures| {
        if !done { done = true; if decode_entities(&c[1]).trim() == title.trim() { return String::new(); } }
        c[0].to_string()
    }).into_owned();
    if !h1.is_match(&html) { return html; }
    let html = rx(&SUB, r"(?i)<(/?)h([2-5])\b([^>]*)>").replace_all(&html, |c: &regex::Captures| format!("<{}h{}{}>", &c[1], c[2].parse::<u8>().unwrap_or(2) + 1, &c[3])).into_owned();
    rx(&H1B, r"(?is)<h1([^>]*)>(.*?)</h1>").replace_all(&html, "<h2$1>$2</h2>").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_inlines_and_title() {
        let h = md_to_html("# My Title\n\nHello *there* **you** ~~old~~ `a<b` and ~5\n\n## Sub\n\n- a\n- b\n  - c\n\n1. x\n2. y\n\n---\n", "My Title", false);
        assert!(!h.contains("My Title"), "{h}");
        for want in ["<p>Hello <em>there</em> <strong>you</strong> <del>old</del> <code>a&lt;b</code> and ~5</p>", "<h2>Sub</h2>", "<ul><li>a</li><li>b<ul><li>c</li></ul></li></ul>", "<ol><li>x</li><li>y</li></ol>", "<hr>"] {
            assert!(h.contains(want), "missing {want}\n{h}");
        }
        // a remaining h1 shifts the hierarchy
        let h = md_to_html("# One\n\n## Two\n", "Other", false);
        assert!(h.contains("<h2>One</h2>") && h.contains("<h3>Two</h3>"), "{h}");
    }

    #[test]
    fn code_tables_tasks_math_and_callouts() {
        let md = "```python\nprint('<hi>')\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n- [ ] todo\n  - [ ] sub\n\nInline $e=mc^2$ and $$\\sum x<y$$ but $5 and $6.\n\n> [!warning] Careful\n> text here\n\n> plain quote\n";
        let h = md_to_html(md, "t", false);
        for want in [r#"<pre><code class="language-text-x-python">print('&lt;hi&gt;')</code></pre>"#,
            r#"<figure class="table"><table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table></figure>"#,
            r#"<ul class="todo-list"><li><label class="todo-list__label"><input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">done</span></label></li>"#,
            r#"<span class="todo-list__label__description">sub</span>"#,
            r#"<span class="math-tex">\(e=mc^2\)</span>"#, r#"<span class="math-tex">\[\sum x&lt;y\]</span>"#, "but $5 and $6.",
            r#"<aside class="admonition warning"><p><strong>Careful</strong></p><p>text here</p></aside>"#, "<blockquote><p>plain quote</p></blockquote>"] {
            assert!(h.contains(want), "missing {want}\n{h}");
        }
    }

    #[test]
    fn obsidian_syntax() {
        let md = "See [[Other Note|alias]] and [[Folder/Note#Head]] ==marked==.\n\n![[Embedded]] ![[pic.png|300]]\n\n%% hidden %% shown\n\n> [!tldr]- Summary\n> body\n\n```\n[[not a link]] ==nor this==\n```\n";
        let h = md_to_html(md, "t", true);
        for want in [r#"<a class="reference-link" href="/Other Note|alias">Other Note|alias</a>"#, r#"href="/Folder/Note#Head""#,
            r#"<span style="background-color:#e5e64d;--tn-background:#e5e64d;">marked</span>"#, r#"<img src="/Embedded">"#, r#"<img src="/pic.png|300">"#,
            "shown", r#"<aside class="admonition note"><p><strong>Summary</strong></p><p>body</p></aside>"#, "[[not a link]] ==nor this=="] {
            assert!(h.contains(want), "missing {want}\n{h}");
        }
        assert!(!h.contains("hidden"));
        // outside Obsidian mode a tldr callout stays a quote
        assert!(md_to_html("> [!tldr]\n> x", "t", false).contains("<blockquote>"));
    }

    #[test]
    fn collapsible_class_is_added_to_the_details_element_itself() {
        let h = md_to_html("<details data-class=\"foo\"><summary>s</summary>\n\nbody\n\n</details>", "t", false);
        assert!(h.contains("<details class=\"trilium-collapsible\" data-class=\"foo\">"), "{h}");
    }

    #[test]
    fn reviewer_markdown_cases() {
        let h = md_to_html("[x](http://a.com/$b$) and $a `b` c$ and [[a `b` c]] and see http://a.com/\"x here and http://a.com/x_y_z.\n\n\u{E000}W1\u{E001}", "t", false);
        assert!(h.contains(r#"<a href="http://a.com/$b$">x</a>"#), "{h}");
        assert!(!h.contains('\u{E000}') && !h.contains('\u{E001}'), "{h}");
        assert!(h.contains(r#"<a href="http://a.com/x_y_z">http://a.com/x_y_z</a>."#) || h.contains(r#"href="http://a.com/x_y_z""#), "{h}");
    }

    #[test]
    fn hostile_markdown_is_sanitised_and_does_not_panic() {
        let h = md_to_html("<script>alert(1)</script>\n\n[x](javascript:alert(1)) <img src=x onerror=alert(1)>\n\n$$ unterminated\n\n[[", "t", true);
        assert!(!h.contains("<script") && !h.contains("onerror") && !h.contains("javascript:"), "{h}");
        let _ = md_to_html(&"> ".repeat(10_000), "t", false);
        let _ = md_to_html(&"- a\n".repeat(50_000), "t", false);
        let _ = md_to_html("日本語 $é$ ==ü== ~", "t", true);
    }
}
