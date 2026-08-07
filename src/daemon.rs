use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, info};

use crate::protocol::*;

type Tx = UnboundedSender<String>;

enum SessionEvent {
    Output(Vec<u8>),
    Exited(u32),
}

struct PaneSession {
    info: PaneInfo,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    scrollback: VecDeque<u8>,
    subs: HashMap<u64, Tx>,
    last_output: std::time::Instant,
    /// Scrollback changed since the last disk flush.
    dirty: bool,
    /// Rendered screen state — activity classification reads what a user would
    /// actually see, not raw scrollback (old status lines linger there).
    screen: vt100::Parser,
}

/// Output has to be quiet this long before we classify working -> waiting/idle.
const QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "nu", "dash"];

/// Strip ANSI escape sequences (CSI + OSC) from raw terminal bytes.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            match bytes.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    if bytes.get(i) == Some(&0x1b) {
                        i += 1; // skip the \ of ST
                    }
                    i += 1;
                }
                Some(_) => i += 2,
                None => {}
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn scrollback_path(id: u64) -> std::path::PathBuf {
    let dir = ruckus_dir().join("scrollback");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{id}.bin"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Persisted {
    snapshot: Snapshot,
    next_id: u64,
}

/// Written on every tree change so a daemon restart can rebuild the world.
fn save_state(st: &State) {
    let p = Persisted { snapshot: st.snapshot(), next_id: st.next_id };
    if let Ok(json) = serde_json::to_vec(&p) {
        let dir = ruckus_dir();
        let tmp = dir.join("state.json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(tmp, dir.join("state.json"));
        }
    }
}

fn flush_scrollbacks(st: &mut State) {
    for (id, p) in st.panes.iter_mut() {
        if p.dirty {
            p.dirty = false;
            let bytes: Vec<u8> = p.scrollback.iter().copied().collect();
            let _ = std::fs::write(scrollback_path(*id), &bytes);
        }
    }
}

/// Rebuild spaces/tabs and respawn running panes from the last saved state.
fn restore_state(state: &Arc<Mutex<State>>, st: &mut State) -> bool {
    let path = ruckus_dir().join("state.json");
    let Ok(data) = std::fs::read(&path) else { return false };
    let Ok(p) = serde_json::from_slice::<Persisted>(&data) else { return false };
    st.next_id = st.next_id.max(p.next_id);

    let mut alive: Vec<u64> = Vec::new();
    for info in &p.snapshot.panes {
        if info.status != PaneStatus::Running {
            let _ = std::fs::remove_file(scrollback_path(info.id));
            continue;
        }
        let mut sb: VecDeque<u8> = std::fs::read(scrollback_path(info.id))
            .map(VecDeque::from)
            .unwrap_or_default();
        sb.extend(b"\r\n\x1b[2m-- ruckus: daemon restarted, process respawned --\x1b[0m\r\n".iter());
        match spawn_pane_with_id(state, st, info.id, info.cmd.clone(), Some(info.cwd.clone()), Some(sb))
        {
            Ok(()) => alive.push(info.id),
            Err(e) => error!("restore: failed to respawn pane {}: {e:#}", info.id),
        }
    }

    for sp in &p.snapshot.spaces {
        let mut tabs = Vec::new();
        for t in &sp.tabs {
            let mut leaves = Vec::new();
            t.layout.leaves(&mut leaves);
            let mut layout = Some(t.layout.clone());
            for l in leaves {
                if !alive.contains(&l) {
                    layout = layout.and_then(|n| n.remove_leaf(l));
                }
            }
            if let Some(layout) = layout {
                let active_pane = if layout.contains(t.active_pane) {
                    t.active_pane
                } else {
                    layout.first_leaf()
                };
                tabs.push(Tab { id: t.id, name: t.name.clone(), active_pane, layout });
            }
        }
        if !tabs.is_empty() {
            let active_tab = if tabs.iter().any(|t| t.id == sp.active_tab) {
                sp.active_tab
            } else {
                tabs[0].id
            };
            st.spaces.push(Space { id: sp.id, name: sp.name.clone(), active_tab, tabs });
        }
    }
    if !st.spaces.iter().any(|s| s.id == p.snapshot.active_space) {
        if let Some(first) = st.spaces.first() {
            st.active_space = first.id;
        }
    } else {
        st.active_space = p.snapshot.active_space;
    }
    info!("restored {} panes across {} spaces", alive.len(), st.spaces.len());
    !st.spaces.is_empty()
}

