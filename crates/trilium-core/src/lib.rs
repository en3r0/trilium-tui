//! Core library: Trilium-compatible local database, hashing, and sync client.
pub mod anytype;
pub mod anytype_pb;
pub mod crypto;
pub mod db;
pub mod edit;
pub mod enexout;
pub mod fileconv;
pub mod formats;
pub mod hash;
pub mod htmlmd;
pub mod htmltree;
pub mod journal;
pub mod keep;
pub mod mdhtml;
pub mod model;
pub mod notion;
pub mod obsidian;
pub mod panels;
pub mod protect;
pub mod search;
pub mod spreadsheet;
pub mod share;
pub mod transfer;
pub mod sync;
pub mod write;
pub mod xlsx;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Default replica location: `$XDG_DATA_HOME/trilium-tui/default.db`.
pub fn default_db_path() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("trilium-tui").join("default.db")
}

/// Create (if missing) an empty local-only database with a root note, so the app works without any server.
pub fn init_local_db(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    let db = db::Db::open(path)?;
    if db.state("instanceId")?.is_none() { db.set_state("instanceId", &write::new_entity_id().to_lowercase())?; }
    db.ensure_root()?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}
