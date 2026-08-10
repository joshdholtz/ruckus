use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, info};

use crate::protocol::*;
use crate::remote::{self, Origin};

type Tx = UnboundedSender<String>;

/// How to reach a remote: ssh host + options + the client's live SSH env. Kept in
/// `remote_specs` so a dropped connection can be auto-reconnected without a client.
#[derive(Clone)]
struct RemoteSpecEnv {
    host: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

/// A LIVE mirrored remote daemon reached over `ssh … ruckus __proxy`. The daemon
/// owns the SSH child (dropping this kills it) and a client to talk to the remote;
/// the cached `snapshot` is origin-prefixed and merged into `State::snapshot`.
struct RemoteConn {
    host: String,
    client: Arc<crate::client::Client>,
    /// kill_on_drop child; kept alive so the SSH survives client disconnects.
    _child: tokio::process::Child,
    snapshot: Snapshot,
}

/// A pane's PTY, owned as a raw master fd + child pid so it can be handed off
/// across a self-exec upgrade (portable-pty's handles can't survive exec, but a
/// bare fd + pid can — exec keeps open fds and child processes alive).
struct Pty {
    master_fd: RawFd,
    pid: i32,
    writer: File,
}

/// Wait for a child and return its exit code (1 if it didn't exit cleanly).
fn reap(pid: i32) -> u32 {
    let mut status = 0i32;
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r > 0 && libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else {
        1
    }
}

/// dup an fd; `cloexec` controls whether the copy survives exec.
fn dup_fd(fd: RawFd, cloexec: bool) -> std::io::Result<RawFd> {
    let new = unsafe { libc::dup(fd) };
    if new < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        let flags = if cloexec { libc::FD_CLOEXEC } else { 0 };
        libc::fcntl(new, libc::F_SETFD, flags);
    }
    Ok(new)
}

impl Pty {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws);
        }
    }

    /// Foreground process group of the terminal (for pane_current_command detection).
    fn fg_pgrp(&self) -> Option<i32> {
        let p = unsafe { libc::tcgetpgrp(self.master_fd) };
        if p > 1 {
            Some(p)
        } else {
            None
        }
    }

    fn kill(&self) {
        unsafe {
            libc::kill(self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGKILL);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // writer (a dup) closes itself; close the canonical master fd.
        unsafe {
            libc::close(self.master_fd);
        }
    }
}

enum SessionEvent {
    Output(Vec<u8>),
    Exited(u32),
}

/// One attached client (TUI / tail / plugin) subscribed to a pane's output.
struct Sub {
    tx: Tx,
    rows: u16,
    cols: u16,
}

struct PaneSession {
    info: PaneInfo,
    pty: Pty,
    scrollback: VecDeque<u8>,
    /// conn_id → subscriber. PTY size is the max of all attached sizes so a
    /// tiny `tail` client cannot shrink a full TUI (and vice-versa fights are
    /// resolved toward the largest viewer).
    subs: HashMap<u64, Sub>,
    last_output: std::time::Instant,
    /// Authoritative activity set by a detector/plugin (OSC 133, foreground-process,
    /// agent hook…). When `Some`, it overrides the output heuristic and the quiet
    /// ticker leaves this pane alone. `None` = heuristic-managed (the default).
    reported: Option<Activity>,
    /// When the PTY was last resized. Output right after a resize is just the
    /// program repainting (e.g. a shell redrawing its prompt), so it must NOT
    /// count as "working" — otherwise idle panes spin every time you switch view.
    resized_at: std::time::Instant,
    /// Throttle for the (expensive) full-screen agent-prompt scan: last scan time
    /// and its cached result. Heavy output (Claude Code redraws) would otherwise
    /// scan the whole screen thousands of times/sec under the state lock.
    last_prompt_scan: std::time::Instant,
    prompt_cached: bool,
    /// Scrollback changed since the last disk flush.
    dirty: bool,
    /// Rendered screen state — activity classification reads what a user would
    /// actually see, not raw scrollback (old status lines linger there).
    screen: vt100::Parser,
}

/// Output arriving within this window after a resize is treated as a repaint,
/// not fresh activity — so switching spaces/tabs doesn't flip idle panes to working.
const RESIZE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "nu", "dash"];

/// Built-in coding-agent command names. Quiet panes running these default to
/// `waiting` (they stopped streaming and likely need you). Quiet `cargo` /
/// `pytest` / etc. default to `idle` instead — no more false NEEDS YOU.
const KNOWN_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "aider",
    "goose",
    "opencode",
    "amp",
    "gemini",
    "cursor-agent",
    "cline",
    "cody",
    "continue",
    "q",
    "grok",
];

fn is_known_agent(prog: &str) -> bool {
    KNOWN_AGENTS.iter().any(|a| a.eq_ignore_ascii_case(prog))
}

/// Kill a pane's process and its process group. portable-pty runs the child in
/// its own session (`setsid`), so `kill(-pgid, …)` reaches the shell and
/// siblings that stayed in that group; SIGHUP mimics a terminal hangup so job
/// control shells tend to tear down foreground jobs too.
fn kill_pane_session(p: &mut PaneSession) {
    if let Some(pgid) = p.pty.fg_pgrp() {
        // Negative pid = whole process group.
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGHUP);
            let _ = libc::kill(-pgid, libc::SIGTERM);
        }
    }
    p.pty.kill();
}

/// Resize the PTY to the max dimensions requested by any attached subscriber.
/// No-op when nobody is attached (keeps the last size).
fn apply_max_attach_size(p: &mut PaneSession) {
    let (rows, cols) = p
        .subs
        .values()
        .fold((0u16, 0u16), |(r, c), s| (r.max(s.rows), c.max(s.cols)));
    if rows == 0 || cols == 0 {
        return;
    }
    p.pty.resize(rows, cols);
    p.resized_at = std::time::Instant::now();
    p.screen.set_size(rows, cols);
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

/// Handoff manifest for a self-exec upgrade: which live fds/pids map to which
/// panes, plus the listening socket fd, so the new binary can adopt them all.
#[derive(serde::Serialize, serde::Deserialize)]
struct Handoff {
    listener_fd: RawFd,
    panes: Vec<HandoffPane>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct HandoffPane {
    id: u64,
    fd: RawFd,
    pid: i32,
    rows: u16,
    cols: u16,
}

fn handoff_path() -> std::path::PathBuf {
    ruckus_dir().join("handoff.json")
}

/// Clear FD_CLOEXEC so the fd survives exec.
fn clear_cloexec(fd: RawFd) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC);
        }
    }
}

