//! Controller tests.

use super::*;
use crate::session::DEFAULT_LEVEL;
use serde_json::json;

const PYTEST: &str = include_str!("../../../../fixtures/outputs/pytest_fail.txt");

fn bash(cmd: &str, out: &str) -> Event {
    Event::ToolCompleted {
        tool_use_id: "t".into(),
        tool: "Bash".into(),
        input: json!({ "command": cmd }),
        output: out.into(),
        failed: false,
    }
}

fn edited(path: &str, hash: &str) -> Event {
    Event::FileEdited {
        tool_use_id: "e".into(),
        path: path.into(),
        after_hash: Some(hash.into()),
    }
}

fn ctl(mode: Mode) -> Controller {
    Controller::new(Config::default(), Capabilities::CLAUDE_CODE, mode)
}

#[test]
fn look_mode_records_but_never_responds() {
    let mut c = ctl(Mode::Look);
    let d = c.handle(bash("pytest", PYTEST), 0);
    assert!(d.response.is_empty());
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            applied: false,
            action: ActionKind::Shape { .. },
            ..
        }
    )));
    assert_eq!(d.blobs.len(), 1, "original output is stored");
}

#[test]
fn optimize_mode_replaces_output_with_footer() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(bash("pytest", PYTEST), 0);
    let text = d.response.replace_output.expect("summary");
    assert!(text.contains("tend show o-1"));
    assert!(text.contains("FAILED tests/test_api.py::test_get_order"));
    assert!(estimate_tokens(&text) * 3 < estimate_tokens(PYTEST));
}

#[test]
fn failed_run_is_not_shaped_when_the_agent_cannot_replace_it() {
    let mut c = ctl(Mode::Optimize);
    let mut ev = bash("pytest", PYTEST);
    if let Event::ToolCompleted { failed, .. } = &mut ev {
        *failed = true;
    }
    let d = c.handle(ev, 0);
    assert_eq!(d.response.replace_output, None);
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            applied: false,
            unsupported: true,
            action: ActionKind::Shape { .. },
            ..
        }
    )));
}

/// A large file view is never shaped: the agent asked for that content.
#[test]
fn file_views_are_not_shaped() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(bash("cat tests/test_api_output.txt", PYTEST), 0);
    assert_eq!(d.response.replace_output, None);
    assert!(!d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            action: ActionKind::Shape { .. },
            ..
        }
    )));
}

#[test]
fn reopen_loosens_shape_level() {
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest", PYTEST), 0);
    let d = c.handle(bash("tend show o-1", PYTEST), 1);
    assert!(d.response.is_empty(), "never shape a re-open");
    assert!(d
        .entries
        .iter()
        .any(|e| matches!(&e.body, Body::Adaptation { .. })));
    assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);
}

/// R1.3: the agent varies the pipe and flags; it is still the same test run.
#[test]
fn loops_and_reruns_use_normalized_commands() {
    let mut c = ctl(Mode::Optimize);
    let variants = [
        "pytest tests/test_api.py -v 2>&1 | tail -40",
        "python -m pytest tests/test_api.py -x --tb=short",
        "source .venv/bin/activate && pytest tests/test_api.py -vv | head -80",
        "pytest -v tests/test_api.py 2>&1",
    ];
    let mut steered = false;
    for (i, cmd) in variants.iter().enumerate() {
        if i > 0 {
            c.handle(edited("src/app.py", &format!("h{i}")), i as u64);
        }
        let d = c.handle(bash(cmd, PYTEST), i as u64);
        steered |= d.entries.iter().any(|e| {
            matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Steer { .. },
                    ..
                }
            )
        });
    }
    assert!(steered, "one loop across command variants");

    // Asking for more of the same output right after a summary is a re-open.
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest -v | tail -20", PYTEST), 0);
    c.handle(bash("pytest -v | tail -200", PYTEST), 1);
    assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);
}

/// R1.3: Bash writes are fix attempts too.
#[test]
fn loops_count_bash_writes() {
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest tests/test_api.py -q", PYTEST), 0);
    let mut steered = false;
    for i in 1..=3u64 {
        c.handle(bash(&format!("sed -i 's/{i}/x/' src/app.py"), ""), i);
        let d = c.handle(bash("pytest tests/test_api.py -q | tail -30", PYTEST), i);
        steered |= d.entries.iter().any(|e| {
            matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Steer { .. },
                    ..
                }
            )
        });
    }
    assert!(steered);
}

