//! Claude Code connector.
//!
//! Payload and output shapes were captured in Phase 0 (docs/TASKS.md S1–S3) with
//! Claude Code 2.1.303. Re-capture from <session>/hooks.jsonl when Claude Code changes.

use std::path::{Path, PathBuf};

use rein_core::text::sha256_hex;
use rein_core::{Capabilities, Event, Permission, Response};
use serde_json::{json, Value};

use super::Connector;

const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

/// Tools rein hooks (see `settings`). Retro looks at the same tools, so its numbers
/// match a live session's.
pub const HOOKED_TOOLS: [&str; 4] = ["Bash", "Edit", "MultiEdit", "Write"];

#[derive(Default)]
pub struct ClaudeCode {
    started: bool,
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
        "Write" => (
            std::fs::read_to_string(resolve(cwd, path)).unwrap_or_default(),
            s(input, "content").to_string(),
        ),
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
        if !self.started {
            self.started = true;
            evs.push(Event::SessionStarted {
                agent: self.agent().into(),
                cwd: cwd.clone(),
                transcript_path: p
                    .get("transcript_path")
                    .and_then(Value::as_str)
                    .map(String::from),
                mode: None,
            });
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
                    let rewrite_ok = matches!(mode, "auto" | "bypassPermissions") && !background;
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
                        let after_hash = std::fs::read_to_string(resolve(&cwd, &path))
                            .ok()
                            .map(|c| sha256_hex(&c));
                        evs.push(Event::FileEdited {
                            tool_use_id: tool_use_id.clone(),
                            path,
                            after_hash,
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
            "UserPromptSubmit" => evs.push(Event::PromptSubmitted {
                chars: s(p, "prompt").chars().count(),
            }),
            "Stop" => evs.push(Event::TurnEnded),
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
                    input["command"] = json!(crate::run::wrapped_command(
                        s(payload, "tool_use_id"),
                        &command
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
    let cmd = |event: &str| format!("\"{}\" hook {event}", exe.display());
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
            // Bash too: the agent may not run `rein allow` (see Controller::on_tool_requested).
            "PreToolUse": entry("PreToolUse", Some(tools)),
            "PostToolUse": entry("PostToolUse", Some(tools)),
            "PostToolUseFailure": entry("PostToolUseFailure", Some(tools)),
            "UserPromptSubmit": entry("UserPromptSubmit", None),
            "Stop": entry("Stop", None),
            "PreCompact": entry("PreCompact", None),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut c = ClaudeCode { started: true };
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash_stderr.json"));
        assert_eq!(completed(&evs), ("out-line\nerr-line", false));
    }

    #[test]
    fn persisted_bash_output_is_the_preview() {
        let mut c = ClaudeCode { started: true };
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
            let mut c = ClaudeCode { started: true };
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

    #[test]
    fn maps_bash_failures() {
        let mut c = ClaudeCode { started: true };
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

    #[test]
    fn maps_edit_and_write() {
        let mut c = ClaudeCode { started: true };
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
        let mut c = ClaudeCode { started: true };
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
        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &p, &r).unwrap();
        let h = &out["hookSpecificOutput"];
        let original = p["tool_input"]["command"].as_str().unwrap();
        assert_eq!(
            h["updatedInput"]["command"],
            format!("rein run --id 'toolu_01' --sh '{original}'")
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
