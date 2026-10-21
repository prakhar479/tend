//! Session report, `why` cause chains, and output lookup. Reads ledger entries only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tend_core::perception::{classify, test_write_targets, writes_files, OutputKind};
use tend_core::record::SteerTrigger;
use tend_core::text::truncate_chars;
use tend_core::{ActionKind, Body, Config, Entry, Event, PeerActivity};

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

/// The transcript at `path`, or the file of the same name in another project dir: a
/// conversation resumed after its folder moved (or renamed) still writes to the old one.
pub fn find_transcript(path: &Path) -> std::path::PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    let (Some(name), Some(projects)) = (path.file_name(), path.parent().and_then(Path::parent))
    else {
        return path.to_path_buf();
    };
    std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(name))
        .find(|p| p.exists())
        .unwrap_or_else(|| path.to_path_buf())
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Totals {
    pub bash_tokens: u64,
    /// Tokens a faithful summary would remove, including output the agent cannot replace.
    pub noise: u64,
    pub loops: u64,
    /// First occurrence to steer, summed over loops.
    pub loop_ms: u64,
    /// Bash output tokens by output kind.
    pub by_kind: BTreeMap<OutputKind, u64>,
    pub test_runs: u64,
    /// Test runs tend wrapped (or would wrap, in look mode or retro).
    pub wrapped: u64,
    /// Edits through Edit/Write (the test guard sees these).
    pub tool_edits: u64,
    /// Bash calls that write files (`sed -i`, heredocs).
    pub bash_writes: u64,
    /// Bash writes that name a test file: the guard compares these before and after (R1.4).
    pub bash_test_writes: u64,
    /// Test-guard flags (a retry the user allowed is not a new flag).
    pub guard_flags: u64,
    /// Flags the user lifted with `tend allow`: likely false positives (R2.3).
    pub guard_allowed: u64,
    /// Loop nudges after which the failure recurred at most once more (R2.3).
    pub loops_stopped: u64,
}

/// A loop nudge counts as followed when its failures recur at most this often after it.
const STOPPED_WITHIN_RUNS: usize = 1;

pub fn totals(entries: &[Entry]) -> Totals {
    let mut t = Totals::default();
    let overrides: HashSet<u64> = entries
        .iter()
        .filter(|e| matches!(e.body, Body::Override { .. }))
        .map(|e| e.seq)
        .collect();
    t.guard_allowed = overrides.len() as u64;
    for (i, e) in entries.iter().enumerate() {
        if let Body::Action {
            action:
                ActionKind::Steer {
                    trigger: SteerTrigger::Loop { signatures, .. },
                    ..
                },
            ..
        } = &e.body
        {
            let again = entries[i + 1..]
                .iter()
                .filter(|later| {
                    matches!(&later.body, Body::Judgment { question, subject, .. }
                        if question == "same_failure" && signatures.contains(subject))
                })
                .count();
            t.loops_stopped += u64::from(again <= STOPPED_WITHIN_RUNS);
        }
    }
    for e in entries {
        match &e.body {
            Body::Output {
                tokens,
                output_kind,
                ..
            } => {
                t.bash_tokens += *tokens as u64;
                *t.by_kind.entry(*output_kind).or_default() += *tokens as u64;
                t.test_runs += u64::from(*output_kind == OutputKind::Test);
            }
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
            Body::Action {
                action: ActionKind::Wrap { .. },
                unsupported: false,
                reason,
                ..
            } if !reason.starts_with("Not wrapped") => t.wrapped += 1,
            Body::Action {
                action: ActionKind::Guard { .. },
                ..
            } if !e.causes.iter().any(|c| overrides.contains(c)) => t.guard_flags += 1,
            Body::Event {
                event: Event::FileEdited { .. },
            } => t.tool_edits += 1,
            Body::Event {
                event: Event::ToolCompleted { tool, input, .. },
            } if tool == "Bash" => {
                let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
                t.bash_writes += u64::from(writes_files(cmd));
                t.bash_test_writes += u64::from(!test_write_targets(cmd).is_empty());
            }
            _ => {}
        }
    }
    t
}

