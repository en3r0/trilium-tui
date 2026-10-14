//! Best-effort LaTeX -> Unicode text for the reading view. Anything not understood stays as its source.

const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"), ("beta", "β"), ("gamma", "γ"), ("delta", "δ"), ("epsilon", "ε"), ("varepsilon", "ε"), ("zeta", "ζ"), ("eta", "η"),
    ("theta", "θ"), ("vartheta", "ϑ"), ("iota", "ι"), ("kappa", "κ"), ("lambda", "λ"), ("mu", "μ"), ("nu", "ν"), ("xi", "ξ"), ("pi", "π"),
    ("rho", "ρ"), ("sigma", "σ"), ("tau", "τ"), ("upsilon", "υ"), ("phi", "φ"), ("varphi", "φ"), ("chi", "χ"), ("psi", "ψ"), ("omega", "ω"),
    ("Gamma", "Γ"), ("Delta", "Δ"), ("Theta", "Θ"), ("Lambda", "Λ"), ("Xi", "Ξ"), ("Pi", "Π"), ("Sigma", "Σ"), ("Phi", "Φ"), ("Psi", "Ψ"), ("Omega", "Ω"),
    ("leq", "≤"), ("le", "≤"), ("geq", "≥"), ("ge", "≥"), ("neq", "≠"), ("ne", "≠"), ("approx", "≈"), ("equiv", "≡"), ("sim", "∼"), ("propto", "∝"),
    ("pm", "±"), ("mp", "∓"), ("times", "×"), ("div", "÷"), ("cdot", "·"), ("ast", "∗"), ("circ", "∘"), ("bullet", "•"),
    ("sum", "∑"), ("prod", "∏"), ("int", "∫"), ("iint", "∬"), ("oint", "∮"), ("partial", "∂"), ("nabla", "∇"), ("infty", "∞"),
    ("rightarrow", "→"), ("to", "→"), ("leftarrow", "←"), ("gets", "←"), ("leftrightarrow", "↔"), ("Rightarrow", "⇒"), ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"), ("iff", "⇔"), ("implies", "⇒"), ("mapsto", "↦"), ("uparrow", "↑"), ("downarrow", "↓"),
    ("in", "∈"), ("notin", "∉"), ("ni", "∋"), ("subset", "⊂"), ("supset", "⊃"), ("subseteq", "⊆"), ("supseteq", "⊇"), ("cup", "∪"), ("cap", "∩"),
    ("emptyset", "∅"), ("varnothing", "∅"), ("forall", "∀"), ("exists", "∃"), ("neg", "¬"), ("lnot", "¬"), ("land", "∧"), ("wedge", "∧"),
    ("lor", "∨"), ("vee", "∨"), ("ldots", "…"), ("cdots", "⋯"), ("dots", "…"), ("vdots", "⋮"), ("ddots", "⋱"), ("angle", "∠"), ("perp", "⊥"),
    ("parallel", "∥"), ("degree", "°"), ("prime", "′"), ("hbar", "ħ"), ("ell", "ℓ"), ("Re", "ℜ"), ("Im", "ℑ"), ("aleph", "ℵ"),
    ("quad", "  "), ("qquad", "    "), ("langle", "⟨"), ("rangle", "⟩"), ("lfloor", "⌊"), ("rfloor", "⌋"), ("lceil", "⌈"), ("rceil", "⌉"),
    ("sin", "sin"), ("cos", "cos"), ("tan", "tan"), ("log", "log"), ("ln", "ln"), ("exp", "exp"), ("lim", "lim"), ("max", "max"), ("min", "min"),
    ("det", "det"), ("gcd", "gcd"), ("mod", "mod"), ("sup", "sup"), ("inf", "inf"),
];

/// Commands that just wrap their argument.
const WRAPPERS: &[&str] = &["text", "mathrm", "mathbf", "mathit", "mathbb", "mathcal", "mathsf", "mathtt", "textbf", "textit", "operatorname", "boldsymbol", "bm", "hat", "bar", "vec", "tilde", "overline", "underline"];

const SUP: &str = "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱ";
const SUP_SRC: &str = "0123456789+-=()ni";
const SUB: &str = "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑₒₓᵢⱼₙ";
const SUB_SRC: &str = "0123456789+-=()aeoxijn";

