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

/// Last non-empty line of a pane's recent output, ANSI-stripped.
fn tail_line(scrollback: &VecDeque<u8>) -> String {
    let tail: Vec<u8> = scrollback
        .iter()
        .skip(scrollback.len().saturating_sub(4096))
        .copied()
        .collect();
    let clean = strip_ansi(&tail);
    let text = String::from_utf8_lossy(&clean);
    text.lines()
        .rev()
        .map(|l| l.trim_end_matches('\r').trim_end())
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

/// Classify a pane that has gone quiet: is it blocked on you, or just idle?
fn classify_quiet(p: &PaneSession) -> Activity {
    let line = tail_line(&p.scrollback);
    let t = line.trim_end();
    let lower = t.to_lowercase();

    // Explicit question / input markers win.
    if t.ends_with('?')
        || lower.contains("(y/n")
        || lower.contains("[y/n")
        || lower.contains("password")
        || lower.contains("continue?")
        || t.ends_with(':')
        || t.ends_with('╯') // bottom of a TUI input box (e.g. Claude Code)
    {
        return Activity::Waiting;
    }
    // Shell-prompt endings mean idle.
    if t.is_empty()
        || t.ends_with('$')
        || t.ends_with('%')
        || t.ends_with('#')
        || t.ends_with('❯')
        || t.ends_with('➜')
        || t.ends_with("$ ")
    {
        return Activity::Idle;
    }
    // A non-shell command (an agent) that stopped streaming is waiting on you;
    // a shell sitting quiet is just idle.
    let prog = basename(p.info.cmd.first().map(String::as_str).unwrap_or(""));
    if SHELLS.contains(&prog.as_str()) {
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

    let state = Arc::new(Mutex::new(State {
        spaces: Vec::new(),
        active_space: 0,
        panes: HashMap::new(),
        conns: HashMap::new(),
        next_id: 1,
    }));

    {
        let mut st = state.lock().unwrap();
        if let Err(e) = ensure_nonempty(&state, &mut st) {
            error!("failed to create default space: {e:#}");
        }
    }

    // Activity ticker: demote panes from working -> waiting/idle once quiet.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tick.tick().await;
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
                for (pane, activity) in changes {
                    broadcast(&st, ServerMsg::Activity { pane, activity });
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
            if !valid_weights(&layout) {
                return err("weights must match children and be non-zero");
            }
            t.layout = layout;
            broadcast_state(&st);
            ServerMsg::Done
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
                Ok(_) => ServerMsg::Done,
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

    let id = st.next();
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
            scrollback: VecDeque::new(),
            subs: HashMap::new(),
            last_output: std::time::Instant::now(),
        },
    );
    Ok(id)
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
    split_at(&mut t.layout, target, dir, pane);
    t.active_pane = pane;
    broadcast_state(&st);
    Ok(ServerMsg::Created { space: space_id, tab: tab_id, pane })
}

fn close_pane(state: &Arc<Mutex<State>>, pane: u64) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    if let Some(mut p) = st.panes.remove(&pane) {
        let _ = p.killer.kill();
    }
    let mut empty_spaces = Vec::new();
    for s in st.spaces.iter_mut() {
        s.tabs.retain_mut(|t| {
            if !t.layout.contains(pane) {
                return true;
            }
            match remove_leaf(t.layout.clone(), pane) {
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

/// Replace the target leaf with a split of [target, new]; if the enclosing split
/// already flows in `dir`, insert as a sibling instead of nesting.
fn valid_weights(node: &Node) -> bool {
    match node {
        Node::Leaf { .. } => true,
        Node::Split { children, weights, .. } => {
            (weights.is_empty()
                || (weights.len() == children.len() && weights.iter().all(|w| *w > 0)))
                && children.iter().all(valid_weights)
        }
    }
}

fn split_at(node: &mut Node, target: u64, dir: Dir, new_pane: u64) -> bool {
    match node {
        Node::Leaf { pane } if *pane == target => {
            *node = Node::Split {
                dir,
                children: vec![Node::Leaf { pane: target }, Node::Leaf { pane: new_pane }],
                weights: Vec::new(),
            };
            true
        }
        Node::Leaf { .. } => false,
        Node::Split { dir: d, children, weights } => {
            if *d == dir {
                if let Some(idx) = children
                    .iter()
                    .position(|c| matches!(c, Node::Leaf { pane } if *pane == target))
                {
                    children.insert(idx + 1, Node::Leaf { pane: new_pane });
                    if !weights.is_empty() {
                        let w = weights[idx];
                        weights.insert(idx + 1, w);
                    }
                    return true;
                }
            }
            children.iter_mut().any(|c| split_at(c, target, dir, new_pane))
        }
    }
}

fn remove_leaf(node: Node, pane: u64) -> Option<Node> {
    match node {
        Node::Leaf { pane: p } if p == pane => None,
        leaf @ Node::Leaf { .. } => Some(leaf),
        Node::Split { dir, children, weights } => {
            let weights = if weights.len() == children.len() {
                weights
            } else {
                vec![1; children.len()]
            };
            let kept: Vec<(Node, u16)> = children
                .into_iter()
                .zip(weights)
                .filter_map(|(c, w)| remove_leaf(c, pane).map(|n| (n, w)))
                .collect();
            match kept.len() {
                0 => None,
                1 => Some(kept.into_iter().next().unwrap().0),
                _ => {
                    let (children, weights): (Vec<Node>, Vec<u16>) = kept.into_iter().unzip();
                    Some(Node::Split { dir, children, weights })
                }
            }
        }
    }
}