#[test]
fn immediate_rerun_loosens_like_a_reopen() {
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest -v", PYTEST), 0);
    let d = c.handle(bash("pytest -v", PYTEST), 1);
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Adaptation { reason, .. } if reason.contains("re-ran the command behind o-1")
    )));
    assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL - 1);

    // After an edit, a re-run is a normal retry, not a re-open.
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest -v", PYTEST), 0);
    c.handle(edited("src/app.py", "h1"), 1);
    c.handle(bash("pytest -v", PYTEST), 2);
    assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL);

    // In look mode the agent saw the full output: nothing to re-open.
    let mut c = ctl(Mode::Look);
    c.handle(bash("pytest -v", PYTEST), 0);
    c.handle(bash("pytest -v", PYTEST), 1);
    assert_eq!(c.session().shape_level(OutputKind::Test), DEFAULT_LEVEL);
}

#[test]
fn loop_is_steered_after_three_distinct_edits() {
    let mut c = ctl(Mode::Optimize);
    assert!(c
        .handle(bash("pytest", PYTEST), 0)
        .response
        .add_context
        .is_empty());
    for i in 1..=3 {
        c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
        let d = c.handle(bash("pytest", PYTEST), i * 60_000);
        let steered = d
            .response
            .add_context
            .iter()
            .any(|t| t.contains("unchanged after 3 fix attempts"));
        assert_eq!(steered, i == 3, "run after edit {i}");
        if i == 3 {
            assert!(d.entries.iter().any(|e| matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Steer {
                        trigger: SteerTrigger::Loop {
                            duration_ms: Some(180_000),
                            ..
                        },
                        ..
                    },
                    ..
                }
            )));
        }
    }
}

/// I3, L1 and L2: a passing run that logs an error, re-run through `tee`, is no loop.
#[test]
fn logged_error_in_a_passing_run_is_not_a_loop() {
    let out = "ConnectionError: refused (retrying)\n3 passed in 0.12s\n";
    let mut c = ctl(Mode::Optimize);
    for i in 0..5 {
        c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
        let d = c.handle(bash("pytest -s tests/test_retry.py", out), i * 60_000);
        assert!(d.response.add_context.is_empty(), "run {i}");
    }
    let mut c = ctl(Mode::Optimize);
    for i in 0..5 {
        let d = c.handle(bash("pytest -q 2>&1 | tee /tmp/t.log", PYTEST), i * 60_000);
        assert!(d.response.add_context.is_empty(), "tee run {i}");
    }
}

/// Checkpoint C2: reading a log or test file that shows a failure is not a run.
#[test]
fn viewing_failure_text_is_not_a_loop() {
    let mut c = ctl(Mode::Optimize);
    for i in 0..=4 {
        c.handle(edited("src/app.py", &format!("h{i}")), i * 60_000);
        let d = c.handle(bash("cat test-output.log", PYTEST), i * 60_000);
        assert!(d.response.add_context.is_empty(), "view {i}");
    }
}

/// Checkpoint C1: a large code read is never summarized, however it is spelled.
#[test]
fn large_file_reads_are_not_shaped() {
    let code: String = (0..3000)
        .map(|i| format!("    let value_{i} = compute(input, {i}); // step {i}\n"))
        .collect();
    let mut c = ctl(Mode::Optimize);
    for cmd in [
        "cd /app; cat -n src/lib.rs",
        "wc -l src/lib.rs && sed -n 1,3000p src/lib.rs",
        "git diff HEAD -- src/lib.rs",
        "grep -n let src/lib.rs",
    ] {
        let d = c.handle(bash(cmd, &code), 0);
        assert!(d.response.replace_output.is_none(), "{cmd}");
    }
}

fn weakening() -> Event {
    Event::EditProposed {
        tool_use_id: "x".into(),
        path: "tests/test_api.py".into(),
        old_text: "assert a == 1\nassert b == 2\n".into(),
        new_text: "assert a == 1\n".into(),
    }
}

