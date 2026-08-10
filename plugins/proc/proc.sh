#!/usr/bin/env bash
# ruckus proc plugin — interactive process killer.
# Fuzzy-search by process name OR listening port; ENTER = SIGTERM,
# CTRL-X = SIGKILL, CTRL-R = refresh, ESC = quit.
set -u
have() { command -v "$1" >/dev/null 2>&1; }

# One row per process: "PID  PORTS  CPU%  COMMAND" (PORTS = listening TCP ports).
list() {
  {
    lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR>1 { n = split($9, a, ":"); print "P", $2, a[n] }'
    ps -Ao pid=,pcpu=,comm=
  } | awk '
    $1 == "P" { if (!seen[$2 "/" $3]++) { ports[$2] = ports[$2] (ports[$2] ? "," : "") $3 } ; next }
    {
      pid = $1; cpu = $2; cmd = "";
      for (i = 3; i <= NF; i++) cmd = cmd (i > 3 ? " " : "") $i;
      rows[++n] = pid "\x1f" cpu "\x1f" cmd
    }
    END {
      for (i = 1; i <= n; i++) {
        split(rows[i], f, "\x1f");
        printf "%-7s  %-16s  %5s%%  %s\n", f[1], (ports[f[1]] ? ports[f[1]] : "-"), f[2], f[3]
      }
    }'
}

if [ "${1:-}" = "--list" ]; then list; exit 0; fi

if ! have fzf; then
  echo "proc: this tool needs fzf (brew install fzf). Falling back to a monitor…"
  sleep 1
  if have btop; then exec btop; elif have htop; then exec htop; else exec top; fi
fi

SELF="$0"
list | fzf --multi --reverse --no-sort --cycle \
  --header $'type a name or port · ENTER kill · CTRL-X force-kill · CTRL-R refresh · ESC quit\nPID      PORTS             CPU    COMMAND' \
  --header-lines=0 \
  --preview 'lsof -nP -p {1} 2>/dev/null | tail -n +2 | head -40' \
  --preview-window 'down,45%,wrap' \
  --bind "enter:execute-silent(echo {+1} | tr ' ' '\n' | xargs -I@ kill @ 2>/dev/null)+reload(bash \"$SELF\" --list)" \
  --bind "ctrl-x:execute-silent(echo {+1} | tr ' ' '\n' | xargs -I@ kill -9 @ 2>/dev/null)+reload(bash \"$SELF\" --list)" \
  --bind "ctrl-r:reload(bash \"$SELF\" --list)" \
  >/dev/null 2>&1
