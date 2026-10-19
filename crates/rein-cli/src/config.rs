//! User config: `~/.config/rein/config.toml` overrides `Config` defaults.
//! The file is flat TOML: `key = number` lines and `#` comments. No tables.
//! A flat format needs no TOML dependency.
//!
//! ```toml
//! shape_threshold_tokens = 2000
//! max_summary_ratio = 0.7
//! loop_threshold = 3
//! tighten_after = 5
//! ```

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rein_core::Config;

/// $REIN_CONFIG, then $XDG_CONFIG_HOME/rein/config.toml, then ~/.config/rein/config.toml.
pub fn path() -> PathBuf {
    if let Some(p) = env::var_os("REIN_CONFIG") {
        return PathBuf::from(p);
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(env::temp_dir);
    base.join("rein/config.toml")
}

/// Defaults when the file does not exist. An error when it exists but is invalid.
pub fn load_from(path: &Path) -> Result<Config> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    parse(&text).with_context(|| format!("config {}", path.display()))
}

/// Never fails: an invalid file is reported on stderr and the defaults are used.
pub fn load() -> Config {
    let p = path();
    load_from(&p).unwrap_or_else(|e| {
        eprintln!("rein: {e:#}; using defaults");
        Config::default()
    })
}

fn parse(text: &str) -> Result<Config> {
    let mut c = Config::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {}: expected `key = value`", n + 1))?;
        let (key, value) = (key.trim(), value.trim());
        let bad = |e: &dyn std::fmt::Display| anyhow!("line {}: {key}: {e}", n + 1);
        match key {
            "shape_threshold_tokens" => {
                c.shape_threshold_tokens = value.parse().map_err(|e| bad(&e))?
            }
            "max_summary_ratio" => {
                let v: f32 = value.parse().map_err(|e| bad(&e))?;
                if !(v > 0.0 && v <= 1.0) {
                    bail!("max_summary_ratio must be in (0, 1], got {v}");
                }
                c.max_summary_ratio = v;
            }
            "loop_threshold" => {
                c.loop_threshold = value.parse().map_err(|e| bad(&e))?;
                if c.loop_threshold == 0 {
                    bail!("loop_threshold must be at least 1");
                }
            }
            "tighten_after" => {
                c.tighten_after = value.parse().map_err(|e| bad(&e))?;
                if c.tighten_after == 0 {
                    bail!("tighten_after must be at least 1");
                }
            }
            other => bail!("line {}: unknown key `{other}`", n + 1),
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_overrides_defaults() {
        let c =
            parse("# tuned\nloop_threshold = 4  # fewer false loops\n\nmax_summary_ratio = 0.5\n")
                .unwrap();
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.max_summary_ratio, 0.5);
        assert_eq!(c.tighten_after, Config::default().tighten_after);
        assert_eq!(c.overrides().len(), 2);
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(parse("loop_treshold = 4").is_err());
        assert!(parse("max_summary_ratio = 1.5").is_err());
        assert!(parse("loop_threshold = 0").is_err());
        assert!(parse("loop_threshold = three").is_err());
        assert!(parse("[section]").is_err());
    }

    #[test]
    fn missing_file_is_defaults() {
        let c = load_from(Path::new("/nonexistent/rein/config.toml")).unwrap();
        assert!(c.overrides().is_empty());
    }
}
