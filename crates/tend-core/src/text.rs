//! Text helpers shared by perception and shaping.

use sha2::{Digest, Sha256};

/// Remove ANSI escape sequences (colors, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
    re!(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07")
        .replace_all(s, "")
        .into_owned()
}

/// Split output into display lines. For progress bars, keep only the text
/// after the last carriage return in each line.
pub fn clean_lines(s: &str) -> Vec<String> {
    strip_ansi(s)
        .lines()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            match l.rfind('\r') {
                Some(i) => l[i + 1..].to_string(),
                None => l.to_string(),
            }
        })
        .collect()
}

/// Rough token estimate: 4 characters per token. Good enough for ratios and reports.
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Lowercase hex SHA-256. Use this for every hash that reaches the ledger.
pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Cut a string to `max` characters and add an ellipsis if it was longer.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// Replace secrets with `[redacted]` before tend writes text to disk (I6 E8): cloud
/// and API keys and tokens by their shape, private key blocks, passwords in URLs, and
/// the values of variables named like secrets. JSON stays valid: a replacement never
/// contains a quote and never swallows one.
pub fn redact(text: &str) -> std::borrow::Cow<'_, str> {
    let shaped = re!(concat!(
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"|\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        r"|\bgh[pousr]_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{50,}",
        r"|\bsk-ant-[A-Za-z0-9_-]{20,}|\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}",
        r"|\bxox[abposr]-[A-Za-z0-9-]{10,}|\bAIza[0-9A-Za-z_-]{35}",
        r"|\b(?:sk|rk)_live_[0-9a-zA-Z]{20,}|\bglpat-[A-Za-z0-9_-]{20,}",
        r"|\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"
    ));
    let url_password = re!(r"(\b[a-z][a-z0-9+.-]*://[^:/\s@]+:)([^@\s/]{3,})@");
    let assignment = re!(
        r#"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PWD|API_?KEY|ACCESS_KEY|PRIVATE_KEY|CREDENTIALS?)[A-Z0-9_]*)(\s*[=:]\s*)([^\s"'\\,;]{8,})"#
    );
    let mut out = std::borrow::Cow::Borrowed(text);
    if shaped.is_match(&out) {
        out = std::borrow::Cow::Owned(shaped.replace_all(&out, "[redacted]").into_owned());
    }
    if url_password.is_match(&out) {
        out = std::borrow::Cow::Owned(
            url_password
                .replace_all(&out, "${1}[redacted]@")
                .into_owned(),
        );
    }
    if assignment.is_match(&out) {
        let replaced = assignment.replace_all(&out, |c: &regex::Captures| {
            let value = &c[3];
            // Counts and sizes (`max_tokens=4096`) and already-redacted values stay.
            if value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                || value.starts_with("[redacted")
            {
                c[0].to_string()
            } else {
                format!("{}{}[redacted]", &c[1], &c[2])
            }
        });
        out = std::borrow::Cow::Owned(replaced.into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn secrets_are_redacted() {
        let text = concat!(
            "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
            "ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz\n",
            "token ghp_0123456789abcdefghijklmnopqrstuvwxyzAB here\n",
            "DB_PASSWORD: hunter2hunter2\n",
            "git clone https://user:s3cretpass@github.com/x/y\n",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n",
            "max_tokens=4096 tokens: 1234567890 passed\n",
        );
        let r = redact(text);
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "sk-ant-api03",
            "ghp_0123",
            "hunter2",
            "s3cretpass",
            "MIIEpAIBAAKCAQEA",
        ] {
            assert!(!r.contains(secret), "{secret} left in:\n{r}");
        }
        assert!(
            r.contains("DB_PASSWORD: [redacted]")
                && r.contains("https://user:[redacted]@github.com")
        );
        assert!(
            r.contains("max_tokens=4096 tokens: 1234567890 passed"),
            "{r}"
        );
        // JSON stays valid.
        let json = r#"{"command":"export API_KEY=abcdefgh12345678","n":1}"#;
        let v: serde_json::Value = serde_json::from_str(&redact(json)).unwrap();
        assert_eq!(v["command"], "export API_KEY=[redacted]");
        assert!(matches!(
            redact("nothing here"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    use super::*;

    #[test]
    fn strips_ansi_and_progress() {
        let raw = "\x1b[32mok\x1b[0m\n10%\r50%\r100%\n";
        assert_eq!(clean_lines(raw), vec!["ok", "100%"]);
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_hex("tend"),
            "4f0a1f5aef53e953cc28e5086b98eca5d40c8820325fce6a33f34b22c84bbefd"
        );
    }
}
