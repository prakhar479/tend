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
        Command::Deliver {
            from: Actor::Tend,
            text,
            ..
        } if text.starts_with("tend: refused") => Some(text.clone()),
        Command::Notify { text } if text.starts_with("tend fleet: refused") => Some(text.clone()),
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
    // A oneshot that fails is restarted like any crash.
    let cmds = r.ev(FleetEvent::SpawnRequest {
        by: slot("lead-1"),
        role: "worker".into(),
    });
    r.start_all(&cmds);
    let cmds = r.ev(FleetEvent::Exited {
        slot: "worker-2".into(),
        code: Some(2),
    });
    assert!(matches!(&cmds[..], [Command::Backoff { .. }]), "{cmds:?}");
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
        matches!(&c[..], [Command::Deliver { to: Address::Slot { id }, text, .. }] if id == "worker-1" && text.contains("may not spawn")),
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
    // The sub-lead crashes; its waiting team members cannot start until it is back.
    r.ev(FleetEvent::Exited {
        slot: "sub-1/worker-1".into(),
        code: Some(1),
    });
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
    assert_eq!(spawned(&c2), ["sub-1/worker-1"], "back with its leader");
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
