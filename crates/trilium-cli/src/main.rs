//! Scriptable access to a local Trilium replica. Output is plain text by default and JSON with
//! `--json`, so it composes with jq, fzf, cron and shell scripts. Write commands record
//! everything needed for sync; add `--sync` to push right away.
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{io::Read, path::PathBuf};
use trilium_core::{db::Db, model::Model, search::{SearchOpts, Searcher}, sync::SyncClient};

#[derive(Parser)]
#[command(name = "trilium-cli", version, about = "Scriptable client for a local Trilium Next replica")]
struct Cli {
    /// Replica database (default: ~/.local/share/trilium-tui/default.db)
    #[arg(long, global = true, env = "TRILIUM_TUI_DB")]
    db: Option<PathBuf>,
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    /// After a write command, sync with the server immediately
    #[arg(long, global = true)]
    sync: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PasswordAction { Set, Change }

#[derive(Subcommand)]
enum Cmd {
    /// Print a shell completion script (bash, zsh, fish, ...)
    Completions {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },
    /// Create an empty local-only database (no server needed)
    Init,
    /// Create or update the replica from a server (first run needs TRILIUM_PASSWORD)
    Sync {
        /// Server address, e.g. http://notes.example.com:8080 (default: the one used last time)
        url: Option<String>,
    },
    /// Recompute every stored hash and compare with the change log
    Verify,
    /// Search with Trilium's query syntax (#label, ~relation, note.title *=* x, orderBy ...)
    Search {
        /// Trilium search expression
        query: String, #[arg(long, default_value_t = 20)] limit: usize, /// include archived notes
        #[arg(long)] archived: bool },
    /// Print a note's content (or --meta for its details)
    Get {
        #[arg(help = "Note id")] note: String,
        /// Print id, type, dates and attributes instead of the content
        #[arg(long)] meta: bool,
    },
    /// List a note's children (default: root)
    Ls {
        /// Note id (default: the root)
        note: Option<String>,
    },
    /// Create a note; content from --content, positional text or stdin (-)
    New {
        /// Title of the new note
        title: String,
        /// Parent note id
        #[arg(long, default_value = "root")] parent: String,
        /// Note type: text, code, book, mermaid, ...
        #[arg(long, default_value = "text")] r#type: String,
        /// MIME type (code notes, e.g. text/x-python)
        #[arg(long)] mime: Option<String>,
        /// Content (or `-` to read stdin)
        #[arg(long)] content: Option<String>,
    },
    /// Replace a note's content (text from argument, or `-` to read stdin)
    Set {
        #[arg(help = "Note id")] note: String,
        /// New content, or `-` to read it from stdin
        text: String,
    },
    /// Append to a note (plain text becomes a paragraph unless --html)
    Append {
        #[arg(help = "Note id")] note: String,
        /// Text to add, or `-` to read it from stdin
        text: String,
        /// Treat the text as HTML instead of wrapping it in a paragraph
        #[arg(long)] html: bool,
    },
    /// Rename a note
    Rename {
        #[arg(help = "Note id")] note: String,
        /// New title
        title: String,
    },
    /// Encrypt a note (title, content, attachments, revisions); needs TRILIUM_PROTECTED_PASSWORD
    Protect {
        #[arg(help = "Note id")] note: String,
        /// Also protect everything below it
        #[arg(long, short)] recursive: bool,
        /// Delete the automatic daily backups afterwards (copies made earlier still hold the plaintext)
        #[arg(long)] delete_backups: bool,
    },
    /// Decrypt a protected note for good; needs TRILIUM_PROTECTED_PASSWORD
    Unprotect {
        #[arg(help = "Note id")] note: String,
        /// Also unprotect everything below it
        #[arg(long, short)] recursive: bool,
    },
    /// Protected-notes password: `set` the first one, or `change` it (TRILIUM_PROTECTED_PASSWORD, plus
    /// TRILIUM_PROTECTED_NEW_PASSWORD for change)
    Password {
        /// set or change
        action: PasswordAction,
    },
    /// Delete a note (its first placement)
    Rm {
        #[arg(help = "Note id")] note: String,
    },
    /// Add a label (name or name=value)
    Label {
        #[arg(help = "Note id")] note: String,
        /// `name` or `name=value`
        spec: String,
    },
    /// Export a note and its subtree as a Trilium .zip (default), Markdown files (.zip), an OPML outline, an Evernote .enex notebook, or a static website (.zip)
    Export {
        /// Note id (the subtree below it is exported too)
        note: String,
        /// File to write
        out: PathBuf,
        /// zip (default), markdown (a zip of .md files), opml, enex or site (a browsable website in a zip); *.opml and *.enex files pick their format automatically
        #[arg(long)] format: Option<String>,
    },
    /// Import a Trilium/Obsidian/Notion/Anytype/Google Keep export (.zip or folder), an Evernote .enex or OPML file, a folder of Markdown/HTML/CSV/... files, or a single file
    Import {
        /// File or folder to import
        path: PathBuf,
        /// Note to import under
        #[arg(long, default_value = "root")] parent: String,
        /// obsidian, notion, anytype, keep or trilium (generic folder/zip); archives are detected automatically, so this is only needed to force one
        #[arg(long)] format: Option<String>,
        /// Keep loose Markdown/CSV/... files as source and file notes instead of converting them to text and spreadsheet notes
        #[arg(long)] keep_sources: bool,
    },
    /// Add another placement (clone) of a note under a parent
    Clone {
        #[arg(help = "Note id")] note: String,
        /// Note to add the extra placement under
        parent: String,
    },
    /// Move a note (its first placement) under another parent
    Mv {
        #[arg(help = "Note id")] note: String,
        /// New parent note
        parent: String,
    },
    /// Deep-copy a note and its subtree next to the original (title gets " (dup)")
    Dup {
        #[arg(help = "Note id")] note: String,
    },
    /// List a note's revisions (newest first)
    Revisions {
        #[arg(help = "Note id")] note: String,
    },
    /// Restore a revision (the current state is saved as a revision first)
    Restore {
        /// Revision id (see `revisions`)
        revision: String,
    },
    /// Save a revision of the note's current state now
    Snapshot {
        #[arg(help = "Note id")] note: String,
    },
    /// List notes that link to this note
    Backlinks {
        #[arg(help = "Note id")] note: String,
    },
    /// List a note's attributes (own and inherited)
    Attrs {
        #[arg(help = "Note id")] note: String,
    },
    /// Attach a file to a note
    Attach {
        #[arg(help = "Note id")] note: String,
        /// File to attach
        file: PathBuf,
    },
    /// List a note's attachments
    Attachments {
        #[arg(help = "Note id")] note: String,
    },
    /// Write an attachment's bytes to stdout
    AttachmentGet {
        /// Attachment id (see `attachments`)
        attachment: String,
    },
    /// Today's (or --date) journal note: print its id, optionally append text
    Daily {
        /// A day (YYYY-MM-DD; default: today)
        #[arg(long)] date: Option<String>,
        /// A week note (YYYY-Www, or "this"); needs #enableWeekNote on the calendar root
        #[arg(long, conflicts_with_all = ["date", "quarter"])] week: Option<String>,
        /// A quarter note (YYYY-Qn, or "this")
        #[arg(long, conflicts_with_all = ["date", "week"])] quarter: Option<String>,
        /// Text to add to the note (or `-` for stdin)
        #[arg(long)] append: Option<String>,
    },
}

fn db_path(cli: &Cli) -> PathBuf { cli.db.clone().unwrap_or_else(trilium_core::default_db_path) }

fn text_arg(s: &str) -> Result<String> {
    if s == "-" {
        let mut b = String::new();
        std::io::stdin().read_to_string(&mut b).context("reading stdin")?;
        Ok(b)
    } else { Ok(s.to_string()) }
}

fn escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;") }

/// One record per line, one field per tab: a title may hold neither, nor any control character, however the output is read
/// (a title with a newline and a fake id would forge a row for scripts that cut and pipe the listing).
fn one_line(s: &str) -> String { s.chars().map(|c| if c.is_control() || ('\u{80}'..='\u{9f}').contains(&c) { ' ' } else { c }).collect() }

fn print<T: serde::Serialize>(json_out: bool, v: &T, text: impl FnOnce() -> String) {
    if json_out { println!("{}", serde_json::to_string(v).unwrap()); } else { println!("{}", for_terminal(text())); }
}

/// Note text is untrusted: on a terminal, control characters (escape sequences that could retitle the window,
/// rewrite the screen or fill the clipboard) are shown as a visible placeholder. Pipes get the bytes untouched.
fn for_terminal(s: String) -> String {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() { return s; }
    s.replace("\r\n", "\n").chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '\u{fffd}' } else { c }).collect()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Completions { shell } = &cli.cmd {
        use clap::CommandFactory;
        clap_complete::generate(*shell, &mut Cli::command(), "trilium-cli", &mut std::io::stdout());
        return Ok(());
    }
    let path = db_path(&cli);
    if !matches!(cli.cmd, Cmd::Init | Cmd::Sync { .. }) && !path.exists() {
        bail!("no database at {}\nCreate an empty local one with `trilium-cli init`, or fetch your notes with `TRILIUM_PASSWORD=... trilium-cli sync <server-url>` (use --db or TRILIUM_TUI_DB to pick another file)", path.display());
    }
    let writes = !matches!(cli.cmd, Cmd::Completions { .. } | Cmd::Verify | Cmd::Search { .. } | Cmd::Get { .. } | Cmd::Ls { .. });
    if writes && path.exists() {
        // one consistent copy per day before anything is changed
        if let Ok(Some(b)) = Db::open(&path).and_then(|d| d.backup_daily(&path, 7)) { eprintln!("backup saved: {}", b.display()); }
    }
    let wrote = run(&cli, &path)?;
    if wrote && cli.sync { sync_now(&path)?; }
    Ok(())
}

