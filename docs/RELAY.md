# Relay transport — design & plan of record

**Goal:** attach to a remote ruckus daemon **from anywhere, behind NAT, with no
SSH** — the remote daemon dials *outward* to a relay and holds the link open; a
client dials the same relay; the relay bridges their byte streams. Remote spaces
mirror into your sidebar and are fully read/write, exactly like the SSH remote
mirror today ([REMOTE.md](REMOTE.md)) — this only swaps the **transport** under
the existing hub.

Status: **design — not yet built.** Terminal-only. Auth MVP: a **shared account
secret**. This doc is the plan of record; nothing here is implemented.

## Why a relay (vs. today's SSH mirror)

The SSH mirror ([REMOTE.md](REMOTE.md)) works, but the reachable box must be
SSH-reachable *inbound* — a public host, a bastion, or a tunnel. A laptop behind
a café NAT or a phone can't be reached. Longhouse's one real advantage is that
the device dials **out** to a rendezvous point, so anything with outbound TCP is
reachable. That is the entire delta. Everything else — persistence, the hub,
origin-namespacing, snapshot merge, auto-reconnect — we already have and reuse
unchanged.

Today's remote is literally *"SSH is a dumb pipe over stdio"* (`client.rs`
`connect_remote_env` → `ssh <host> ruckus __proxy` → `proxy()` copies stdio↔unix
socket). A relay is the **same idea**, NAT-friendly and brokered: a dumb pipe,
just established by outbound dials from both ends and spliced in the middle.

## Topology

```
   device daemon (behind NAT)                 client TUI (anywhere)
        │  outbound TLS                              │  outbound TLS
        ▼                                            ▼
   ┌─────────────────────────────  relay  ─────────────────────────────┐
   │  auth (account+secret) · device registry · session bridge (bytes) │
   └───────────────────────────────────────────────────────────────────┘
```

- **Device** = a ruckus daemon offering itself under a name (e.g. `workbox`).
  Keeps one long-lived **control** connection; opens one **data** connection per
  incoming session.
- **Client** = a ruckus TUI wanting to attach. Opens **one** connection: it
  handshakes, then that same connection *becomes* the data pipe.
- **Relay** = a dumb broker. Authenticates both ends, matches a client to a
  device by name within an account, and splices the two data streams. It does
  **not** parse the ruckus protocol — once a session is bridged it copies bytes.

Asymmetry is deliberate: a device must stay registered to receive many sessions
(long-lived control channel), while a client needs only its one session (its
connection converts straight to data — no second dial, no client-side mux).

## Auth — shared account secret (MVP)

One account, one secret, your machines. A client may reach only devices
registered under the **same account**.

- Relay is configured with an account table: `account → secret_hash` (argon2/
  bcrypt; constant-time compare on the hash).
- Device and client both present `account` + `secret` in their `hello`.
- **TLS is mandatory** so the secret is never on the wire in cleartext.
- Secret is supplied to ruckus via **env** (`secret_env`), never committed in
  `config.toml`.

**Trust assumption (documented, not hidden):** the relay bridges raw bytes, so a
malicious or compromised relay can read and *inject* — and protocol input is
arbitrary command execution. For an MVP relay **you host**, TLS + account auth is
acceptable. Making the relay untrusted transport (end-to-end auth/encryption
between client and device, e.g. Noise `XX` with pinned device keys) is the
**Phase-4 hardening** below, out of MVP scope.

Non-goals for MVP: web/mobile client, per-device keypairs & pairing tokens,
relay-side ACLs beyond the account, connection multiplexing (we use one
connection per session instead), E2E encryption above TLS.

## Wire protocol

Two framings, cleanly separated:

1. **Relay control frames** — newline-delimited JSON (same convention as the
   ruckus protocol, so we reuse the framing code). Used only for the handshake
   and control channel.
2. **Bridged payload** — after a session is spliced, bytes are **opaque** to the
   relay. They are the ordinary ruckus protocol (`ClientFrame`/`ServerFrame`,
   also newline-JSON — but the relay never parses them).

### Frames (proposed `src/relay.rs`)

```rust
// Sent by device/client → relay. Tagged enum, serde snake_case, one per line.
enum RelayHello {
    // long-lived device registration (control channel)
    Device  { proto: u8, account: String, secret: String, device: String },
    // a client wanting to attach; THIS connection becomes the data pipe
    Client  { proto: u8, account: String, secret: String, device: String },
    // the device's per-session data connection, opened after `NewSession`
    Data    { proto: u8, account: String, secret: String, session: Uuid },
}

// relay → device/client
enum RelayMsg {
    Welcome     { device_id: String },                 // auth ok (device/client)
    Denied      { reason: String },                    // auth failed → close
    NewSession  { session: Uuid, client_meta: Meta },  // → device control channel
    SessionReady,                                      // → client: raw protocol follows
    SessionFailed { reason: SessionErr },              // device_offline | timeout | denied
    Ping, Pong,                                        // keepalive on idle control channels
}

enum SessionErr { DeviceOffline, Timeout, Denied }
struct Meta { client_id: Option<String>, ts: u64 }    // advisory, for logging
```