fn bash_changed(path: &str, old: &str, new: &str) -> Event {
    Event::FileChanged {
        tool_use_id: "t1".into(),
        path: path.into(),
        old_text: old.into(),
        new_text: new.into(),
    }
}

/// R1.4: a Bash write already happened, so the guard warns in context.
#[test]
fn guard_warns_after_a_bash_write_weakens_a_test() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(
        bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
        0,
    );
    assert!(d.response.permission.is_none());
    let [m] = &d.response.add_context[..] else {
        panic!("{:?}", d.response)
    };
    assert!(
        m.contains("the last command changed /r/tests/test_api.py"),
        "{m}"
    );
    assert!(m.contains("assertions removed (2 → 1)"), "{m}");
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            action: ActionKind::Guard {
                decision: GuardDecision::Warn,
                ..
            },
            applied: true,
            ..
        }
    )));
    // The ledger keeps blob references, not the file text.
    assert!(
        d.entries.iter().any(|e| matches!(&e.body, Body::Event { event: Event::FileChanged { old_text, .. } } if old_text.starts_with("<blob:")))
    );

    // Look mode records the same action and changes nothing.
    let mut c = ctl(Mode::Look);
    let d = c.handle(
        bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
        0,
    );
    assert!(d.response.is_empty());
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            action: ActionKind::Guard { .. },
            applied: false,
            ..
        }
    )));

    // A new test file, or a change outside tests, is not flagged.
    let d = c.handle(bash_changed("/r/tests/test_new.py", "", "assert a\n"), 1);
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Action { .. })));
    let d = c.handle(bash_changed("/r/src/app.py", "assert a\n", ""), 2);
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Judgment { .. })));
}

/// D9: a hook already past its deadline is handled as look mode: nothing applied,
/// nothing returned, nothing to allow later.
#[test]
fn late_hook_records_nothing_as_applied() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle_unanswered(weakening(), 0);
    assert!(d.response.is_empty());
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            action: ActionKind::Guard { .. },
            applied: false,
            ..
        }
    )));
    assert!(c.allow(None, 1).is_err(), "no block reached the agent");
    assert_eq!(c.mode(), Mode::Optimize, "mode is restored");
    assert!(c.handle(weakening(), 2).response.permission.is_some());
}

/// D9: a reply that missed its deadline after handling is recorded as undelivered,
/// and state that assumed the agent got it is dropped.
#[test]
fn undelivered_reply_is_recorded_and_forgotten() {
    let mut c = ctl(Mode::Optimize);
    let deny = c.handle(weakening(), 0);
    let wrap = c.handle(requested("t1", "pytest -q", true), 1);
    assert!(wrap.response.wrap);
    let d = c.undelivered(&[deny, wrap], 2);
    assert!(matches!(
        &d.entries[..],
        [Entry { body: Body::Undelivered { actions, .. }, .. }] if actions.len() == 2
    ));
    assert!(
        c.allow(None, 3).is_err(),
        "the deny never reached the agent"
    );
    assert_eq!(c.filter_level("t1"), None, "the call runs unwrapped");
    assert!(c.undelivered(&[], 4).entries.is_empty());
}

/// T3: a summary that never reached the agent does not make the next run a re-open,
/// and an undelivered deny cannot be allowed by id.
#[test]
fn undelivered_actions_leave_no_state_behind() {
    let mut c = ctl(Mode::Optimize);
    let shaped = c.handle(bash("pytest", PYTEST), 0);
    assert!(shaped.response.replace_output.is_some());
    c.undelivered(&[shaped], 1);
    let d = c.handle(bash("pytest", PYTEST), 2);
    assert!(
        !d.entries
            .iter()
            .any(|e| matches!(&e.body, Body::Adaptation { .. })),
        "not a re-open: the agent never saw a summary"
    );
    let deny = c.handle(weakening(), 3);
    c.undelivered(&[deny], 4);
    assert!(c.allow(Some("a-2"), 5).is_err() && c.allow(None, 5).is_err());
}