/// Fire a macOS system notification (no-op elsewhere).
fn notify_system(title: &str, msg: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!("display notification {msg:?} with title {title:?}");
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, msg);
    }
}

/// Classify a pane that has gone quiet: is it blocked on you, or just idle?
fn classify_quiet(p: &PaneSession) -> Activity {
    let text = p.screen.screen().contents();
    let prog = basename(p.info.cmd.first().map(String::as_str).unwrap_or(""));
    classify_tail(&prog, &text)
}

/// Pure classification over the ANSI-stripped tail of a pane's output.
/// Agent-aware: coding agents print "esc to interrupt" while running, and a
/// quiet agent without that marker is waiting on you.
pub(crate) fn classify_tail(prog: &str, text: &str) -> Activity {
    let recent: Vec<&str> = text
        .lines()
        .rev()
        .map(|l| l.trim_end_matches('\r').trim_end())
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .collect();
    let recent_lower = recent.join("\n").to_lowercase();

    // Agent working markers: still busy even while producing no output
    // (long tool calls run silent). Covers Claude Code, Codex, and friends.
    if recent_lower.contains("esc to interrupt")
        || recent_lower.contains("esc to cancel")
        || recent_lower.contains("ctrl+c to interrupt")
    {
        return Activity::Working;
    }

    let last = recent.first().copied().unwrap_or("");
    let lower = last.to_lowercase();

    // Explicit question / input markers win.
    if last.ends_with('?')
        || recent_lower.contains("(y/n")
        || recent_lower.contains("[y/n")
        || lower.contains("password")
        || lower.contains("continue?")
        || last.ends_with(':')
        || last.ends_with('╯') // bottom of a TUI input box (e.g. Claude Code)
    {
        return Activity::Waiting;
    }
    // Shell-prompt endings mean idle.
    if last.is_empty()
        || last.ends_with('$')
        || last.ends_with('%')
        || last.ends_with('#')
        || last.ends_with('❯')
        || last.ends_with('➜')
    {
        return Activity::Idle;
    }
    // A non-shell command (an agent) that stopped streaming is waiting on you;
    // a shell sitting quiet is just idle.
    if SHELLS.contains(&prog) {
        Activity::Idle
    } else {
        Activity::Waiting
    }
}

struct Space {
    id: u64,
    name: String,
    active_tab: u64,
    tabs: Vec<Tab>,
}

struct Tab {
    id: u64,
    name: String,
    active_pane: u64,
    layout: Node,
}

struct State {
    spaces: Vec<Space>,
    active_space: u64,
    panes: HashMap<u64, PaneSession>,
    conns: HashMap<u64, Tx>,
    next_id: u64,
    notify_waiting: bool,
    notify_done: bool,
}

impl State {
    fn next(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            spaces: self
                .spaces
                .iter()
                .map(|s| SpaceInfo {
                    id: s.id,
                    name: s.name.clone(),
                    active_tab: s.active_tab,
                    tabs: s
                        .tabs
                        .iter()
                        .map(|t| TabInfo {
                            id: t.id,
                            name: t.name.clone(),
                            active_pane: t.active_pane,
                            layout: t.layout.clone(),
                        })
                        .collect(),
                })
                .collect(),
            active_space: self.active_space,
            panes: self.panes.values().map(|p| p.info.clone()).collect(),
        }
    }
}

