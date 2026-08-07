# ruckus

**A persistent runtime for your coding agents — organized noise.**

Your agents keep running in a background daemon that owns their terminals. Close the window, drop SSH, shut the lid — the ruckus continues. The TUI shows every **space → tab → pane** at once, tells you which panes are **working / waiting / idle / done**, and gets out of the way until something needs you.

## Status

Early. Core runtime works: daemon, spaces/tabs/panes, live split rendering, activity detection, config (keys + theme), JSON-RPC plugin socket.

## Install

```sh
cargo install --path .
```

## Quickstart

```sh
ruckus                  # open the TUI (starts the daemon if needed)
ruckus new claude       # new tab running claude, attached
ruckus new -d -- cargo watch -x test    # create without attaching
ruckus ls               # tree of everything + activity states
ruckus status --json    # full state as JSON (for scripts and agents)
ruckus send 4 "yes"     # type into a pane
ruckus restart 4        # respawn an exited pane in place
ruckus tail 4           # stream a pane's output (like tail -f)
ruckus attach 4         # open the TUI focused on pane 4
ruckus kill 4           # kill + remove a pane
```

Any `<target>` accepts a pane id, a tab name, or a substring. Agents get a full
control + config surface — see **[AGENTS.md](AGENTS.md)**.

Quitting the TUI never kills your sessions — they live in the daemon.

## The view

```
  ruckus   main › claude·2               ● 1 waiting  ● 2 working
────────────────┬─────────────────────────────────────────────────
 NEEDS YOU      │ ● 1 claude·2   ● 2 zsh·5   +
  ● claude·2    │ ╭ ● claude·2 ─────────────╮╭ ● zsh·5 ─────────╮
                │ │                         ││                   │
 SPACES         │ │                         ││                   │
 ● main         │ │                         ││                   │
    ● claude·2  │ │                         ││                   │
    ● zsh·5     │ ╰─────────────────────────╯╰───────────────────╯
 ● api          │
    ● srv·7     │
────────────────┴─────────────────────────────────────────────────
 alt+a next waiting   alt+v split   alt+t tab   alt+b sidebar   …
```

Header: logo, breadcrumb, live attention counts. Sidebar: a **NEEDS YOU** queue (click to jump) plus the full spaces → tabs tree. Everything is clickable; every dot is an activity state aggregated upward.

Look: no box borders — panes are **background layers** separated by dark gutters, each with a filled title bar (`▎` accent marker on the focused one), IDE-style. Working panes animate a braille spinner, waiting panes pulse, unfocused panes dim. Hover highlights rows, tabs, and footer buttons. Right-click a pane for a context menu (split / new tab / close). **Drag the gutters to resize** — layouts persist in the daemon. `alt+/` opens the key reference; errors appear as toasts that fade after a few seconds.

### Mobile / SSH

Over SSH from a phone (Termius, Blink, Prompt) taps arrive as clicks, so everything stays reachable without a modifier key: footer buttons run actions, the sidebar auto-collapses under 70 columns, and narrow screens switch to tap-first `[split] [+tab] [close]` chips. On desktop macOS, stock Terminal/iTerm sends Option+key as a special character (`œ`, `ß`, …) — ruckus maps those back automatically, and `ctrl+q` always quits. For alt+i / alt+n (dead keys) enable "Use Option as Meta" (Terminal) or "Esc+" (iTerm).

Every dot is an activity state, aggregated upward (pane → tab → space), so a glance at the space bar tells you if anything anywhere needs you.

| State | Meaning | Detected by |
|---|---|---|
| working | producing output | output within the last 3s |
| waiting | blocked on your input | quiet + question/prompt-box tail line, or a quiet non-shell command |
| idle | nothing happening | quiet + shell-prompt tail line |
| done | process exited | exit code (green ok / red err) |

Detection is heuristic v0 — per-agent adapters and plugin-supplied detectors are on the roadmap.

## Default keys

All rebindable in `~/.ruckus/config.toml` (created on first run).

| Key | Action | Key | Action |
|---|---|---|---|
| alt+v | split right | alt+t | new tab |
| alt+s | split down | alt+] / alt+[ | next / prev tab |
| alt+x | close pane | alt+1..9 | jump to tab |
| alt+o / alt+i | next / prev pane | alt+n | new space |
| alt+pgup / alt+pgdn | scroll history | alt+. / alt+, | next / prev space |
| alt+a | jump to next pane that needs you | alt+b | toggle sidebar |
| alt+/ | key reference overlay | alt+q or ctrl+q | quit TUI (daemon keeps running) |

Mouse: click anything — sidebar rows, tabs, the `+` button, footer buttons, panes. Right-click panes for a menu. Drag borders to resize. Wheel scrolls history.

## Customization

`~/.ruckus/config.toml` — the whole UI is yours:

| Section | Controls |
|---|---|
| `[keys]` | Every action, multiple bindings per action; footer hints re-render from your bindings |
| `[theme]` | All 15 colors — 4 background layers, accent, text tiers, state colors |
| `[ui]` | Sidebar side (`left`/`right`/`off`) + width + section order; gutter width (0 = dense, 2 = airy); pane padding; pane title bars on/off; header/footer position (`top`/`bottom`/`off`); tab strip on/off; narrow-collapse threshold (`narrow_below = 0` keeps the sidebar on phones); spinner speed; toast position/duration; mouse capture |
| `[glyphs]` | State icons, focus marker, spinner frames |
| Row templates | `space_row` / `tab_row` / `queue_row` with `{icon} {title} {name} {id} {cmd} {cwd}` tokens |

The only thing ruckus can't control is the font — that belongs to your terminal app.

**Live reload:** `ruckus config set …` applies to a running TUI instantly (theme, glyphs, keys, layout — all of it). Hand-edited the file? `ruckus reload` pushes it to every attached client. Only `spinner_ms` needs a restart.

## Plugins / scripting

The daemon speaks newline-delimited JSON-RPC over `~/.ruckus/ruckus.sock` — the same protocol the TUI uses, available to any language. See [docs/PROTOCOL.md](docs/PROTOCOL.md). A Raycast script, a Stream Deck button, or a phone client is a socket connection away. Embedded scripting (Lua) is planned.

## Architecture

| Piece | What |
|---|---|
| daemon | tokio; owns every PTY via portable-pty; scrollback ring per pane; survives client disconnects |
| protocol | JSON-RPC over unix socket; requests + pushed events (output, activity, state) |
| TUI | ratatui; renders vt100 state per visible pane; full mouse support |
| config | TOML keymap + theme, loaded per client |

## Roadmap

- [x] persistent daemon, spaces / tabs / panes, live splits
- [x] activity detection (working / waiting / idle / done) + aggregated dots
- [x] rebindable keys (multi-binding), theme, mouse, hover, right-click menus
- [x] drag-to-resize pane borders (weights persist in the daemon)
- [x] attention queue (sidebar NEEDS YOU + alt+a jump)
- [x] spinner/pulse animation, dimmed unfocused panes, toasts, help overlay
- [x] mobile-SSH mode: tappable footer actions, auto-collapsing sidebar
- [x] JSON-RPC socket (plugin API v0) + agent CLI (status/send/split/config…)
- [x] agent-aware activity detection (esc-to-interrupt / input-box cues)
- [x] pane zoom, restart-in-place, tab/space rename
- [x] session persistence across daemon restarts (panes respawn under same ids)
- [x] system notifications when a pane needs you and nobody's attached
- [ ] layout presets, drag-select copy
- [ ] Lua scripting
- [ ] remote transport (TCP + auth) → native phone / web clients
