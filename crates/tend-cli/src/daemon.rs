//! Session daemon: holds the controller, connector, and store for one session.
//! One thread per connection; one mutex around all session state.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tend_core::{Controller, Mode, Response};

use crate::connector::{self, Connector};
use crate::ipc::{Reply, Request};
use crate::paths::{now_ms, socket_path};
use crate::store::Store;

/// Time to encode and send a reply. A hook later than its deadline minus this is late.
const REPLY_MARGIN_MS: u64 = 10;
/// A client gets this long to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Larger than any hook payload or filter request (8 MiB of output, JSON-escaped).
const MAX_REQUEST_BYTES: u64 = 64 << 20;

struct State {
    controller: Controller,
    connector: Box<dyn Connector>,
    store: Store,
    /// Where this project's learned shape levels live (E9), once the session started.
    memory: Option<PathBuf>,
    /// Desktop notifications are on (`notify = true`).
    notify: bool,
    /// The session's working directory, once it started (for the fleet watcher).
    cwd: Option<String>,
    /// The fleet's claims file, and when it was last read (F5c).
    claims: Option<(PathBuf, Option<std::time::SystemTime>)>,
    /// This session's fleet inbox, and how much of it was read (F4).
    inbox: Option<(PathBuf, u64)>,
    /// This session's id and project, for the machine's learned fixes (F2).
    session_id: String,
    project: Option<String>,
}

/// When the daemon last got a request (ms), for the idle exit.
static LAST_REQUEST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn run(
    dir: PathBuf,
    parent_pid: Option<i32>,
    mode: Mode,
    agent: &str,
    idle_exit_minutes: Option<u64>,
) -> Result<()> {
    let sock = socket_path(&dir);
    // The socket accepts hook events and `tend allow`: keep it in a private dir.
    crate::paths::ensure_private_dir(&dir)?;
    if let Some(parent) = sock.parent() {
        crate::paths::ensure_private_dir(parent)?;
    }
    // Bind under a temporary name and rename: the socket appears at its path only once
    // it listens, so a client never finds a socket that refuses (bind creates the file
    // before listen).
    // The temporary name is no longer than `tendd.sock`, so it fits the path limit too.
    let tmp = sock.with_file_name(format!("{}.t", std::process::id() % 10_000_000));
    let _ = fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
    fs::rename(&tmp, &sock).with_context(|| format!("rename to {}", sock.display()))?;
    let connector = connector::for_agent(agent)?;
    let mut controller = Controller::new(crate::config::load(), connector.capabilities(), mode);
    // A fleet's hub starts its sessions with TEND_FLEET_SLOT set (F5a).
    controller.set_in_fleet(std::env::var_os("TEND_FLEET_SLOT").is_some());
    // Its role's write scope, and where the fleet's claims are (F5c).
    let rules: Option<tend_core::fleet::scope::RulesFile> = std::env::var("TEND_FLEET_RULES")
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok());
    if let Some(r) = &rules {
        controller.set_fleet_rules(tend_core::fleet::scope::Rules::new(r));
    }
    let store = Store::open(&dir)?;
    // Without a token file (a daemon started by hand), control requests are open.
    let token: Arc<Option<String>> = Arc::new(crate::paths::read_token(&dir));
    let state = Arc::new(Mutex::new(State {
        controller,
        connector,
        store,
        memory: None,
        notify: crate::config::load().notify,
        cwd: None,
        claims: rules
            .as_ref()
            .and_then(|r| r.claims.clone())
            .map(|p| (PathBuf::from(p), None)),
        inbox: rules.as_ref().and_then(|r| r.inbox.clone()).map(|p| {
            let p = PathBuf::from(p);
            let seen = inbox_seen(&p);
            (p, seen)
        }),
        session_id: dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        project: None,
    }));
    eprintln!("tendd: listening on {} (mode {mode:?})", sock.display());

    if let Some(pid) = parent_pid {
        let sock = sock.clone();
        thread::spawn(move || watch_parent(pid, &sock));
    }
    LAST_REQUEST_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    if crate::config::load().fleet != tend_core::FleetSetting::Off {
        let state = Arc::clone(&state);
        thread::spawn(move || watch_fleet(dir, &state));
    }
    if let Some(minutes) = idle_exit_minutes {
        let sock = sock.clone();
        thread::spawn(move || watch_idle(minutes, &sock));
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let sock = sock.clone();
                let token = Arc::clone(&token);
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &state, &sock, &token) {
                        eprintln!("tendd: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("tendd: accept: {e}"),
        }
    }
    Ok(())
}

