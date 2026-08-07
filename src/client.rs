use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::protocol::*;

pub struct Client {
    tx: UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMsg>>>>,
    seq: AtomicU64,
}

pub async fn connect() -> Result<(Client, UnboundedReceiver<ServerMsg>)> {
    let stream = UnixStream::connect(socket_path()).await?;
    let (read_half, mut write_half) = stream.into_split();

    let (out_tx, mut out_rx) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });

    let (ev_tx, ev_rx) = unbounded_channel::<ServerMsg>();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerMsg>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let p2 = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
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

    Ok((Client { tx: out_tx, pending, seq: AtomicU64::new(1) }, ev_rx))
}

impl Client {
    pub async fn request(&self, req: Request) -> Result<ServerMsg> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(seq, tx);
        let line = serde_json::to_string(&ClientFrame { seq, req })?;
        self.tx
            .send(line)
            .map_err(|_| anyhow!("daemon connection closed"))?;
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(ServerMsg::Error { message })) => bail!("{message}"),
            Ok(Ok(msg)) => Ok(msg),
            _ => bail!("request timed out"),
        }
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        match self.request(Request::Snapshot).await? {
            ServerMsg::State { snapshot } => Ok(snapshot),
            _ => bail!("unexpected response to snapshot"),
        }
    }
}

/// Resolve a pane by numeric id or title substring.
pub fn resolve_pane(snapshot: &Snapshot, target: &str) -> Result<PaneInfo> {
    if let Ok(id) = target.parse::<u64>() {
        if let Some(p) = snapshot.pane(id) {
            return Ok(p.clone());
        }
    }
    let matches: Vec<&PaneInfo> = snapshot
        .panes
        .iter()
        .filter(|p| p.title.contains(target))
        .collect();
    match matches.len() {
        0 => bail!("no pane matching '{target}'"),
        1 => Ok(matches[0].clone()),
        n => bail!("'{target}' is ambiguous ({n} panes match)"),
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
