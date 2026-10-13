//! Trilium note HTML → Markdown (GFM flavoured), for the Markdown export. It understands the markup CKEditor
//! stores: headings, lists and to-do lists, code blocks with mime-style language classes, tables, admonitions,
//! highlights, math spans, images and links. Anything it has no Markdown form for (tables without a header
//! row, collapsibles, coloured text, includes) is kept as inline HTML, so nothing is dropped.
use crate::htmltree::{El, Node, decode_entities, escape_attr, parse, serialize};
use regex::Regex;
use std::sync::OnceLock;

const INLINE: &[&str] = &["a", "b", "strong", "i", "em", "code", "s", "del", "strike", "u", "mark", "span", "sup", "sub", "kbd", "img", "br", "small", "abbr", "big", "cite", "q", "label", "input", "font", "time", "var", "samp", "ins", "en-media"];
const HIGHLIGHT_BG: &str = "#e5e64d";

pub fn html_to_markdown(html: &str) -> String {
    let nodes = parse(html);
    let blocks = blocks(&nodes, 0);
    let mut out = blocks.join("\n\n");
    while out.contains("\n\n\n") { out = out.replace("\n\n\n", "\n\n"); }
    let mut out = out.trim().to_string();
    if !out.is_empty() { out.push('\n'); }
    out
}

fn has_class(e: &El, c: &str) -> bool { e.attr_dec("class").is_some_and(|v| v.split_whitespace().any(|x| x == c)) }

/// Join artefacts (a space ending the text plus a space leading an emphasised run) become one space.
fn tidy_spaces(s: &str) -> String {
    static SP: OnceLock<Regex> = OnceLock::new();
    SP.get_or_init(|| Regex::new(r"(\S) {2,}(\S)").unwrap()).replace_all(s, "$1 $2").into_owned()
}

fn collapse_ws(s: &str) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    WS.get_or_init(|| Regex::new(r"[ \t\r\n\x0c]+").unwrap()).replace_all(s, " ").into_owned()
}

fn escape_md(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        match c {
            '\\' | '*' | '_' | '`' | '[' | ']' | '$' => { out.push('\\'); out.push(c); }
            // doubled markers would read as ==highlight== / ~~strike~~
            '=' | '~' if next == Some(c) || prev == Some(c) => { out.push('\\'); out.push(c); }
            // only where Markdown would read it as HTML / an entity
            '<' if next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')) => out.push_str("&lt;"),
            '&' if next.is_some_and(|n| n.is_ascii_alphanumeric() || n == '#') => out.push_str("&amp;"),
            _ => out.push(c),
        }
    }
    // characters that start a block when they open a line
    let t = out.trim_start();
    let lead = out.len() - t.len();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let ordered = digits > 0 && digits <= 9 && t[digits..].starts_with(['.', ')']) && (t[digits + 1..].is_empty() || t[digits + 1..].starts_with(' '));
    if ordered {
        return format!("{}{}\\{}", &out[..lead], &t[..digits], &t[digits..]);
    }
    let rule = t.len() >= 2 && t.chars().all(|c| c == '-' || c == ' ') && t.starts_with("--");
    let needs = t.starts_with('#') || t.starts_with('>') || ((t.starts_with('-') || t.starts_with('+')) && t[1..].starts_with(' ')) || rule;
    if needs { format!("{}\\{}", &out[..lead], t) } else { out }
}

/// Block-level rendering of a node list; consecutive inline content becomes one paragraph.
fn blocks(nodes: &[Node], depth: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<String>| {
        let t = tidy_spaces(run.trim());
        if !t.is_empty() { out.push(t); }
        run.clear();
    };
    if depth > 200 { return vec![serialize(nodes)]; }
    for n in nodes {
        match n {
            Node::Text(t) => run.push_str(&escape_md(&collapse_ws(&decode_entities(t)))),
            Node::El(e) if INLINE.contains(&e.name.as_str()) => run.push_str(&inline(e, depth + 1)),
            Node::El(e) => {
                flush(&mut run, &mut out);
                block(e, depth + 1, &mut out);
            }
        }
    }
    flush(&mut run, &mut out);
    out
}

fn inline_nodes(nodes: &[Node], depth: usize) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            Node::Text(t) => s.push_str(&escape_md(&collapse_ws(&decode_entities(t)))),
            Node::El(e) if INLINE.contains(&e.name.as_str()) => s.push_str(&inline(e, depth + 1)),
            // a block inside inline context (e.g. a <p> in a table cell): flatten
            Node::El(e) => { let mut b = vec![]; block(e, depth + 1, &mut b); s.push_str(&b.join(" ")); s.push(' '); }
        }
    }
    s
}