pub async fn run() -> Result<()> {
    let dir = ruckus_dir();
    let logfile = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Mutex::new(logfile))
        .init();

    let sock = socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        info!("daemon already running, exiting");
        return Ok(());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    info!("ruckus daemon listening on {}", sock.display());

    let cfg = crate::config::Config::load();
    let state = Arc::new(Mutex::new(State {
        spaces: Vec::new(),
        active_space: 0,
        panes: HashMap::new(),
        conns: HashMap::new(),
        next_id: 1,
        notify_waiting: cfg.notify.system && cfg.notify.events.iter().any(|e| e == "waiting"),
        notify_done: cfg.notify.system && cfg.notify.events.iter().any(|e| e == "done"),
    }));

    {
        let mut st = state.lock().unwrap();
        restore_state(&state, &mut st);
        if let Err(e) = ensure_nonempty(&state, &mut st) {
            error!("failed to create default space: {e:#}");
        }
        save_state(&st);
    }

    // Activity ticker: demote panes from working -> waiting/idle once quiet.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                n += 1;
                let mut st = state.lock().unwrap();
                let mut changes = Vec::new();
                for (id, p) in st.panes.iter_mut() {
                    if p.info.status == PaneStatus::Running
                        && p.info.activity == Activity::Working
                        && p.last_output.elapsed() >= QUIET_AFTER
                    {
                        let next = classify_quiet(p);
                        if next != p.info.activity {
                            p.info.activity = next;
                            changes.push((*id, next));
                        }
                    }
                }
                for (pane, activity) in &changes {
                    broadcast(&st, ServerMsg::Activity { pane: *pane, activity: *activity });
                }
                for (pane, activity) in changes {
                    if activity == Activity::Waiting && st.notify_waiting {
                        if let Some(p) = st.panes.get(&pane) {
                            if p.subs.is_empty() {
                                notify_system(
                                    "ruckus",
                                    &format!("{} is waiting for you", p.info.title),
                                );
                            }
                        }
                    }
                }
                if n % 5 == 0 {
                    flush_scrollbacks(&mut st);
                }
            }
        });
    }

    let mut next_conn: u64 = 1;
    loop {
        let (stream, _) = listener.accept().await?;
        let conn_id = next_conn;
        next_conn += 1;
        info!("conn {conn_id}: accepted");
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(st, conn_id, stream).await {
                error!("conn {conn_id}: {e:#}");
            }
            info!("conn {conn_id}: closed");
        });
    }
}

async fn handle_conn(state: Arc<Mutex<State>>, conn_id: u64, stream: UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = unbounded_channel::<String>();
    state.lock().unwrap().conns.insert(conn_id, tx.clone());

    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ClientFrame>(&line) {
            Ok(frame) => {
                info!("conn {conn_id}: request {:?}", frame.req);
                let msg = handle_request(&state, conn_id, frame.req);
                info!("conn {conn_id}: responding");
                send(&tx, Some(frame.seq), msg);
            }
            Err(e) => send(&tx, None, ServerMsg::Error { message: format!("bad request: {e}") }),
        }
    }

    {
        let mut st = state.lock().unwrap();
        st.conns.remove(&conn_id);
        for p in st.panes.values_mut() {
            p.subs.remove(&conn_id);
        }
    }
    writer_task.abort();
    Ok(())
}

fn send(tx: &Tx, seq: Option<u64>, msg: ServerMsg) {
    match serde_json::to_string(&ServerFrame { seq, msg }) {
        Ok(s) => {
            let _ = tx.send(s);
        }
        Err(e) => error!("failed to serialize response: {e}"),
    }
}

fn broadcast(st: &State, msg: ServerMsg) {
    match serde_json::to_string(&ServerFrame { seq: None, msg }) {
        Ok(s) => {
            for tx in st.conns.values() {
                let _ = tx.send(s.clone());
            }
        }
        Err(e) => error!("failed to serialize broadcast: {e}"),
    }
}

fn broadcast_state(st: &State) {
    broadcast(st, ServerMsg::State { snapshot: st.snapshot() });
    save_state(st);
}

fn err(message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error { message: message.into() }
}