/// Zero-downtime upgrade: keep every pane's PTY + child alive by re-exec'ing the
/// current binary in place (exec preserves open fds and child processes). Writes
/// a handoff manifest, un-CLOEXECs the fds to keep, then execs. Never returns on
/// success (the process image is replaced).
fn do_upgrade(st: &mut State) -> Result<()> {
    flush_scrollbacks(st);
    save_state(st);
    let mut panes = Vec::new();
    for (id, p) in st.panes.iter() {
        let (rows, cols) = p.screen.screen().size();
        clear_cloexec(p.pty.master_fd);
        panes.push(HandoffPane {
            id: *id,
            fd: p.pty.master_fd,
            pid: p.pty.pid,
            rows,
            cols,
        });
    }
    clear_cloexec(st.listener_fd);
    let hf = Handoff {
        listener_fd: st.listener_fd,
        panes,
    };
    std::fs::write(handoff_path(), serde_json::to_vec(&hf)?)?;
    let exe = std::env::current_exe()?;
    use std::os::unix::process::CommandExt;
    let e = std::process::Command::new(exe)
        .arg("daemon")
        .env("RUCKUS_HANDOFF", "1")
        .exec();
    Err(anyhow!("exec failed: {e}"))
}

/// Written on every tree change so a daemon restart can rebuild the world.
/// Persists LOCAL state only — mirrored remotes are ephemeral (memory-only), so
/// they must never leak into state.json (else they'd "restore" as ghost spaces
/// with no backing panes).
fn save_state(st: &State) {
    let mut snapshot = st.snapshot();
    snapshot
        .spaces
        .retain(|s| remote::origin_of(s.id) == remote::LOCAL);
    snapshot
        .panes
        .retain(|p| remote::origin_of(p.id) == remote::LOCAL);
    snapshot.remote_hosts.clear();
    let p = Persisted {
        snapshot,
        next_id: st.next_id,
    };
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
fn restore_state(
    state: &Arc<Mutex<State>>,
    st: &mut State,
    handoff: Option<&HashMap<u64, HandoffPane>>,
) -> bool {
    let path = ruckus_dir().join("state.json");
    let Ok(data) = std::fs::read(&path) else {
        return false;
    };
    let Ok(p) = serde_json::from_slice::<Persisted>(&data) else {
        return false;
    };
    st.next_id = st.next_id.max(p.next_id);

    let mut alive: Vec<u64> = Vec::new();
    for info in &p.snapshot.panes {
        if info.status != PaneStatus::Running {
            let _ = std::fs::remove_file(scrollback_path(info.id));
            continue;
        }
        // Upgrade path: adopt the still-running PTY. Cold start: respawn.
        let result = if let Some(hp) = handoff.and_then(|m| m.get(&info.id)) {
            adopt_pane(state, st, info.clone(), hp.fd, hp.pid, hp.rows, hp.cols)
        } else {
            let mut sb: VecDeque<u8> = std::fs::read(scrollback_path(info.id))
                .map(VecDeque::from)
                .unwrap_or_default();
            sb.extend(
                b"\r\n\x1b[2m-- ruckus: daemon restarted, process respawned --\x1b[0m\r\n".iter(),
            );
            spawn_pane_with_id(
                state,
                st,
                info.id,
                info.cmd.clone(),
                Some(info.cwd.clone()),
                Some(sb),
            )
        };
        match result {
            Ok(()) => alive.push(info.id),
            Err(e) => error!("restore: failed to restore pane {}: {e:#}", info.id),
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
                tabs.push(Tab {
                    id: t.id,
                    name: t.name.clone(),
                    active_pane,
                    layout,
                });
            }
        }
        if !tabs.is_empty() {
            let active_tab = if tabs.iter().any(|t| t.id == sp.active_tab) {
                sp.active_tab
            } else {
                tabs[0].id
            };
            st.spaces.push(Space {
                id: sp.id,
                name: sp.name.clone(),
                active_tab,
                tabs,
            });
        }
    }
    if !st.spaces.iter().any(|s| s.id == p.snapshot.active_space) {
        if let Some(first) = st.spaces.first() {
            st.active_space = first.id;
        }
    } else {
        st.active_space = p.snapshot.active_space;
    }
    info!(
        "restored {} panes across {} spaces",
        alive.len(),
        st.spaces.len()
    );
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

/// True when a single line looks like an interactive input prompt, not a log
/// line that happens to end with `:` (`Error:`, `Compiling foo:`, URLs, …).
fn looks_like_input_prompt(line: &str) -> bool {
    let t = line.trim_end();
    if t.is_empty() {
        return false;
    }
    if t.ends_with('?') || t.ends_with('╯') {
        // bottom of a TUI input box (e.g. Claude Code) ends with ╯
        return true;
    }
    let lower = t.to_lowercase();
    if lower.contains("password") || lower.contains("passphrase") || lower.contains("continue?") {
        return true;
    }
    // Trailing-colon prompts only when short and not an obvious log/status line.
    let Some(stripped) = t.strip_suffix(':') else {
        return false;
    };
    let stripped = stripped.trim_end();
    if stripped.is_empty() || stripped.len() > 48 {
        return false;
    }
    if stripped.contains("://") {
        return false;
    }
    if lower.starts_with("error")
        || lower.starts_with("warning")
        || lower.starts_with("note")
        || lower.starts_with("info")
        || lower.starts_with("debug")
        || lower.starts_with("trace")
        || lower.contains("compiling")
        || lower.contains("downloading")
        || lower.contains("fetching")
        || lower.contains("building")
        || lower.contains("running ")
    {
        return false;
    }
    // Must contain a letter so pure timestamps / numbers don't match.
    stripped.chars().any(|c| c.is_alphabetic())
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

    // Agent PROMPTS asking for your input win first — these mean "waiting on you",
    // not "working". Covers Claude Code / Codex permission & selection prompts.
    // (Note: "esc to cancel" / "tab to amend" appear at a prompt; "esc to
    // interrupt" appears while actually running — those are different states.)
    if recent_lower.contains("esc to cancel")
        || recent_lower.contains("tab to amend")
        || recent_lower.contains("do you want to proceed")
        || recent_lower.contains("do you want to")
        || recent_lower.contains("(y/n")
        || recent_lower.contains("[y/n")
        || recent_lower.contains("❯ 1.")
        || recent_lower.contains("press enter")
    {
        return Activity::Waiting;
    }

    // Agent working markers: still busy even while producing no output
    // (long tool calls run silent). Covers Claude Code, Codex, and friends.
    if recent_lower.contains("esc to interrupt") || recent_lower.contains("ctrl+c to interrupt") {
        return Activity::Working;
    }

    let last = recent.first().copied().unwrap_or("");

    // Explicit question / input markers win (narrower than "any trailing colon").
    if looks_like_input_prompt(last) {
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
    // Quiet coding agents (or an agent name as the pane command) likely need you.
    // Quiet batch tools (`cargo`, `pytest`, `sleep`, …) are just idle — not NEEDS YOU.
    if SHELLS.contains(&prog) {
        Activity::Idle
    } else if is_known_agent(prog) {
        Activity::Waiting
    } else {
        Activity::Idle
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
    quiet_after: std::time::Duration,
    detect_osc133: bool,
    detect_foreground: bool,
    agent_commands: Vec<String>,
    /// Listening socket fd, handed off (un-CLOEXEC'd) across a self-exec upgrade.
    listener_fd: RawFd,
    /// Hybrid remote mirror: LIVE connections by origin; the known specs (persist
    /// across a dropped link so it auto-reconnects); a stable host→origin map (a
    /// reconnect reuses the same origin); and in-flight attempts.
    remotes: BTreeMap<Origin, RemoteConn>,
    remote_specs: BTreeMap<Origin, RemoteSpecEnv>,
    remote_origins: BTreeMap<String, Origin>,
    connecting: std::collections::HashSet<Origin>,
    next_origin: Origin,
}

impl State {
    fn next(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn snapshot(&self) -> Snapshot {
        let mut snap = Snapshot {
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
            panes: self
                .panes
                .values()
                .map(|p| {
                    let mut info = p.info.clone();
                    info.preview = pane_preview(&p.screen);
                    info.git_branch = git_branch(&p.info.cwd);
                    info
                })
                .collect(),
            remote_hosts: BTreeMap::new(),
        };
        // Merge each mirrored remote (its cached snapshot is already
        // origin-prefixed) and expose origin→host so clients tag remote rows.
        for (origin, rc) in &self.remotes {
            snap.spaces.extend(rc.snapshot.spaces.iter().cloned());
            snap.panes.extend(rc.snapshot.panes.iter().cloned());
            snap.remote_hosts.insert(*origin, rc.host.clone());
        }
        snap
    }
}

/// Git branch of `cwd` (or a short detached SHA), by walking up to the repo and
/// reading `.git/HEAD` — no subprocess. Empty when not inside a repo.
fn git_branch(cwd: &str) -> String {
    let mut dir = std::path::PathBuf::from(cwd);
    loop {
        if let Ok(head) = std::fs::read_to_string(dir.join(".git/HEAD")) {
            let head = head.trim();
            return match head.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => head.chars().take(7).collect(), // detached HEAD
            };
        }
        if !dir.pop() {
            return String::new();
        }
    }
}

/// Last non-empty line of a pane's rendered screen, trimmed and length-capped —
/// the one-line preview shown on mobile deck cards.
fn pane_preview(screen: &vt100::Parser) -> String {
    let text = screen.screen().contents();
    let line = text
        .lines()
        .rev()
        .map(|l| l.trim_end())
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    line.chars().take(80).collect()
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
    // Upgrade handoff: adopt the inherited listener + live panes from the old image.
    let handoff: Option<Handoff> = if std::env::var("RUCKUS_HANDOFF").is_ok() {
        std::fs::read(handoff_path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
    } else {
        None
    };
    let listener = if let Some(hf) = &handoff {
        let std_l = unsafe { std::os::unix::net::UnixListener::from_raw_fd(hf.listener_fd) };
        std_l.set_nonblocking(true)?;
        info!(
            "ruckus daemon adopted {} panes across upgrade",
            hf.panes.len()
        );
        UnixListener::from_std(std_l)?
    } else {
        if UnixStream::connect(&sock).await.is_ok() {
            info!("daemon already running, exiting");
            return Ok(());
        }
        let _ = std::fs::remove_file(&sock);
        UnixListener::bind(&sock)?
    };
    let listener_fd = listener.as_raw_fd();
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
        quiet_after: std::time::Duration::from_millis(cfg.ui.activity_quiet_ms),
        detect_osc133: cfg.ui.detect_osc133,
        detect_foreground: cfg.ui.detect_foreground,
        agent_commands: cfg.ui.agent_commands.clone(),
        listener_fd,
        remotes: BTreeMap::new(),
        remote_specs: BTreeMap::new(),
        remote_origins: BTreeMap::new(),
        connecting: std::collections::HashSet::new(),
        next_origin: 0,
    }));

    {
        let mut st = state.lock().unwrap();
        let hmap: Option<HashMap<u64, HandoffPane>> = handoff
            .as_ref()
            .map(|hf| hf.panes.iter().map(|p| (p.id, p.clone())).collect());
        restore_state(&state, &mut st, hmap.as_ref());
        if let Err(e) = ensure_nonempty(&state, &mut st) {
            error!("failed to create default space: {e:#}");
        }
        save_state(&st);
    }
    let _ = std::fs::remove_file(handoff_path());

    // Remote reconnect ticker: re-dial any known remote whose link dropped. The
    // spec (incl. the client's SSH env) is retained, so this works with no client
    // attached — as long as those credentials are still valid.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                let todo: Vec<(Origin, RemoteSpecEnv)> = {
                    let mut st = state.lock().unwrap();
                    let pending: Vec<(Origin, RemoteSpecEnv)> = st
                        .remote_specs
                        .iter()
                        .filter(|(o, _)| !st.remotes.contains_key(o) && !st.connecting.contains(o))
                        .map(|(o, s)| (*o, s.clone()))
                        .collect();
                    for (o, _) in &pending {
                        st.connecting.insert(*o);
                    }
                    pending
                };
                for (origin, spec) in todo {
                    spawn_remote_connect(state.clone(), origin, spec);
                }
            }
        });
    }

    // Activity ticker: demote panes from working -> waiting/idle once quiet.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                n += 1;
                let mut st = state.lock().unwrap();
                let quiet_after = st.quiet_after;
                let mut changes = Vec::new();
                for (id, p) in st.panes.iter_mut() {
                    if p.reported.is_none()
                        && p.info.status == PaneStatus::Running
                        && p.info.activity == Activity::Working
                        && p.last_output.elapsed() >= quiet_after
                    {
                        let next = classify_quiet(p);
                        if next != p.info.activity {
                            p.info.activity = next;
                            p.info.activity_since = unix_now();
                            changes.push((*id, next));
                        }
                    }
                }
                for (pane, activity) in &changes {
                    broadcast(
                        &st,
                        ServerMsg::Activity {
                            pane: *pane,
                            activity: *activity,
                        },
                    );
                }
                for (pane, activity) in changes {
                    if activity == Activity::Waiting && st.notify_waiting {
                        if let Some(p) = st.panes.get(&pane) {
                            if p.subs.is_empty() {
                                notify_system("ruckus", &format!("🐏 {} needs you", p.info.title));
                            }
                        }
                    }
                }
                // ~ every 5s (ticker runs at 250ms)
                if n.is_multiple_of(20) {
                    flush_scrollbacks(&mut st);
                }
            }
        });
    }

    // Process probe (~1 Hz): live cwd for every running pane, plus (opt-in)
    // foreground-command agent detection like tmux's pane_current_command.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(1000));
            loop {
                tick.tick().await;
                let (targets, detect_fg) = {
                    let st = state.lock().unwrap();
                    let targets: Vec<(u64, i32)> = st
                        .panes
                        .iter()
                        .filter(|(_, p)| p.info.status == PaneStatus::Running)
                        .filter_map(|(id, p)| p.pty.fg_pgrp().map(|pid| (*id, pid)))
                        .collect();
                    (targets, st.detect_foreground)
                };
                if targets.is_empty() {
                    continue;
                }
                let pids: Vec<i32> = targets.iter().map(|(_, pid)| *pid).collect();
                let cwds = resolve_cwds(&pids);
                let names = if detect_fg {
                    resolve_fg_names(&pids)
                } else {
                    HashMap::new()
                };
                let mut st = state.lock().unwrap();
                let allow = st.agent_commands.clone();
                let mut changed = false;
                for (id, pid) in targets {
                    if let Some(cwd) = cwds.get(&pid) {
                        if let Some(p) = st.panes.get_mut(&id) {
                            if &p.info.cwd != cwd {
                                p.info.cwd = cwd.clone();
                                // Keep shell pane titles in sync with the directory.
                                let prog =
                                    p.info.cmd.first().map(|s| basename(s)).unwrap_or_default();
                                if SHELLS.contains(&prog.as_str())
                                    || matches!(prog.as_str(), "tcsh" | "ksh" | "pwsh")
                                {
                                    p.info.title = default_pane_title(
                                        p.info.cmd.first().map(String::as_str).unwrap_or("sh"),
                                        cwd,
                                    );
                                }
                                changed = true;
                            }
                        }
                    }
                    if !detect_fg {
                        continue;
                    }
                    let base = names.get(&pid).map(|s| basename(s)).unwrap_or_default();
                    // Only known coding agents count — not transient commands (git, ls…).
                    // An empty allowlist means "any non-shell command".
                    let is_agent = !base.is_empty()
                        && !SHELLS.contains(&base.as_str())
                        && (allow.is_empty()
                            || allow.iter().any(|a| a.eq_ignore_ascii_case(&base)));
                    let agent = if is_agent { Some(base) } else { None };
                    if let Some(p) = st.panes.get_mut(&id) {
                        if p.info.agent != agent {
                            p.info.agent = agent;
                            changed = true;
                        }
                    }
                }
                if changed {
                    broadcast_state(&st);
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

    // Read newline-delimited requests with a hard per-line cap, so a peer that
    // never sends a newline (or sends a giant payload) can't balloon the daemon.
    const MAX_LINE: usize = 16 * 1024 * 1024;
    let mut reader = BufReader::new(read_half);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let (found_nl, used, eof) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                (false, 0, true)
            } else if let Some(nl) = available.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..nl]);
                (true, nl + 1, false)
            } else {
                buf.extend_from_slice(available);
                (false, available.len(), false)
            }
        };
        reader.consume(used);
        if buf.len() > MAX_LINE {
            send(
                &tx,
                None,
                ServerMsg::Error {
                    message: "request too large".into(),
                },
            );
            break;
        }
        if found_nl {
            let line = String::from_utf8_lossy(&buf).into_owned();
            buf.clear();
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ClientFrame>(&line) {
                Ok(frame) => {
                    log_request(conn_id, &frame.req);
                    let mut req = frame.req;
                    // Route by the request's id origin: local ids (origin 0) are
                    // handled here; remote ids are stripped to local and forwarded
                    // to that daemon over its ssh pipe, then the reply re-prefixed.
                    let origin = remote::route_request(&mut req);
                    let msg = if origin == remote::LOCAL {
                        handle_request(&state, conn_id, req)
                    } else {
                        let client = state
                            .lock()
                            .unwrap()
                            .remotes
                            .get(&origin)
                            .map(|rc| rc.client.clone());
                        match client {
                            Some(c) => match c.request(req).await {
                                Ok(mut m) => {
                                    remote::prefix_servermsg(&mut m, origin);
                                    m
                                }
                                Err(e) => ServerMsg::Error {
                                    message: format!("remote {origin}: {e}"),
                                },
                            },
                            None => ServerMsg::Error {
                                message: format!("remote {origin} not connected"),
                            },
                        }
                    };
                    send(&tx, Some(frame.seq), msg);
                }
                Err(e) => send(
                    &tx,
                    None,
                    ServerMsg::Error {
                        message: format!("bad request: {e}"),
                    },
                ),
            }
        }
        if eof {
            break;
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

/// Log a request without dumping payloads — Input carries keystrokes/pastes we
/// must not write to disk, and other variants can be large.
fn log_request(conn_id: u64, req: &Request) {
    match req {
        Request::Input { pane, data } => {
            info!("conn {conn_id}: input pane {pane} ({} bytes)", data.len())
        }
        other => info!("conn {conn_id}: {other:?}"),
    }
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
    broadcast(
        st,
        ServerMsg::State {
            snapshot: st.snapshot(),
        },
    );
    save_state(st);
}

/// Kill a pane's process and drop its scrollback file (no layout cascade).
fn drop_pane(st: &mut State, pane: u64) {
    if let Some(mut p) = st.panes.remove(&pane) {
        kill_pane_session(&mut p);
        broadcast(st, ServerMsg::PaneClosed { pane });
    }
    let _ = std::fs::remove_file(scrollback_path(pane));
}

fn close_tab(state: &Arc<Mutex<State>>, tab: u64) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let leaves: Vec<u64> = {
        let mut v = Vec::new();
        let found = st.spaces.iter().flat_map(|s| &s.tabs).find(|t| t.id == tab);
        match found {
            Some(t) => t.layout.leaves(&mut v),
            None => return Ok(err(format!("no tab {tab}"))),
        }
        v
    };
    for p in leaves {
        drop_pane(&mut st, p);
    }
    let mut empty = Vec::new();
    for s in st.spaces.iter_mut() {
        let had = s.tabs.iter().any(|t| t.id == tab);
        s.tabs.retain(|t| t.id != tab);
        if had && !s.tabs.iter().any(|t| t.id == s.active_tab) {
            if let Some(f) = s.tabs.first() {
                s.active_tab = f.id;
            }
        }
        if s.tabs.is_empty() {
            empty.push(s.id);
        }
    }
    st.spaces.retain(|s| !empty.contains(&s.id));
    if !st.spaces.iter().any(|s| s.id == st.active_space) {
        if let Some(f) = st.spaces.first() {
            st.active_space = f.id;
        }
    }
    ensure_nonempty(state, &mut st)?;
    broadcast_state(&st);
    Ok(ServerMsg::Done)
}

