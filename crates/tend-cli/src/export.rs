//! `tend export`: one session as a sanitized JSON bundle, for a bug report or to share
//! what tend did (I6 E10). Paths under the project become `/project/…`, the home
//! directory `~`, the user name `user`; secrets are redacted (they already are on disk).

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

pub fn export(dir: &Path, outputs: bool) -> Result<String> {
    let entries = crate::store::read_entries(dir)?;
    let cwd = crate::sessions::session_cwd(dir).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    let clean = |text: &str| -> String {
        let mut t = tend_core::text::redact(text).into_owned();
        if cwd.len() > 1 {
            t = t.replace(&cwd, "/project");
        }
        if home.len() > 1 {
            t = t.replace(&home, "~");
        }
        if user.len() > 2 {
            t = t.replace(&user, "user");
        }
        t
    };
    let ledger: Vec<Value> = entries
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .filter_map(|line| serde_json::from_str(&clean(&line)).ok())
        .collect();
    let mut blobs = serde_json::Map::new();
    if outputs {
        for entry in &entries {
            if let tend_core::Body::Output { blob, .. } = &entry.body {
                if let Ok(text) = crate::store::read_blob(dir, blob) {
                    blobs.insert(blob.clone(), Value::String(clean(&text)));
                }
            }
        }
    }
    let bundle = json!({
        "tend_export": 1,
        "tend_version": env!("CARGO_PKG_VERSION"),
        "session": dir.file_name().map(|n| n.to_string_lossy().to_string()),
        "ledger": ledger,
        "outputs": blobs,
    });
    Ok(serde_json::to_string_pretty(&bundle)?)
}