fn handle_request(state: &Arc<Mutex<State>>, conn_id: u64, req: Request) -> ServerMsg {
    match req {
        Request::Snapshot => {
            let st = state.lock().unwrap();
            ServerMsg::State { snapshot: st.snapshot() }
        }
        Request::NewSpace { name, cwd } => new_space(state, name, cwd)
            .unwrap_or_else(|e| err(format!("{e:#}"))),
        Request::NewTab { space, name, cmd, cwd } => new_tab(state, space, name, cmd, cwd)
            .unwrap_or_else(|e| err(format!("{e:#}"))),
        Request::Split { pane, dir, cmd, cwd } => split(state, pane, dir, cmd, cwd)
            .unwrap_or_else(|e| err(format!("{e:#}"))),
        Request::SetLayout { tab, layout } => {
            let mut st = state.lock().unwrap();
            let Some(t) = st
                .spaces
                .iter_mut()
                .flat_map(|s| s.tabs.iter_mut())
                .find(|t| t.id == tab)
            else {
                return err(format!("no tab {tab}"));
            };
            let mut old = Vec::new();
            t.layout.leaves(&mut old);
            let mut new = Vec::new();
            layout.leaves(&mut new);
            let (mut a, mut b) = (old.clone(), new.clone());
            a.sort_unstable();
            b.sort_unstable();
            if a != b {
                return err("layout must contain exactly the tab's panes");
            }
            if !layout.valid_weights() {
                return err("weights must match children and be non-zero");
            }
            t.layout = layout;
            broadcast_state(&st);
            ServerMsg::Done
        }
        Request::RenameSpace { space, name } => {
            let mut st = state.lock().unwrap();
            let Some(s) = st.spaces.iter_mut().find(|s| s.id == space) else {
                return err(format!("no space {space}"));
            };
            s.name = name;
            broadcast_state(&st);
            ServerMsg::Done
        }
        Request::RenameTab { tab, name } => {
            let mut st = state.lock().unwrap();
            let Some(t) = st
                .spaces
                .iter_mut()
                .flat_map(|s| s.tabs.iter_mut())
                .find(|t| t.id == tab)
            else {
                return err(format!("no tab {tab}"));
            };
            t.name = name;
            broadcast_state(&st);
            ServerMsg::Done
        }
        Request::Restart { pane } => {
            let mut st = state.lock().unwrap();
            let Some(p) = st.panes.get(&pane) else {
                return err(format!("no pane {pane}"));
            };
            if p.info.status == PaneStatus::Running {
                return err("pane is still running (close it first, or wait for it to exit)");
            }
            let (cmd, cwd) = (p.info.cmd.clone(), p.info.cwd.clone());
            let mut sb = p.scrollback.clone();
            sb.extend(b"\r\n\x1b[2m-- ruckus: restarted --\x1b[0m\r\n".iter());
            match spawn_pane_with_id(state, &mut st, pane, cmd, Some(cwd), Some(sb)) {
                Ok(()) => {
                    broadcast_state(&st);
                    ServerMsg::Done
                }
                Err(e) => err(format!("restart failed: {e:#}")),
            }
        }
        Request::ClosePane { pane } => close_pane(state, pane)
            .unwrap_or_else(|e| err(format!("{e:#}"))),
        Request::SetActive { space, tab, pane } => {
            let mut st = state.lock().unwrap();
            if st.spaces.iter().any(|s| s.id == space) {
                st.active_space = space;
            }
            if let Some(s) = st.spaces.iter_mut().find(|s| s.id == space) {
                if s.tabs.iter().any(|t| t.id == tab) {
                    s.active_tab = tab;
                }
                if let Some(t) = s.tabs.iter_mut().find(|t| t.id == tab) {
                    if t.layout.contains(pane) {
                        t.active_pane = pane;
                    }
                }
            }
            broadcast_state(&st);
            ServerMsg::Done
        }
        Request::Attach { pane, rows, cols } => {
            let mut st = state.lock().unwrap();
            let Some(tx) = st.conns.get(&conn_id).cloned() else {
                return err("connection not registered");
            };
            let Some(p) = st.panes.get_mut(&pane) else {
                return err(format!("no pane {pane}"));
            };
            let _ = p.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            p.screen.set_size(rows, cols);
            p.subs.insert(conn_id, tx);
            let scrollback = B64.encode(p.scrollback.make_contiguous());
            ServerMsg::Attached { pane, scrollback }
        }
        Request::Detach { pane } => {
            let mut st = state.lock().unwrap();
            if let Some(p) = st.panes.get_mut(&pane) {
                p.subs.remove(&conn_id);
            }
            ServerMsg::Done
        }
        Request::Input { pane, data } => {
            let mut st = state.lock().unwrap();
            let Some(p) = st.panes.get_mut(&pane) else {
                return err(format!("no pane {pane}"));
            };
            let Ok(bytes) = B64.decode(&data) else {
                return err("bad base64");
            };
            match p.writer.write_all(&bytes) {
                Ok(_) => ServerMsg::Done,
                Err(e) => err(format!("write failed: {e}")),
            }
        }
        Request::Resize { pane, rows, cols } => {
            let mut st = state.lock().unwrap();
            let Some(p) = st.panes.get_mut(&pane) else {
                return err(format!("no pane {pane}"));
            };
            match p.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                Ok(_) => {
                    p.screen.set_size(rows, cols);
                    ServerMsg::Done
                }
                Err(e) => err(format!("resize failed: {e}")),
            }
        }
    }
}