fn close_space(state: &Arc<Mutex<State>>, space: u64) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let leaves: Vec<u64> = {
        let Some(s) = st.spaces.iter().find(|s| s.id == space) else {
            return Ok(err(format!("no space {space}")));
        };
        let mut v = Vec::new();
        for t in &s.tabs {
            t.layout.leaves(&mut v);
        }
        v
    };
    for p in leaves {
        drop_pane(&mut st, p);
    }
    st.spaces.retain(|s| s.id != space);
    if st.active_space == space {
        if let Some(f) = st.spaces.first() {
            st.active_space = f.id;
        }
    }
    ensure_nonempty(state, &mut st)?;
    broadcast_state(&st);
    Ok(ServerMsg::Done)
}

fn move_tab(state: &Arc<Mutex<State>>, tab: u64, to: usize) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let mut moved = false;
    for s in st.spaces.iter_mut() {
        if let Some(i) = s.tabs.iter().position(|t| t.id == tab) {
            let t = s.tabs.remove(i);
            let to = to.min(s.tabs.len());
            s.tabs.insert(to, t);
            moved = true;
            break;
        }
    }
    if !moved {
        return Ok(err(format!("no tab {tab}")));
    }
    broadcast_state(&st);
    Ok(ServerMsg::Done)
}

fn move_space(state: &Arc<Mutex<State>>, space: u64, to: usize) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let Some(i) = st.spaces.iter().position(|s| s.id == space) else {
        return Ok(err(format!("no space {space}")));
    };
    let s = st.spaces.remove(i);
    let to = to.min(st.spaces.len());
    st.spaces.insert(to, s);
    broadcast_state(&st);
    Ok(ServerMsg::Done)
}

