//! HTML (CKEditor output as stored by Trilium) -> wrapped, styled terminal lines.
//! This is a *view* only: the stored HTML is never modified by rendering.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use scraper::{ElementRef, Html, Node};
use unicode_width::UnicodeWidthStr;

enum Prefix {
    Quote,
    Indent(usize),
}

struct Word {
    pieces: Vec<(String, Style)>,
    width: usize,
}

/// A hyperlink found while rendering.
#[derive(Clone, Debug)]
pub struct Link { pub href: String, pub text: String, pub image: bool }

/// Rendered lines plus the links in them: `span_links[line][span]` is the link a span belongs to.
pub struct Rendered { pub lines: Vec<Line<'static>>, pub links: Vec<Link>, pub span_links: Vec<Vec<Option<u32>>> }

/// A link's id travels through wrapping inside the span style's (otherwise unused) underline colour and
/// is stripped again in `finish`, so the layout code needs no link-specific plumbing.
fn encode_id(id: u32) -> Color { Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8) }
fn decode_id(c: Color) -> Option<u32> { if let Color::Rgb(r, g, b) = c { Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32) } else { None } }

struct Renderer<'a> {
    links: Vec<Link>,
    width: usize,
    lines: Vec<Line<'static>>,
    runs: Vec<(String, Style)>,
    prefix: Vec<Prefix>,
    lists: Vec<(bool, usize)>,
    marker: Option<String>,
    resolver: &'a dyn Fn(&str) -> Option<(String, String)>,
    depth: usize,
    /// element nesting (a note can nest tens of thousands of tags; the walk is recursive) and embedded notes rendered so far
    nest: usize,
    includes: usize,
}

/// Resolves an included note id to `(title, html)`; `None` leaves a placeholder.
pub type Resolver<'a> = &'a dyn Fn(&str) -> Option<(String, String)>;

#[cfg(test)]
pub fn render_html(html: &str, width: usize) -> Vec<Line<'static>> {
    render_html_with(html, width, &|_| None)
}

#[cfg(test)]
pub fn render_html_with(html: &str, width: usize, resolver: Resolver) -> Vec<Line<'static>> {
    render_html_full(html, width, resolver).lines
}

pub fn render_html_full(html: &str, width: usize, resolver: Resolver) -> Rendered {
    let doc = Html::parse_fragment(html);
    let mut r = Renderer { links: vec![], width: width.max(20), lines: vec![], runs: vec![], prefix: vec![], lists: vec![], marker: None, resolver, depth: 0, nest: 0, includes: 0 };
    for c in doc.root_element().children() {
        r.node(c, Style::default());
    }
    r.flush();
    while r.lines.last().is_some_and(|l| l.spans.is_empty()) { r.lines.pop(); }
    r.finish()
}

fn is_block(name: &str) -> bool {
    matches!(name, "p" | "div" | "section" | "figure" | "figcaption" | "article" | "header" | "footer" | "main" | "aside" | "details" | "summary"
        | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "li" | "blockquote" | "pre" | "table" | "hr" | "dl" | "dt" | "dd")
}