#[test]
fn guard_denies_with_reason_in_claude_code() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(weakening(), 0);
    match d.response.permission {
        Some(Permission::Deny(m)) => {
            assert!(m.contains("assertions removed (2 → 1)"), "{m}");
            assert!(m.contains("! tend allow"), "{m}");
        }
        p => panic!("want deny, got {p:?}"),
    }
}

#[test]
fn user_allows_a_blocked_edit_once() {
    let mut c = ctl(Mode::Optimize);
    c.handle(weakening(), 0);
    assert!(c.allow(Some("a-9"), 1).is_err(), "unknown action");
    let d = c.allow(None, 1).expect("allow the latest");
    assert!(matches!(&d.entries[0].body, Body::Override { action } if action == "a-1"));
    let override_seq = d.entries[0].seq;

    let d = c.handle(weakening(), 2);
    assert_eq!(d.response.permission, None, "allowed once");
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action { applied: false, reason, .. } if reason.contains("tend allow a-1")
    ) && e.causes.contains(&override_seq)));

    let d = c.handle(weakening(), 3);
    assert!(
        matches!(d.response.permission, Some(Permission::Deny(_))),
        "only once"
    );
}

/// F4: an edit of a file the guard does not read keeps hashes only, no copies.
#[test]
fn edits_of_non_test_files_store_no_blobs() {
    let mut c = ctl(Mode::Look);
    let edit = |path: &str| Event::EditProposed {
        tool_use_id: "x".into(),
        path: path.into(),
        old_text: "a = 1\n".repeat(1000),
        new_text: "a = 2\n".repeat(1000),
    };
    assert!(c.handle(edit("package-lock.json"), 0).blobs.is_empty());
    assert_eq!(c.handle(edit("tests/test_a.py"), 1).blobs.len(), 2);
}

/// G6: an agent that cannot deny gets a warning, and a warning is nothing to allow.
#[test]
fn guard_warns_where_the_agent_cannot_deny() {
    let caps = Capabilities {
        deny: false,
        ask: false,
        ..Capabilities::CLAUDE_CODE
    };
    let mut c = Controller::new(Config::default(), caps, Mode::Optimize);
    let d = c.handle(weakening(), 0);
    assert_eq!(d.response.permission, None);
    assert!(d
        .response
        .add_context
        .iter()
        .any(|m| m.contains("may weaken tests")));
    assert!(!d.response.add_context[0].contains("tend allow"));
    assert!(c.allow(None, 1).is_err(), "a warning blocks nothing");
}

/// G6: the Bash-write warning (R1.4) is not a block either.
#[test]
fn bash_write_warning_is_nothing_to_allow() {
    let mut c = ctl(Mode::Optimize);
    c.handle(
        bash_changed("/r/tests/test_api.py", "assert a\nassert b\n", "assert a\n"),
        0,
    );
    assert!(c.allow(None, 1).is_err());
}

/// G2: an edit of a Rust unit test in `src/` is judged.
#[test]
fn inline_rust_test_edit_is_guarded() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "x".into(),
            path: "crates/shop/src/orders.rs".into(),
            old_text: "#[test]\nfn total() {\n    assert_eq!(total(), 6);\n}\n".into(),
            new_text: "#[test]\n#[ignore]\nfn total() {\n    assert_eq!(total(), 6);\n}\n".into(),
        },
        0,
    );
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
}

fn requested(id: &str, cmd: &str, rewrite_ok: bool) -> Event {
    Event::ToolRequested {
        tool_use_id: id.into(),
        tool: "Bash".into(),
        input: json!({ "command": cmd }),
        rewrite_ok,
    }
}

fn failed_bash(id: &str, cmd: &str, out: &str) -> Event {
    Event::ToolCompleted {
        tool_use_id: id.into(),
        tool: "Bash".into(),
        input: json!({ "command": cmd }),
        output: out.into(),
        failed: true,
    }
}