pub fn latex_to_text(src: &str) -> String {
    let s = src.trim();
    let s = s.strip_prefix("\\(").and_then(|r| r.strip_suffix("\\)"))
        .or_else(|| s.strip_prefix("\\[").and_then(|r| r.strip_suffix("\\]")))
        .or_else(|| s.strip_prefix("$$").and_then(|r| r.strip_suffix("$$")))
        .unwrap_or(s);
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    parse(&chars, &mut 0, &mut out, 0);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is this source a display equation (`\[ … \]` or `$$ … $$`)?
pub fn is_display(src: &str) -> bool { let s = src.trim(); s.starts_with("\\[") || s.starts_with("$$") }

fn group(c: &[char], i: &mut usize, depth: usize) -> String {
    while *i < c.len() && c[*i] == ' ' { *i += 1; }
    let mut out = String::new();
    if *i >= c.len() { return out; }
    if c[*i] == '{' {
        *i += 1;
        parse(c, i, &mut out, depth + 1);
        if *i < c.len() && c[*i] == '}' { *i += 1; }
    } else if c[*i] == '\\' {
        let start = *i;
        *i += 1;
        while *i < c.len() && c[*i].is_ascii_alphabetic() { *i += 1; }
        if *i == start + 1 && *i < c.len() { *i += 1; }
        let cmd: String = c[start..*i].iter().collect();
        let mut tmp = Vec::new();
        tmp.extend(cmd.chars());
        parse(&tmp, &mut 0, &mut out, depth + 1);
    } else {
        out.push(c[*i]);
        *i += 1;
    }
    out
}

fn script(arg: &str, sup: bool) -> String {
    let (from, to) = if sup { (SUP_SRC, SUP) } else { (SUB_SRC, SUB) };
    let to: Vec<char> = to.chars().collect();
    let mapped: Option<String> = arg.chars().map(|ch| from.chars().position(|f| f == ch).map(|p| to[p])).collect();
    match mapped {
        Some(m) if !m.is_empty() => m,
        _ if arg.chars().count() <= 1 => format!("{}{arg}", if sup { '^' } else { '_' }),
        _ => format!("{}({arg})", if sup { '^' } else { '_' }),
    }
}

fn paren(s: &str) -> String {
    if s.chars().count() <= 1 || s.chars().all(|c| c.is_alphanumeric()) { s.to_string() } else { format!("({s})") }
}

fn parse(c: &[char], i: &mut usize, out: &mut String, depth: usize) {
    if depth > 40 { return; }
    while *i < c.len() {
        let ch = c[*i];
        match ch {
            '}' if depth > 0 => return,
            '{' => { *i += 1; parse(c, i, out, depth + 1); if *i < c.len() && c[*i] == '}' { *i += 1; } }
            '^' | '_' => { *i += 1; let a = group(c, i, depth); out.push_str(&script(&a, ch == '^')); }
            '~' => { out.push(' '); *i += 1; }
            '\\' => {
                *i += 1;
                if *i >= c.len() { out.push('\\'); return; }
                if !c[*i].is_ascii_alphabetic() {
                    let n = c[*i];
                    *i += 1;
                    match n { ',' | ';' | ':' | ' ' | '!' => out.push(' '), '\\' => out.push(' '), _ => out.push(n) }
                    continue;
                }
                let st = *i;
                while *i < c.len() && c[*i].is_ascii_alphabetic() { *i += 1; }
                let cmd: String = c[st..*i].iter().collect();
                match cmd.as_str() {
                    "frac" | "dfrac" | "tfrac" => { let a = group(c, i, depth); let b = group(c, i, depth); out.push_str(&format!("{}/{}", paren(&a), paren(&b))); }
                    "sqrt" => { let a = group(c, i, depth); out.push_str(&format!("√{}", paren(&a))); }
                    "binom" => { let a = group(c, i, depth); let b = group(c, i, depth); out.push_str(&format!("C({a},{b})")); }
                    "left" | "right" | "big" | "Big" | "bigg" | "limits" | "displaystyle" => {}
                    w if WRAPPERS.contains(&w) => { let a = group(c, i, depth); out.push_str(&a); }
                    _ => match SYMBOLS.iter().find(|(k, _)| *k == cmd) {
                        Some((_, v)) => out.push_str(v),
                        None => { out.push('\\'); out.push_str(&cmd); out.push(' '); }
                    },
                }
            }
            _ => { out.push(ch); *i += 1; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::latex_to_text as t;

    #[test]
    fn common_math_becomes_unicode() {
        assert_eq!(t(r"\(E = mc^2\)"), "E = mc²");
        assert_eq!(t(r"\(\alpha + \beta \leq \frac{a}{b}\)"), "α + β ≤ a/b");
        assert_eq!(t(r"\[\sum_{i=1}^{n} x_i \rightarrow \sqrt{y}\]"), "∑ᵢ₌₁ⁿ xᵢ → √y");
        assert_eq!(t(r"\frac{x+1}{2}"), "(x+1)/2");
        assert_eq!(t(r"x^{10} + y^{ab}"), "x¹⁰ + y^(ab)");
        assert_eq!(t(r"\text{speed} \cdot \mathbf{v}"), "speed · v");
    }

    #[test]
    fn unknown_commands_and_bad_input_stay_readable() {
        assert_eq!(t(r"\foo{x}"), "\\foo x");
        assert_eq!(t(r"{{{{"), "");
        assert_eq!(t("x^"), "x^");
        assert_eq!(t(&"{".repeat(5000)), "");
    }
}
