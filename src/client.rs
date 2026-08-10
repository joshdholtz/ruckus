use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::protocol::*;

pub struct Client {
    tx: UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMsg>>>>,
    seq: AtomicU64,
}

/// Speak the JSON-RPC protocol over any framed reader + writer. Used for the
/// local unix socket and for a remote daemon reached over an SSH stdio pipe.
pub fn connect_io<R, W>(read: R, mut write: W) -> (Client, UnboundedReceiver<ServerMsg>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = write.flush().await;
        }
    });

    let (ev_tx, ev_rx) = unbounded_channel::<ServerMsg>();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMsg>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let p2 = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let frame = match serde_json::from_str::<ServerFrame>(&line) {
                Ok(f) => f,
                Err(e) => {
                    if std::env::var("RUCKUS_DEBUG").is_ok() {
                        eprintln!("ruckus: bad frame ({e}): {line}");
                    }
                    continue;
                }
            };
            match frame.seq {
                Some(seq) => {
                    if let Some(tx) = p2.lock().unwrap().remove(&seq) {
                        let _ = tx.send(frame.msg);
                    }
                }
                None => {
                    if ev_tx.send(frame.msg).is_err() {
                        break;
                    }
                }
            }
        }
    });

    (
        Client {
            tx: out_tx,
            pending,
            seq: AtomicU64::new(1),
        },
        ev_rx,
    )
}

pub async fn connect() -> Result<(Client, UnboundedReceiver<ServerMsg>)> {
    let stream = UnixStream::connect(socket_path()).await?;
    let (read_half, write_half) = stream.into_split();
    Ok(connect_io(read_half, write_half))
}

/// Connect to a remote daemon over SSH: `ssh [args] <host> ruckus __proxy`
/// relays the remote's unix socket over stdio. Returns the child so the caller
/// can kill it on disconnect. Reuses SSH auth — no ports, no new protocol.
pub async fn connect_remote(
    host: &str,
    args: &[String],
) -> Result<(Client, UnboundedReceiver<ServerMsg>, tokio::process::Child)> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(args)
        .arg(host)
        .arg("ruckus")
        .arg("__proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| anyhow!("ssh {host}: {e}"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let (client, ev) = connect_io(stdout, stdin);
    Ok((client, ev, child))
}

/// The remote side of `connect_remote`: relay this box's daemon socket over
/// stdin/stdout (invoked as `ruckus __proxy` over SSH). Starts the daemon first.
pub async fn proxy() -> Result<()> {
    ensure_daemon().await?;
    let stream = UnixStream::connect(socket_path()).await?;
    let (mut sr, mut sw) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        _ = tokio::io::copy(&mut stdin, &mut sw) => {}
        _ = tokio::io::copy(&mut sr, &mut stdout) => {}
    }
    Ok(())
}

impl Client {
    pub async fn request(&self, req: Request) -> Result<ServerMsg> {
        // Attach base64-encodes up to ~2MB of scrollback — needs a longer budget
        // than a tiny snapshot/input. General requests get 10s (was 5s).
        let timeout = match &req {
            Request::Attach { .. } => Duration::from_secs(30),
            _ => Duration::from_secs(10),
        };
        self.request_timeout(req, timeout).await
    }

    pub async fn request_timeout(&self, req: Request, timeout: Duration) -> Result<ServerMsg> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(seq, tx);
        let line = serde_json::to_string(&ClientFrame { seq, req })?;
        self.tx
            .send(line)
            .map_err(|_| anyhow!("daemon connection closed"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(ServerMsg::Error { message })) => bail!("{message}"),
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => bail!("daemon connection closed"),
            Err(_) => bail!("request timed out"),
        }
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        match self.request(Request::Snapshot).await? {
            ServerMsg::State { snapshot } => Ok(snapshot),
            _ => bail!("unexpected response to snapshot"),
        }
    }
}

/// Find the (space, tab) containing a pane.
pub fn locate_pane(snapshot: &Snapshot, pane: u64) -> Option<(u64, u64)> {
    for s in &snapshot.spaces {
        for t in &s.tabs {
            if t.layout.contains(pane) {
                return Some((s.id, t.id));
            }
        }
    }
    None
}

/// Resolve a pane by numeric id, pane-title substring, or tab name. Tab names
/// resolve to that tab's active pane — agents and humans refer to the name they
/// gave a tab, not the auto-generated pane title.
pub fn resolve_pane(snapshot: &Snapshot, target: &str) -> Result<PaneInfo> {
    if let Ok(id) = target.parse::<u64>() {
        if let Some(p) = snapshot.pane(id) {
            return Ok(p.clone());
        }
    }
    // Exact tab name wins (unambiguous intent).
    let tab_active: Vec<u64> = snapshot
        .spaces
        .iter()
        .flat_map(|s| s.tabs.iter())
        .filter(|t| t.name == target)
        .map(|t| t.active_pane)
        .collect();
    if tab_active.len() == 1 {
        if let Some(p) = snapshot.pane(tab_active[0]) {
            return Ok(p.clone());
        }
    }
    let mut matches: Vec<&PaneInfo> = snapshot
        .panes
        .iter()
        .filter(|p| p.title.contains(target))
        .collect();
    // Fall back to substring match on tab names → their active panes.
    if matches.is_empty() {
        let ids: Vec<u64> = snapshot
            .spaces
            .iter()
            .flat_map(|s| s.tabs.iter())
            .filter(|t| t.name.contains(target))
            .map(|t| t.active_pane)
            .collect();
        matches = snapshot
            .panes
            .iter()
            .filter(|p| ids.contains(&p.id))
            .collect();
    }
    // Last resort: a space name → its active tab's active pane.
    if matches.is_empty() {
        let ids: Vec<u64> = snapshot
            .spaces
            .iter()
            .filter(|s| s.name == target || s.name.contains(target))
            .filter_map(|s| {
                s.tabs
                    .iter()
                    .find(|t| t.id == s.active_tab)
                    .or(s.tabs.first())
            })
            .map(|t| t.active_pane)
            .collect();
        matches = snapshot
            .panes
            .iter()
            .filter(|p| ids.contains(&p.id))
            .collect();
    }
    match matches.len() {
        0 => bail!("no pane, tab, or title matching '{target}'"),
        1 => Ok(matches[0].clone()),
        n => bail!("'{target}' is ambiguous ({n} matches) — use a pane id"),
    }
}

pub async fn ensure_daemon() -> Result<()> {
    if UnixStream::connect(socket_path()).await.is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if UnixStream::connect(socket_path()).await.is_ok() {
            return Ok(());
        }
    }
    bail!("daemon failed to start (see ~/.ruckus/daemon.log)")
}
