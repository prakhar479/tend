# winnow — Reference

| Item | Value |
|---|---|
| Status | Draft for review |
| Date | 2026-10-19 |
| Replaces | Handoff of 2026-10-19 (content merged here) |
| Audience | Human reviewer |

**How to use this document.** Sections 1–4 define the product. Sections 5–7 give feasibility and risk. Sections 8–13 give the design. Sections 14–18 give the plan and records.

**Tags.** **[Verify]** = the fact comes from a secondary source. Confirm it in Phase 0. **[New]** = not in the handoff.

---

## 1. Summary

winnow is a local, open-source supervisor for command-line coding agents. The first agents are Claude Code and Codex.

The user starts the agent through winnow: `winnow claude`, `winnow codex`.

winnow does four things:

1. It removes noise from tool output before the agent reads it.
2. It finds loops: the agent repeats the same failure.
3. It finds edits that weaken tests.
4. It records each action, gives the reason, and lets the user undo it.

winnow adjusts its behavior during a session. Each adjustment has a one-sentence reason.

**Position [New].** winnow keeps agents honest and unstuck. It uses fewer tokens to do this. Token savings are secondary, because other tools already do them (see section 6).

---

## 2. Problem

| Problem | Effect |
|---|---|
| Context bloat | Long tool output fills the context. Cost and latency increase. Agent attention decreases. |
| Doom loops | The agent repeats a failed edit. The agent weakens a test to get a pass. |
| Cache cost | A change to earlier context breaks the provider prompt cache. |
| Fragmentation | Each agent needs its own fix. |

---

## 3. Principles

1. **Add to the future. Do not edit the past.** A decision about content is final. Adaptation changes only later content.
2. **Observe before you act.** Every action is optional.
3. **Explain and undo.** Every action has a reason and an undo.
4. **Black-box and local.** Do not change agent code or schemas. Data stays on the machine. Exception: a decider that the user enables.
5. **Capability-honest.** State what each agent supports. Degrade when necessary (example: deny → warn).
6. **Simple adaptation.** Describe each change in one sentence.
7. **Deciders judge. They do not act.** Policy selects actions.
8. **Fail open [New].** If winnow fails or is slow, the agent continues without change.

---

## 4. Scope

**In scope**

- Claude Code and Codex connectors.
- Two modes: look (observe) and optimize (act).
- Output reduction, loop detection, test guard, snapshots, explanation, undo.
- A typed contract for external deciders.

**Not in scope**

- A new coding agent or chat interface.
- A terminal interface or multiplexer.
- A cloud service, remote telemetry, or hosted storage.
- Dependence on one language, one agent, or one vendor.

---

## 5. Feasibility

### 5.1 Integration method [New]

Both agents have lifecycle hooks. winnow uses these hooks.
winnow does not proxy API traffic. winnow does not wrap the terminal.
`winnow claude` is a launcher, not a wrapper.

### 5.2 Claude Code

| Need | Mechanism | Note |
|---|---|---|
| Replace tool output | PostToolUse `updatedToolOutput` | All tools since v2.1.121. **[Verify]** |
| Block or ask | PreToolUse `permissionDecision`: allow, deny, ask | |
| Rewrite input | PreToolUse `updatedInput` | |
| Add context | `additionalContext` | Stays in the transcript. |
| See failed calls | PostToolUseFailure | |
| Stop compaction | PreCompact, decision `block` | Trigger: `manual` or `auto`. **[Verify]** |
| Send events to daemon | Hook type `http` | **[Verify]** |

### 5.3 Codex

Source: official Codex hooks documentation.

| Need | Mechanism | Note |
|---|---|---|
| Events | SessionStart, PreToolUse, PermissionRequest, PostToolUse, PreCompact, PostCompact, UserPromptSubmit, SubagentStart, SubagentStop, Stop, Interrupt, SessionEnd | |
| Replace tool output | PostToolUse `decision: "block"` | Codex replaces the tool result with the hook feedback. `updatedMCPToolOutput` is not supported. |
| Rewrite Bash input | PreToolUse `updatedInput` with `permissionDecision: "allow"` | Alternative to block-replace. |
| Block | PreToolUse `permissionDecision: "deny"` or exit code 2 | |
| Ask | Not supported | Hook run fails. Tool call continues. |
| Failed Bash commands | PostToolUse | Runs after non-zero exit. |
| Trust | Review per hook | Trust is stored by hash. A changed hook is skipped until trusted. |
| Hook output size | ~2,500 tokens default | Larger output goes to disk. The model gets a preview. |
| Coverage | Shell, apply_patch, MCP, local function tools | Hosted tools (example: WebSearch) are not hooked. Some paths can opt out. |

