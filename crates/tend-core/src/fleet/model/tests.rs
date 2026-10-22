//! Fleet model tests.

use super::*;
use crate::fleet::spec::{resolve, Preset, SpecFile};

fn spec(v: serde_json::Value) -> Spec {
    let f: SpecFile = serde_json::from_value(v).unwrap();
    resolve(&f).unwrap()
}

fn preset(p: Preset) -> Spec {
    resolve(&SpecFile {
        preset: Some(p),
        ..SpecFile::default()
    })
    .unwrap()
}

/// A model, and every record it wrote, with a clock.
struct Sim {
    m: Model,
    log: Vec<FleetRecord>,
    now: u64,
}

impl Sim {
    fn new(s: Spec) -> Self {
        Self {
            m: Model::new(s),
            log: Vec::new(),
            now: 1_000,
        }
    }

    /// One event; its commands.
    fn ev(&mut self, e: FleetEvent) -> Vec<Command> {
        let recs = self.m.step(e, self.now);
        let cmds = commands(&recs);
        self.log.extend(recs);
        cmds
    }

    fn at(&mut self, ms: u64, e: FleetEvent) -> Vec<Command> {
        self.now = ms;
        self.ev(e)
    }

    /// Start every slot the commands spawn, and what those starts spawn, as the hub
    /// would.
    fn start_all(&mut self, cmds: &[Command]) {
        for c in cmds {
            if let Command::Spawn { slot, .. } = c {
                let session = format!("s-{slot}");
                let more = self.ev(FleetEvent::Started {
                    slot: slot.clone(),
                    session,
                });
                self.start_all(&more);
            }
        }
    }

    fn state(&self, id: &str) -> SlotState {
        self.m.slots()[id].state
    }
}

fn commands(recs: &[FleetRecord]) -> Vec<Command> {
    recs.iter()
        .filter_map(|r| match &r.body {
            FleetBody::Command { command, .. } => Some(command.clone()),
            _ => None,
        })
        .collect()
}

fn spawned(cmds: &[Command]) -> Vec<&str> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Spawn { slot, .. } => Some(slot.as_str()),
            _ => None,
        })
        .collect()
}

fn delivered(cmds: &[Command]) -> Vec<(String, String)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Deliver {
                to: Address::Slot { id },
                text,
                ..
            } => Some((id.clone(), text.clone())),
            Command::Deliver {
                to: Address::User,
                text,
                ..
            } => Some(("user".into(), text.clone())),
            _ => None,
        })
        .collect()
}

fn refused(cmds: &[Command]) -> Option<String> {
    cmds.iter().find_map(|c| match c {
        Command::Answer { text, .. } if text.starts_with("tend: refused") => Some(text.clone()),
        _ => None,
    })
}

fn slot(id: &str) -> Actor {
    Actor::Slot { id: id.into() }
}

fn msg(from: Actor, to: Address, kind: MsgKind) -> FleetEvent {
    FleetEvent::Message {
        from,
        to,
        kind,
        text: "hi".into(),
    }
}

fn role(name: &str) -> Address {
    Address::Role { name: name.into() }
}

/// Up starts the stream roles' minimum sessions; oneshot roles wait for tasks; an
/// attached role is the user's to start.
#[test]
fn up_per_preset() {
    let table: [(Preset, &[&str], Option<&str>); 9] = [
        (Preset::Solo, &[], Some("agent")),
        (Preset::Peers, &["peer-1", "peer-2"], None),
        (Preset::Lead, &["lead-1"], None),
        (Preset::Pipeline, &[], None),
        (Preset::Review, &[], None),
        (Preset::BestOf, &[], None),
        (Preset::Specialists, &["lead-1"], None),
        (Preset::Pool, &[], None),
        (Preset::Tree, &["lead-1", "sub-1", "sub-2"], None),
    ];
    for (p, want, attach) in table {
        let mut r = Sim::new(preset(p));
        let cmds = r.ev(FleetEvent::Up);
        assert_eq!(spawned(&cmds), want, "{p:?}");
        let attached: Vec<&Command> = cmds
            .iter()
            .filter(|c| matches!(c, Command::Attach { .. }))
            .collect();
        assert_eq!(attached.len(), usize::from(attach.is_some()), "{p:?}");
        if let Some(a) = attach {
            assert!(matches!(attached[0], Command::Attach { role } if role == a));
        }
    }
}

#[test]
fn spawn_carries_the_role() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"lead": {"model": "opus", "permission_mode": "auto"}}
    })));
    let cmds = r.ev(FleetEvent::Up);
    let Command::Spawn {
        run,
        model,
        permission_mode,
        leader,
        brief,
        ..
    } = &cmds[0]
    else {
        panic!("{cmds:?}")
    };
    assert_eq!(*run, Run::Stream);
    assert_eq!(model.as_deref(), Some("opus"));
    assert_eq!(permission_mode.as_deref(), Some("auto"));
    assert_eq!(*leader, None);
    assert!(brief.contains("tend task add"), "{brief}");
}

#[test]
fn a_crash_restarts_with_backoff_then_gives_up() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let mut t = 10_000;
    for n in 1..=MAX_RESTARTS {
        let cmds = r.at(
            t,
            FleetEvent::Exited {
                slot: "lead-1".into(),
                code: Some(1),
            },
        );
        let wait = BACKOFF_MS << (n - 1);
        assert!(
            matches!(&cmds[..], [Command::Backoff { slot, at_ms }] if slot == "lead-1" && *at_ms == t + wait),
            "{cmds:?}"
        );
        assert_eq!(r.state("lead-1"), SlotState::Waiting);
        assert!(
            spawned(&r.at(t + wait - 1, FleetEvent::Tick)).is_empty(),
            "too early"
        );
        let cmds = r.at(t + wait, FleetEvent::Tick);
        assert_eq!(spawned(&cmds), ["lead-1"]);
        r.start_all(&cmds);
        t += wait + 1_000;
    }
    let cmds = r.at(
        t,
        FleetEvent::Exited {
            slot: "lead-1".into(),
            code: None,
        },
    );
    assert!(
        matches!(&cmds[..], [Command::Notify { text }] if text.contains("stopped restarting")),
        "{cmds:?}"
    );
    assert_eq!(r.state("lead-1"), SlotState::Failed);
    assert!(spawned(&r.at(t + 999_999, FleetEvent::Tick)).is_empty());
}

#[test]
fn a_oneshot_that_finishes_is_done() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let cmds = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    assert_eq!(spawned(&cmds), ["worker-1"]);
    r.start_all(&cmds);
    let cmds = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(0),
    });
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(r.state("worker-1"), SlotState::Done);
    // A oneshot that fails is not restarted: a task it had goes to a fresh session.
    let cmds = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    r.start_all(&cmds);
    let cmds = r.ev(FleetEvent::Exited {
        slot: "worker-2".into(),
        code: Some(2),
    });
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(r.state("worker-2"), SlotState::Failed);
}

#[test]
fn the_session_limit_makes_slots_wait_until_room() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"sessions": 2}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let first = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    assert_eq!(spawned(&first), ["worker-1"]);
    r.start_all(&first);
    let second = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    assert!(spawned(&second).is_empty());
    assert!(
        matches!(&second[..], [Command::Notify { text }] if text.contains("worker-2 waits") && text.contains("limit of 2")),
        "{second:?}"
    );
    assert_eq!(r.state("worker-2"), SlotState::Waiting);
    // Told once, not on every tick.
    assert!(r.ev(FleetEvent::Tick).is_empty());
    let cmds = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(0),
    });
    assert_eq!(spawned(&cmds), ["worker-2"]);
}

#[test]
fn a_silent_busy_session_is_told_then_interrupted_then_restarted() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "stuck_minutes": 1}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    // Idle sessions are never stuck: they wait for work.
    assert!(r.at(10 * 60_000, FleetEvent::Tick).is_empty());
    let t = r.now;
    r.ev(msg(Actor::User, role("lead"), MsgKind::Task));
    assert!(r.at(t + 59_000, FleetEvent::Tick).is_empty());
    let c = r.at(t + 60_000, FleetEvent::Tick);
    assert!(
        matches!(&c[..], [Command::Notify { text }] if text.contains("lead-1 has shown no activity")),
        "{c:?}"
    );
    assert!(
        r.at(t + 90_000, FleetEvent::Tick).is_empty(),
        "once per step"
    );
    let c = r.at(t + 120_000, FleetEvent::Tick);
    assert!(
        matches!(&c[..], [Command::Interrupt { slot }] if slot == "lead-1"),
        "{c:?}"
    );
    let c = r.at(t + 180_000, FleetEvent::Tick);
    assert!(
        matches!(&c[..], [Command::Stop { slot }] if slot == "lead-1"),
        "{c:?}"
    );
    // That stop was to restart it.
    let c = r.ev(FleetEvent::Exited {
        slot: "lead-1".into(),
        code: None,
    });
    assert!(matches!(&c[..], [Command::Backoff { .. }]), "{c:?}");
}

