//! `ruckus agent-hook <agent>` — the universal per-agent adapter handler.
//!
//! Installed into an agent's own hook config, it runs *inside* a ruckus pane, so
//! it reads `RUCKUS_PANE` from the environment and the agent's hook JSON from
//! stdin, normalizes it to a `ReportAgentState`, and (with `--gate`) blocks on
//! `AwaitDecision` so you can approve/deny a tool from the sidebar — from any
//! attached client, including a relay laptop. See docs/AGENT_ADAPTERS.md.

use crate::client::connect;
use crate::protocol::*;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub async fn run(agent: String, gate: bool) -> Result<()> {
    // Read the hook payload from stdin (agents pass JSON there; Codex is special).
    let mut input = String::new();
    let _ = tokio::io::stdin().read_to_string(&mut input).await;
    let v: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);

    // We only act when running inside a ruckus pane.
    let Some(pane) = std::env::var("RUCKUS_PANE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return Ok(());
    };

    match agent.as_str() {
        "claude" => claude(pane, gate, &v).await,
        "codex" => codex(pane, &v).await,
        other => {
            report(
                pane,
                AgentState {
                    agent: other.to_string(),
                    phase: AgentPhase::Working,
                    session: None,
                    pending: None,
                    summary: None,
                },
            )
            .await
        }
    }
}

/// Push one agent-state report; silently no-op if the daemon isn't reachable
/// (never fail the agent because ruckus is down).
async fn report(pane: u64, state: AgentState) -> Result<()> {
    if let Ok((client, _ev)) = connect().await {
        let _ = client.request(Request::ReportAgentState { pane, state }).await;
    }
    Ok(())
}

// ─────────────────────────────── Claude Code ───────────────────────────────

async fn claude(pane: u64, gate: bool, v: &Value) -> Result<()> {
    let ev = str_of(v, "hook_event_name");
    let session = v.get("session_id").and_then(Value::as_str).map(String::from);
    let tool = str_of(v, "tool_name");

    // Approval gate: block on the tool-use gate and answer from the sidebar.
    if gate && (ev == "PreToolUse" || ev == "PermissionRequest") {
        return claude_gate(pane, session, tool, v).await;
    }

    let phase = match ev {
        "Notification" | "Stop" | "StopFailure" => AgentPhase::AwaitingInput,
        "SessionEnd" => AgentPhase::Done,
        "PermissionRequest" => AgentPhase::AwaitingApproval,
        _ => AgentPhase::Working, // UserPromptSubmit / PreToolUse / PostToolUse / …
    };
    let summary = match ev {
        "PreToolUse" | "PostToolUse" if !tool.is_empty() => Some(tool.to_string()),
        "Notification" => v.get("message").and_then(Value::as_str).map(String::from),
        _ => None,
    };
    let pending = (phase == AgentPhase::AwaitingApproval).then(|| approval_from(tool, v));
    report(
        pane,
        AgentState {
            agent: "claude".into(),
            phase,
            session,
            pending,
            summary,
        },
    )
    .await
}

/// Report the pending approval, block until the user resolves it (or the daemon
/// escalates on timeout), then print Claude's decision JSON to stdout.
async fn claude_gate(
    pane: u64,
    session: Option<String>,
    tool: &str,
    v: &Value,
) -> Result<()> {
    let approval = approval_from(tool, v);
    let Ok((client, _ev)) = connect().await else {
        // No daemon — let Claude show its own prompt.
        return Ok(());
    };
    let _ = client
        .request(Request::ReportAgentState {
            pane,
            state: AgentState {
                agent: "claude".into(),
                phase: AgentPhase::AwaitingApproval,
                session,
                pending: Some(approval.clone()),
                summary: Some(approval.title.clone()),
            },
        })
        .await;

    // Block for the decision. The daemon times out to Escalate at 180s; give the
    // client request a little more so the daemon's timeout wins first.
    let decision = match client
        .request_timeout(
            Request::AwaitDecision {
                pane,
                request_id: approval.request_id.clone(),
            },
            Duration::from_secs(200),
        )
        .await
    {
        Ok(ServerMsg::Decided { decision }) => decision,
        _ => Decision::Escalate,
    };

    // Clear the pending state (back to working) regardless of outcome.
    let _ = client
        .request(Request::ReportAgentState {
            pane,
            state: AgentState {
                agent: "claude".into(),
                phase: AgentPhase::Working,
                session: None,
                pending: None,
                summary: None,
            },
        })
        .await;

    let (decision_str, reason) = match decision {
        Decision::Allow => ("allow", "Approved from Ruckus"),
        Decision::Deny => ("deny", "Denied from Ruckus"),
        Decision::Escalate => ("escalate", "Escalated by Ruckus"),
    };
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision_str,
                "permissionDecisionReason": reason,
            }
        })
    );
    Ok(())
}

/// Build an `Approval` from a Claude tool-use payload.
fn approval_from(tool: &str, v: &Value) -> Approval {
    let input = v.get("tool_input");
    let (kind, detail) = match tool {
        "Bash" => (
            ApprovalKind::Command,
            input
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        "Edit" | "Write" | "MultiEdit" => (
            ApprovalKind::FileChange,
            input
                .and_then(|i| i.get("file_path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        _ => (
            ApprovalKind::Tool,
            input.map(|i| i.to_string()).unwrap_or_default(),
        ),
    };
    let short: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = short.chars().take(60).collect();
    let title = if short.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}({short})")
    };
    let request_id = v
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("{tool}-{}", nanos()));
    Approval {
        request_id,
        kind,
        title,
        detail,
    }
}

// ─────────────────────────────── Codex (notify) ───────────────────────────────

async fn codex(pane: u64, v: &Value) -> Result<()> {
    // Codex's `notify` fires only on `agent-turn-complete` → the turn ended and
    // it's waiting for you. (Full state/approvals need `codex app-server` — AA5.)
    let summary = v
        .get("last-assistant-message")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(80).collect::<String>());
    let session = v.get("thread-id").and_then(Value::as_str).map(String::from);
    report(
        pane,
        AgentState {
            agent: "codex".into(),
            phase: AgentPhase::AwaitingInput,
            session,
            pending: None,
            summary,
        },
    )
    .await
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
