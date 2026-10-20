//! User config: `~/.config/tend/config.toml` overrides `Config` defaults.
//! The file is flat TOML: `key = number` lines and `#` comments. No tables.
//! A flat format needs no TOML dependency.
//!
//! ```toml
//! shape_threshold_tokens = 2000
//! max_summary_ratio = 0.7
//! loop_threshold = 3
//! tighten_after = 5
//! idle_gap_minutes = 10
//! keep_days = 30            # tend claude deletes older sessions; 0 keeps all
//! experimental_audit = false  # audit what each session changed in tests and checks
//! plugin_mode = look         # mode of sessions the Claude Code plugin starts
//! notify = false             # desktop notification on a loop or a blocked edit
//! ```

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use tend_core::Config;

/// $TEND_CONFIG, then $XDG_CONFIG_HOME/tend/config.toml, then ~/.config/tend/config.toml.
pub fn path() -> PathBuf {
    if let Some(p) = env::var_os("TEND_CONFIG") {
        return PathBuf::from(p);
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(env::temp_dir);
    base.join("tend/config.toml")
}

/// The config, and one warning per line that was not used. A missing file gives the
/// defaults; a bad line is skipped and the rest still apply.
pub fn load_from(path: &Path) -> (Config, Vec<String>) {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (
            Config::default(),
            vec![format!("read {}: {e}; using defaults", path.display())],
        ),
    }
}

/// Never fails. Warnings go to stderr: the launcher's reaches the user before claude
/// starts; the daemon's goes to `daemon.log`.
pub fn load() -> Config {
    let p = path();
    let (c, warnings) = load_from(&p);
    for w in warnings {
        eprintln!("tend: config {}: {w}", p.display());
    }
    c
}

const KEYS: [&str; 9] = [
    "shape_threshold_tokens",
    "max_summary_ratio",
    "loop_threshold",
    "tighten_after",
    "idle_gap_minutes",
    "keep_days",
    "experimental_audit",
    "plugin_mode",
    "notify",
];

fn parse(text: &str) -> (Config, Vec<String>) {
    let mut c = Config::default();
    let mut warnings = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Err(e) = apply(&mut c, line, &mut seen) {
            warnings.push(format!("line {}: {e}; line ignored", n + 1));
        }
    }
    (c, warnings)
}

fn apply(c: &mut Config, line: &str, seen: &mut std::collections::BTreeSet<String>) -> Result<()> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `key = value`"))?;
    let (key, value) = (key.trim(), value.trim());
    if !KEYS.contains(&key) {
        match KEYS.iter().find(|k| edit_distance(k, key) <= 2) {
            Some(k) => bail!("unknown key `{key}` (did you mean `{k}`?)"),
            None => bail!("unknown key `{key}`"),
        }
    }
    if !seen.insert(key.to_string()) {
        bail!("`{key}` is set twice; the first value stays");
    }
    let int = |min: u64, max: u64| -> Result<u64> {
        let v: u64 = value.parse().map_err(|e| anyhow!("{key}: {e}"))?;
        if !(min..=max).contains(&v) {
            bail!("{key} must be in {min}..={max}, got {v}");
        }
        Ok(v)
    };
    match key {
        "shape_threshold_tokens" => c.shape_threshold_tokens = int(100, 10_000_000)? as usize,
        "max_summary_ratio" => {
            let v: f32 = value.parse().map_err(|e| anyhow!("{key}: {e}"))?;
            if !(v > 0.0 && v <= 1.0) {
                bail!("max_summary_ratio must be in (0, 1], got {v}");
            }
            c.max_summary_ratio = v;
        }
        "loop_threshold" => c.loop_threshold = int(1, 100)?,
        "tighten_after" => c.tighten_after = int(1, 1000)? as u32,
        "idle_gap_minutes" => c.idle_gap_minutes = int(1, 24 * 60)?,
        "keep_days" => c.keep_days = int(0, 36_500)?,
        "plugin_mode" => {
            c.plugin_mode = match value.trim_matches('"') {
                "look" => tend_core::Mode::Look,
                "optimize" => tend_core::Mode::Optimize,
                v => bail!("{key} must be look or optimize, got {v}"),
            }
        }
        "notify" => {
            c.notify = match value {
                "true" => true,
                "false" => false,
                v => bail!("{key} must be true or false, got {v}"),
            }
        }
        "experimental_audit" => {
            c.experimental_audit = match value {
                "true" => true,
                "false" => false,
                v => bail!("{key} must be true or false, got {v}"),
            }
        }
        _ => unreachable!("checked against KEYS"),
    }
    Ok(())
}

/// Levenshtein distance, for "did you mean".
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The repo policy for `dir`: the nearest `.tend.toml` in `dir` or above, up to the
/// git root (a directory with `.git`). Returns the policy (empty if there is none) and
/// warnings, each naming the file.
pub fn load_policy(dir: &Path) -> (tend_core::policy::Policy, Vec<String>) {
    let mut at = Some(dir);
    while let Some(d) = at {
        let file = d.join(".tend.toml");
        if let Ok(text) = fs::read_to_string(&file) {
            let (p, w) = tend_core::policy::parse(&text, &d.to_string_lossy());
            let w = w
                .into_iter()
                .map(|w| format!("{}: {w}", file.display()))
                .collect();
            return (p, w);
        }
        if d.join(".git").exists() {
            break;
        }
        at = d.parent();
    }
    (tend_core::policy::Policy::default(), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_overrides_defaults() {
        let (c, w) =
            parse("# tuned\nloop_threshold = 4  # fewer false loops\n\nmax_summary_ratio = 0.5\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.max_summary_ratio, 0.5);
        assert_eq!(c.tighten_after, Config::default().tighten_after);
        assert_eq!(c.overrides().len(), 2);
    }

    /// One bad line no longer throws away the good ones (U6).
    #[test]
    fn bad_lines_are_skipped_with_a_warning() {
        let (c, w) = parse(
            "loop_threshold = 4\nshape_threshold_tokens = 5000\nloop_treshold = 4\nmax_summary_ratio = 1.5\nidle_gap_minutes = 0\nloop_threshold = 9\n[section]\n",
        );
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.shape_threshold_tokens, 5000);
        assert_eq!(c.idle_gap_minutes, Config::default().idle_gap_minutes);
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w[0].contains("did you mean `loop_threshold`"), "{w:?}");
        assert!(w[3].contains("set twice"), "{w:?}");
        assert!(parse("shape_threshold_tokens = 0").1[0].contains("100..="));
        assert!(parse("loop_threshold = three").1.len() == 1);
    }

    #[test]
    fn missing_file_is_defaults() {
        let (c, w) = load_from(Path::new("/nonexistent/tend/config.toml"));
        assert!(c.overrides().is_empty() && w.is_empty());
    }
}
