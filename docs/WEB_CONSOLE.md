# Web console — design & plan of record

**Goal:** a custom, normalized, consistent-across-agents **web UI** for your agent
sessions — a searchable timeline + a session detail view with inline
approve/deny/reply — reachable from a browser or phone. This is Longhouse's
actual core surface (its `/timeline` web UI + per-provider ingest parsers), and
the biggest real gap between Ruckus and Longhouse.

**Non-negotiable:** **the terminal TUI does not change.** The web console is an
*additive, parallel* front-end over the same daemon. The raw-terminal view stays
byte-for-byte what it is today; the web view is a second lens for people who want
the clean chat/timeline instead of a PTY. Two front-ends, one brain.

Status: **design — not yet built.** Web-first (iOS = a PWA later).

## Why this is very buildable on Ruckus

The daemon already holds everything a web console needs — we're adding a *view*,
not a new backend:

- **Live session data** — the JSON-RPC event stream (`Output`, `AgentState`,
  `Activity`, pane lifecycle) already broadcasts to every client (`daemon.rs`
  `broadcast`). A web client is just another subscriber.
- **Structured agent truth** — our adapters already produce `AgentState`
  (phase + pending `Approval`) per pane. That's the normalized "what's happening"
  the timeline renders.
- **The conversation itself** — the Claude agent-hook already receives
  `transcript_path`, a JSONL of the full conversation (user/assistant/tool
  messages). That's the gold source for a normalized transcript — no scraping.
- **Two-way control** — `ResolveDecision` (approve/deny) and `Input` (reply)
  already exist and route through the hub, so a web click drives a session on any
  machine, including over the relay.
- **Transport-agnostic protocol** — `connect_io` speaks the protocol over any
  stream, so a WebSocket bridge is the same trick that made the relay work.

## Architecture

```
                          ┌───────────────── daemon (unchanged core) ─────────────────┐
  raw terminal  ◀── unix ─┤  panes/PTYs · event stream · AgentState · ResolveDecision  │
     (TUI)               │                          │                                  │
                          │             ┌────────────┴───────────┐                     │
                          │             │  session model + ingest │  (new, additive)   │
                          │             │  parsers (Claude/Codex)  │                    │
                          └─────────────┴────────────┬────────────┘────────────────────┘
                                                     │  JSON-RPC + SessionLog/Append
                                        ┌────────────┴────────────┐
                                        │  `ruckus web` HTTP + WS  │  (new binary path)
                                        └────────────┬────────────┘
                                     localhost:PORT  │  (reach via Tailscale, or relay WS)
                                        ┌────────────┴────────────┐
                                        │   web UI: timeline +     │  (new, self-contained)
                                        │   session detail + reply │
                                        └──────────────────────────┘
```

Everything left of "new, additive" exists and is untouched.

## The normalized session model

One provider-agnostic transcript both parsers emit and the web renders:

```rust
struct SessionMsg {
    seq: u64,
    role: Role,                 // User | Assistant | Tool | System
    kind: MsgKind,              // Text | ToolCall | ToolResult | Approval | TurnStart | TurnEnd
    text: String,               // rendered body / assistant text / tool summary
    tool: Option<ToolRef>,      // name + args + status for ToolCall/Result
    approval: Option<Approval>, // reuse protocol::Approval (pending → inline buttons)
    ts: u64,
}
enum Role { User, Assistant, Tool, System }
enum MsgKind { Text, ToolCall, ToolResult, Approval, TurnStart, TurnEnd }
struct ToolRef { name: String, args: String, status: ToolStatus } // Running|Ok|Err
```

`protocol::AgentState`/`Approval`/`Decision` are reused as-is; this is the
*history/transcript* layer that sits beside them.

## Ingest parsers (per provider)

- **Claude** — tail the `transcript_path` JSONL (we already get the path in the
  agent-hook). Each line is a message/tool event → map to `SessionMsg`. Live tool
  calls + approvals also arrive via the existing hook (`ReportAgentState`), so the
  timeline updates in real time and the transcript backfills detail.
- **Codex** — from `app-server`/`exec --json` events (AA5): `turn.*`,
  `item.*` (agent message / command / file change) → `SessionMsg`.
- **Others / bare CLI** — fall back to the raw PTY scrollback we already keep,
  shown as a terminal block (still ingested, just not richly parsed) — mirrors
  Longhouse's "unmanaged sessions stay observable".

Parsers live daemon-side; the model is exposed over the protocol:

```rust
Request::SessionLog { pane, since: u64 } -> ServerMsg::Session { pane, msgs }
ServerMsg::SessionAppend { pane, msg }   // pushed live
```

## Web server — `ruckus web`

- New subcommand: `ruckus web --listen 127.0.0.1:8080` (a sibling of `ruckus
  relay`). Serves **self-contained** static assets (bundled into the binary via
  `include_str!`/`include_bytes!`) + a **WebSocket** that bridges to the daemon's
  JSON-RPC/event stream (the same `connect_io` protocol, framed over WS).
- No external CDN/deps — one binary, works offline on your tailnet.
- Reach: bind localhost and reach it over **Tailscale** (like Longhouse's
  `localhost:8080`), or tunnel the WS through the **relay** for no-Tailscale
  access. The page is a pure protocol client, so it doesn't care which.

## Web UI

- **Timeline** — all sessions across panes/spaces/machines, state-colored,
  attention-sorted, with **full-text search** (the Longhouse headline).