impl Renderer<'_> {
    fn node(&mut self, n: ego_tree::NodeRef<Node>, style: Style) {
        // deeper than any real document: the rest is left out rather than overflowing the stack
        if self.nest >= 200 { return; }
        self.nest += 1;
        self.node_inner(n, style);
        self.nest -= 1;
    }

    fn node_inner(&mut self, n: ego_tree::NodeRef<Node>, style: Style) {
        match n.value() {
            Node::Text(t) => self.text(t, style),
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(n) { self.element(el, style); }
            }
            _ => {}
        }
    }

    fn children(&mut self, el: ElementRef, style: Style) {
        for c in el.children() { self.node(c, style); }
    }

    fn text(&mut self, t: &str, style: Style) {
        let mut s = String::new();
        let mut prev_space = false;
        for ch in t.chars() {
            if ch.is_ascii_whitespace() {
                if !prev_space { s.push(' '); }
                prev_space = true;
            } else {
                s.push(ch);
                prev_space = false;
            }
        }
        if s == " " && self.runs.is_empty() { return; }
        if !s.is_empty() { self.runs.push((s, style)); }
    }

    fn element(&mut self, el: ElementRef, style: Style) {
        let name = el.value().name();
        match name {
            "script" | "style" | "head" | "template" => return,
            "br" => { self.flush_keep_blank(); return; }
            "hr" => {
                self.block_break();
                let w = self.avail();
                self.emit(vec![Span::styled("─".repeat(w), Style::default().fg(Color::DarkGray))]);
                self.blank();
                return;
            }
            "img" => {
                let alt = el.value().attr("alt").filter(|a| !a.is_empty())
                    .or_else(|| el.value().attr("src").and_then(|s| s.rsplit('/').next())).unwrap_or("image");
                let mut st = Style::default().fg(Color::Magenta);
                if let Some(src) = el.value().attr("src").filter(|s| !s.is_empty()) {
                    let id = self.links.len() as u32;
                    self.links.push(Link { href: src.to_string(), text: alt.to_string(), image: true });
                    st = st.underline_color(encode_id(id));
                }
                self.runs.push((format!("[image: {alt}]"), st));
                return;
            }
            "input" => {
                let checked = el.value().attr("checked").is_some();
                self.runs.push((if checked { "☑ " } else { "☐ " }.to_string(), style));
                return;
            }
            "pre" => { self.pre(el); return; }
            "table" => { self.table(el); return; }
            _ => {}
        }
        if matches!(name, "section" | "figure") && el.value().classes().any(|c| c == "include-note") {
            if let Some(id) = el.value().attr("data-note-id") { self.include_note(id); return; }
        }
        if name == "section" && el.value().classes().any(|c| c == "link-embed") {
            if let Some(url) = el.value().attr("data-url").filter(|u| !u.is_empty()) { self.link_embed(el, url); return; }
        }
        if is_block(name) { self.block_break(); }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let st = match name {
                    "h1" => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    "h2" => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    "h3" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::BOLD),
                };
                self.children(el, style.patch(st));
            }
            "ul" | "ol" => {
                self.lists.push((name == "ol", 0));
                self.children(el, style);
                self.lists.pop();
            }
            "li" => {
                let marker = match self.lists.last_mut() {
                    Some((true, n)) => { *n += 1; format!("{n}. ") }
                    _ => "• ".to_string(),
                };
                let todo = el.value().classes().any(|c| c == "todo-list__label") || el.children().any(|c| {
                    ElementRef::wrap(c).is_some_and(|e| e.value().name() == "label")
                });
                let marker = if todo { String::new() } else { marker };
                self.prefix.push(Prefix::Indent(marker.width()));
                self.marker = if marker.is_empty() { None } else { Some(marker) };
                self.children(el, style);
                self.flush();
                self.marker = None;
                self.prefix.pop();
            }
            "blockquote" => {
                self.prefix.push(Prefix::Quote);
                self.children(el, style.add_modifier(Modifier::ITALIC));
                self.flush();
                self.prefix.pop();
            }
            "strong" | "b" => self.children(el, style.add_modifier(Modifier::BOLD)),
            "em" | "i" => self.children(el, style.add_modifier(Modifier::ITALIC)),
            "u" => self.children(el, style.add_modifier(Modifier::UNDERLINED)),
            "s" | "del" | "strike" => self.children(el, style.add_modifier(Modifier::CROSSED_OUT)),
            "code" => self.children(el, style.fg(Color::Yellow)),
            "mark" => self.children(el, style.bg(Color::Yellow).fg(Color::Black)),
            "a" => {
                let mut st = style.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
                if let Some(href) = el.value().attr("href").filter(|h| !h.is_empty()) {
                    let id = self.links.len() as u32;
                    self.links.push(Link { href: href.to_string(), text: el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "), image: false });
                    st = st.underline_color(encode_id(id));
                }
                self.children(el, st);
            }
            _ => self.children(el, style),
        }
        if is_block(name) {
            self.flush();
            if !matches!(name, "li" | "ul" | "ol") || (matches!(name, "ul" | "ol") && self.lists.is_empty()) { self.blank(); }
        }
    }

    /// Link preview ("link-embed"): everything is in `data-*` attributes, so show title, description and URL as a card.
    fn link_embed(&mut self, el: ElementRef, url: &str) {
        self.flush();
        self.blank();
        let attr = |n: &str| el.value().attr(n).map(|v| v.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|v| !v.is_empty());
        let id = self.links.len() as u32;
        let title = attr("data-title").unwrap_or_else(|| url.to_string());
        self.links.push(Link { href: url.to_string(), text: title.clone(), image: false });
        let link = Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED).underline_color(encode_id(id));
        self.prefix.push(Prefix::Quote);
        self.runs.push(("🔗 ".to_string(), Style::default()));
        self.runs.push((title, link.add_modifier(Modifier::BOLD)));
        self.flush();
        if let Some(d) = attr("data-description") {
            self.runs.push((d, Style::default()));
            self.flush();
        }
        self.runs.push((url.to_string(), link.fg(Color::DarkGray)));
        self.flush();
        self.prefix.pop();
        self.blank();
    }

    /// Embedded note ("include note"): shown inline in a bar-prefixed box, nesting limited to 3.
    fn include_note(&mut self, id: &str) {
        self.flush();
        self.blank();
        let dim = Style::default().fg(Color::DarkGray);
        // nesting is limited to 3 and the whole page to 40 embedded notes (a note embedding itself many times is cubic)
        self.includes += 1;
        let found = if self.depth < 3 && self.includes <= 40 { (self.resolver)(id) } else { None };
        match found {
            Some((title, html)) => {
                self.prefix.push(Prefix::Quote);
                self.emit(vec![Span::styled(format!("▸ {title}"), Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan))]);
                self.depth += 1;
                let doc = Html::parse_fragment(&html);
                for c in doc.root_element().children() { self.node(c, Style::default()); }
                self.flush();
                self.depth -= 1;
                self.prefix.pop();
            }
            None => self.emit(vec![Span::styled(format!("[included note {id} unavailable]"), dim)]),
        }
        self.blank();
    }

    fn finish(mut self) -> Rendered {
        let mut span_links = Vec::with_capacity(self.lines.len());
        for line in &mut self.lines {
            let mut ids = Vec::with_capacity(line.spans.len());
            for span in &mut line.spans {
                let id = span.style.underline_color.and_then(decode_id);
                if id.is_some() { span.style.underline_color = None; }
                ids.push(id);
            }
            span_links.push(ids);
        }
        Rendered { lines: self.lines, links: self.links, span_links }
    }

    fn avail(&self) -> usize {
        let used: usize = self.prefix.iter().map(|p| match p { Prefix::Quote => 2, Prefix::Indent(n) => *n }).sum();
        self.width.saturating_sub(used).max(10)
    }

    fn prefix_spans(&mut self, first: bool) -> Vec<Span<'static>> {
        let last = self.prefix.len().saturating_sub(1);
        let mut out = Vec::new();
        for (i, p) in self.prefix.iter().enumerate() {
            match p {
                Prefix::Quote => out.push(Span::styled("│ ", Style::default().fg(Color::DarkGray))),
                Prefix::Indent(n) => match (first, i == last, self.marker.as_ref()) {
                    (true, true, Some(m)) => out.push(Span::styled(m.clone(), Style::default().fg(Color::DarkGray))),
                    _ => out.push(Span::raw(" ".repeat(*n))),
                },
            }
        }
        if first { self.marker = None; }
        out
    }

    fn emit(&mut self, spans: Vec<Span<'static>>) {
        let mut line = self.prefix_spans(true);
        line.extend(spans);
        self.lines.push(Line::from(line));
    }

    fn emit_cont(&mut self, spans: Vec<Span<'static>>, first: bool) {
        let mut line = self.prefix_spans(first);
        line.extend(spans);
        self.lines.push(Line::from(line));
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) { self.lines.push(Line::default()); }
    }

    fn block_break(&mut self) { self.flush(); }

    /// `<br>`: end the current line but keep paragraph flow (an empty run list yields a blank line).
    fn flush_keep_blank(&mut self) {
        if self.runs.is_empty() { self.lines.push(Line::default()); } else { self.flush(); }
    }

    fn flush(&mut self) {
        let runs = std::mem::take(&mut self.runs);
        if runs.iter().all(|(s, _)| s.trim().is_empty()) { return; }
        let words = words_of(&runs);
        let avail = self.avail();
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut cur_w = 0usize;
        let mut first = true;
        for w in words {
            let mut pieces = w.pieces;
            let mut ww = w.width;
            // hard-split words longer than a line
            while ww > avail {
                if cur_w > 0 { let l = std::mem::take(&mut cur); self.emit_cont(l, first); first = false; cur_w = 0; }
                let (head, tail) = split_pieces(pieces, avail);
                let hs: Vec<Span<'static>> = head.into_iter().map(|(s, st)| Span::styled(s, st)).collect();
                self.emit_cont(hs, first); first = false;
                ww = tail.iter().map(|(s, _)| s.width()).sum();
                pieces = tail;
            }
            if pieces.is_empty() { continue; }
            let need = ww + usize::from(cur_w > 0);
            if cur_w > 0 && cur_w + need > avail {
                let l = std::mem::take(&mut cur);
                self.emit_cont(l, first); first = false; cur_w = 0;
            }
            if cur_w > 0 { cur.push(Span::raw(" ")); cur_w += 1; }
            for (s, st) in pieces { cur.push(Span::styled(s, st)); }
            cur_w += ww;
        }
        if !cur.is_empty() { self.emit_cont(cur, first); }
    }

    fn pre(&mut self, el: ElementRef) {
        self.flush();
        let text: String = el.text().collect();
        let st = Style::default().fg(Color::Yellow);
        let avail = self.avail();
        for l in text.trim_end_matches('\n').split('\n') {
            let mut rest = l.replace('\t', "    ");
            loop {
                let (head, tail) = split_at_width(&rest, avail);
                self.emit(vec![Span::styled(head.to_string(), st)]);
                if tail.is_empty() { break; }
                rest = tail.to_string();
            }
        }
        self.blank();
    }

    fn table(&mut self, el: ElementRef) {
        self.flush();
        let sel_tr = scraper::Selector::parse("tr").unwrap();
        let sel_cell = scraper::Selector::parse("th,td").unwrap();
        let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
        for tr in el.select(&sel_tr) {
            // CKEditor emits <th> for row headers in body rows too, so only <thead> rows or
            // rows made entirely of <th> count as the table header.
            let in_thead = tr.parent().and_then(ElementRef::wrap).is_some_and(|p| p.value().name() == "thead");
            let mut all_th = true;
            let cells: Vec<String> = tr.select(&sel_cell).map(|c| {
                all_th &= c.value().name() == "th";
                c.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
            }).collect();
            let header = in_thead || all_th;
            if !cells.is_empty() { rows.push((header, cells)); }
        }
        let ncols = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
        if ncols == 0 { return; }
        let mut widths = vec![1usize; ncols];
        for (_, c) in &rows { for (i, s) in c.iter().enumerate() { widths[i] = widths[i].max(s.width()); } }
        let avail = self.avail();
        let sep_total = 3 * (ncols - 1);
        while widths.iter().sum::<usize>() + sep_total > avail {
            let (i, &m) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if m <= 4 { break; }
            widths[i] -= 1;
        }
        let bar = Style::default().fg(Color::DarkGray);
        for (header, cells) in rows {
            let mut spans = Vec::new();
            for i in 0..ncols {
                if i > 0 { spans.push(Span::styled(" │ ", bar)); }
                let s = cells.get(i).map(String::as_str).unwrap_or("");
                let mut t = split_at_width(s, widths[i]).0.to_string();
                if t.width() < s.width() && widths[i] > 1 { t = split_at_width(s, widths[i] - 1).0.to_string() + "…"; }
                let pad = widths[i].saturating_sub(t.width());
                let st = if header { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
                spans.push(Span::styled(format!("{t}{}", " ".repeat(pad)), st));
            }
            self.emit(spans);
            if header {
                let line: String = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─");
                self.emit(vec![Span::styled(line, bar)]);
            }
        }
        self.blank();
    }
}