/// Where Bash output goes, how many test runs tend wraps, and which edits the guard sees.
pub fn coverage(
    by_kind: &BTreeMap<OutputKind, u64>,
    test_runs: u64,
    wrapped: u64,
    tool_edits: u64,
    bash_writes: u64,
    bash_test_writes: u64,
) -> String {
    let mut out = String::new();
    if by_kind.values().any(|n| *n > 0) {
        let _ = writeln!(out, "Bash output by kind: {}", kind_shares(by_kind));
    }
    if test_runs > 0 {
        let _ = writeln!(
            out,
            "Test runs: {test_runs} · wrapped (or would be, in auto mode): {wrapped} ({}%)",
            pct(wrapped, test_runs)
        );
    }
    if tool_edits + bash_writes > 0 {
        let _ = writeln!(
            out,
            "Edits: {tool_edits} through Edit/Write (guarded before they run) · {bash_writes} Bash writes, {bash_test_writes} naming a test file (guarded after they run)"
        );
    }
    out
}

/// What tend did in a session, in one line: the launcher prints it at the end (R3.2).
pub fn one_line(entries: &[Entry]) -> String {
    let t = totals(entries);
    let (mut tools, mut shaped, mut saved, mut applied) = (0u64, 0u64, 0u64, false);
    for e in entries {
        match &e.body {
            Body::Event {
                event: Event::ToolCompleted { .. },
            } => tools += 1,
            Body::Action {
                action:
                    ActionKind::Shape {
                        from_tokens,
                        to_tokens,
                        ..
                    },
                applied: a,
                unsupported: false,
                ..
            } => {
                shaped += 1;
                saved += from_tokens.saturating_sub(*to_tokens) as u64;
                applied |= *a;
            }
            _ => {}
        }
    }
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = vec![plural(tools, "tool call", "tool calls")];
    if shaped > 0 {
        let (verb, saving) = if applied {
            ("shaped", "saved")
        } else {
            ("would shape", "saving")
        };
        parts.push(format!(
            "{verb} {}, {saving} {} tokens",
            plural(shaped, "output", "outputs"),
            k(saved)
        ));
    }
    if t.guard_flags > 0 {
        parts.push(plural(t.guard_flags, "guard flag", "guard flags"));
    }
    if t.loops > 0 {
        parts.push(plural(t.loops, "loop", "loops"));
    }
    let ignored = entries.iter().any(|e| {
        matches!(&e.body, Body::Undelivered { reason, .. } if reason.contains("ignored tend's rewrite"))
    });
    if ignored {
        parts.push("Claude Code ignored tend's rewrite (run scripts/conformance.py)".into());
    }
    parts.join(" · ")
}

/// `tend status`: "tend look · 12 tool calls · …" (R3.5).
pub fn status_line(entries: &[Entry]) -> String {
    let mut mode = None;
    for e in entries {
        match &e.body {
            Body::Event {
                event: Event::SessionStarted { mode: Some(m), .. },
            } => mode = Some(*m),
            Body::ModeChanged { to, .. } => mode = Some(*to),
            _ => {}
        }
    }
    let mode = mode.map_or("?".to_string(), |m| format!("{m:?}").to_lowercase());
    format!("tend {mode} · {}", one_line(entries))
}

/// How often the guard and the loop nudges were right, as far as the ledger shows (R2.3).
/// `live`: from a session's ledger, where the user could allow a flag (not a retro).
pub fn precision(t: &Totals, live: bool) -> String {
    let mut out = String::new();
    if t.guard_flags > 0 && !live {
        let _ = writeln!(
            out,
            "Guard flags: {} (a retro cannot tell which ones the user would allow)",
            t.guard_flags
        );
    } else if t.guard_flags > 0 {
        let _ = writeln!(
            out,
            "Guard precision: {} flagged · {} allowed by the user ({}%, likely false positives)",
            t.guard_flags,
            t.guard_allowed,
            pct(t.guard_allowed, t.guard_flags)
        );
    }
    if t.loops > 0 {
        let _ = writeln!(
            out,
            "Loop nudges: {} · failure stopped within {STOPPED_WITHIN_RUNS} more run: {} ({}%)",
            t.loops,
            t.loops_stopped,
            pct(t.loops_stopped, t.loops)
        );
    }
    out
}