/// Per-project memory (E9): at session start, take up the shape levels earlier sessions
/// in the project learned; after an adaptation, save them. Kept in
/// `<home>/projects/<hash of the repo root>.json`.
fn remember(st: &mut State, decisions: &[tend_core::Decision]) -> Result<()> {
    for e in decisions.iter().flat_map(|d| &d.entries) {
        if let tend_core::Body::Event {
            event: tend_core::Event::SessionStarted { cwd, .. },
        } = &e.body
        {
            st.cwd = Some(cwd.clone());
            let cwd = Path::new(cwd);
            let root = crate::audit::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let key = tend_core::text::sha256_hex(&root.to_string_lossy());
            let file = crate::paths::home()
                .join("projects")
                .join(format!("{}.json", &key[..16]));
            let levels: std::collections::BTreeMap<String, u8> = fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| serde_json::from_value(v.get("shape_levels")?.clone()).ok())
                .unwrap_or_default();
            let d = st.controller.restore_levels(&levels, now_ms());
            st.store.persist(&d)?;
            st.memory = Some(file);
        }
    }
    let adapted = decisions
        .iter()
        .flat_map(|d| &d.entries)
        .any(|e| matches!(e.body, tend_core::Body::Adaptation { .. }));
    if let (true, Some(file)) = (adapted, &st.memory) {
        if let Some(dir) = file.parent() {
            let _ = crate::paths::ensure_private_dir(dir);
        }
        let body = serde_json::json!({ "shape_levels": st.controller.learned_levels() });
        let _ = fs::write(file, body.to_string());
    }
    Ok(())
}

/// What deserves a desktop notification (E11): a loop (steered or not) and an edit tend
/// blocked, which waits for the user. Warnings do not: the agent sees them.
fn notifications(decisions: &[tend_core::Decision]) -> Vec<String> {
    use tend_core::{ActionKind, Body};
    let mut out = Vec::new();
    for e in decisions.iter().flat_map(|d| &d.entries) {
        if let Body::Action {
            action, applied, ..
        } = &e.body
        {
            match action {
                ActionKind::Steer { text, .. } => out.push(tend_core::text::truncate_chars(
                    text.trim_start_matches("tend: "),
                    180,
                )),
                ActionKind::Guard {
                    path,
                    decision:
                        tend_core::guard::GuardDecision::Deny | tend_core::guard::GuardDecision::Ask,
                } if *applied => out.push(format!(
                    "Blocked an edit of {path}. If it is intended, run `! tend allow`."
                )),
                _ => {}
            }
        }
    }
    out
}