/// Spawn a PTY + process; returns the new pane id. Caller must place it in the tree.
fn spawn_pane(
    state: &Arc<Mutex<State>>,
    st: &mut State,
    cmd: Vec<String>,
    cwd: Option<String>,
) -> Result<u64> {
    let id = st.next();
    spawn_pane_with_id(state, st, id, cmd, cwd, None)?;
    Ok(id)
}

/// Spawn (or respawn) a pane under a specific id. Replaces any existing session
/// with that id; `scrollback` seeds history (restore / restart).
fn spawn_pane_with_id(
    state: &Arc<Mutex<State>>,
    st: &mut State,
    id: u64,
    cmd: Vec<String>,
    cwd: Option<String>,
    scrollback: Option<VecDeque<u8>>,
) -> Result<()> {
    let cmdline = if cmd.is_empty() { vec![default_shell()] } else { cmd };
    let cwd = cwd
        .or_else(|| dirs::home_dir().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/".to_string());

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
    let mut builder = CommandBuilder::new(&cmdline[0]);
    builder.args(&cmdline[1..]);
    builder.env("TERM", "xterm-256color");
    builder.cwd(&cwd);
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow!("spawn {:?}: {e}", cmdline))?;
    drop(pair.slave);

    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    let title = format!("{}·{}", basename(&cmdline[0]), id);
    let info = PaneInfo {
        id,
        title,
        cmd: cmdline,
        cwd,
        status: PaneStatus::Running,
        activity: Activity::Working,
        created: unix_now(),
    };

    let mut screen = vt100::Parser::new(24, 80, 0);
    if let Some(sb) = &scrollback {
        let bytes: Vec<u8> = sb.iter().copied().collect();
        screen.process(&bytes);
    }

    let (ev_tx, ev_rx) = unbounded_channel::<SessionEvent>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ev_tx.send(SessionEvent::Output(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
        let _ = ev_tx.send(SessionEvent::Exited(code));
    });
    tokio::spawn(pump(state.clone(), id, ev_rx));

    st.panes.insert(
        id,
        PaneSession {
            info,
            master: pair.master,
            writer,
            killer,
            scrollback: scrollback.unwrap_or_default(),
            subs: HashMap::new(),
            last_output: std::time::Instant::now(),
            dirty: false,
            screen,
        },
    );
    Ok(())
}