#[test]
fn activity_keeps_a_busy_session_from_looking_stuck() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "stuck_minutes": 1}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    r.ev(msg(Actor::User, role("lead"), MsgKind::Task));
    let t = r.now;
    r.at(
        t + 50_000,
        FleetEvent::Activity {
            slot: "lead-1".into(),
            turn_ended: false,
        },
    );
    assert!(r.at(t + 100_000, FleetEvent::Tick).is_empty());
    r.at(
        t + 105_000,
        FleetEvent::Activity {
            slot: "lead-1".into(),
            turn_ended: true,
        },
    );
    assert!(
        r.at(t + 999_000, FleetEvent::Tick).is_empty(),
        "the turn ended"
    );
}

#[test]
fn scale_stream_roles_within_their_range() {
    let mut r = Sim::new(preset(Preset::Peers));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(FleetEvent::Scale {
        role: "peer".into(),
        n: 3,
    });
    assert_eq!(spawned(&c), ["peer-3"]);
    r.start_all(&c);
    // peer-2 is busy; the newest idle one goes first.
    r.ev(msg(
        Actor::User,
        Address::Slot {
            id: "peer-2".into(),
        },
        MsgKind::Task,
    ));
    let c = r.ev(FleetEvent::Scale {
        role: "peer".into(),
        n: 2,
    });
    assert!(
        matches!(&c[..], [Command::Stop { slot }] if slot == "peer-3"),
        "{c:?}"
    );
    for (role, n, why) in [
        ("peer", 9, "outside that"),
        ("peer", 1, "outside that"),
        ("ghost", 2, "no role ghost"),
    ] {
        let c = r.ev(FleetEvent::Scale {
            role: role.into(),
            n,
        });
        assert!(
            refused(&c).is_some_and(|t| t.contains(why)),
            "{role} {n}: {c:?}"
        );
    }
    let mut r = Sim::new(preset(Preset::Lead));
    r.ev(FleetEvent::Up);
    let c = r.ev(FleetEvent::Scale {
        role: "worker".into(),
        n: 2,
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("not a stream role")),
        "{c:?}"
    );
}

#[test]
fn only_roles_that_may_spawn_do_and_within_their_maximum() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"sessions": 8}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for _ in 0..4 {
        let c = r.ev(FleetEvent::SpawnRequest {
            by: slot("lead-1"),
            role: "worker".into(),
        });
        r.start_all(&c);
    }
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("maximum of 4")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("worker-1"),
        role: "worker".into(),
    });
    assert!(
        matches!(&c[..], [Command::Answer { to: Actor::Slot { id }, text }] if id == "worker-1" && text.contains("may not spawn")),
        "the refusal goes to the asker: {c:?}"
    );
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "ghost".into(),
    });
    assert!(refused(&c).is_some(), "{c:?}");
    // The user may stop anything; a worker may not stop the lead.
    let c = r.ev(FleetEvent::StopRequest {
        by: slot("worker-1"),
        slot: "lead-1".into(),
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("team it leads")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::StopRequest {
        by: Actor::User,
        slot: "worker-2".into(),
    });
    assert!(
        matches!(&c[..], [Command::Stop { slot }] if slot == "worker-2"),
        "{c:?}"
    );
}

#[test]
fn messages_follow_the_channels() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"sessions": 4}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for _ in 0..2 {
        let c = r.ev(FleetEvent::SpawnRequest {
            by: slot("lead-1"),
            role: "worker".into(),
        });
        r.start_all(&c);
    }
    let lead = slot("lead-1");
    let w1 = slot("worker-1");
    // To a role: every running session of it.
    let c = r.ev(msg(lead.clone(), role("worker"), MsgKind::Task));
    let to: Vec<String> = delivered(&c).into_iter().map(|(t, _)| t).collect();
    assert_eq!(to, ["worker-1", "worker-2"]);
    let cases: [(Actor, Address, MsgKind, Option<&str>); 7] = [
        (w1.clone(), role("lead"), MsgKind::Report, Some("lead-1")),
        (
            w1.clone(),
            Address::Slot {
                id: "lead-1".into(),
            },
            MsgKind::Question,
            Some("lead-1"),
        ),
        (Actor::User, role("lead"), MsgKind::Task, Some("lead-1")),
        (lead.clone(), Address::User, MsgKind::Question, Some("user")),
        (w1.clone(), role("worker"), MsgKind::Task, None),
        (w1.clone(), Address::User, MsgKind::Report, None),
        (lead.clone(), role("worker"), MsgKind::Handoff, None),
    ];
    for (from, to, kind, want) in cases {
        let c = r.ev(msg(from.clone(), to.clone(), kind));
        match want {
            Some(id) => assert_eq!(
                delivered(&c).first().map(|d| d.0.as_str()),
                Some(id),
                "{from:?} → {to:?} {kind:?}: {c:?}"
            ),
            None => assert!(refused(&c).is_some(), "{from:?} → {to:?} {kind:?}: {c:?}"),
        }
    }
    // A worker's message to itself as a role reaches no one.
    let c = r.ev(msg(
        Actor::User,
        Address::Slot {
            id: "worker-1".into(),
        },
        MsgKind::Task,
    ));
    assert!(
        refused(&c).is_some(),
        "the lead preset has no user → worker channel: {c:?}"
    );
}

#[test]
fn messages_are_rate_limited_per_edge() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for _ in 0..RATE_PER_MIN {
        let c = r.ev(msg(Actor::User, role("lead"), MsgKind::Answer));
        assert_eq!(delivered(&c).len(), 1);
    }
    let c = r.ev(msg(Actor::User, role("lead"), MsgKind::Answer));
    assert!(
        refused(&c).is_some_and(|t| t.contains("messages a minute")),
        "{c:?}"
    );
    let t = r.now;
    let c = r.at(t + 60_000, msg(Actor::User, role("lead"), MsgKind::Answer));
    assert_eq!(delivered(&c).len(), 1);
}

/// The tree preset: a lead, two sub-leads, and each sub-lead's own workers.
#[test]
fn hierarchy_routes_spawns_and_limits_by_team() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "tree", "limits": {"sessions": 8}}),
    ));
    let up = r.ev(FleetEvent::Up);
    assert_eq!(spawned(&up), ["lead-1", "sub-1", "sub-2"]);
    r.start_all(&up);
    // A sub-lead spawns in its own team.
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-1"),
        role: "worker".into(),
    });
    assert!(
        matches!(&c[..], [Command::Spawn { slot, leader: Some(l), .. }] if slot == "sub-1/worker-1" && l == "sub-1"),
        "{c:?}"
    );
    r.start_all(&c);
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-2"),
        role: "worker".into(),
    });
    r.start_all(&c);
    // The team limit (2 sessions below each sub-lead) holds.
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-1"),
        role: "worker".into(),
    });
    r.start_all(&c);
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-1"),
        role: "worker".into(),
    });
    assert!(spawned(&c).is_empty(), "{c:?}");
    assert!(
        c.iter()
            .any(|c| matches!(c, Command::Notify { text } if text.contains("team of sub-1"))),
        "{c:?}"
    );
    // The lead cannot spawn into a sub-lead's team; it has no `worker` at its level.
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    assert!(refused(&c).is_some(), "{c:?}");

    // Messages: a sub-lead reaches its own workers only; a worker reaches its parent.
    let c = r.ev(msg(slot("sub-1"), role("worker"), MsgKind::Task));
    let to: Vec<String> = delivered(&c).into_iter().map(|(t, _)| t).collect();
    assert_eq!(to, ["sub-1/worker-1", "sub-1/worker-2"]);
    let c = r.ev(msg(
        slot("sub-1/worker-1"),
        role("parent"),
        MsgKind::Question,
    ));
    assert_eq!(delivered(&c).first().map(|d| d.0.as_str()), Some("sub-1"));
    let c = r.ev(msg(
        slot("sub-1/worker-1"),
        Address::Slot { id: "sub-1".into() },
        MsgKind::Report,
    ));
    assert_eq!(delivered(&c).first().map(|d| d.0.as_str()), Some("sub-1"));
    // Escalation goes through the parent: a worker cannot write to the lead or the user.
    for to in [
        role("lead"),
        Address::Slot {
            id: "lead-1".into(),
        },
        Address::User,
    ] {
        let c = r.ev(msg(slot("sub-1/worker-1"), to.clone(), MsgKind::Question));
        assert!(refused(&c).is_some(), "{to:?}: {c:?}");
    }
    // A worker of one team cannot write to the other team's sub-lead.
    let c = r.ev(msg(
        slot("sub-2/worker-1"),
        Address::Slot { id: "sub-1".into() },
        MsgKind::Question,
    ));
    assert!(refused(&c).is_some(), "{c:?}");
    // The lead talks to sub-leads; sub-leads report to the lead.
    let c = r.ev(msg(slot("lead-1"), role("sub"), MsgKind::Task));
    assert_eq!(delivered(&c).len(), 2);
    let c = r.ev(msg(slot("sub-2"), role("lead"), MsgKind::Report));
    assert_eq!(delivered(&c).first().map(|d| d.0.as_str()), Some("lead-1"));

    // Stops: a sub-lead stops its own workers only.
    let c = r.ev(FleetEvent::StopRequest {
        by: slot("sub-2"),
        slot: "sub-1/worker-1".into(),
    });
    assert!(refused(&c).is_some(), "{c:?}");
    let c = r.ev(FleetEvent::StopRequest {
        by: slot("sub-1"),
        slot: "sub-1/worker-1".into(),
    });
    assert!(matches!(&c[..], [Command::Stop { .. }]), "{c:?}");
}