`proto: 1` gates the relay protocol version independently of the ruckus wire
version.

### Session establishment (client attach)

```
client                         relay                          device (control up)
  │── TLS connect ──────────────▶│
  │── Hello::Client{acct,sec,    │
  │        device:"workbox"} ───▶│  auth acct+sec
  │                              │  find "workbox" ctrl chan (same account)
  │                              │── NewSession{session} ──────▶│
  │                              │                              │── TLS connect ─▶ (2nd conn)
  │                              │◀── Hello::Data{acct,sec,session} ──│
  │                              │  match session → SPLICE
  │◀── SessionReady ─────────────│                              │
  │                              │                              │  (device runs handle_conn
  │══ raw ruckus protocol ═══════╪══════════════════════════════╪══  over this data stream)
  │   (relay copies bytes)       │                              │
```

1. Client TLS-dials, sends `Hello::Client{account, secret, device}`.
2. Relay authenticates. Bad creds → `Denied`, close.
3. Relay looks up the device's control channel in that account. Offline →
   `SessionFailed{DeviceOffline}`, close.
4. Relay mints `session` (uuid), pushes `NewSession{session}` to the device's
   control channel.
5. Device TLS-dials a **second** connection, sends `Hello::Data{account, secret,
   session}`. Relay matches `session`, pairs the two sockets.
6. Relay sends the client `SessionReady`. From here it is a **byte bridge** —
   `tokio::io::copy` both directions, no parsing.
7. **Client** hands its post-handshake stream to `connect_io(read, write)` →
   `(Client, events)`. **Device** hands its data stream to the generalized
   `handle_conn` (below). The ordinary ruckus protocol now flows end to end;
   neither side's protocol logic changed.

Keepalive: the relay `Ping`s idle control channels on an interval; a device that
misses N `Pong`s is marked offline (and any in-flight `NewSession` for it fails
fast). Data connections need no keepalive — the bridged protocol has its own
traffic, and a dead TCP conn tears down the session like an SSH drop today.

## Mapping into ruckus (change surface)

The client transport seam already exists; the daemon inbound seam and the
identity layer do not. Precise anchors from the current tree:

- **RL0 — generalize `handle_conn` (enabling refactor, no behavior change).**
  `handle_conn(state, conn_id, stream: UnixStream)` at `daemon.rs:1253` becomes
  generic over a framed duplex, mirroring `connect_io`:
  ```rust
  async fn handle_conn<R, W>(state: StateHandle, conn_id: u64, read: R, write: W) -> Result<()>
  where R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin + Send + 'static
  ```
  The unix accept loop (`daemon.rs:1237-1250`) splits the `UnixStream` with
  `.into_split()` and calls it. Nothing else moves. This is the one refactor that
  unlocks inbound-over-relay.

- **RL2 — the relay broker: `ruckus relay`.** New hidden-ish subcommand in
  `main.rs` dispatch (beside `daemon`/`__proxy`). Owns the account table, a
  `device registry` (name → control-channel `Tx` within account), a pending-
  session map (`session → waiting client sink`), and the splice. Structured so
  the bridge core takes any `AsyncRead+AsyncWrite` pair — so it runs over
  in-memory duplexes in tests (see harness).

- **RL3 — device dial-out: `relay_dial_loop`.** A task started from daemon
  `run()` next to the remote reconnect ticker (`daemon.rs:1007-1034`). Maintains
  the control connection (auth → `Welcome`, then await `NewSession`), and on each
  `NewSession` dials the data connection and calls the generalized `handle_conn`
  — i.e. an incoming relay session is indistinguishable from an accepted unix
  connection. Backoff-reconnects the control channel like the 5s remote retry.

- **RL4 — client attach + hub integration.**
  - `connect_via_relay(url, account, secret, device) -> Result<(Client, events)>`
    in `client.rs`, beside `connect_remote_env` (`client.rs:113`): TLS dial +
    `Hello::Client` + await `SessionReady`, then `connect_io(read, write)`.
  - The remote **hub is reused wholesale**. `spawn_remote_connect`
    (`daemon.rs:1902`) swaps only the transport that produces its
    `(Client, events, handle)` triple: SSH path vs. relay path. Downstream —
    origin prefixing (`src/remote.rs`), `State::snapshot()` merge
    (`daemon.rs:829`), `remote_event_loop` (`daemon.rs:1983`), auto-reconnect —
    is **untouched**.
  - Protocol: add a relay form to `Request::ConnectRemote` (`protocol.rs:126`) or
    a sibling `ConnectRelay { device }`. Serde round-trip test like H1.

