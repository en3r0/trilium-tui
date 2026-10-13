//! Port of `search/services/lex.ts`: splits a query into the full-text part and the expression part.
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tok {
    pub token: String,
    pub in_quotes: bool,
    pub start: isize,
    pub end: isize,
}

#[derive(Debug, Default)]
pub struct Lexed {
    pub fulltext_query: String,
    pub fulltext_tokens: Vec<Tok>,
    pub expression_tokens: Vec<Tok>,
    pub leading_operator: String,
}

fn is_op(c: char) -> bool { "=*><!-+%,".contains(c) }

fn group_start(rest: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\(+\s*(#|~|note\.)").unwrap()).is_match(rest)
}

pub fn lex(input: &str) -> Lexed {
    let mut s: Vec<char> = input.to_lowercase().chars().collect();
    let mut leading_operator = String::new();
    if s.len() > 1 && s[0] == '=' && s[1] != '=' && !s[1].is_whitespace() {
        leading_operator = "=".into();
        s.remove(0);
    }
    let mut fulltext_query = String::new();
    let mut fulltext_query_before_last = String::new();
    let mut fulltext_tokens: Vec<Tok> = vec![];
    let mut expression_tokens: Vec<Tok> = vec![];
    let mut quotes: Option<char> = None;
    let mut fulltext_ended = false;
    let mut word = String::new();

    macro_rules! finish {
        ($end:expr, $also_empty:expr) => {{
            if !(word.is_empty() && !$also_empty) {
                let end: isize = $end;
                let len = word.chars().count() as isize;
                let rec = Tok { token: std::mem::take(&mut word), in_quotes: quotes.is_some(), start: end - len + 1, end };
                if fulltext_ended {
                    expression_tokens.push(rec);
                } else {
                    fulltext_tokens.push(rec);
                    fulltext_query_before_last = fulltext_query.clone();
                    let upto = ((end + 1).max(0) as usize).min(s.len());
                    fulltext_query = s[..upto].iter().collect();
                }
                word.clear();
            }
        }};
    }

    let mut i = 0usize;
    while i < s.len() {
        let chr = s[i];
        if chr == '\\' {
            if i + 1 < s.len() { i += 1; word.push(s[i]); } else { word.push(chr); }
            i += 1;
            continue;
        } else if matches!(chr, '"' | '\'' | '`') {
            match quotes {
                None => {
                    let prev_is_op = word.chars().last().is_some_and(is_op);
                    if word.is_empty() || prev_is_op {
                        finish!(i as isize - 1, false);
                        quotes = Some(chr);
                    } else {
                        word.push(chr); // d'Artagnan stays one token
                    }
                }
                Some(q) if q == chr => {
                    finish!(i as isize - 1, true);
                    quotes = None;
                }
                Some(_) => word.push(chr),
            }
            i += 1;
            continue;
        } else if quotes.is_none() {
            if !fulltext_ended && word == "note" && chr == '.' && i + 1 < s.len() { fulltext_ended = true; }
            if !fulltext_ended && word.is_empty() && chr == '(' {
                let rest: String = s[i..].iter().collect();
                if group_start(&rest) { fulltext_ended = true; }
            }
            if chr == '#' || chr == '~' {
                if !fulltext_ended && !word.is_empty() { word.push(chr); i += 1; continue; }
                if !fulltext_ended { fulltext_ended = true; } else { finish!(i as isize - 1, false); }
                word = chr.to_string();
                i += 1;
                continue;
            } else if (word == "#" || word == "~") && chr == '!' {
                word.push(chr); i += 1; continue;
            } else if word == "~" && (chr == '=' || chr == '*') {
                word.push(chr); i += 1; continue;
            } else if chr.is_whitespace() {
                finish!(i as isize - 1, false); i += 1; continue;
            } else if fulltext_ended && matches!(chr, '(' | ')' | '.') {
                finish!(i as isize - 1, false);
                word.push(chr);
                finish!(i as isize, false);
                i += 1;
                continue;
            } else if fulltext_ended && word != "#!" && word != "~!" && word.chars().last().is_some_and(is_op) != is_op(chr) && !word.is_empty() {
                // isPreviousSymbolAnOperator() is false for an empty word, which differs from is_op(chr) only when chr is an operator
                finish!(i as isize - 1, false);
                word.push(chr);
                i += 1;
                continue;
            } else if fulltext_ended && word.is_empty() && is_op(chr) {
                // empty word vs operator char: JS compares false !== true and "finishes" the (empty) word, then starts the operator
                word.push(chr);
                i += 1;
                continue;
            }
        }
        if chr == ',' && quotes.is_none() { i += 1; continue; }
        word.push(chr);
        i += 1;
    }
    finish!(s.len() as isize - 1, false);

    // an unquoted "and" joining the fulltext to the expression is syntax, not a search word
    if !expression_tokens.is_empty() {
        if let Some(last) = fulltext_tokens.last() {
            if last.token == "and" && !last.in_quotes {
                fulltext_tokens.pop();
                fulltext_query = fulltext_query_before_last;
            }
        }
    }
    let fulltext_query = fulltext_query.split_whitespace().collect::<Vec<_>>().join(" ");
    Lexed { fulltext_query, fulltext_tokens, expression_tokens, leading_operator }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn toks(v: &[Tok]) -> Vec<&str> { v.iter().map(|t| t.token.as_str()).collect() }

    #[test]
    fn fulltext_only_and_quotes() {
        let l = lex("hello \"big world\" d'artagnan, c#");
        assert_eq!(toks(&l.fulltext_tokens), ["hello", "big world", "d'artagnan", "c#"]);
        assert!(l.fulltext_tokens[1].in_quotes);
        assert!(l.expression_tokens.is_empty());
    }

    #[test]
    fn attributes_and_operators_split_the_query() {
        let l = lex("towers #book #year >= 2020 note.title *=* ring");
        assert_eq!(toks(&l.fulltext_tokens), ["towers"]);
        assert_eq!(toks(&l.expression_tokens), ["#book", "#year", ">=", "2020", "note", ".", "title", "*=*", "ring"]);
        let l = lex("#a=b#c");
        assert_eq!(toks(&l.expression_tokens), ["#a", "=", "b", "#c"]);
    }

    #[test]
    fn groups_negation_and_connectors() {
        let l = lex("(#a OR #b) AND NOT(#c)");
        assert_eq!(toks(&l.expression_tokens), ["(", "#a", "or", "#b", ")", "and", "not", "(", "#c", ")"]);
        let l = lex("foo and #bar");
        assert_eq!(toks(&l.fulltext_tokens), ["foo"], "joining 'and' is dropped from the fulltext");
        let l = lex("#!archived ~!template");
        assert_eq!(toks(&l.expression_tokens), ["#!archived", "~!template"]);
    }

    #[test]
    fn leading_equals_means_exact_and_quoted_values_keep_commas() {
        let l = lex("=exact phrase");
        assert_eq!(l.leading_operator, "=");
        assert_eq!(toks(&l.fulltext_tokens), ["exact", "phrase"]);
        let l = lex("#geo=\"48.8,2.3\"");
        assert_eq!(toks(&l.expression_tokens), ["#geo", "=", "48.8,2.3"]);
        assert!(l.expression_tokens[2].in_quotes);
    }
}
