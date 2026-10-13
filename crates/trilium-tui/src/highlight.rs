//! Syntax highlighting for code notes and for the HTML source view.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use std::str::FromStr;
use syntect::{
    easy::HighlightLines,
    highlighting::{Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

/// Sentinel marking "ANSI palette color N" inside a syntect colour (syntect only knows RGB).
const ANSI_MARK_G: u8 = 255;
const ANSI_MARK_B: u8 = 254;

fn ansi(n: u8) -> SynColor { SynColor { r: n, g: ANSI_MARK_G, b: ANSI_MARK_B, a: 255 } }

/// Sentinel for "the terminal's own foreground" (unscoped text must not become syntect's default black).
const DEFAULT_FG: SynColor = SynColor { r: 0, g: 255, b: 253, a: 255 };

fn ansi_to_ratatui(c: SynColor) -> Color {
    if c == DEFAULT_FG { return Color::Reset; }
    if c.g == ANSI_MARK_G && c.b == ANSI_MARK_B { Color::Indexed(c.r) } else { Color::Rgb(c.r, c.g, c.b) }
}

/// A theme made only of terminal palette colours, so highlighting follows the terminal theme
/// (Omarchy changes the palette; nothing here needs to know which theme is active).
fn palette_theme() -> Theme {
    let item = |scope: &str, color: u8, style: Option<FontStyle>| ThemeItem {
        scope: ScopeSelectors::from_str(scope).unwrap(),
        style: StyleModifier { foreground: Some(if color == 0xFF { DEFAULT_FG } else { ansi(color) }), background: None, font_style: style },
    };
    Theme {
        name: Some("terminal-palette".into()),
        author: None,
        settings: ThemeSettings { foreground: Some(DEFAULT_FG), ..ThemeSettings::default() },
        scopes: vec![
            item("comment, punctuation.definition.comment", 8, Some(FontStyle::ITALIC)),
            item("string, markup.raw, punctuation.definition.string", 2, None),
            item("constant.numeric, constant.language, constant.character, constant.other", 3, None),
            item("keyword, storage, keyword.operator.word", 5, None),
            item("entity.name.function, support.function, variable.function, meta.function-call", 4, None),
            item("entity.name.type, entity.name.class, support.class, support.type, entity.other.inherited-class", 6, None),
            item("entity.name.tag, markup.heading, entity.name.section", 1, Some(FontStyle::BOLD)),
            item("entity.other.attribute-name, variable.parameter", 3, None),
            item("markup.bold", 0xFF, Some(FontStyle::BOLD)),
            item("markup.italic", 0xFF, Some(FontStyle::ITALIC)),
            item("markup.underline.link, string.other.link", 4, Some(FontStyle::UNDERLINE)),
            item("invalid", 1, None),
        ],
    }
}

/// Beyond this size highlighting costs more than it helps in a terminal.
const MAX_HIGHLIGHT_BYTES: usize = 400_000;

fn token_for_mime(mime: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    let table: &[(&str, &str)] = &[
        ("javascript", "js"), ("typescript", "js"), ("json", "json"), ("python", "py"), ("rust", "rs"),
        ("x-sh", "sh"), ("shell", "sh"), ("html", "html"), ("css", "css"), ("xml", "xml"), ("sql", "sql"),
        ("markdown", "md"), ("yaml", "yaml"), ("x-c++", "cpp"), ("x-csrc", "c"), ("x-java", "java"),
        ("x-go", "go"), ("x-ruby", "rb"), ("x-php", "php"), ("x-lua", "lua"), ("x-perl", "pl"),
        ("x-diff", "diff"), ("x-haskell", "hs"), ("x-csharp", "cs"),
    ];
    table.iter().find(|(k, _)| m.contains(k)).map(|(_, v)| *v).unwrap_or("txt")
}

pub fn highlight(text: &str, mime: &str) -> Vec<Line<'static>> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return text.lines().map(|l| Line::from(l.to_string())).collect();
    }
    let ss = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme = THEME.get_or_init(palette_theme);
    let syntax = ss.find_syntax_by_token(token_for_mime(mime)).unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(text) {
        let spans = match h.highlight_line(line, ss) {
            Ok(r) => r.into_iter().map(|(st, s)| {
                let mut style = Style::default().fg(ansi_to_ratatui(st.foreground));
                if st.font_style.contains(FontStyle::BOLD) { style = style.add_modifier(Modifier::BOLD); }
                if st.font_style.contains(FontStyle::ITALIC) { style = style.add_modifier(Modifier::ITALIC); }
                Span::styled(s.trim_end_matches(['\n', '\r']).replace('\t', "    "), style)
            }).collect(),
            Err(_) => vec![Span::raw(line.trim_end().to_string())],
        };
        out.push(Line::from(spans));
    }
    out
}

/// Hard-wrap styled lines to `width` columns (no word logic: this is for source text).
pub fn hard_wrap(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(8);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let total: usize = line.spans.iter().map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref())).sum();
        if total <= width { out.push(line); continue; }
        let (mut cur, mut w): (Vec<Span<'static>>, usize) = (vec![], 0);
        for span in line.spans {
            let mut buf = String::new();
            for ch in span.content.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > width {
                    if !buf.is_empty() { cur.push(Span::styled(std::mem::take(&mut buf), span.style)); }
                    out.push(Line::from(std::mem::take(&mut cur)));
                    w = 0;
                }
                buf.push(ch);
                w += cw;
            }
            if !buf.is_empty() { cur.push(Span::styled(buf, span.style)); }
        }
        out.push(Line::from(cur));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_text_uses_the_terminal_foreground() {
        for mime in ["text/plain", "text/html", "text/x-rust"] {
            let lines = highlight("hello world\nlet x = 1;", mime);
            for sp in lines.iter().flat_map(|l| l.spans.iter()) {
                assert!(!matches!(sp.style.fg, Some(Color::Rgb(..))), "{mime}: {:?}", sp.style.fg);
            }
        }
    }

    #[test]
    fn highlights_and_wraps() {
        let l = highlight("fn main() {\n    println!(\"hi\");\n}\n", "text/x-rustsrc");
        assert_eq!(l.len(), 3);
        assert!(l[0].spans.len() > 1, "rust should be tokenised");
        let kw = l[0].spans.iter().find(|s| s.content.trim() == "fn").expect("keyword span");
        assert_eq!(kw.style.fg, Some(Color::Indexed(5)), "keywords use the terminal's magenta, not a fixed RGB");
        let w = hard_wrap(vec![Line::from("x".repeat(25))], 10);
        assert_eq!(w.len(), 3);
    }
}
