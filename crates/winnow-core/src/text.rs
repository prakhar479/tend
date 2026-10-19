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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_progress() {
        let raw = "\x1b[32mok\x1b[0m\n10%\r50%\r100%\n";
        assert_eq!(clean_lines(raw), vec!["ok", "100%"]);
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_hex("winnow"),
            "f1c01ae726fd268ed10606d63225deced6528b0bd1d69417e21c792851d68447"
        );
    }
}