#[test]
fn a_team_waits_while_its_leader_is_down() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "tree", "limits": {"sessions": 6}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-1"),
        role: "worker".into(),
    });
    r.start_all(&c);
    // The sub-lead crashes; a worker asked for meanwhile waits until it is back.
    let t = r.now;
    let c = r.at(
        t,
        FleetEvent::Exited {
            slot: "sub-1".into(),
            code: Some(1),
        },
    );
    assert!(
        matches!(&c[..], [Command::Backoff { slot, .. }] if slot == "sub-1"),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::SpawnRequest {
        by: Actor::User,
        role: "sub-1/worker".into(),
    });
    assert!(spawned(&c).is_empty(), "{c:?}");
    assert_eq!(r.state("sub-1/worker-2"), SlotState::Waiting);
    let c = r.at(t + BACKOFF_MS, FleetEvent::Tick);
    assert_eq!(
        spawned(&c),
        ["sub-1"],
        "the leader first; its worker waits for it"
    );
    let c2 = r.ev(FleetEvent::Started {
        slot: "sub-1".into(),
        session: "s2".into(),
    });
    assert_eq!(spawned(&c2), ["sub-1/worker-2"], "back with its leader");
    // The other sub-lead's team was never paused.
    assert_eq!(r.state("sub-2"), SlotState::Running);
}

#[test]
fn deep_trees_work_the_same_way() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "limits": {"depth": 3, "sessions": 4},
        "role": {"api": {"run": "stream", "replicas": 1, "team": {"preset": "lead",
            "role": {"db": {"run": "stream", "replicas": 1, "team": {"preset": "lead"}}}}}}
    })));
    let up = r.ev(FleetEvent::Up);
    assert_eq!(spawned(&up), ["api-1", "lead-1"]);
    r.start_all(&up);
    // api-1 leads a team whose `db` role starts with it, and db leads its own.
    let db = r.m.slots().get("api-1/db-1").map(|s| s.state);
    assert_eq!(db, Some(SlotState::Running));
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("api-1/db-1"),
        role: "worker".into(),
    });
    assert_eq!(spawned(&c), ["api-1/db-1/worker-1"], "{c:?}");
    r.start_all(&c);
    let c = r.ev(msg(
        slot("api-1/db-1/worker-1"),
        role("parent"),
        MsgKind::Report,
    ));
    assert_eq!(
        delivered(&c).first().map(|d| d.0.as_str()),
        Some("api-1/db-1")
    );
    let c = r.ev(msg(slot("api-1/db-1"), role("parent"), MsgKind::Report));
    assert_eq!(delivered(&c).first().map(|d| d.0.as_str()), Some("api-1"));
    // Four sessions are live: the fleet limit holds across levels.
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("api-1"),
        role: "worker".into(),
    });
    assert!(spawned(&c).is_empty(), "{c:?}");
}

#[test]
fn down_stops_everything_and_nothing_restarts() {
    let mut r = Sim::new(preset(Preset::Peers));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(FleetEvent::Down);
    assert_eq!(
        c.iter()
            .filter(|c| matches!(c, Command::Stop { .. }))
            .count(),
        2
    );
    let c = r.ev(FleetEvent::Exited {
        slot: "peer-1".into(),
        code: None,
    });
    assert!(c.is_empty(), "{c:?}");
    assert_eq!(r.state("peer-1"), SlotState::Stopped);
    assert!(r.at(999_999, FleetEvent::Tick).is_empty());
}

/// Every command names its event as the cause and has a reason; ids count up.
#[test]
fn every_command_is_recorded_with_its_cause_and_reason() {
    let mut r = Sim::new(preset(Preset::Tree));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    r.ev(msg(slot("lead-1"), role("sub"), MsgKind::Task));
    r.ev(FleetEvent::Exited {
        slot: "sub-2".into(),
        code: Some(3),
    });
    let mut last_event = None;
    let mut ids = Vec::new();
    for (i, rec) in r.log.iter().enumerate() {
        assert_eq!(rec.seq, i as u64, "seq is gapless across steps");
        match &rec.body {
            FleetBody::Event { .. } => last_event = Some(rec.seq),
            FleetBody::Command { id, reason, .. } => {
                assert_eq!(rec.causes, vec![last_event.unwrap()]);
                assert!(!reason.is_empty() && reason.ends_with('.'), "{reason}");
                ids.push(id.clone());
            }
        }
    }
    let want: Vec<String> = (1..=ids.len()).map(|n| format!("a-{n}")).collect();
    assert_eq!(ids, want);
}

/// Feeding the recorded events to a new model gives the same records (replay).
#[test]
fn replay_is_deterministic() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "tree", "limits": {"sessions": 5}, "stuck_minutes": 1}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(FleetEvent::SpawnRequest {
        by: slot("sub-1"),
        role: "worker".into(),
    });
    r.start_all(&c);
    r.ev(msg(slot("sub-1"), role("worker"), MsgKind::Task));
    r.at(
        5_000,
        FleetEvent::Exited {
            slot: "sub-2".into(),
            code: Some(1),
        },
    );
    r.at(200_000, FleetEvent::Tick);
    r.ev(FleetEvent::Scale {
        role: "sub".into(),
        n: 2,
    });
    r.ev(FleetEvent::Down);

    let mut again = Model::new(r.m.spec().clone());
    let mut replayed = Vec::new();
    for rec in &r.log {
        if let FleetBody::Event { event } = &rec.body {
            replayed.extend(again.step(event.clone(), rec.ts_ms));
        }
    }
    assert_eq!(replayed, r.log);
    // And the records survive a trip through JSON, as the ledger stores them.
    let text: Vec<String> = r
        .log
        .iter()
        .map(|x| serde_json::to_string(x).unwrap())
        .collect();
    let back: Vec<FleetRecord> = text
        .iter()
        .map(|t| serde_json::from_str(t).unwrap())
        .collect();
    assert_eq!(back, r.log);
}

// --- Tasks (F5a) ---------------------------------------------------------------------

fn add(by: Actor, title: &str, deps: &[&str], role: Option<&str>) -> FleetEvent {
    FleetEvent::TaskAdd {
        by,
        title: title.into(),
        body: String::new(),
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
        scope: vec![],
        role: role.map(String::from),
        check: None,
    }
}

fn done(slot_id: &str, task: &str) -> FleetEvent {
    FleetEvent::TaskDone {
        by: slot(slot_id),
        task: task.into(),
        summary: "ok".into(),
    }
}

fn answer(cmds: &[Command]) -> String {
    cmds.iter()
        .find_map(|c| match c {
            Command::Answer { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Which slot each delivered task went to.
fn tasks_to(cmds: &[Command]) -> Vec<(String, String)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Deliver {
                to: Address::Slot { id },
                kind: MsgKind::Task,
                text,
                ..
            } => Some((
                id.clone(),
                text.split(':')
                    .next()?
                    .trim_start_matches("Task ")
                    .to_string(),
            )),
            _ => None,
        })
        .collect()
}

fn task_state(r: &Sim, id: &str) -> TaskState {
    r.m.tasks().find(|t| t.id == id).map(|t| t.state).unwrap()
}

/// The acceptance case: the lead adds three tasks, one depending on another; oneshot
/// workers do them, and the dependent one starts only after its dependency is done.
#[test]
fn lead_tasks_go_to_fresh_workers_in_dependency_order() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"sessions": 3}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let lead = slot("lead-1");
    let c = r.ev(add(lead.clone(), "A", &[], None));
    assert_eq!(answer(&c), "Added t-1, for worker.");
    assert_eq!(
        spawned(&c),
        ["worker-1"],
        "a fresh oneshot for a ready task"
    );
    let c2 = r.ev(add(lead.clone(), "B", &["t-1"], None));
    assert!(spawned(&c2).is_empty(), "B waits for A");
    let c3 = r.ev(add(lead.clone(), "C", &[], Some("worker")));
    assert_eq!(spawned(&c3), ["worker-2"]);
    let mut all = c.clone();
    all.extend(c3.clone());
    let started: Vec<Command> = all;
    let mut delivered = Vec::new();
    for c in &started {
        if let Command::Spawn { slot, .. } = c {
            delivered.extend(tasks_to(&r.ev(FleetEvent::Started {
                slot: slot.clone(),
                session: format!("s-{slot}"),
            })));
        }
    }
    assert_eq!(
        delivered,
        [
            ("worker-1".into(), "t-1".into()),
            ("worker-2".into(), "t-3".into())
        ]
    );
    assert_eq!(task_state(&r, "t-2"), TaskState::Queued);
    // A is done: the lead hears it, and B goes to a fresh worker (the limit allows it,
    // worker-1's session is still finishing its turn).
    let c = r.ev(done("worker-1", "t-1"));
    assert_eq!(answer(&c), "t-1 is done.");
    assert!(
        c.iter().any(|c| matches!(c, Command::Deliver { to: Address::Slot { id }, kind: MsgKind::Report, text, .. } if id == "lead-1" && text.contains("t-1 is done (worker-1): ok"))),
        "{c:?}"
    );
    assert!(
        spawned(&c).is_empty(),
        "three sessions run: B waits for room"
    );
    // worker-1 ends its turn: a oneshot that had its task gets no other.
    let c = r.ev(FleetEvent::Activity {
        slot: "worker-1".into(),
        turn_ended: true,
    });
    assert!(tasks_to(&c).is_empty(), "{c:?}");
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(0),
    });
    assert_eq!(spawned(&c), ["worker-3"]);
    r.start_all(&c);
    r.ev(done("worker-2", "t-3"));
    r.ev(done("worker-3", "t-2"));
    for t in ["t-1", "t-2", "t-3"] {
        assert_eq!(task_state(&r, t), TaskState::Done, "{t}");
    }
}