/// Wrap `inner` in `mark` keeping any leading/trailing space outside (Markdown forbids spaces inside emphasis).
fn wrap(mark: &str, inner: &str) -> String {
    let t = inner.trim();
    if t.is_empty() { return inner.to_string(); }
    let lead = &inner[..inner.len() - inner.trim_start().len()];
    let trail = &inner[inner.trim_end().len()..];
    format!("{lead}{mark}{t}{mark}{trail}")
}

fn code_span(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') || text.starts_with(' ') && text.ends_with(' ') && !text.trim().is_empty() { " " } else { "" };
    format!("{fence}{pad}{text}{pad}{fence}")
}

fn url_escape(u: &str) -> String { u.replace(' ', "%20").replace('(', "%28").replace(')', "%29") }

fn inline(e: &El, depth: usize) -> String {
    let kids = || inline_nodes(&e.kids, depth);
    match e.name.as_str() {
        "strong" | "b" => wrap("**", &kids()),
        "em" | "i" | "cite" | "var" => wrap("_", &kids()),
        "del" | "s" | "strike" => wrap("~~", &kids()),
        "mark" => wrap("==", &kids()),
        "code" => code_span(&decode_entities(&serialize_text(&e.kids))),
        "br" => "  \n".into(),
        "a" => {
            let text = kids();
            match e.attr_dec("href").filter(|h| !h.is_empty()) {
                Some(h) if !text.trim().is_empty() => format!("[{}]({})", text.trim(), url_escape(&h)),
                Some(h) => format!("<{}>", h),
                None => text,
            }
        }
        "img" => {
            if e.has_attr("width") || e.has_attr("height") { return serialize(&[Node::El(e.clone())]); }
            let src = e.attr_dec("src").unwrap_or_default();
            if src.is_empty() { return String::new(); }
            let alt = escape_md(&e.attr_dec("alt").unwrap_or_default());
            let title = e.attr_dec("title").filter(|t| !t.is_empty()).map(|t| format!(" \"{}\"", t.replace('"', "\\\""))).unwrap_or_default();
            format!("![{alt}]({}{title})", url_escape(&src))
        }
        "input" | "en-media" => String::new(),
        "span" => {
            if has_class(e, "math-tex") {
                let t = decode_entities(&serialize_text(&e.kids));
                let t = t.trim();
                if let Some(f) = t.strip_prefix("\\[").and_then(|x| x.strip_suffix("\\]")) { return format!("\n\n$${}$$\n\n", f.trim()); }
                if let Some(f) = t.strip_prefix("\\(").and_then(|x| x.strip_suffix("\\)")) { return format!("${}$", f.trim()); }
                return format!("${t}$");
            }
            let style = e.attr_dec("style").unwrap_or_default();
            if style.contains(&format!("background-color:{HIGHLIGHT_BG}")) && !style.contains("color:") || style.replace(' ', "").starts_with(&format!("background-color:{HIGHLIGHT_BG};--tn-background:{HIGHLIGHT_BG};")) {
                return wrap("==", &kids());
            }
            if style.is_empty() && !e.has_attr("class") { return kids(); }
            if has_class(e, "tn-icon") { return String::new(); }
            raw_with_md_kids(e, depth)
        }
        "u" | "sup" | "sub" | "kbd" | "small" | "big" | "ins" | "q" | "abbr" | "samp" | "font" => raw_with_md_kids(e, depth),
        _ => kids(),
    }
}

fn raw_with_md_kids(e: &El, depth: usize) -> String {
    let mut open = El { kids: vec![], ..e.clone() };
    open.self_close = false;
    let tag = serialize(&[Node::El(open)]);
    let inner = tag.rsplit_once("</").map_or(tag.clone(), |(a, _)| a.to_string());
    format!("{inner}{}</{}>", inline_nodes(&e.kids, depth), e.name)
}

fn serialize_text(nodes: &[Node]) -> String { crate::htmltree::text_content(nodes) }

