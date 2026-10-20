//! `tend doctor`: check the install and the environment tend runs in (I6, U10).
//! Each line says ok, warn, or info. It changes nothing.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{config, paths, sessions};

/// The report, and whether everything that must work does.
pub fn run() -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let mut line = |status: &str, text: String| {
        if status == "FAIL" {
            healthy = false;
        }
        let _ = writeln!(out, "{status:<5} {text}");
    };

    // The `tend` the agent will run (hooks call this binary by absolute path; the agent's
    // `tend show` goes through PATH).
    let exe = env::current_exe().ok();
    match (find_on_path("tend"), &exe) {
        (Some(p), Some(e)) if same_file(&p, e) => {
            line("ok", format!("tend {} at {}", env!("CARGO_PKG_VERSION"), e.display()))
        }
        (Some(p), Some(e)) => line(
            "warn",
            format!(
                "`tend` on PATH is {}, not this binary ({}); reinstall with `cargo install --path crates/tend-cli`",
                p.display(),
                e.display()
            ),
        ),
        (None, _) => line("warn", "`tend` is not on PATH".into()),
        _ => {}
    }

    let bin = env::var("TEND_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    match Command::new(&bin).arg("--version").output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .to_string();
            let checked = env!("TEND_CONFORMANT").split(',').any(|c| c == v);
            if checked {
                line("ok", format!("Claude Code {v} (conformance checked)"));
            } else {
                line(
                    "warn",
                    format!(
                        "Claude Code {v} has no conformance record; run scripts/conformance.py"
                    ),
                );
            }
        }
        _ => line(
            "FAIL",
            format!("`{bin}` not found or failed (set TEND_CLAUDE_BIN)"),
        ),
    }

    let home = paths::home();
    match fs::symlink_metadata(&home) {
        Err(_) => line(
            "info",
            format!("data dir {} does not exist yet", home.display()),
        ),
        Ok(m) if m.file_type().is_symlink() => line(
            "FAIL",
            format!(
                "data dir {} is a symlink; tend will not use it",
                home.display()
            ),
        ),
        Ok(m) if m.permissions().mode() & 0o077 != 0 => line(
            "warn",
            format!(
                "data dir {} is readable by others ({:o}); tend makes it 0700 on start",
                home.display(),
                m.permissions().mode() & 0o777
            ),
        ),
        Ok(_) => line("ok", format!("data dir {}", home.display())),
    }

    let cfg_path = config::path();
    let (cfg, warnings) = config::load_from(&cfg_path);
    if !cfg_path.exists() {
        line(
            "info",
            format!("no config at {} (defaults)", cfg_path.display()),
        );
    } else if warnings.is_empty() {
        let over = cfg.overrides();
        line(
            "ok",
            format!("config {} ({} values set)", cfg_path.display(), over.len()),
        );
    } else {
        for w in warnings {
            line("warn", format!("config {}: {w}", cfg_path.display()));
        }
    }

    let settings = env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".claude/settings.json"))
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    if settings.contains("tend status") {
        line("ok", "status line shows `tend status`".into());
    } else {
        line(
            "info",
            "status line not set to `tend status` (see README)".into(),
        );
    }

    let root = paths::sessions_dir();
    let all: Vec<PathBuf> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let live = all.iter().filter(|d| sessions::is_live(d)).count();
    let bytes: u64 = all.iter().map(|d| dir_size(d)).sum();
    line(
        "info",
        format!(
            "{} sessions ({live} live), {:.1} MB; `tend claude` deletes those older than keep_days = {}",
            all.len(),
            bytes as f64 / 1e6,
            cfg.keep_days
        ),
    );
    (out, healthy)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}
