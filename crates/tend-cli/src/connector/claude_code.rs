//! Claude Code connector.
//!
//! Payload and output shapes were captured in Phase 0 (docs/TASKS.md S1–S3) with
//! Claude Code 2.1.303. Re-capture from <session>/hooks.jsonl when Claude Code changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tend_core::text::sha256_hex;
use tend_core::{Capabilities, Event, Permission, Response};

use super::Connector;
use crate::paths::now_ms;

const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

/// Tools tend hooks (see `settings`). Retro looks at the same tools, so its numbers
/// match a live session's.
pub const HOOKED_TOOLS: [&str; 4] = ["Bash", "Edit", "MultiEdit", "Write"];

#[derive(Default)]
pub struct ClaudeCode {
    started: bool,
    /// Claude Code's id of the current conversation.
    session_id: String,
    /// Test files a Bash call may write, as they were before it ran (R1.4).
    /// Keyed by tool use id, with the time taken (ms).
    snapshots: BTreeMap<String, (u64, Snapshot)>,
}

/// Files as a Bash call found them before it ran: path and text (None: no such file).
type Snapshot = Vec<(PathBuf, Option<String>)>;

/// Largest file the Bash-write guard snapshots. Test files are far smaller.
const MAX_SNAPSHOT_BYTES: u64 = 1 << 20;
/// At the end of a turn, snapshots older than this are dropped.
const SNAPSHOT_KEEP_MS: u64 = 10 * 60 * 1000;
/// Most files one Bash call snapshots.
const MAX_SNAPSHOT_FILES: usize = 32;
/// Bash calls whose snapshots wait for a result. A call that never reports (interrupted)
/// is dropped at the end of the turn; this bounds the rest.
const MAX_PENDING_SNAPSHOTS: usize = 64;

/// Largest file whose content hash PostToolUse records for edit cycles.
const MAX_HASH_BYTES: u64 = 16 << 20;

/// A file's text: None if it does not exist. Err if it cannot be compared (a directory,
/// too large, not UTF-8), so the guard skips it.
fn read_text(path: &Path) -> Result<Option<String>, ()> {
    read_text_up_to(path, MAX_SNAPSHOT_BYTES)
}

fn read_text_up_to(path: &Path, max: u64) -> Result<Option<String>, ()> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(m) if m.is_file() && m.len() <= max => {
            std::fs::read_to_string(path).map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// The files a target names: itself, or the matches of a `*` in its last component.
fn expand(base: &Path, target: &str) -> Vec<PathBuf> {
    let path = base.join(target);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let Some((prefix, suffix)) = name.split_once('*') else {
        return vec![path];
    };
    if suffix.contains('*') {
        return vec![];
    }
    let dir = path.parent().unwrap_or(base);
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.len() >= prefix.len() + suffix.len() && n.starts_with(prefix) && n.ends_with(suffix)
        })
        .collect();
    found.sort();
    found
}