fn block(e: &El, depth: usize, out: &mut Vec<String>) {
    match e.name.as_str() {
        "p" => {
            let t = tidy_spaces(&inline_nodes(&e.kids, depth));
            let t = t.trim().trim_matches('\u{a0}');
            if !t.is_empty() { out.push(t.to_string()); }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let n = e.name[1..].parse::<usize>().unwrap_or(1);
            let t = inline_nodes(&e.kids, depth);
            let t = t.trim();
            if !t.is_empty() { out.push(format!("{} {}", "#".repeat(n), t.replace('\n', " "))); }
        }
        "hr" => out.push("---".into()),
        "pre" => out.push(code_block(e)),
        "blockquote" => {
            let inner = blocks(&e.kids, depth).join("\n\n");
            if !inner.trim().is_empty() { out.push(quote(&inner)); }
        }
        "aside" if has_class(e, "admonition") => {
            let kind = ["note", "tip", "important", "caution", "warning"].iter().find(|k| has_class(e, k)).copied().unwrap_or("note").to_uppercase();
            let inner = blocks(&e.kids, depth).join("\n\n");
            out.push(quote(&format!("[!{kind}]\n{inner}")));
        }
        "ul" | "ol" => out.push(list(e, depth)),
        "table" => out.push(table(e, depth)),
        "figure" => {
            if let Some(t) = e.child_els().find(|c| c.name == "table") { out.push(table(t, depth)); }
            else { out.extend(blocks(&e.kids, depth)); }
        }
        "details" => out.push(details(e, depth)),
        "section" if has_class(e, "include-note") || has_class(e, "link-embed") => out.push(serialize(&[Node::El(e.clone())])),
        "script" | "style" | "head" | "title" | "meta" | "link" => {}
        "li" | "dd" | "dt" | "div" | "section" | "article" | "main" | "body" | "html" | "span" | "header" | "footer" | "nav" | "center" | "dl" | "figcaption" | "thead" | "tbody" | "tr" | "td" | "th" => {
            out.extend(blocks(&e.kids, depth));
        }
        _ => out.push(serialize(&[Node::El(e.clone())])),
    }
}