#[test]
fn a_task_from_a_working_session_is_a_part_and_the_whole_waits_for_it() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(Actor::User, "Ship it", &[], Some("lead")));
    assert_eq!(
        tasks_to(&c),
        [("lead-1".into(), "t-1".into())],
        "to the idle lead"
    );
    let c = r.ev(add(slot("lead-1"), "part one", &[], None));
    assert!(answer(&c).starts_with("Added t-1.1"), "{c:?}");
    let c = r.ev(done("lead-1", "t-1"));
    assert!(
        refused(&c).is_some_and(|t| t.contains("t-1.1, a part of t-1, is not done")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::Started {
        slot: "worker-1".into(),
        session: "s".into(),
    });
    assert_eq!(tasks_to(&c), [("worker-1".into(), "t-1.1".into())]);
    r.ev(done("worker-1", "t-1.1"));
    let c = r.ev(done("lead-1", "t-1"));
    assert_eq!(answer(&c), "t-1 is done.");
    assert!(
        c.iter().any(
            |c| matches!(c, Command::Notify { text } if text.contains("t-1 is done (lead-1)"))
        ),
        "the user created it: {c:?}"
    );
}

#[test]
fn open_tasks_are_claimed_or_assigned() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "specialists", "limits": {"sessions": 4}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let lead = slot("lead-1");
    // Two roles may take tasks from the lead: the task stays open.
    let c = r.ev(add(lead.clone(), "write tests", &[], None));
    assert!(answer(&c).contains("tend task assign t-1"), "{c:?}");
    r.ev(add(lead.clone(), "implement", &[], None));
    // Assigning starts a session of that role.
    let c = r.ev(FleetEvent::TaskAssign {
        by: lead.clone(),
        task: "t-2".into(),
        to: "implementer".into(),
    });
    assert_eq!(answer(&c), "t-2 is for implementer.");
    assert_eq!(spawned(&c), ["implementer-1"]);
    // A tester session started some other way claims the open one.
    let c = r.ev(FleetEvent::SpawnRequest {
        by: lead.clone(),
        role: "tester".into(),
    });
    r.start_all(&c);
    let c = r.ev(FleetEvent::TaskClaim {
        by: slot("tester-1"),
        task: None,
    });
    let text = answer(&c);
    assert!(
        text.starts_with("Task t-1: write tests") && text.contains("tend task done t-1"),
        "{text}"
    );
    assert_eq!(task_state(&r, "t-1"), TaskState::Working);
    let c = r.ev(FleetEvent::TaskClaim {
        by: slot("tester-1"),
        task: None,
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("already has t-1")),
        "{c:?}"
    );
    // Rules: workers may not add or assign; only the task's session ends it.
    let c = r.ev(add(slot("tester-1"), "x", &[], None));
    assert!(
        refused(&c).is_some_and(|t| t.contains("may not add tasks")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::TaskAssign {
        by: slot("tester-1"),
        task: "t-2".into(),
        to: "tester".into(),
    });
    assert!(refused(&c).is_some(), "{c:?}");
    let c = r.ev(done("implementer-1", "t-1"));
    assert!(
        refused(&c).is_some_and(|t| t.contains("not this session's task")),
        "{c:?}"
    );
    let c = r.ev(add(lead.clone(), "x", &["t-9"], None));
    assert!(
        refused(&c).is_some_and(|t| t.contains("no task t-9")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::TaskAssign {
        by: lead,
        task: "t-1".into(),
        to: "implementer".into(),
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("being worked on")),
        "{c:?}"
    );
}

#[test]
fn a_session_that_ends_without_done_costs_an_attempt_then_the_task_fails() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"worker": {"budget": {"attempts": 2}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "flaky", &[], None));
    r.start_all(&c);
    let t = r.now;
    let c = r.at(
        t,
        FleetEvent::Exited {
            slot: "worker-1".into(),
            code: Some(0),
        },
    );
    assert!(
        matches!(&c[..], [Command::TaskRetry { task, at_ms }] if task == "t-1" && *at_ms == t + BACKOFF_MS),
        "{c:?}"
    );
    assert!(spawned(&r.at(t + BACKOFF_MS - 1, FleetEvent::Tick)).is_empty());
    let c = r.at(t + BACKOFF_MS, FleetEvent::Tick);
    assert_eq!(spawned(&c), ["worker-2"], "a fresh session");
    let c2 = r.ev(FleetEvent::Started {
        slot: "worker-2".into(),
        session: "s".into(),
    });
    let text = c2.iter().find_map(|c| match c {
        Command::Deliver {
            kind: MsgKind::Task,
            text,
            ..
        } => Some(text.clone()),
        _ => None,
    });
    assert!(text.is_some_and(|t| t.contains("attempt 2")), "{c2:?}");
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-2".into(),
        code: None,
    });
    assert_eq!(task_state(&r, "t-1"), TaskState::Failed);
    assert!(
        c.iter().any(|c| matches!(c, Command::Deliver { to: Address::Slot { id }, text, .. } if id == "lead-1" && text.contains("t-1 failed"))),
        "{c:?}"
    );
}

#[test]
fn blocked_tasks_go_back_to_their_creator_as_a_question() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "migrate", &[], None));
    r.start_all(&c);
    let c = r.ev(FleetEvent::TaskBlocked {
        by: slot("worker-1"),
        task: "t-1".into(),
        reason: "the schema file is missing".into(),
    });
    assert!(
        c.iter().any(|c| matches!(c, Command::Deliver { to: Address::Slot { id }, kind: MsgKind::Question, text, .. } if id == "lead-1" && text.contains("schema file is missing"))),
        "{c:?}"
    );
    assert_eq!(task_state(&r, "t-1"), TaskState::Blocked);
    // Its session ending now is not a failed attempt.
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(0),
    });
    assert!(c.is_empty(), "{c:?}");
}

#[test]
fn stream_roles_take_tasks_when_idle() {
    let mut r = Sim::new(preset(Preset::Peers));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(Actor::User, "one", &[], None));
    let c2 = r.ev(add(Actor::User, "two", &[], None));
    let c3 = r.ev(add(Actor::User, "three", &[], None));
    assert_eq!(tasks_to(&c), [("peer-1".into(), "t-1".into())]);
    assert_eq!(tasks_to(&c2), [("peer-2".into(), "t-2".into())]);
    assert!(tasks_to(&c3).is_empty(), "both peers are busy");
    // peer-1 finishes and ends its turn: it takes the next one.
    r.ev(done("peer-1", "t-1"));
    let c = r.ev(FleetEvent::Activity {
        slot: "peer-1".into(),
        turn_ended: true,
    });
    assert_eq!(tasks_to(&c), [("peer-1".into(), "t-3".into())]);
}

#[test]
fn replay_with_tasks_is_deterministic() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"sessions": 3}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for (title, deps) in [("A", vec![]), ("B", vec!["t-1"]), ("C", vec![])] {
        let c = r.ev(add(slot("lead-1"), title, &deps, None));
        r.start_all(&c);
    }
    r.ev(done("worker-1", "t-1"));
    let c = r.at(
        9_000,
        FleetEvent::Exited {
            slot: "worker-1".into(),
            code: Some(0),
        },
    );
    r.start_all(&c);
    r.at(
        10_000,
        FleetEvent::Exited {
            slot: "worker-2".into(),
            code: Some(1),
        },
    );
    r.at(20_000, FleetEvent::Tick);
    let mut again = Model::new(r.m.spec().clone());
    let mut replayed = Vec::new();
    for rec in &r.log {
        if let FleetBody::Event { event } = &rec.body {
            replayed.extend(again.step(event.clone(), rec.ts_ms));
        }
    }
    assert_eq!(replayed, r.log);
}

