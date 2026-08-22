//! Relay transport — reach a daemon from anywhere with no SSH and no inbound
//! port. Both the device (a daemon) and the client dial *outward* to a broker
//! that authenticates them (shared account secret) and splices their byte
//! streams. The broker never parses the ruckus protocol — once a session is
//! bridged it copies bytes, exactly like `ruckus __proxy` does over SSH stdio.
//!
//! Intended to run inside a private network (Tailscale) that already provides
//! encryption + reachability, so this layer is plain TCP + a shared secret. See
//! docs/RELAY.md for the full design; TLS / end-to-end crypto is a later phase.

use anyhow::{bail, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{info, warn};

/// Relay wire-protocol version, independent of the ruckus protocol version.
pub const PROTO: u8 = 1;

/// A buffered read half + write half of a relay TCP connection, ready to be
/// handed to `connect_io` (client) or `handle_conn` (device data conn). The
/// `BufReader` carries any bytes already read past the handshake line.
pub type Halves = (BufReader<OwnedReadHalf>, OwnedWriteHalf);

/// First line a peer sends to the relay: who it is and what it wants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayHello {
    /// A daemon registering itself; keeps this connection as its control channel.
    Device {
        proto: u8,
        account: String,
        secret: String,
        device: String,
    },
    /// A client wanting to attach to `device`; this connection becomes the pipe.
    Client {
        proto: u8,
        account: String,
        secret: String,
        device: String,
    },
    /// The device's per-session data connection, dialed after `NewSession`.
    Data {
        proto: u8,
        account: String,
        secret: String,
        session: u64,
    },
}

/// Relay → peer control messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMsg {
    /// Auth ok.
    Welcome { device_id: String },
    /// Auth failed; the relay closes the connection.
    Denied { reason: String },
    /// Pushed to a device's control channel: a client wants a session.
    NewSession { session: u64 },
    /// To a client: the pipe is live, raw ruckus protocol follows.
    SessionReady,
    /// To a client: could not bridge.
    SessionFailed { reason: String },
    /// Keepalive.
    Ping,
    Pong,
}

/// Write one newline-delimited JSON frame and flush.
pub async fn write_frame<W, T>(w: &mut W, v: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Read one newline-delimited JSON frame. `Ok(None)` on clean EOF.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncBufReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    let n = r.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let line = line.trim_end();
    if line.is_empty() {
        // tolerate blank keepalive lines
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(line)?))
}

// ─────────────────────────── broker (`ruckus relay`) ───────────────────────────

struct Broker {
    account: String,
    secret: String,
    /// (account, device) → control-channel sender to push `NewSession`/`Ping`.
    devices: Mutex<HashMap<(String, String), mpsc::UnboundedSender<RelayMsg>>>,
    /// session id → sink that hands the device's data conn to the waiting client.
    pending: Mutex<HashMap<u64, oneshot::Sender<Halves>>>,
    next_session: AtomicU64,
}

impl Broker {
    fn auth(&self, account: &str, secret: &str) -> bool {
        account == self.account && secret == self.secret
    }
}

/// Run the relay broker, listening on `addr` (e.g. `0.0.0.0:9777` on a tailnet).
/// A single account/secret pair for the MVP.
pub async fn run_broker(addr: &str, account: String, secret: String) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    info!("relay: listening on {local} (account `{account}`)");
    let broker = Arc::new(Broker {
        account,
        secret,
        devices: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
    });
    loop {
        let (stream, peer) = listener.accept().await?;
        let broker = broker.clone();
        tokio::spawn(async move {
            if let Err(e) = broker_conn(stream, broker).await {
                warn!("relay: conn {peer}: {e:#}");
            }
        });
    }
}