fn err(message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error {
        message: message.into(),
    }
}

fn handle_request(state: &Arc<Mutex<State>>, conn_id: u64, req: Request) -> ServerMsg {
    match req {
        Request::Snapshot => {
            let st = state.lock().unwrap();
            ServerMsg::State {
                snapshot: st.snapshot(),
            }
        }
        Request::NewSpace { name, cwd } => {
            new_space(state, name, cwd).unwrap_or_else(|e| err(format!("{e:#}")))
        }
        Request::NewTab {
            space,
            name,
            cmd,
            cwd,
        } => new_tab(state, space, name, cmd, cwd).unwrap_or_else(|e| err(format!("{e:#}"))),
        Request::Split {
            pane,
            dir,
            cmd,
            cwd,
        } => split(state, pane, dir, cmd, cwd).unwrap_or_else(|e| err(format!("{e:#}"))),
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
        Request::ClosePane { pane } => {
            close_pane(state, pane).unwrap_or_else(|e| err(format!("{e:#}")))
        }
        Request::CloseTab { tab } => {
            close_tab(state, tab).unwrap_or_else(|e| err(format!("{e:#}")))
        }
        Request::CloseSpace { space } => {
            close_space(state, space).unwrap_or_else(|e| err(format!("{e:#}")))
        }
        Request::MoveTab { tab, to } => {
            move_tab(state, tab, to).unwrap_or_else(|e| err(format!("{e:#}")))
        }
        Request::MoveSpace { space, to } => {
            move_space(state, space, to).unwrap_or_else(|e| err(format!("{e:#}")))
        }
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
            broadcast(&st, ServerMsg::Focus { space, tab, pane });
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
            let rows = rows.max(1);
            let cols = cols.max(1);
            p.subs.insert(conn_id, Sub { tx, rows, cols });
            // Max-of-subscribers size: small clients cannot shrink a large TUI.
            apply_max_attach_size(p);
            let scrollback = B64.encode(p.scrollback.make_contiguous());
            ServerMsg::Attached { pane, scrollback }
        }
        Request::Detach { pane } => {
            let mut st = state.lock().unwrap();
            if let Some(p) = st.panes.get_mut(&pane) {
                p.subs.remove(&conn_id);
                // Recompute size from remaining subscribers (if any).
                apply_max_attach_size(p);
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
            match p.pty.write(&bytes) {
                Ok(_) => ServerMsg::Done,
                Err(e) => err(format!("write failed: {e}")),
            }
        }
        Request::Resize { pane, rows, cols } => {
            let mut st = state.lock().unwrap();
            let Some(p) = st.panes.get_mut(&pane) else {
                return err(format!("no pane {pane}"));
            };
            let rows = rows.max(1);
            let cols = cols.max(1);
            if let Some(sub) = p.subs.get_mut(&conn_id) {
                sub.rows = rows;
                sub.cols = cols;
                apply_max_attach_size(p);
            } else {
                // No active attach for this conn — apply size directly (scripted resize).
                p.pty.resize(rows, cols);
                p.resized_at = std::time::Instant::now();
                p.screen.set_size(rows, cols);
            }
            ServerMsg::Done
        }
        Request::ReportActivity {
            pane,
            state: report,
        } => {
            let mut st = state.lock().unwrap();
            if !st.panes.contains_key(&pane) {
                return err(format!("no pane {pane}"));
            }
            let change = st
                .panes
                .get_mut(&pane)
                .and_then(|p| apply_report(p, &report));
            if let Some(a) = change {
                broadcast(&st, ServerMsg::Activity { pane, activity: a });
            }
            ServerMsg::Done
        }
        Request::ReportAgent { pane, name } => {
            let mut st = state.lock().unwrap();
            let Some(p) = st.panes.get_mut(&pane) else {
                return err(format!("no pane {pane}"));
            };
            if p.info.agent != name {
                p.info.agent = name;
                broadcast_state(&st);
            }
            ServerMsg::Done
        }
        Request::Reload => {
            let mut st = state.lock().unwrap();
            let cfg = crate::config::Config::load();
            st.notify_waiting =
                cfg.notify.system && cfg.notify.events.iter().any(|e| e == "waiting");
            st.notify_done = cfg.notify.system && cfg.notify.events.iter().any(|e| e == "done");
            st.quiet_after = std::time::Duration::from_millis(cfg.ui.activity_quiet_ms);
            st.detect_osc133 = cfg.ui.detect_osc133;
            st.detect_foreground = cfg.ui.detect_foreground;
            st.agent_commands = cfg.ui.agent_commands.clone();
            info!("config reloaded; notifying {} clients", st.conns.len());
            broadcast(&st, ServerMsg::ConfigChanged);
            ServerMsg::Done
        }
        Request::Upgrade => {
            let mut st = state.lock().unwrap();
            info!("upgrading: re-exec keeping {} panes alive", st.panes.len());
            // On success this never returns (the process image is replaced).
            match do_upgrade(&mut st) {
                Ok(()) => ServerMsg::Done,
                Err(e) => err(format!("upgrade failed: {e}")),
            }
        }
        Request::ConnectRemote { host, args, env } => {
            let spec = RemoteSpecEnv {
                host: host.clone(),
                args,
                env,
            };
            let mut st = state.lock().unwrap();
            // Stable origin per host, so a reconnect reuses ids the client knows.
            let origin = match st.remote_origins.get(&host) {
                Some(o) => *o,
                None => {
                    st.next_origin += 1;
                    let o = st.next_origin;
                    st.remote_origins.insert(host.clone(), o);
                    o
                }
            };
            // Always refresh the spec (new SSH env may unstick a stale-cred reconnect).
            st.remote_specs.insert(origin, spec.clone());
            if st.remotes.contains_key(&origin) {
                return ServerMsg::Done; // already mirrored
            }
            if st.connecting.insert(origin) {
                spawn_remote_connect(state.clone(), origin, spec);
            }
            ServerMsg::Done
        }
        Request::DisconnectRemote { origin } => {
            let mut st = state.lock().unwrap();
            let host = st.remotes.get(&origin).map(|r| r.host.clone());
            st.remotes.remove(&origin); // drop → kill_on_drop kills the ssh
            st.remote_specs.remove(&origin); // forget → no auto-reconnect
            st.connecting.remove(&origin);
            if let Some(h) = host.or_else(|| {
                st.remote_origins
                    .iter()
                    .find(|(_, o)| **o == origin)
                    .map(|(h, _)| h.clone())
            }) {
                st.remote_origins.remove(&h);
            }
            broadcast_state(&st);
            ServerMsg::Done
        }
    }
}

