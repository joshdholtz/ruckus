# Agent adapters — design & plan of record

**Goal:** make Ruckus *understand and drive* each coding agent, not just watch its
terminal. Turn today's heuristic "this pane is quiet, maybe waiting" into exact
state — **working / awaiting-approval / awaiting-input / done** — carrying *what*
it's blocked on (`Bash(rm -rf …)`, a file diff), and let you **approve / deny /
answer / interrupt from the sidebar** (from any attached client, including a
relay laptop — see [RELAY.md](RELAY.md)).

Status: **AA0–AA3 implemented** (Claude observe + approve-from-sidebar, Codex
notify-observe, and the TUI approval cards + keybinds). AA4 opencode/Cursor, AA5
Codex app-server, AA6 scopes pending. Grounded in the current tree + a survey of
Claude Code, Codex, opencode, and Cursor integration surfaces (2026).

## Implemented surface

- `ruckus agent-hook <agent> [--gate]` — the universal adapter handler. Runs
  inside a pane; reads `RUCKUS_PANE` + the agent's hook JSON on stdin; reports
  exact state via `ReportAgentState`. `--gate` blocks on `AwaitDecision` and
  prints Claude's `permissionDecision` so you can approve/deny from the sidebar.
- `ruckus agent-setup <agent> [--gate] [--write <settings.json>]` — print (or
  idempotently merge) the hook config into Claude's settings / Codex's config.