// --- The check gate (F5b) -----------------------------------------------------------

fn checked(task: &str, passed: bool, excerpt: &str, flags: &[&str]) -> FleetEvent {
    FleetEvent::CheckResult {
        task: task.into(),
        passed,
        exit: Some(if passed { 0 } else { 1 }),
        excerpt: excerpt.into(),
        flags: flags.iter().map(|f| f.to_string()).collect(),
        diff_lines: None,
    }
}

fn runs_check(cmds: &[Command]) -> Option<(String, String)> {
    cmds.iter().find_map(|c| match c {
        Command::RunCheck { slot, command, .. } => Some((slot.clone(), command.clone())),
        _ => None,
    })
}

fn released(cmds: &[Command]) -> Vec<&str> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Release { slot } => Some(slot.as_str()),
            _ => None,
        })
        .collect()
}

fn turn_end(slot: &str) -> FleetEvent {
    FleetEvent::Activity {
        slot: slot.into(),
        turn_ended: true,
    }
}

/// A lead with a fleet check, and one worker on t-1.
fn gated() -> Sim {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "check": "make check",
        "role": {"worker": {"budget": {"attempts": 3}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "fix the parser", &[], None));
    r.start_all(&c);
    r
}

fn told_lead(cmds: &[Command], what: &str) -> bool {
    cmds.iter().any(|c| matches!(c, Command::Deliver { to: Address::Slot { id }, text, .. } if id == "lead-1" && text.contains(what)))
}

#[test]
fn done_counts_once_the_check_passes() {
    let mut r = gated();
    let c = r.ev(done("worker-1", "t-1"));
    assert_eq!(
        runs_check(&c),
        Some(("worker-1".into(), "make check".into()))
    );
    assert!(
        answer(&c).contains("tend runs its check (`make check`)"),
        "{c:?}"
    );
    assert_eq!(task_state(&r, "t-1"), TaskState::Checking);
    assert!(!told_lead(&c, "is done"), "not yet");
    // The oneshot ends its turn: it stays open while its task is checked.
    assert!(released(&r.ev(turn_end("worker-1"))).is_empty());
    let c = r.ev(checked("t-1", true, "", &[]));
    assert_eq!(task_state(&r, "t-1"), TaskState::Done);
    assert!(
        told_lead(&c, "t-1 is done (worker-1), and `make check` passed: ok"),
        "{c:?}"
    );
    assert_eq!(released(&c), ["worker-1"]);
}

#[test]
fn a_failed_check_goes_back_to_the_open_session() {
    let mut r = gated();
    r.ev(done("worker-1", "t-1"));
    r.ev(turn_end("worker-1"));
    let c = r.ev(checked(
        "t-1",
        false,
        "FAILED tests/test_parse.py::test_dates - AssertionError",
        &[],
    ));
    let back = tasks_to(&c);
    assert_eq!(
        back,
        [("worker-1".into(), "t-1".into())],
        "the session that made the change"
    );
    let text = c
        .iter()
        .find_map(|c| match c {
            Command::Deliver {
                kind: MsgKind::Task,
                text,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        text.contains("The check `make check` failed (exit 1)"),
        "{text}"
    );
    assert!(text.contains("test_dates - AssertionError"), "{text}");
    assert!(text.contains("tend task done t-1"), "{text}");
    assert_eq!(task_state(&r, "t-1"), TaskState::Working);
    assert!(released(&c).is_empty());
    // It fixes it; the check runs again and passes.
    let c = r.ev(done("worker-1", "t-1"));
    assert!(runs_check(&c).is_some());
    r.ev(checked("t-1", true, "", &[]));
    assert_eq!(task_state(&r, "t-1"), TaskState::Done);
    assert_eq!(r.m.tasks().next().unwrap().attempts, 1);
}

#[test]
fn a_task_that_fails_after_failed_checks_says_so() {
    // Live: the worker said done twice, the check failed twice, then its third turn
    // ended without done. The user should hear about the check, not only the end.
    let mut r = gated();
    for _ in 0..2 {
        r.ev(done("worker-1", "t-1"));
        r.ev(checked("t-1", false, "FAILED tests/test_parse.py::x", &[]));
    }
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(0),
    });
    let said: Vec<String> = c
        .iter()
        .filter_map(|c| match c {
            Command::Notify { text } => Some(text.clone()),
            Command::Deliver { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        said.iter().any(
            |t| t.contains("t-1 failed: its session ended without `tend task done`")
                && t.contains("Before that, its check `make check` failed (exit 1).")
        ),
        "{said:?}"
    );
}

#[test]
fn a_failed_check_after_the_session_ended_goes_to_a_fresh_one() {
    let mut r = gated();
    r.ev(done("worker-1", "t-1"));
    // The session exits while its check runs (it crashed): no attempt is lost for that.
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: None,
    });
    assert!(
        c.iter().all(|c| !matches!(c, Command::TaskRetry { .. })),
        "{c:?}"
    );
    assert_eq!(task_state(&r, "t-1"), TaskState::Checking);
    let t = r.now;
    let c = r.at(
        t,
        checked("t-1", false, "error[E0308]: mismatched types", &[]),
    );
    assert!(
        matches!(&c[..], [Command::TaskRetry { at_ms, .. }] if *at_ms == t + BACKOFF_MS),
        "{c:?}"
    );
    let c = r.at(t + BACKOFF_MS, FleetEvent::Tick);
    assert_eq!(spawned(&c), ["worker-2"]);
    let c = r.ev(FleetEvent::Started {
        slot: "worker-2".into(),
        session: "s".into(),
    });
    let text = c
        .iter()
        .find_map(|c| match c {
            Command::Deliver {
                kind: MsgKind::Task,
                text,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        text.contains("Attempt 2 of this task") && text.contains("mismatched types"),
        "{text}"
    );
}

#[test]
fn failing_checks_use_up_the_attempts() {
    let mut r = gated();
    for n in 1..=3 {
        r.ev(done("worker-1", "t-1"));
        let c = r.ev(checked("t-1", false, "boom", &[]));
        if n < 3 {
            assert_eq!(tasks_to(&c).len(), 1, "attempt {n}");
        } else {
            assert_eq!(task_state(&r, "t-1"), TaskState::Failed);
            assert!(told_lead(&c, "t-1 failed after 3 attempts"), "{c:?}");
        }
    }
}

#[test]
fn a_flagged_change_waits_for_its_creator() {
    let mut r = gated();
    r.ev(done("worker-1", "t-1"));
    r.ev(turn_end("worker-1"));
    let c = r.ev(checked(
        "t-1",
        true,
        "",
        &["tests/test_api.py: tests removed"],
    ));
    assert_eq!(task_state(&r, "t-1"), TaskState::Review);
    assert!(
        told_lead(
            &c,
            "audit (experimental) flags its diff: tests/test_api.py: tests removed"
        ),
        "{c:?}"
    );
    assert_eq!(
        released(&c),
        ["worker-1"],
        "the worker is done; the lead decides"
    );
    // Only the creator (or the user) reviews it.
    let c = r.ev(FleetEvent::TaskDone {
        by: slot("worker-1"),
        task: "t-1".into(),
        summary: "x".into(),
    });
    assert!(refused(&c).is_some(), "{c:?}");
    let c = r.ev(FleetEvent::TaskReject {
        by: slot("lead-1"),
        task: "t-1".into(),
        reason: "keep the test".into(),
    });
    assert_eq!(answer(&c), "t-1 goes back to work.");
    assert_eq!(task_state(&r, "t-1"), TaskState::Queued);
    // The second time, the lead accepts it.
    let mut r = gated();
    r.ev(done("worker-1", "t-1"));
    r.ev(checked("t-1", true, "", &["flag"]));
    let c = r.ev(FleetEvent::TaskDone {
        by: slot("lead-1"),
        task: "t-1".into(),
        summary: "fine: the test was a duplicate".into(),
    });
    assert_eq!(answer(&c), "t-1 is done.");
    assert_eq!(task_state(&r, "t-1"), TaskState::Done);
}

#[test]
fn a_tasks_own_check_wins_and_the_user_may_mark_done() {
    let mut r = gated();
    let c = r.ev(FleetEvent::TaskAdd {
        by: slot("lead-1"),
        title: "docs".into(),
        body: String::new(),
        depends_on: vec![],
        scope: vec![],
        role: None,
        check: Some("mdlint docs/".into()),
    });
    r.start_all(&c);
    let c = r.ev(done("worker-2", "t-2"));
    assert_eq!(runs_check(&c).map(|x| x.1).as_deref(), Some("mdlint docs/"));
    // The user's own `done` skips the gate: the user decides.
    let c = r.ev(FleetEvent::TaskDone {
        by: Actor::User,
        task: "t-1".into(),
        summary: "I checked it".into(),
    });
    assert!(runs_check(&c).is_none());
    assert_eq!(task_state(&r, "t-1"), TaskState::Done);
}

#[test]
fn a_oneshot_ends_after_its_turn_when_its_task_is_over_or_left_open() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "a", &[], None));
    r.start_all(&c);
    r.ev(done("worker-1", "t-1"));
    assert_eq!(released(&r.ev(turn_end("worker-1"))), ["worker-1"]);
    // One that ends its turn without `done` is ended too; its exit costs an attempt.
    let c = r.ev(add(slot("lead-1"), "b", &[], None));
    r.start_all(&c);
    assert_eq!(released(&r.ev(turn_end("worker-2"))), ["worker-2"]);
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-2".into(),
        code: Some(0),
    });
    assert!(matches!(&c[..], [Command::TaskRetry { .. }]), "{c:?}");
}

