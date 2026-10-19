//! Session store: append-only JSONL ledger plus content-addressed blobs.
//!
//! Prototype deviation from the reference (section 9.1): JSONL instead of SQLite.
//! Keep this module's API stable so the backend can change later.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rein_core::{Decision, Entry, LEDGER_FORMAT};
use serde_json::{json, Value};

pub struct Store {
    dir: PathBuf,
    ledger: File,
    hooks: Option<File>,
}

fn append_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("blobs"))?;
        // Raw payloads hold code and file contents. Keep them only when asked.
        let capture = std::env::var("REIN_CAPTURE").is_ok_and(|v| v == "1");
        Ok(Self {
            dir: dir.to_path_buf(),
            ledger: append_file(&dir.join("ledger.jsonl"))?,
            hooks: if capture {
                Some(append_file(&dir.join("hooks.jsonl"))?)
            } else {
                None
            },
        })
    }

    /// Keep the raw hook payload. Phase 0 spikes and new fixtures come from this file.
    pub fn capture(&mut self, ts_ms: u64, event: &str, payload: &Value) -> Result<()> {
        if let Some(f) = self.hooks.as_mut() {
            writeln!(
                f,
                "{}",
                json!({ "ts_ms": ts_ms, "event": event, "payload": payload })
            )?;
        }
        Ok(())
    }

    /// Write blobs first, then entries, so every blob an entry names exists.
    pub fn persist(&mut self, d: &Decision) -> Result<()> {
        for b in &d.blobs {
            let p = self.dir.join("blobs").join(&b.hash);
            if !p.exists() {
                fs::write(&p, &b.content)?;
            }
        }
        for e in &d.entries {
            writeln!(self.ledger, "{}", serde_json::to_string(e)?)?;
        }
        Ok(())
    }
}

pub fn read_entries(dir: &Path) -> Result<Vec<Entry>> {
    let path = dir.join("ledger.jsonl");
    let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Check the version before parsing entries: a newer format may not parse at all.
        if out.is_empty() {
            check_format(&line, &path)?;
        }
        match serde_json::from_str(&line) {
            Ok(e) => out.push(e),
            Err(err) => eprintln!("rein: skip ledger line {}: {err}", n + 1),
        }
    }
    Ok(out)
}

/// Reject a ledger written by a newer rein. No `format` entry means version 1.
fn check_format(first_line: &str, path: &Path) -> Result<()> {
    let Ok(v) = serde_json::from_str::<Value>(first_line) else {
        return Ok(());
    };
    if v["body"]["kind"] != "format" {
        return Ok(());
    }
    let version = v["body"]["version"].as_u64().unwrap_or(0);
    if version > u64::from(LEDGER_FORMAT) {
        bail!(
            "{} uses ledger format {version}, but this rein reads format {LEDGER_FORMAT} or older. Upgrade rein to read it.",
            path.display()
        );
    }
    Ok(())
}

pub fn read_blob(dir: &Path, hash: &str) -> Result<String> {
    let p = dir.join("blobs").join(hash);
    fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(name: &str, first: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rein-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let entry = r#"{"seq":1,"ts_ms":0,"body":{"kind":"event","event":{"type":"turn_ended"}}}"#;
        fs::write(d.join("ledger.jsonl"), format!("{first}\n{entry}\n")).unwrap();
        d
    }

    #[test]
    fn reads_current_and_unversioned_ledgers() {
        let current = format!(
            r#"{{"seq":0,"ts_ms":0,"body":{{"kind":"format","version":{LEDGER_FORMAT}}}}}"#
        );
        let d = ledger("current", &current);
        assert_eq!(read_entries(&d).unwrap().len(), 2);
        let old = r#"{"seq":0,"ts_ms":0,"body":{"kind":"event","event":{"type":"turn_ended"}}}"#;
        let d2 = ledger("old", old);
        assert_eq!(read_entries(&d2).unwrap().len(), 2);
        let _ = fs::remove_dir_all(d);
        let _ = fs::remove_dir_all(d2);
    }

    #[test]
    fn rejects_newer_format_with_a_clear_message() {
        let d = ledger(
            "newer",
            r#"{"seq":0,"ts_ms":0,"body":{"kind":"format","version":99}}"#,
        );
        let err = read_entries(&d).unwrap_err().to_string();
        assert!(err.contains("ledger format 99"), "{err}");
        assert!(err.contains("Upgrade rein"), "{err}");
        let _ = fs::remove_dir_all(d);
    }
}