/// Connect (or reconnect) a remote in the background: spawn `ssh … ruckus
/// __proxy` with the client's SSH env, fetch + origin-prefix its snapshot, merge
/// it in, then pump its events until the link drops. Never stalls the main loop.
fn spawn_remote_connect(state: Arc<Mutex<State>>, origin: Origin, spec: RemoteSpecEnv) {
    tokio::spawn(async move {
        let mut ssh_args = vec![
            "-o".to_string(),
            "ConnectTimeout=6".to_string(),
            // Detached daemon can't answer a prompt — fail fast to agent/pubkey.
            "-o".to_string(),
            "BatchMode=yes".to_string(),
        ];
        ssh_args.extend(spec.args.iter().cloned());
        info!("connect remote {} (origin {origin})", spec.host);
        let done = |state: &Arc<Mutex<State>>| {
            state.lock().unwrap().connecting.remove(&origin);
        };
        let (client, events, child) =
            match crate::client::connect_remote_env(&spec.host, &ssh_args, &spec.env).await {
                Ok(t) => t,
                Err(e) => {
                    error!("connect remote {}: ssh failed: {e:#}", spec.host);
                    return done(&state);
                }
            };
        let client = Arc::new(client);
        let mut snap = match client.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                error!("connect remote {}: snapshot failed: {e:#}", spec.host);
                return done(&state);
            }
        };
        remote::prefix_snapshot(&mut snap, origin);
        {
            let mut st = state.lock().unwrap();
            st.connecting.remove(&origin);
            // A concurrent disconnect may have forgotten this origin — respect it.
            if !st.remote_specs.contains_key(&origin) {
                return;
            }
            // A concurrent connect already landed this origin — don't double-mirror.
            if st.remotes.contains_key(&origin) {
                return;
            }
            st.remotes.insert(
                origin,
                RemoteConn {
                    host: spec.host.clone(),
                    client: client.clone(),
                    _child: child,
                    snapshot: snap,
                },
            );
            info!("remote {} mirrored (origin {origin})", spec.host);
            broadcast_state(&st);
        }
        remote_event_loop(state, origin, events).await;
    });
}