fn words_of(runs: &[(String, Style)]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Option<Word> = None;
    for (text, st) in runs {
        for ch in text.chars() {
            if ch == ' ' {
                if let Some(w) = cur.take() { words.push(w); }
            } else {
                let w = cur.get_or_insert_with(|| Word { pieces: vec![], width: 0 });
                match w.pieces.last_mut() {
                    Some((s, ps)) if ps == st => s.push(ch),
                    _ => w.pieces.push((ch.to_string(), *st)),
                }
                w.width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    if let Some(w) = cur { words.push(w); }
    words
}

fn split_at_width(s: &str, max: usize) -> (&str, &str) {
    let mut w = 0;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max { return (&s[..i], &s[i..]); }
        w += cw;
    }
    (s, "")
}

fn split_pieces(pieces: Vec<(String, Style)>, max: usize) -> (Vec<(String, Style)>, Vec<(String, Style)>) {
    let (mut head, mut tail, mut used) = (vec![], vec![], 0usize);
    for (s, st) in pieces {
        if !tail.is_empty() { tail.push((s, st)); continue; }
        let (h, t) = split_at_width(&s, max - used);
        used += h.width();
        if !h.is_empty() { head.push((h.to_string(), st)); }
        if !t.is_empty() { tail.push((t.to_string(), st)); }
    }
    (head, tail)
}

pub fn is_markdown_mime(mime: &str) -> bool { matches!(mime, "text/markdown" | "text/x-markdown" | "text/x-gfm") }

/// Markdown source -> HTML for the terminal renderer. `[[noteId]]` wiki links become links to that note,
/// titled with `title_of` (the id when unknown).
pub fn markdown_to_html(src: &str, title_of: &dyn Fn(&str) -> Option<String>) -> String {
    use pulldown_cmark::{Options, Parser, html};
    static WIKI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let wiki = WIKI.get_or_init(|| regex::Regex::new(r"\[\[([A-Za-z0-9_]+)\]\]").unwrap());
    let prepared = wiki.replace_all(src, |c: &regex::Captures| {
        let title = title_of(&c[1]).unwrap_or_else(|| c[1].to_string());
        format!("[{}](#root/{})", title.replace(['[', ']'], ""), &c[1])
    });
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(&prepared, opts));
    out
}

/// Rendered text as plain lines (used for diffs).
pub fn plain_lines(html: &str, width: usize) -> Vec<String> {
    render_html_full(html, width, &|_| None).lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
}

#[cfg(test)]
pub fn plain(lines: &[Line]) -> String {
    lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_inline_and_wrapping() {
        let out = plain(&render_html("<p>Hello <strong>bold</strong>world and <em>more</em></p><p>second</p>", 80));
        assert_eq!(out, "Hello boldworld and more\n\nsecond");
        let wrapped = plain(&render_html("<p>aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo ppp</p>", 20));
        assert!(wrapped.lines().all(|l| l.width() <= 20), "{wrapped}");
        assert!(wrapped.lines().count() >= 3);
    }

    #[test]
    fn lists_quotes_and_pre() {
        let out = plain(&render_html("<ul><li>one</li><li>two<ol><li>x</li><li>y</li></ol></li></ul><blockquote><p>q</p></blockquote><pre>a\n  b</pre>", 40));
        assert!(out.contains("• one"), "{out}");
        assert!(out.contains("• two"), "{out}");
        assert!(out.contains("  1. x") && out.contains("  2. y"), "{out}");
        assert!(out.contains("│ q"), "{out}");
        assert!(out.contains("  b"), "{out}");
    }

    #[test]
    fn tables_headings_todo_and_images() {
        let out = plain(&render_html("<h2>Title</h2><table><thead><tr><th>A</th><th>B</th></tr></thead><tbody><tr><td>1</td><td>22</td></tr></tbody></table><ul class=\"todo-list\"><li><label><input type=\"checkbox\" checked disabled><span>done</span></label></li></ul><img src=\"x/pic.png\">", 40));
        assert!(out.contains("Title"), "{out}");
        assert!(out.contains("A │ B") && out.contains("1 │ 22"), "{out}");
        assert!(out.contains("☑ done"), "{out}");
        assert!(out.contains("[image: pic.png]"), "{out}");
    }

    #[test]
    fn row_headers_in_body_do_not_become_header_rows() {
        let out = plain(&render_html("<table><tbody><tr><th>r1</th><td>a</td></tr><tr><th>r2</th><td>b</td></tr></tbody></table>", 40));
        assert!(!out.contains('─'), "{out}");
    }

    #[test]
    fn include_note_embeds_inline_and_limits_depth() {
        let resolver = |id: &str| match id {
            "abc" => Some(("Current Focus".to_string(), "<p>ship the TUI</p><section class=\"include-note\" data-note-id=\"abc\"></section>".to_string())),
            _ => None,
        };
        let out = plain(&render_html_with("<p>before</p><section class=\"include-note\" data-note-id=\"abc\" data-box-size=\"expandable\"></section><p>after</p><section class=\"include-note\" data-note-id=\"zzz\"></section>", 60, &resolver));
        assert!(out.contains("before") && out.contains("after"), "{out}");
        assert!(out.contains("│ ▸ Current Focus") && out.contains("│ ship the TUI"), "{out}");
        assert!(out.matches("Current Focus").count() <= 4, "self-inclusion must terminate: {out}");
        assert!(out.contains("[included note zzz unavailable]"), "{out}");
    }

    #[test]
    fn links_are_collected_with_their_spans() {
        let r = render_html_full("<p>see <a href=\"#root/a/b/NOTE1\">the <b>first</b> link</a> and <a href=\"https://x.y/z\">web</a></p>", 60, &|_| None);
        assert_eq!(r.links.iter().map(|l| (l.href.as_str(), l.text.as_str())).collect::<Vec<_>>(), [("#root/a/b/NOTE1", "the first link"), ("https://x.y/z", "web")]);
        let ids: Vec<Option<u32>> = r.span_links[0].clone();
        assert!(ids.contains(&Some(0)) && ids.contains(&Some(1)) && ids.contains(&None));
        assert!(r.lines[0].spans.iter().all(|s| s.style.underline_color.is_none()), "encoding must not leak into the display");
    }

    #[test]
    fn link_embed_shows_title_description_and_url() {
        let html = "<p>intro</p><section class=\"link-embed\" data-url=\"https://x.org/a?b=1\" data-embed-type=\"opengraph\" data-title=\"The Title\" data-description=\"Some text\">&nbsp;</section><p>after</p>";
        let r = render_html_full(html, 60, &|_| None);
        let text = plain(&r.lines);
        assert!(text.contains("The Title") && text.contains("Some text") && text.contains("https://x.org/a?b=1") && text.contains("after"), "{text}");
        assert_eq!(r.links.len(), 1);
        assert_eq!(r.links[0].href, "https://x.org/a?b=1");
    }

    #[test]
    fn markdown_renders_through_the_same_pipeline() {
        let html = markdown_to_html("# Title\n\nsome **bold** and `code`, a [link](https://x.y)\n\n- one\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nsee [[abc123]] and [[zzz]]", &|id| (id == "abc123").then(|| "Other note".to_string()));
        let r = render_html_full(&html, 60, &|_| None);
        let text = plain(&r.lines);
        assert!(text.contains("Title") && text.contains("some bold and code") && text.contains("• one") && text.contains("☑ done") && text.contains("a │ b"), "{text}");
        assert_eq!(r.links.iter().map(|l| (l.text.as_str(), l.href.as_str())).collect::<Vec<_>>(),
            [("link", "https://x.y"), ("Other note", "#root/abc123"), ("zzz", "#root/zzz")]);
    }

    #[test]
    fn long_words_are_split_and_nothing_overflows() {
        let out = plain(&render_html(&format!("<p>{}</p>", "x".repeat(100)), 30));
        assert!(out.lines().all(|l| l.width() <= 30));
        assert_eq!(out.replace('\n', "").len(), 100);
    }
}
