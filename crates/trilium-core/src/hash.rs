//! Hash rules that must match Trilium byte for byte, otherwise the server's sector
//! checks (`/api/sync/check`) report divergence. Derived from
//! `becca/entities/*.ts` (`hashedProperties`), `abstract_becca_entity.ts` and `blob.ts`.
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::Value;
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

/// Trilium's `hash()`: base64(sha1(NFC(text))).
pub fn hash_full(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    B64.encode(Sha1::digest(nfc.as_bytes()))
}

/// Entity hashes are truncated to 10 chars.
pub fn hash10(text: &str) -> String {
    hash_full(text)[..10].to_string()
}

/// How a property that is absent from a row renders in the hashed string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Missing {
    /// `null`: what an entity reloaded from the DB hashes as.
    Null,
    /// `undefined`: what a freshly constructed entity hashes as when the field was never set.
    Undefined,
}

fn render(v: Option<&Value>, missing: Missing) -> String {
    match v {
        None => match missing {
            Missing::Null => "null".into(),
            Missing::Undefined => "undefined".into(),
        },
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(o) => o.to_string(),
    }
}

pub fn hashed_properties(entity: &str) -> Option<&'static [&'static str]> {
    Some(match entity {
        "notes" => &["noteId", "title", "isProtected", "type", "mime", "blobId"],
        "branches" => &["branchId", "noteId", "parentNoteId", "prefix"],
        "attributes" => &["attributeId", "noteId", "type", "name", "value", "isInheritable"],
        "attachments" => &["attachmentId", "ownerId", "role", "mime", "title", "blobId", "utcDateScheduledForErasureSince"],
        "revisions" => &[
            "revisionId", "noteId", "title", "description", "source", "isProtected", "dateLastEdited",
            "dateCreated", "utcDateLastEdited", "utcDateCreated", "utcDateModified", "blobId",
        ],
        "options" => &["name", "value"],
        _ => return None,
    })
}

/// Entity hash for an entity row. `None` for entities with other rules (blobs, reordering).
pub fn entity_hash(entity: &str, row: &Value, is_deleted: bool, missing: Missing) -> Option<String> {
    let props = hashed_properties(entity)?;
    let mut s = String::new();
    for p in props {
        s.push('|');
        if matches!(*p, "isProtected" | "isInheritable") {
            // entity classes coerce these with `!!x`, so they hash as "true"/"false", not 0/1
            let truthy = match row.get(*p) {
                Some(Value::Bool(b)) => *b,
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
                _ => false,
            };
            s.push_str(if truthy { "true" } else { "false" });
        } else {
            s.push_str(&render(row.get(*p), missing));
        }
    }
    if is_deleted {
        s.push_str("|deleted");
    }
    Some(hash10(&s))
}

/// Blob hash (`calculateContentHash`): full sha1 of `blobId|content[|textRepresentation]`,
/// content decoded as lossy UTF-8 exactly like `Buffer.toString()`.
pub fn blob_hash(blob_id: &str, content: &[u8], text_representation: Option<&str>) -> String {
    let mut s = format!("{blob_id}|{}", String::from_utf8_lossy(content));
    if let Some(t) = text_representation.filter(|t| !t.is_empty()) {
        s.push('|');
        s.push_str(t);
    }
    hash_full(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Real values captured from a Trilium v0.106.0 server (see docs/P0-findings.md §10).
    #[test]
    fn booleans_hash_as_words() {
        let a = json!({"noteId":"n","title":"t","isProtected":0,"type":"text","mime":"text/html","blobId":"b"});
        let b = json!({"noteId":"n","title":"t","isProtected":false,"type":"text","mime":"text/html","blobId":"b"});
        assert_eq!(entity_hash("notes", &a, false, Missing::Null), entity_hash("notes", &b, false, Missing::Null));
        assert_eq!(hash10("|n|t|false|text|text/html|b"), entity_hash("notes", &a, false, Missing::Null).unwrap());
    }

    #[test]
    fn attachment_fresh_vs_reloaded() {
        let row = json!({"attachmentId":"5CFyWzchOLw7","ownerId":"uOfJ0pKVvALR","role":"image","mime":"image/jpg",
            "title":"codenames.jpg","blobId":"LrS1YPSpuxzzQeELzLW3","utcDateScheduledForErasureSince":null});
        assert_eq!(entity_hash("attachments", &row, false, Missing::Null).unwrap(), "TjuQhy0SHu");
        let mut fresh = row.clone();
        fresh.as_object_mut().unwrap().remove("utcDateScheduledForErasureSince");
        assert_eq!(entity_hash("attachments", &fresh, false, Missing::Undefined).unwrap(), "PpXJXTbEoq");
    }
}