**Result:** Codex hooks are a guardrail, not an enforcement boundary.

### 5.4 Capability matrix

| Capability | Claude Code | Codex | Main difficulty |
|---|---|---|---|
| Observe | Hooks + transcript | Hooks + transcript | Transcript format is not a stable interface. |
| Shape | Native | Block-replace or command rewrite | Do not hide data the agent needs. |
| Steer | `additionalContext` | `additionalContext`, Stop continuation | Timing. Nudges are permanent. |
| Block | deny, ask | deny only | False positives. |
| Snapshot | winnow, in git | winnow, in git | Do not change user git state. |
| Adapt | winnow logic | winnow logic | Low signal in one session. |

### 5.5 Corrections to the handoff [New]

| Handoff statement | Correction |
|---|---|
| "Never edit the past" is a hard problem and a differentiator. | Hooks act before content enters the conversation. The principle holds by design. Other hook tools also have it. |
| Steering "never alters the stored conversation." | Not possible with hooks. Injected text stays in the transcript. Keep nudges short and rare. |
| Shaping makes long output short. | Agents already truncate. winnow value is faithful reduction: keep all failures, remove repetition. |
| Jev is the reference decider. | Not verified. Keep it out of the v1 critical path. |

---

## 6. Landscape

| Tool | Function | Overlap |
|---|---|---|
| RTK | Hook rewrites Bash commands. Per-command filters for 100+ commands. Less than 10 ms overhead. Savings statistics. Scans old sessions for missed savings. Does not cover Read, Grep, or Glob. | High (Shape) |
| context-mode | MCP server. Sandbox tools keep raw data out of context. Savings statistics. | Medium (Shape) |
| curb | Local watchdog. Reads session logs. Policy loop. Local ledger. | Medium (Observe) |
| AgentGov, fettle, Reins | Governance and harness tools. | Not assessed |

**Differentiators.** No tool in this list does all of these:

- Detect the same underlying failure across different text.
- Detect test weakening.
- Tune output reduction from evidence that the agent needed hidden data.
- Explain and undo each action, across agents.

**Option.** Accept RTK-style filters as one reduction backend. Put winnow effort into adaptation and protection.

---

## 7. Risks

| Risk | Effect | Mitigation |
|---|---|---|
| Agent hook APIs change | Connector breaks | Thin connectors. Conformance tests per agent version. |
| Low signal in one session | Adaptation is noise | Few knobs. Bounded steps. Optional repo profile later. |
| Test guard false positives | Users disable it | Look mode first. Override feeds strictness. Measure false-positive rate. |
| Reduction hides needed data | Agent fails | Keep failures verbatim. Full output is always retrievable. A/B gate. |
| Hook latency | Every tool call is slower | Daemon + small shim. Target p95 < 20 ms. Fail open. |
| Codex trust review | Friction at each change | Install stable hooks once. |
| winnow sees all output | Data exposure | Local only. Outbound deciders off by default. Redaction in broker. |

---

## 8. Architecture

### 8.1 Process model [New]

```
 user ──> winnow claude|codex  (launcher)
              │ starts
              ▼
          winnowd  (one daemon per session, Unix socket)
              ▲
              │ JSON over socket (or HTTP on Claude Code)
              │
          winnow hook <event>  (shim, called by agent hooks)
              ▲
              │ stdin JSON / stdout JSON
              │
          Agent (Claude Code, Codex)
```

| Part | Function |
|---|---|
| Launcher | Creates the session directory. Starts the daemon. Starts the agent with `WINNOW_SESSION` set. Claude Code: injects hooks through a generated settings file. **[Verify flag]** |
| Codex setup | `winnow init codex` installs stable user-level hooks once. The user trusts them once. |
| Shim | Sends hook input to the daemon. Prints the reply. If `WINNOW_SESSION` is not set, it does nothing. If the daemon does not reply in time, it exits 0. |
| Daemon | Holds the session model, controller, broker, and ledger. |

