use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCROLLBACK_MAX: usize = 2 * 1024 * 1024;

pub fn ruckus_dir() -> PathBuf {
    let dir = dirs::home_dir().expect("no home directory").join(".ruckus");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn socket_path() -> PathBuf {
    ruckus_dir().join("ruckus.sock")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Snapshot,
    NewSpace { name: Option<String>, cwd: Option<String> },
    NewTab { space: u64, name: Option<String>, cmd: Vec<String>, cwd: Option<String> },
    Split { pane: u64, dir: Dir, cmd: Vec<String>, cwd: Option<String> },
    /// Replace a tab's layout (same set of panes, new arrangement/weights).
    SetLayout { tab: u64, layout: Node },
    ClosePane { pane: u64 },
    SetActive { space: u64, tab: u64, pane: u64 },
    Attach { pane: u64, rows: u16, cols: u16 },
    Detach { pane: u64 },
    Input { pane: u64, data: String },
    Resize { pane: u64, rows: u16, cols: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    State { snapshot: Snapshot },
    Created { space: u64, tab: u64, pane: u64 },
    Attached { pane: u64, scrollback: String },
    Done,
    Error { message: String },
    Output { pane: u64, data: String },
    Exited { pane: u64, code: u32 },
    Activity { pane: u64, activity: Activity },
}

/// Attention state of a pane, detected by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Producing output right now.
    Working,
    /// Quiet and it looks like it's blocked on input from you.
    Waiting,
    /// Quiet at a shell prompt / nothing happening.
    Idle,
    /// Process exited.
    Done,
}

impl Activity {
    /// Higher = more deserving of your attention (for aggregating tab/space state).
    pub fn urgency(self) -> u8 {
        match self {
            Activity::Waiting => 3,
            Activity::Done => 2,
            Activity::Working => 1,
            Activity::Idle => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub seq: u64,
    pub req: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub msg: ServerMsg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Right,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Leaf { pane: u64 },
    Split {
        dir: Dir,
        children: Vec<Node>,
        /// Relative size weights, one per child; equal when omitted.
        #[serde(default)]
        weights: Vec<u16>,
    },
}

impl Node {
    pub fn first_leaf(&self) -> u64 {
        match self {
            Node::Leaf { pane } => *pane,
            Node::Split { children, .. } => children[0].first_leaf(),
        }
    }

    pub fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf { pane } => out.push(*pane),
            Node::Split { children, .. } => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    pub fn contains(&self, pane: u64) -> bool {
        let mut v = Vec::new();
        self.leaves(&mut v);
        v.contains(&pane)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub spaces: Vec<SpaceInfo>,
    pub active_space: u64,
    pub panes: Vec<PaneInfo>,
}

impl Snapshot {
    pub fn pane(&self, id: u64) -> Option<&PaneInfo> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: u64) -> Option<&mut PaneInfo> {
        self.panes.iter_mut().find(|p| p.id == id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub id: u64,
    pub name: String,
    pub active_tab: u64,
    pub tabs: Vec<TabInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: u64,
    pub name: String,
    pub active_pane: u64,
    pub layout: Node,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u64,
    pub title: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub status: PaneStatus,
    pub activity: Activity,
    pub created: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PaneStatus {
    Running,
    Exited { code: u32 },
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

pub fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}
