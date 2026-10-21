# tend fleet: topologies and management

**Status:** design, 2026-10-21. Owner decisions are in §9. It extends `docs/fleet-2026-10-21.md` (layers L1–L4, F1–F6). It replaces the short F3 and F5 there. Nothing is built.

## 1. Goal

A user describes a team of agents once. tend runs it.

- The user writes a fleet file: roles, how many of each, who talks to whom, what each may touch, and when work counts as done.
- tend starts the sessions, gives each its role, routes work and messages, enforces the limits, checks results, restarts what fails, and records everything.
- The agents do the thinking. tend never plans the work and never calls a model itself.

The one-line pitch: **Kubernetes for coding agents on one machine, with tend's honesty rules built in.** A task is done when its check passes, not when an agent says so.

## 2. End use cases

| # | Use case | Topology | What tend does that a script cannot |
|---|---|---|---|
| U1 | "Migrate 40 modules from API v1 to v2." | Lead and workers | Lead splits the work into tasks. tend spawns 4 workers in worktrees, gives each a task, stops two workers from claiming one file, and reruns the tests on each branch before the lead sees "done". |
| U2 | Implement, then review, then fix. | Review loop | The reviewer gets the implementer's diff, audit, and test result. Its findings go back as a task. The loop stops after N rounds, or when the check passes and the reviewer approves. |
| U3 | Overnight backlog. | Worker pool | Workers pull items from a queue (for example `docs/TASKS.md`). In the morning, one report shows each item: done and verified, blocked with a reason, or failed after K tries. |
| U4 | "Try three ways." | Best of N | The same task goes to N sessions (different models, later different agents). Each is verified. A judge (a rule, an agent, or the user) picks one. The others are archived. |
| U5 | Separation of duties. | Specialists | A test-writer may edit only tests. An implementer may not edit tests. tend enforces this with deny. Test weakening becomes impossible by role, not just detectable. |
| U6 | Watch a long run. | Supervisor | A supervisor agent gets loop, guard, and stuck notices from every worker. It decides: nudge, reassign, or ask the user. |
| U7 | The user is the lead. | Human lead | The user posts and assigns tasks with `tend task`. Workers report to the user's inbox and notifications. |
| U8 | Large project. | Hierarchy | A lead has sub-leads, each with workers (opt-in, depth limit). |

## 3. The model

A fleet file describes six things. Presets fill most of it.

### 3.1 Roles