async fn pump(state: Arc<Mutex<State>>, id: u64, mut rx: UnboundedReceiver<SessionEvent>) {
    while let Some(ev) = rx.recv().await {
        let mut st = state.lock().unwrap();
        match ev {
            SessionEvent::Output(bytes) => {
                let now_working = {
                    let Some(p) = st.panes.get_mut(&id) else { continue };
                    p.scrollback.extend(bytes.iter().copied());
                    while p.scrollback.len() > SCROLLBACK_MAX {
                        p.scrollback.pop_front();
                    }
                    p.last_output = std::time::Instant::now();
                    p.dirty = true;
                    p.screen.process(&bytes);
                    let flip = p.info.activity != Activity::Working
                        && p.info.status == PaneStatus::Running;
                    if flip {
                        p.info.activity = Activity::Working;
                    }
                    flip
                };
                if now_working {
                    broadcast(&st, ServerMsg::Activity { pane: id, activity: Activity::Working });
                }
                let Some(p) = st.panes.get_mut(&id) else { continue };
                if !p.subs.is_empty() {
                    let frame = serde_json::to_string(&ServerFrame {
                        seq: None,
                        msg: ServerMsg::Output { pane: id, data: B64.encode(&bytes) },
                    })
                    .unwrap();
                    p.subs.retain(|_, tx| tx.send(frame.clone()).is_ok());
                }
            }
            SessionEvent::Exited(code) => {
                let known = if let Some(p) = st.panes.get_mut(&id) {
                    p.info.status = PaneStatus::Exited { code };
                    p.info.activity = Activity::Done;
                    true
                } else {
                    false
                };
                if known {
                    broadcast(&st, ServerMsg::Exited { pane: id, code });
                    broadcast_state(&st);
                    if st.notify_done {
                        if let Some(p) = st.panes.get(&id) {
                            if p.subs.is_empty() {
                                notify_system(
                                    "ruckus",
                                    &format!("{} finished (exit {code})", p.info.title),
                                );
                            }
                        }
                    }
                }
                break;
            }
        }
    }
}

fn new_space(state: &Arc<Mutex<State>>, name: Option<String>, cwd: Option<String>) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let pane = spawn_pane(state, &mut st, Vec::new(), cwd)?;
    let tab_id = st.next();
    let space_id = st.next();
    let tab_name = st.panes[&pane].info.title.clone();
    st.spaces.push(Space {
        id: space_id,
        name: name.unwrap_or_else(|| format!("space·{space_id}")),
        active_tab: tab_id,
        tabs: vec![Tab { id: tab_id, name: tab_name, active_pane: pane, layout: Node::Leaf { pane } }],
    });
    st.active_space = space_id;
    broadcast_state(&st);
    Ok(ServerMsg::Created { space: space_id, tab: tab_id, pane })
}

fn new_tab(
    state: &Arc<Mutex<State>>,
    space: u64,
    name: Option<String>,
    cmd: Vec<String>,
    cwd: Option<String>,
) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    if !st.spaces.iter().any(|s| s.id == space) {
        return Ok(err(format!("no space {space}")));
    }
    let pane = spawn_pane(state, &mut st, cmd, cwd)?;
    let tab_id = st.next();
    let tab_name = name.unwrap_or_else(|| st.panes[&pane].info.title.clone());
    let s = st.spaces.iter_mut().find(|s| s.id == space).unwrap();
    s.tabs.push(Tab { id: tab_id, name: tab_name, active_pane: pane, layout: Node::Leaf { pane } });
    s.active_tab = tab_id;
    broadcast_state(&st);
    Ok(ServerMsg::Created { space, tab: tab_id, pane })
}

fn split(
    state: &Arc<Mutex<State>>,
    target: u64,
    dir: Dir,
    cmd: Vec<String>,
    cwd: Option<String>,
) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let Some((space_id, tab_id)) = locate(&st, target) else {
        return Ok(err(format!("pane {target} not in any tab")));
    };
    let cwd = cwd.or_else(|| st.panes.get(&target).map(|p| p.info.cwd.clone()));
    let pane = spawn_pane(state, &mut st, cmd, cwd)?;
    let s = st.spaces.iter_mut().find(|s| s.id == space_id).unwrap();
    let t = s.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
    t.layout.split_at(target, dir, pane);
    t.active_pane = pane;
    broadcast_state(&st);
    Ok(ServerMsg::Created { space: space_id, tab: tab_id, pane })
}