#[test]
fn failing_test_run_is_shaped_through_tend_run() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(requested("t1", "pytest -v", true), 0);
    assert!(d.response.wrap, "test command is wrapped");

    // `tend run` reports the full output and prints the summary.
    let (d, text) = c.filter("t1", PYTEST.into(), 1);
    let text = text.expect("summary");
    assert!(text.contains("tend show o-1"));
    assert!(text.contains("FAILED tests/test_api.py::test_get_order"));
    assert!(
        !d.entries.iter().any(|e| matches!(
            e.body,
            Body::Action {
                action: ActionKind::Shape { .. },
                ..
            }
        )),
        "not recorded until the agent's view is known"
    );
    assert_eq!(c.filter("t1", PYTEST.into(), 1).1, None, "only once");

    // The hook sees the wrapper and the summary: record the shape, do not re-shape.
    let seen = format!("Exit code 1\n{text}");
    let d = c.handle(
        failed_bash("t1", "tend run --id t1 --sh 'pytest -v'", &seen),
        2,
    );
    assert!(d.response.is_empty());
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Output { .. })));
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            applied: true,
            action: ActionKind::Shape { .. },
            ..
        }
    )));
    assert_eq!(d.blobs.len(), 1, "the summary the agent saw is stored");
}

/// R0.2 (D3): the daemon answered too late, so tend run printed the original.
#[test]
fn late_filter_reply_is_recorded_as_not_applied() {
    let mut c = ctl(Mode::Optimize);
    c.handle(requested("t1", "pytest -v", true), 0);
    c.filter("t1", PYTEST.into(), 1).1.expect("summary");
    let seen = format!("Exit code 1\n{PYTEST}");
    let d = c.handle(
        failed_bash("t1", "tend run --id t1 --sh 'pytest -v'", &seen),
        2,
    );
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action { applied: false, action: ActionKind::Shape { .. }, reason, .. }
            if reason.contains("printed the original")
    )));
    // No summary was seen, so running it again is not a re-open.
    c.handle(requested("t2", "pytest -v", true), 3);
    let (d, _) = c.filter("t2", PYTEST.into(), 4);
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Adaptation { .. })));
}

/// Auto mode denied a wrapped call ([Auto-Mode Bypass]): no tool result, no filter.
/// The retry must run as the agent wrote it.
#[test]
fn denied_wrap_is_not_repeated() {
    let mut c = ctl(Mode::Optimize);
    assert!(
        c.handle(requested("t1", "pytest -q tests/a.py", true), 0)
            .response
            .wrap
    );
    c.handle(Event::TurnEnded, 1);
    let d = c.handle(requested("t2", "pytest -v tests/a.py | tail", true), 2);
    assert!(!d.response.wrap, "same command key: not wrapped again");
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action { applied: false, action: ActionKind::Wrap { .. }, reason, .. }
            if reason.contains("got no result before")
    )));
    assert!(
        c.handle(requested("t3", "pytest tests/b.py", true), 3)
            .response
            .wrap
    );
}

/// R3: after `/clear`, failures of the old conversation do not count toward a loop.
#[test]
fn new_conversation_starts_loops_afresh() {
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest", PYTEST), 0);
    for i in 1..=2 {
        c.handle(edited("src/app.py", &format!("h{i}")), i);
        c.handle(bash("pytest", PYTEST), i);
    }
    c.handle(
        Event::ConversationChanged {
            transcript_path: None,
            reason: "clear".into(),
        },
        3,
    );
    c.handle(edited("src/app.py", "h3"), 4);
    let d = c.handle(bash("pytest", PYTEST), 4);
    assert!(
        d.response.add_context.is_empty(),
        "no loop across conversations"
    );
}

/// F8: Claude Code ran the agent's command, not the wrapper (it ignored
/// `updatedInput`). The wrap is recorded as not applied, and nothing more is wrapped.
#[test]
fn ignored_rewrite_is_recorded_and_stops_wrapping() {
    let mut c = ctl(Mode::Optimize);
    assert!(
        c.handle(requested("t1", "pytest tests/a.py", true), 0)
            .response
            .wrap
    );
    let mut ran = bash("pytest tests/a.py", PYTEST);
    if let Event::ToolCompleted { tool_use_id, .. } = &mut ran {
        *tool_use_id = "t1".into();
    }
    let d = c.handle(ran, 1);
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Undelivered { actions, reason } if actions == &["a-1"] && reason.contains("ignored tend's rewrite")
    )));
    let d = c.handle(requested("t2", "pytest tests/b.py", true), 2);
    assert!(!d.response.wrap);
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action { reason, .. } if reason.contains("ignored tend's command rewrite")
    )));
}

