//! Retro look: run the look-mode pipeline over an existing Claude Code transcript.
//! The user gets a report without changing how they work.
//!
//! Time comes from each line's `timestamp`. Limit: edit hashes are approximate (hash of
//! path + new text), so edit-cycle detection is weaker than in a live session.
//! Transcript shape checked in S5.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rein_core::text::sha256_hex;
use rein_core::{Capabilities, Controller, Entry, Event, Mode};
use serde_json::Value;

use crate::connector::claude_code::{extract_output, runner_is_executable, HOOKED_TOOLS};
use crate::report;

pub fn run(transcript: &Path) -> Result<String> {
    let entries = analyze(transcript)?;
    let label = transcript
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(report::render(&label, &entries))
}

/// Ledger entries look mode would have written for this transcript.
pub fn analyze(transcript: &Path) -> Result<Vec<Entry>> {
    let f = File::open(transcript).with_context(|| format!("open {}", transcript.display()))?;
    let mut ctl = Controller::new(crate::config::load(), Capabilities::CLAUDE_CODE, Mode::Look);
    let mut entries = Vec::new();
    let mut pending: HashMap<String, (String, Value)> = HashMap::new();

    entries.extend(
        ctl.handle(
            Event::SessionStarted {
                agent: "claude-code (retro)".into(),
                cwd: String::new(),
                transcript_path: Some(transcript.display().to_string()),
                mode: None,
            },
            0,
        )
        .entries,
    );

    let mut now = 0u64;
    let mut cwd = String::new();
    let mut seen = std::collections::HashSet::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(c) = v["cwd"].as_str() {
            cwd = c.to_string();
        }
        // Keep time monotonic even if a line has no timestamp or goes backwards.
        if let Some(t) = v["timestamp"].as_str().and_then(parse_rfc3339_ms) {
            now = now.max(t);
        }
        let Some(items) = v["message"]["content"].as_array() else {
            continue;
        };
        for item in items {
            match item["type"].as_str() {
                Some("tool_use") => {
                    let id = item["id"].as_str().unwrap_or("").to_string();
                    // One API message can appear on several transcript lines with the same
                    // content blocks: handle each tool call once.
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    // Live hooks never see other tools (Read, Grep, …).
                    if !HOOKED_TOOLS.contains(&name.as_str()) {
                        continue;
                    }
                    let input = item["input"].clone();
                    if name == "Bash" {
                        // What PreToolUse would have seen, assuming auto mode: lets the
                        // report say how many test runs rein would wrap.
                        let cmd = input["command"].as_str().unwrap_or("");
                        let ev = Event::ToolRequested {
                            tool_use_id: id.clone(),
                            tool: name.clone(),
                            input: input.clone(),
                            rewrite_ok: runner_is_executable(cmd, &cwd),
                        };
                        entries.extend(ctl.handle(ev, now).entries);
                    }
                    if name == "Edit" || name == "MultiEdit" {
                        let ev = Event::EditProposed {
                            tool_use_id: id.clone(),
                            path: input["file_path"].as_str().unwrap_or("").into(),
                            old_text: input["old_string"].as_str().unwrap_or("").into(),
                            new_text: input["new_string"].as_str().unwrap_or("").into(),
                        };
                        entries.extend(ctl.handle(ev, now).entries);
                    }
                    pending.insert(id, (name, input));
                }
                Some("tool_result") => {
                    let id = item["tool_use_id"].as_str().unwrap_or("");
                    let Some((tool, input)) = pending.remove(id) else {
                        continue;
                    };
                    let failed = item["is_error"].as_bool().unwrap_or(false);
                    if !failed && matches!(tool.as_str(), "Edit" | "MultiEdit" | "Write") {
                        let path = input["file_path"].as_str().unwrap_or("").to_string();
                        let approx = format!(
                            "{path}\n{}",
                            input["new_string"]
                                .as_str()
                                .or(input["content"].as_str())
                                .unwrap_or("")
                        );
                        let ev = Event::FileEdited {
                            tool_use_id: id.to_string(),
                            path,
                            after_hash: Some(sha256_hex(&approx)),
                        };
                        entries.extend(ctl.handle(ev, now).entries);
                    }
                    let ev = Event::ToolCompleted {
                        tool_use_id: id.to_string(),
                        tool,
                        input,
                        output: extract_output(&item["content"]),
                        failed,
                    };
                    entries.extend(ctl.handle(ev, now).entries);
                }
                _ => {}
            }
        }
    }
    Ok(entries)
}

/// Session transcripts under a Claude Code projects dir: `<root>/<project>/<session>.jsonl`.
/// Subagent transcripts in deeper folders are part of their session and are skipped.
pub fn transcripts(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .flat_map(|d| fs::read_dir(d.path()).into_iter().flatten())
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    out.sort();
    out
}

struct Row {
    label: String,
    prompt: u64,
    totals: report::Totals,
}