- `ruckus resolve <pane> <request_id> allow|deny|escalate` — resolve a pending
  approval (what the TUI's approve/deny button will call).
- New protocol: `AgentState`/`AgentPhase`/`Approval`/`Decision`,
  `PaneInfo.agent_state`, `ReportAgentState`/`AwaitDecision`/`ResolveDecision`,
  `AgentState`/`Decided` events, a pending-decision registry on the daemon. All
  pane-scoped, so approvals raised on a device resolve from any relay client.

Verified end-to-end: a blocked Claude `PreToolUse` gate surfaces the approval
(`Bash(rm -rf …)`), `resolve allow` propagates back as
`permissionDecision:allow`.

- **TUI (AA3):** the NEEDS-YOU queue shows a pane's pending approval
  (`⚠ Bash(rm -rf …)`) with `⌥y approve · ⌥r deny`; `approve_agent`/`deny_agent`
  actions (default `alt-y`/`alt-r`, also in the palette) resolve the focused
  pane's request. `ServerMsg::AgentState` updates the view live. The card lives
  in the NEEDS-YOU section, so enable it:
  `[ui] sidebar_sections = ["needs_you", "spaces"]`. (The `alt-y`/`alt-r` keys
  work regardless of whether the section is shown.)
- **Mobile (AA3):** on a narrow terminal, tapping into a blocked pane shows big
  `[approve] [deny]` tap chips in the action bar (beside `type…`/`next`).
- Verified via pyte-rendered PTYs (desktop card + mobile chips): both render and
  the daemon reports `awaiting_approval`.

**Pending:** AA4 (opencode/Cursor via the same `agent-hook`), AA5 (Codex
`app-server` for real state + interrupt/steer), AA6 (capability scopes gating
remote control). Everything below is the design of record.

## What Ruckus does today (the clean slate we build on)

Agent-awareness is **fully generic and heuristic** — there is *no* per-agent code
path; `claude` and `codex` hit identical machinery.

- **Heuristic classifier** `classify_tail` (`daemon.rs:716-773`) ANSI-strips the
  screen tail and matches phrases (`"do you want to proceed"`, `"esc to
  interrupt"`, `"(y/n"`, `"❯ 1."`, trailing `?`/`╯`) into the 4-state `Activity`
  enum (`protocol.rs:191-202`). It already catches Claude/Codex approval prompts
  as `Waiting` — it just can't say *what's* being approved.
- **Two push-in seams (the write API an adapter uses):**
  `Request::ReportActivity{pane,state}` (`protocol.rs:105`) and
  `Request::ReportAgent{pane,name}` (`protocol.rs:111`), via `apply_report`
  (`daemon.rs:2269`). Crucially, a report sets `p.reported` which **freezes the
  heuristics for that pane** (`daemon.rs:1055,2457`) — so an adapter cleanly
  *overrides* guessing with truth. CLI: `ruckus report-activity` / `report-agent`.
- **Callback seam:** every pane gets `RUCKUS_SOCK`/`RUCKUS_DIR`/`RUCKUS_PANE`
  (`daemon.rs:2124-2130`). A process *inside* a pane can reach the daemon and
  call the full protocol — so an agent's own hook can report in, keyed by
  `RUCKUS_PANE` (no fragile session-id correlation needed on our side).
- **Status model** `PaneInfo` (`protocol.rs:419-444`): has `agent: Option<String>`
  (display-only today) but **no** structured agent state — no tool name, no
  approval detail, no model/turn/token fields. That's the gap a per-agent UI
  needs filled.
- **Reactive seam** `[[hook]]` (`config.rs:1023`, `spawn_hook` `daemon.rs:627`)
  fires a command on Ruckus's *own* activity transitions — outbound, the inverse
  of an adapter. Not reused here except as prior art.

## Key finding: agents split into TWO adapter classes

They do **not** fit one shape (this is the central design fact):

| Agent | Integration surface | Class | Approval signal? | Approve/drive from outside? |
|---|---|---|---|---|
| **Claude Code** | `command` hooks → callback over `RUCKUS_SOCK` | **A: in-pane hook** | ✅ `PreToolUse`/`PermissionRequest` carry tool+args | ✅ hook returns `allow`/`deny`/`escalate` |
| **opencode** | TS plugin API (`permission.asked`/`replied`, `session.idle`) | **A: in-pane hook** | ✅ | ✅ |
| **Cursor CLI** | `~/.cursor/hooks.json` (Claude-style) | **A (partial)** | ⚠️ shell events only; buggy/unreliable now | ⚠️ observe-only for now |
| **Codex** | `codex app-server` JSON-RPC control channel | **B: control-channel** | ✅ `*/requestApproval` (only via app-server — the TUI emits nothing) | ✅ `accept`/`decline`, `turn/interrupt`, `turn/steer` |

- **Class A — in-pane hook:** the normal interactive agent runs in the pane; its
  native hooks call back over `RUCKUS_SOCK`. Ruckus stays "a terminal the agent
  runs inside." Cheap, high-value, the MVP.
- **Class B — control-channel:** Codex's TUI is a **dead end** — its only hook
  (`notify`) fires once, on `agent-turn-complete`, with no approval signal. Real
  state + control requires backing the pane with `codex app-server` (bidirectional
  JSON-RPC: `turn/started`, `*/requestApproval`, `turn/completed`, `turn/interrupt`,
  `turn/steer`). Different shape, bigger lift → staged later. *(Method names are
  an evolving surface — pin to the target `codex` version and confirm against a
  live capture before shipping.)*

## The common model (new core types)

One model both classes normalize into, complementing (not replacing) `Activity`:

```rust
enum AgentPhase { Working, AwaitingApproval, AwaitingInput, Done, Error }

struct AgentState {
    agent:   String,            // "claude" | "codex" | "opencode" | ...
    phase:   AgentPhase,
    session: Option<String>,    // agent's own session/thread id (dedup/telemetry)
    pending: Option<Approval>,  // Some(..) iff phase == AwaitingApproval
    summary: Option<String>,    // last assistant line / "what it's doing"
    model:   Option<String>,    // optional telemetry
}

struct Approval {
    request_id: String,         // correlates the resolve
    kind:  ApprovalKind,        // Command | FileChange | Tool
    title: String,              // "Bash(rm -rf /tmp/*)"  — one-line for the queue
    detail: String,             // full command / diff summary / tool args
}
enum ApprovalKind { Command, FileChange, Tool }
enum Decision { Allow, Deny, Escalate }   // Claude allow/deny/escalate ≡ Codex accept/decline/cancel
```

**Backward-compat mapping** onto the existing 4-state `Activity` (so all current
rendering, dots, and the NEEDS-YOU queue keep working unchanged):
`Working→Working`, `AwaitingApproval|AwaitingInput→Waiting`, `Done→Done`,
`Error→Done`. The structured `AgentState` rides alongside for the richer UI.

## The adapter contract

Two directions. **Observe** is low-risk and ships first; **control** is staged.

**Inbound (observe) — generalizes `ReportActivity`:**
```rust
Request::ReportAgentState { pane: u64, state: AgentState }   // adapter → daemon
ServerMsg::AgentState      { pane: u64, state: AgentState }   // daemon → all clients
```
Sets `p.reported` (freezing heuristics) *and* stores `p.info.agent_state` (new
`PaneInfo` field), then broadcasts. Reuses the exact `apply_report` path
(`daemon.rs:2269`).

**Control (approve / answer / interrupt) — new pending-decision registry:**
```rust
Request::AwaitDecision   { pane, request_id }              // in-pane hook BLOCKS here
Request::ResolveDecision { pane, request_id, decision }    // any client (TUI) → daemon
```
The daemon holds a `pending: HashMap<(pane,request_id), oneshot::Sender<Decision>>`.
Flow for a Class-A blocking hook:

```
 agent hits a tool          in-pane hook (ruckus agent-hook)         Ruckus daemon           attached client(s)
      │  PreToolUse ────────────▶│                                       │                          │
      │                          │── ReportAgentState{AwaitingApproval,   │                          │
      │                          │        Approval{Bash(rm…)}} ─────────▶│── AgentState ───────────▶│ shows card in NEEDS YOU
      │                          │── AwaitDecision{pane,req} ────────────▶│ (parked in `pending`)    │
      │                          │        ⋯ blocks ⋯                     │◀── ResolveDecision(Allow)─│ user taps Approve
      │                          │◀────────── Decision{Allow} ───────────│ (fulfills oneshot)       │
      │◀─ hook prints {allow} ───│                                       │                          │
      │  tool runs               │                                       │                          │
```

`AwaitDecision`'s *reply* is the decision — it reuses the existing seq-based
request/response (`client.rs:45-70`); the daemon just answers late. On hook
**timeout** (Claude hooks are time-boxed) the adapter defaults to `Escalate`, so
the agent falls back to its own in-pane prompt and **nothing is ever wedged** —
you can still approve directly in the terminal.

**Cross-machine, for free:** the registry lives in the *daemon*, and
`ResolveDecision` is an ordinary request that routes through the remote hub like
any other. So an approval raised on the always-on Mac Mini shows up — and can be
resolved — from a relay-attached work or personal laptop, with no extra work.
This is why adapters always run **on the device where the agent runs**, and the
control loop funnels through that device's daemon.

## Class A — in-pane hook adapters

Ship a single universal handler: **`ruckus agent-hook <agent>`** (new internal
subcommand beside `report-activity`, `main.rs:104`). It reads `RUCKUS_PANE` from
env + the agent's hook JSON from stdin, normalizes to `AgentState`, calls
`ReportAgentState`, and — for approval hooks — calls `AwaitDecision` and prints
the agent's native decision JSON. The user's agent config just points hooks at it.

**Claude Code** — `~/.claude/settings.json` (or project `.claude/settings.json`):
- **State (non-blocking):** `PreToolUse`/`PostToolUse` → Working; `Stop` →
  AwaitingInput/Done; `Notification` → attention. Each hook payload carries
  `session_id`, `tool_name`, `tool_input`, `cwd`.
- **Approve-from-sidebar (blocking):** use **`PreToolUse`** — it can block *and*
  return `{"hookSpecificOutput":{"permissionDecision":"allow|deny|escalate"}}`.
  The handler reports `AwaitingApproval` with `Approval{title:"Bash(…)", detail}`
  then blocks on `AwaitDecision`; the returned `Decision` becomes the
  `permissionDecision`. *(`PreToolUse` is the reliable blocking+decision hook;
  `PermissionRequest` is a good non-blocking state signal. Exact blocking
  semantics vary by Claude version — verify against the target before shipping
  the control path; the observe path is safe regardless.)*

**opencode** — its TS plugin API is the richest of all: `permission.asked` /
`permission.replied` (approval + resolution), `session.idle` (done),
`tool.execute.before/after`. Same `ReportAgentState` normalization; approvals via
the plugin's reply mechanism.

**Cursor CLI** — `~/.cursor/hooks.json` mirrors Claude's shape
(`beforeShellExecution`, `stop`, `preToolUse`…), but is reportedly buggy (hooks
not firing on some platforms). **Ship observe-only**, upgrade to control when it
stabilizes.

## Class B — control-channel adapter (Codex)

Back the pane with `codex app-server` (`--listen unix://` or stdio) and speak
JSON-RPC 2.0:
- **State:** `turn/started`→Working, inbound `item/commandExecution/requestApproval`
  or `item/fileChange/requestApproval`→AwaitingApproval (the request *is* the
  signal + carries the command/diff), `turn/completed`→AwaitingInput/Done.
- **Control:** reply `accept`/`decline`/`cancel` to a `requestApproval`;
  `turn/interrupt` (stop), `turn/steer` (answer/redirect mid-turn).
- `thread.sessionId` is the stable id.

Open UX question: does the pane show Codex's own TUI (display) while Ruckus drives
a *parallel* app-server session (control), or does Ruckus render the interaction
itself? The former is simpler but the two aren't the same session; the latter is
a real front-end build. **Interim cheap win:** Codex's `notify` (turn-complete
edge) + today's heuristic classifier already gives "needs you" — so Codex users
get *something* from day one, with app-server richness staged as AA5.

