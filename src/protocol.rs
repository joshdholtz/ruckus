use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCROLLBACK_MAX: usize = 2 * 1024 * 1024;

pub fn ruckus_dir() -> PathBuf {
    let dir = match std::env::var("RUCKUS_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => dirs::home_dir().expect("no home directory").join(".ruckus"),
    };
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
    RenameSpace { space: u64, name: String },
    RenameTab { tab: u64, name: String },
    /// Respawn an exited pane's command in place (same pane id, scrollback kept).
    Restart { pane: u64 },
    ClosePane { pane: u64 },
    /// Close a whole tab (kills its panes; removes the space if it empties).
    CloseTab { tab: u64 },
    /// Close a whole space (kills all its panes).
    CloseSpace { space: u64 },
    /// Reorder a tab to index `to` within its space.
    MoveTab { tab: u64, to: usize },
    /// Reorder a space to index `to` in the space list.
    MoveSpace { space: u64, to: usize },
    SetActive { space: u64, tab: u64, pane: u64 },
    Attach { pane: u64, rows: u16, cols: u16 },
    Detach { pane: u64 },
    Input { pane: u64, data: String },
    Resize { pane: u64, rows: u16, cols: u16 },
    /// Detector seam: report an authoritative activity for a pane, overriding
    /// the built-in output heuristic. Any external process on the socket can send
    /// it. `state`: "working" | "waiting" | "idle" | "auto" ("auto" hands the pane
    /// back to heuristic detection). Lets OSC 133 / shell-integration and
    /// foreground-process detectors live as plugins instead of core behaviour.
    ReportActivity { pane: u64, state: String },
    /// Detector seam: report the agent running in a pane (e.g. "claude"), or
    /// `null` to clear. Sets PaneInfo.agent, which drives the AGENTS list.
    ReportAgent { pane: u64, name: Option<String> },
    /// Re-read config.toml: daemon refreshes its own settings and tells every
    /// attached client to reload and re-render.
    Reload,
    /// Zero-downtime upgrade: the daemon re-execs the current binary in place,
    /// keeping every pane's PTY + child process alive. Clients reconnect.
    Upgrade,
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
    /// Pushed to every client when config should be reloaded from disk.
    ConfigChanged,
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

    /// Swap two panes' positions in the layout tree.
    pub fn swap_leaves(&mut self, a: u64, b: u64) {
        match self {
            Node::Leaf { pane } => {
                if *pane == a {
                    *pane = b;
                } else if *pane == b {
                    *pane = a;
                }
            }
            Node::Split { children, .. } => {
                for c in children {
                    c.swap_leaves(a, b);
                }
            }
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

    pub fn valid_weights(&self) -> bool {
        match self {
            Node::Leaf { .. } => true,
            Node::Split { children, weights, .. } => {
                (weights.is_empty()
                    || (weights.len() == children.len() && weights.iter().all(|w| *w > 0)))
                    && children.iter().all(|c| c.valid_weights())
            }
        }
    }

    /// Replace the target leaf with a split of [target, new]; if the enclosing
    /// split already flows in `dir`, insert as a sibling instead of nesting.
    pub fn split_at(&mut self, target: u64, dir: Dir, new_pane: u64) -> bool {
        match self {
            Node::Leaf { pane } if *pane == target => {
                *self = Node::Split {
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
                children.iter_mut().any(|c| c.split_at(target, dir, new_pane))
            }
        }
    }

    /// Remove a leaf, collapsing single-child splits. None if the tree empties.
    pub fn remove_leaf(self, pane: u64) -> Option<Node> {
        match self {
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
                    .filter_map(|(c, w)| c.remove_leaf(pane).map(|n| (n, w)))
                    .collect();
                match kept.len() {
                    0 => None,
                    1 => Some(kept.into_iter().next().unwrap().0),
                    _ => {
                        let (children, weights): (Vec<Node>, Vec<u16>) =
                            kept.into_iter().unzip();
                        Some(Node::Split { dir, children, weights })
                    }
                }
            }
        }
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
    /// Detected agent name (e.g. "claude"), if a detector reported one — set by
    /// the foreground-process detector or an external plugin via report_agent.
    #[serde(default)]
    pub agent: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(p: u64) -> Node {
        Node::Leaf { pane: p }
    }

    #[test]
    fn split_nests_and_inserts_siblings() {
        let mut n = leaf(1);
        assert!(n.split_at(1, Dir::Right, 2));
        // same-dir split inserts a sibling, not a nested split
        assert!(n.split_at(2, Dir::Right, 3));
        match &n {
            Node::Split { dir, children, .. } => {
                assert_eq!(*dir, Dir::Right);
                assert_eq!(children.len(), 3);
            }
            _ => panic!("expected split"),
        }
        // cross-dir split nests
        assert!(n.split_at(2, Dir::Down, 4));
        let mut leaves = Vec::new();
        n.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 2, 4, 3]);
    }

    #[test]
    fn split_preserves_weights_when_inserting() {
        let mut n = Node::Split {
            dir: Dir::Right,
            children: vec![leaf(1), leaf(2)],
            weights: vec![30, 70],
        };
        assert!(n.split_at(2, Dir::Right, 3));
        match &n {
            Node::Split { weights, .. } => assert_eq!(weights, &vec![30, 70, 70]),
            _ => panic!(),
        }
    }

    #[test]
    fn remove_leaf_collapses() {
        let n = Node::Split {
            dir: Dir::Right,
            children: vec![leaf(1), leaf(2)],
            weights: vec![40, 60],
        };
        let n = n.remove_leaf(1).unwrap();
        assert!(matches!(n, Node::Leaf { pane: 2 }));
        assert!(leaf(5).remove_leaf(5).is_none());
    }

    #[test]
    fn remove_leaf_keeps_sibling_weights() {
        let n = Node::Split {
            dir: Dir::Down,
            children: vec![leaf(1), leaf(2), leaf(3)],
            weights: vec![10, 20, 30],
        };
        match n.remove_leaf(2).unwrap() {
            Node::Split { weights, children, .. } => {
                assert_eq!(weights, vec![10, 30]);
                assert_eq!(children.len(), 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn weights_validation() {
        assert!(leaf(1).valid_weights());
        let ok = Node::Split { dir: Dir::Right, children: vec![leaf(1), leaf(2)], weights: vec![] };
        assert!(ok.valid_weights());
        let bad_len =
            Node::Split { dir: Dir::Right, children: vec![leaf(1), leaf(2)], weights: vec![1] };
        assert!(!bad_len.valid_weights());
        let zero =
            Node::Split { dir: Dir::Right, children: vec![leaf(1), leaf(2)], weights: vec![0, 1] };
        assert!(!zero.valid_weights());
    }

    #[test]
    fn frames_roundtrip() {
        let frame = ClientFrame {
            seq: 7,
            req: Request::Split { pane: 3, dir: Dir::Down, cmd: vec!["claude".into()], cwd: None },
        };
        let s = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 7);
        assert!(matches!(back.req, Request::Split { pane: 3, dir: Dir::Down, .. }));

        // weights default when omitted on the wire
        let n: Node = serde_json::from_str(
            r#"{"kind":"split","dir":"right","children":[{"kind":"leaf","pane":1},{"kind":"leaf","pane":2}]}"#,
        )
        .unwrap();
        match n {
            Node::Split { weights, .. } => assert!(weights.is_empty()),
            _ => panic!(),
        }
    }
}