/// Open the replica; with TRILIUM_PROTECTED_PASSWORD set, protected notes are unlocked for this command.
/// A search hit's title; a locked protected note has none in the search corpus.
fn shown(c: &trilium_core::search::eval::Corpus, id: &str) -> String {
    let i = c.idx[id] as usize;
    if c.title[i].is_empty() && c.protected[i] { "[protected]".into() } else { c.title[i].clone() }
}

fn open_db(path: &std::path::Path) -> Result<Db> {
    let db = Db::open(path)?;
    if let Ok(pw) = std::env::var("TRILIUM_PROTECTED_PASSWORD") {
        if !pw.is_empty() {
            if !db.has_password() { bail!("TRILIUM_PROTECTED_PASSWORD is set but this replica has no protected-notes password (create one with `trilium-cli password set`)"); }
            if !db.unlock(&pw)? { bail!("wrong TRILIUM_PROTECTED_PASSWORD"); }
        }
    }
    Ok(db)
}

fn sync_now(path: &PathBuf) -> Result<()> {
    let mut db = open_db(path)?;
    let url = db.state("serverUrl")?.context("no server known; run `trilium-cli sync <url>` first")?;
    let secret = db.state("documentSecret")?.context("no sync secret; run `trilium-cli sync <url>` first")?;
    let mut c = SyncClient::new(&url, &db.instance_id()?)?;
    let out = c.sync(&mut db, &secret, |_, _| {})?;
    eprintln!("synced (pushed {}, {} sectors checked{})", out.pushed, out.sectors_checked,
        if out.repaired > 0 { format!(", {} repaired", out.repaired) } else { String::new() });
    Ok(())
}