**Language.** Rust or Go. Requirement: one static binary, millisecond startup. Do not use Node or Python for the shim.

### 8.2 Layers

```
 Agent ⇄ Connector ⇄ Perception → Controller → Executor
                          │          ▲   │
                          ▼          │   ▼
                     Session model ◄─┘  Judgment broker ⇄ Deciders
                          │               │
                          └──────► Ledger ◄┘
```

| Layer | Function |
|---|---|
| Connector | Translates agent events to the common vocabulary. Applies actions. Declares capabilities. |
| Perception | Turns events into facts: command kind, error signature, output size, test file change. |
| Session model | Small, readable state. Built from the ledger. |
| Controller | Loop: sense, judge, decide, act, observe. Owns thresholds and adaptation. |
| Judgment broker | Owns the question catalog. Routes each question to a rule or a decider. Validates, applies deadlines and fallbacks, redacts, freezes. |
| Executor | Applies actions through the connector. In look mode, it records only. |
| Ledger | Append-only record. Single source of truth. |

### 8.3 Two speeds

The fast path never waits for a decider.
A late decider answer does not change the action. The rule answer applies.
The late answer goes to the ledger and the session model.

---

## 9. Data model

### 9.1 Ledger

- One SQLite file per session. WAL mode. Append-only.
- Row: `seq, ts, kind, payload, causes[]`.
- `causes[]` links each row to the rows that caused it. `winnow why` follows these links.

### 9.2 Blob store

- Full tool outputs, stored by content hash, compressed.
- Each reduced output points to its blob.

### 9.3 Events (connector → core)

| Event | Fields |
|---|---|
| `SessionStarted` | agent, version, cwd, mode |
| `ToolRequested` | tool, input |
| `ToolCompleted` | tool, output_ref, exit, duration |
| `FileEdited` | path, before_hash, after_hash, diff |
| `PromptSubmitted` | text_ref |
| `TurnEnded` | — |
| `Compacted` | trigger |

### 9.4 Decision records (core output)

| Record | Fields |
|---|---|
| `Judgment` | question, version, answer, source (rule or decider), fallback flag |
| `Action` | kind, target, reason, undo_ref, applied (true in optimize, false in look) |
| `Adaptation` | knob, from, to, reason |
| `Override` | action_ref, user choice |

### 9.5 Capability declaration

Each connector declares these fields. The controller plans only with declared capabilities.

| Field | Claude Code | Codex |
|---|---|---|
| `replace_output` | yes | via block |
| `rewrite_input` | yes | yes |
| `deny` | yes | yes |
| `ask` | yes | no |
| `inject_context` | yes | yes |
| `continue_turn` | yes (Stop) | yes (Stop) |
| `block_compaction` | yes | yes |
| `hook_output_limit` | none known | ~2,500 tokens |

---

## 10. Components

### 10.1 Perception

**Command classifier.** Regex table per ecosystem. Kinds: test, build, install, git, search, other.

**Output parsers (v1).** pytest, jest/vitest, cargo test, go test, tsc, eslint, generic.

**Error-signature normalizer.** This is the most important code in v1.

1. Remove paths, line numbers, memory addresses, timestamps, temporary names.
2. Keep error class, normalized message, top N stack frames (function names only).
3. Hash the result.

Same hash = same failure. This covers most cases of "different text, same error" without a model.

### 10.2 Shaping

- v1 reduces Bash output only. Do not reduce source file reads.
- Reduce only above a size threshold (initial value: 2,000 tokens; tune in Phase 2).
- Keep each failure verbatim, with a per-failure line limit.
- Replace passing tests with counts.
- Remove repeated stack traces. Show one copy and a count.
- Remove progress bars and ANSI codes.
- Add a footer to each reduced output:

```
[winnow: 4,812 lines → 37 · full output: winnow show o-17]
```

### 10.3 Session model and adaptation

| Part | Content |
|---|---|
| Repo and task | Work kind, tools, noisiest commands |
| Agent | Failure recovery, re-open rate, loop pattern |
| User | Overrides, undos, accepted and ignored actions |

**Knobs (v1).**