| Field | Meaning | Example |
|---|---|---|
| `agent` | Which agent runs it | `claude`, later `codex` |
| `model` | Model for the session | `sonnet` |
| `replicas` | Min and max running at once | `1..4` |
| `run` | How the session runs (§4.1) | `oneshot` (default for workers), `stream` (default for leads), `attached` |
| `workspace` | Where it works (§3.6) | `worktree-per-worker` (default), `team:<name>` (one worktree for a group), `shared` (the user's checkout) |
| `permission_mode` | Claude Code permission mode | `auto` |
| `may_write` / `may_not_write` | Path globs, enforced by PreToolUse deny | implementer: `may_not_write = ["tests/**"]` |
| `may` | Fleet verbs this role may use | lead: `task.add`, `task.assign`, `spawn`, `merge.propose` |
| `guard` | Guard profile | `strict` for workers |
| `budget` | Tokens, wall time, attempts per task | `tokens = 2M`, `wall = 45m`, `attempts = 3` |
| `brief` | Role text added at session start | "You are a worker. Claim one task …" |

### 3.2 Channels

Edges between roles. Each edge allows some message kinds.

- Kinds: `task`, `report`, `question`, `answer`, `review`, `notice`, `handoff`.
- Delivery: `next-hook` (any session), `turn` (a new turn for a streamed session), or `on-stop` (when the sender stops).
- A kind not on an edge is refused and recorded. Example: in `lead`, a worker cannot send a `task` to another worker.

### 3.3 Task board

Shared state that tend owns. Agents change it only through `tend task …`.

```
open → claimed → working → checking → done
                   │           │
                   ▼           ▼
                blocked      failed → open (retry, up to attempts)
```

A task has: id `t-N`, title, body, `depends_on`, `scope` (paths it expects to touch), assignee, attempts, and artifacts (branch, diff ref, audit line, check result, final summary).

### 3.4 Checks (the verification gate)

- Each task, or each role, names a check: a command such as `cargo test`, `make check`, or a test file id.
- When a worker says `tend task done t-4`, the task moves to `checking`, not to `done`.
- tend runs the check itself in the worker's workspace. It records exit code, failures (with the existing parsers), and the session audit (E2).
- Pass: `done`. Fail: back to the worker with the failures, as a new attempt.
- The guard and the audit are part of the check. A task whose diff removes a test is `checking: flagged` and goes to the lead or the user.

This is what tend adds over other orchestrators: "done" is checked, not taken on trust.

### 3.5 Claims

- A lease on path globs, with a TTL, held by a session or a task.
- `scope` on a task becomes a claim when the task is claimed.
- A write outside the agent's claims, inside someone else's: a notice by default, a deny if the fleet file says `claims = "enforce"`.
- Claims expire. A dead session never holds a lock.
- With worktrees, claims also predict merge conflicts before they happen.

### 3.6 Workspaces

**Default: one worktree per worker.** A worker is a slot, not a session.

- The slot owns a worktree and a branch (`tend/<fleet>/<worker>`), under one tend-owned directory.
- Workers run oneshot, so each task is a fresh session in the slot's worktree, and the branch collects the slot's tasks in order.
- tend records a base tree at the start and end of each task (the E2 temp-index method). So each task still has its own diff, audit, and check, even on a shared branch. tend never commits for the agent and never moves the user's refs.

**Teams share a worktree.** Several workers may work in one worktree (`workspace = "team:api"`), for tasks too entangled to split. They coordinate through tend:

- **Claims** on the paths a task names in `scope`. In a team worktree claims default to `enforce`: a write into another worker's claim is denied, and the reason names the holder.
- **Awareness** (F1) on every write: stale-read and concurrent-write notices go to the other team members at their next hook.
- **Messages** on the team channel (`#team-api`) for "I need `src/api/mod.rs` for a minute" and "done with it".
- **Serialized checks.** A team's checks run one at a time, so one worker's half-done edit does not fail another's check. A check failure caused by a file that another worker changed during the task is marked `interference` and retried once, rather than counted against the task.

**Across worktrees.** Claims and touches also predict merge conflicts between worker branches. `tend fleet merge` uses them to order merges, and the lead gets a notice when two branches start to overlap.

### 3.7 Hierarchy (first-class)

A role can be a **sub-lead**. It leads its own small fleet inside the parent fleet.

- **Task trees.** A lead assigns task `t-3` to a sub-lead. The sub-lead splits it into `t-3.1`, `t-3.2`, … on its own sub-board. `t-3` is done only when all its children are done and `t-3`'s own check passes.
- **Budgets and rights only narrow downward.** A sub-lead gets a slice of its parent's budget, replicas, and paths. It can never grant a child more than it has. The fleet file sets a depth limit (default 2 when hierarchy is used; hard cap 4).
- **Escalation goes up.** A blocked child goes to its sub-lead, then to the lead, then to the user. Each step is a ledger entry with a cause.
- **Reports roll up.** `tend fleet report` shows the tree. Each node shows state, spend, attempts, and check results for itself and its subtree.
- **Workspaces follow the tree.** By default a sub-lead's workers branch from the sub-lead's branch. The sub-lead proposes merging its children into its branch. Only the user merges into the base branch.
- **Failure isolation.** If a sub-lead crashes, its subtree pauses. tend restarts the sub-lead from its ledger and board. The rest of the fleet keeps going.

In the core, a hierarchy is the same `step()` applied to nested fleets. A sub-fleet is a fleet whose limits come from its parent instead of from the user.

### 3.8 Triggers

Rules of the form "on event, do action". Events come from the existing controller and from the hub.

| Event | Default action |
|---|---|
| Worker stops (turn ends with no task) | Give it the next open task it may take, or retire it |
| Task done | Unblock tasks that depend on it; notify the lead |
| Loop detected (existing `loops.rs`) | Notice to the worker; after N, notice to the lead or supervisor |
| Guard deny | Notice to the lead; count it against the task |
| Stuck (process alive, no hook for M minutes) | Notice; then interrupt; then restart |
| Session crashed | Restart with backoff, up to attempts; then mark the task failed |
| Budget hit | Stop new work for that role; ask the user |
| Usage limit hit (as found in the benchmark) | Pause the fleet; resume after the reset time |
| All tasks done or failed | Final report; fleet stops |

## 4. Mechanics (what makes it possible)

### 4.1 Three ways a session runs

| Mode | How | Can tend give it work while idle? | Use |
|---|---|---|---|
| `stream` | `claude -p --input-format stream-json --output-format stream-json`, under the tend launcher. tend holds stdin and stdout. | **Yes.** A message or task is a new user turn. | Default for leads, sub-leads, and supervisors |
| `oneshot` | `claude -p "<task>"` per task. A fresh session each time. | No; one task, then exit. | **Default for workers** (owner decision): clean context per task; best of N |
| `attached` | The user's own terminal (`tend claude --tend-role lead`). | No. Only at its next hook. The user sees an unread count. | Human-in-the-loop leads |

`stream` mode is the key. Without it, an idle agent cannot be woken, and leader topologies would need tricks with the Stop hook. **Confirmed by spike F-S1 (2026-10-21, `docs/TASKS.md`):**
- Hooks fire per turn as in a normal session, under the `--session-id` tend picks.
- A new user line wakes an idle session; lines sent during a turn are queued.
- An interrupt is the control request `{"type":"control_request","request":{"subtype":"interrupt"}}`; the session takes the next turn after it.
- Closing stdin ends the session cleanly.
- `UserPromptSubmit` context reaches the model, so attached sessions can be told things at their next prompt.
- Forks and resumes copy the parent's messages into a new transcript, so token budgets count each message uuid once.

Other useful flags (Claude Code 2.1.306, from `claude --help`):
- `--session-id <uuid>`: tend picks the id, so the ledger, hub, and transcript line up from the start.
- `--append-system-prompt`: the role brief.
- `--resume <id> --fork-session`: fork one session's context into many. A lead plans once; N workers start from that plan. This also gives best of N a shared start.
- `--model`, `--permission-mode`, `--settings` (hooks, as today).

### 4.2 Processes

```
 user ── tend fleet up fleet.toml
              │
              ▼
          tend hub  (one per user; owns the fleet; Unix socket, 0700)
          │  fleet ledger, task board, claims, triggers
          │
          ├── session daemon ◄── shim ◄── agent (stream)   worker-1, worktree A
          ├── session daemon ◄── shim ◄── agent (stream)   worker-2, worktree B
          ├── session daemon ◄── shim ◄── agent (stream)   lead
          └── session daemon ◄── shim ◄── agent (attached) user's terminal
```

- The hub starts and stops sessions, holds the stdin of `stream` sessions, and runs checks.
- Session daemons stay as they are: one per session, fast hook path. They report to the hub off the hot path.
- If the hub dies, each session keeps running as a solo tend session (fail open). On restart, the hub rebuilds the fleet state from its ledger and reattaches to live daemons. `stream` sessions whose stdin it lost are resumed with `--resume`.

### 4.3 The core stays pure

`tend-core/src/fleet/` holds:
- the fleet file model and its validation;
- the task-board state machine;
- claims;
- channel permissions;
- triggers, as `fn step(state, event, now_ms) -> (state, Vec<Command>)`.

Commands are plain data: `Spawn`, `SendTurn`, `Interrupt`, `Stop`, `RunCheck`, `Notify`, `Record`. The CLI executes them. This keeps rule 2 (no I/O in core) and rule 6 (deterministic). It also allows `tend fleet replay` with a different fleet file, the same way `tend replay` works for one session.

### 4.4 How agents use it

Through a shell command, so it works for any agent with a shell. An MCP server with the same verbs comes later (E13).

| Verb | Who (default) |
|---|---|
| `tend task list` / `show t-N` | Everyone |
| `tend task add "…" [--depends t-2] [--scope 'src/api/**'] [--check 'cargo test']` | Lead, user |
| `tend task assign t-N <session\|role>` | Lead, user |
| `tend task claim [t-N]` / `done t-N "summary"` / `blocked t-N "reason"` | Worker |
| `tend msg <to> "…"` | Along channels |
| `tend fleet spawn <role>` | Lead, if `may` allows it; within replicas and budget |
| `tend merge propose <task>` | Lead |

- The daemon checks every verb against the role (PreToolUse sees the Bash command). A verb the role may not use is denied with a reason, the same way the agent's own `tend allow` is denied today (S6).
- Each session's brief lists its verbs, its channels, and its peers.

## 5. Topology presets

| Preset | Roles | Channels | Workspace | Notes |
|---|---|---|---|---|
| `solo` | 1 any | none | shared | Default: awareness and learning only |
| `peers` | N peers | all ↔ all: `notice`, `question`, `answer` | shared or worktrees | Claims advisory |
| `lead` | lead, workers 1..N | lead → worker: `task`; worker → lead: `report`, `question` | worktree per worker | Lead proposes merges; the user merges |
| `pipeline` | stage 1 … stage K | stage i → i+1: `handoff` on stop | one branch, moved along | Example: plan → implement → test → docs |
| `review` | author, reviewer | author → reviewer: `handoff`; reviewer → author: `review` | one worktree | `max_rounds`; stops on pass + approval |
| `best-of` | N candidates, judge | judge ← candidates: `report` | worktree each (oneshot from a fork) | Judge: `rule` (check + smallest diff), `agent`, or `user` |
| `specialists` | tester, implementer, … | via lead or pipeline | shared or worktrees | Path scopes per role enforce separation of duties |
| `supervisor` | supervisor + any preset | all → supervisor: `notice` | — | Overlay. Adds a watchdog to another preset |
| `pool` | workers 1..N | queue → worker: `task` | worktree per worker | Queue from `tend task add` or from a file (`docs/TASKS.md` checkboxes) |
| `tree` | lead, sub-leads, workers | down: `task`; up: `report`, `question`, escalation | sub-lead branch, worker worktrees from it | §3.7 |
| `custom` | any | any | any | The full fleet file |

Presets compose: `lead + supervisor`, `pool + review` (each task gets a reviewer before `done`).

A short fleet file:

```toml
preset = "lead"
check  = "make check"

[role.lead]
model = "opus"
run   = "stream"

[role.worker]
model          = "sonnet"
run            = "oneshot"              # default
workspace      = "worktree-per-worker"  # default"
replicas       = "1..4"
may_not_write  = ["tests/**"]       # tests come from the lead's tasks
budget         = { tokens = "2M", wall = "45m", attempts = 3 }

[limits]
sessions = 5            # default: free memory / 300 MiB, capped (F-S2: ~300 MiB per agent)
tokens   = "20M"
```

## 6. Management (the user's side)

| Command | Function |
|---|---|
| `tend fleet up [file] [--preset lead]` | Start a fleet. Prints its id. |
| `tend fleet` / `status` | Roles, sessions, task board, claims, budget used, problems |
| `tend fleet scale worker=3` | Change replicas |
| `tend fleet pause` / `resume` | Stop handing out work; let running turns finish |
| `tend fleet stop <session>` / `down` | Stop one session, or the fleet (S7 signal path) |
| `tend fleet attach <session>` | Prints the command to open a session in a terminal (`tend claude --resume <id>`); the session becomes `attached` |
| `tend task …` | The user acts as a lead |
| `tend fleet inbox` | Questions and escalations addressed to the user, with replies by `tend msg` |
| `tend fleet report` | One report: per task (state, attempts, check, audit), per session (tokens, time, loops, guard), totals |
| `tend fleet merge [task]` | Show the merge plan for done tasks: order, predicted conflicts from claims and touches. Merges only on the user's yes |
| `tend fleet prune` | Remove finished worktrees with no unmerged work, after confirmation |
| `tend fleet replay <id> --file other.toml` | Rerun the recorded fleet against another fleet file (no model calls) |

The user decides by default:
- merges into the base branch;
- spawns beyond the fleet file's replicas or budget;
- tasks flagged by the guard or the audit.

The fleet file can hand any of these to the lead.

## 7. Safety

| Risk | Rule |
|---|---|
| A compromised or confused lead gives bad tasks | Worker permissions come from the fleet file, never from the lead. A lead cannot widen a scope, a mode, or a budget. |
| Injection spreads between agents | Messages are untrusted data with provenance. They never grant permission, lift a block, or change the fleet file. Rate limits per edge. |
| Runaway spawning | Replicas, a session limit, a token budget, and a depth limit (1 without hierarchy; 2 by default with it; hard cap 4). Children get a slice of the parent's limits, never more. `tend fleet down` always works. |
| An agent edits outside its workspace | Path scopes enforced in PreToolUse. Bash writes are best-effort (as for R1.4), so worktrees are the real isolation. |
| Lost work | Worktrees are never removed with unmerged commits. Every task's diff is kept as a blob. |
| Silent cost | Budgets are checked from transcripts after every turn. The status always shows spend. |
| The usage limit is shared by all sessions | The hub detects a limit hit (as the bench runner does) and pauses the whole fleet. |
| "Done" that is not done | The check gate (§3.4). |

All of this stays capability-honest. Codex cannot `ask`, so any "ask the user" step becomes deny plus an inbox item.

## 8. Plan

This replaces F3 and F5 in `docs/fleet-2026-10-21.md`. F1, F2, F4, and F6 stay. F1 (awareness) is still built first.

| # | Task | Acceptance | Size |
|---|---|---|---|
| F-S1 | **Spike: stream mode.** (Done 2026-10-21: all confirmed.) Long-lived `claude -p` with stream-json in and out, under tend's `--settings`. Do hooks fire? Can a turn be sent when idle? What does an interrupt look like? Does `--resume --fork-session` work headless with hooks? | Results under "Spike results" | S |
| F-S2 | **Spike: concurrency.** (Done 2026-10-21: no rate limits at 8 Haiku sessions; each agent ~300 MiB, each tend daemon ~6.5 MiB.) Run 4 stream sessions at once. Measure rate-limit and usage-limit behavior and the hub's CPU. | Numbers in the doc | S |
| F3a | **Spawn and lifecycle.** (Done 2026-10-21: `tend-cli/src/hub.rs`; a dead hub ends its stream sessions, see `docs/TASKS.md`.) Hub spawns `stream` and `oneshot` sessions (with `--session-id`, brief, worktree); stop, restart with backoff, stuck detection, `fleet status`, `stop`, `down`, `prune`. | e2e: 2 fake stream workers; kill one; it restarts; `down` stops all; no worktree lost | M |
| F3b | **Core fleet model.** (Done 2026-10-21: `tend-core/src/fleet/spec.rs` and `model.rs`; see `docs/TASKS.md`.) `fleet/` in tend-core: fleet file, roles, channels, presets, `step()` with commands. | Table tests per preset; replay is deterministic | M |
| F5a | **Task board and verbs.** (Done 2026-10-21: see `docs/TASKS.md`; sessions are identified by process tree.) `tend task …`, role checks in PreToolUse, assignment to idle stream sessions. | e2e: lead adds 3 tasks, 2 workers finish them, dependencies respected | M |
| F5b | **Check gate.** (Done 2026-10-21; audit flags only with `experimental_audit = true`.) tend runs the check in the workspace; failures go back as a new attempt; guard and audit flags escalate. | e2e: a worker that says done on a failing test gets the failure back | M |
| F5c | **Workspaces, claims, and scopes.** Worker slots with worktrees; team worktrees with enforced claims, team channel, serialized checks, `interference` retry; `may_write`; cross-worktree conflict prediction. | Two workers in one team worktree: a write into the other's claim is denied; in `specialists` an implementer's test edit is denied | M |
| F5d | **Budgets and limits.** Tokens per session, role, and fleet; usage-limit pause; spawn depth. | A fleet stops handing out work at its budget | S |
| F5e | **Reports and merge plan.** `fleet report`, `fleet merge` (plan only, merge on yes), `fleet replay`. | Report matches a seeded fleet run | M |
| F5f | **More presets.** `review`, `best-of`, `pipeline`, `supervisor`, `pool` (from TASKS.md). | One seeded run per preset in `bench/` | M |
| F5g | **Hierarchy.** Sub-leads, task trees, budget slices, escalation, roll-up reports, nested `step()`, sub-lead restart. | e2e: lead → 2 sub-leads → 2 workers each; a child cannot exceed its slice; a killed sub-lead resumes its subtree | M–L |

Order: F-S1 → F-S2 → F3b → F3a → F5a → F5b → F5d → F5c → F5g → F5e → F5f.

Hierarchy is first-class, so F3b designs the core model as nested from the start: a flat fleet is a tree of depth 1. F5g then adds sub-leads without reshaping the core.

**Benchmark.** Extend `bench/` with fleet tasks: a fan-out refactor (U1) and a backlog (U3), with and without the check gate. The first question to answer is whether the gate catches false "done" claims that would otherwise reach the lead.

## 9. Owner decisions (2026-10-21)

1. **Workers run oneshot**: a fresh session per task. Leads, sub-leads, and supervisors run `stream`.
2. **The user merges** into the base branch. tend shows the merge plan; leads and sub-leads only propose.
3. **One worktree per worker** by default. Several workers may share a team worktree, where they coordinate through enforced claims, awareness notices, a team channel, and serialized checks (§3.6).
4. **Hierarchy is first-class** (§3.7). The core model is nested from the start.
5. Still proposed, not decided: a lead may spawn by default, within the fleet file's limits.