- **Session detail** — the normalized chat: messages, tool cards, and **inline
  approve/deny buttons** on a pending `Approval` (→ `ResolveDecision`), a **reply
  box** (→ `Input`), later interrupt/steer (AA5). Identical look for every
  provider.
- **"Raw" toggle** — one click drops to the actual PTY (xterm.js over the same
  `Output`/`Attach` stream) so nothing is ever hidden behind the parse. This is
  what keeps us honest: the clean view is a lens, not a cage.
- Tech: vanilla + a tiny render layer, or a small framework bundled at build time
  — decided at WC2.

## Rich interactions — plans, questions, permissions

Agents don't only stream text. Claude presents **plans** (ExitPlanMode),
**multiple-choice questions** (AskUserQuestion), **permission prompts**, and MCP
**elicitations**; Codex has approvals + steer. In the transcript/hooks these are
*typed tool calls with structured payloads*, so the model gets a dedicated kind
and the UI renders the matching control:

```rust
enum MsgKind { Text, ToolCall, ToolResult, Approval, Prompt, TurnStart, TurnEnd }
struct Prompt {
    kind: PromptKind,          // Plan | Choice | Permission | FreeText
    title: String,             // "Approve this plan?" / the question text
    body: String,              // plan markdown / question detail
    options: Vec<Choice>,      // for Choice/Plan: the selectable answers
    answered: Option<String>,
}
struct Choice { label: String, value: String }  // value = what to send back
```

**Parser mapping** (Claude): `ExitPlanMode` → `Plan` (accept/reject + the plan
body), `AskUserQuestion` → `Choice` (render each option as a button),
`PreToolUse`/`PermissionRequest` → `Permission` (approve/deny), `Elicitation` →
`FreeText`/`Choice`. Codex: `requestApproval` → `Permission`, its prompts → the
matching kind.

**Answering — best available channel, and honest about what works** (Longhouse's
own principle: "expose the controls a session can *actually* perform"):
- **Permission** → `ResolveDecision` — clean, programmatic (the gate hook).
- **Plan accept / choice / free text** → `Input` keystrokes into the *real pane*
  (e.g. send `1␍`, or arrow+enter): the PTY is right there, so answering is just
  driving the terminal the user would've driven. Always works as a floor.
- **Codex** → `app-server` structured `steer`/approve replies (AA5).
- **No clean channel** → the **"raw" toggle** drops to the live PTY so you answer
  inline. Nothing is ever un-answerable — worst case you're in the real terminal.

**Capability map:** each provider × interaction advertises which answer channels
it supports; the UI only shows buttons that actually do something (a greyed
"open terminal to answer" otherwise). So a plan from Claude gets Accept/Reject
buttons that send the keystroke; a question gets tappable option chips; a
permission gets true one-tap approve — and anything exotic degrades to the raw
terminal instead of lying to you.

## Search / archive

Longhouse stores history in SQLite on the Runtime Host. Match it: persist
`SessionMsg`s to a SQLite archive on the daemon's box with an FTS index →
"find any past session in seconds." Phase WC4 (in-memory + on-disk log works
before then).

## Reuse vs. new

- **Reused, untouched:** the entire daemon core, event stream, `AgentState`,
  `ResolveDecision`/`Input`, the relay, the TUI (a peer client — unaffected).
- **New:** the `SessionMsg` model + ingest parsers; `SessionLog`/`SessionAppend`
  protocol; `ruckus web` (HTTP + WS + bundled assets); the web UI; (WC4) SQLite
  archive + FTS.

## Phases (each ships something usable)

| phase | builds | proves |
|---|---|---|
| **WC0** | `SessionMsg` model + `SessionLog`/`SessionAppend` protocol; Claude transcript parser (tail the JSONL) | serde tests; a CLI `ruckus session-log <pane>` prints normalized messages |
| **WC1** | `ruckus web` server: static shell + WS bridge to the daemon protocol | open `localhost:8080`, see live pane list + output over WS |
| **WC2** | web **session detail**: normalized chat render + **inline approve/deny** (ResolveDecision) + reply box (Input) | approve a Claude command from the browser; reply to a turn |
| **WC3** | web **timeline**: all sessions, state, live updates; "raw" xterm.js toggle | glance dashboard; drop to real PTY and back |
| **WC4** | SQLite archive + full-text **search** across sessions/machines | search past sessions like Longhouse |
| **WC5** | reach via relay WS tunnel; **PWA** (installable, offline shell) → phone | add-to-home-screen; open from cellular |
| **WC6** | Codex ingest (app-server, needs AA5); web push on `needs_user` | Codex sessions render + notify |

## Open questions

- **Persistence at WC0:** in-memory + on-disk JSONL first, SQLite at WC4 — or
  SQLite from the start? (Leaning: JSONL first, it's cheaper and search isn't
  needed until WC4.)
- **Remote reach:** Tailscale-direct is simplest and needs no new code; the relay
  WS tunnel (WC5) covers non-Tailscale. Ship Tailscale-first?
- **Web framework:** vanilla + a 100-line render loop keeps it self-contained and
  dependency-free; a framework is nicer but heavier to bundle. Leaning vanilla for
  WC1–WC3, revisit if the UI grows.
- **iOS:** PWA (WC5) first; a native app is a separate track only if push/UX
  demands it.
