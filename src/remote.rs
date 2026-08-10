//! Multi-daemon id namespacing — the pure boundary layer for the remote mirror
//! (see docs/REMOTE.md). The client uses bare `u64` ids everywhere; to merge
//! several daemons (each numbering from 1) we pack the *origin* into the high
//! bits of every id at the ingest/egress boundaries. `pack(0, id) == id`, so
//! local (origin 0) traffic is unchanged.
//!
//! Phase R0: pure + fully tested. Wired into the client in R1+.
#![allow(dead_code)] // wired in R1; kept pure + tested first

use crate::protocol::{Node, Request, ServerMsg, Snapshot};

/// Which daemon an id belongs to. 0 = the local daemon.
pub type Origin = u16;
pub const LOCAL: Origin = 0;

const ORIGIN_SHIFT: u32 = 48;
const LOCAL_MASK: u64 = (1u64 << ORIGIN_SHIFT) - 1;

/// Combine an origin + a daemon-local id into a global id.
#[inline]
pub fn pack(origin: Origin, local: u64) -> u64 {
    ((origin as u64) << ORIGIN_SHIFT) | (local & LOCAL_MASK)
}

/// The daemon an id belongs to.
#[inline]
pub fn origin_of(id: u64) -> Origin {
    (id >> ORIGIN_SHIFT) as Origin
}

/// The daemon-local id (strip the origin).
#[inline]
pub fn local_of(id: u64) -> u64 {
    id & LOCAL_MASK
}

fn prefix_node(n: &mut Node, origin: Origin) {
    match n {
        Node::Leaf { pane } => *pane = pack(origin, *pane),
        Node::Split { children, .. } => {
            for c in children {
                prefix_node(c, origin);
            }
        }
    }
}

/// Rewrite every id in a snapshot to global (packed) form. No-op for `LOCAL`.
pub fn prefix_snapshot(snap: &mut Snapshot, origin: Origin) {
    snap.active_space = pack(origin, snap.active_space);
    for s in &mut snap.spaces {
        s.id = pack(origin, s.id);
        s.active_tab = pack(origin, s.active_tab);
        for t in &mut s.tabs {
            t.id = pack(origin, t.id);
            t.active_pane = pack(origin, t.active_pane);
            prefix_node(&mut t.layout, origin);
        }
    }
    for p in &mut snap.panes {
        p.id = pack(origin, p.id);
    }
}

/// Rewrite every id in a pushed event to global form. No-op for `LOCAL`.
pub fn prefix_servermsg(msg: &mut ServerMsg, origin: Origin) {
    let g = |id: &mut u64| *id = pack(origin, *id);
    match msg {
        ServerMsg::State { snapshot } => prefix_snapshot(snapshot, origin),
        ServerMsg::Created { space, tab, pane } => {
            g(space);
            g(tab);
            g(pane);
        }
        ServerMsg::Attached { pane, .. } => g(pane),
        ServerMsg::Output { pane, .. } => g(pane),
        ServerMsg::Exited { pane, .. } => g(pane),
        ServerMsg::Activity { pane, .. } => g(pane),
        ServerMsg::PaneOpened { space, tab, pane } => {
            g(space);
            g(tab);
            g(pane);
        }
        ServerMsg::PaneClosed { pane } => g(pane),
        ServerMsg::Focus { space, tab, pane } => {
            g(space);
            g(tab);
            g(pane);
        }
        ServerMsg::Done | ServerMsg::Error { .. } | ServerMsg::ConfigChanged => {}
    }
}

fn strip_node(n: &mut Node) {
    match n {
        Node::Leaf { pane } => *pane = local_of(*pane),
        Node::Split { children, .. } => {
            for c in children {
                strip_node(c);
            }
        }
    }
}

