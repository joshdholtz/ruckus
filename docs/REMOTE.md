# Remote mirror — design & plan of record

**Goal:** connect remote ruckus daemons (over SSH) into one local client, so a
remote box's spaces show in your sidebar and are **fully read/write** — view
panes, send input, split, create, close — exactly like local ones.

Status: building, test-first. Phases below land green (with their tests) one at
a time.

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
| **R5** | polish: per-host status, latency/drop handling | reconnect + partial failure |

## Test harness

`tests/daemon.rs` spins a real daemon on an isolated `RUCKUS_DIR`. Two daemons →
independent sockets that both mint ids from 1 (the collision case). A local
byte-copy proxy (newline-JSON frames) fakes "remote over SSH" — so the whole
R/W path is CI-testable with **no real SSH**.
