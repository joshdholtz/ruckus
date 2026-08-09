# ruckus protocol — plugin API v0

Newline-delimited JSON over the unix socket at `~/.ruckus/ruckus.sock`
(or `$RUCKUS_DIR/ruckus.sock`). This is the same protocol the bundled TUI uses;
anything it can do, your script can do.

## Framing

Client → daemon, one JSON object per line:

```json
{"seq": 1, "req": {"type": "snapshot"}}
```

Daemon → client:

- **Responses** carry your `seq`: `{"seq": 1, "msg": {...}}`
- **Events** have no `seq`: `{"msg": {"type": "output", ...}}` — pushed to you as things happen.

## Requests

| type | fields | response msg |
|---|---|---|
| `snapshot` | — | `state` (full tree + pane infos) |
| `new_space` | `name?`, `cwd?` | `created` |
| `new_tab` | `space`, `name?`, `cmd: []`, `cwd?` | `created` |
| `split` | `pane`, `dir: right\|down`, `cmd: []`, `cwd?` | `created` |
| `set_layout` | `tab`, `layout: Node` | `done` (same panes, new arrangement/weights) |
| `rename_space` | `space`, `name` | `done` |
| `rename_tab` | `tab`, `name` | `done` |
| `restart` | `pane` | `done` (respawn an exited pane in place; scrollback kept) |
| `reload` | — | `done` (re-read config; pushes `config_changed` to all clients) |
| `close_pane` | `pane` | `done` |
| `close_tab` | `tab` | `done` (kills panes; removes empty space) |
| `close_space` | `space` | `done` |
| `move_tab` | `tab`, `to` (0-based index in its space) | `done` |
| `move_space` | `space`, `to` (0-based index) | `done` |
| `set_active` | `space`, `tab`, `pane` | `done` |
| `attach` | `pane`, `rows`, `cols` | `attached` (base64 scrollback replay; you now receive `output` for this pane) |
| `detach` | `pane` | `done` |
| `input` | `pane`, `data` (base64) | `done` |
| `resize` | `pane`, `rows`, `cols` | `done` |
| `report_activity` | `pane`, `state` | `done` — authoritative activity override (see below) |
| `report_agent` | `pane`, `name?` | `done` — set/clear `PaneInfo.agent` (`null`/omit to clear) |

Empty `cmd` spawns `$SHELL`.

### Attach / resize size policy

Each connection that `attach`es a pane reports its `rows`/`cols`. The daemon sets
the **PTY size to the max of all attached clients** for that pane. A small
`tail` viewer therefore cannot shrink a full TUI, and the last attach no longer
blindly overwrites the others. `detach` recomputes from remaining subscribers.
`resize` updates *your* subscription size and re-applies the max.

### Activity reporting (detector seam)

Heuristics mis-fire. Prefer authoritative reports when you know the truth:

| `state` | meaning |
|---|---|
| `"working"` | producing work / tool call in flight |
| `"waiting"` | blocked on the user |
| `"idle"` | quiet, not waiting |
| `"auto"` | hand the pane back to built-in heuristics |

While a pane has a non-`auto` report, the quiet-ticker and raw-output heuristic
leave it alone (exit still forces `done`).

```json
{"seq": 2, "req": {"type": "report_activity", "pane": 4, "state": "waiting"}}
{"seq": 3, "req": {"type": "report_agent", "pane": 4, "name": "claude"}}
{"seq": 4, "req": {"type": "report_agent", "pane": 4, "name": null}}
```

CLI equivalents: `ruckus report-activity 4 waiting`, `ruckus report-agent 4 claude`.

## Events

| type | fields | when |
|---|---|---|
| `output` | `pane`, `data` (base64) | pane produced output (attached panes only) |
| `activity` | `pane`, `activity` | activity changed: `working` / `waiting` / `idle` / `done` |
| `pane_opened` | `space`, `tab`, `pane` | a pane was created (split / new tab / new space) |
| `pane_closed` | `pane` | a pane was killed/removed |
| `focus` | `space`, `tab`, `pane` | the active space/tab/pane changed |
| `exited` | `pane`, `code` | pane's process exited |
| `state` | `snapshot` | tree changed (created/closed/moved/active/cwd/agent) — every connection |
| `config_changed` | — | config should be reloaded from disk (after a `reload`) |

Granular events save you diffing snapshots. `ruckus events` streams all of the
above (except `output`/`state`) as newline-delimited JSON — the observe half of
the plugin/agent API:

```sh
ruckus events | while read -r ev; do echo "got: $ev"; done
```

## Snapshot shape

```json
{
  "spaces": [
    {
      "id": 1,
      "name": "main",
      "active_tab": 2,
      "tabs": [
        {
          "id": 2,
          "name": "claude",
          "active_pane": 3,
          "layout": { "kind": "leaf", "pane": 3 }
        }
      ]
    }
  ],
  "active_space": 1,
  "panes": [
    {
      "id": 3,
      "title": "claude",
      "cmd": ["claude"],
      "cwd": "/Users/you/proj",
      "status": { "state": "running" },
      "activity": "waiting",
      "created": 1710000000,
      "agent": "claude"
    }
  ]
}
```

`status` is either `{"state":"running"}` or `{"state":"exited","code":N}`.
`agent` is optional (null/absent when unknown). `cwd` is updated live from the
pane process (~1 Hz) when the OS allows.

Layout `Node`:

```json
{ "kind": "leaf", "pane": 3 }
{ "kind": "split", "dir": "right", "children": [/* Node */], "weights": [1, 2] }
```

`dir` is `right` or `down`. Empty `weights` means equal shares.

## Example: notify when any agent needs you

```python
import socket, json, os

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.path.expanduser("~/.ruckus/ruckus.sock"))
for line in s.makefile():
    frame = json.loads(line)
    m = frame.get("msg", {})
    if m.get("type") == "activity" and m.get("activity") == "waiting":
        print(f"pane {m['pane']} is waiting for you")
```

## Example: agent hook reporting activity

From inside an agent (or a wrapper script) after you know the user must answer:

```sh
# CLI (starts daemon if needed)
ruckus report-activity "$PANE_ID" waiting
ruckus report-agent "$PANE_ID" my-agent

# … later, hand detection back to heuristics
ruckus report-activity "$PANE_ID" auto
```

Or raw socket:

```python
def rpc(sock, seq, req):
    sock.sendall((json.dumps({"seq": seq, "req": req}) + "\n").encode())
    for line in sock.makefile():
        frame = json.loads(line)
        if frame.get("seq") == seq:
            return frame["msg"]

rpc(s, 1, {"type": "report_activity", "pane": 4, "state": "waiting"})
```

## Stability

v0: shapes may change until 1.0; the `type` discriminant scheme and framing will not.