// --- Budgets (F5d) ------------------------------------------------------------------

fn usage(slot_id: &str, tokens: u64, cost: f64) -> FleetEvent {
    FleetEvent::Usage {
        slot: slot_id.into(),
        tokens,
        cost_usd: cost,
    }
}

fn notified(cmds: &[Command], what: &str) -> bool {
    cmds.iter()
        .any(|c| matches!(c, Command::Notify { text } if text.contains(what)))
}

#[test]
fn a_session_over_its_token_budget_is_stopped_and_its_task_retried() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"worker": {"budget": {"tokens": "1k"}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "big", &[], None));
    r.start_all(&c);
    assert!(r.ev(usage("worker-1", 600, 0.01)).is_empty());
    let c = r.ev(usage("worker-1", 600, 0.02));
    assert!(
        matches!(&c[0], Command::Stop { slot } if slot == "worker-1"),
        "{c:?}"
    );
    assert!(
        notified(&c, "used 1200 tokens, over its budget of 1000"),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: None,
    });
    assert!(
        c.iter().any(|c| matches!(c, Command::TaskRetry { .. })),
        "{c:?}"
    );
    assert_eq!(r.m.spend(), (1200, 0.02));
}

#[test]
fn the_fleet_pauses_at_its_token_limit_and_resumes() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "lead", "limits": {"tokens": 5000}}),
    ));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(usage("lead-1", 6000, 0.1));
    assert!(notified(&c, "over its limit of 5000"), "{c:?}");
    assert!(r.m.paused());
    // Paused: a new task waits, and nothing starts.
    let c = r.ev(add(slot("lead-1"), "later", &[], None));
    assert!(spawned(&c).is_empty(), "{c:?}");
    let c = r.ev(FleetEvent::Resume);
    assert!(notified(&c, "resumed"));
    assert_eq!(spawned(&c), ["worker-1"]);
}

#[test]
fn the_plans_usage_limit_pauses_the_fleet() {
    let mut r = Sim::new(preset(Preset::Peers));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(FleetEvent::UsageLimit {
        slot: "peer-1".into(),
        text: "5-hour limit reached ∙ resets 3pm".into(),
    });
    assert!(notified(&c, "hit the plan's usage limit"), "{c:?}");
    let c = r.ev(add(Actor::User, "x", &[], None));
    assert!(tasks_to(&c).is_empty(), "no work while paused: {c:?}");
    // Pausing twice says nothing more.
    assert!(r.ev(FleetEvent::Pause).is_empty());
}

#[test]
fn a_session_past_its_wall_time_is_stopped() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"lead": {"budget": {"wall": "10m"}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let t = r.now;
    assert!(r.at(t + 9 * 60_000, FleetEvent::Tick).is_empty());
    let c = r.at(t + 11 * 60_000, FleetEvent::Tick);
    assert!(
        matches!(&c[0], Command::Stop { slot } if slot == "lead-1"),
        "{c:?}"
    );
    assert!(notified(&c, "wall-time budget of 10 min"));
    // Stopped for its budget, it is not restarted.
    let c = r.ev(FleetEvent::Exited {
        slot: "lead-1".into(),
        code: None,
    });
    assert!(
        spawned(&c).is_empty() && !c.iter().any(|c| matches!(c, Command::Backoff { .. })),
        "{c:?}"
    );
}

// --- Attached sessions and messages (F4) ---------------------------------------------

#[test]
fn a_terminal_session_joins_as_an_attached_role_and_leaves_for_good() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"reviewer": {"run": "attached"}}
    })));
    let up = r.ev(FleetEvent::Up);
    assert!(up
        .iter()
        .any(|c| matches!(c, Command::Attach { role } if role == "reviewer")));
    r.start_all(&up);
    let c = r.ev(FleetEvent::Attached {
        role: "reviewer".into(),
        name: Some("rev".into()),
        session: "pid-9".into(),
    });
    assert_eq!(answer(&c), "rev");
    assert_eq!(r.state("rev"), SlotState::Running);
    // Added to the lead preset, it is a member: it asks the lead, the lead answers.
    let to_lead = Address::Role {
        name: "lead".into(),
    };
    assert_eq!(
        r.m.default_kind(&slot("rev"), &to_lead),
        Some(MsgKind::Question)
    );
    assert_eq!(
        r.m.default_kind(&slot("lead-1"), &Address::Slot { id: "rev".into() }),
        Some(MsgKind::Answer)
    );
    let c = r.ev(msg(slot("rev"), to_lead, MsgKind::Question));
    assert_eq!(delivered(&c).first().map(|d| d.0.as_str()), Some("lead-1"));
    // A second reviewer, a stream role, or a taken name: refused.
    let c = r.ev(FleetEvent::Attached {
        role: "reviewer".into(),
        name: None,
        session: "x".into(),
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("already has its 1 session")),
        "{c:?}"
    );
    let c = r.ev(FleetEvent::Attached {
        role: "lead".into(),
        name: None,
        session: "x".into(),
    });
    assert!(
        refused(&c).is_some_and(|t| t.contains("not attached")),
        "{c:?}"
    );
    // The user leaves: no restart.
    let c = r.ev(FleetEvent::Exited {
        slot: "rev".into(),
        code: Some(0),
    });
    assert!(c.is_empty(), "{c:?}");
    assert_eq!(r.state("rev"), SlotState::Stopped);
    // And may come back under the same name.
    let c = r.ev(FleetEvent::Attached {
        role: "reviewer".into(),
        name: Some("rev".into()),
        session: "pid-10".into(),
    });
    assert_eq!(answer(&c), "rev");
}

#[test]
fn a_reply_to_a_question_is_an_answer() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let lead = Address::Role {
        name: "lead".into(),
    };
    assert_eq!(
        r.m.default_kind(&Actor::User, &lead),
        Some(MsgKind::Question)
    );
    r.ev(msg(Actor::User, lead, MsgKind::Question));
    assert_eq!(
        r.m.default_kind(&slot("lead-1"), &Address::User),
        Some(MsgKind::Answer)
    );
}

// --- Hub restart (G1) ------------------------------------------------------------------

#[test]
fn after_a_hub_restart_stream_slots_come_back_and_tasks_are_not_charged() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "check": "make check", "role": {"reviewer": {"run": "attached"}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    r.ev(FleetEvent::Attached {
        role: "reviewer".into(),
        name: None,
        session: "p".into(),
    });
    // A worker on t-1, and t-2 being checked.
    let c = r.ev(add(slot("lead-1"), "one", &[], Some("worker")));
    r.start_all(&c);
    let c = r.ev(add(slot("lead-1"), "two", &[], Some("worker")));
    r.start_all(&c);
    r.ev(done("worker-2", "t-2"));
    let c = r.ev(FleetEvent::HubRestarted);
    // The check runs again; the lead starts again (no crash); the worker's task is queued.
    assert!(
        c.iter()
            .any(|c| matches!(c, Command::RunCheck { task, .. } if task == "t-2")),
        "{c:?}"
    );
    assert!(spawned(&c).contains(&"lead-1"), "{c:?}");
    assert_eq!(r.m.slots()["lead-1"].restarts, 0);
    assert_eq!(r.state("reviewer-1"), SlotState::Stopped);
    assert_eq!(r.state("worker-1"), SlotState::Stopped);
    let t1 = r.m.tasks().find(|t| t.id == "t-1").unwrap();
    assert_eq!(t1.attempts, 0);
    assert!(
        spawned(&c).iter().any(|s| s.starts_with("worker-")),
        "t-1 gets a fresh worker: {c:?}"
    );
    assert!(c
        .iter()
        .any(|c| matches!(c, Command::Notify { text } if text.contains("hub restarted"))));
}