/// Returns true when the database was modified.
fn run(cli: &Cli, path: &PathBuf) -> Result<bool> {
    match &cli.cmd {
        Cmd::Completions { .. } => Ok(false),
        Cmd::Init => { trilium_core::init_local_db(path)?; println!("{}", path.display()); Ok(false) }
        Cmd::Sync { url } => {
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            sync_full(url.as_deref(), path)?;
            Ok(false)
        }
        Cmd::Verify => {
            let db = open_db(path)?;
            let r = db.recompute_hashes()?;
            let bad: u32 = r.values().map(|(_, b)| *b).sum();
            print(cli.json, &r, || r.iter().map(|(n, (ok, b))| format!("{n:12} ok {ok:6}  mismatch {b}")).collect::<Vec<_>>().join("\n"));
            if bad > 0 { bail!("{bad} hashes differ from the change log"); }
            Ok(false)
        }
        Cmd::Search { query, limit, archived } => {
            let db = open_db(path)?;
            let searcher = Searcher::new(&db)?;
            let out = searcher.search(&db, query, &SearchOpts { limit: Some(*limit), include_archived: *archived, ..Default::default() })?;
            if let Some(e) = &out.error { eprintln!("search warning: {e}"); }
            let c = searcher.corpus();
            let rows: Vec<_> = out.hits.iter().map(|h| json!({"id": h.note_id, "title": shown(c, &h.note_id), "score": h.score,
                "path": h.path.iter().map(|i| shown(c, i)).collect::<Vec<_>>().join(" / ")})).collect();
            print(cli.json, &rows, || out.hits.iter().map(|h| format!("{}\t{}", h.note_id, one_line(&shown(c, &h.note_id)))).collect::<Vec<_>>().join("\n"));
            if out.error.is_some() { std::process::exit(3); }
            Ok(false)
        }
        Cmd::Get { note, meta } => {
            let db = open_db(path)?;
            let m = Model::load(&db)?;
            let n = m.notes.get(note).with_context(|| format!("note {note} not found"))?;
            if *meta || cli.json {
                let attrs: Vec<_> = db.note_attributes(note)?.into_iter().map(|a| json!({"type": a.kind, "name": a.name, "value": a.value})).collect();
                let v = json!({"id": n.id, "title": n.title, "type": n.ntype, "mime": n.mime, "protected": n.protected,
                    "attributes": attrs, "parents": m.parents.get(note), "children": m.children.get(note).map(|c| c.iter().map(|b| b.note_id.clone()).collect::<Vec<_>>()),
                    "content": if cli.json && (!n.protected || db.is_unlocked()) { db.note_content(note)? } else { None }});
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                if n.protected && !db.is_unlocked() { bail!("this note is protected; set TRILIUM_PROTECTED_PASSWORD to read it"); }
                print!("{}", for_terminal(db.note_content(note)?.unwrap_or_default()));
            }
            Ok(false)
        }
        Cmd::Ls { note } => {
            let db = open_db(path)?;
            let m = Model::load(&db)?;
            let id = note.as_deref().unwrap_or("root");
            let kids: Vec<_> = m.children.get(id).into_iter().flatten().filter(|b| b.note_id != "_hidden").collect();
            let rows: Vec<_> = kids.iter().map(|b| json!({"id": b.note_id, "title": m.display_title(b), "type": m.notes[&b.note_id].ntype})).collect();
            print(cli.json, &rows, || kids.iter().map(|b| format!("{}\t{}", b.note_id, one_line(&m.display_title(b)))).collect::<Vec<_>>().join("\n"));
            Ok(false)
        }
        Cmd::New { title, parent, r#type, mime, content } => {
            let db = open_db(path)?;
            let mime = mime.clone().unwrap_or_else(|| match r#type.as_str() { "text" => "text/html".into(), "code" => "text/plain".into(), _ => "text/plain".into() });
            let content = match content { Some(c) => text_arg(c)?, None => String::new() };
            let id = db.create_note(parent, title, r#type, &mime, &content)?;
            print(cli.json, &json!({"id": id}), || id.clone());
            Ok(true)
        }
        Cmd::Set { note, text } => { open_db(path)?.set_note_content(note, &text_arg(text)?)?; Ok(true) }
        Cmd::Append { note, text, html } => {
            let db = open_db(path)?;
            let add = text_arg(text)?;
            let cur = db.note_content(note)?.context("note not found")?;
            let ntype: String = db.conn.query_row("SELECT type FROM notes WHERE noteId=? AND isDeleted=0", [note], |r| r.get(0)).context("note not found")?;
            let new = if ntype == "text" {
                let add = if *html { add } else { format!("<p>{}</p>", escape(add.trim_end()).replace('\n', "<br>")) };
                format!("{cur}{add}")
            } else {
                // code / markdown / plain notes are not HTML: add the text on its own line
                let sep = if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" };
                format!("{cur}{sep}{}\n", add.trim_end())
            };
            db.set_note_content(note, &new)?;
            Ok(true)
        }
        Cmd::Protect { note, recursive, .. } | Cmd::Unprotect { note, recursive } => {
            let protect = matches!(cli.cmd, Cmd::Protect { .. });
            let db = open_db(path)?;
            db.ensure_writable()?;
            if !db.is_unlocked() { bail!("set TRILIUM_PROTECTED_PASSWORD to {} notes", if protect { "protect" } else { "unprotect" }); }
            let n = db.protect_note(note, protect, *recursive)?;
            print(cli.json, &json!({"changed": n}), || format!("{} {n} note(s)", if protect { "protected" } else { "unprotected" }));
            if protect {
                if matches!(cli.cmd, Cmd::Protect { delete_backups: true, .. }) { eprintln!("deleted {} backup(s)", db.delete_backups()); }
                else if !db.backups().is_empty() { eprintln!("note: {} automatic backup(s) next to the database were made before protecting and still hold the plaintext; re-run with --delete-backups to remove them", db.backups().len()); }
            }
            Ok(true)
        }
        Cmd::Password { action } => {
            let db = Db::open(path)?;
            db.ensure_writable()?;
            let pw = std::env::var("TRILIUM_PROTECTED_PASSWORD").ok().filter(|p| !p.is_empty()).context("set TRILIUM_PROTECTED_PASSWORD")?;
            match action {
                PasswordAction::Set => { db.set_password(&pw)?; println!("protected-notes password set"); }
                PasswordAction::Change => {
                    let new = std::env::var("TRILIUM_PROTECTED_NEW_PASSWORD").ok().filter(|p| !p.is_empty()).context("set TRILIUM_PROTECTED_NEW_PASSWORD")?;
                    db.change_password(&pw, &new)?;
                    println!("protected-notes password changed");
                }
            }
            Ok(true)
        }
        Cmd::Rename { note, title } => { open_db(path)?.set_note_title(note, title)?; Ok(true) }
        Cmd::Rm { note } => {
            let db = open_db(path)?;
            let b = db.first_branch_of(note)?.with_context(|| format!("note {note} has no placement"))?;
            db.delete_branch(&b)?;
            Ok(true)
        }
        Cmd::Label { note, spec } => {
            let db = open_db(path)?;
            let s = spec.trim_start_matches('#');
            let (name, value) = s.split_once('=').unwrap_or((s, ""));
            db.add_attribute(note, "label", name, value, false)?;
            Ok(true)
        }
        Cmd::Export { note, out, format } => {
            let db = open_db(path)?;
            let ext = out.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
            let fmt = format.clone().unwrap_or_else(|| match ext.as_str() { "opml" => "opml".into(), "enex" => "enex".into(), _ => "zip".into() });
            let st = match fmt.as_str() {
                "zip" => db.export_zip_file(note, out)?,
                "site" | "share" => db.export_site_file(note, out)?,
                "markdown" | "md" => db.export_markdown_file(note, out)?,
                "enex" => trilium_core::transfer::ExportStats { notes: db.export_enex_file(note, out)?, attachments: 0 },
                "opml" => trilium_core::transfer::ExportStats { notes: db.export_opml_file(note, out)?, attachments: 0 },
                other => bail!("unknown export format '{other}' (zip, markdown, opml, enex or site)"),
            };
            print(cli.json, &json!({"notes": st.notes, "attachments": st.attachments, "file": out}), || format!("exported {} notes, {} attachments to {}", st.notes, st.attachments, out.display()));
            Ok(false)
        }
        Cmd::Import { path: src, parent, format, keep_sources } => {
            let db = open_db(path)?;
            let st = db.import_path(src, parent, &trilium_core::transfer::ImportOpts { format: format.clone(), keep_sources: *keep_sources, ..Default::default() })?;
            print(cli.json, &json!({"notes": st.notes, "attachments": st.attachments, "attributes": st.attributes, "clones": st.clones, "root": st.root}),
                || format!("imported {} notes, {} attachments, {} attributes, {} clones{}", st.notes, st.attachments, st.attributes, st.clones, st.root.as_ref().map(|r| format!(" (first: {r})")).unwrap_or_default()));
            Ok(true)
        }
        Cmd::Clone { note, parent } => { let b = open_db(path)?.clone_note(note, parent, None)?; print(cli.json, &json!({"branch": b}), || b.clone()); Ok(true) }
        Cmd::Mv { note, parent } => {
            let db = open_db(path)?;
            let b = db.first_branch_of(note)?.context("note has no placement")?;
            let nb = db.move_branch_to(&b, parent)?;
            print(cli.json, &json!({"branch": nb}), || nb.clone());
            Ok(true)
        }
        Cmd::Dup { note } => {
            let db = open_db(path)?;
            let b = db.first_branch_of(note)?.context("note has no placement")?;
            let parent: String = db.conn.query_row("SELECT parentNoteId FROM branches WHERE branchId=?", [&b], |r| r.get(0))?;
            let id = db.duplicate_subtree(note, &parent, true)?;
            print(cli.json, &json!({"id": id}), || id.clone());
            Ok(true)
        }
        Cmd::Revisions { note } => {
            let db = open_db(path)?;
            let revs = db.revisions(note)?;
            let rows: Vec<_> = revs.iter().map(|r| json!({"id": r.id, "title": r.title, "source": r.source, "created": r.utc_created, "size": r.size})).collect();
            print(cli.json, &rows, || revs.iter().map(|r| format!("{}\t{}\t{}\t{}", r.id, r.utc_created, r.source, one_line(&r.title))).collect::<Vec<_>>().join("\n"));
            Ok(false)
        }
        Cmd::Restore { revision } => { open_db(path)?.restore_revision(revision)?; Ok(true) }
        Cmd::Snapshot { note } => {
            let id = open_db(path)?.save_revision(note, "", "manual")?;
            print(cli.json, &json!({"id": id}), || id.clone());
            Ok(true)
        }
        Cmd::Backlinks { note } => {
            let db = open_db(path)?;
            let m = Model::load(&db)?;
            let b = db.backlinks(note)?;
            let rows: Vec<_> = b.iter().map(|x| json!({"id": x.source, "title": m.title_of(&x.source), "relation": x.relation, "isLink": x.is_link})).collect();
            print(cli.json, &rows, || b.iter().map(|x| format!("{}\t{}\t{}", x.source, x.relation, one_line(m.title_of(&x.source)))).collect::<Vec<_>>().join("\n"));
            Ok(false)
        }
        Cmd::Attrs { note } => {
            let db = open_db(path)?;
            let a = db.attribute_rows(note)?;
            let rows: Vec<_> = a.iter().map(|x| json!({"id": x.id, "type": x.kind, "name": x.name, "value": x.value, "inheritable": x.inheritable, "owner": x.owner})).collect();
            print(cli.json, &rows, || a.iter().map(|x| format!("{}{}{}{}", if x.kind == "label" { "#" } else { "~" }, one_line(&x.name), if x.value.is_empty() { String::new() } else { format!("={}", one_line(&x.value)) }, if x.owner != *note { "  (inherited)" } else { "" })).collect::<Vec<_>>().join("\n"));
            Ok(false)
        }
        Cmd::Attach { note, file } => {
            let db = open_db(path)?;
            let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            let title = file.file_name().and_then(|n| n.to_str()).unwrap_or("attachment");
            let mime = match file.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
                Some("png") => "image/png", Some("jpg" | "jpeg") => "image/jpeg", Some("gif") => "image/gif", Some("svg") => "image/svg+xml",
                Some("pdf") => "application/pdf", Some("txt" | "md") => "text/plain", Some("json") => "application/json", _ => "application/octet-stream",
            };
            let id = db.add_attachment(note, title, mime, "file", &bytes)?;
            print(cli.json, &json!({"id": id}), || id.clone());
            Ok(true)
        }
        Cmd::Attachments { note } => {
            let db = open_db(path)?;
            let a = db.attachments(note)?;
            let rows: Vec<_> = a.iter().map(|x| json!({"id": x.id, "title": x.title, "mime": x.mime, "size": x.size, "role": x.role})).collect();
            print(cli.json, &rows, || a.iter().map(|x| format!("{}\t{}\t{}\t{} B", x.id, one_line(&x.title), one_line(&x.mime), x.size)).collect::<Vec<_>>().join("\n"));
            Ok(false)
        }
        Cmd::AttachmentGet { attachment } => {
            use std::io::Write;
            let bytes = open_db(path)?.attachment_bytes(attachment)?.context("attachment not found")?;
            std::io::stdout().write_all(&bytes)?;
            Ok(false)
        }
        Cmd::Daily { date, week, quarter, append } => {
            let db = open_db(path)?;
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let id = if let Some(w) = week {
                db.week_note(&if w == "this" { db.week_of(&today)? } else { w.clone() })?
            } else if let Some(q) = quarter {
                use chrono::Datelike;
                let now = chrono::Local::now();
                db.quarter_note(&if q == "this" { format!("{}-Q{}", now.year(), now.month0() / 3 + 1) } else { q.clone() })?
            } else { match date { Some(d) => db.day_note(d)?, None => db.today_note()? } };
            if let Some(t) = append {
                let cur = db.note_content(&id)?.unwrap_or_default();
                db.set_note_content(&id, &format!("{cur}<p>{}</p>", escape(&text_arg(t)?)))?;
            }
            print(cli.json, &json!({"id": id}), || id.clone());
            Ok(true)
        }
    }
}

/// The first sync sends the account password; over plain HTTP outside a private network that is readable by anyone en route.
fn plaintext_warning(url: &str) -> Option<String> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("http://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = hostport.strip_prefix('[') { v6.split(']').next().unwrap_or("") } else { hostport.split(':').next().unwrap_or("") };
    let private = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local() || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1])) || ip == Ipv4Addr::LOCALHOST,
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80 || ip == Ipv6Addr::LOCALHOST,
        Err(_) => host == "localhost" || host.ends_with(".local") || host.ends_with(".lan") || host.ends_with(".home.arpa") || (!host.is_empty() && !host.contains('.')),
    };
    (!private).then(|| format!("{host} is reached over plain http://; your Trilium password and notes cross the network unencrypted. Prefer https://"))
}