/// Show notifications without waiting: `notify-send` (Linux) or `osascript` (macOS).
/// At most one a minute; a missing tool is ignored.
fn notify(messages: &[String]) {
    static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some(msg) = messages.first() else {
        return;
    };
    let now = now_ms();
    if now.saturating_sub(LAST_MS.load(std::sync::atomic::Ordering::Relaxed)) < 60_000 {
        return;
    }
    LAST_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    let mut cmd = if cfg!(target_os = "macos") {
        let quoted = msg.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = std::process::Command::new("osascript");
        c.arg("-e").arg(format!(
            "display notification \"{quoted}\" with title \"tend\""
        ));
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.arg("tend").arg(msg);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Read the fleet's claims again if the file changed (F5c). A missing or bad file
/// leaves the last ones: fail open.
fn refresh_claims(st: &mut State) {
    let Some((path, seen)) = &mut st.claims else {
        return;
    };
    let Ok(modified) = fs::metadata(&*path).and_then(|m| m.modified()) else {
        return;
    };
    if *seen == Some(modified) {
        return;
    }
    *seen = Some(modified);
    if let Ok(claims) = fs::read_to_string(&*path).map_err(|_| ()).and_then(|t| {
        serde_json::from_str::<Vec<tend_core::fleet::scope::ClaimFile>>(&t).map_err(|_| ())
    }) {
        st.controller.set_fleet_claims(&claims);
    }
}

/// New messages in this session's fleet inbox become events, so this hook's reply can
/// carry them (F4). A missing file means none yet.
fn read_inbox(st: &mut State) -> Result<()> {
    let Some((path, offset)) = &mut st.inbox else {
        return Ok(());
    };
    for v in new_inbox_lines(path, offset) {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let ev = tend_core::Event::Message {
            from: s("from"),
            kind: s("kind"),
            text: s("text"),
        };
        let d = st.controller.handle(ev, now_ms());
        st.store.persist(&d)?;
    }
    Ok(())
}

/// Where an earlier daemon of this slot stopped reading its inbox (a restart, a
/// rejoin): old messages are not delivered twice.
fn inbox_seen(path: &Path) -> u64 {
    fs::read_to_string(format!("{}.seen", path.display()))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Whole lines of the inbox past `offset`, which moves past them and is kept in
/// `<inbox>.seen`.
fn new_inbox_lines(path: &Path, offset: &mut u64) -> Vec<serde_json::Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(new) = text.get(*offset as usize..) else {
        return Vec::new();
    };
    let end = new.rfind('\n').map_or(0, |i| i + 1);
    let lines = new[..end]
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    *offset += end as u64;
    if end > 0 {
        let _ = fs::write(format!("{}.seen", path.display()), offset.to_string());
    }
    lines
}

/// How often the fleet watcher looks at the other sessions (F1).
const FLEET_POLL: Duration = Duration::from_secs(1);

/// Fleet awareness (F1): once the session has started, record its workspace, then feed
/// the other live sessions' activity to the controller. Off the hook path: the lock is
/// held only to handle what one round found. Errors are logged and skipped.
fn watch_fleet(dir: PathBuf, state: &Mutex<State>) {
    let cwd = loop {
        thread::sleep(FLEET_POLL);
        if let Some(c) = state.lock().unwrap_or_else(|e| e.into_inner()).cwd.clone() {
            break c;
        }
    };
    let ws = crate::fleet::workspace(Path::new(&cwd));
    let mut watcher = crate::fleet::Watcher::new(dir, ws.common.clone());
    let mut events = vec![tend_core::Event::Workspace {
        top: ws.top,
        common: ws.common,
    }];
    let root = crate::paths::sessions_dir();
    loop {
        if !events.is_empty() {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            for ev in events.drain(..) {
                let d = st.controller.handle(ev, now_ms());
                if let Err(e) = st.store.persist(&d) {
                    eprintln!("tendd: fleet: {e:#}");
                }
            }
        }
        thread::sleep(FLEET_POLL);
        events = watcher.poll(&root, now_ms());
    }
}

/// Plugin mode has no launcher to watch: exit after a long quiet spell. A later hook
/// starts a new daemon for the same session if the agent comes back.
fn watch_idle(minutes: u64, sock: &Path) {
    loop {
        thread::sleep(Duration::from_secs(30));
        let last = LAST_REQUEST_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now_ms().saturating_sub(last) > minutes * 60_000 {
            eprintln!("tendd: idle for {minutes} min; exiting");
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

/// Exit when the launcher is gone, so no daemon outlives its session.
fn watch_parent(pid: i32, sock: &Path) {
    loop {
        thread::sleep(Duration::from_secs(2));
        // SAFETY: kill with signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            eprintln!("tendd: launcher {pid} is gone; exiting");
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

/// What one hook call decides, and the response to send. Time comes from `clock`, so the
/// late paths can be tested (I5, T3). D9: a reply after the shim's deadline reaches no
/// one. Late before handling: nothing is applied. Late after: the decisions are recorded
/// as not delivered, and nothing is sent.
fn decide_hook(
    controller: &mut Controller,
    connector: &mut dyn Connector,
    event: &str,
    payload: &serde_json::Value,
    deadline_ms: Option<u64>,
    clock: &mut dyn FnMut() -> u64,
    session_id: &str,
) -> (Vec<tend_core::Decision>, Response) {
    let late = |t: u64| deadline_ms.is_some_and(|d| t + REPLY_MARGIN_MS > d);
    let now = clock();
    let unanswered = late(now);
    let mut decisions = Vec::new();
    for ev in connector.map(event, payload) {
        // The repo's policy, from the session's working directory (E7).
        if let tend_core::Event::SessionStarted { cwd, .. } = &ev {
            let (policy, warnings) = crate::config::load_policy(Path::new(cwd));
            for w in warnings {
                eprintln!("tendd: {w}");
            }
            controller.set_policy(policy.clone());
            connector.set_policy(&policy);
            // Shared learning (F2): other sessions' fixes and this project's notes.
            if controller.config().learn != tend_core::FleetSetting::Off {
                let project = crate::learnstore::project_of(Path::new(cwd));
                controller.set_learning(
                    &project,
                    crate::learnstore::load_fixes(session_id),
                    crate::learnstore::approved_notes(&project),
                );
            }
        }
        decisions.push(if unanswered {
            controller.handle_unanswered(ev, now)
        } else {
            controller.handle(ev, now)
        });
    }
    let mut response = Response::default();
    for d in &decisions {
        response.merge(d.response.clone());
    }
    if !response.is_empty() {
        let t = clock();
        if late(t) {
            let d = controller.undelivered(&decisions, t);
            decisions.push(d);
            response = Response::default();
        }
    }
    (decisions, response)
}

fn reply_to(mut stream: UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    Ok(())
}

fn serve(
    stream: UnixStream,
    state: &Mutex<State>,
    sock: &Path,
    token: &Option<String>,
) -> Result<()> {
    // A client that connects and sends nothing must not pin a thread forever (F9).
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)?;
    // A liveness probe (`is_live`) connects and closes: not an error, and not activity,
    // or peers' fleet watchers would keep a plugin daemon from its idle exit.
    if line.is_empty() {
        return Ok(());
    }
    LAST_REQUEST_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    let req: Request = serde_json::from_str(&line).context("bad request")?;
    let mut shutdown = false;

    // Control requests must show the session token (R0.8).
    let given = match &req {
        Request::SetMode { token, .. }
        | Request::Allow { token, .. }
        | Request::Shutdown { token } => Some(token),
        _ => None,
    };
    if let Some(given) = given {
        if token.is_some() && given != token {
            eprintln!("tendd: refused a control request without the session token");
            return reply_to(stream, &Reply {
                output: None,
                message: Some(
                    "tend: refused: this needs the session's control token. Run `tend allow` or `tend mode` yourself.".into(),
                ),
            });
        }
    }

    // Requests that must not wait behind other work (R0.3).
    let req = match req {
        // Liveness: answer without the lock, so a busy daemon is never taken for dead.
        Request::Ping => {
            return reply_to(
                stream,
                &Reply {
                    output: None,
                    message: Some("pong".into()),
                },
            );
        }
        // The heavy part of a filter (hash, failures, summary) runs outside the lock,
        // so hooks from parallel tool calls are not delayed by a large output.
        Request::Filter {
            tool_use_id,
            output,
        } => {
            let plan = {
                let st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.controller
                    .filter_level(&tool_use_id)
                    .map(|level| (level, st.controller.config().clone()))
            };
            let message = match plan {
                None => None,
                Some((level, cfg)) => {
                    let prepared = tend_core::prepare(&output, level, &cfg);
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    let st = &mut *st;
                    let (d, text) =
                        st.controller
                            .filter_prepared(&tool_use_id, output, prepared, now_ms());
                    st.store.persist(&d)?;
                    text
                }
            };
            return reply_to(
                stream,
                &Reply {
                    output: None,
                    message,
                },
            );
        }
        other => other,
    };

    let reply = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *st;
        match req {
            Request::Hook {
                event,
                payload,
                deadline_ms,
            } => {
                st.store.capture(now_ms(), &event, &payload)?;
                if event == "PreToolUse" {
                    refresh_claims(st);
                }
                read_inbox(st)?;
                let (decisions, response) = decide_hook(
                    &mut st.controller,
                    st.connector.as_mut(),
                    &event,
                    &payload,
                    deadline_ms,
                    &mut now_ms,
                    &st.session_id.clone(),
                );
                for d in &decisions {
                    st.store.persist(d)?;
                }
                if st.project.is_none() {
                    st.project =
                        decisions
                            .iter()
                            .flat_map(|d| &d.entries)
                            .find_map(|e| match &e.body {
                                tend_core::Body::Event {
                                    event: tend_core::Event::SessionStarted { cwd, .. },
                                } => Some(crate::learnstore::project_of(Path::new(cwd))),
                                _ => None,
                            });
                }
                if let Some(project) = &st.project {
                    crate::learnstore::save_fixes(&decisions, &st.session_id, project);
                }
                remember(st, &decisions)?;
                if st.notify {
                    notify(&notifications(&decisions));
                }
                Reply {
                    output: st.connector.encode(&event, &payload, &response),
                    message: None,
                }
            }
            Request::SetMode { mode, .. } => {
                let d = st.controller.set_mode(mode, now_ms());
                st.store.persist(&d)?;
                Reply {
                    output: None,
                    message: Some(format!("mode: {mode:?}").to_lowercase()),
                }
            }
            Request::Allow { action, .. } => {
                let message = match st.controller.allow(action.as_deref(), now_ms()) {
                    Ok(d) => {
                        st.store.persist(&d)?;
                        "Allowed once. The agent can retry the edit now.".to_string()
                    }
                    Err(m) => m,
                };
                Reply {
                    output: None,
                    message: Some(message),
                }
            }
            Request::Ping | Request::Filter { .. } => unreachable!("answered above"),
            Request::Shutdown { .. } => {
                shutdown = true;
                Reply {
                    output: None,
                    message: Some("bye".into()),
                }
            }
        }
    };

    reply_to(stream.try_clone()?, &reply)?;
    if shutdown {
        // Wait for any request that is writing the ledger, then exit (R0.6).
        let _done = state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = fs::remove_file(sock);
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_daemon_does_not_deliver_old_inbox_messages_again() {
        let d = std::env::temp_dir().join(format!("tend-inbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        fs::write(&inbox, "{\"text\":\"one\"}\n{\"text\":\"half").unwrap();
        let mut offset = inbox_seen(&inbox);
        assert_eq!(new_inbox_lines(&inbox, &mut offset).len(), 1);
        fs::write(&inbox, "{\"text\":\"one\"}\n{\"text\":\"two\"}\n").unwrap();
        // A restart: the offset comes from the file, not from 0.
        let mut offset = inbox_seen(&inbox);
        let got = new_inbox_lines(&inbox, &mut offset);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["text"], "two");
        let _ = fs::remove_dir_all(&d);
    }
    use crate::connector::claude_code::ClaudeCode;
    use tend_core::{Body, Capabilities, Config};

    fn weakening_edit() -> serde_json::Value {
        let p = format!(
            "{}/../../fixtures/claude_code/pre_tool_use_edit_test.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    /// Run one guard-deny hook with a clock that returns `times` in turn.
    fn decide(deadline: Option<u64>, times: &[u64]) -> (Vec<tend_core::Decision>, Response) {
        let mut c = Controller::new(Config::default(), Capabilities::CLAUDE_CODE, Mode::Optimize);
        let mut conn = ClaudeCode::default();
        let mut t = times.iter().copied();
        let mut clock = move || t.next().unwrap_or(u64::MAX / 2);
        decide_hook(
            &mut c,
            &mut conn,
            "PreToolUse",
            &weakening_edit(),
            deadline,
            &mut clock,
            "test",
        )
    }

    fn applied(ds: &[tend_core::Decision]) -> Vec<bool> {
        ds.iter()
            .flat_map(|d| &d.entries)
            .filter_map(|e| match &e.body {
                Body::Action { applied, .. } => Some(*applied),
                _ => None,
            })
            .collect()
    }

    fn undelivered(ds: &[tend_core::Decision]) -> bool {
        ds.iter()
            .flat_map(|d| &d.entries)
            .any(|e| matches!(e.body, Body::Undelivered { .. }))
    }

    #[test]
    fn blocked_edits_and_loops_are_worth_a_notification() {
        let (ds, _) = decide(Some(1_000), &[100, 200]);
        let n = notifications(&ds);
        assert_eq!(n.len(), 1);
        assert!(n[0].contains("tend allow"), "{n:?}");
        // Late: nothing was blocked, nothing to say.
        let (ds, _) = decide(Some(1_000), &[2_000]);
        assert!(notifications(&ds).is_empty());
    }

    #[test]
    fn on_time_reply_is_sent_and_applied() {
        let (ds, r) = decide(Some(1_000), &[100, 200]);
        assert!(r.permission.is_some());
        assert_eq!(applied(&ds), [true]);
        assert!(!undelivered(&ds));
    }

    #[test]
    fn late_before_handling_applies_nothing() {
        let (ds, r) = decide(Some(1_000), &[2_000]);
        assert!(r.is_empty());
        assert!(applied(&ds).iter().all(|a| !a));
    }

    /// Within the reply margin counts as late: the reply could not get out in time.
    #[test]
    fn inside_the_margin_is_late() {
        let (_, r) = decide(Some(1_000), &[1_000 - REPLY_MARGIN_MS + 1]);
        assert!(r.is_empty());
        let (_, r) = decide(Some(1_000), &[1_000 - REPLY_MARGIN_MS - 1, 0]);
        assert!(r.permission.is_some());
    }

    #[test]
    fn late_after_handling_is_recorded_as_undelivered() {
        let (ds, r) = decide(Some(1_000), &[100, 5_000]);
        assert!(r.is_empty(), "nothing is sent");
        assert!(undelivered(&ds));
    }

    #[test]
    fn no_deadline_is_never_late() {
        let (ds, r) = decide(None, &[u64::MAX / 4, u64::MAX / 4]);
        assert!(r.permission.is_some());
        assert!(!undelivered(&ds));
    }
}
