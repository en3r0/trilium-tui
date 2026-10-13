//! Sync client (pull side): seed, login, chunked pull, and server-side hash verification.
use crate::db::{ApplyStats, Db};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;

/// The sync protocol version of the newest Trilium source (app_info.ts); servers refuse other versions.
pub const SYNC_VERSION: u64 = 40;
/// Versions we can speak: 39 is every released server up to v0.106.0, 40 only changed the include-note content format.
const SUPPORTED_SYNC_VERSIONS: &[u64] = &[39, 40];

/// The server's own version from its "Non-matching sync versions, local is version 39, remote is 40" refusal.
fn server_sync_version(msg: &str) -> Option<u64> {
    let rest = msg.split("local is version ").nth(1)?;
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// A server's error text for display: untrusted, so short and without control characters (escape sequences, clipboard writes).
fn short(body: &str) -> String {
    let cut: String = body.chars().take(200).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if body.chars().nth(200).is_some() { format!("{cut}…") } else { cut }
}

pub struct SyncClient {
    http: reqwest::blocking::Client,
    base: String,
    pub server_instance: Option<String>,
    local_instance: String,
    /// server database version seen at the last login
    pub last_db_version: Option<i64>,
    /// highest entity change id the server reported at login
    pub server_max_change: Option<i64>,
    /// the protocol version the server accepted last time: a wrong guess is a failed login, and the server stops answering
    /// a client after 10 failed logins in 15 minutes (`createLoginRateLimiter`), so a released server must not be tried with
    /// the development version on every cycle
    sync_version: u64,
}

/// Trilium's sync.ts limits.
pub const MAX_SYNC_ROUNDS: u32 = 50;
pub const MAX_SECTOR_RESYNC_ATTEMPTS: u32 = 3;

#[derive(Default)]
struct CheckRound {
    /// another round can achieve something (changes still pending, or sectors were re-queued)
    again: bool,
    requeued: usize,
    /// sectors that used up their repair attempts
    give_up: Vec<(String, String)>,
    compared: usize,
}

#[derive(Default, Debug)]
pub struct SyncOutcome {
    pub stats: ApplyStats,
    pub pushed: usize,
    pub rounds: u32,
    /// hash sectors that differed and were queued for sending again
    pub repaired: usize,
    pub sectors_checked: usize,
}

#[derive(Debug)]
pub struct CheckReport {
    pub failed: Vec<(String, String)>, // (entityName, sector)
    pub sectors_checked: usize,
}

impl SyncClient {
    pub fn new(base: &str, local_instance: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::blocking::Client::builder().cookie_store(true).build()?,
            base: base.trim_end_matches('/').to_string(),
            server_instance: None,
            local_instance: local_instance.to_string(),
            last_db_version: None,
            server_max_change: None,
            sync_version: SYNC_VERSION,
        })
    }

    /// First-time setup: fetch `(documentId, documentSecret)` using the account password.
    pub fn fetch_seed(&self, password: &str) -> Result<(String, String)> {
        let cred = B64.encode(format!("u:{password}"));
        let r = self.http.get(format!("{}/api/setup/sync-seed", self.base)).header("trilium-cred", cred).send()?;
        if !r.status().is_success() { bail!("sync-seed failed: HTTP {} {}", r.status(), short(&r.text().unwrap_or_default())); }
        let v: Value = r.json()?;
        let get = |n: &str| -> Result<String> {
            v["options"].as_array().and_then(|a| a.iter().find(|o| o["name"] == n))
                .and_then(|o| o["value"].as_str()).map(String::from).with_context(|| format!("seed lacks {n}"))
        };
        Ok((get("documentId")?, get("documentSecret")?))
    }

    pub fn login(&mut self, document_secret: &str) -> Result<i64> {
        let mut version = self.sync_version;
        let mut retried = false;
        let r = loop {
            let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3fZ").to_string();
            // key = the secret string's own bytes (not base64-decoded), exactly as the server does
            let mut mac = Hmac::<Sha256>::new_from_slice(document_secret.as_bytes())?;
            mac.update(ts.as_bytes());
            let r = self.http.post(format!("{}/api/login/sync", self.base))
                .json(&json!({"timestamp": ts, "syncVersion": version, "hash": B64.encode(mac.finalize().into_bytes())}))
                .send()?;
            if r.status().is_success() { break r; }
            let (status, body) = (r.status(), r.text().unwrap_or_default());
            // an older (released) server: speak its version once if we know it
            // (once: a server that keeps changing its answer is not followed back and forth)
            match server_sync_version(&body).filter(|v| *v != version && SUPPORTED_SYNC_VERSIONS.contains(v)) {
                Some(v) if status.as_u16() == 400 && !retried => { version = v; retried = true; }
                _ => bail!("sync login failed: HTTP {status} {}", short(&body)),
            }
        };
        self.sync_version = version;
        let v: Value = r.json()?;
        let server_instance = v["instanceId"].as_str().context("instanceId")?.to_string();
        // sync.ts doLogin: a client pointed at itself (or at a copy of its own database) would otherwise "sync" with nothing
        if server_instance == self.local_instance {
            bail!("sync server has instance ID '{server_instance}' which is also local; the sync URL probably points back to this replica");
        }
        self.server_instance = Some(server_instance);
        // remember the server's database version (the session cookie from login authorises /api/app-info)
        if let Ok(r) = self.http.get(format!("{}/api/app-info", self.base)).send() {
            if let Some(dbv) = r.json::<Value>().ok().and_then(|j| j["dbVersion"].as_i64()) { self.last_db_version = Some(dbv); }
        }
        let max = v["maxEntityChangeId"].as_i64().unwrap_or(0);
        self.server_max_change = Some(max);
        Ok(max)
    }

    /// Pull until the server has nothing newer; returns accumulated stats. `progress(applied_so_far)`.
    pub fn pull(&self, db: &mut Db, mut progress: impl FnMut(&ApplyStats, i64)) -> Result<ApplyStats> {
        let server = self.server_instance.clone().context("not logged in")?;
        if let Some(v) = self.last_db_version { db.set_state("serverDbVersion", &v.to_string())?; }
        let mut cursor: i64 = db.state("lastSyncedPull")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        // a server restored from a backup (or rebuilt) can be behind our cursor: its newer changes would never be seen
        if let Some(max) = self.server_max_change { if cursor > max { cursor = 0; db.set_state("lastSyncedPull", "0")?; } }
        let mut batches = 0u32;
        let mut total = ApplyStats::default();
        loop {
            let url = format!("{}/api/sync/changed?instanceId={}&lastEntityChangeId={cursor}&logMarkerId=tui", self.base, self.local_instance);
            let r: Value = self.http.get(url).send()?.error_for_status()?.json()?;
            let recs = r["entityChanges"].as_array().context("entityChanges")?;
            // a server that never runs out of "outstanding" changes but sends none must not hold us here for ever
            batches += 1;
            if recs.is_empty() || batches > 100_000 {
                // an empty answer still moves the cursor when the server skipped past our own changes (it only does that
                // when nothing foreign is left up to there); without this the content check below sees "outstanding pulls"
                // for ever after every push
                if let Some(next) = r["lastEntityChangeId"].as_i64().filter(|n| *n > cursor && recs.is_empty()) {
                    db.set_state("lastSyncedPull", &next.to_string())?;
                }
                break;
            }
            let s = db.apply_changes(&server, recs)?;
            // keep the replaced text on disk before anything else can fail, and before the cursor moves on
            // (the batch is already committed, so a failure here must be reported, not allowed to stall the sync)
            let mut s = s;
            if let Err(e) = db.save_overwritten(&s.overwritten) {
                for (_, id, title, _) in &s.overwritten { s.conflicts.push((id.clone(), format!("{title}: replaced local text could not be kept ({e:#})"))); }
            }
            total.applied += s.applied; total.skipped_known += s.skipped_known;
            total.local_newer += s.local_newer; total.erased += s.erased; total.conflicts.extend(s.conflicts); total.overwritten.extend(s.overwritten);
            let next = r["lastEntityChangeId"].as_i64().context("lastEntityChangeId")?;
            // persist the cursor only after the batch committed
            db.set_state("lastSyncedPull", &next.to_string())?;
            progress(&total, r["outstandingPullCount"].as_i64().unwrap_or(0));
            if next == cursor || r["outstandingPullCount"].as_i64() == Some(0) { break; }
            cursor = next;
        }
        // the server drops blobs nothing refers to any more without telling us: do the same (only once everything referring to
        // blobs has been pulled), so superseded content, plaintext of a note that was just protected included, doesn't linger
        db.sweep_orphan_blobs()?;
        Ok(total)
    }

    /// Push local changes the server hasn't seen (everything after `lastSyncedPush` not originating from the server).
    pub fn push(&self, db: &Db) -> Result<usize> {
        let server = self.server_instance.clone().context("not logged in")?;
        let mut last: i64 = db.state("lastSyncedPush")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut pushed = 0;
        loop {
            let mut stmt = db.conn.prepare(
                "SELECT id,entityName,entityId,hash,isErased,changeId,componentId,instanceId,isSynced,utcDateChanged
                 FROM entity_changes WHERE isSynced=1 AND id>? ORDER BY id LIMIT 1000")?;
            let ecs: Vec<Value> = stmt.query_map([last], |r| Ok(json!({
                "id": r.get::<_, i64>(0)?, "entityName": r.get::<_, String>(1)?, "entityId": r.get::<_, String>(2)?,
                "hash": r.get::<_, String>(3)?, "isErased": r.get::<_, i64>(4)?, "changeId": r.get::<_, String>(5)?,
                "componentId": r.get::<_, String>(6)?, "instanceId": r.get::<_, String>(7)?,
                "isSynced": r.get::<_, i64>(8)?, "utcDateChanged": r.get::<_, String>(9)?,
            })))?.collect::<rusqlite::Result<_>>()?;
            if ecs.is_empty() { break; }
            let batch_last = ecs.last().unwrap()["id"].as_i64().unwrap();
            let mut records = Vec::new();
            for ec in ecs.into_iter().filter(|e| e["instanceId"] != server.as_str()) {
                if ec["isErased"].as_i64() == Some(1) { records.push(json!({"entityChange": ec})); continue; }
                let name = ec["entityName"].as_str().unwrap().to_string();
                let id = ec["entityId"].as_str().unwrap().to_string();
                let entity = if name == "note_reordering" {
                    let mut stmt = db.conn.prepare("SELECT branchId, notePosition FROM branches WHERE parentNoteId=? AND isDeleted=0")?;
                    let m: serde_json::Map<String, Value> = stmt.query_map([&id], |r| Ok((r.get::<_, String>(0)?, Value::from(r.get::<_, i64>(1)?))))?.collect::<rusqlite::Result<_>>()?;
                    Value::Object(m)
                } else {
                    let pk = crate::db::primary_key(&name).with_context(|| format!("unknown entity {name}"))?;
                    match db.row_json(&name, pk, &id)? { Some(r) => r, None => continue }
                };
                records.push(json!({"entityChange": ec, "entity": entity}));
            }
            if !records.is_empty() {
                pushed += records.len();
                self.put_json(&format!("{}/api/sync/update?logMarkerId=tui", self.base),
                    &json!({"entities": records, "instanceId": self.local_instance}).to_string())?;
            }
            db.set_state("lastSyncedPush", &batch_last.to_string())?;
            last = batch_last;
        }
        Ok(pushed)
    }

    /// PUT with Trilium's paging protocol (headers pageCount/pageIndex/requestId, 1,000,000-char pages).
    fn put_json(&self, url: &str, body: &str) -> Result<()> {
        const PAGE: usize = 1_000_000;
        let mut pages: Vec<&str> = Vec::new();
        let mut start = 0;
        while start < body.len() {
            let mut end = (start + PAGE).min(body.len());
            while !body.is_char_boundary(end) { end -= 1; }
            pages.push(&body[start..end]);
            start = end;
        }
        if pages.is_empty() { pages.push(""); }
        let request_id: String = crate::write::new_entity_id()[..10].to_string();
        for (i, page) in pages.iter().enumerate() {
            let r = self.http.put(url)
                .header("Content-Type", if pages.len() == 1 { "application/json" } else { "text/plain" })
                .header("pageCount", pages.len()).header("pageIndex", i).header("requestId", &request_id)
                .body(page.to_string()).send()?;
            if !r.status().is_success() { bail!("push failed: HTTP {} {}", r.status(), short(&r.text().unwrap_or_default())); }
        }
        Ok(())
    }

    /// One complete sync, the way Trilium's own client runs it (sync.ts `sync()`): login, pull, push, tell the server we
    /// are done, verify the per-sector content hashes, and when sectors differ re-queue them on both sides and go round
    /// again (at most [`MAX_SYNC_ROUNDS`] rounds, at most [`MAX_SECTOR_RESYNC_ATTEMPTS`] repairs per sector).
    ///
    /// Deliberate difference: Trilium pushes before it pulls; we pull first, so that `apply_changes` still sees our unpushed
    /// edits and can keep the text a newer remote change replaces. The result is the same last-writer-wins state.
    pub fn sync(&mut self, db: &mut Db, secret: &str, mut progress: impl FnMut(&ApplyStats, i64)) -> Result<SyncOutcome> {
        let mut out = SyncOutcome::default();
        if let Some(v) = db.state("serverSyncVersion")?.and_then(|s| s.parse().ok()).filter(|v| SUPPORTED_SYNC_VERSIONS.contains(v)) { self.sync_version = v; }
        let mut attempts: BTreeMap<(String, String), u32> = BTreeMap::new();
        let mut unresolvable: Vec<(String, String)> = Vec::new();
        loop {
            out.rounds += 1;
            if out.rounds > MAX_SYNC_ROUNDS { bail!("sync did not converge after {MAX_SYNC_ROUNDS} rounds, giving up for now"); }
            self.login(secret)?;
            if db.state("serverSyncVersion")?.as_deref() != Some(&self.sync_version.to_string()) { db.set_state("serverSyncVersion", &self.sync_version.to_string())?; }
            let s = self.pull(db, &mut progress)?;
            out.stats.applied += s.applied; out.stats.skipped_known += s.skipped_known; out.stats.local_newer += s.local_newer;
            out.stats.erased += s.erased; out.stats.conflicts.extend(s.conflicts); out.stats.overwritten.extend(s.overwritten);
            out.pushed += self.push(db)?;
            self.http.post(format!("{}/api/sync/finished", self.base)).send()?.error_for_status()?;
            let round = self.check_round(db, &mut attempts)?;
            out.sectors_checked = round.compared.max(out.sectors_checked);
            out.repaired += round.requeued;
            unresolvable.extend(round.give_up);
            if !round.again { break; }
        }
        if !unresolvable.is_empty() {
            let list: Vec<String> = unresolvable.iter().map(|(n, s)| format!("{n}/{s}")).collect();
            bail!("content hashes of {} sector(s) still differ from the server after {MAX_SECTOR_RESYNC_ATTEMPTS} repair attempts: {}", list.len(), list.join(", "));
        }
        Ok(out)
    }

    /// True while the content check cannot run yet: changes are still waiting to be pulled or pushed (sync.ts
    /// `checkContentHash` returns `continueSync: true` for both).
    fn pending_pull_or_push(&self, db: &Db) -> Result<bool> {
        let r: Value = self.http.get(format!("{}/api/sync/check", self.base)).send()?.error_for_status()?.json()?;
        let max = r["maxEntityChangeId"].as_i64().unwrap_or(0);
        let pulled: i64 = db.state("lastSyncedPull")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let pushed: i64 = db.state("lastSyncedPush")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let unpushed: i64 = db.conn.query_row("SELECT EXISTS(SELECT 1 FROM entity_changes WHERE isSynced=1 AND id>?)", [pushed], |r| r.get(0))?;
        Ok(pulled < max || unpushed != 0)
    }

    /// sync.ts `checkContentHash`.
    fn check_round(&self, db: &mut Db, attempts: &mut BTreeMap<(String, String), u32>) -> Result<CheckRound> {
        if self.pending_pull_or_push(db)? { return Ok(CheckRound { again: true, ..Default::default() }); }
        let report = self.check(db)?;
        let (mut retry, mut give_up) = (Vec::new(), Vec::new());
        for (name, sector) in report.failed {
            // names come from the server's answer: only plain identifiers go into a URL path
            let plain = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !plain(&name) || !plain(&sector) { give_up.push((name, sector)); continue; }
            let n = attempts.entry((name.clone(), sector.clone())).or_insert(0);
            *n += 1;
            if *n < MAX_SECTOR_RESYNC_ATTEMPTS { retry.push((name, sector)); }
            else if *n == MAX_SECTOR_RESYNC_ATTEMPTS { give_up.push((name, sector)); }
        }
        if !retry.is_empty() {
            self.http.post(format!("{}/api/sync/check-entity-changes", self.base)).send()?.error_for_status()?;
        }
        for (name, sector) in &retry {
            db.requeue_sector(name, sector)?;
            self.http.post(format!("{}/api/sync/queue-sector/{name}/{sector}", self.base)).send()?.error_for_status()?;
        }
        Ok(CheckRound { again: !retry.is_empty(), requeued: retry.len(), give_up, compared: report.sectors_checked })
    }

    /// Compare our sector hashes with the server's `/api/sync/check`.
    pub fn check(&self, db: &Db) -> Result<CheckReport> {
        let r: Value = self.http.get(format!("{}/api/sync/check", self.base)).send()?.error_for_status()?.json()?;
        let remote: BTreeMap<String, BTreeMap<String, String>> = serde_json::from_value(r["entityHashes"].clone())?;
        let local = db.sector_hashes()?;
        let (mut failed, mut checked) = (Vec::new(), 0);
        // the server only iterates over its own entity names, mirror that plus ours
        let names: std::collections::BTreeSet<_> = local.keys().chain(remote.keys()).cloned().collect();
        for n in names {
            let (l, rm) = (local.get(&n), remote.get(&n));
            let sectors: std::collections::BTreeSet<_> = l.into_iter().flat_map(|m| m.keys()).chain(rm.into_iter().flat_map(|m| m.keys())).cloned().collect();
            for s in sectors {
                checked += 1;
                if l.and_then(|m| m.get(&s)) != rm.and_then(|m| m.get(&s)) { failed.push((n.clone(), s)); }
            }
        }
        Ok(CheckReport { failed, sectors_checked: checked })
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn the_servers_version_is_read_from_the_refusal() {
        let m = r#"{"message":"Non-matching sync versions, local is version 39, remote is 40. It is recommended to run same version of Trilium on both sides of sync."}"#;
        assert_eq!(server_sync_version(m), Some(39));
        assert_eq!(server_sync_version("nope"), None);
        assert!(SUPPORTED_SYNC_VERSIONS.contains(&SYNC_VERSION));
    }

    #[test]
    fn the_accepted_version_is_remembered_so_later_logins_do_not_fail_first() {
        // a released server answers the development version with a 400, and counts it towards its login rate limit
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rejected = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = rejected.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains("/api/login/sync") && req.contains("\"syncVersion\":40") {
                    r2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ("400 Bad Request", r#"{"message":"Non-matching sync versions, local is version 39, remote is 40."}"#.to_string())
                } else if req.contains("/api/login/sync") {
                    ("200 OK", r#"{"instanceId":"srv","maxEntityChangeId":1}"#.to_string())
                } else { ("200 OK", "{}".to_string()) };
                let _ = s.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
            }
        });
        let mut c = SyncClient::new(&format!("http://127.0.0.1:{port}"), "inst").unwrap();
        c.login("secret").unwrap();
        c.login("secret").unwrap();
        c.login("secret").unwrap();
        assert_eq!(rejected.load(std::sync::atomic::Ordering::SeqCst), 1, "only the first login guesses wrong");
    }

    #[test]
    fn a_server_that_flip_flops_is_followed_only_once() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let v = if n % 2 == 0 { 39 } else { 40 };
                let body = format!(r#"{{"message":"Non-matching sync versions, local is version {v}, remote is 0."}}"#);
                let _ = s.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
                if n > 10 { break; }
            }
        });
        let mut c = SyncClient::new(&format!("http://127.0.0.1:{port}"), "inst").unwrap();
        assert!(c.login("secret").is_err());
        assert!(hits.load(std::sync::atomic::Ordering::SeqCst) <= 2, "one retry at most");
    }
}