/// Max remote events coalesced into one lock acquisition (bounds work per batch).
const REMOTE_BATCH: usize = 256;

/// Pump one remote's event stream: origin-prefix each message, update the cached
/// snapshot on State, and forward to local clients.
///
/// Hot-path discipline (this was the freeze bug): a mirrored remote streams
/// output continuously, so we must NOT (a) hold the state mutex across I/O or
/// (b) relock per message. So we **batch** a burst into one short lock, coalesce
/// repeated State updates into a single merged broadcast, snapshot the client
/// senders, then release the lock BEFORE serializing/sending — and never
/// `save_state` here (remotes are ephemeral; one fsync per frame wedged it).
/// On stream end the live conn is dropped (reconnect ticker re-dials from spec).
async fn remote_event_loop(
    state: Arc<Mutex<State>>,
    origin: Origin,
    mut events: UnboundedReceiver<ServerMsg>,
) {
    while let Some(first) = events.recv().await {
        // Drain everything already queued so one lock handles the whole burst.
        let mut batch = vec![first];
        while batch.len() < REMOTE_BATCH {
            match events.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }

        let (frames, txs): (Vec<String>, Vec<Tx>) = {
            let Ok(mut st) = state.lock() else { return };
            if !st.remotes.contains_key(&origin) {
                return; // disconnected under us
            }
            let mut frames: Vec<String> = Vec::new();
            let mut state_changed = false;
            for msg in batch {
                match msg {
                    ServerMsg::State { mut snapshot } => {
                        remote::prefix_snapshot(&mut snapshot, origin);
                        if let Some(rc) = st.remotes.get_mut(&origin) {
                            rc.snapshot = snapshot;
                        }
                        state_changed = true; // coalesce: one merged State at the end
                    }
                    mut other => {
                        remote::prefix_servermsg(&mut other, origin);
                        if let Ok(s) = serde_json::to_string(&ServerFrame {
                            seq: None,
                            msg: other,
                        }) {
                            frames.push(s);
                        }
                    }
                }
            }
            if state_changed {
                if let Ok(s) = serde_json::to_string(&ServerFrame {
                    seq: None,
                    msg: ServerMsg::State {
                        snapshot: st.snapshot(),
                    },
                }) {
                    frames.push(s);
                }
            }
            let txs = st.conns.values().cloned().collect();
            (frames, txs)
        }; // lock released before any send

        for f in &frames {
            for tx in &txs {
                let _ = tx.send(f.clone());
            }
        }
    }

    let mut st = state.lock().unwrap();
    if st.remotes.remove(&origin).is_some() {
        info!("remote origin {origin} link dropped");
        broadcast_state(&st);
    }
}

/// Spawn a PTY + process; returns the new pane id. Caller must place it in the tree.
/// A human-friendly default pane title. Shells are named after their working
/// directory (the useful bit), everything else after the command itself.
fn default_pane_title(cmd: &str, cwd: &str) -> String {
    let base = basename(cmd);
    let is_shell = matches!(
        base.as_str(),
        "zsh" | "bash" | "sh" | "fish" | "dash" | "tcsh" | "ksh" | "nu" | "pwsh"
    );
    if !is_shell {
        return base;
    }
    if let Some(home) = dirs::home_dir() {
        if std::path::Path::new(cwd) == home {
            return "~".to_string();
        }
    }
    let dir = basename(cwd);
    if dir.is_empty() {
        "/".to_string()
    } else {
        dir
    }
}

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
    let cmdline = if cmd.is_empty() {
        vec![default_shell()]
    } else {
        cmd
    };
    let cwd = cwd
        .or_else(|| dirs::home_dir().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/".to_string());

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut builder = CommandBuilder::new(&cmdline[0]);
    builder.args(&cmdline[1..]);
    builder.env("TERM", "xterm-256color");
    // Context contract: anything running in a pane can drive ruckus back through
    // the socket (`ruckus …` / the JSON API) and knows which pane it is.
    builder.env("RUCKUS_SOCK", socket_path().display().to_string());
    builder.env("RUCKUS_DIR", ruckus_dir().display().to_string());
    builder.env("RUCKUS_PANE", id.to_string());
    builder.cwd(&cwd);
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow!("spawn {:?}: {e}", cmdline))?;
    drop(pair.slave);

    // Take ownership of the master fd as a bare, handoff-capable fd: dup a
    // non-CLOEXEC canonical copy (survives exec), then drop portable-pty's
    // wrapper (closing its fd) and forget the child (we reap via waitpid).
    let raw = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| anyhow!("pty has no raw fd"))?;
    let pid = child.process_id().unwrap_or(0) as i32;
    let master_fd = dup_fd(raw, false)?; // canonical, survives exec
    let reader_fd = dup_fd(raw, true)?; // reader thread (dies on exec)
    let writer_fd = dup_fd(raw, true)?; // writer (recreated on adopt)
    drop(pair.master);
    std::mem::forget(child);
    let reader = unsafe { File::from_raw_fd(reader_fd) };
    let writer = unsafe { File::from_raw_fd(writer_fd) };
    let pty = Pty {
        master_fd,
        pid,
        writer,
    };

    let title = default_pane_title(&cmdline[0], &cwd);
    let info = PaneInfo {
        id,
        title,
        cmd: cmdline,
        cwd,
        status: PaneStatus::Running,
        activity: Activity::Working,
        created: unix_now(),
        agent: None,
        preview: String::new(),
        activity_since: unix_now(),
        git_branch: String::new(),
    };
    install_pane(
        state,
        st,
        info,
        pty,
        reader,
        scrollback.unwrap_or_default(),
        24,
        80,
    );
    Ok(())
}

