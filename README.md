# ruckus

**A persistent runtime for your coding agents — organized noise.**

Your agents keep running in a background daemon that owns their terminals. Close the window, drop SSH, shut the lid — the ruckus continues. The TUI shows every **space → tab → pane** at once, tells you which panes are **working / waiting / idle / done**, and gets out of the way until something needs you.

## Status

Usable, still pre-1.0. Working: the daemon + spaces/tabs/panes, live split rendering, agent-aware activity detection, a choose-tree command palette, floating popups, click-to-open link handlers, session persistence across restarts, a JSON-RPC socket with a lifecycle **event stream**, and an installable **plugin system**. Wire formats may still shift before 1.0.

## Install

**Prebuilt binary** (macOS + Linux, no toolchain):

```sh
curl -fsSL https://raw.githubusercontent.com/joshdholtz/ruckus/main/install.sh | sh
```

Detects your OS/arch, grabs the matching binary from the [latest release](https://github.com/joshdholtz/ruckus/releases/latest), and installs to `/usr/local/bin` (or `~/.local/bin`, adding it to your PATH if needed). Pin with `RUCKUS_VERSION=v0.1.0`, redirect with `RUCKUS_INSTALL_DIR=…`, skip the PATH edit with `RUCKUS_NO_MODIFY_PATH=1`.

**From source** (needs Rust):

```sh
cargo install --git https://github.com/joshdholtz/ruckus     # or: cargo install --path .
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
ruckus events           # stream lifecycle events as JSON (agents/plugins)
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
 ⌃b a next waiting   ⌃b % split   ⌃b c tab   ⌃b b sidebar   …
```

Header: logo, breadcrumb, live attention counts. Sidebar: a **NEEDS YOU** queue (click to jump) plus the full spaces → tabs tree. Everything is clickable; every dot is an activity state aggregated upward.

Look: no box borders — panes are **background layers** separated by dark gutters, each with a filled title bar (`▎` accent marker on the focused one), IDE-style. Working panes animate a braille spinner, waiting panes pulse, unfocused panes dim. Hover highlights rows, tabs, and footer buttons. Right-click a pane for a context menu (split / new tab / close). **Drag the gutters — or the sidebar edge — to resize**; layouts and sidebar width persist. **Click a URL** to open it (a copy shows a brief confirmation popup); `alt+/` opens the key reference and lists your plugin bindings.

**Command palette** (`alt+p`) is a **choose-tree navigator**: fuzzy-jump to any space / tab / pane, fold branches with `←/→`, or type `>` for the flat command list. **Popups** (`⌃q` to close) run a tool — lazygit, k9s, a scratch shell — in a floating window without cluttering the tree.

### Mobile / SSH

Still fully usable — same daemon, same panes — but **triage-first** on a phone. Over SSH (Termius, Blink, Prompt) taps are clicks, so you never need a modifier key:

- The home screen is the **deck**: big state-colored cards, one per tab, attention-sorted. Tap a card to drop into that pane near-fullscreen; `☰` (or the command bar) brings you back.
- The palette's tree jumps anywhere; a slim command bar shows the real keys for your keymap.
- Under ~70 columns the sidebar becomes a `☰` drawer and the focused pane **auto-zooms**.

Desktop stays the multi-pane chaos surface. Phone is glance → decide → reply.

On desktop macOS, stock Terminal/iTerm sends Option+key as a special character (`œ`, `ß`, …) — ruckus maps those back automatically. For alt+i / alt+n (dead keys) enable "Use Option as Meta" (Terminal) or "Esc+" (iTerm).

Every dot is an activity state, aggregated upward (pane → tab → space), so a glance at the space bar tells you if anything anywhere needs you.

| State | Meaning | Detected by |
|---|---|---|
| working | producing output | output within the last 3s |
| waiting | blocked on your input | quiet + question/prompt-box tail line, or a quiet non-shell command |
| idle | nothing happening | quiet + shell-prompt tail line |
| done | process exited | exit code (green ok / red err) |

Detection is heuristic v0 — external detectors can override it over the socket (`ruckus report-activity`), and per-agent adapters are on the roadmap.

## Default keys

The default keymap is **`tmux`** — a `⌃b` prefix (`⌃b c` new tab, `⌃b z` zoom, `⌃b /` search, `⌃b d` detach…) with the one-step **alt** keys below as a fallback. Switch the base scheme with `ruckus keymap alt | tmux | both`. Everything is rebindable in `~/.ruckus/config.toml` (created on first run).

| Key | Action | Key | Action |
|---|---|---|---|
| alt+v | split right | alt+t | new tab |
| alt+s | split down | alt+] / alt+[ | next / prev tab |
| alt+x | close pane | alt+1..9 | jump to tab |
| alt+o / alt+i | next / prev pane | alt+n | new space |
| alt+pgup / alt+pgdn | scroll history | alt+. / alt+, | next / prev space |
| alt+; / alt+l | last pane / last space (jump back) | alt+b | toggle sidebar |
| alt+a | jump to next pane that needs you | | |
| alt+p | palette (jump to any space/tab/pane · `>` for commands) | alt+f | search scrollback (n/N cycle) |
| alt+z | zoom focused pane | alt+d | deck (mobile card view) |
| alt+/ | key reference + your plugin binds | alt+q / ctrl+q | quit TUI (daemon keeps running) |

Mouse: click anything — sidebar rows, tabs, the `+` button, footer buttons, panes, and **URLs** (open in browser). Right-click panes for a menu. Drag pane gutters or the sidebar edge to resize. Wheel scrolls history — or the app inside the pane, if it wants the mouse.

## Customization

`~/.ruckus/config.toml` — the whole UI is yours:

| Section | Controls |
|---|---|
| `[keys]` / `[prefix_keys]` | Every action, multiple bindings each; the `keymap` preset (alt / tmux / both) |
| `[[bind]]` | Key → run a command in a split / tab / popup (`where = "right\|down\|tab\|popup"`) |
| `[[link]]` | Regex on pane text → run a command (`${url}`/`${1}`…); `link_click = plain\|ctrl\|shift` |
| `plugins` | Plugin refs installed on startup — portable across machines |
| `[theme]` | All 15 colors — 4 background layers, accent, text tiers, state colors |
| `[ui]` | Sidebar side/width/section order; gutter + pane padding; title bars; header/footer position; tab strip; narrow-collapse threshold; deck on/off; spinner speed; toast position; mouse |
| `[glyphs]` | State icons, focus marker, spinner frames |
| Row templates | `space_row` / `tab_row` / `queue_row` with `{icon} {title} {name} {id} {cmd} {cwd}` tokens |

Or pick a built-in theme: `ruckus theme` lists them (macchiato, latte, gruvbox, nord, tokyonight, dracula, rosepine), `ruckus theme nord` switches live. The only thing ruckus can't control is the font — that belongs to your terminal app.

**Live reload:** `ruckus config set …` applies to a running TUI instantly (theme, glyphs, keys, layout — all of it). Hand-edited the file? `ruckus reload` pushes it to every attached client. Only `spinner_ms` needs a restart.

## Plugins

A plugin is a folder with a `ruckus-plugin.toml` that adds **command shortcuts** and **link handlers** — no build step, no runtime, any language for the tools it launches.

```toml
[[bind]]                      # a key → open a command in a split / tab / popup
key = "alt-g"
run = "lazygit"
where = "popup"

[[link]]                      # click matching text → run a command
pattern = 'https://github.com/[^/]+/[^/]+/pull/[0-9]+'
run = "gh pr view ${url}"
where = "right"
```

```sh
ruckus plugin install owner/repo             # or owner/repo/subfolder (monorepo)
ruckus plugin link ./plugins                 # dev: link a whole folder of plugins
ruckus plugin list / update / remove <name>
```

**Portable setup** — list them in config and they install on startup, so a copied `config.toml` reproduces your setup on a new machine:

```toml
plugins = ["joshdholtz/ruckus/plugins/gh-dash", "joshdholtz/ruckus/plugins/pr-review"]
```

**Scriptable** — the daemon speaks newline-delimited JSON-RPC over `~/.ruckus/ruckus.sock`, the same protocol the TUI uses. `ruckus events` streams lifecycle events (pane opened/closed, activity, focus, exit) as JSON, and every spawned pane gets `RUCKUS_SOCK` / `RUCKUS_DIR` / `RUCKUS_PANE` in its env — so a tool running *inside* a pane can drive ruckus back (jump, spawn, send input, subscribe). See **[docs/PROTOCOL.md](docs/PROTOCOL.md)** and **[docs/PLUGINS.md](docs/PLUGINS.md)**.

Capabilities are declared in the manifest and surfaced by `ruckus plugin list`, but not yet enforced — install ones you trust.

## Architecture

| Piece | What |
|---|---|
| daemon | tokio; owns every PTY via portable-pty; scrollback ring per pane; survives client disconnects and zero-downtime `ruckus upgrade` |
| protocol | JSON-RPC over a unix socket; requests + pushed events (output, activity, pane opened/closed, focus, exit, state) |
| TUI | ratatui; renders vt100 state per visible pane; full mouse support; client-local popups |
| config | TOML keymap + theme + `[[bind]]`/`[[link]]` + declared plugins, loaded per client |

## Roadmap

- [x] persistent daemon, spaces / tabs / panes, live splits, session persistence
- [x] activity detection (working / waiting / idle / done) + aggregated dots
- [x] rebindable keys (multi-binding + keymap presets), theme presets, mouse, hover, right-click menus
- [x] drag-to-resize pane gutters **and** the sidebar (weights + width persist)
- [x] attention queue (sidebar NEEDS YOU + alt+a jump) + system notifications
- [x] choose-tree command palette (jump spaces/tabs/panes · `>` commands), scrollback search
- [x] floating command popups (lazygit/k9s/scratch in a window)
- [x] click-to-open link handlers (regex → command, in a pane or detached)
- [x] mobile deck (card home) + tree, tappable, auto-collapsing sidebar
- [x] JSON-RPC socket + lifecycle **event stream** + `RUCKUS_*` context env
- [x] plugin system: `ruckus plugin install/link/sync`, manifests, config-declared portable setup
- [x] agent CLI (status/send/split/config/events…), cwd-aware splits, bracketed paste, mouse forwarding
- [x] remote mirror: `[[remote]]` daemons over SSH, merged into the sidebar, full read/write (docs/REMOTE.md)
- [ ] `[[event]]` handlers (run a command on an event) + capability enforcement
- [ ] plugin-rendered pane UI / native widgets
- [ ] auto-reconnect dropped remotes; native phone / web clients