- **Reused verbatim (no changes):** `connect_io<R,W>` (`client.rs:23`), the whole
  `Client` API + `Request`/`ServerMsg` protocol, `src/remote.rs` id packing,
  snapshot merge, event pump, per-origin reconnect, and all TUI rendering
  (`PaneView` is origin-agnostic).

## Config

```toml
[relay]
url        = "wss://relay.example.com"    # relay endpoint (TLS)
account    = "josh"
secret_env = "RUCKUS_RELAY_SECRET"        # secret read from env, never the file
device     = "laptop"                     # this daemon's advertised name (daemon side)

# attach a remote device over the relay (client side) — reuses the [[remote]] hub
[[remote]]
relay  = true                             # use the configured [relay] transport
device = "workbox"                        # remote device name to attach
# host set → SSH mode (today); relay=true + device → relay mode. Mutually exclusive.
```

`RemoteSpec` (`config.rs:769`) gains the mutually-exclusive relay variant;
`[relay]` is a new top-level section. On the daemon, `device` advertises this box;
on a client, `[[remote]] relay device=…` attaches one.

## Failure / reconnect semantics

- **Device control drops** → `relay_dial_loop` backoff-reconnects and
  re-registers. Sessions in flight fail fast (`SessionFailed`).
- **Client session drops** → identical to an SSH remote dropping today: the
  `RemoteConn` is removed and auto-reconnect re-dials via the relay.
- **Relay down** → additive-only: the daemon keeps serving locally over its unix
  socket and any SSH `[[remote]]`s. The relay is never a hard dependency.

## Test harness (no network, no TLS, no SSH)

Mirrors the existing ethos: `tests/daemon.rs` already fakes "remote over SSH"
with a **local byte-copy proxy** so the full R/W path is CI-testable. We add an
**in-process relay**: a tokio task speaking the relay control protocol over
`tokio::io::duplex()` pipes, bridging a client duplex to a device duplex — no
sockets, no TLS. Combined with a 2nd-`RUCKUS_DIR` daemon, the whole
attach-over-relay path (auth, `NewSession`, splice, attach, input, split remote,
disconnect) is deterministic in CI. TLS is exercised only in RL5's manual
localhost check.

## Phases (each: tests first, land green)

| phase | builds | tests |
|---|---|---|
| **RL0** | generalize `handle_conn` to a framed duplex; unix accept loop calls it | existing daemon integ tests stay green (pure refactor, no new surface) |
| **RL1** | `src/relay.rs` wire types (`RelayHello`/`RelayMsg`) + framing reuse | serde round-trip; version-gate on `proto` |
| **RL2** | `ruckus relay` broker: account auth, device registry, session splice; bridge core generic over duplex | in-process relay bridges client↔device; `Denied` on bad secret; `SessionFailed` when device offline |
| **RL3** | device `relay_dial_loop`: register, await `NewSession`, serve via `handle_conn`; backoff reconnect | 2nd `RUCKUS_DIR` daemon registers to the in-process relay; control channel survives idle (`Ping`/`Pong`) |
| **RL4** | client `connect_via_relay`; hub relay variant in `spawn_remote_connect`; `[relay]` + `[[remote]] relay` config; `ConnectRelay` protocol | full **read/write** over the in-process relay to the 2nd daemon: attach, send input, split remote, disconnect — no SSH/TLS |
| **RL5** | TLS (rustls) on all three legs; keepalive + backoff; `secret_env` plumbing | manual: real `ruckus relay` on localhost with TLS; drop link → auto-reconnect |
| **RL6** | fmt/clippy/docs; auto-reconnect parity with SSH mirror; `ruckus attach <device>` + palette action + sidebar tag | full suite green |

**Phase 4 hardening (post-MVP, tracked here):** per-device ed25519 keys + pairing
tokens replacing the shared secret; end-to-end Noise between client and device so
the relay is untrusted transport (defeats relay MITM/injection); relay-side rate
limiting and per-device revocation.

## Open questions (settle before RL5)

- **Relay endpoint transport:** `wss://` (WebSocket over TLS — friendlier to
  corporate proxies / cloud LBs) vs. raw TLS TCP (simpler, one less dependency).
  Leaning `wss` for reachability; RL2–RL4 are transport-agnostic (duplex), so
  this only bites at RL5.
- **Device-name collisions** within an account (two boxes both `laptop`):
  last-writer-wins + warn, or reject the second registration? Leaning reject.
- **`ruckus attach <device>` with no local daemon** (pure client): MVP routes
  through the local hub (needs a local daemon, like SSH remotes today). A
  daemonless direct-attach is a later nicety.
