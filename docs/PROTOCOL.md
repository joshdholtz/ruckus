# ruckus protocol — plugin API v0

Newline-delimited JSON over the unix socket at `~/.ruckus/ruckus.sock`. This is the same protocol the bundled TUI uses; anything it can do, your script can do.

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
| `restart` | `pane` | `done` (respawn an exited pane in place) |
| `reload` | — | `done` (re-read config; pushes `config_changed` to all clients) |
| `close_pane` | `pane` | `done` |
| `set_active` | `space`, `tab`, `pane` | `done` |
| `attach` | `pane`, `rows`, `cols` | `attached` (base64 scrollback replay; you now receive `output` events for this pane) |
| `detach` | `pane` | `done` |
| `input` | `pane`, `data` (base64) | `done` |
| `resize` | `pane`, `rows`, `cols` | `done` |

Empty `cmd` spawns `$SHELL`.

## Events

| type | fields | when |
|---|---|---|
| `output` | `pane`, `data` (base64) | pane produced output (attached panes only) |
| `activity` | `pane`, `activity` | activity changed: `working` / `waiting` / `idle` / `done` |
| `exited` | `pane`, `code` | pane's process exited |
| `state` | `snapshot` | tree changed (created/closed/moved/active) — sent to every connection |
| `config_changed` | — | config should be reloaded from disk (after a `reload`) |

## Example: notify when any agent needs you

```python
import socket, json

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(f"{HOME}/.ruckus/ruckus.sock")
for line in s.makefile():
    frame = json.loads(line)
    m = frame.get("msg", {})
    if m.get("type") == "activity" and m.get("activity") == "waiting":
        notify(f"pane {m['pane']} is waiting for you")
```

## Stability

v0: shapes may change until 1.0; the `type` discriminant scheme and framing will not.