fn denied(id: &str) -> Event {
    Event::ToolDenied {
        tool_use_id: id.into(),
        tool: "Bash".into(),
        reason: "[Auto-Mode Bypass] wraps the command".into(),
    }
}

/// R3.7: auto mode denied a wrapped call. The agent may retry, and the retry runs
/// unwrapped at once (not only after the turn ends).
#[test]
fn denied_wrapped_call_is_retried_unwrapped() {
    let mut c = ctl(Mode::Optimize);
    assert!(
        c.handle(requested("t1", "pytest -q", true), 0)
            .response
            .wrap
    );
    let d = c.handle(denied("t1"), 1);
    assert!(d.response.retry);
    let retry = d.entries.iter().find(|e| {
        matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Retry { .. },
                applied: true,
                ..
            }
        )
    });
    assert_eq!(
        retry.map(|e| e.causes.len()),
        Some(2),
        "the denial and the wrap"
    );
    assert!(
        !c.handle(requested("t2", "pytest -q", true), 2)
            .response
            .wrap
    );

    // A denial of a call tend did not change is only recorded.
    let d = c.handle(denied("t9"), 3);
    assert!(d.response.is_empty());
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Action { .. })));
}

/// R0.2/R0.7: a stopped call never gets a tool result; the turn end settles it.
#[test]
fn stopped_wrapped_call_is_settled_at_turn_end() {
    let mut c = ctl(Mode::Optimize);
    c.handle(requested("t1", "pytest -v", true), 0);
    c.filter("t1", PYTEST.into(), 1);
    c.handle(requested("t2", "pytest -q", true), 2);
    let d = c.handle(Event::TurnEnded, 3);
    let settled = d
        .entries
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                Body::Action { applied: false, action: ActionKind::Shape { .. }, reason, .. }
                    if reason.contains("No tool result arrived")
            )
        })
        .count();
    assert_eq!(settled, 1, "only the call that had a summary");
    assert_eq!(c.filter("t2", PYTEST.into(), 4).1, None, "entries dropped");
}

#[test]
fn loops_are_found_through_tend_run() {
    let mut c = ctl(Mode::Optimize);
    let mut steered = false;
    for i in 0..4u64 {
        if i > 0 {
            c.handle(edited("src/app.py", &format!("h{i}")), i);
        }
        let id = format!("t{i}");
        c.handle(requested(&id, "pytest -v", true), i);
        let (d, _) = c.filter(&id, PYTEST.into(), i);
        steered |= d.entries.iter().any(|e| {
            matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Steer { .. },
                    ..
                }
            )
        });
    }
    assert!(steered, "original command keys the loop detector");
}

#[test]
fn wrap_downgrades_honestly() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(requested("a", "pytest", false), 0);
    assert!(!d.response.wrap, "permission mode may prompt");
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Action {
            unsupported: true,
            action: ActionKind::Wrap { .. },
            ..
        }
    )));
    // R1.1: a leading cd stays in the agent's shell, so the rest can be wrapped;
    // a cd after the test would be lost.
    assert!(
        c.handle(requested("b", "cd api && pytest", true), 1)
            .response
            .wrap
    );
    assert!(
        !c.handle(requested("b1", "pytest && cd api", true), 1)
            .response
            .wrap
    );
    // R1.1: state that does not outlive the call is fine to wrap.
    for (id, cmd) in [
        (
            "w1",
            "source .venv/bin/activate && python -m pytest -q 2>&1 | tail -20",
        ),
        ("w2", "export CI=1 && pytest"),
        ("w3", "set -e; CI=1 pytest -x"),
        (
            "w4",
            "cd /home/u/app && source .venv/bin/activate && pytest -q",
        ),
    ] {
        assert!(c.handle(requested(id, cmd, true), 1).response.wrap, "{cmd}");
    }
    // R0.5: newline and group forms of a state change are caught too.
    for (id, cmd) in [
        ("b2", "cd api\npytest"),
        ("b3", "{ cd api; pytest; }"),
        ("b4", "git commit -m 'fix pytest'"),
    ] {
        assert!(
            !c.handle(requested(id, cmd, true), 1).response.wrap,
            "{cmd}"
        );
    }
    assert!(!c.handle(requested("c", "ls -la", true), 2).response.wrap);
    assert!(
        !ctl(Mode::Look)
            .handle(requested("d", "pytest", true), 0)
            .response
            .wrap
    );
    // Unwrapped or unknown ids: print the original.
    assert_eq!(c.filter("zzz", "out".into(), 3).1, None);
    // A wrapped call whose `tend run` never reported falls back to normal handling.
    c.handle(requested("e", "pytest -v", true), 4);
    let d = c.handle(
        failed_bash("e", "tend run --id e --sh 'pytest -v'", PYTEST),
        5,
    );
    assert!(d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Output { .. })));
}