#[test]
fn a_worker_that_asks_waits_for_the_answer() {
    let mut r = Sim::new(preset(Preset::Lead));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "migrate", &[], None));
    r.start_all(&c);
    r.ev(msg(
        slot("worker-1"),
        Address::Role {
            name: "lead".into(),
        },
        MsgKind::Question,
    ));
    // Its turn ends: it waits, rather than being released.
    assert!(released(&r.ev(turn_end("worker-1"))).is_empty());
    let c = r.ev(msg(
        slot("lead-1"),
        Address::Slot {
            id: "worker-1".into(),
        },
        MsgKind::Answer,
    ));
    assert_eq!(
        delivered(&c).first().map(|d| d.0.as_str()),
        Some("worker-1")
    );
    // With the answer it goes on; its next turn end, without `done`, releases it.
    assert_eq!(released(&r.ev(turn_end("worker-1"))), ["worker-1"]);
    // One that never gets an answer ends after the wait.
    let c = r.ev(add(slot("lead-1"), "other", &[], None));
    r.start_all(&c);
    r.ev(msg(
        slot("worker-2"),
        Address::Role {
            name: "lead".into(),
        },
        MsgKind::Question,
    ));
    r.ev(turn_end("worker-2"));
    let t = r.now;
    assert!(released(&r.at(t + QUESTION_WAIT_MS - 1, FleetEvent::Tick)).is_empty());
    assert_eq!(
        released(&r.at(t + QUESTION_WAIT_MS + 1, FleetEvent::Tick)),
        ["worker-2"]
    );
}

#[test]
fn a_team_channel_reaches_the_others_in_the_shared_worktree() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead",
        "role": {"worker": {"workspace": "team:api", "replicas": "0..3"}, "docs": {}},
        "limits": {"sessions": 5}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for _ in 0..3 {
        let c = r.ev(add(slot("lead-1"), "part", &[], Some("worker")));
        r.start_all(&c);
    }
    let team = Address::Team { name: "api".into() };
    assert_eq!(
        r.m.default_kind(&slot("worker-1"), &team),
        Some(MsgKind::Notice)
    );
    let c = r.ev(msg(slot("worker-1"), team.clone(), MsgKind::Notice));
    let to: Vec<String> = delivered(&c).into_iter().map(|(t, _)| t).collect();
    assert_eq!(to, ["worker-2", "worker-3"], "the others, not itself");
    // The lead is not in the team's worktree; a task is not team talk.
    let c = r.ev(msg(slot("lead-1"), team.clone(), MsgKind::Notice));
    assert!(
        refused(&c).is_some_and(|t| t.contains("does not work in team api")),
        "{c:?}"
    );
    let c = r.ev(msg(slot("worker-1"), team, MsgKind::Task));
    assert!(refused(&c).is_some(), "{c:?}");
}

#[test]
fn a_failure_in_a_busy_shared_worktree_is_not_charged_the_first_time() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "check": "make check",
        "role": {"worker": {"workspace": "team:api", "replicas": "0..2"}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    for t in ["a", "b"] {
        let c = r.ev(add(slot("lead-1"), t, &[], None));
        r.start_all(&c);
    }
    r.ev(done("worker-1", "t-1"));
    let c = r.ev(checked("t-1", false, "boom", &[]));
    let text = c
        .iter()
        .find_map(|c| match c {
            Command::Deliver {
                kind: MsgKind::Task,
                text,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(text.contains("may come from its change"), "{text}");
    assert_eq!(r.m.tasks().next().unwrap().attempts, 0);
    // The second failure counts.
    r.ev(done("worker-1", "t-1"));
    r.ev(checked("t-1", false, "boom", &[]));
    assert_eq!(r.m.tasks().next().unwrap().attempts, 1);
}

// --- Handoff flows and best-of (G8, G9) ----------------------------------------------

fn task<'a>(r: &'a Sim, id: &str) -> &'a crate::fleet::model::Task {
    r.m.tasks()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("no {id}"))
}

/// Start what the commands spawn and hand back the slots that got a task.
fn run_tasks(r: &mut Sim, cmds: &[Command]) -> Vec<(String, String)> {
    let mut got = tasks_to(cmds);
    for c in cmds {
        if let Command::Spawn { slot, .. } = c {
            got.extend(tasks_to(&r.ev(FleetEvent::Started {
                slot: slot.clone(),
                session: "s".into(),
            })));
        }
    }
    got
}

#[test]
fn a_pipeline_hands_each_stage_to_the_next() {
    let mut r = Sim::new(preset(Preset::Pipeline));
    r.ev(FleetEvent::Up);
    let c = r.ev(add(Actor::User, "parse dates", &[], Some("plan")));
    assert_eq!(run_tasks(&mut r, &c), [("plan-1".into(), "t-1".into())]);
    let c = r.ev(done("plan-1", "t-1"));
    assert!(
        notified(&c, "t-1 (plan) is handed to implement as t-2"),
        "{c:?}"
    );
    assert_eq!(task(&r, "t-2").title, "parse dates — implement");
    assert!(task(&r, "t-2")
        .body
        .contains("Handed over from plan (plan-1): ok"));
    assert_eq!(
        run_tasks(&mut r, &c),
        [("implement-1".into(), "t-2".into())]
    );
    let c = r.ev(done("implement-1", "t-2"));
    assert_eq!(task(&r, "t-3").title, "parse dates — verify");
    run_tasks(&mut r, &c);
    let c = r.ev(done("verify-1", "t-3"));
    assert!(
        !c.iter()
            .any(|c| matches!(c, Command::Notify { text } if text.contains("handed"))),
        "the last stage hands nothing: {c:?}"
    );
    assert_eq!(
        task(&r, "t-3").created_by,
        Actor::User,
        "outcomes reach whoever asked"
    );
}

#[test]
fn a_review_sends_work_back_until_max_rounds() {
    let mut r = Sim::new(spec(
        serde_json::json!({"preset": "review", "max_rounds": 1}),
    ));
    r.ev(FleetEvent::Up);
    let c = r.ev(add(Actor::User, "add caching", &[], Some("author")));
    run_tasks(&mut r, &c);
    let c = r.ev(done("author-1", "t-1"));
    assert_eq!(run_tasks(&mut r, &c), [("reviewer-1".into(), "t-2".into())]);
    let c = r.ev(FleetEvent::TaskBlocked {
        by: slot("reviewer-1"),
        task: "t-2".into(),
        reason: "the cache never expires".into(),
    });
    assert!(
        notified(&c, "reviewer sent t-1 back; author gets t-3"),
        "{c:?}"
    );
    assert!(task(&r, "t-3").body.contains("the cache never expires"));
    assert_eq!(task(&r, "t-3").title, "add caching — author, round 2");
    let got = run_tasks(&mut r, &c);
    assert_eq!(got, [("author-2".into(), "t-3".into())]);
    let c = r.ev(done("author-2", "t-3"));
    run_tasks(&mut r, &c);
    let c = r.ev(FleetEvent::TaskBlocked {
        by: slot("reviewer-2"),
        task: "t-4".into(),
        reason: "still wrong".into(),
    });
    assert!(notified(&c, "sent back 1 times; it stays blocked"), "{c:?}");
}

fn best_of(judge: &str) -> Sim {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "best-of", "judge": judge, "check": "make check", "limits": {"sessions": 4}
    })));
    r.ev(FleetEvent::Up);
    let c = r.ev(add(Actor::User, "speed up dedup", &[], Some("candidate")));
    assert!(
        notified(&c, "t-1 goes to 3 candidates (t-1.1, t-1.2, t-1.3)"),
        "{c:?}"
    );
    assert_eq!(run_tasks(&mut r, &c).len(), 3);
    r
}

fn finish_part(
    r: &mut Sim,
    slot_id: &str,
    task_id: &str,
    passed: bool,
    lines: u64,
) -> Vec<Command> {
    r.ev(done(slot_id, task_id));
    r.ev(FleetEvent::CheckResult {
        task: task_id.into(),
        passed,
        exit: Some(if passed { 0 } else { 1 }),
        excerpt: String::new(),
        flags: vec![],
        diff_lines: Some(lines),
    })
}

#[test]
fn best_of_by_rule_picks_the_smallest_passing_change() {
    let mut r = best_of("rule");
    finish_part(&mut r, "candidate-1", "t-1.1", true, 40);
    finish_part(&mut r, "candidate-2", "t-1.2", true, 12);
    // The third fails its check every time; the others still get judged.
    for _ in 0..3 {
        r.ev(done("candidate-3", "t-1.3"));
        let c = r.ev(checked("t-1.3", false, "slow", &[]));
        run_tasks(&mut r, &c);
        if task(&r, "t-1.3").state == TaskState::Failed {
            break;
        }
        let t = r.now + 60_000;
        let c = r.at(t, FleetEvent::Tick);
        run_tasks(&mut r, &c);
    }
    let t1 = task(&r, "t-1");
    assert_eq!(t1.state, TaskState::Done, "{t1:?}");
    assert!(
        t1.outcome.as_deref().unwrap().contains("picked t-1.2"),
        "{t1:?}"
    );
    assert_eq!(task(&r, "t-1.2").picked, Some(true));
    assert_eq!(task(&r, "t-1.1").picked, Some(false));
}

