//! Session report, `why` cause chains, and output lookup. Reads ledger entries only.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;
use winnow_core::perception::classify;
use winnow_core::record::SteerTrigger;
use winnow_core::text::truncate_chars;
use winnow_core::{ActionKind, Body, Config, Entry, Event};

/// Token usage from a Claude Code transcript. Field names confirmed in S5: totals match
/// Claude Code's own usage for the same session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
    pub messages: u64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.cache_creation += o.cache_creation;
        self.cache_read += o.cache_read;
        self.output += o.output;
        self.messages += o.messages;
    }

    pub fn prompt_total(&self) -> u64 {
        self.input + self.cache_creation + self.cache_read
    }
}

pub fn usage_from_transcript(path: &Path) -> Option<Usage> {
    let f = File::open(path).ok()?;
    usage_from_lines(BufReader::new(f).lines().map_while(Result::ok))
}

/// One API message can appear on several transcript lines (one per content block).
/// S5: the lines usually repeat the same usage, but an early line can hold zeros,
/// so the last line of each message wins.
fn usage_from_lines(lines: impl Iterator<Item = String>) -> Option<Usage> {
    let mut by_msg: HashMap<String, Usage> = HashMap::new();
    let mut anonymous = Usage::default();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let msg = &v["message"];
        let usage = &msg["usage"];
        if !usage.is_object() {
            continue;
        }
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let line_usage = Usage {
            input: n("input_tokens"),
            cache_creation: n("cache_creation_input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            output: n("output_tokens"),
            messages: 1,
        };
        match msg["id"].as_str().or(v["uuid"].as_str()) {
            Some(key) if !key.is_empty() => {
                by_msg.insert(key.to_string(), line_usage);
            }
            _ => anonymous.add(&line_usage),
        }
    }
    // Summing is order-independent, so HashMap order does not reach the output.
    let mut u = anonymous;
    for m in by_msg.values() {
        u.add(m);
    }
    (u.messages > 0).then_some(u)
}

pub fn k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn pct(part: u64, whole: u64) -> u64 {
    (part * 100).checked_div(whole).unwrap_or(0)
}

/// Whole minutes, or seconds under a minute.
pub fn minutes(ms: u64) -> String {
    if ms < 60_000 {
        format!("{} s", ms / 1000)
    } else {
        format!("{} min", ms / 60_000)
    }
}

fn status(applied: bool, done: &str, would: &str) -> String {
    format!("[{}]", if applied { done } else { would })
}

/// Session numbers that retro over many sessions compares.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub bash_tokens: u64,
    /// Tokens a faithful summary would remove, including output the agent cannot replace.
    pub noise: u64,
    pub loops: u64,
    /// First occurrence to steer, summed over loops.
    pub loop_ms: u64,
}

pub fn totals(entries: &[Entry]) -> Totals {
    let mut t = Totals::default();
    for e in entries {
        match &e.body {
            Body::Output { tokens, .. } => t.bash_tokens += *tokens as u64,
            Body::Action {
                action:
                    ActionKind::Shape {
                        from_tokens,
                        to_tokens,
                        ..
                    },
                ..
            } => t.noise += from_tokens.saturating_sub(*to_tokens) as u64,
            Body::Action {
                action:
                    ActionKind::Steer {
                        trigger: SteerTrigger::Loop { duration_ms, .. },
                        ..
                    },
                ..
            } => {
                t.loops += 1;
                t.loop_ms += duration_ms.unwrap_or(0);
            }
            _ => {}
        }
    }
    t
}

