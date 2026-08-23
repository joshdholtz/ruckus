# Autopilot spaces — design & plan of record

**Goal:** turn a ruckus space into an **ops center** — agents that *monitor,
triage, and prepare* work on a schedule, surface it in a live dashboard, and
**never ship anything without your tap.** You skim a high-level overview from
your phone, then either dispatch a prepared item ("okay, go do it") or approve a
draft to ship. The agents work the night shift; you're the only one who can ship.

Status: **design — foundation being built.** Terminal/daemon core, agent
adapters, the approval gate, the web console, the relay, and MCP connectors all
already exist (see RELAY.md, AGENT_ADAPTERS.md, WEB_CONSOLE.md); autopilot is the
orchestration + dashboard layer on top.

## The trust model (the whole point)

Two tiers of action, split by a hard boundary:

- **Prepare (auto):** read anything, summarize, draft a reply, write code on a
  branch, file a Linear task, open a *draft* PR. Safe, reversible, no external
  effect.
- **Ship (gated):** send an email, merge/push, deploy, publish, post to Slack —
  anything with an outside effect. **Always requires your explicit approval**
  (the `--gate` mechanism from AGENT_ADAPTERS.md, pointed at *egress* tools).

"Prepare, never ship" only holds if the **egress boundary is airtight** — see
Security below. This is the crux of the whole feature.

**Rollout by trust:** v1 is **read-only** (surface + suggest + file Linear tasks,
nothing outbound). Earn trust, then allow drafting PRs, then drafting emails —
always draft-only, always gated to send. Never hand it `send` on day one.

## Flagship example: the "mostly good metrics" ops center

The data sources are already MCP connectors: **MGM** (mostly good metrics),
**Gmail**, **Linear**, **Sentry** (Grafana via its API/token). A scheduled agent
keeps these fresh and pre-drafts; the dashboard shows:

- **🔴 Alerts** — Grafana anomalies + Sentry errors (new/spiking). Each →
  *stage Linear task* · *prepare fix PR* · *dismiss*.
- **📥 Inbox** — emails triaged into *needs-reply / archive / FYI*, with a
  **drafted response** for reply-worthy ones. *send draft* (gated) · *archive* ·
  *open*.
- **📈 Retention** — from MGM: **churn-risk** users + **new signups**, each with a
  **drafted outreach email**. *send* (gated) · *snooze*.
- **🛠 Prepared** — PR drafts for issues it could solve; Linear tasks it filed.
  *approve → ship/dispatch*.
- **Snapshot** — the few MGM numbers you actually watch.

The agent's job = watch + first-draft. Yours = skim + tap.

### The stage → review → dispatch loop

1. Agent **stages** work: Linear task / PR draft / email draft.
2. You **skim the dashboard** and, on an item, tap **"go do it."**
3. ruckus **dispatches** a fresh agent in that space with the item's context; it
   **prepares the change and blocks on your approval to ship.**

Linear-as-staging-queue fits the existing workflow; "assign to agent" is dispatch.

## The dashboard = agent-authored HTML + a component kit

Rather than a fixed UI, a space's dashboard is **just HTML that updates** — an
agent (or you) writes it; ruckus renders it. Separation of concerns: ruckus is
the **frame + components + a safe bridge**; the content is the agent's.

- **Where it lives:** `~/.ruckus/dashboards/<space>.html` (agent writes it; the
  web console renders it for that space). A `dashboard.json` + fixed-widget
  template is the safer alternative for untrusted content.
- **Component kit** (`/vendor/dash.css`): theme-aware primitives so agent-authored
  dashboards look like ruckus — cards, stat tiles, status pills, tables, a diff
  block, an approve button. Consistency for free.
- **Safe bridge** (later): a *curated* JS API the dashboard may call —
  `ruckus.open(pane)`, `ruckus.approve(pane, reqId)` (routed through the gate),
  `ruckus.data(key)` / `ruckus.onUpdate(cb)`. **No raw socket** — only gated,
  curated actions. Same trust boundary as prepare-vs-ship: show anything, act
  only through approved channels.
- **Live update:** the console re-fetches / the bridge subscribes; the agent just
  rewrites the file on its schedule.

## Architecture — how it maps to ruckus

| Need | Provided by |
|---|---|
| Always-on watcher | the persistent daemon on the Mini |
| Run triage/prep agents | agent adapters (`ruckus new -- claude …`) |
| Prepare-don't-ship | the approval **gate** (egress tools gated) |
| Data sources | MCP connectors (MGM/Gmail/Linear/Sentry) |
| The overview | web console + agent-authored dashboard |
| Reach anywhere | the relay |
| Staging queue | Linear (via MCP) |

## Scheduling the trigger

Options (pick one for v1): OS `launchd`/cron running `ruckus new -- claude -p
"<briefing prompt>"`; a `[[schedule]]` block in ruckus config; or Claude Code
routines. The daemon spawns the scheduled tab; the agent runs its prompt, updates
the dashboard, files tasks/drafts, and exits (or persists).

## Security (the boundary that makes it safe)

"Prepare, never ship" is a **containment** problem, not just intercepting named
tools. An agent with shell access can `curl` its way out. Real safety needs:

- **Egress deny-by-default:** every ship path (push, PR merge, deploy, publish,
  email send, webhooks) gated; nothing outbound auto-runs.
- **Sandbox:** prepared work in **git worktrees/branches** (never main); ideally
  restricted network for prep agents.
- **Dashboard isolation:** agent-authored HTML in a **sandboxed iframe** (strict
  CSP, no same-origin, no direct socket) — only the curated bridge can act.
- **Outbound email is the scariest:** to real customers → **draft-only + explicit
  per-message approval, never auto-send.**

## Phases (buildable increments)

| phase | builds |
|---|---|
| **AP0** | **Dashboard surface** — the web console renders a per-space HTML dashboard (sandboxed iframe) from `~/.ruckus/dashboards/<space>.html`; a "Dashboard" entry appears for spaces that have one. Live-refresh. *(the foundation — no egress, safe)* |
| **AP1** | **Component kit** — `/vendor/dash.css` (theme-aware cards/tiles/pills/tables) + a sample dashboard, so agents can author consistent overviews. |
| **AP2** | **Briefing agent (read-only)** — a scheduled `claude`/`codex` run that reads MGM/Gmail/Linear/Sentry via MCP and writes the dashboard + files Linear tasks. Nothing outbound. |
| **AP3** | **Safe bridge** — curated `ruckus.*` API (open/approve/data) so dashboard buttons act through the gate; wire "go do it" dispatch. |
| **AP4** | **Prepare tier** — draft PRs (worktrees) + draft emails, all gated to ship; the egress denylist. |
| **AP5** | **Trusted ship** — one-tap approve-to-send/merge from the dashboard, per-item, with the gate + per-message confirmation. |

## Open questions

- **Dashboard content model:** raw agent HTML (flexible) vs. JSON + fixed widgets
  (safer). Leaning: support both, default JSON+widgets for untrusted, raw HTML for
  power use.
- **Schedule mechanism:** launchd vs. a ruckus-native `[[schedule]]`. Leaning
  native so it's self-contained + portable in config.
- **Egress enforcement depth:** named-tool gating (easy, leaky) vs. sandbox +
  network control (robust). For anything unattended, the latter.
