//! Background sync worker. Owns its own database connection (SQLite WAL permits this) and
//! reports to the UI thread over a channel, so the UI never blocks on the network.
use std::{path::PathBuf, sync::mpsc::{Receiver, Sender, channel}, time::Duration};
use trilium_core::{db::Db, sync::SyncClient};

pub enum SyncMsg {
    Syncing,
    Done { pulled: u32, pushed: usize, conflicts: Vec<(String, String)>, overwritten: usize },
    Error(String),
    /// the text-search index has been built; searches may use it
    IndexReady,
}

const INTERVAL: Duration = Duration::from_secs(60);

fn cycle(path: &PathBuf, url: &str) -> anyhow::Result<(u32, usize, Vec<(String, String)>, usize)> {
    let mut db = Db::open(path)?;
    let secret = db.state("documentSecret")?.ok_or_else(|| anyhow::anyhow!("no sync secret: run `trilium-cli sync <url>` once"))?;
    let mut client = SyncClient::new(url, &db.instance_id()?)?;
    let out = client.sync(&mut db, &secret, |_, _| {})?;
    Ok((out.stats.applied, out.pushed, out.stats.conflicts, out.stats.overwritten.len()))
}

/// Returns a sender; sending `()` triggers an immediate sync.
pub fn spawn(path: PathBuf, url: String, tx: Sender<SyncMsg>) -> Sender<()> {
    let (cmd_tx, cmd_rx): (Sender<()>, Receiver<()>) = channel();
    std::thread::spawn(move || loop {
        let _ = tx.send(SyncMsg::Syncing);
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cycle(&path, &url)))
            .unwrap_or_else(|p| {
                let why = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                Err(anyhow::anyhow!("sync worker panicked (will retry): {why}"))
            });
        let msg = match ran {
            Ok((pulled, pushed, conflicts, overwritten)) => SyncMsg::Done { pulled, pushed, conflicts, overwritten },
            Err(e) => SyncMsg::Error(format!("{e:#}")),
        };
        if tx.send(msg).is_err() { return; } // UI gone
        match cmd_rx.recv_timeout(INTERVAL) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            _ => {}
        }
    });
    cmd_tx
}

/// Build/refresh the text-search index off the UI thread (own connection; WAL lets the UI keep reading).
pub fn spawn_indexer(path: PathBuf, tx: Sender<SyncMsg>) {
    std::thread::spawn(move || {
        if let Ok(db) = trilium_core::db::Db::open(&path) {
            let _ = trilium_core::search::index::ensure_index(&db);
            let _ = trilium_core::search::index::prune_index(&db);
        }
        let _ = tx.send(SyncMsg::IndexReady);
    });
}