fn quote(inner: &str) -> String {
    inner.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

fn details(e: &El, depth: usize) -> String {
    let summary = e.child_els().find(|c| c.name == "summary").map(|s| inline_nodes(&s.kids, depth).trim().to_string()).unwrap_or_default();
    let body_nodes: Vec<Node> = e.kids.iter().filter(|n| !matches!(n, Node::El(c) if c.name == "summary")).cloned().collect();
    let body = blocks(&body_nodes, depth).join("\n\n");
    let open = if e.has_attr("open") { " open" } else { "" };
    let class = e.attr_dec("class").filter(|c| !c.is_empty()).map(|c| format!(" class=\"{}\"", escape_attr(&c))).unwrap_or_default();
    format!("<details{class}{open}>\n<summary>{summary}</summary>\n\n{body}\n\n</details>")
}

fn md_lang(class: &str) -> String {
    let Some(l) = class.split_whitespace().find_map(|c| c.strip_prefix("language-")) else { return String::new() };
    if l == "text-x-trilium-auto" || l.is_empty() { return String::new(); }
    if l == "mermaid" { return l.into(); }
    if l.starts_with("application-javascript-env") { return "javascript".into(); }
    // find the markdown name whose mime normalises to this class
    for (name, mime) in crate::formats::LANGS {
        let norm = Regex::new(r"[\W_]+").unwrap().replace_all(&mime.to_lowercase(), "-").into_owned();
        if norm == l && !matches!(*name, "clike" | "md" | "js" | "py" | "ts" | "yml" | "zsh" | "jsonc") { return name.to_string(); }
    }
    l.rsplit('-').next().unwrap_or("").to_string()
}

fn code_block(pre: &El) -> String {
    let code = pre.child_els().find(|c| c.name == "code");
    let lang = code.map(|c| md_lang(&c.attr_dec("class").unwrap_or_default())).unwrap_or_default();
    let text = code.map_or_else(|| pre.text(), |c| c.text());
    let longest = text.lines().map(|l| l.trim_start().chars().take_while(|c| *c == '`').count()).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{lang}\n{text}\n{fence}")
}

fn list(e: &El, depth: usize) -> String {
    let todo = has_class(e, "todo-list");
    let ordered = e.name == "ol";
    let start = e.attr_dec("start").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
    let mut items: Vec<String> = vec![];
    let mut loose = false;
    for (i, li) in e.child_els().filter(|c| c.name == "li").enumerate() {
        let is_todo_item = todo || li.child_els().any(|c| c.name == "label" && has_class(c, "todo-list__label"));
        let marker = if is_todo_item { "- ".to_string() } else if ordered { format!("{:<4}", format!("{}.", start + i)) } else { "*   ".to_string() };
        let mut checkbox = String::new();
        let mut kids: Vec<Node> = vec![];
        if todo || li.child_els().any(|c| c.name == "label" && has_class(c, "todo-list__label")) {
            for k in &li.kids {
                match k {
                    Node::El(l) if l.name == "label" && has_class(l, "todo-list__label") => {
                        let checked = l.child_els().any(|c| c.name == "input" && c.has_attr("checked"));
                        checkbox = if checked { "[x] " } else { "[ ] " }.to_string();
                        if let Some(d) = l.child_els().find(|c| has_class(c, "todo-list__label__description")) { kids.extend(d.kids.clone()); }
                    }
                    other => kids.push(other.clone()),
                }
            }
        } else { kids = li.kids.clone(); }
        let parts = blocks(&kids, depth);
        if parts.iter().filter(|p| !p.starts_with("- ") && !p.starts_with("*   ") && !p.starts_with(|c: char| c.is_ascii_digit())).count() > 1 { loose = true; }
        let mut text = String::new();
        let pad = " ".repeat(marker.len());
        for (j, p) in parts.iter().enumerate() {
            let sep = if j == 0 { String::new() } else if loose || !(p.starts_with("- ") || p.starts_with("*   ") || p.chars().next().is_some_and(|c| c.is_ascii_digit()) && p.contains(". ")) { "\n\n".to_string() } else { "\n".to_string() };
            let indented = p.lines().enumerate().map(|(k, l)| if (j == 0 && k == 0) || l.is_empty() { l.to_string() } else { format!("{pad}{l}") }).collect::<Vec<_>>().join("\n");
            let indented = if j > 0 { indented.lines().map(|l| if l.is_empty() { String::new() } else if l.starts_with(&pad) { l.to_string() } else { format!("{pad}{l}") }).collect::<Vec<_>>().join("\n") } else { indented };
            text.push_str(&sep);
            text.push_str(&indented);
        }
        items.push(format!("{marker}{checkbox}{text}"));
    }
    items.join("\n")
}

fn cell_text(c: &El, depth: usize) -> String {
    inline_nodes(&c.kids, depth).trim().replace('|', "\\|").replace("  \n", "<br>").replace('\n', "<br>")
}

fn table(t: &El, depth: usize) -> String {
    let mut rows: Vec<(bool, Vec<&El>)> = vec![];
    fn collect<'a>(e: &'a El, in_head: bool, rows: &mut Vec<(bool, Vec<&'a El>)>) {
        for c in e.child_els() {
            match c.name.as_str() {
                "thead" => collect(c, true, rows),
                "tbody" | "tfoot" => collect(c, false, rows),
                "tr" => rows.push((in_head, c.child_els().filter(|x| x.name == "td" || x.name == "th").collect())),
                _ => {}
            }
        }
    }
    collect(t, false, &mut rows);
    let header_ok = rows.first().is_some_and(|(h, cells)| (*h || cells.iter().all(|c| c.name == "th")) && !cells.is_empty());
    let simple = rows.iter().all(|(_, cells)| cells.iter().all(|c| !c.has_attr("colspan") && !c.has_attr("rowspan")));
    if !header_ok || !simple || rows.is_empty() { return serialize(&[Node::El(t.clone())]); }
    let width = rows[0].1.len();
    let line = |cells: &[&El]| {
        let mut v: Vec<String> = cells.iter().map(|c| cell_text(c, depth)).collect();
        v.resize(width, String::new());
        format!("| {} |", v.join(" | "))
    };
    let mut out = vec![line(&rows[0].1), format!("| {} |", vec!["---"; width].join(" | "))];
    for (_, cells) in rows.iter().skip(1) { out.push(line(cells)); }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdhtml::md_to_html;

    fn md(h: &str) -> String { html_to_markdown(h) }

    #[test]
    fn basic_blocks_and_inlines() {
        assert_eq!(md("<h2>Title</h2><p>Hello <strong>bold</strong> and <i>it</i> <a href=\"https://x.org/a b\">link</a> <code>a`b</code></p><hr><p>*star* _u_</p>"),
            "## Title\n\nHello **bold** and _it_ [link](https://x.org/a%20b) ``a`b``\n\n---\n\n\\*star\\* \\_u\\_\n");
        assert_eq!(md("<p>a<br>b</p><p>&nbsp;</p><p><s>old</s> <mark>m</mark> <u>u</u> x<sup>2</sup></p>"), "a  \nb\n\n~~old~~ ==m== <u>u</u> x<sup>2</sup>\n");
        assert_eq!(md("<p>a &lt;b&gt; &amp;amp; &lt;/x</p>"), "a &lt;b> &amp;amp; &lt;/x\n");
        assert_eq!(md("<p>1 &lt; 2 &amp; <b> spaced </b>.</p><p>- not a list</p>"), "1 < 2 & **spaced** .\n\n\\- not a list\n");
    }

    #[test]
    fn lists_code_tables_quotes() {
        let h = r#"<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul><ol start="3"><li>x</li><li>y</li></ol>
            <ul class="todo-list"><li><label class="todo-list__label"><input type="checkbox" checked="checked" disabled="disabled"><span class="todo-list__label__description">done</span></label></li><li><label class="todo-list__label"><input type="checkbox" disabled="disabled"><span class="todo-list__label__description">open</span></label></li></ul>
            <pre><code class="language-text-x-python">print(1)
```
</code></pre><pre><code class="language-text-x-trilium-auto">plain</code></pre>
            <figure class="table"><table><thead><tr><th>a</th><th>b|c</th></tr></thead><tbody><tr><td>1</td><td><b>2</b></td></tr></tbody></table></figure>
            <blockquote><p>q1</p><p>q2</p></blockquote><aside class="admonition tip"><p>be nice</p></aside>"#;
        let m = md(h);
        for want in ["*   a\n    *   b\n*   c", "3.  x\n4.  y", "- [x] done\n- [ ] open", "````python\nprint(1)\n```\n\n````", "```\nplain\n```", "| a | b\\|c |\n| --- | --- |\n| 1 | **2** |", "> q1\n>\n> q2", "> [!TIP]\n> be nice"] {
            assert!(m.contains(want), "missing {want:?}\n{m}");
        }
        // a table without a header row keeps its HTML
        assert!(md("<table><tbody><tr><td>x</td></tr></tbody></table>").starts_with("<table>"));
    }

    #[test]
    fn math_images_and_html_fallbacks() {
        let m = md(r#"<p>Euler <span class="math-tex">\(e^{i\pi}\)</span></p><p><img src="a b.png" alt="pic" title="T"> <img src="s.png" width="100"></p><details class="trilium-collapsible"><summary>Sum</summary><p>body</p></details><span style="color:red">r</span>"#);
        for want in ["Euler $e^{i\\pi}$", "![pic](a%20b.png \"T\")", "<img src=\"s.png\" width=\"100\">", "<details class=\"trilium-collapsible\">\n<summary>Sum</summary>\n\nbody\n\n</details>", "<span style=\"color:red\">r</span>"] {
            assert!(m.contains(want), "missing {want:?}\n{m}");
        }
    }

    #[test]
    fn markdown_round_trips_through_html() {
        let src = "Intro paragraph with **bold**, _emphasis_ and `code`.\n\n## Heading\n\n- one\n- two\n  - nested\n\n- [x] done\n- [ ] todo\n\n```rust\nfn main() {}\n```\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n> [!NOTE]\n> remember\n\nSee [a link](https://example.org) and ![img](pic.png).\n";
        let once = md(&md_to_html(src, "t", false));
        let twice = md(&md_to_html(&once, "t", false));
        assert_eq!(once, twice, "stable after one round trip\n{once}");
        for want in ["**bold**", "_emphasis_", "## Heading", "- nested".trim_start_matches("- "), "- [x] done", "```rust", "| a | b |", "> [!NOTE]", "[a link](https://example.org)", "![img](pic.png)"] {
            assert!(once.contains(want), "missing {want:?}\n{once}");
        }
    }

    #[test]
    fn text_that_looks_like_markdown_is_escaped() {
        let m = md("<p>line1<br>2. x</p><p>3) y</p><p>a ==x== ~~y~~ $z$</p><p>---</p><p>- dash</p><p>http://a.com/x_y_z</p>");
        for want in ["line1  \n2\\. x", "3\\) y", "a \\=\\=x\\=\\= \\~\\~y\\~\\~ \\$z\\$", "\\---", "\\- dash", "http://a.com/x\\_y\\_z"] {
            assert!(m.contains(want), "missing {want:?}\n{m}");
        }
        // and it all reads back as the same text
        let back = md(&crate::mdhtml::md_to_html(&m, "t", false));
        for plain in ["2. x", "3) y", "==x==", "~~y~~", "$z$", "- dash", "http://a.com/x_y_z"] {
            assert!(back.replace('\\', "").contains(plain) || crate::htmltree::text_content(&crate::htmltree::parse(&crate::mdhtml::md_to_html(&m, "t", false))).contains(plain), "{plain}\n{back}");
        }
        assert!(!crate::mdhtml::md_to_html(&m, "t", false).contains("<hr"), "a paragraph of dashes stays a paragraph");
    }

    #[test]
    fn hostile_html_does_not_panic() {
        let deep = "<ul><li>".repeat(2000) + "x" + &"</li></ul>".repeat(2000);
        let _ = md(&deep);
        let _ = md(&"<div>".repeat(5000));
        let _ = md("<table><tr><td colspan=3>é</td></tr></table><ul><li><li></ul><pre>\u{0}</pre><a>");
    }
}