| Knob | Range | Signal |
|---|---|---|
| Shape level, per output kind | 0–3 | Re-opens |
| Loop sensitivity | low, medium, high | Steers that the agent ignores or the user reverses |
| Test guard strictness | warn, ask, deny | User approvals and overrides |

**Re-open loop [New].** The agent gets full output with `winnow show <id>` through Bash. winnow sees each re-open.

- Re-open within K turns → loosen that output kind one level.
- No re-open after N reduced outputs of a kind → tighten one level.
- Each change writes an `Adaptation` row with a one-sentence reason.

Scope is the session. Nothing carries over. A repo profile is an option for later.

### 10.4 Loop detection

A loop is one of these:

- The same error signature after 3 or more **different** edits.
- A file hash cycle: edit, revert, edit.

Failures at the start of a session are normal. They do not count.

### 10.5 Steering

- One sentence. Example: "This error is unchanged after 3 different edits."
- One steer per error signature.
- Nudges stay in the transcript. Use them rarely.

### 10.6 Test guard

**Signals** (edits to test paths):

- Assertion removed.
- Skip, xfail, or `only` marker added.
- Tolerance increased.
- Expected value changed to the actual value from the last failure.
- `|| true` added to a test script.

**Behavior by mode and agent.**

| Mode | Claude Code | Codex |
|---|---|---|
| Look | Log | Log |
| Optimize | Ask | Deny with reason, or warn |

A user override changes strictness for the session.

### 10.7 Snapshots and undo

- Write snapshots as private git refs: `refs/winnow/<session>/<n>`.
- Build them from a temporary index.
- Do not touch the working tree, index, or stash of the user.
- `winnow undo <id>` restores from the ref.

### 10.8 Judgment broker and question catalog

v1 implements rule answers only. Every question and answer goes to the ledger.

| Question | Answer type | v1 source | Decider later? |
|---|---|---|---|
| `same_failure(a, b)` | same, different, unsure | Normalizer | Yes, if rules lose in evaluation |
| `test_weakened(diff)` | weakened, neutral, strengthened, unsure + confidence | Rules | Yes, if rules lose in evaluation |
| `output_reducer(kind, sample)` | reducer id | Rules | Only for unknown kinds, once per kind |
| `session_phase` | exploring, implementing, debugging, verifying | Rules | Not planned |
| `nudge_kind` | choice | Rules | Not planned |

Block, ask, or allow is policy applied to these answers. It is not a question.

### 10.9 Deciders

**Contract.** winnow owns it. Each question is a versioned type with a bounded view of state and a typed answer: choice, score, or probability.

**Safeguards.**

- Advisory only for destructive actions (block, rollback).
- Session content is untrusted. Typed, bounded answers limit damage.
- Outbound calls only to deciders that the user enables. Self-hosted deciders keep data local.
- Deadline, rule fallback, and validation on every call.
- Each question, answer, and fallback goes to the ledger. Decisions are frozen.

**Offline evaluation [New].** Run a decider on recorded questions from the ledger. Compare with the rule answers. Enable a decider only where it is better than the rules.

---

## 11. Modes

| Mode | Function | Limit |
|---|---|---|
| Look | Observe. Report what winnow would do. Change nothing. | Cannot show whether hidden data would hurt the agent. |
| Optimize | Act. Start with conservative, reversible actions. | — |

Both modes use the same pipeline. Only the executor is different.

**Retro look [New].** Run look mode on existing transcripts. The user gets a report without a change to the workflow.

---

## 12. CLI reference

| Command | Function |
|---|---|
| `winnow claude [args]` | Start Claude Code under winnow. |
| `winnow codex [args]` | Start Codex under winnow. |
| `winnow init codex` | Install stable Codex hooks once. |
| `winnow mode look\|optimize` | Change mode in a live session. |
| `winnow report [session]` | Show tokens, noise, cache, loops, would-have actions. |
| `winnow show <output-id>` | Show full original output. |
| `winnow why <action-id>` | Show the cause chain of an action. |
| `winnow undo <action-id>` | Reverse an action. |
| `winnow replay <session> --policy <file>` | Run a recorded session against a different policy. |

---

## 13. Example report (look mode)

