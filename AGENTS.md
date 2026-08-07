# Driving ruckus from an agent

ruckus is scriptable two ways: the **CLI** (easiest) and the **JSON-RPC socket**
(for long-lived integrations — see [docs/PROTOCOL.md](docs/PROTOCOL.md)). Both
talk to the same background daemon, which is started automatically on first use.

Every command that takes a `<target>` accepts a **pane id**, a **tab name**, or a
substring of either. `$RUCKUS_DIR` (default `~/.ruckus`) isolates state — set it
to run a throwaway instance.

## Inspect

```sh
ruckus status --json     # entire tree + pane states as JSON — parse this
ruckus status            # same, human-readable
ruckus ls                # compact tree
```

`status --json` returns `{ spaces, active_space, panes }`. Each pane has
`id`, `title`, `cmd`, `cwd`, `status` (`running` / `exited{code}`), and
`activity` (`working` / `waiting` / `idle` / `done`). Poll it to know when an
agent you launched needs input or has finished.

## Launch & control

```sh
ruckus new -d -n build -- cargo watch -x test   # new tab, detached (-d), named
ruckus split <target> right -- claude           # split a pane, run a command
ruckus send <target> "yes"                      # type into a pane (+Enter)
ruckus send <target> --no-enter "partial"       # no trailing Enter
ruckus focus <target>                           # make it the active pane
ruckus rename tab <id> "reviewer"               # rename a tab (or: rename space)
ruckus restart <target>                         # respawn an exited pane in place
ruckus kill <target>                            # kill + remove a pane
ruckus tail <target>                            # stream a pane's output
```

## Configure

Agents can set up a user's environment without hand-editing TOML. Edits preserve
comments and formatting.

```sh
ruckus config get ui.gutter                 # read one value
ruckus config set ui.gutter 2               # numbers, strings, bools
ruckus config set keys.quit '["alt-q","ctrl-q"]'   # arrays: pass valid TOML
ruckus config set theme.accent '"#ff8800"'  # quote string values
ruckus config unset ui.gutter               # revert to the built-in default
ruckus config list                          # print the whole file
ruckus config path                          # where it lives
```

Config keys are dotted paths into `config.toml`: `ui.*`, `theme.*`, `glyphs.*`,
`keys.*`, `notify.*`. Changes apply the next time a TUI client starts.

## Typical loop

```sh
pane=$(ruckus split build right -- claude | grep -o '[0-9]*')
# ... wait for it to need you ...
while :; do
  a=$(ruckus status --json | jq -r ".panes[] | select(.id==$pane) | .activity")
  [ "$a" = waiting ] && break
  sleep 2
done
ruckus send "$pane" "approved"
```
