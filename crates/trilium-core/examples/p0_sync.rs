//! P0 prototype: sync login + full pull against a Trilium server, then verify that our
//! entity hash implementation reproduces the server's `entity_changes.hash` values.
//! usage: p0_sync <base-url> <password>
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

const SYNC_VERSION: u64 = 40;

fn hash10(s: &str) -> String {
    let nfc: String = s.nfc().collect();
    let enc = B64.encode(Sha1::digest(nfc.as_bytes()));
    enc[..10].to_string()
}

/// JS template-literal rendering of a JSON value.
fn js(v: Option<&Value>) -> String {
    match v {
        None => "null".into(), // pulled rows omit NULL columns; the server hashes them as null
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(o) => o.to_string(),
    }
}

/// Booleans in BNote/BAttribute are coerced from 0/1 rows, so try both renderings.
fn entity_hash(entity: &str, row: &Value, deleted: bool, bool_as_words: bool) -> Option<String> {
    let props: &[&str] = match entity {
        "notes" => &["noteId", "title", "isProtected", "type", "mime", "blobId"],
        "branches" => &["branchId", "noteId", "parentNoteId", "prefix"],
        "attributes" => &["attributeId", "noteId", "type", "name", "value", "isInheritable"],
        "attachments" => &["attachmentId", "ownerId", "role", "mime", "title", "blobId", "utcDateScheduledForErasureSince"],
        "revisions" => &["revisionId", "noteId", "title", "description", "source", "isProtected", "dateLastEdited", "dateCreated", "utcDateLastEdited", "utcDateCreated", "utcDateModified", "blobId"],
        _ => return None,
    };
    let mut s = String::new();
    for p in props {
        s.push('|');
        let v = row.get(*p);
        let bool_prop = matches!(*p, "isProtected" | "isInheritable");
        match (bool_prop, bool_as_words, v) {
            (true, true, Some(Value::Number(n))) => s.push_str(if n.as_i64() == Some(0) { "false" } else { "true" }),
            _ => s.push_str(&js(v)),
        }
    }
    if deleted {
        s.push_str("|deleted");
    }
    Some(hash10(&s))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (base, password) = (args[1].trim_end_matches('/'), &args[2]);
    let http = reqwest::blocking::Client::builder().cookie_store(true).build()?;

    // 1. seed
    let cred = B64.encode(format!("u:{password}"));
    let seed: Value = http.get(format!("{base}/api/setup/sync-seed")).header("trilium-cred", cred).send()?.error_for_status()?.json()?;
    let opt = |n: &str| seed["options"].as_array().unwrap().iter().find(|o| o["name"] == n).unwrap()["value"].as_str().unwrap().to_string();
    let secret = opt("documentSecret");

    // 2. login
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3fZ").to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
    mac.update(ts.as_bytes());
    let hash = B64.encode(mac.finalize().into_bytes());
    let login: Value = http.post(format!("{base}/api/login/sync"))
        .json(&serde_json::json!({"timestamp": ts, "syncVersion": SYNC_VERSION, "hash": hash}))
        .send()?.error_for_status()?.json()?;
    println!("login ok: server instance {} maxEntityChangeId {}", login["instanceId"], login["maxEntityChangeId"]);

    // 3. pull everything
    let our_instance = "p0proto0001";
    let mut cursor = 0i64;
    let (mut ok, mut bad) = (BTreeMap::<String, u32>::new(), BTreeMap::<String, u32>::new());
    let mut total = 0;
    loop {
        let r: Value = http.get(format!("{base}/api/sync/changed?instanceId={our_instance}&lastEntityChangeId={cursor}&logMarkerId=p0"))
            .send()?.error_for_status()?.json()?;
        let recs = r["entityChanges"].as_array().unwrap();
        for rec in recs {
            total += 1;
            let ec = &rec["entityChange"];
            let name = ec["entityName"].as_str().unwrap();
            let Some(row) = rec.get("entity").filter(|e| !e.is_null()) else { continue };
            let deleted = row.get("isDeleted").and_then(Value::as_i64) == Some(1);
            let want = ec["hash"].as_str().unwrap();
            // blobs hash differently (content hash); skip here
            let matched = [false, true].iter().any(|w| {
                [deleted, !deleted].iter().any(|d| entity_hash(name, row, *d && deleted || *d, *w).as_deref() == Some(want))
            });
            if !matched && name == "attachments" && std::env::var("DBG").is_ok() { println!("{} want={want}", row); }
            let key = name.to_string();
            if entity_hash(name, row, false, false).is_some() {
                *(if matched { &mut ok } else { &mut bad }).entry(key).or_default() += 1;
            }
        }
        let next = r["lastEntityChangeId"].as_i64().unwrap();
        if recs.is_empty() || next == cursor { break; }
        cursor = next;
    }
    println!("pulled {total} changes up to id {cursor}");
    println!("hash match   : {ok:?}");
    println!("hash MISMATCH: {bad:?}");
    Ok(())
}
