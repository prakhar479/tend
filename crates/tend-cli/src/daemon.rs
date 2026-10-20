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
}

pub fn run(dir: PathBuf, parent_pid: Option<i32>, mode: Mode, agent: &str) -> Result<()> {
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
    let controller = Controller::new(crate::config::load(), connector.capabilities(), mode);
    let store = Store::open(&dir)?;
    // Without a token file (a daemon started by hand), control requests are open.
    let token: Arc<Option<String>> = Arc::new(crate::paths::read_token(&dir));
    let state = Arc::new(Mutex::new(State {
        controller,
        connector,
        store,
    }));
    eprintln!("tendd: listening on {} (mode {mode:?})", sock.display());

    if let Some(pid) = parent_pid {
        let sock = sock.clone();
        thread::spawn(move || watch_parent(pid, &sock));
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
) -> (Vec<tend_core::Decision>, Response) {
    let late = |t: u64| deadline_ms.is_some_and(|d| t + REPLY_MARGIN_MS > d);
    let now = clock();
    let unanswered = late(now);
    let mut decisions = Vec::new();
    for ev in connector.map(event, payload) {
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
    // A liveness probe (`is_live`) connects and closes: not an error.
    if line.is_empty() {
        return Ok(());
    }
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
                let (decisions, response) = decide_hook(
                    &mut st.controller,
                    st.connector.as_mut(),
                    &event,
                    &payload,
                    deadline_ms,
                    &mut now_ms,
                );
                for d in &decisions {
                    st.store.persist(d)?;
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
