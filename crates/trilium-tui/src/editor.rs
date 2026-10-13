//! Hand a note's stored text to $VISUAL / $EDITOR and bring the result back unchanged except
//! for what the user actually edited (editors that append a final newline are compensated).
use anyhow::{Context, Result, bail};
use std::io::Write;

pub fn extension_for(ntype: &str, mime: &str) -> &'static str {
    if ntype == "text" { return ".html"; }
    let m = mime.to_ascii_lowercase();
    let table: &[(&str, &str)] = &[
        ("markdown", ".md"), ("x-gfm", ".md"), ("javascript", ".js"), ("json", ".json"), ("python", ".py"), ("rust", ".rs"),
        ("x-sh", ".sh"), ("shell", ".sh"), ("html", ".html"), ("css", ".css"), ("xml", ".xml"), ("sql", ".sql"), ("yaml", ".yaml"),
    ];
    table.iter().find(|(k, _)| m.contains(k)).map(|(_, v)| *v).unwrap_or(".txt")
}

fn editor_command() -> Vec<String> {
    let raw = std::env::var("VISUAL").ok().filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "nvim".to_string());
    raw.split_whitespace().map(String::from).collect()
}

/// Returns `Some(new_text)` if the user changed the content, `None` if not.
pub fn edit_text(initial: &str, ext: &str) -> Result<Option<String>> {
    edit_text_with(initial, ext, &editor_command())
}

pub fn edit_text_with(initial: &str, ext: &str, cmd: &[String]) -> Result<Option<String>> {
    let (prog, args) = cmd.split_first().context("empty editor command")?;
    let mut file = tempfile::Builder::new().prefix("trilium-").suffix(ext).tempfile().context("creating temp file")?;
    file.write_all(initial.as_bytes())?;
    file.flush()?;
    let status = std::process::Command::new(prog).args(args).arg(file.path()).status()
        .with_context(|| format!("starting editor '{prog}' (set $EDITOR)"))?;
    if !status.success() {
        // a crashed or interrupted editor may still have left the user's text in the file: keep it when it differs
        if std::fs::read_to_string(file.path()).is_ok_and(|t| t != initial) {
            let (_, kept) = file.keep().context("keeping the edited file")?;
            bail!("editor exited with {status}; what was written so far is kept at {}", kept.display());
        }
        bail!("editor exited with {status}");
    }
    let mut new = match std::fs::read_to_string(file.path()) {
        Ok(s) => s,
        Err(err) => {
            // e.g. the editor saved in another encoding: don't throw the work away with the temp file
            let (_, kept) = file.keep().context("keeping the edited file")?;
            bail!("could not read the edited file back ({err}); it is kept at {}", kept.display());
        }
    };
    // vim & co. add a trailing newline to files that lacked one; that is not an edit
    if !initial.ends_with('\n') && new.ends_with('\n') { new.pop(); }
    Ok((new != initial).then_some(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> { vec!["sh".into(), "-c".into(), format!("{script}; true #"), "x".into()] }

    #[test]
    fn unchanged_content_is_none_even_if_editor_adds_newline() {
        // the "editor" appends a trailing newline like vim's fixeol
        let cmd = vec!["sh".into(), "-c".into(), r#"printf '\n' >> "$0""#.into()];
        assert_eq!(edit_text_with("<p>x</p>", ".html", &cmd).unwrap(), None);
    }

    #[test]
    fn edits_round_trip_byte_exact() {
        let cmd = vec!["sh".into(), "-c".into(), r#"sed -i 's/old/new/' "$0""#.into()];
        let initial = "<p>old \u{1F600} ünï</p>\r\n<p>two</p>\n\n\n";
        assert_eq!(edit_text_with(initial, ".html", &cmd).unwrap().as_deref(), Some("<p>new \u{1F600} ünï</p>\r\n<p>two</p>\n\n\n"));
    }

    #[test]
    fn failing_editor_is_an_error_and_ext_mapping_works() {
        assert!(edit_text_with("a", ".txt", &["false".into()]).is_err());
        assert_eq!(extension_for("text", "text/html"), ".html");
        assert_eq!(extension_for("code", "text/x-python"), ".py");
        assert_eq!(extension_for("code", "text/x-markdown"), ".md");
        let _ = sh;
    }
}