pub fn render(label: &str, entries: &[Entry]) -> String {
    let mut agent = String::from("?");
    let mut mode = String::from("?");
    let mut transcript: Option<String> = None;
    let t = totals(entries);
    let (mut tools, mut bash, mut reopens, mut judgments) = (0u64, 0u64, 0u64, 0u64);
    let (mut shaped, mut before, mut after, mut shaped_applied) = (0u64, 0u64, 0u64, 0u64);
    let mut unshapeable = 0u64;
    let (mut loops, mut cycles, mut guards, mut adaptations, mut modes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut config = Vec::new();
    let defaults = Config::default().fields();

    for e in entries {
        match &e.body {
            Body::Event { event } => match event {
                Event::SessionStarted {
                    agent: a,
                    transcript_path,
                    mode: m,
                    ..
                } => {
                    agent = a.clone();
                    transcript = transcript_path.clone();
                    if let Some(m) = m {
                        mode = format!("{m:?}").to_lowercase();
                    }
                }
                Event::ToolCompleted { tool, input, .. } => {
                    tools += 1;
                    if tool == "Bash" {
                        bash += 1;
                        let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
                        if classify(cmd).show_target.is_some() {
                            reopens += 1;
                        }
                    }
                }
                _ => {}
            },
            Body::Output { .. } | Body::Format { .. } => {}
            Body::Judgment { .. } => judgments += 1,
            Body::Action {
                action,
                applied,
                reason,
                unsupported,
                ..
            } => match action {
                ActionKind::Shape { .. } if *unsupported => unshapeable += 1,
                ActionKind::Shape {
                    from_tokens,
                    to_tokens,
                    ..
                } => {
                    shaped += 1;
                    before += *from_tokens as u64;
                    after += *to_tokens as u64;
                    shaped_applied += u64::from(*applied);
                }
                ActionKind::Steer { trigger, text } => {
                    let st = status(*applied, "steered", "would steer");
                    match trigger {
                        SteerTrigger::Loop { duration_ms, .. } => {
                            let line = match duration_ms {
                                Some(ms) => format!("{st} after {}: {text}", minutes(*ms)),
                                None => format!("{st} {text}"),
                            };
                            loops.push(line);
                        }
                        SteerTrigger::Cycle { .. } => cycles.push(format!("{st} {text}")),
                    }
                }
                ActionKind::Guard { decision, .. } => {
                    let d = format!("{decision:?}").to_lowercase();
                    guards.push(format!(
                        "{} {reason}",
                        status(*applied, &d, &format!("would {d}"))
                    ));
                }
            },
            Body::Adaptation { reason, .. } => adaptations.push(reason.clone()),
            Body::ModeChanged { to, .. } => {
                mode = format!("{to:?}").to_lowercase();
                modes.push(mode.clone());
            }
            Body::Config { overrides } => {
                config = overrides
                    .iter()
                    .map(|(name, v)| match defaults.get(name) {
                        Some(d) => format!("{name} = {v} (default {d})"),
                        None => format!("{name} = {v}"),
                    })
                    .collect();
            }
        }
    }

    let first = entries.iter().map(|e| e.ts_ms).filter(|t| *t > 0).min();
    let last = entries.iter().map(|e| e.ts_ms).max();
    let duration = match (first, last) {
        (Some(a), Some(b)) if b >= a => minutes(b - a),
        _ => "duration n/a".into(),
    };

    let mut out = String::new();
    let _ = writeln!(out, "Session {label} · {agent} · {duration} · mode: {mode}");
    if !config.is_empty() {
        let _ = writeln!(out, "Config: {}", config.join(" · "));
    }
    match transcript
        .as_deref()
        .map(Path::new)
        .and_then(usage_from_transcript)
    {
        Some(u) => {
            let _ = writeln!(
                out,
                "Model tokens: {} prompt ({} cache read, {}%) · {} output · {} messages",
                k(u.prompt_total()),
                k(u.cache_read),
                pct(u.cache_read, u.prompt_total()),
                k(u.output),
                u.messages
            );
        }
        None => {
            let _ = writeln!(out, "Model tokens: transcript not available");
        }
    }
    let _ = writeln!(
        out,
        "Tool calls: {tools} (Bash {bash}) · Bash output: {} tokens (est.) · noise {}% (est.)",
        k(t.bash_tokens),
        pct(t.noise, t.bash_tokens)
    );
    let saved = before.saturating_sub(after);
    let verb = if shaped_applied > 0 {
        "applied"
    } else {
        "would apply"
    };
    let _ = writeln!(
        out,
        "Shaping: {shaped} outputs · {} → {} tokens · saves {} ({}% of Bash output) [{verb}]",
        k(before),
        k(after),
        k(saved),
        pct(saved, t.bash_tokens)
    );
    if unshapeable > 0 {
        let _ = writeln!(
            out,
            "Not shapeable: {unshapeable} outputs (the agent cannot replace failed-call output)"
        );
    }
    let _ = writeln!(out, "Re-opens: {reopens}");
    let loop_title = if t.loop_ms > 0 {
        format!("Loops ({} in loops)", minutes(t.loop_ms))
    } else {
        "Loops".to_string()
    };
    for (title, items) in [
        (loop_title.as_str(), &loops),
        ("Edit cycles", &cycles),
        ("Test guard", &guards),
        ("Adaptations", &adaptations),
        ("Mode changes", &modes),
    ] {
        let _ = writeln!(out, "{title}: {}", items.len());
        for i in items.iter() {
            let _ = writeln!(out, "  - {}", truncate_chars(i, 200));
        }
    }
    let _ = writeln!(out, "Judgments recorded: {judgments}");
    out
}

pub fn find_output_blob(entries: &[Entry], id: &str) -> Option<String> {
    entries.iter().find_map(|e| match &e.body {
        Body::Output { id: o, blob, .. } if o == id => Some(blob.clone()),
        _ => None,
    })
}

fn describe(e: &Entry) -> String {
    let body = match &e.body {
        Body::Action {
            id,
            action,
            reason,
            applied,
            unsupported,
        } => {
            let kind = match action {
                ActionKind::Shape { .. } => "shape",
                ActionKind::Steer { .. } => "steer",
                ActionKind::Guard { .. } => "guard",
            };
            let state = match (applied, unsupported) {
                (true, _) => "applied",
                (false, true) => "not applied, unsupported by the agent",
                (false, false) => "not applied",
            };
            format!("action {id} ({kind}, {state}): {reason}")
        }
        Body::Judgment {
            question,
            version,
            subject,
            answer,
            source,
        } => {
            format!("judgment {question}@v{version}({subject}) = {answer:?} from {source:?}")
        }
        Body::Output {
            id,
            output_kind,
            lines,
            tokens,
            ..
        } => {
            format!(
                "output {id}: {} kind, {lines} lines, {tokens} tokens",
                output_kind.label()
            )
        }
        Body::Adaptation {
            knob,
            from,
            to,
            reason,
        } => format!("adaptation {knob} {from} → {to}: {reason}"),
        Body::ModeChanged { from, to } => format!("mode {from:?} → {to:?}"),
        Body::Config { overrides } => format!("config {overrides:?}"),
        Body::Format { version } => format!("ledger format {version}"),
        Body::Event { event } => {
            format!(
                "event {}",
                truncate_chars(&serde_json::to_string(event).unwrap_or_default(), 200)
            )
        }
    };
    format!("#{} {body}", e.seq)
}

/// Cause chain for an action (a-N), output (o-N), or ledger entry number.
pub fn why(entries: &[Entry], id: &str) -> Result<String> {
    let by_seq: HashMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let target = entries
        .iter()
        .find(|e| match &e.body {
            Body::Action { id: a, .. } => a == id,
            Body::Output { id: o, .. } => o == id,
            _ => id.parse::<u64>().ok() == Some(e.seq),
        })
        .ok_or_else(|| anyhow!("no entry `{id}`"))?;
    let mut out = String::new();
    let mut stack = vec![(target, 0usize)];
    let mut seen = HashSet::new();
    while let Some((e, depth)) = stack.pop() {
        if !seen.insert(e.seq) {
            continue;
        }
        let prefix = if depth == 0 {
            String::new()
        } else {
            format!("{}└ caused by ", "  ".repeat(depth - 1))
        };
        let _ = writeln!(out, "{prefix}{}", describe(e));
        for c in e.causes.iter().rev() {
            if let Some(ce) = by_seq.get(c) {
                stack.push((ce, depth + 1));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry {
            seq,
            ts_ms: 0,
            causes: vec![],
            body,
        }
    }

    fn shape(id: &str, from: usize, to: usize, applied: bool, unsupported: bool) -> Body {
        Body::Action {
            id: id.into(),
            action: ActionKind::Shape {
                output_id: "o-1".into(),
                from_tokens: from,
                to_tokens: to,
                from_lines: 10,
                to_lines: 2,
            },
            reason: "r".into(),
            applied,
            unsupported,
        }
    }

    fn output(id: &str, tokens: usize) -> Body {
        Body::Output {
            id: id.into(),
            blob: "h".into(),
            output_kind: winnow_core::perception::OutputKind::Test,
            lines: 10,
            tokens,
        }
    }

    #[test]
    fn noise_counts_failed_runs_but_savings_do_not() {
        let entries = vec![
            entry(0, output("o-1", 4000)),
            entry(1, shape("a-1", 4000, 1000, false, false)),
            entry(2, output("o-2", 5000)),
            entry(3, shape("a-2", 5000, 1000, false, true)),
            entry(4, output("o-3", 1000)),
        ];
        let r = render("t", &entries);
        assert!(
            r.contains("Bash output: 10.0k tokens (est.) · noise 70% (est.)"),
            "{r}"
        );
        assert!(r.contains("Shaping: 1 outputs · 4.0k → 1.0k tokens · saves 3.0k (30% of Bash output) [would apply]"), "{r}");
        assert!(r.contains("Not shapeable: 1 outputs"), "{r}");
    }

    #[test]
    fn token_counts_are_short() {
        assert_eq!(k(999), "999");
        assert_eq!(k(1_500), "1.5k");
        assert_eq!(k(816_181_800), "816.2M");
    }

    #[test]
    fn loop_lines_show_duration() {
        let steer = |dur| Body::Action {
            id: "a-1".into(),
            action: ActionKind::Steer {
                trigger: SteerTrigger::Loop {
                    signatures: vec!["s".into()],
                    duration_ms: dur,
                },
                text: "winnow: stuck".into(),
            },
            reason: "r".into(),
            applied: false,
            unsupported: false,
        };
        let r = render(
            "t",
            &[entry(0, steer(Some(22 * 60_000))), entry(1, steer(None))],
        );
        assert!(r.contains("Loops (22 min in loops): 2"), "{r}");
        assert!(
            r.contains("[would steer] after 22 min: winnow: stuck"),
            "{r}"
        );
        assert!(r.contains("  - [would steer] winnow: stuck"), "{r}");
    }

    #[test]
    fn usage_dedupes_by_message_and_last_line_wins() {
        let lines = [
            r#"{"uuid":"a","message":{"id":"m1","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            r#"{"uuid":"b","message":{"id":"m1","usage":{"input_tokens":2,"cache_read_input_tokens":900,"cache_creation_input_tokens":100,"output_tokens":40}}}"#,
            r#"{"uuid":"c","message":{"id":"m2","usage":{"input_tokens":3,"cache_read_input_tokens":1000,"output_tokens":7}}}"#,
            r#"{"uuid":"d","message":{"id":"m2","usage":{"input_tokens":3,"cache_read_input_tokens":1000,"output_tokens":7}}}"#,
            r#"{"type":"user","message":{"content":"hi"}}"#,
        ];
        let u = usage_from_lines(lines.iter().map(|s| s.to_string())).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 5,
                cache_creation: 100,
                cache_read: 1900,
                output: 47,
                messages: 2
            }
        );
        assert_eq!(u.prompt_total(), 2005);
    }
}
