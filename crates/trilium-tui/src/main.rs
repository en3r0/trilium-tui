mod actions;
mod app;
mod collections;
mod edit_view;
mod editbuf;
mod config;
mod editor;
mod highlight;
mod images;
mod keymap;
mod math;
mod panel;
mod render;
mod rich_view;
mod sync;
mod tabs;
mod ui;

use anyhow::{Result, bail};
use app::App;
use ratatui::crossterm::{
    event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement},
};
use std::{path::PathBuf, sync::mpsc::channel, time::Duration};

/// Whether the terminal speaks the kitty keyboard protocol, asked once at startup (the query can take seconds on terminals that ignore it).
static ENHANCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const USAGE: &str = r#"usage: trilium-tui [--db PATH] [--server URL] [--no-sync] [--today] [--new]

A terminal client for Trilium Next with a local replica that syncs in the background.

options:
  --db PATH       replica database (default: ~/.local/share/trilium-tui/default.db; fetch your notes
                  with `trilium-cli sync <url>` or start empty with --new)
  --server URL    sync with this Trilium server (the first login needs TRILIUM_PASSWORD)
  --no-sync       work offline: never contact the server
  --today         open today's journal note on start
  --new           create an empty local notes database if there is none yet
  -h, --help      this text

Inside the app F1 lists every key. The command palette (see F1) also holds the import and export
commands: Markdown, OPML, Evernote, Obsidian, Notion, Anytype, Google Keep and a static website,
and the protected-notes commands (unlock, lock, protect, unprotect)."#;

fn main() -> Result<()> {
    let mut db_path = trilium_core::default_db_path();
    let cfg = config::Config::load();
    let (mut server, mut no_sync, mut today, mut new_db) = (cfg.server.clone(), false, cfg.open_today_on_startup, false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db_path = PathBuf::from(args.next().unwrap_or_default()),
            "--server" => server = args.next(),
            "--no-sync" => no_sync = true,
            "--today" => today = true,
            "--new" => new_db = true,
            "-h" | "--help" => { println!("{USAGE}"); return Ok(()); }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    if new_db && !db_path.exists() { trilium_core::init_local_db(&db_path)?; eprintln!("created empty local notes database at {}", db_path.display()); }
    if !db_path.exists() {
        bail!("no database at {}\nCreate one with: TRILIUM_PASSWORD=... trilium-cli sync <server-url> {}", db_path.display(), db_path.display());
    }
    let mut app = App::new(&db_path, server)?;
    app.imgs.enabled = cfg.inline_images;
    app.style = cfg.pane_style.as_deref().map(ui::PaneStyle::parse).unwrap_or_default();
    app.persist_settings = true;
    if let Ok(Some(b)) = app.db.backup_daily(&db_path, 7) { app.status = format!("backup saved: {}", b.display()); }
    let (tx, rx) = channel();
    app.index_ready = false;
    sync::spawn_indexer(db_path.clone(), tx.clone());
    if !no_sync {
        if let (Some(url), true) = (app.server_url.clone(), app.db.state("documentSecret")?.is_some()) {
            app.sync_trigger = Some(sync::spawn(db_path, url, tx));
        }
    }

    // `--today` waits (inside open_today) for the first sync when the day note doesn't exist yet
    let mut terminal = ratatui::init();
    if today { app.open_today(); }
    {
        // ratatui's panic hook leaves the alternate screen but not mouse/paste/keyboard modes
        let prev = std::panic::take_hook();
        let main_thread = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            // worker threads (sync, indexer) must not tear down a terminal the UI is still using
            if std::thread::current().id() != main_thread { let _ = info; return; }
            let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste, PopKeyboardEnhancementFlags);
            prev(info);
        }));
    }
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    ENHANCED.store(enhanced, std::sync::atomic::Ordering::Relaxed);
    if enhanced {
        // lets the terminal report Ctrl+Shift+J, Ctrl+. etc. the way the web app's shortcuts expect
        let _ = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // ask the terminal which graphics protocol it speaks (sixel on foot, kitty graphics, ...); fall back to half-blocks
    app.picker = Some(images::make_picker());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        let _ = signal_hook::flag::register(sig, stop.clone());
    }
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut terminal, &mut app, rx, &stop))) {
        Ok(r) => r,
        Err(p) => { app.save_unsaved_on_exit(); std::panic::resume_unwind(p) } // keep an open editor's text
    };
    if result.is_err() { app.save_unsaved_on_exit(); } // e.g. the terminal vanished under an open editor
    images::cleanup_temp();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    if enhanced { let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags); }
    ratatui::restore();
    result
}

/// Leave the alternate screen so a full-screen editor can own the terminal.
fn suspend(_t: &mut ratatui::DefaultTerminal) {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    if ENHANCED.load(std::sync::atomic::Ordering::Relaxed) { let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags); }
    ratatui::restore();
}

fn resume(t: &mut ratatui::DefaultTerminal) -> Result<()> {
    // re-enter the screen by hand: ratatui::init() would stack another panic hook on every editor run
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    if ENHANCED.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    t.clear()?;
    Ok(())
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App, rx: std::sync::mpsc::Receiver<sync::SyncMsg>, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) { app.save_unsaved_on_exit(); return Ok(()); }
        terminal.draw(|f| ui::draw(f, app))?;
        if app.quit { return Ok(()); }
        app.check_autolock();
        // crossterm's poll spins forever once the terminal has hung up; watch for that ourselves
        if input_hung_up(0) {
            app.save_unsaved_on_exit();
            return Ok(());
        }
        let ready = event::poll(Duration::ZERO)?;
        if !ready && input_hung_up(250) {
            app.save_unsaved_on_exit();
            return Ok(());
        }
        if ready {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
                Event::Mouse(m) => app.handle_mouse(m),
                Event::Paste(t) => app.handle_paste(&t),
                _ => {}
            }
        }
        if let Some(id) = app.pending_edit.take() {
            if let Some((content, ext)) = app.edit_payload(&id) {
                suspend(terminal);
                let result = editor::edit_text(&content, ext);
                let back = resume(terminal);
                app.finish_edit(&id, result); // even if the screen could not be restored: the edit must not be lost
                back?;
            }
        }
        while let Ok(m) = rx.try_recv() { app.on_sync(m); }
    }
}

/// True if the terminal has gone away (hangup/error on stdin); waits up to `ms` for that.
fn input_hung_up(ms: i32) -> bool {
    // SAFETY: plain poll(2) on a stack-allocated pollfd.
    unsafe {
        if libc::isatty(0) != 1 { std::thread::sleep(Duration::from_millis(ms as u64)); return false; }
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let n = libc::poll(&mut fd, 1, ms);
        n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }
}
