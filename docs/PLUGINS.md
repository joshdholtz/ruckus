# ruckus plugins

A plugin is a folder with a `ruckus-plugin.toml` that adds **command shortcuts**
and **link handlers** to ruckus. They install into `~/.ruckus/plugins/` and merge
into your config (your own `config.toml` wins on conflicts).

## Manifest — `ruckus-plugin.toml`

```toml
[plugin]
name = "lazygit"
version = "0.1.0"
description = "git UI in a popup + issue links"
capabilities = ["run-commands", "open-links"]   # declared (surfaced by `list`)

# Same shapes as config.toml — see `ruckus config path`.
[[bind]]
key = "alt-g"
run = "lazygit"
where = "popup"           # right | down | tab | popup

[[link]]
pattern = '#([0-9]+)'     # Rust regex
run = "open https://github.com/owner/repo/issues/${match}"
```

`${url}` / `${match}` (and regex captures `${1}`..) are substituted with the
matched text as a **single argument** — never through a shell, so pane output
can't inject commands.

## Context contract (drive ruckus from inside a pane)

Every command ruckus spawns — a pane, a split, a popup, a link handler — gets
these env vars, so a plugin tool knows its context and can call back through the
socket / the `ruckus` CLI:

| var | meaning |
|---|---|
| `RUCKUS_SOCK` | path to the JSON-RPC socket |
| `RUCKUS_DIR` | ruckus dir (the `ruckus` CLI honors it, so it targets this daemon) |
| `RUCKUS_PANE` | the pane's own id (daemon-spawned panes) |
| `RUCKUS_MATCH` | the matched text, for detached link handlers |

So a plugin can be a normal TUI you launch in a split/popup that talks back:
subscribe to `ruckus events`, `ruckus status --json`, `ruckus split/send/focus`,
or speak the socket directly — that's how a reviewer / dashboard / picker plugin
works without ruckus needing to render it.

## CLI

```sh
ruckus plugin install owner/repo            # from a repo root
ruckus plugin install owner/repo/plugins/x  # from a subfolder (monorepo)
ruckus plugin install owner/repo/plugins    # a whole FOLDER of plugins at once
ruckus plugin link ./plugins                # symlink a local dir (dev; also a folder)
ruckus plugin list / remove <h> / update    # manage
ruckus plugin sync                          # install everything declared in config
ruckus reload                               # apply live (no restart)
```

## Portable setup (a new machine)

List your plugins in `config.toml` and they install themselves on startup —
copy that one file to another machine and you're done:

```toml
plugins = [
  "joshdholtz/ruckus/plugins/gh-dash",
  "joshdholtz/ruckus/plugins/pr-review",
]
```

`ruckus` installs any missing ones when it starts (or run `ruckus plugin sync`).
Cheap when they're all present; only clones what's missing.

## Status & roadmap

- **Now (v1):** manifests add `[[bind]]` + `[[link]]`; discovery/merge; the CLI
  above. Capabilities are **declared and surfaced but not yet enforced** — a
  plugin you install can run any command, so install ones you trust.
- **Next (5b):** `[[event]]` handlers (run a command on `pane_opened` /
  `activity` / … — the reactive half, consuming the `ruckus events` stream),
  and capability **enforcement** (deny-by-default scopes).