#[test]
fn agent_cannot_allow_itself() {
    let mut c = ctl(Mode::Optimize);
    let req = |cmd: &str| requested("b", cmd, false);
    let d = c.handle(req("cd /x && tend allow a-1"), 0);
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
    let d = c.handle(req("/home/u/.cargo/bin/tend allow"), 0);
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
    // R0.8: a commit message that mentions it is not running it.
    let d = c.handle(req("git commit -m 'document tend allow'"), 1);
    assert!(d.response.is_empty());
    assert!(c.handle(req("tend show o-1"), 1).response.is_empty());
    assert!(ctl(Mode::Look)
        .handle(req("tend allow"), 0)
        .response
        .is_empty());
}

#[test]
fn guard_downgrades_ask_to_deny_on_codex() {
    let mut c = Controller::new(Config::default(), Capabilities::CODEX, Mode::Optimize);
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "x".into(),
            path: "tests/test_api.py".into(),
            old_text: "it('a', f)".into(),
            new_text: "it.skip('a', f)".into(),
        },
        0,
    );
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
}

#[test]
fn non_default_config_is_recorded_at_session_start() {
    let start = || Event::SessionStarted {
        agent: "a".into(),
        cwd: "/".into(),
        transcript_path: None,
        mode: None,
    };
    let d = ctl(Mode::Look).handle(start(), 0);
    assert!(!d
        .entries
        .iter()
        .any(|e| matches!(e.body, Body::Config { .. })));

    let cfg = Config {
        loop_threshold: 4,
        ..Config::default()
    };
    let mut c = Controller::new(cfg, Capabilities::CLAUDE_CODE, Mode::Look);
    let d = c.handle(start(), 0);
    let want: BTreeMap<String, String> = [("loop_threshold".to_string(), "4".to_string())].into();
    assert!(d
        .entries
        .iter()
        .any(|e| matches!(&e.body, Body::Config { overrides } if *overrides == want)));
}

#[test]
fn first_entry_is_the_format_version() {
    let mut c = ctl(Mode::Look);
    let first = c.handle(edited("a", "h1"), 5).entries;
    assert_eq!(
        first[0].body,
        Body::Format {
            version: LEDGER_FORMAT
        }
    );
    assert_eq!(first[0].seq, 0);
    let next = c.handle(edited("a", "h2"), 6).entries;
    assert!(next.iter().all(|e| !matches!(e.body, Body::Format { .. })));
}

#[test]
fn loop_time_skips_idle_gaps() {
    let mut c = ctl(Mode::Optimize);
    let min = 60_000;
    c.handle(bash("pytest", PYTEST), 0);
    // Edit 1 a minute later; then the session sits idle for two days.
    c.handle(edited("src/app.py", "h1"), min);
    c.handle(bash("pytest", PYTEST), 2 * min);
    let back = 2 * 24 * 60 * min;
    c.handle(edited("src/app.py", "h2"), back);
    c.handle(bash("pytest", PYTEST), back + min);
    c.handle(edited("src/app.py", "h3"), back + 2 * min);
    let d = c.handle(bash("pytest", PYTEST), back + 3 * min);
    let dur = d.entries.iter().find_map(|e| match &e.body {
        Body::Action {
            action:
                ActionKind::Steer {
                    trigger: SteerTrigger::Loop { duration_ms, .. },
                    ..
                },
            ..
        } => *duration_ms,
        _ => None,
    });
    // 2 min + 10 min (capped idle gap) + 3 min.
    assert_eq!(dur, Some(15 * min));
}