fn sync_full(url_arg: Option<&str>, path: &PathBuf) -> Result<()> {
    let mut db = open_db(path)?;
    let instance = match db.state("instanceId")? {
        Some(i) => i,
        None => {
            let i = trilium_core::write::new_entity_id().to_lowercase();
            db.set_state("instanceId", &i)?;
            i
        }
    };
    let url = match (url_arg, db.state("serverUrl")?) {
        (Some(u), _) => u.trim_end_matches('/').to_string(),
        (None, Some(u)) => u,
        (None, None) => bail!("first sync needs a server URL: trilium-cli sync http://host:8080"),
    };
    db.set_state("serverUrl", &url)?;
    let mut client = SyncClient::new(&url, &instance)?;
    let secret = match db.state("documentSecret")? {
        Some(s) => s,
        None => {
            let pw = std::env::var("TRILIUM_PASSWORD").context("first sync needs TRILIUM_PASSWORD")?;
            if let Some(w) = plaintext_warning(&url) { eprintln!("warning: {w}"); }
            let (id, secret) = client.fetch_seed(&pw)?;
            db.set_state("documentId", &id)?;
            db.set_state("documentSecret", &secret)?;
            #[cfg(unix)]
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?; // holds the sync secret
            secret
        }
    };
    let t = std::time::Instant::now();
    let out = client.sync(&mut db, &secret, |s, left| eprintln!("  applied {} ({} outstanding)", s.applied, left))?;
    eprintln!("sync done in {:?}: applied {}, conflicts {}, replaced local edits {}, {} round(s)", t.elapsed(), out.stats.applied, out.stats.conflicts.len(), out.stats.overwritten.len(), out.rounds);
    if !out.stats.overwritten.is_empty() { eprintln!("  your replaced edits were saved in the 'overwritten' folder next to {}", path.display()); }
    eprintln!("pushed {} changes", out.pushed);
    if out.repaired > 0 { eprintln!("{} hash sector(s) differed and were re-synced", out.repaired); }
    println!("content hash check PASSED ({} sectors)", out.sectors_checked);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::plaintext_warning as w;

    #[test]
    fn plain_http_warning_only_for_non_private_hosts() {
        for ok in ["https://notes.example.com", "http://192.168.1.10:8080", "http://localhost:8080", "http://[::1]:8080", "http://nas/", "http://10.0.0.5", "HTTP://172.20.1.1", "http://box.local", "http://100.100.1.1", "http://[fd00::1]:80"] {
            assert!(w(ok).is_none(), "{ok}");
        }
        for bad in ["http://10.evil.com", "http://192.168.1.1.evil.org", "http://127.0.0.1@evil.com", "http://100.example.com", "http://[2001:db8::1]:8080", "http://user@notes.example.com", "http://8.8.8.8", "http://172.32.0.1", "http://100.200.1.1"] {
            assert!(w(bad).is_some(), "{bad}");
        }
    }
}