#[test]
fn best_of_by_agent_gives_the_judge_every_result() {
    let mut r = best_of("agent");
    for (s, t, n) in [
        ("candidate-1", "t-1.1", 5),
        ("candidate-2", "t-1.2", 9),
        ("candidate-3", "t-1.3", 7),
    ] {
        let c = finish_part(&mut r, s, t, true, n);
        if t == "t-1.3" {
            // All three ended: the judge starts with the list.
            let got = run_tasks(&mut r, &c);
            assert_eq!(got, [("judge-1".into(), "t-1".into())], "{c:?}");
        }
    }
    let text = r.m.task_text("t-1");
    assert!(
        text.contains("- t-1.2 (candidate-2): ok [9 lines changed]") && text.contains("Pick one"),
        "{text}"
    );
    r.ev(FleetEvent::TaskDone {
        by: slot("judge-1"),
        task: "t-1".into(),
        summary: "t-1.3: clearest".into(),
    });
    // The judge's own `done` passes the fleet's check too.
    r.ev(checked("t-1", true, "", &[]));
    assert_eq!(task(&r, "t-1").state, TaskState::Done);
    assert_eq!(task(&r, "t-1.3").picked, Some(true));
    assert_eq!(task(&r, "t-1.1").picked, Some(false));
}

#[test]
fn best_of_by_user_asks_the_user() {
    let mut r = best_of("user");
    finish_part(&mut r, "candidate-1", "t-1.1", true, 5);
    finish_part(&mut r, "candidate-2", "t-1.2", true, 9);
    let c = finish_part(&mut r, "candidate-3", "t-1.3", true, 7);
    assert!(notified(&c, "t-1's candidates are done"), "{c:?}");
    assert_eq!(task(&r, "t-1").state, TaskState::Review);
    r.ev(FleetEvent::TaskDone {
        by: Actor::User,
        task: "t-1".into(),
        summary: "t-1.1, the smallest".into(),
    });
    assert_eq!(task(&r, "t-1.1").picked, Some(true));
}

// --- Review fixes ----------------------------------------------------------------------

#[test]
fn a_terminal_session_is_never_taken_for_stuck_or_over_budget() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "stuck_minutes": 1,
        "role": {"reviewer": {"run": "attached", "budget": {"wall": "5m"}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    r.ev(FleetEvent::Attached {
        role: "reviewer".into(),
        name: Some("rev".into()),
        session: "p".into(),
    });
    r.ev(add(slot("lead-1"), "look", &[], Some("reviewer")));
    for m in [2, 4, 8, 30] {
        let c = r.at(m * 60_000, FleetEvent::Tick);
        assert!(
            !c.iter().any(|c| matches!(c, Command::Stop { slot } | Command::Interrupt { slot } if slot == "rev")),
            "{m} min: {c:?}"
        );
    }
}

#[test]
fn tasks_that_depend_on_a_failed_one_are_blocked_not_left_waiting() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"worker": {"budget": {"attempts": 1}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "a", &[], None));
    r.start_all(&c);
    r.ev(add(slot("lead-1"), "b", &["t-1"], None));
    r.ev(add(slot("lead-1"), "c", &["t-2"], None));
    let c = r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: Some(1),
    });
    assert_eq!(task_state(&r, "t-1"), TaskState::Failed);
    assert_eq!(task_state(&r, "t-2"), TaskState::Blocked);
    assert_eq!(
        task_state(&r, "t-3"),
        TaskState::Blocked,
        "and what waits on it"
    );
    assert!(
        told_lead(&c, "t-2 is blocked: it depends on t-1, which failed"),
        "{c:?}"
    );
}

// --- Review fixes, second pass ----------------------------------------------------------

#[test]
fn a_leader_that_restarts_keeps_its_team() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "limits": {"depth": 2, "sessions": 6},
        "role": {"api": {"run": "stream", "replicas": 1,
                 "team": {"preset": "lead", "role": {"db": {"run": "stream", "replicas": 1}}}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    assert_eq!(r.state("api-1/db-1"), SlotState::Running);
    let t = r.now;
    r.at(
        t,
        FleetEvent::Exited {
            slot: "api-1".into(),
            code: Some(1),
        },
    );
    let c = r.at(t + BACKOFF_MS, FleetEvent::Tick);
    r.start_all(&c);
    assert!(
        !r.m.slots().contains_key("api-1/db-2"),
        "no second db: {:?}",
        r.m.slots().keys().collect::<Vec<_>>()
    );
    // After a hub restart too.
    let c = r.ev(FleetEvent::HubRestarted);
    r.start_all(&c);
    assert!(!r.m.slots().contains_key("api-1/db-2"));
    assert_eq!(r.state("api-1/db-1"), SlotState::Running);
}

#[test]
fn a_blocked_candidate_does_not_hold_up_the_judging() {
    for judge in ["rule", "agent"] {
        let mut r = best_of(judge);
        finish_part(&mut r, "candidate-1", "t-1.1", true, 5);
        finish_part(&mut r, "candidate-2", "t-1.2", true, 9);
        let c = r.ev(FleetEvent::TaskBlocked {
            by: slot("candidate-3"),
            task: "t-1.3".into(),
            reason: "no idea".into(),
        });
        if judge == "rule" {
            assert_eq!(task_state(&r, "t-1"), TaskState::Done, "{c:?}");
            continue;
        }
        assert_eq!(
            run_tasks(&mut r, &c),
            [("judge-1".into(), "t-1".into())],
            "{c:?}"
        );
        let c = r.ev(FleetEvent::TaskDone {
            by: slot("judge-1"),
            task: "t-1".into(),
            summary: "t-1.2 is cleaner".into(),
        });
        assert!(runs_check(&c).is_some(), "the judge may finish: {c:?}");
        r.ev(checked("t-1", true, "", &[]));
        assert_eq!(task(&r, "t-1.2").picked, Some(true));
        assert_eq!(task(&r, "t-1.1").picked, Some(false));
    }
}

#[test]
fn a_check_whose_session_ended_runs_again_after_a_hub_restart() {
    let mut r = gated();
    r.ev(done("worker-1", "t-1"));
    r.ev(FleetEvent::Exited {
        slot: "worker-1".into(),
        code: None,
    });
    let c = r.ev(FleetEvent::HubRestarted);
    assert!(
        c.iter()
            .any(|c| matches!(c, Command::RunCheck { task, .. } if task == "t-1")),
        "{c:?}"
    );
}

// --- Bug hunt (t-1.4) ---------------------------------------------------------------

#[test]
fn an_attached_name_does_not_make_a_later_slot_overwrite_it() {
    let mut r = Sim::new(spec(serde_json::json!({
        "role": {"a": {"run": "attached"}, "b": {"run": "attached"}}
    })));
    r.ev(FleetEvent::Up);
    // `b` takes the name the next unnamed `a` would get.
    r.ev(FleetEvent::Attached {
        role: "b".into(),
        name: Some("a-1".into()),
        session: "s1".into(),
    });
    r.ev(FleetEvent::Attached {
        role: "a".into(),
        name: None,
        session: "s2".into(),
    });
    let roles: Vec<(&str, &str)> =
        r.m.slots()
            .values()
            .map(|s| (s.id.as_str(), s.role.as_str()))
            .collect();
    assert_eq!(roles.len(), 2, "{roles:?}");
    assert_eq!(r.m.slots()["a-1"].role, "b");
}

#[test]
fn a_huge_attempt_budget_does_not_overflow_the_backoff() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "role": {"worker": {"budget": {"attempts": 70}}}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let mut c = r.ev(add(slot("lead-1"), "flaky", &[], None));
    for _ in 0..90 {
        r.start_all(&c);
        let running: Vec<String> =
            r.m.slots()
                .values()
                .filter(|s| s.role == "worker" && s.state == SlotState::Running)
                .map(|s| s.id.clone())
                .collect();
        for id in running {
            r.ev(FleetEvent::Exited {
                slot: id,
                code: Some(0),
            });
        }
        r.now += 6_000_000;
        c = r.ev(FleetEvent::Tick);
    }
    assert_eq!(task_state(&r, "t-1"), TaskState::Failed);
}

#[test]
fn usage_after_a_session_ended_is_not_counted_twice() {
    let mut r = Sim::new(spec(serde_json::json!({"preset": "lead"})));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    r.ev(usage("lead-1", 10, 0.5));
    r.ev(FleetEvent::Exited {
        slot: "lead-1".into(),
        code: None,
    });
    r.ev(usage("lead-1", 5, 0.5));
    assert_eq!(r.m.spend().1, 0.5);
}

#[test]
fn a_waiting_oneshot_with_a_task_does_not_start_empty_after_a_hub_restart() {
    let mut r = Sim::new(spec(serde_json::json!({
        "preset": "lead", "limits": {"sessions": 2}
    })));
    let up = r.ev(FleetEvent::Up);
    r.start_all(&up);
    let c = r.ev(add(slot("lead-1"), "one", &[], None));
    r.start_all(&c);
    let c = r.ev(add(slot("lead-1"), "two", &[], None));
    r.start_all(&c);
    assert_eq!(r.state("worker-2"), SlotState::Waiting);
    let c = r.ev(FleetEvent::HubRestarted);
    assert!(
        !spawned(&c).contains(&"worker-2"),
        "its task went back to the queue: {c:?}"
    );
}
