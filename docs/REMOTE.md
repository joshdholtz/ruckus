# Remote mirror — design & plan of record

**Goal:** connect remote ruckus daemons (over SSH) into one local client, so a
remote box's spaces show in your sidebar and are **fully read/write** — view
panes, send input, split, create, close — exactly like local ones.

Status: **working (R0–R5 landed); migrating the hub from client → daemon (H1–H5).**
Remote spaces mirror into the sidebar and are fully read/write. Connecting is in
the background (a slow/dead SSH never stalls the UI) and dropped remotes
**auto-reconnect**.

## Hybrid redesign (H-phases): the daemon owns the connection

**Why:** ruckus's whole pitch is persistence — "close the window, the ruckus
continues." R0–R5 put the multi-daemon hub in the *client*, so the mirror died
on TUI quit, every attached client opened its own SSH, and the daemon knew
nothing about the remote when no client was attached. The hub belongs in the
**daemon**.

**The one blocker — SSH auth from a detached daemon** — is solved without
fd-passing: the **client hands the daemon its live SSH env** (`SSH_AUTH_SOCK`,
…) on connect; the **daemon spawns and owns the `ssh … ruckus __proxy`** using
those credentials. Agent auth + hardware-key touch work (they're agent-side);
the daemon can even re-dial on its own while the agent socket stays valid, only
needing a client to refresh stale creds. Net: survives TUI quit · one SSH for N
clients · reuses your agent.

**Model:** the daemon becomes a hub that is *also* the local origin (0). It keeps
its real local `State` (owns PTYs) plus a `remotes: BTreeMap<Origin, RemoteConn>`
of mirrored daemons. The pure id layer (`src/remote.rs`) is unchanged — it just
moves from being called in the client to being called in the daemon.

| phase | builds | tests |
|---|---|---|
| **H1** | protocol: `Request::ConnectRemote { host, args, env }` / `DisconnectRemote { origin }` | serde round-trip |
| **H2** | `State.remotes` + `snapshot()` appends each remote's cached (prefixed) snapshot | merge unit tests (local-only unchanged; local+remote ids don't collide) |
| **H3** | daemon connect task (spawn ssh w/ env), per-remote event task (prefix → update cache → broadcast), request routing in `handle_conn` (origin 0 → local; else forward+prefix), disconnect | integ: 2nd `RUCKUS_DIR` daemon via `__proxy`, connect through it, read/write, disconnect |
| **H4** | client reverts to a single local connection: drop `conns`/`route`/multi-merge; `connect remote` sends `ConnectRemote` with the live env; `disconnect remote` sends `DisconnectRemote` | existing daemon integ tests stay green |
| **H5** | fmt/clippy/docs; auto-reconnect from the daemon; stale-cred refresh on next connect | full suite green |

Local pane handling in the daemon is **untouched** by all of this (additive
`remotes` field + one routing branch), so live sessions are never at risk.

### Pre-H (client-side) — original design, retained below for reference

## Using it

The remote box just needs `ruckus` on `$PATH` (it starts its own daemon).

**Runtime (the normal way — remotes are ephemeral):** run the **`connect remote`**
action (command palette, or bind a key to it) → type an ssh host → it mirrors in.
Gone on restart; auto-reconnects if the link drops while running.

**Config (optional — for the ones you always want auto-connected):**

```toml
[[remote]]
host = "workbox"        # anything ssh accepts (alias, user@host)
args = []               # extra ssh opts, e.g. ["-p", "2222"]
```