async fn broker_conn(stream: TcpStream, broker: Arc<Broker>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut w = w;

    let hello: RelayHello = match read_frame(&mut r).await? {
        Some(h) => h,
        None => return Ok(()),
    };
    let (account, secret) = match &hello {
        RelayHello::Device {
            account, secret, ..
        }
        | RelayHello::Client {
            account, secret, ..
        }
        | RelayHello::Data {
            account, secret, ..
        } => (account.clone(), secret.clone()),
    };
    if !broker.auth(&account, &secret) {
        write_frame(
            &mut w,
            &RelayMsg::Denied {
                reason: "bad account or secret".into(),
            },
        )
        .await
        .ok();
        return Ok(());
    }

    match hello {
        RelayHello::Device { device, .. } => {
            let (tx, mut rx) = mpsc::unbounded_channel::<RelayMsg>();
            let key = (account.clone(), device.clone());
            broker.devices.lock().await.insert(key.clone(), tx);
            write_frame(
                &mut w,
                &RelayMsg::Welcome {
                    device_id: device.clone(),
                },
            )
            .await?;
            info!("relay: device `{device}` online");
            // Push control messages to the device; notice its disconnect.
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(m) => write_frame(&mut w, &m).await?,
                        None => break,
                    },
                    frame = read_frame::<_, RelayMsg>(&mut r) => match frame {
                        Ok(Some(_)) => {}          // Pong / ignore
                        Ok(None) | Err(_) => break, // device gone
                    },
                }
            }
            broker.devices.lock().await.remove(&key);
            info!("relay: device `{device}` offline");
        }

        RelayHello::Client { device, .. } => {
            let ctrl = broker
                .devices
                .lock()
                .await
                .get(&(account.clone(), device.clone()))
                .cloned();
            let Some(ctrl) = ctrl else {
                write_frame(
                    &mut w,
                    &RelayMsg::SessionFailed {
                        reason: "device_offline".into(),
                    },
                )
                .await
                .ok();
                return Ok(());
            };
            let session = broker.next_session.fetch_add(1, Ordering::Relaxed);
            let (otx, orx) = oneshot::channel::<Halves>();
            broker.pending.lock().await.insert(session, otx);
            if ctrl.send(RelayMsg::NewSession { session }).is_err() {
                broker.pending.lock().await.remove(&session);
                write_frame(
                    &mut w,
                    &RelayMsg::SessionFailed {
                        reason: "device_offline".into(),
                    },
                )
                .await
                .ok();
                return Ok(());
            }
            match tokio::time::timeout(Duration::from_secs(15), orx).await {
                Ok(Ok((mut dr, mut dw))) => {
                    write_frame(&mut w, &RelayMsg::SessionReady).await?;
                    // Dumb byte bridge: client r/w ⇆ device data dr/dw.
                    let c2d = tokio::io::copy(&mut r, &mut dw);
                    let d2c = tokio::io::copy(&mut dr, &mut w);
                    tokio::select! { _ = c2d => {}, _ = d2c => {} }
                    info!("relay: session {session} for `{device}` closed");
                }
                _ => {
                    broker.pending.lock().await.remove(&session);
                    write_frame(
                        &mut w,
                        &RelayMsg::SessionFailed {
                            reason: "timeout".into(),
                        },
                    )
                    .await
                    .ok();
                }
            }
        }

        RelayHello::Data { session, .. } => {
            // Hand this (buffered) stream to the waiting client task; it owns the
            // splice. If the session is stale, just drop the connection.
            if let Some(otx) = broker.pending.lock().await.remove(&session) {
                let _ = otx.send((r, w));
            }
        }
    }
    Ok(())
}

// ─────────────────────────── dial helpers (device + client) ───────────────────────────

/// Client side: dial the relay, ask for `device`, and return the raw stream
/// halves once the relay says `SessionReady`. Hand these to `connect_io`.
pub async fn open_client(
    addr: &str,
    account: &str,
    secret: &str,
    device: &str,
) -> Result<Halves> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut w = w;
    write_frame(
        &mut w,
        &RelayHello::Client {
            proto: PROTO,
            account: account.into(),
            secret: secret.into(),
            device: device.into(),
        },
    )
    .await?;
    loop {
        match read_frame::<_, RelayMsg>(&mut r).await? {
            Some(RelayMsg::SessionReady) => return Ok((r, w)),
            Some(RelayMsg::Welcome { .. }) | Some(RelayMsg::Ping) | Some(RelayMsg::Pong) => continue,
            Some(RelayMsg::SessionFailed { reason }) => bail!("relay: {reason}"),
            Some(RelayMsg::Denied { reason }) => bail!("relay denied: {reason}"),
            Some(RelayMsg::NewSession { .. }) => continue,
            None => bail!("relay closed before session ready"),
        }
    }
}

/// Device side: dial the relay and register as `device`, returning the control
/// connection halves once `Welcome` arrives. The caller loops reading
/// `NewSession`/`Ping` on the reader and replies `Pong` on the writer.
pub async fn open_control(
    addr: &str,
    account: &str,
    secret: &str,
    device: &str,
) -> Result<Halves> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut w = w;
    write_frame(
        &mut w,
        &RelayHello::Device {
            proto: PROTO,
            account: account.into(),
            secret: secret.into(),
            device: device.into(),
        },
    )
    .await?;
    loop {
        match read_frame::<_, RelayMsg>(&mut r).await? {
            Some(RelayMsg::Welcome { .. }) => return Ok((r, w)),
            Some(RelayMsg::Denied { reason }) => bail!("relay denied: {reason}"),
            Some(_) => continue,
            None => bail!("relay closed before welcome"),
        }
    }
}

/// Device side: dial a fresh per-session data connection for `session` and
/// return its halves, ready to hand to `handle_conn`. The relay splices it to
/// the waiting client and sends nothing back on this connection.
pub async fn open_data(
    addr: &str,
    account: &str,
    secret: &str,
    session: u64,
) -> Result<Halves> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();
    let r = BufReader::new(r);
    let mut w = w;
    write_frame(
        &mut w,
        &RelayHello::Data {
            proto: PROTO,
            account: account.into(),
            secret: secret.into(),
            session,
        },
    )
    .await?;
    Ok((r, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_and_msg_round_trip() {
        let h = RelayHello::Client {
            proto: PROTO,
            account: "josh".into(),
            secret: "s".into(),
            device: "mini".into(),
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"type\":\"client\""));
        let back: RelayHello = serde_json::from_str(&s).unwrap();
        matches!(back, RelayHello::Client { .. });

        let m = RelayMsg::NewSession { session: 7 };
        let s = serde_json::to_string(&m).unwrap();
        let back: RelayMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, RelayMsg::NewSession { session: 7 }));
    }
}