/// Wire a Pty into a PaneSession: start the reader thread (reap on EOF), the
/// pump task, seed the screen from scrollback, and insert it. Shared by fresh
/// spawns and post-upgrade adoption.
#[allow(clippy::too_many_arguments)]
fn install_pane(
    state: &Arc<Mutex<State>>,
    st: &mut State,
    info: PaneInfo,
    pty: Pty,
    mut reader: File,
    scrollback: VecDeque<u8>,
    rows: u16,
    cols: u16,
) {
    let id = info.id;
    let pid = pty.pid;
    let mut screen = vt100::Parser::new(rows.max(1), cols.max(1), 0);
    if !scrollback.is_empty() {
        let bytes: Vec<u8> = scrollback.iter().copied().collect();
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
        let _ = ev_tx.send(SessionEvent::Exited(reap(pid)));
    });
    tokio::spawn(pump(state.clone(), id, ev_rx));
    let now = std::time::Instant::now();
    st.panes.insert(
        id,
        PaneSession {
            info,
            pty,
            scrollback,
            subs: HashMap::new(),
            last_output: now,
            resized_at: now,
            reported: None,
            last_prompt_scan: now,
            prompt_cached: false,
            dirty: false,
            screen,
        },
    );
}

/// Adopt an already-running PTY (its master fd + child survived a self-exec
/// upgrade). Rebuilds the pane around the live fd instead of respawning.
fn adopt_pane(
    state: &Arc<Mutex<State>>,
    st: &mut State,
    info: PaneInfo,
    master_fd: RawFd,
    pid: i32,
    rows: u16,
    cols: u16,
) -> Result<()> {
    let reader = unsafe { File::from_raw_fd(dup_fd(master_fd, true)?) };
    let writer = unsafe { File::from_raw_fd(dup_fd(master_fd, true)?) };
    let pty = Pty {
        master_fd,
        pid,
        writer,
    };
    let scrollback: VecDeque<u8> = std::fs::read(scrollback_path(info.id))
        .map(VecDeque::from)
        .unwrap_or_default();
    install_pane(state, st, info, pty, reader, scrollback, rows, cols);
    Ok(())
}

/// Apply a detector's authoritative activity to a pane. Returns the new activity
/// if it changed (so the caller can broadcast). "auto" relinquishes to heuristics.
fn apply_report(p: &mut PaneSession, state: &str) -> Option<Activity> {
    let target = match state {
        "auto" => {
            p.reported = None;
            return None;
        }
        "working" => Activity::Working,
        "waiting" => Activity::Waiting,
        "idle" => Activity::Idle,
        _ => return None,
    };
    p.reported = Some(target);
    if p.info.status == PaneStatus::Running && p.info.activity != target {
        p.info.activity = target;
        p.info.activity_since = unix_now();
        Some(target)
    } else {
        None
    }
}

/// Scan a byte chunk for OSC 133 shell-integration marks (FinalTerm FTCS, the
/// same protocol iTerm2 / WezTerm / kitty / VS Code use) and return the activity
/// implied by the last relevant mark: `C` (command executed) = working,
/// `D` (finished) / `A` (prompt) = idle. Returns None if no mark is present.
fn scan_osc133(bytes: &[u8]) -> Option<&'static str> {
    let mut result = None;
    let mut i = 0;
    while i + 6 < bytes.len() {
        if bytes[i] == 0x1b
            && bytes[i + 1] == b']'
            && &bytes[i + 2..i + 5] == b"133"
            && bytes[i + 5] == b';'
        {
            match bytes[i + 6] {
                b'C' => result = Some("working"),
                b'D' | b'A' => result = Some("idle"),
                _ => {}
            }
            i += 6;
        } else {
            i += 1;
        }
    }
    result
}

/// Resolve foreground pids to their command name via one `ps` call
/// (pid -> comm; macOS gives a path, so callers basename it).
fn resolve_fg_names(pids: &[i32]) -> HashMap<i32, String> {
    let mut map = HashMap::new();
    if pids.is_empty() {
        return map;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "pid=,comm=", "-p", &list])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if let Some((pid_s, comm)) = line.split_once(char::is_whitespace) {
                if let Ok(pid) = pid_s.trim().parse::<i32>() {
                    map.insert(pid, comm.trim().to_string());
                }
            }
        }
    }
    map
}

/// Resolve each pid's current working directory. Linux uses `/proc/<pid>/cwd`;
/// macOS (and other Unix) falls back to a single batched `lsof`.
fn resolve_cwds(pids: &[i32]) -> HashMap<i32, String> {
    let mut map = HashMap::new();
    if pids.is_empty() {
        return map;
    }

    #[cfg(target_os = "linux")]
    {
        for &pid in pids {
            let link = format!("/proc/{pid}/cwd");
            if let Ok(path) = std::fs::read_link(&link) {
                map.insert(pid, path.display().to_string());
            }
        }
        return map;
    }

    #[cfg(not(target_os = "linux"))]
    {
        // lsof -a -d cwd -Fn -p p1,p2,…
        // emits blocks: p<pid>\n n<path>\n
        let list = pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let Ok(out) = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
            .output()
        else {
            return map;
        };
        let mut cur: Option<i32> = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(rest) = line.strip_prefix('p') {
                cur = rest.trim().parse().ok();
            } else if let Some(path) = line.strip_prefix('n') {
                if let Some(pid) = cur {
                    // lsof sometimes prefixes "n" already stripped; path is the rest.
                    if !path.is_empty() {
                        map.insert(pid, path.to_string());
                    }
                }
            }
        }
        map
    }
}

