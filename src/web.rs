//! `ruckus web` — a clean, mobile-first web console over the same daemon.
//!
//! It is a *parallel* front-end: the terminal TUI is untouched. The web server
//! bridges the browser to the daemon protocol over a WebSocket (live session
//! list, state, approvals, reply) and reads each agent's own transcript archive
//! to render a normalized, provider-agnostic session view. See docs/WEB_CONSOLE.md.

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::client::{connect, ensure_daemon};
use crate::protocol::*;

/// Serve the web console on `addr` (e.g. `0.0.0.0:8080`, reached over Tailscale).
pub async fn run(addr: &str) -> Result<()> {
    ensure_daemon().await?;
    let app = Router::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/ws", get(ws_upgrade));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    println!("ruckus web console on http://{local}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    Html(include_str!("web/index.html"))
}

async fn manifest() -> impl IntoResponse {
    (
        [("content-type", "application/manifest+json")],
        include_str!("web/manifest.webmanifest"),
    )
}

async fn ws_upgrade(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| async move {
        if let Err(e) = ws_session(socket).await {
            tracing::warn!("web ws: {e:#}");
        }
    })
}

/// One browser connection: bridge it to a fresh daemon client. The browser
/// speaks a tiny JSON protocol (see below); daemon events are pushed as they
/// arrive so the timeline + open session stay live.
async fn ws_session(socket: WebSocket) -> Result<()> {
    let (client, mut events) = connect().await?;
    let (mut tx, mut rx) = socket.split();

    // Push the initial snapshot so the UI paints immediately.
    if let Ok(snap) = client.snapshot().await {
        let _ = tx
            .send(Message::Text(
                json!({ "t": "snapshot", "snapshot": snap }).to_string(),
            ))
            .await;
    }

    loop {
        tokio::select! {
            // Daemon → browser: forward every pushed event.
            ev = events.recv() => {
                let Some(ev) = ev else { break };
                let _ = tx.send(Message::Text(json!({ "t": "event", "msg": ev }).to_string())).await;
            }
            // Browser → daemon.
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                match v.get("t").and_then(Value::as_str) {
                    Some("snapshot") => {
                        if let Ok(snap) = client.snapshot().await {
                            let _ = tx.send(Message::Text(
                                json!({ "t": "snapshot", "snapshot": snap }).to_string())).await;
                        }
                    }
                    // Read + parse this pane's transcript into normalized messages.
                    Some("session") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        let (msgs, model) = load_session(&client, pane).await;
                        let _ = tx.send(Message::Text(
                            json!({ "t": "session", "pane": pane, "msgs": msgs, "model": model })
                                .to_string())).await;
                    }
                    // Approve / deny a pending agent request.
                    Some("resolve") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        let request_id = v.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
                        let decision = match v.get("decision").and_then(Value::as_str) {
                            Some("allow") => Decision::Allow,
                            Some("deny") => Decision::Deny,
                            _ => Decision::Escalate,
                        };
                        client.notify(Request::ResolveDecision { pane, request_id, decision });
                    }
                    // Start a new tab (optionally in a fresh space) running a
                    // command — e.g. claude / codex / a shell — from the phone.
                    Some("new_tab") => {
                        let cmd_str = v.get("cmd").and_then(Value::as_str).unwrap_or("").trim().to_string();
                        let mut cmd: Vec<String> =
                            cmd_str.split_whitespace().map(String::from).collect();
                        if cmd.is_empty() {
                            cmd = vec![default_shell()];
                        }
                        let name = cmd.first().map(|c| {
                            c.rsplit('/').next().unwrap_or(c).to_string()
                        });
                        let mut space = v.get("space").and_then(Value::as_u64).unwrap_or(0);
                        if space == 0 {
                            if let Ok(ServerMsg::Created { space: s, .. }) = client
                                .request(Request::NewSpace { name: name.clone(), cwd: None })
                                .await
                            {
                                space = s;
                            }
                        }
                        if space != 0 {
                            let _ = client
                                .request(Request::NewTab { space, name, cmd, cwd: None })
                                .await;
                        }
                        if let Ok(snap) = client.snapshot().await {
                            let _ = tx.send(Message::Text(
                                json!({ "t": "snapshot", "snapshot": snap }).to_string())).await;
                        }
                    }
                    Some("new_space") => {
                        let name = v.get("name").and_then(Value::as_str)
                            .filter(|s| !s.is_empty()).map(String::from);
                        let _ = client.request(Request::NewSpace { name, cwd: None }).await;
                        if let Ok(snap) = client.snapshot().await {
                            let _ = tx.send(Message::Text(
                                json!({ "t": "snapshot", "snapshot": snap }).to_string())).await;
                        }
                    }
                    // Send a reply (types text + Enter into the real session).
                    Some("input") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        let mut text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        if v.get("enter").and_then(Value::as_bool).unwrap_or(true) {
                            text.push('\r');
                        }
                        client.notify(Request::Input { pane, data: B64.encode(text.as_bytes()) });
                    }
                    // Attach to a pane to stream its live terminal output.
                    Some("attach") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        if let Ok(ServerMsg::Attached { scrollback, .. }) = client
                            .request(Request::Attach { pane, rows: 50, cols: 120 })
                            .await
                        {
                            let _ = tx.send(Message::Text(
                                json!({ "t": "attached", "pane": pane, "scrollback": scrollback })
                                    .to_string())).await;
                        }
                    }
                    Some("detach") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        client.notify(Request::Detach { pane });
                    }
                    // Session management from the phone.
                    Some("close") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        let _ = client.request(Request::ClosePane { pane }).await;
                        if let Ok(snap) = client.snapshot().await {
                            let _ = tx.send(Message::Text(
                                json!({ "t": "snapshot", "snapshot": snap }).to_string())).await;
                        }
                    }
                    Some("restart") => {
                        let pane = v.get("pane").and_then(Value::as_u64).unwrap_or(0);
                        let _ = client.request(Request::Restart { pane }).await;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Resolve a pane's live agent session id (from the daemon snapshot), locate its
/// provider transcript, and parse it into normalized messages.
async fn load_session(client: &crate::client::Client, pane: u64) -> (Vec<Value>, Option<String>) {
    let snap = match client.snapshot().await {
        Ok(s) => s,
        Err(_) => return (vec![], None),
    };
    let Some(p) = snap.panes.iter().find(|p| p.id == pane) else {
        return (vec![], None);
    };
    let session = p
        .agent_state
        .as_ref()
        .and_then(|s| s.session.clone())
        .unwrap_or_default();
    if !session.is_empty() {
        if let Some(path) = find_claude_transcript(&session) {
            return parse_claude_transcript(&path);
        }
    }
    (vec![], None)
}

/// Short friendly model name from a full id (claude-opus-4-8 → "opus").
fn friendly_model(id: &str) -> String {
    let l = id.to_lowercase();
    for tag in ["opus", "sonnet", "haiku", "fable"] {
        if l.contains(tag) {
            return tag.to_string();
        }
    }
    id.to_string()
}

/// Find `~/.claude/projects/<any>/<session>.jsonl` for a Claude session id.
fn find_claude_transcript(session: &str) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    let base = dirs::home_dir()?.join(".claude/projects");
    let file = format!("{session}.jsonl");
    for entry in std::fs::read_dir(&base).ok()?.flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse a Claude transcript JSONL into normalized chat messages. Each record's
/// `message.content` is a string (user) or a list of blocks (assistant):
/// text / thinking / tool_use / tool_result.
fn parse_claude_transcript(path: &PathBuf) -> (Vec<Value>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (vec![], None);
    };
    let mut out = Vec::new();
    let mut model: Option<String> = None;
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = match rec.get("type").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if role == "assistant" {
            if let Some(m) = rec
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
            {
                model = Some(friendly_model(m));
            }
        }
        let content = rec.get("message").and_then(|m| m.get("content"));
        match content {
            Some(Value::String(s)) => {
                if !s.trim().is_empty() {
                    out.push(json!({ "role": role, "kind": "text", "text": s }));
                }
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "text" => {
                            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                            if !t.trim().is_empty() {
                                out.push(json!({ "role": role, "kind": "text", "text": t }));
                            }
                        }
                        "tool_use" => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = b.get("input").cloned().unwrap_or(json!({}));
                            out.push(json!({
                                "role": "tool", "kind": "tool_call",
                                "tool": name, "input": tool_summary(name, &input),
                            }));
                        }
                        "tool_result" => {
                            let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            out.push(json!({
                                "role": "tool", "kind": "tool_result",
                                "is_error": is_error, "text": result_text(b),
                            }));
                        }
                        _ => {} // thinking / other → skip in the clean view
                    }
                }
            }
            _ => {}
        }
    }
    (out, model)
}

/// A one-line-ish summary of a tool call for the chat card.
fn tool_summary(name: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let s = match name {
        "Bash" => pick("command"),
        "Read" | "Edit" | "Write" | "MultiEdit" => pick("file_path"),
        "Grep" => pick("pattern"),
        "Glob" => pick("pattern"),
        _ => input.to_string(),
    };
    s.chars().take(240).collect()
}

fn result_text(b: &Value) -> String {
    let c = b.get("content");
    let s = match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    s.chars().take(400).collect()
}