## UI (per-agent, terminal-only)

- **NEEDS-YOU queue** upgrades from "● claude·2 (waiting)" to a **card** with the
  `Approval.title` and `[a]pprove / [d]eny / [e]scalate` actions; the current
  agents list (`tui.rs:1519-1550`, render `:4119-4153`) and `{agent}` row token
  (`:4709`) get an agent-state glyph + summary.
- **Pane header:** agent name + phase (e.g. `claude · awaiting approval`), model if
  known.
- **Actions & keys:** approve/deny/answer/interrupt bound in the keymap and the
  command palette; on the **mobile deck**, the card's buttons are tap targets —
  approve a command from your phone.
- **Answer/steer:** for AwaitingInput, a quick-reply that sends `Input` (Class A)
  or `turn/steer` (Class B).

## Security

The in-pane socket is **unauthenticated** and plugin capabilities are **declared
but unenforced** (`config.rs:1067`, README:163). Class-A adapters rely on that
open access — convenient, but it means *any* local process could inject a
`ResolveDecision` or a bogus `AgentState`. For an MVP on machines you control this
is the same trust model as today. Two notes carried forward:
1. **Approval detail crosses the relay** (command text, diffs) — already true of
   all pane content, but worth stating: don't relay through a broker you don't
   trust (see RELAY.md's Phase-4 E2E note).
2. The **control** path (remote approve) is exactly where the roadmap's
   deny-by-default **capability scopes** should land first — gate
   `ResolveDecision`/`AwaitDecision` behind a scoped token before this is exposed
   beyond a trusted LAN.

## Config

```toml
[agents]
enable  = ["claude", "codex", "opencode"]   # which adapters to activate
control = true                              # allow approve/deny/answer from Ruckus (else observe-only)
install_hooks = true                        # auto-write the agent hook config (else print instructions)
```
`install_hooks` writes the `ruckus agent-hook <agent>` entries into each agent's
config (merging, never clobbering) and can `--dry-run`. Class-B (`codex`)
launches app-server behind the pane when Ruckus spawns a `codex` command.

## Reuse vs. new core

- **Reused as-is:** the `apply_report` freeze-heuristics mechanism
  (`daemon.rs:2269,1055`), pane env seam (`daemon.rs:2124`), seq request/response
  (`client.rs:45`), the NEEDS-YOU queue + dots, the remote hub (relay clients get
  approvals free).
- **New core:** `AgentState`/`Approval`/`Decision` types; `PaneInfo.agent_state`
  field; `ReportAgentState`/`AwaitDecision`/`ResolveDecision` requests +
  `ServerMsg::AgentState`; the `pending` registry; `ruckus agent-hook`; per-agent
  UI widgets (README's "plugin-rendered pane UI" is unchecked `:190`, so this is
  core, not a plugin).
- **Ships as plugin (optional later):** the *hook install* + a Codex app-server
  shim could be packaged as installable adapters once core lands.

## Test harness (no real agents)

Mirrors `tests/daemon.rs`. A **fake agent-hook** binary/script feeds canned
Claude/Codex payloads to `ruckus agent-hook` against an isolated `RUCKUS_DIR`
daemon; assert `AgentState` broadcasts, the NEEDS-YOU card, and the full
`AwaitDecision`→`ResolveDecision`→decision round-trip — deterministic, no network,
no real Claude/Codex. Class-B adds a stub app-server speaking scripted JSON-RPC.

## Phases (each: tests first, land green)

| phase | builds | tests |
|---|---|---|
| **AA0** | `AgentState`/`Approval`/`Decision` types; `PaneInfo.agent_state`; `ReportAgentState` + `ServerMsg::AgentState`; Activity mapping | serde round-trip; heuristic still works when no state reported |
| **AA1** | `ruckus agent-hook claude` (observe-only) + settings.json install (`[agents]`, `install_hooks`) | fake Claude payloads → correct phase/summary; NEEDS-YOU shows `Bash(…)` title |
| **AA2** | pending-decision registry + `AwaitDecision`/`ResolveDecision`; Claude `PreToolUse` approve/deny/escalate; timeout→escalate | full round-trip approve **and** deny; timeout falls back to in-pane prompt |
| **AA3** | per-agent UI: NEEDS-YOU cards + actions, pane header, keymap/palette bindings, deck tap targets | render snapshot tests; key + click resolve a pending approval |
| **AA4** | opencode adapter (permission.asked/idle) + Cursor observe-only; all Class-A via the same `agent-hook` | opencode approve round-trip; cursor state-only |
| **AA5** | Codex Class-B: `app-server` shim — state (`requestApproval`) + control (`interrupt`/`steer`/approve); interim `notify` edge | stub app-server: block→approve; interrupt |
| **AA6** | fmt/clippy/docs; **capability scopes** gating `ResolveDecision`/`AwaitDecision`; auto-install `--dry-run` polish | scope denies un-tokened control; full suite green |

## Open questions

- **Codex pane UX** (AA5): show Codex's TUI + parallel app-server control, or let
  Ruckus render the session? Leaning "TUI for display, app-server for state +
  control" as the pragmatic first cut, accepting they're distinct sessions.
- **Auto-install intrusiveness:** writing into a user's `~/.claude/settings.json`
  / `~/.codex/config.toml` is invasive. Default to `--dry-run` + print, opt in to
  auto-write? (Leaning yes.)
- **Where control gets gated first:** ship approve-from-sidebar LAN/relay-only
  behind the shared-secret (RELAY.md) until capability scopes (AA6) exist — or
  block remote control until scopes land? (Leaning: observe remotely now, control
  remotely only after scopes.)