```
Session 7f3a · Claude Code · 41 min · look mode
Tokens: 412k   Noise: 38% (est.)   Cache read: 81%
Would reduce: 23 outputs (−131k tokens est.)
Loops: 1 — "AssertionError: expected 200" unchanged across 5 edits (22 min)
Test guard: 1 flag — tests/test_api.py: expected value changed to actual
```

---

## 14. Plan

| Phase | Scope | Exit criteria |
|---|---|---|
| 0. Spikes (1–2 weeks) | Test every **[Verify]** item. Measure shim latency. Test Codex block-replace vs command rewrite. Find token and cache fields in transcripts. Check name on GitHub, crates.io or Go module path, npm, PyPI, domain. | Section 5.4 contains tested facts only. |
| 1. Look mode, Claude Code | Launcher, shim, daemon, ledger, perception (3–4 ecosystems), loop detection, would-have estimates, `report`, retro look. | Owners of ~20 real sessions agree the reports are correct. |
| 2. Optimize + Codex | Shaping with `show`, re-open adaptation, loop steering, Codex connector. | A/B gate passes (section 15). |
| 3. Protect | Test guard, snapshots, `undo`, `why`. | Testers keep the guard on. False-positive rate is measured and accepted. |
| 4. Deciders | Contract, offline evaluation, one self-hosted adapter. | Decider is better than rules on the target questions. |

**First task.** Write the error-signature rules for pytest and jest. Loop detection and `same_failure` depend on them.

---

## 15. Evaluation

**Offline.** Replay recorded sessions. Estimate savings and would-have actions.

**Online A/B.** 30–50 fixed tasks. Run each with and without winnow.

| Metric | Target |
|---|---|
| Task success | No decrease (gate) |
| Total tokens | Decrease |
| Cache read ratio | No decrease |
| Wall time | Decrease or equal |
| Re-open rate | Low and stable |
| Time in loops | Decrease |

Replay alone cannot prove that reduction is safe. The A/B gate is mandatory.

---

## 16. Decision log

| Decision | Status |
|---|---|
| Name: winnow, used as a prefix command | Decided (availability not verified) |
| Open-source tool first; research as an option | Decided |
| Clean core plus connectors; first: Claude Code, Codex | Decided |
| Two modes: look, optimize | Decided |
| Session adapts to the user; scope is the session | Decided |
| Only high-value semantic judgments go to deciders | Decided |
| Layered core | Decided |
| Core calls no model; deciders are plugins | Proposed |
| Deciders judge; they do not act | Proposed, leaning accepted |
| winnow owns the decider contract; Jev is one adapter | Proposed |
| Integrate through agent hooks; launcher, not wrapper | **New, proposed** |
| Fail open | **New, proposed** |
| v1 reduces Bash output only | **New, proposed** |
| Nudges are permanent; use rarely | **New, proposed** |
| Deciders go live only after offline evaluation | **New, proposed** |
| Lead with loops and honesty; tokens second | **New, proposed** |

---

## 17. Open questions

**Design**

- Ask is not available on Codex. Is warn an acceptable substitute?
- Should a repo profile carry over between sessions (opt-in)?
- Can deciders ever propose actions?
- Shape Read/Grep/Glob output in a later phase?

**Product**

- Interoperate with RTK filters, or compete?
- Final positioning against curb and context-mode?
- Is the name available?

---

## 18. Glossary

| Term | Meaning |
|---|---|
| Session | One run of an agent on a repo and task. |
| Connector | Agent-specific adapter for events and actions. |
| Shim | Small binary that agent hooks call. |
| Daemon | Per-session process that holds winnow state. |
| Decider | External service that answers typed questions. |
| Question catalog | The set of typed judgments. |
| Frozen decision | A decision that never changes after it is made. |
| Ledger | Append-only record of all events and decisions. |
| Error signature | Hash of a normalized error. |
| Re-open | Agent request for the full output of a reduced output. |
| Look / optimize | Observe-only mode / acting mode. |

---

## 19. Sources

- Codex hooks (official): https://developers.openai.com/codex/hooks
- Claude Code hooks (secondary; confirm with official docs): https://claude.nagdy.me/learn/hooks/
- RTK: https://github.com/PicoJr/rtk
- context-mode: https://github.com/mksglu/context-mode
- curb: https://github.com/misty-step/curb
