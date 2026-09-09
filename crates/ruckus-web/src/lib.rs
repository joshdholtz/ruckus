//! The web client: serves the deck PWA and bridges WebSocket <-> the daemon's
//! unix socket. This is a *client* of the frozen v1 protocol, exactly like the
//! TUI — the daemon knows nothing about it. Each browser connection gets its
//! own socket connection, so the daemon sees it as one more ordinary client.
//!
//! Transport note: frames pass through 1:1 (one WS text message == one NDJSON
//! line), so anything the protocol grows works here with zero bridge changes.

use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{error, info};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const MANIFEST: &str = include_str!("../assets/manifest.json");

/// Serve the PWA + WS bridge on `listen` (e.g. "127.0.0.1:9787"). Binds
/// localhost by default — put Tailscale/SSH in front for remote access rather
/// than exposing it; there is deliberately no auth layer here yet.
pub async fn serve(listen: &str) -> Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/manifest.json",
            get(|| async { ([("content-type", "application/json")], MANIFEST) }),
        )
        .route("/ws", get(ws_upgrade));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!("ruckus web on http://{listen} (daemon socket: {})", ruckus_core::protocol::socket_path().display());
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_upgrade(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| async {
        if let Err(e) = bridge(socket).await {
            error!("ws bridge closed: {e:#}");
        }
    })
}

/// Pump frames both ways until either side closes.
async fn bridge(mut ws: WebSocket) -> Result<()> {
    let sock = UnixStream::connect(ruckus_core::protocol::socket_path())
        .await
        .context("connect daemon socket (is the daemon running?)")?;
    let (rd, mut wr) = sock.into_split();
    let mut lines = BufReader::new(rd).lines();

    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) => ws.send(Message::Text(l)).await?,
                None => break, // daemon closed
            },
            msg = ws.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    wr.write_all(t.as_bytes()).await?;
                    wr.write_all(b"\n").await?;
                }
                Some(Ok(Message::Close(_))) | None => break, // browser closed
                Some(Ok(_)) => {} // ping/pong handled by axum; ignore binary
                Some(Err(e)) => return Err(e.into()),
            },
        }
    }
    Ok(())
}