/// Rewrite a request's ids from global to a single daemon's local ids, and
/// return which daemon it targets. All ids in one request share one origin;
/// no-id requests (Snapshot/Reload/Upgrade) and NewSpace default to `LOCAL`.
pub fn route_request(req: &mut Request) -> Origin {
    let strip1 = |id: &mut u64| -> Origin {
        let o = origin_of(*id);
        *id = local_of(*id);
        o
    };
    match req {
        Request::NewTab { space, .. } => strip1(space),
        Request::Split { pane, .. } => strip1(pane),
        Request::SetLayout { tab, layout } => {
            let o = strip1(tab);
            strip_node(layout);
            o
        }
        Request::RenameSpace { space, .. } => strip1(space),
        Request::RenameTab { tab, .. } => strip1(tab),
        Request::Restart { pane } => strip1(pane),
        Request::ClosePane { pane } => strip1(pane),
        Request::CloseTab { tab } => strip1(tab),
        Request::CloseSpace { space } => strip1(space),
        Request::MoveTab { tab, .. } => strip1(tab),
        Request::MoveSpace { space, .. } => strip1(space),
        Request::SetActive { space, tab, pane } => {
            let o = strip1(space);
            *tab = local_of(*tab);
            *pane = local_of(*pane);
            o
        }
        Request::Attach { pane, .. } => strip1(pane),
        Request::Detach { pane } => strip1(pane),
        Request::Input { pane, .. } => strip1(pane),
        Request::Resize { pane, .. } => strip1(pane),
        Request::ReportActivity { pane, .. } => strip1(pane),
        Request::ReportAgent { pane, .. } => strip1(pane),
        Request::Snapshot | Request::NewSpace { .. } | Request::Reload | Request::Upgrade => LOCAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Dir, PaneInfo, PaneStatus, SpaceInfo, TabInfo};

    #[test]
    fn pack_unpack_roundtrip() {
        for (o, l) in [(0u16, 0u64), (0, 42), (1, 1), (7, 999), (65535, LOCAL_MASK)] {
            let id = pack(o, l);
            assert_eq!(origin_of(id), o, "origin for ({o},{l})");
            assert_eq!(local_of(id), l, "local for ({o},{l})");
        }
        // origin 0 is an identity — local traffic is unchanged
        assert_eq!(pack(0, 12345), 12345);
        assert_eq!(origin_of(12345), 0);
    }

    fn pane(id: u64) -> PaneInfo {
        PaneInfo {
            id,
            title: String::new(),
            cmd: Vec::new(),
            cwd: String::new(),
            status: PaneStatus::Running,
            activity: crate::protocol::Activity::Idle,
            created: 0,
            agent: None,
            preview: String::new(),
            activity_since: 0,
            git_branch: String::new(),
        }
    }

    fn fixture() -> Snapshot {
        let layout = Node::Split {
            dir: Dir::Right,
            children: vec![Node::Leaf { pane: 10 }, Node::Leaf { pane: 11 }],
            weights: vec![],
        };
        Snapshot {
            active_space: 1,
            spaces: vec![SpaceInfo {
                id: 1,
                name: "s".into(),
                active_tab: 5,
                tabs: vec![TabInfo {
                    id: 5,
                    name: "t".into(),
                    active_pane: 10,
                    layout,
                }],
            }],
            panes: vec![pane(10), pane(11)],
        }
    }

    #[test]
    fn prefix_snapshot_tags_every_id() {
        let mut snap = fixture();
        prefix_snapshot(&mut snap, 3);
        assert_eq!(origin_of(snap.active_space), 3);
        assert_eq!(local_of(snap.active_space), 1);
        let s = &snap.spaces[0];
        assert_eq!((origin_of(s.id), local_of(s.id)), (3, 1));
        assert_eq!((origin_of(s.active_tab), local_of(s.active_tab)), (3, 5));
        let t = &s.tabs[0];
        assert_eq!((origin_of(t.active_pane), local_of(t.active_pane)), (3, 10));
        let mut leaves = Vec::new();
        t.layout.leaves(&mut leaves);
        assert_eq!(leaves, vec![pack(3, 10), pack(3, 11)]);
        assert_eq!(
            snap.panes.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![pack(3, 10), pack(3, 11)]
        );
    }

    #[test]
    fn prefix_snapshot_local_is_noop() {
        let mut snap = fixture();
        let before = format!("{:?}", snap);
        prefix_snapshot(&mut snap, LOCAL);
        assert_eq!(format!("{:?}", snap), before);
    }

    #[test]
    fn prefix_servermsg_tags_ids() {
        let mut m = ServerMsg::Output {
            pane: 9,
            data: String::new(),
        };
        prefix_servermsg(&mut m, 2);
        assert!(matches!(m, ServerMsg::Output { pane, .. } if pane == pack(2, 9)));

        let mut m = ServerMsg::Created {
            space: 1,
            tab: 2,
            pane: 3,
        };
        prefix_servermsg(&mut m, 4);
        assert!(matches!(m, ServerMsg::Created { space, tab, pane }
            if space == pack(4, 1) && tab == pack(4, 2) && pane == pack(4, 3)));

        // no-id messages are untouched
        let mut m = ServerMsg::ConfigChanged;
        prefix_servermsg(&mut m, 5);
        assert!(matches!(m, ServerMsg::ConfigChanged));
    }

    #[test]
    fn route_request_strips_and_returns_origin() {
        let mut r = Request::Input {
            pane: pack(2, 42),
            data: "x".into(),
        };
        assert_eq!(route_request(&mut r), 2);
        assert!(matches!(r, Request::Input { pane, .. } if pane == 42));

        let mut r = Request::SetActive {
            space: pack(6, 1),
            tab: pack(6, 5),
            pane: pack(6, 10),
        };
        assert_eq!(route_request(&mut r), 6);
        assert!(
            matches!(r, Request::SetActive { space, tab, pane } if space == 1 && tab == 5 && pane == 10)
        );

        // SetLayout strips the layout's leaf ids too
        let layout = Node::Leaf { pane: pack(3, 7) };
        let mut r = Request::SetLayout {
            tab: pack(3, 5),
            layout,
        };
        assert_eq!(route_request(&mut r), 3);
        if let Request::SetLayout { tab, layout } = &r {
            assert_eq!(*tab, 5);
            let mut leaves = Vec::new();
            layout.leaves(&mut leaves);
            assert_eq!(leaves, vec![7]);
        } else {
            panic!();
        }

        // no-id requests → local
        let mut r = Request::Snapshot;
        assert_eq!(route_request(&mut r), LOCAL);
    }

    #[test]
    fn prefix_then_route_round_trips() {
        // A snapshot from origin 5, then a request built from its ids must route
        // back to origin 5 with the daemon's original local ids.
        let mut snap = fixture();
        prefix_snapshot(&mut snap, 5);
        let global_pane = snap.panes[0].id; // pack(5, 10)
        let mut r = Request::Attach {
            pane: global_pane,
            rows: 24,
            cols: 80,
        };
        assert_eq!(route_request(&mut r), 5);
        assert!(matches!(r, Request::Attach { pane, .. } if pane == 10));
    }
}
