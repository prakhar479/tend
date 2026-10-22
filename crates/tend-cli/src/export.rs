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
            t = replace_path(&t, &cwd, "/project");
        }
        if home.len() > 1 {
            t = replace_path(&t, &home, "~");
        }
        if user.len() > 2 {
            t = t.replace(&user, "user");
        }
        t
    };
    // Clean the values, not the JSON text: a short user name (`seq`) must not rename keys.
    let ledger: Vec<Value> = entries
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .map(|mut v| {
            clean_strings(&mut v, &clean);
            v
        })
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

/// Replace `prefix` (a directory, trailing slashes ignored) where it ends at a path
/// boundary: `/work/app` is replaced in `/work/app/x`, not in `/work/app2`.
fn replace_path(text: &str, prefix: &str, with: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let end = i + prefix.len();
        let after = rest[end..].chars().next();
        out.push_str(&rest[..i]);
        if after.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))) {
            out.push_str(with);
        } else {
            out.push_str(prefix);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Apply `clean` to every string in `v`, keys left as they are.
fn clean_strings(v: &mut Value, clean: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = clean(s),
        Value::Array(a) => a.iter_mut().for_each(|x| clean_strings(x, clean)),
        Value::Object(o) => o.values_mut().for_each(|x| clean_strings(x, clean)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_replaced_at_boundaries_only() {
        assert_eq!(
            replace_path("/w/app/x /w/app2 /w/app", "/w/app/", "/project"),
            "/project/x /w/app2 /project"
        );
    }

    #[test]
    fn a_user_name_that_is_also_a_key_does_not_rename_keys() {
        let mut v = serde_json::json!({"seq": 1, "body": {"path": "/home/seq/x", "seq": "seq"}});
        clean_strings(&mut v, &|t: &str| t.replace("seq", "user"));
        assert_eq!(
            v,
            serde_json::json!({"seq": 1, "body": {"path": "/home/user/x", "seq": "user"}})
        );
    }
}
