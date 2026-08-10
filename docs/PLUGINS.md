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

`${url}` / `${match}` are substituted with the matched text as a **single
argument** — never through a shell, so pane output can't inject commands.

## CLI

```sh
ruckus plugin install owner/repo   # git clone from GitHub into the plugins dir
ruckus plugin link ./my-plugin     # symlink a local dir (dev)
ruckus plugin list                 # what's installed (handle, binds/links, caps)
ruckus plugin remove <handle>      # by directory handle or manifest name
ruckus plugin path                 # print the plugins dir
ruckus reload                      # apply changes live (no restart)
```

## Status & roadmap

- **Now (v1):** manifests add `[[bind]]` + `[[link]]`; discovery/merge; the CLI
  above. Capabilities are **declared and surfaced but not yet enforced** — a
  plugin you install can run any command, so install ones you trust.
- **Next (5b):** `[[event]]` handlers (run a command on `pane_opened` /
  `activity` / … — the reactive half, consuming the `ruckus events` stream),
  and capability **enforcement** (deny-by-default scopes).