fn close_pane(state: &Arc<Mutex<State>>, pane: u64) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    if let Some(mut p) = st.panes.remove(&pane) {
        let _ = p.killer.kill();
    }
    let _ = std::fs::remove_file(scrollback_path(pane));
    let mut empty_spaces = Vec::new();
    for s in st.spaces.iter_mut() {
        s.tabs.retain_mut(|t| {
            if !t.layout.contains(pane) {
                return true;
            }
            match t.layout.clone().remove_leaf(pane) {
                Some(layout) => {
                    if t.active_pane == pane {
                        t.active_pane = layout.first_leaf();
                    }
                    t.layout = layout;
                    true
                }
                None => false,
            }
        });
        if !s.tabs.iter().any(|t| t.id == s.active_tab) {
            if let Some(first) = s.tabs.first() {
                s.active_tab = first.id;
            }
        }
        if s.tabs.is_empty() {
            empty_spaces.push(s.id);
        }
    }
    st.spaces.retain(|s| !empty_spaces.contains(&s.id));
    if !st.spaces.iter().any(|s| s.id == st.active_space) {
        if let Some(first) = st.spaces.first() {
            st.active_space = first.id;
        }
    }
    ensure_nonempty(state, &mut st)?;
    broadcast_state(&st);
    Ok(ServerMsg::Done)
}

fn ensure_nonempty(state: &Arc<Mutex<State>>, st: &mut State) -> Result<()> {
    if !st.spaces.is_empty() {
        return Ok(());
    }
    let pane = spawn_pane(state, st, Vec::new(), None)?;
    let tab_id = st.next();
    let space_id = st.next();
    let tab_name = st.panes[&pane].info.title.clone();
    st.spaces.push(Space {
        id: space_id,
        name: "main".to_string(),
        active_tab: tab_id,
        tabs: vec![Tab { id: tab_id, name: tab_name, active_pane: pane, layout: Node::Leaf { pane } }],
    });
    st.active_space = space_id;
    Ok(())
}

fn locate(st: &State, pane: u64) -> Option<(u64, u64)> {
    for s in &st.spaces {
        for t in &s.tabs {
            if t.layout.contains(pane) {
                return Some((s.id, t.id));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_and_osc() {
        assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m"), b"red");
        assert_eq!(strip_ansi(b"\x1b]0;title\x07text"), b"text");
        assert_eq!(strip_ansi(b"plain"), b"plain");
    }

    #[test]
    fn quiet_shell_prompt_is_idle() {
        assert_eq!(classify_tail("zsh", "josh@mac ruckus %"), Activity::Idle);
        assert_eq!(classify_tail("fish", "~/code ❯"), Activity::Idle);
        assert_eq!(classify_tail("bash", ""), Activity::Idle);
    }

    #[test]
    fn questions_are_waiting() {
        assert_eq!(classify_tail("python3", "continue? "), Activity::Waiting);
        assert_eq!(classify_tail("sh", "Overwrite? [y/N]"), Activity::Waiting);
        assert_eq!(classify_tail("ssh", "password:"), Activity::Waiting);
    }

    #[test]
    fn claude_working_marker_wins_even_when_quiet() {
        let tail = "✻ Cogitating…\n  (esc to interrupt)";
        assert_eq!(classify_tail("claude", tail), Activity::Working);
    }

    #[test]
    fn claude_input_box_is_waiting() {
        let tail = "some output\n╭────────╮\n│ >      │\n╰────────╯";
        assert_eq!(classify_tail("claude", tail), Activity::Waiting);
    }

    #[test]
    fn quiet_nonshell_defaults_to_waiting_and_shell_to_idle() {
        assert_eq!(classify_tail("cargo", "Compiling foo v0.1.0"), Activity::Waiting);
        assert_eq!(classify_tail("zsh", "some scrollback text"), Activity::Idle);
    }
}
