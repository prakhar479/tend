//! Session daemon: holds the controller, connector, and store for one session.
//! One thread per connection; one mutex around all session state.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rein_core::{Controller, Mode, Response};

use crate::connector::{self, Connector};
use crate::ipc::{Reply, Request};
use crate::paths::{now_ms, socket_path};
use crate::store::Store;

struct State {
    controller: Controller,
    connector: Box<dyn Connector>,
    store: Store,
}

pub fn run(dir: PathBuf, parent_pid: Option<i32>, mode: Mode, agent: &str) -> Result<()> {
    let sock = socket_path(&dir);
    // The socket accepts hook events and `rein allow`: keep it in a private dir.
    crate::paths::ensure_private_dir(&dir)?;
    if let Some(parent) = sock.parent() {
        crate::paths::ensure_private_dir(parent)?;
    }
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    let connector = connector::for_agent(agent)?;
    let controller = Controller::new(crate::config::load(), connector.capabilities(), mode);
    let store = Store::open(&dir)?;
    let state = Arc::new(Mutex::new(State {
        controller,
        connector,
        store,
    }));
    eprintln!("reind: listening on {} (mode {mode:?})", sock.display());

    if let Some(pid) = parent_pid {
        let sock = sock.clone();
        thread::spawn(move || watch_parent(pid, &sock));
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let sock = sock.clone();
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &state, &sock) {
                        eprintln!("reind: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("reind: accept: {e}"),
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
            eprintln!("reind: launcher {pid} is gone; exiting");
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

fn reply_to(mut stream: UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    Ok(())
}

fn serve(stream: UnixStream, state: &Mutex<State>, sock: &Path) -> Result<()> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let req: Request = serde_json::from_str(&line).context("bad request")?;
    let mut shutdown = false;

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
                    let prepared = rein_core::prepare(&output, level, &cfg);
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
            Request::Hook { event, payload } => {
                let now = now_ms();
                st.store.capture(now, &event, &payload)?;
                let mut response = Response::default();
                for ev in st.connector.map(&event, &payload) {
                    let d = st.controller.handle(ev, now);
                    st.store.persist(&d)?;
                    response.merge(d.response);
                }
                Reply {
                    output: st.connector.encode(&event, &payload, &response),
                    message: None,
                }
            }
            Request::SetMode { mode } => {
                let d = st.controller.set_mode(mode, now_ms());
                st.store.persist(&d)?;
                Reply {
                    output: None,
                    message: Some(format!("mode: {mode:?}").to_lowercase()),
                }
            }
            Request::Allow { action } => {
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
            Request::Shutdown => {
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
