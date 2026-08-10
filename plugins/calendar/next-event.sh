#!/usr/bin/env bash
# ruckus calendar plugin — print the next upcoming calendar event, compactly.
# Uses icalBuddy (fast; reads the local EventKit cache). Prints an instantly-
# returned cache and refreshes it in a detached, timeout-guarded background job,
# so the status bar's 3s #(command) budget is never blocked.
#
# One-time setup: grant your terminal app "Calendars" access —
#   System Settings → Privacy & Security → Calendars → enable (e.g. Ghostty).
# Until then this prints nothing (icalBuddy can't see any calendars).
set -u
DIR="$HOME/.ruckus/cache"
CACHE="$DIR/next-event.txt"
LOCK="$DIR/next-event.lock"
mkdir -p "$DIR"

# 1) emit the cached value immediately (empty until permission is granted)
[ -f "$CACHE" ] && head -c 48 "$CACHE"

# 2) refresh in the background when stale (>90s) or missing
now=$(date +%s)
age=999999
[ -f "$CACHE" ] && age=$(( now - $(stat -f %m "$CACHE" 2>/dev/null || echo 0) ))
if [ -d "$LOCK" ] && [ "$(( now - $(stat -f %m "$LOCK" 2>/dev/null || echo "$now") ))" -gt 120 ]; then
  rmdir "$LOCK" 2>/dev/null
fi
if [ "$age" -gt 90 ] && command -v icalBuddy >/dev/null 2>&1 && mkdir "$LOCK" 2>/dev/null; then
  (
    ev="$(timeout 6 icalBuddy -n -nc -nrd -b '' -ps '| |' \
            -iep 'datetime,title' -df '' -tf '%H:%M' -li 1 eventsToday+2 \
          2>/dev/null | head -1 | tr -s ' ' | sed 's/^ *//; s/ *$//; s/ *| */ /')"
    printf '%s' "$ev" > "$CACHE"
    rmdir "$LOCK" 2>/dev/null
  ) >/dev/null 2>&1 &
fi
exit 0