Either way, the host's spaces appear in your sidebar tagged `workbox: …` and are
view/type/split like local ones. SSH handles auth (mosh is orthogonal — your
link, not the mirror's).

**Disconnect:** the `disconnect remote` action drops the remote whose space
you're currently on — kills its SSH, forgets it (no auto-reconnect), removes its
spaces, and jumps you back to a local space.

## Core decision: origin-encoded `u64` ids

The client uses bare `u64` ids everywhere (`views`, `focused`, `pane_rects`,
`seen`/`unread`/`flash`, drag/select, hit-tables, `Target`/`Jump`, `Node`
equality). Rather than rewrite all of that to a `(host, id)` type, we **pack the
origin into the high bits of the id**:

```
id = (origin << 48) | local_id      // origin 0 = local
origin_of(id) = id >> 48
local_of(id)  = id & 0x0000_FFFF_FFFF_FFFF
```

Each daemon still numbers from 1 (48 bits ≈ 2.8e14 — never exhausted). Ids stay
`u64`, so **all id-consuming code is untouched**. Only three boundaries change,
and each is a pure, unit-tested function (`src/remote.rs`):

1. **ingest** a remote snapshot/event → `prefix_*` adds the origin
2. **egress** a request → `route_request` reads the origin, strips ids to local,
   returns which daemon to send to
3. **response** (e.g. `Created` returns local ids) → `prefix_servermsg` re-adds
   the origin before the client uses them

Local traffic is origin 0, and `pack(0, id) == id`, so **local behavior is
byte-for-byte unchanged** — that's what keeps this safe.

## Architecture

- **Connection registry:** `App.client: Client` → `conns: BTreeMap<Origin, Conn>`
  (Local = 0 always present). Each `Conn` is a `Client` + its event stream +
  host label.
- **Router:** every `self.client.request(req)` → `self.route(req)`:
  `origin_of` the request's primary id, `route_request` strips to local, send to
  that conn, `prefix_servermsg` the reply. No-id requests (`Snapshot`, `Reload`,
  `Upgrade`) go per-conn; `NewSpace` defaults to local.
- **Merged snapshot:** `on_server(State)` **merges** that origin's tree into
  `self.snap` (replace only that origin's spaces/panes) instead of replacing the
  whole thing.
- **Multi-stream loop:** `run()` `select!` gains one arm per conn; each conn has
  its own reconnect (mirrors the existing local reconnect).
- **Transport (SSH):** a hidden `ruckus __proxy` on the remote relays its daemon
  socket over stdio; the local client spawns `ssh <host> ruckus __proxy` and
  speaks the protocol over that pipe. Reuses SSH auth — no ports, works through
  bastions. (mosh is *your* terminal link, orthogonal; the mirror link is SSH +
  auto-reconnect, not mosh-grade roaming.)
- **Config:** `[[remote]] host = "workbox"` (+ optional `ssh_args`) — connected on
  startup, like plugins.

## Change surface (from the audit)

- `App.client`/`events` (single) → registry + multi-arm loop (`tui.rs` `run`,
  `reconnect`, ~20 `self.client.request` sites → `self.route`).
- `on_server`: prefix ids on `State`/`Created`/`Attached`/`Output`/`Exited`/
  `Activity`/`Focus`/`PaneOpened`/`PaneClosed`; merge instead of replace `snap`.
- Daemon: **unchanged** — it keeps numbering from 1; the client namespaces on
  ingest.
- Everything else (id comparisons, layout math, rendering) is origin-agnostic.

## Phases (each: tests first, land green)

| phase | builds | tests |
|---|---|---|
| **R0** | `src/remote.rs` pure layer: `pack`/`origin_of`/`local_of`, `prefix_snapshot`/`prefix_servermsg`, `route_request` | exhaustive unit tests; round-trip prefix↔route |
| **R1** | connection registry + `route()` with a single local conn (origin 0) | routing hits the right conn; existing daemon integ tests still green |
| **R2** | multi-stream loop; merge snapshots by origin | two local daemons merged, ids don't collide |
| **R3** | `ruckus __proxy` + spawn transport | full **read/write** over a local byte-copy proxy to a 2nd `RUCKUS_DIR` (SSH substitute): attach, send input, split remote |
| **R4** | `[[remote]]` config, sidebar host tags, per-conn reconnect | connect/merge/disconnect lifecycle |
| **R5** | connect_remotes on startup + `ruckus reload`; ConnectTimeout; host-stable origins | (manual reconnect via reload) |

All landed. Follow-up: auto-reconnect a dropped remote in the background
(needs a non-blocking per-conn retry task so a hung SSH can't stall the UI).

## Test harness

`tests/daemon.rs` spins a real daemon on an isolated `RUCKUS_DIR`. Two daemons →
independent sockets that both mint ids from 1 (the collision case). A local
byte-copy proxy (newline-JSON frames) fakes "remote over SSH" — so the whole
R/W path is CI-testable with **no real SSH**.