/// Snapshot the test files a Bash command may write (R1.4). Relative paths start where
/// the command runs: the hook's cwd, then any leading `cd DIR &&`.
fn snapshot(command: &str, cwd: &str) -> Vec<(PathBuf, Option<String>)> {
    let targets = tend_core::perception::code_write_targets(command);
    if targets.is_empty() {
        return vec![];
    }
    let (_, dirs) = tend_core::perception::shell::leading_cd(command);
    let base = dirs.iter().fold(resolve(cwd, ""), |b, d| b.join(d));
    let mut files: Vec<(PathBuf, Option<String>)> = Vec::new();
    for path in targets.iter().flat_map(|t| expand(&base, t)) {
        if files.len() >= MAX_SNAPSHOT_FILES || files.iter().any(|(p, _)| *p == path) {
            continue;
        }
        let Ok(text) = read_text(&path) else {
            continue;
        };
        // A source file counts only when it holds tests (Rust unit tests in `src/`).
        let is_test = tend_core::guard::is_test_path(&path.to_string_lossy())
            || text.as_deref().is_some_and(tend_core::guard::has_test_code);
        if is_test {
            files.push((path, text));
        }
    }
    files
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn resolve(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() || cwd.is_empty() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Bytes of stdout the agent sees when Claude Code persists a large output to a file.
const PERSISTED_PREVIEW_BYTES: usize = 2000;

/// The `<persisted-output>` message the agent sees in place of a large Bash output.
/// Captured in S1: the preview is the first 2000 bytes of stdout, cut back to the last
/// newline (no newline: exactly 2000 bytes). The size uses base-1024 KB, or MB from 1 MiB:
/// 1988895 bytes shows as "1.9MB", 1048576 bytes as "1MB".
fn persisted_output(stdout: &str, path: &str, size: u64) -> String {
    let mut end = stdout.len().min(PERSISTED_PREVIEW_BYTES);
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &stdout[..end];
    let preview = if end < stdout.len() {
        cut.rfind('\n').map_or(cut, |i| &cut[..i])
    } else {
        cut
    };
    // One decimal, without a trailing ".0": 106.3KB, 1.9MB, 1MB (N14).
    let one = |v: f64| {
        let t = format!("{v:.1}");
        t.strip_suffix(".0").map(str::to_string).unwrap_or(t)
    };
    let kb = size as f64 / 1024.0;
    let size = if kb >= 1024.0 {
        format!("{}MB", one(kb / 1024.0))
    } else {
        format!("{}KB", one(kb))
    };
    format!(
        "<persisted-output>\nOutput too large ({size}). Full output saved to: {path}\n\nPreview (first 2KB):\n{preview}\n...\n</persisted-output>"
    )
}

/// Text the agent sees for a tool result.
///
/// Captured in S1 (Claude Code 2.1.303). Bash PostToolUse `tool_response` is
/// `{stdout, stderr, interrupted, isImage, noOutputExpected}`. stderr is merged into
/// stdout, and `stderr` was always empty. There is no exit-code field. Outputs over
/// 30000 chars are cut and persisted, with `persistedOutputPath` and `persistedOutputSize`.
/// PostToolUseFailure `error` is a string: `"Exit code N\n"` plus the output.
pub fn extract_output(v: &Value) -> String {
    match v {
        Value::String(t) => t.clone(),
        Value::Object(o) if o.get("persistedOutputPath").is_some_and(Value::is_string) => {
            persisted_output(
                o.get("stdout").and_then(Value::as_str).unwrap_or(""),
                o.get("persistedOutputPath")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                o.get("persistedOutputSize")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        }
        Value::Object(o) if o.contains_key("stdout") || o.contains_key("stderr") => {
            let out = o.get("stdout").and_then(Value::as_str).unwrap_or("");
            let err = o.get("stderr").and_then(Value::as_str).unwrap_or("");
            if err.is_empty() {
                out.to_string()
            } else if out.is_empty() {
                err.to_string()
            } else {
                format!("{out}\n{err}")
            }
        }
        Value::Object(o) if o.contains_key("content") => extract_output(&o["content"]),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Every program in a test command is a bash builtin or a real executable. `tend run`
/// uses plain bash, so a name that exists only as a function or alias in the agent's
/// shell snapshot (`conda activate`, a custom `t`) would fail there (R0.4). A
/// `source X/bin/activate` adds X/bin to the PATH searched for later programs (R1.1).
pub fn runner_is_executable(command: &str, cwd: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    use tend_core::perception::shell;
    if tend_core::perception::runner_segment(command).is_none() {
        return true;
    }
    const BUILTINS: [&str; 20] = [
        "source", ".", "export", "set", "unset", "echo", "printf", "true", "false", "test", "[",
        ":", "exit", "exec", "command", "type", "shopt", "ulimit", "umask", "wait",
    ];
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let mut path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    // Relative names resolve against the directory the leading `cd`s end in.
    let (prefix, dirs) = shell::leading_cd(command);
    let cwd = dirs
        .iter()
        .fold(PathBuf::from(cwd), |at, d| at.join(d))
        .display()
        .to_string();
    let cwd = cwd.as_str();
    for seg in shell::parse(&command[prefix..]).segments {
        let Some(program) = seg.iter().find(|w| !shell::is_assignment(w)) else {
            continue;
        };
        if matches!(program.as_str(), "source" | ".") {
            if let Some(script) = seg.iter().skip_while(|w| *w != program).nth(1) {
                if let Some(bin) = resolve(cwd, script).parent() {
                    path.insert(0, bin.to_path_buf());
                }
            }
            continue;
        }
        if BUILTINS.contains(&program.as_str()) {
            continue;
        }
        let found = if program.contains('/') {
            executable(&resolve(cwd, program))
        } else {
            path.iter().any(|d| executable(&d.join(program)))
        };
        if !found {
            return false;
        }
    }
    true
}

/// (path, old text, new text) for an edit tool call.
fn edit_texts(tool: &str, input: &Value, cwd: &str) -> Option<(String, String, String)> {
    let path = s(input, "file_path");
    if path.is_empty() {
        return None;
    }
    let (old, new) = match tool {
        "Edit" => (
            s(input, "old_string").to_string(),
            s(input, "new_string").to_string(),
        ),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array)?;
            let join = |k: &str| edits.iter().map(|e| s(e, k)).collect::<Vec<_>>().join("\n");
            (join("old_string"), join("new_string"))
        }
        // The old text matters only to the guard: read it for files that can hold tests,
        // up to the snapshot size, never a 200 MB data file under the daemon lock (I4, F4).
        "Write" => {
            let old = if tend_core::guard::may_hold_tests(path) {
                read_text(&resolve(cwd, path))
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (old, s(input, "content").to_string())
        }
        _ => return None,
    };
    Some((path.to_string(), old, new))
}

/// Mirror the original `tool_response` shape when replacing output.
/// S2: for Bash, the object form reaches the agent; a plain string is silently ignored.
fn encode_replacement(original: Option<&Value>, text: &str) -> Value {
    match original {
        Some(Value::Object(o)) if o.contains_key("stdout") => {
            let mut o = o.clone();
            o.insert("stdout".into(), Value::String(text.to_string()));
            o.insert("stderr".into(), Value::String(String::new()));
            Value::Object(o)
        }
        _ => Value::String(text.to_string()),
    }
}

impl Connector for ClaudeCode {
    fn agent(&self) -> &'static str {
        "claude-code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CLAUDE_CODE
    }

    fn map(&mut self, hook_event: &str, p: &Value) -> Vec<Event> {
        let mut evs = Vec::new();
        let cwd = s(p, "cwd").to_string();
        let transcript_path = p
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(String::from);
        let session_id = s(p, "session_id");
        if !self.started {
            self.started = true;
            evs.push(Event::SessionStarted {
                agent: self.agent().into(),
                cwd: cwd.clone(),
                transcript_path,
                mode: None,
            });
        } else if !session_id.is_empty()
            && !self.session_id.is_empty()
            && session_id != self.session_id
        {
            // `/clear` or `/resume` in the same process: a new conversation (I6, R3).
            let reason = match (hook_event, s(p, "source")) {
                ("SessionStart", source) if !source.is_empty() => source.to_string(),
                _ => "new session id".to_string(),
            };
            self.snapshots.clear();
            evs.push(Event::ConversationChanged {
                transcript_path,
                reason,
            });
        }
        if !session_id.is_empty() {
            self.session_id = session_id.to_string();
        }
        let tool = s(p, "tool_name").to_string();
        let tool_use_id = s(p, "tool_use_id").to_string();
        let input = p.get("tool_input").cloned().unwrap_or(Value::Null);

        match hook_event {
            "PreToolUse" => {
                if EDIT_TOOLS.contains(&tool.as_str()) {
                    if let Some((path, old_text, new_text)) = edit_texts(&tool, &input, &cwd) {
                        evs.push(Event::EditProposed {
                            tool_use_id,
                            path,
                            old_text,
                            new_text,
                        });
                    }
                } else {
                    // S2/D4: Claude Code checks permissions on the rewritten command, so a
                    // rewrite is safe only where no prompt can appear. Background calls keep
                    // their own output handling.
                    let mode = s(p, "permission_mode");
                    let background = input
                        .get("run_in_background")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let command = s(&input, "command");
                    if tool == "Bash" && self.snapshots.len() < MAX_PENDING_SNAPSHOTS {
                        let files = snapshot(command, &cwd);
                        if !files.is_empty() {
                            self.snapshots
                                .insert(tool_use_id.clone(), (now_ms(), files));
                        }
                    }
                    let rewrite_ok = matches!(mode, "auto" | "bypassPermissions")
                        && !background
                        && runner_is_executable(command, &cwd);
                    evs.push(Event::ToolRequested {
                        tool_use_id,
                        tool,
                        input,
                        rewrite_ok,
                    });
                }
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let failed = hook_event == "PostToolUseFailure";
                let output = match p.get("error") {
                    Some(e) if failed => extract_output(e),
                    _ => extract_output(p.get("tool_response").unwrap_or(&Value::Null)),
                };
                if !failed && EDIT_TOOLS.contains(&tool.as_str()) {
                    let path = s(&input, "file_path").to_string();
                    if !path.is_empty() {
                        // Edit cycles compare content hashes; a huge file is skipped.
                        let after_hash = read_text_up_to(&resolve(&cwd, &path), MAX_HASH_BYTES)
                            .ok()
                            .flatten()
                            .map(|c| sha256_hex(&c));
                        evs.push(Event::FileEdited {
                            tool_use_id: tool_use_id.clone(),
                            path,
                            after_hash,
                        });
                    }
                }
                let files = self.snapshots.remove(&tool_use_id).map(|(_, f)| f);
                for (path, before) in files.unwrap_or_default() {
                    let Ok(after) = read_text(&path) else {
                        continue;
                    };
                    if after != before {
                        evs.push(Event::FileChanged {
                            tool_use_id: tool_use_id.clone(),
                            path: path.display().to_string(),
                            old_text: before.unwrap_or_default(),
                            new_text: after.unwrap_or_default(),
                        });
                    }
                }
                evs.push(Event::ToolCompleted {
                    tool_use_id,
                    tool,
                    input,
                    output,
                    failed,
                });
            }
            "PermissionDenied" => evs.push(Event::ToolDenied {
                tool_use_id,
                tool,
                reason: s(p, "reason").to_string(),
            }),
            "UserPromptSubmit" => evs.push(Event::PromptSubmitted {
                chars: s(p, "prompt").chars().count(),
            }),
            "Stop" => {
                // A call with no result by now was interrupted or denied, unless it still
                // runs in the background (a subagent, `run_in_background`): keep recent
                // ones, drop the rest (I6, R4).
                let cutoff = now_ms().saturating_sub(SNAPSHOT_KEEP_MS);
                self.snapshots.retain(|_, (t, _)| *t >= cutoff);
                evs.push(Event::TurnEnded);
            }
            "PreCompact" => evs.push(Event::Compacted {
                trigger: s(p, "trigger").to_string(),
            }),
            _ => {}
        }
        evs
    }

    fn encode(&self, hook_event: &str, payload: &Value, r: &Response) -> Option<Value> {
        if r.is_empty() {
            return None;
        }
        let context = (!r.add_context.is_empty()).then(|| r.add_context.join("\n"));
        let mut h = json!({ "hookEventName": hook_event });
        match hook_event {
            "PreToolUse" => {
                if r.wrap {
                    let mut input = payload.get("tool_input").cloned().unwrap_or(json!({}));
                    let command = s(&input, "command").to_string();
                    // The daemon is the tend binary; its absolute path does not depend on
                    // the PATH in the agent's shell snapshot (R0.4).
                    let exe = std::env::current_exe().unwrap_or_else(|_| "tend".into());
                    // A leading `cd DIR &&` runs in the agent's shell, so it persists.
                    let (prefix, _) = tend_core::perception::shell::leading_cd(&command);
                    input["command"] = json!(format!(
                        "{}{}",
                        &command[..prefix],
                        crate::run::wrapped_command(
                            &exe,
                            s(payload, "tool_use_id"),
                            &command[prefix..]
                        )
                    ));
                    // No permissionDecision: Claude Code still checks the new command.
                    h["updatedInput"] = input;
                }
                if let Some(p) = &r.permission {
                    let (decision, reason) = match p {
                        Permission::Ask(m) => ("ask", m),
                        Permission::Deny(m) => ("deny", m),
                    };
                    h["permissionDecision"] = json!(decision);
                    h["permissionDecisionReason"] = json!(reason);
                }
            }
            "PostToolUse" => {
                if let Some(t) = &r.replace_output {
                    h["updatedToolOutput"] = encode_replacement(payload.get("tool_response"), t);
                }
            }
            // R3.7: the only output this event takes (2.1.303 hook docs).
            "PermissionDenied" => {
                if !r.retry {
                    return None;
                }
                return Some(json!({ "hookSpecificOutput": {
                    "hookEventName": hook_event,
                    "retry": true,
                }}));
            }
            "PostToolUseFailure" | "UserPromptSubmit" | "SessionStart" => {}
            _ => return None,
        }
        if let Some(c) = context {
            h["additionalContext"] = json!(c);
        }
        Some(json!({ "hookSpecificOutput": h }))
    }
}

/// Settings file the launcher passes with `claude --settings <file>`.
/// S3: these hooks merge with user, project, and local hooks.
pub fn settings(exe: &Path) -> Value {
    // Quoted for the shell Claude Code runs hooks with: a path may hold `"` or `$`.
    let exe = crate::run::sh_quote(&exe.to_string_lossy());
    let cmd = |event: &str| format!("{exe} hook {event}");
    let entry = |event: &str, matcher: Option<&str>| {
        let mut m =
            json!({ "hooks": [{ "type": "command", "command": cmd(event), "timeout": 5 }] });
        if let Some(matcher) = matcher {
            m["matcher"] = json!(matcher);
        }
        json!([m])
    };
    let tools = HOOKED_TOOLS.join("|");
    let tools = tools.as_str();
    json!({
        "hooks": {
            "SessionStart": entry("SessionStart", None),
            // Bash too: the agent may not run `tend allow` (see Controller::on_tool_requested).
            "PreToolUse": entry("PreToolUse", Some(tools)),
            "PostToolUse": entry("PostToolUse", Some(tools)),
            "PostToolUseFailure": entry("PostToolUseFailure", Some(tools)),
            // R3.7: only tend's own rewrites (Bash) can make auto mode deny a call.
            "PermissionDenied": entry("PermissionDenied", Some("Bash")),
            "UserPromptSubmit": entry("UserPromptSubmit", None),
            "Stop": entry("Stop", None),
            "PreCompact": entry("PreCompact", None),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/../../fixtures/claude_code/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn completed(evs: &[Event]) -> (&str, bool) {
        match evs.last() {
            Some(Event::ToolCompleted { output, failed, .. }) => (output, *failed),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn maps_bash_post_tool_use() {
        let mut c = ClaudeCode::default();
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash.json"));
        assert!(matches!(evs[0], Event::SessionStarted { .. }));
        match &evs[1] {
            Event::ToolCompleted {
                tool,
                output,
                failed,
                ..
            } => {
                assert_eq!(tool, "Bash");
                assert!(output.starts_with("===="));
                assert!(output.contains("300 passed"));
                assert!(!failed);
            }
            e => panic!("unexpected {e:?}"),
        }
        assert_eq!(
            c.map("PostToolUse", &fixture("post_tool_use_bash.json"))
                .len(),
            1
        );
    }

    #[test]
    fn bash_stderr_is_merged_into_stdout() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash_stderr.json"));
        assert_eq!(completed(&evs), ("out-line\nerr-line", false));
    }

    #[test]
    fn persisted_bash_output_is_the_preview() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let p = fixture("post_tool_use_bash_persisted.json");
        let evs = c.map("PostToolUse", &p);
        let (output, _) = completed(&evs);
        let stdout = p["tool_response"]["stdout"].as_str().unwrap();
        assert_eq!(stdout.len(), 30000);
        assert!(output.starts_with(
            "<persisted-output>\nOutput too large (106.3KB). Full output saved to: /home/dev/"
        ));
        assert!(output.contains("Preview (first 2KB):\n1\n2\n3\n"));
        assert!(output.ends_with("\n526\n527\n...\n</persisted-output>"));
        assert!(output.len() < 2300);
    }

    #[test]
    fn persisted_size_switches_to_mb() {
        let out = persisted_output("1\n2\n", "/p", 1_988_895);
        assert!(out.contains("Output too large (1.9MB)"), "{out}");
    }

    /// Captured N14: 40000 `x` and exactly 1 MiB of `a`, both with no newline.
    #[test]
    fn persisted_edge_cases() {
        for (name, size, preview_len) in [
            (
                "post_tool_use_bash_persisted_no_newline.json",
                "39.1KB",
                2000,
            ),
            ("post_tool_use_bash_persisted_1mib.json", "1MB", 2000),
        ] {
            let mut c = ClaudeCode {
                started: true,
                ..Default::default()
            };
            let evs = c.map("PostToolUse", &fixture(name));
            let (out, _) = completed(&evs);
            assert!(
                out.contains(&format!("Output too large ({size})")),
                "{name}: {out}"
            );
            let preview = out
                .split("Preview (first 2KB):\n")
                .nth(1)
                .and_then(|p| p.strip_suffix("\n...\n</persisted-output>"))
                .unwrap();
            assert_eq!(preview.len(), preview_len, "{name}");
        }
    }

    /// R3: a new session id in the same process (`/clear`, `/resume`) is a new
    /// conversation with its own transcript.
    #[test]
    fn new_session_id_is_a_new_conversation() {
        let mut c = ClaudeCode::default();
        let mut p = fixture("pre_tool_use_bash_auto.json");
        p["hook_event_name"] = json!("SessionStart");
        p["source"] = json!("startup");
        assert!(matches!(
            c.map("SessionStart", &p)[0],
            Event::SessionStarted { .. }
        ));
        assert!(c.map("SessionStart", &p).is_empty(), "same conversation");
        p["session_id"] = json!("9b1d6f2e-0000-4000-8000-000000000002");
        p["transcript_path"] = json!("/home/dev/.claude/projects/x/second.jsonl");
        p["source"] = json!("clear");
        let evs = c.map("SessionStart", &p);
        assert!(
            matches!(&evs[..], [Event::ConversationChanged { transcript_path: Some(t), reason }]
                if t.ends_with("second.jsonl") && reason == "clear"),
            "{evs:?}"
        );
    }

    /// T8: real captures from Claude Code 2.1.304 (conformance run): a PreToolUse Bash
    /// payload in auto mode, and the PostToolUseFailure of a wrapped test run, whose
    /// error text is tend's summary.
    #[test]
    fn real_auto_mode_and_wrapped_failure_payloads() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let pre = fixture("pre_tool_use_bash_auto.json");
        assert_eq!(pre["permission_mode"], "auto");
        let evs = c.map("PreToolUse", &pre);
        assert!(
            matches!(&evs[..], [Event::ToolRequested { tool, input, .. }]
                if tool == "Bash" && input["command"] == "python3 -m pytest -v tests"),
            "{evs:?}"
        );
        let post = fixture("post_tool_use_failure_wrapped.json");
        let evs = c.map("PostToolUseFailure", &post);
        let Some(Event::ToolCompleted {
            failed,
            output,
            input,
            ..
        }) = evs.last()
        else {
            panic!("{evs:?}")
        };
        assert!(*failed);
        assert!(input["command"].as_str().unwrap().contains(" run --id "));
        assert!(output.contains("full output: tend show o-1"), "{output}");
        assert!(output.contains("FAILED tests/test_api.py::test_total_empty"));
    }

    /// R3.7: payload shape from the 2.1.303 hook schema (not yet captured live).
    #[test]
    fn permission_denied_maps_and_encodes_retry() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let p = fixture("permission_denied_bash.json");
        let evs = c.map("PermissionDenied", &p);
        assert!(matches!(
            &evs[..],
            [Event::ToolDenied { tool_use_id, reason, .. }]
                if tool_use_id == "toolu_07" && reason.starts_with("[Auto-Mode Bypass]")
        ));
        assert_eq!(c.encode("PermissionDenied", &p, &Response::default()), None);
        let r = Response {
            retry: true,
            ..Default::default()
        };
        assert_eq!(
            c.encode("PermissionDenied", &p, &r),
            Some(json!({ "hookSpecificOutput": {
                "hookEventName": "PermissionDenied", "retry": true
            }}))
        );
        let hooks = &settings(Path::new("/x/tend"))["hooks"];
        assert_eq!(hooks["PermissionDenied"][0]["matcher"], "Bash");
    }

    #[test]
    fn maps_bash_failures() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map(
            "PostToolUseFailure",
            &fixture("post_tool_use_failure_bash.json"),
        );
        let (output, failed) = completed(&evs);
        assert!(failed);
        assert!(output.starts_with("Exit code 1\n"));
        assert!(output.contains("[16523 characters truncated]"));
        assert!(output.contains("FAILED tests/test_api.py::test_create_order - KeyError: 'total'"));

        let evs = c.map(
            "PostToolUseFailure",
            &fixture("post_tool_use_failure_bash_exit.json"),
        );
        assert_eq!(completed(&evs), ("Exit code 3\nout-line\nerr-line", true));
    }

    /// R1.4: test files a Bash command names are compared before and after it runs.
    /// I2 G2: a Bash write to a source file that holds unit tests is guarded too; a
    /// source file without tests is not snapshotted.
    #[test]
    fn bash_writes_to_inline_tests_become_file_changes() {
        let dir = std::env::temp_dir().join(format!("tend-bashw-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let lib = dir.join("src/lib.rs");
        let plain = dir.join("src/main.rs");
        let tests = "fn t() -> u32 { 6 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t6() { assert_eq!(super::t(), 6); }\n}\n";
        std::fs::write(&lib, tests).unwrap();
        std::fs::write(&plain, "fn main() { assert!(true); }\n").unwrap();

        let mut c = ClaudeCode::default();
        let mut pre = fixture("post_tool_use_bash.json");
        pre["hook_event_name"] = json!("PreToolUse");
        pre.as_object_mut().unwrap().remove("tool_response");
        pre["cwd"] = json!(dir.display().to_string());
        pre["tool_input"]["command"] = json!("sed -i '/assert/d' src/lib.rs src/main.rs");
        c.map("PreToolUse", &pre);
        std::fs::write(&lib, tests.replace("assert_eq!(super::t(), 6);", "")).unwrap();
        std::fs::write(&plain, "fn main() { }\n").unwrap();

        let mut post = fixture("post_tool_use_bash.json");
        post["cwd"] = pre["cwd"].clone();
        post["tool_input"] = pre["tool_input"].clone();
        let paths: Vec<String> = c
            .map("PostToolUse", &post)
            .iter()
            .filter_map(|e| match e {
                Event::FileChanged { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(paths, [lib.display().to_string()]);
    }

    #[test]
    fn bash_writes_to_test_files_become_file_changes() {
        let dir = std::env::temp_dir().join(format!("tend-bashw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shop/tests")).unwrap();
        let test = dir.join("shop/tests/test_a.py");
        std::fs::write(&test, "assert a\nassert b\n").unwrap();
        std::fs::write(dir.join("shop/tests/test_b.py"), "assert c\n").unwrap();

        let mut c = ClaudeCode::default();
        let mut pre = fixture("post_tool_use_bash.json");
        pre["hook_event_name"] = json!("PreToolUse");
        pre.as_object_mut().unwrap().remove("tool_response");
        pre["cwd"] = json!(dir.display().to_string());
        pre["tool_input"]["command"] = json!("cd shop && sed -i '/assert b/d' tests/*.py");
        c.map("PreToolUse", &pre);
        std::fs::write(&test, "assert a\n").unwrap();
        // R4: the turn ends while the call still runs (a background subagent): the
        // snapshot is recent, so it stays.
        c.map("Stop", &json!({}));

        let mut post = fixture("post_tool_use_bash.json");
        post["cwd"] = pre["cwd"].clone();
        post["tool_input"] = pre["tool_input"].clone();
        let evs = c.map("PostToolUse", &post);
        let changed: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                Event::FileChanged {
                    path,
                    old_text,
                    new_text,
                    ..
                } => Some((path.clone(), old_text.as_str(), new_text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            changed,
            [(
                test.display().to_string(),
                "assert a\nassert b\n",
                "assert a\n"
            )],
            "test_b.py did not change"
        );
        assert!(matches!(evs.last(), Some(Event::ToolCompleted { .. })));
        // The snapshot is used once.
        let evs = c.map("PostToolUse", &post);
        assert!(!evs.iter().any(|e| matches!(e, Event::FileChanged { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maps_edit_and_write() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map("PreToolUse", &fixture("pre_tool_use_edit_test.json"));
        match &evs[0] {
            Event::EditProposed {
                path,
                old_text,
                new_text,
                ..
            } => {
                assert_eq!(path, "/home/dev/shop/tests/test_api.py");
                assert_eq!(old_text.matches("assert").count(), 2);
                assert_eq!(new_text.matches("assert").count(), 1);
            }
            e => panic!("unexpected {e:?}"),
        }
        let evs = c.map("PreToolUse", &fixture("pre_tool_use_write.json"));
        assert!(
            matches!(&evs[0], Event::EditProposed { path, new_text, .. } if path == "/home/dev/shop/notes.txt" && new_text == "first\n")
        );
        for name in ["post_tool_use_edit.json", "post_tool_use_write.json"] {
            let evs = c.map("PostToolUse", &fixture(name));
            assert!(matches!(&evs[0], Event::FileEdited { .. }), "{name}");
            assert!(
                matches!(&evs[1], Event::ToolCompleted { failed: false, .. }),
                "{name}"
            );
        }
    }

    #[test]
    fn maps_lifecycle_events() {
        let mut c = ClaudeCode::default();
        let evs = c.map("SessionStart", &fixture("session_start.json"));
        assert!(
            matches!(&evs[..], [Event::SessionStarted { cwd, transcript_path: Some(_), .. }] if cwd == "/home/dev/shop")
        );
        let evs = c.map("UserPromptSubmit", &fixture("user_prompt_submit.json"));
        assert!(matches!(&evs[..], [Event::PromptSubmitted { chars }] if *chars > 0));
        let evs = c.map("Stop", &fixture("stop.json"));
        assert!(matches!(&evs[..], [Event::TurnEnded]));
    }

    #[test]
    fn encodes_replacement_in_original_shape() {
        let c = ClaudeCode::default();
        let p = fixture("post_tool_use_bash.json");
        let r = Response {
            replace_output: Some("short".into()),
            ..Default::default()
        };
        let out = c.encode("PostToolUse", &p, &r).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedToolOutput"]["stdout"],
            "short"
        );
        assert_eq!(c.encode("PostToolUse", &p, &Response::default()), None);
    }

    #[test]
    fn wraps_test_commands_only_where_no_prompt_can_appear() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let mut p = fixture("post_tool_use_bash.json");
        p["hook_event_name"] = json!("PreToolUse");
        p.as_object_mut().unwrap().remove("tool_response");
        for (mode, bg, want) in [
            ("auto", false, true),
            ("bypassPermissions", false, true),
            ("default", false, false),
            ("acceptEdits", false, false),
            ("auto", true, false),
        ] {
            p["permission_mode"] = json!(mode);
            p["tool_input"]["run_in_background"] = json!(bg);
            let evs = c.map("PreToolUse", &p);
            assert!(
                matches!(&evs[0], Event::ToolRequested { rewrite_ok, .. } if *rewrite_ok == want),
                "{mode} bg={bg}"
            );
        }
        // R0.4: a runner that is not an executable (a shell function, a missing path).
        let mut q = p.clone();
        q["permission_mode"] = json!("auto");
        q["tool_input"]["run_in_background"] = json!(false);
        for cmd in [
            "/nonexistent/bin/pytest -q",
            "./no-such-venv/bin/python -m pytest",
        ] {
            q["tool_input"]["command"] = json!(cmd);
            let evs = c.map("PreToolUse", &q);
            assert!(
                matches!(
                    &evs[0],
                    Event::ToolRequested {
                        rewrite_ok: false,
                        ..
                    }
                ),
                "{cmd}"
            );
        }
        // R1.1: a runner inside an activated venv is found through the activate script.
        let venv = std::env::temp_dir().join(format!("tend-venv-{}", std::process::id()));
        std::fs::create_dir_all(venv.join(".venv/bin")).unwrap();
        std::fs::write(venv.join(".venv/bin/activate"), "").unwrap();
        let runner = venv.join(".venv/bin/py.test");
        std::fs::write(&runner, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        q["cwd"] = json!(venv.display().to_string());
        q["tool_input"]["command"] =
            json!("source .venv/bin/activate && py.test -q 2>&1 | tail -5");
        let evs = c.map("PreToolUse", &q);
        assert!(matches!(
            &evs[0],
            Event::ToolRequested {
                rewrite_ok: true,
                ..
            }
        ));
        // The same from the parent dir, after a leading cd.
        q["cwd"] = json!(venv.parent().unwrap().display().to_string());
        let name = venv.file_name().unwrap().to_string_lossy().to_string();
        let cmd = format!("cd {name} && source .venv/bin/activate && py.test -q");
        q["tool_input"]["command"] = json!(cmd);
        let evs = c.map("PreToolUse", &q);
        assert!(matches!(
            &evs[0],
            Event::ToolRequested {
                rewrite_ok: true,
                ..
            }
        ));
        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &q, &r).unwrap();
        let new = out["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(
            new.starts_with(&format!("cd {name} && '")),
            "the cd stays outside: {new}"
        );
        assert!(
            new.ends_with("--sh 'source .venv/bin/activate && py.test -q'"),
            "{new}"
        );
        let _ = std::fs::remove_dir_all(&venv);

        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &p, &r).unwrap();
        let h = &out["hookSpecificOutput"];
        let original = p["tool_input"]["command"].as_str().unwrap();
        assert_eq!(
            h["updatedInput"]["command"],
            crate::run::wrapped_command(&std::env::current_exe().unwrap(), "toolu_01", original)
        );
        assert_eq!(
            h["updatedInput"]["run_in_background"], true,
            "other fields kept"
        );
        assert!(h.get("permissionDecision").is_none(), "never auto-allow");
    }

    #[test]
    fn encodes_permission() {
        let c = ClaudeCode::default();
        let r = Response {
            permission: Some(Permission::Ask("why".into())),
            ..Default::default()
        };
        let out = c
            .encode("PreToolUse", &fixture("pre_tool_use_edit_test.json"), &r)
            .unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "ask");
        assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], "why");
    }
}