/// Retro over every transcript: a total, then the top sessions by noise and by loop time.
pub fn run_all(root: &Path, top: usize) -> Result<String> {
    let mut rows = Vec::new();
    let mut skipped = 0;
    for path in transcripts(root) {
        let Ok(entries) = analyze(&path) else {
            skipped += 1;
            continue;
        };
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        rows.push(Row {
            label,
            prompt: report::usage_from_transcript(&path).map_or(0, |u| u.prompt_total()),
            totals: report::totals(&entries),
        });
    }

    let sum = |f: fn(&Row) -> u64| rows.iter().map(f).sum::<u64>();
    let (bash, noise) = (sum(|r| r.totals.bash_tokens), sum(|r| r.totals.noise));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Retro over {} transcripts in {}{}",
        rows.len(),
        root.display(),
        if skipped > 0 {
            format!(" ({skipped} unreadable, skipped)")
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        out,
        "Total: {} prompt tokens · Bash output {} tokens (est.) · noise {}% (est.) · {} loops ({} in loops)",
        report::k(sum(|r| r.prompt)),
        report::k(bash),
        report::pct(noise, bash),
        sum(|r| r.totals.loops),
        report::minutes(sum(|r| r.totals.loop_ms)),
    );
    let mut by_kind = std::collections::BTreeMap::new();
    for r in &rows {
        for (k, n) in &r.totals.by_kind {
            *by_kind.entry(*k).or_insert(0u64) += n;
        }
    }
    let _ = write!(
        out,
        "{}",
        report::coverage(
            &by_kind,
            sum(|r| r.totals.test_runs),
            sum(|r| r.totals.wrapped),
            sum(|r| r.totals.tool_edits),
            sum(|r| r.totals.bash_writes),
            sum(|r| r.totals.bash_test_writes),
        )
    );

    // Ties break on the label, so the order is stable.
    rows.sort_by(|a, b| {
        b.totals
            .noise
            .cmp(&a.totals.noise)
            .then(a.label.cmp(&b.label))
    });
    let _ = writeln!(out, "Top {top} by noise:");
    for r in rows.iter().filter(|r| r.totals.noise > 0).take(top) {
        let _ = writeln!(
            out,
            "  {} noise tokens ({}% of Bash output) · {}",
            report::k(r.totals.noise),
            report::pct(r.totals.noise, r.totals.bash_tokens),
            r.label
        );
    }
    rows.sort_by(|a, b| {
        b.totals
            .loop_ms
            .cmp(&a.totals.loop_ms)
            .then(a.label.cmp(&b.label))
    });
    let _ = writeln!(out, "Top {top} by loop time:");
    for r in rows.iter().filter(|r| r.totals.loops > 0).take(top) {
        let _ = writeln!(
            out,
            "  {} in {} loops · {}",
            report::minutes(r.totals.loop_ms),
            r.totals.loops,
            r.label
        );
    }
    Ok(out)
}

/// Parse `2026-10-19T08:35:44.123Z` (UTC, optional fraction) to unix milliseconds.
fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = ((days * 24 + hh) * 60 + mm) * 60 + ss;
    u64::try_from(total * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retro_all_ranks_sessions() {
        let fixture = format!(
            "{}/../../fixtures/transcripts/loop_session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let root = std::env::temp_dir().join(format!("rein-retro-all-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for p in ["-home-dev-shop", "-home-dev-api"] {
            fs::create_dir_all(root.join(p).join("s1/subagents")).unwrap();
            fs::copy(&fixture, root.join(p).join("s1.jsonl")).unwrap();
            fs::copy(&fixture, root.join(p).join("s1/subagents/a.jsonl")).unwrap();
            fs::write(root.join(p).join("notes.txt"), "x").unwrap();
        }
        let out = run_all(&root, 5).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert!(out.contains("Retro over 2 transcripts"), "{out}");
        assert!(
            out.contains("Test runs: 8 · wrapped (or would be, in auto mode): 8 (100%)"),
            "{out}"
        );
        assert!(out.contains("360.9k prompt tokens"), "{out}");
        assert!(out.contains("2 loops (27 s in loops)"), "{out}");
        let by_loop = out.split("Top 5 by loop time:\n").nth(1).unwrap();
        assert_eq!(
            by_loop.lines().collect::<Vec<_>>(),
            vec![
                "  13 s in 1 loops · -home-dev-api/s1.jsonl",
                "  13 s in 1 loops · -home-dev-shop/s1.jsonl"
            ]
        );
    }

    /// Claude Code repeats a message's content blocks on several lines.
    #[test]
    fn repeated_lines_count_once() {
        let fixture = format!(
            "{}/../../fixtures/transcripts/loop_session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = fs::read_to_string(&fixture).unwrap();
        let doubled: String = text.lines().flat_map(|l| [l, "\n", l, "\n"]).collect();
        let path = std::env::temp_dir().join(format!("rein-dup-{}.jsonl", std::process::id()));
        fs::write(&path, doubled).unwrap();
        let (a, b) = (run(Path::new(&fixture)).unwrap(), run(&path).unwrap());
        let _ = fs::remove_file(&path);
        let body = |s: &str| s.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert_eq!(body(&a), body(&b));
    }

    #[test]
    fn parses_transcript_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-10-19T08:35:44.123Z"),
            Some(1_792_398_944_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T23:59:59.5Z"),
            Some(1_709_251_199_500)
        );
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }

    #[test]
    fn finds_loop_in_sample_transcript() {
        let path = format!(
            "{}/../../fixtures/transcripts/loop_session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let out = run(Path::new(&path)).unwrap();
        assert!(out.contains("Loops (13 s in loops): 1"), "{out}");
        assert!(out.contains("[would steer] after 13 s:"), "{out}");
        assert!(out.contains("would steer"), "{out}");
        // R2.2: coverage lines.
        assert!(out.contains("Bash output by kind: test 100%"), "{out}");
        assert!(
            out.contains("Test runs: 4 · wrapped (or would be, in auto mode): 4 (100%)"),
            "{out}"
        );
        assert!(out.contains("Edits: 3 through Edit/Write"), "{out}");
        // The live session of this transcript reported "Tool calls: 7 (Bash 4)".
        assert!(out.contains("Tool calls: 7 (Bash 4)"), "{out}");
        assert!(
            out.contains("180.5k prompt (169.3k cache read, 93%)"),
            "{out}"
        );
        assert!(out.contains("cache read"), "{out}");
    }
}