async fn pump(state: Arc<Mutex<State>>, id: u64, mut rx: UnboundedReceiver<SessionEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            SessionEvent::Output(bytes) => {
                // Encode the broadcast frame BEFORE taking the global lock. For a
                // large chunk the base64 + JSON is the dominant per-chunk cost;
                // doing it lock-free keeps one noisy pane from stalling every other
                // pane that contends on the single State mutex.
                let out_frame = serde_json::to_string(&ServerFrame {
                    seq: None,
                    msg: ServerMsg::Output {
                        pane: id,
                        data: B64.encode(&bytes),
                    },
                })
                .unwrap();

                // Hold the lock only for state mutation + collecting subscriber
                // handles. Fan-out and notifications run after unlock so a slow
                // client cannot stall every other pane on the single mutex.
                let (changed, notify_waiting_title, osc_change, sub_txs) = {
                    let mut st = state.lock().unwrap();
                    let notify_waiting = st.notify_waiting;
                    let detect_osc = st.detect_osc133;
                    let (changed, notify_title, sub_txs) = {
                        let Some(p) = st.panes.get_mut(&id) else {
                            continue;
                        };
                        p.scrollback.extend(bytes.iter().copied());
                        while p.scrollback.len() > SCROLLBACK_MAX {
                            p.scrollback.pop_front();
                        }
                        p.last_output = std::time::Instant::now();
                        p.dirty = true;
                        p.screen.process(&bytes);
                        // Throttle full-screen agent-prompt scans — Claude redraws
                        // would otherwise peg the daemon under the lock.
                        if p.last_prompt_scan.elapsed() >= std::time::Duration::from_millis(250) {
                            let text = p.screen.screen().contents().to_lowercase();
                            p.prompt_cached = text.contains("esc to cancel")
                                || text.contains("do you want to proceed")
                                || text.contains("tab to amend");
                            p.last_prompt_scan = std::time::Instant::now();
                        }
                        let waiting_prompt = p.prompt_cached;
                        // Ignore repaint bursts after resize (view switch).
                        let repaint = p.resized_at.elapsed() < RESIZE_GRACE;
                        let target = if waiting_prompt {
                            Some(Activity::Waiting)
                        } else if !repaint {
                            Some(Activity::Working)
                        } else {
                            None
                        };
                        // Detector owns this pane — don't let raw output override it.
                        let changed = match target {
                            Some(a)
                                if p.reported.is_none()
                                    && p.info.status == PaneStatus::Running
                                    && p.info.activity != a =>
                            {
                                p.info.activity = a;
                                p.info.activity_since = unix_now();
                                Some(a)
                            }
                            _ => None,
                        };
                        let notify_title = match changed {
                            Some(Activity::Waiting) if notify_waiting && p.subs.is_empty() => {
                                Some(p.info.title.clone())
                            }
                            _ => None,
                        };
                        let sub_txs: Vec<Tx> = p.subs.values().map(|s| s.tx.clone()).collect();
                        (changed, notify_title, sub_txs)
                    }; // p borrow ends here
                    let osc_change = if detect_osc {
                        scan_osc133(&bytes)
                            .and_then(|s| st.panes.get_mut(&id).and_then(|p| apply_report(p, s)))
                    } else {
                        None
                    };
                    (changed, notify_title, osc_change, sub_txs)
                };
                for tx in &sub_txs {
                    let _ = tx.send(out_frame.clone());
                }
                if changed.is_some() || osc_change.is_some() {
                    let st = state.lock().unwrap();
                    if let Some(a) = changed {
                        broadcast(
                            &st,
                            ServerMsg::Activity {
                                pane: id,
                                activity: a,
                            },
                        );
                    }
                    if let Some(a) = osc_change {
                        broadcast(
                            &st,
                            ServerMsg::Activity {
                                pane: id,
                                activity: a,
                            },
                        );
                    }
                }
                if let Some(title) = notify_waiting_title {
                    notify_system("ruckus", &format!("🐏 {title} needs you"));
                }
            }
            SessionEvent::Exited(code) => {
                let mut st = state.lock().unwrap();
                let known = if let Some(p) = st.panes.get_mut(&id) {
                    p.info.status = PaneStatus::Exited { code };
                    p.info.activity = Activity::Done;
                    p.info.activity_since = unix_now();
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
                                    &if code == 0 {
                                        format!("✓ {} finished", p.info.title)
                                    } else {
                                        format!("✗ {} exited ({code})", p.info.title)
                                    },
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

fn new_space(
    state: &Arc<Mutex<State>>,
    name: Option<String>,
    cwd: Option<String>,
) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    let pane = spawn_pane(state, &mut st, Vec::new(), cwd)?;
    let tab_id = st.next();
    let space_id = st.next();
    let tab_name = st.panes[&pane].info.title.clone();
    st.spaces.push(Space {
        id: space_id,
        name: name.unwrap_or_else(|| format!("space·{space_id}")),
        active_tab: tab_id,
        tabs: vec![Tab {
            id: tab_id,
            name: tab_name,
            active_pane: pane,
            layout: Node::Leaf { pane },
        }],
    });
    st.active_space = space_id;
    broadcast_state(&st);
    broadcast(
        &st,
        ServerMsg::PaneOpened {
            space: space_id,
            tab: tab_id,
            pane,
        },
    );
    Ok(ServerMsg::Created {
        space: space_id,
        tab: tab_id,
        pane,
    })
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
    s.tabs.push(Tab {
        id: tab_id,
        name: tab_name,
        active_pane: pane,
        layout: Node::Leaf { pane },
    });
    s.active_tab = tab_id;
    broadcast_state(&st);
    broadcast(
        &st,
        ServerMsg::PaneOpened {
            space,
            tab: tab_id,
            pane,
        },
    );
    Ok(ServerMsg::Created {
        space,
        tab: tab_id,
        pane,
    })
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
    broadcast(
        &st,
        ServerMsg::PaneOpened {
            space: space_id,
            tab: tab_id,
            pane,
        },
    );
    Ok(ServerMsg::Created {
        space: space_id,
        tab: tab_id,
        pane,
    })
}

fn close_pane(state: &Arc<Mutex<State>>, pane: u64) -> Result<ServerMsg> {
    let mut st = state.lock().unwrap();
    if let Some(mut p) = st.panes.remove(&pane) {
        kill_pane_session(&mut p);
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
        tabs: vec![Tab {
            id: tab_id,
            name: tab_name,
            active_pane: pane,
            layout: Node::Leaf { pane },
        }],
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
        assert_eq!(classify_tail("sh", "Enter host name:"), Activity::Waiting);
    }

    #[test]
    fn log_colons_are_not_waiting() {
        assert_eq!(
            classify_tail("cargo", "error: could not compile `foo`"),
            Activity::Idle
        );
        assert_eq!(
            classify_tail("app", "Error: something failed"),
            Activity::Idle
        );
        assert_eq!(
            classify_tail("app", "https://example.com/path:"),
            Activity::Idle
        );
        assert_eq!(
            classify_tail("cargo", "   Compiling foo v0.1.0"),
            Activity::Idle
        );
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
    fn quiet_agent_waits_batch_tools_idle() {
        // Known agent command quiet without markers → needs you.
        assert_eq!(
            classify_tail("claude", "some prior tool output"),
            Activity::Waiting
        );
        assert_eq!(classify_tail("codex", "thinking done"), Activity::Waiting);
        // Batch tools / unknown CLIs quiet → idle, not NEEDS YOU.
        assert_eq!(
            classify_tail("cargo", "Compiling foo v0.1.0"),
            Activity::Idle
        );
        assert_eq!(classify_tail("pytest", "...."), Activity::Idle);
        assert_eq!(classify_tail("sleep", ""), Activity::Idle);
        assert_eq!(classify_tail("zsh", "some scrollback text"), Activity::Idle);
    }

    #[test]
    fn looks_like_input_prompt_rules() {
        assert!(looks_like_input_prompt("password:"));
        assert!(looks_like_input_prompt("Name:"));
        assert!(looks_like_input_prompt("continue?"));
        assert!(!looks_like_input_prompt("Error: boom"));
        assert!(!looks_like_input_prompt("Compiling foo:"));
        assert!(!looks_like_input_prompt("https://x.com:"));
    }
}