/// "view 51% · search 18% · test 8%": shares of Bash output by kind, largest first.
pub fn kind_shares(by_kind: &BTreeMap<OutputKind, u64>) -> String {
    let total: u64 = by_kind.values().sum();
    let mut v: Vec<(&OutputKind, &u64)> = by_kind.iter().filter(|(_, n)| **n > 0).collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    v.iter()
        .map(|(k, n)| format!("{} {}%", k.label(), pct(**n, total)))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `live`: a session ledger (not a retro), where the user could act on flags.
/// The session report. Without `full`, sections with nothing in them are left out, and
/// what needs the user's attention (loops, guard actions, undelivered replies) comes
/// first (I4, U4).
pub fn render(label: &str, entries: &[Entry], live: bool, full: bool) -> String {
    let mut agent = String::from("?");
    let mut cwd = String::new();
    let mut mode = String::from("?");
    let mut transcripts: Vec<String> = Vec::new();
    let (mut conversations, mut ignored) = (1u64, 0u64);
    let t = totals(entries);
    let (mut tools, mut bash, mut reopens, mut judgments) = (0u64, 0u64, 0u64, 0u64);
    let (mut shaped, mut before, mut after, mut shaped_applied) = (0u64, 0u64, 0u64, 0u64);
    let (mut unshapeable, mut wrapped, mut denied, mut undelivered) = (0u64, 0u64, 0u64, 0u64);
    let (mut loops, mut cycles, mut guards, mut adaptations, mut modes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut config = Vec::new();
    let defaults = Config::default().fields();
    // Fleet (F1): this session's workspace, the peers it saw, and the notices.
    let mut workspace: Option<(String, String)> = None;
    let mut peers: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut fleet = Vec::new();

    let overrides: HashSet<u64> = entries
        .iter()
        .filter(|e| matches!(e.body, Body::Override { .. }))
        .map(|e| e.seq)
        .collect();
    // Ledgers from before the `unsupported` flag (no `format` entry) recorded shapes of
    // failed runs as supported. Claude Code cannot replace those (S2): infer it.
    let versioned = entries
        .iter()
        .any(|e| matches!(e.body, Body::Format { .. }));
    let by_seq: HashMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let legacy_failed = |e: &Entry| {
        !versioned
            && e.causes
                .first()
                .and_then(|o| by_seq.get(o))
                .and_then(|o| o.causes.first())
                .and_then(|ev| by_seq.get(ev))
                .is_some_and(|ev| {
                    matches!(
                        &ev.body,
                        Body::Event {
                            event: Event::ToolCompleted { failed: true, .. }
                        }
                    )
                })
    };
    for e in entries {
        match &e.body {
            Body::Event { event } => match event {
                Event::SessionStarted {
                    agent: a,
                    transcript_path,
                    mode: m,
                    cwd: c,
                } => {
                    agent = a.clone();
                    cwd = c.clone();
                    transcripts.extend(transcript_path.clone());
                    if let Some(m) = m {
                        mode = format!("{m:?}").to_lowercase();
                    }
                }
                Event::ConversationChanged {
                    transcript_path, ..
                } => {
                    conversations += 1;
                    transcripts.extend(transcript_path.clone());
                }
                Event::Workspace { top, common } => {
                    workspace = Some((top.clone(), common.clone()));
                }
                Event::Peer {
                    session,
                    activity: PeerActivity::Joined { top, common, .. },
                    ..
                } => {
                    peers.insert(session.clone(), (top.clone(), common.clone()));
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
            Body::Undelivered { actions, reason } if reason.contains("ignored tend's rewrite") => {
                ignored += actions.len() as u64
            }
            Body::Undelivered { actions, .. } => undelivered += actions.len() as u64,
            Body::Override { action } => guards.push(format!("[user allowed] {action} once")),
            Body::Judgment { .. } => judgments += 1,
            Body::Action {
                action,
                applied,
                reason,
                unsupported,
                ..
            } => match action {
                ActionKind::Shape { .. } if *unsupported || legacy_failed(e) => unshapeable += 1,
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
                ActionKind::Fleet { text, .. } => fleet.push(format!(
                    "{} {}",
                    status(*applied, "told", "would tell"),
                    text.trim_start_matches("tend: ")
                )),
                ActionKind::Wrap { .. } => wrapped += u64::from(*applied),
                ActionKind::Retry { .. } => denied += 1,
                ActionKind::BlockCommand { command } => guards.push(format!(
                    "{} the agent's `{}`: {reason}",
                    status(*applied, "blocked", "would block"),
                    truncate_chars(command, 60)
                )),
                ActionKind::Guard { .. } if e.causes.iter().any(|c| overrides.contains(c)) => {
                    guards.push(format!("[allowed] {reason}"));
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
    let convs = if conversations > 1 {
        format!(" · {conversations} conversations")
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "Session {label} · {agent} · {duration} · mode: {mode}{convs}"
    );
    if !cwd.is_empty() {
        let _ = writeln!(out, "Project: {cwd}");
    }
    if !config.is_empty() {
        let _ = writeln!(out, "Config: {}", config.join(" · "));
    }
    if let (Some((top, common)), false) = (&workspace, peers.is_empty()) {
        use tend_core::fleet::{relation, Relation};
        let (mut same, mut worktree, mut other) = (0, 0, 0);
        for (t, c) in peers.values() {
            match relation(top, common, t, c) {
                Relation::SameCheckout => same += 1,
                Relation::Worktree => worktree += 1,
                Relation::Other => other += 1,
            }
        }
        let n = peers.len();
        let _ = writeln!(
            out,
            "Fleet: {n} other tend session{} ({same} in this checkout, {worktree} in other worktrees, {other} elsewhere)",
            if n == 1 { "" } else { "s" }
        );
    }
    let loop_title = if t.loop_ms > 0 {
        format!("Loops ({} in loops)", minutes(t.loop_ms))
    } else {
        "Loops".to_string()
    };
    let attention = [
        (loop_title.as_str(), &loops),
        ("Edit cycles", &cycles),
        ("Test guard", &guards),
        ("Fleet notices", &fleet),
    ];
    if !full {
        let mut parts: Vec<String> = attention
            .iter()
            .filter(|(_, items)| !items.is_empty())
            .map(|(title, items)| {
                let name = title.split(" (").next().unwrap_or(title).to_lowercase();
                format!("{} {name}", items.len())
            })
            .collect();
        if undelivered > 0 {
            parts.push(format!("{undelivered} not delivered"));
        }
        let _ = writeln!(
            out,
            "Needs attention: {}",
            if parts.is_empty() {
                "nothing".to_string()
            } else {
                parts.join(" · ")
            }
        );
        for (title, items) in &attention {
            if !items.is_empty() {
                write_section(&mut out, title, items);
            }
        }
    }
    // One transcript per conversation (`/clear` and `/resume` start another, R3).
    let usage = transcripts
        .iter()
        .filter_map(|t| usage_from_transcript(&find_transcript(Path::new(t))))
        .reduce(|mut a, b| {
            a.add(&b);
            a
        });
    match usage {
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
    if full || shaped > 0 {
        let _ = writeln!(
            out,
            "Shaping: {shaped} outputs · {} → {} tokens · saves {} ({}% of Bash output) [{verb}]",
            k(before),
            k(after),
            k(saved),
            pct(saved, t.bash_tokens)
        );
    }
    if wrapped > 0 {
        // R3.7: auto mode sometimes denies a wrapped call; each denial is a Retry action.
        let _ = writeln!(
            out,
            "Wrapped test runs: {wrapped} (summarized even when they fail) · denied by auto mode: {denied} ({}%)",
            pct(denied, wrapped)
        );
    }
    if unshapeable > 0 {
        let _ = writeln!(
            out,
            "Not shapeable: {unshapeable} outputs (the agent cannot replace failed-call output)"
        );
    }
    let _ = write!(
        out,
        "{}",
        coverage(
            &t.by_kind,
            t.test_runs,
            t.wrapped,
            t.tool_edits,
            t.bash_writes,
            t.bash_test_writes
        )
    );
    if undelivered > 0 {
        let _ = writeln!(
            out,
            "Not delivered: {undelivered} actions (the reply came after the hook timeout)"
        );
    }
    if ignored > 0 {
        let _ = writeln!(
            out,
            "Rewrites ignored by Claude Code: {ignored} (wrapping stopped; run scripts/conformance.py)"
        );
    }
    let _ = write!(out, "{}", precision(&t, live));
    if full || reopens > 0 {
        let _ = writeln!(out, "Re-opens: {reopens}");
    }
    let mut rest = vec![("Adaptations", &adaptations), ("Mode changes", &modes)];
    if full {
        rest.splice(0..0, attention);
    }
    for (title, items) in rest {
        if full || !items.is_empty() {
            write_section(&mut out, title, items);
        }
    }
    if full {
        let _ = writeln!(out, "Judgments recorded: {judgments}");
    }
    out
}

fn write_section(out: &mut String, title: &str, items: &[String]) {
    let _ = writeln!(out, "{title}: {}", items.len());
    for i in items {
        let _ = writeln!(out, "  - {}", truncate_chars(i, 200));
    }
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
                ActionKind::BlockCommand { .. } => "block",
                ActionKind::Wrap { .. } => "wrap",
                ActionKind::Retry { .. } => "retry",
                ActionKind::Fleet { .. } => "fleet",
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
        Body::Undelivered { actions, reason } => {
            format!("undelivered {}: {reason}", actions.join(", "))
        }
        Body::Config { overrides } => format!("config {overrides:?}"),
        Body::Format { version } => format!("ledger format {version}"),
        Body::Override { action } => format!("user allowed {action} once"),
        Body::Event {
            event:
                Event::Peer {
                    session,
                    seq,
                    activity,
                    ..
                },
        } => {
            let what = match activity {
                PeerActivity::Joined { top, .. } => format!("started in {top}"),
                PeerActivity::Edited { path } => format!("edited {path}"),
                PeerActivity::Git { command } => format!("ran `{command}`"),
                PeerActivity::Left => "ended".to_string(),
            };
            format!("peer session {session} (its entry #{seq}) {what}")
        }
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
/// The entry an id names: an action (a-N), an output (o-N), or an entry number.
pub fn find_target<'a>(entries: &'a [Entry], id: &str) -> Option<&'a Entry> {
    entries.iter().find(|e| match &e.body {
        Body::Action { id: a, .. } => a == id,
        Body::Output { id: o, .. } => o == id,
        _ => id.parse::<u64>().ok() == Some(e.seq),
    })
}

/// One line for an entry: what it is and why it exists.
pub fn describe_entry(e: &Entry) -> String {
    describe(e)
}

/// `why`, and for each peer event in the chain, the entry it names in the peer's
/// ledger (F1). `lookup(session, seq)` reads it; a missing peer ledger is skipped.
pub fn why_across(
    entries: &[Entry],
    id: &str,
    lookup: &dyn Fn(&str, u64) -> Option<Entry>,
) -> Result<String> {
    let mut out = why(entries, id)?;
    let by_seq: HashMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let target = find_target(entries, id).ok_or_else(|| anyhow!("no entry `{id}`"))?;
    let mut peers = Vec::new();
    let mut stack = vec![target];
    let mut seen = HashSet::new();
    while let Some(e) = stack.pop() {
        if !seen.insert(e.seq) {
            continue;
        }
        if let Body::Event {
            event: Event::Peer { session, seq, .. },
        } = &e.body
        {
            peers.push((session.clone(), *seq));
        }
        stack.extend(e.causes.iter().filter_map(|c| by_seq.get(c).copied()));
    }
    peers.sort();
    for (session, seq) in peers {
        if let Some(pe) = lookup(&session, seq) {
            let _ = writeln!(out, "In session {session}: {}", describe(&pe));
        }
    }
    Ok(out)
}

pub fn why(entries: &[Entry], id: &str) -> Result<String> {
    let by_seq: HashMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let target = find_target(entries, id).ok_or_else(|| anyhow!("no entry `{id}`"))?;
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

    /// A resumed conversation whose project folder was renamed: the reported path does
    /// not exist, the same file under another project dir does.
    #[test]
    fn transcript_is_found_in_another_project_dir() {
        let root = std::env::temp_dir().join(format!("tend-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("-old")).unwrap();
        std::fs::write(root.join("-old/s.jsonl"), "{}").unwrap();
        let found = find_transcript(&root.join("-new/s.jsonl"));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, root.join("-old/s.jsonl"));
    }

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
            output_kind: tend_core::perception::OutputKind::Test,
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
        let r = render("t", &entries, true, true);
        assert!(
            r.contains("Bash output: 10.0k tokens (est.) · noise 70% (est.)"),
            "{r}"
        );
        assert!(r.contains("Shaping: 1 outputs · 4.0k → 1.0k tokens · saves 3.0k (30% of Bash output) [would apply]"), "{r}");
        assert!(r.contains("Not shapeable: 1 outputs"), "{r}");
    }

    #[test]
    fn allowed_edit_is_shown_as_allowed() {
        let guard = |id: &str, applied: bool| Body::Action {
            id: id.into(),
            action: ActionKind::Guard {
                path: "tests/t.py".into(),
                decision: tend_core::guard::GuardDecision::Deny,
            },
            reason: format!("reason {id}"),
            applied,
            unsupported: false,
        };
        let mut allowed = entry(2, guard("a-2", false));
        allowed.causes = vec![1];
        let r = render(
            "t",
            &[
                entry(0, guard("a-1", true)),
                entry(
                    1,
                    Body::Override {
                        action: "a-1".into(),
                    },
                ),
                allowed,
            ],
            true,
            false,
        );
        assert!(r.contains("[deny] reason a-1"), "{r}");
        assert!(r.contains("[user allowed] a-1 once"), "{r}");
        assert!(r.contains("[allowed] reason a-2"), "{r}");
    }

    #[test]
    fn old_ledgers_infer_unshapeable_failed_runs() {
        let completed = |failed| Body::Event {
            event: Event::ToolCompleted {
                tool_use_id: "t".into(),
                tool: "Bash".into(),
                input: serde_json::json!({ "command": "pytest" }),
                output: "<blob:h>".into(),
                failed,
            },
        };
        let chain = |base: u64, failed: bool| {
            let mut o = entry(base + 1, output("o", 4000));
            o.causes = vec![base];
            let mut a = entry(base + 2, shape("a", 4000, 1000, true, false));
            a.causes = vec![base + 1];
            vec![entry(base, completed(failed)), o, a]
        };
        let mut old = chain(0, true);
        old.extend(chain(3, false));
        let r = render("t", &old, true, true);
        assert!(r.contains("Shaping: 1 outputs"), "{r}");
        assert!(r.contains("Not shapeable: 1 outputs"), "{r}");

        // A versioned ledger is trusted as written.
        let mut new = vec![entry(100, Body::Format { version: 1 })];
        new.extend(chain(0, true));
        assert!(render("t", &new, true, true).contains("Shaping: 1 outputs"));
    }

    /// R2.3: guard flags the user allowed, and loops whose failure stopped after the nudge.
    #[test]
    fn precision_counts_allowed_flags_and_stopped_loops() {
        let guard = |id: &str| Body::Action {
            id: id.into(),
            action: ActionKind::Guard {
                path: "tests/test_a.py".into(),
                decision: tend_core::guard::GuardDecision::Deny,
            },
            reason: "r".into(),
            applied: true,
            unsupported: false,
        };
        let steer = |sig: &str| Body::Action {
            id: "a-9".into(),
            action: ActionKind::Steer {
                trigger: SteerTrigger::Loop {
                    signatures: vec![sig.into()],
                    duration_ms: None,
                },
                text: "tend: stuck".into(),
            },
            reason: "r".into(),
            applied: true,
            unsupported: false,
        };
        let again = |sig: &str| Body::Judgment {
            question: "same_failure".into(),
            version: 1,
            subject: sig.into(),
            answer: tend_core::broker::Answer::Same,
            source: tend_core::broker::Source::Rule,
        };
        let mut retry = entry(4, guard("a-3"));
        retry.causes = vec![3];
        let entries = [
            entry(0, guard("a-1")),
            entry(1, guard("a-2")),
            entry(2, guard("a-2b")),
            entry(
                3,
                Body::Override {
                    action: "a-2".into(),
                },
            ),
            retry,
            entry(5, steer("s1")),
            entry(6, again("s1")),
            entry(7, steer("s2")),
            entry(8, again("s2")),
            entry(9, again("s2")),
        ];
        let t = totals(&entries);
        assert_eq!((t.guard_flags, t.guard_allowed), (3, 1));
        assert_eq!((t.loops, t.loops_stopped), (2, 1));
        let p = precision(&t, true);
        assert!(
            p.contains("Guard precision: 3 flagged · 1 allowed by the user (33%"),
            "{p}"
        );
        assert!(
            p.contains("Loop nudges: 2 · failure stopped within 1 more run: 1 (50%)"),
            "{p}"
        );
    }

    #[test]
    fn one_line_says_what_tend_did() {
        let saved = |applied| shape("a-1", 5_000, 400, applied, false);
        let entries = [entry(0, saved(true)), entry(1, saved(true))];
        assert_eq!(
            one_line(&entries),
            "0 tool calls · shaped 2 outputs, saved 9.2k tokens"
        );
        assert_eq!(
            one_line(&[entry(0, saved(false))]),
            "0 tool calls · would shape 1 output, saving 4.6k tokens"
        );
        // Output the agent cannot replace is not counted as shaped.
        assert_eq!(
            one_line(&[entry(0, shape("a-1", 5_000, 400, false, true))]),
            "0 tool calls"
        );
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
                text: "tend: stuck".into(),
            },
            reason: "r".into(),
            applied: false,
            unsupported: false,
        };
        let r = render(
            "t",
            &[entry(0, steer(Some(22 * 60_000))), entry(1, steer(None))],
            true,
            false,
        );
        assert!(r.contains("Loops (22 min in loops): 2"), "{r}");
        assert!(r.contains("[would steer] after 22 min: tend: stuck"), "{r}");
        assert!(r.contains("  - [would steer] tend: stuck"), "{r}");
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