#[test]
fn causes_point_backwards() {
    let mut c = ctl(Mode::Look);
    let mut all = Vec::new();
    all.extend(c.handle(bash("pytest", PYTEST), 0).entries);
    all.extend(c.handle(edited("a", "h1"), 1).entries);
    for (i, e) in all.iter().enumerate() {
        assert_eq!(e.seq, i as u64);
        assert!(e.causes.iter().all(|c| *c < e.seq));
    }
}

#[test]
fn entries_round_trip_through_json() {
    let mut c = ctl(Mode::Optimize);
    for e in c.handle(bash("pytest", PYTEST), 0).entries {
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Entry>(&s).unwrap(), e);
    }
}

/// E4: after `assert 0 == 1` fails, changing the test's expected 1 to 0 is flagged.
#[test]
fn expected_value_changed_to_the_failing_value_is_flagged() {
    let out = "FAILED tests/test_t.py::test_total - assert 0 == 1\n1 failed, 3 passed in 0.10s\n";
    let mut c = ctl(Mode::Optimize);
    c.handle(bash("pytest tests/test_t.py", out), 0);
    let edit = |new: &str| Event::EditProposed {
        tool_use_id: "x".into(),
        path: "tests/test_t.py".into(),
        old_text: "    assert total([]) == 1\n".into(),
        new_text: new.into(),
    };
    let d = c.handle(edit("    assert total([]) == 0\n"), 1);
    match &d.response.permission {
        Some(Permission::Deny(m)) => {
            assert!(m.contains("failing run's actual value (1 → 0)"), "{m}")
        }
        p => panic!("{p:?}"),
    }
    // Fixing the code instead, or a passing run since, is not flagged.
    c.handle(bash("pytest tests/test_t.py", "4 passed in 0.10s\n"), 2);
    assert!(c
        .handle(edit("    assert total([]) == 0\n"), 3)
        .response
        .permission
        .is_none());
}

/// E5: a snapshot-rewriting run is pointed out, never blocked.
#[test]
fn snapshot_update_runs_get_a_warning() {
    let mut c = ctl(Mode::Optimize);
    let d = c.handle(requested("t1", "npx jest -u", true), 0);
    assert!(d.response.permission.is_none());
    assert!(d
        .response
        .add_context
        .iter()
        .any(|m| m.contains("`jest -u` rewrites expected snapshots")));
    let mut look = ctl(Mode::Look);
    let d = look.handle(requested("t1", "cargo insta accept", true), 0);
    assert!(d.response.add_context.is_empty());
    assert!(d
        .entries
        .iter()
        .any(|e| matches!(&e.body, Body::Action { applied: false, .. })));
}

/// E7: `.tend.toml` protects paths and adds test paths.
#[test]
fn repo_policy_protects_paths_and_adds_tests() {
    let (policy, w) = crate::policy::parse(
        "protected = [\"migrations/**\"]\ntest_paths = [\"checks/*.py\"]\n",
        "/r",
    );
    assert!(w.is_empty());
    let mut c = ctl(Mode::Optimize);
    c.set_policy(policy);
    let edit = |path: &str, old: &str, new: &str| Event::EditProposed {
        tool_use_id: "x".into(),
        path: path.into(),
        old_text: old.into(),
        new_text: new.into(),
    };
    let d = c.handle(
        edit("/r/migrations/0002.sql", "ALTER TABLE a;", "ALTER TABLE b;"),
        0,
    );
    match &d.response.permission {
        Some(Permission::Deny(m)) => assert!(
            m.contains("protected by .tend.toml (`migrations/**`)"),
            "{m}"
        ),
        p => panic!("{p:?}"),
    }
    // A checks/ file is a test file now: removing its assertion is flagged.
    let d = c.handle(
        edit("/r/checks/a.py", "assert a\nassert b\n", "assert a\n"),
        1,
    );
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
    // Elsewhere, nothing changes.
    assert!(c
        .handle(edit("/r/src/app.py", "a = 1", "a = 2"), 2)
        .response
        .permission
        .is_none());
}
