# ruckus design notes

Distilled from research into herdr, zellij, lazygit, k9s, yazi, superfile, and crush/opencode (Aug 2026). ✅ = shipped, ⬜ = roadmap.

## Identity

- ✅ **Borderless background layers** (crush/opencode style, per Josh): darkest root bg shows as 1-cell gutters between panes; pane content on a `surface` layer; filled per-pane title bars with `▎` accent focus marker. Box borders are reserved for popups/dialogs only — no tmux lines.
- ✅ Narrow-screen sidebar drawer: `☰` in the header opens the full sidebar as an overlay on phones (the docked sidebar needs ≥70 cols)
- ✅ One accent + neutral grays; semantic colors reserved for state (waiting/working/done/idle)
- ✅ State glyphs, plain Unicode: `◉` waiting (pulses) · braille spinner working · `◍` done · `○` idle
- ✅ State rollup pane → tab → space (a waiting pane colors its whole chain)
- ✅ Done-but-unseen stays in NEEDS YOU until you focus it; seen demotes it (herdr's signature detail)
- ✅ Attention-sorted sidebar queue; click/alt+a jumps
- ✅ Persistent footer hint bar (herdr *lacks* this — reviewers flagged it; we keep it)
- ✅ Info in the border: title + scroll offset + exit code chips in pane titles
- ⬜ Scroll position bottom-right + scrollbar thumb `▐` drawn on the border itself (lazygit)

## Interaction

- ✅ Mouse-native: click to focus, wheel scrolls pane under cursor, hover highlights, tap-first footer buttons
- ✅ Modifier-free drag on pane borders to resize (weights persist via SetLayout)
- ✅ Right-click context menu on panes
- ✅ `mouse = false` escape hatch in config
- ✅ Help overlay (alt+/)
- ⬜ Drag-select to copy without leaving mouse mode; double-click copies token
- ⬜ Modals: dim-mask backdrop, pill buttons, cancel-default on destructive
- ⬜ Rename in context menu (needs daemon rename support)

## Aliveness & finish

- ✅ Braille spinner ~8fps on working; waiting pulses; unfocused panes dim
- ✅ Toasts bottom-right, auto-dismiss
- ⬜ Toast delivery modes like herdr: `terminal` (OSC) / `system` notification when detached
- ⬜ Row flash ~2s on state change (k9s)
- ⬜ Dead pane frame: `[ EXITED ]` + `enter restart · esc close` hints (zellij)
- ⬜ Light/dark auto-switching themes (`auto_switch`, `light_name`/`dark_name` like herdr)
- ⬜ Templatable sidebar row format (config tokens → spans)
- ⬜ Nerd Font icons strictly opt-in (everything today is plain Unicode)

## Bars herdr sets that we differentiate on

- Coherent, fully rebindable keys (HN called herdr's "all over the place")
- Persistent hint bar (herdr hides keys behind prefix+?)
- Working-state spinner (herdr has none)
- Mobile-SSH tap-first mode (footer buttons, auto-collapsing sidebar)
- ⬜ Fixed/tmuxp-style layout presets (requested on HN, unmet by herdr)
