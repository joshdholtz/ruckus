//! Split-tree geometry: map a tab's `Node` layout onto terminal rectangles,
//! find gutters for drag-resize, and walk paths into the tree.
//!
//! Kept separate from the TUI so geometry stays unit-testable and free of
//! widget/draw concerns.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::protocol::{Dir, Node};

pub fn split_chunks(dir: Dir, n: usize, weights: &[u16], area: Rect, gutter: u16) -> Vec<Rect> {
    let cons: Vec<Constraint> = if weights.len() == n {
        weights.iter().map(|w| Constraint::Fill(*w)).collect()
    } else {
        (0..n).map(|_| Constraint::Ratio(1, n as u32)).collect()
    };
    let direction = match dir {
        Dir::Right => Direction::Horizontal,
        Dir::Down => Direction::Vertical,
    };
    Layout::default()
        .direction(direction)
        .constraints(cons)
        .spacing(gutter)
        .split(area)
        .to_vec()
}

pub fn node_rects(node: &Node, area: Rect, gutter: u16, out: &mut Vec<(u64, Rect)>) {
    match node {
        Node::Leaf { pane } => out.push((*pane, area)),
        Node::Split {
            dir,
            children,
            weights,
        } => {
            for (c, r) in
                children
                    .iter()
                    .zip(split_chunks(*dir, children.len(), weights, area, gutter))
            {
                node_rects(c, r, gutter, out);
            }
        }
    }
}

/// Collect subtle divider segments in the gutter between sibling panes.
/// Each entry is (line rect, is_vertical).
pub fn node_dividers(node: &Node, area: Rect, gutter: u16, out: &mut Vec<(Rect, bool)>) {
    let Node::Split {
        dir,
        children,
        weights,
    } = node
    else {
        return;
    };
    let chunks = split_chunks(*dir, children.len(), weights, area, gutter);
    if gutter > 0 {
        for pair in chunks.windows(2) {
            let a = pair[0];
            match dir {
                Dir::Right => {
                    let gx = a.x + a.width + gutter / 2;
                    out.push((Rect::new(gx, area.y, 1, area.height), true));
                }
                Dir::Down => {
                    let gy = a.y + a.height + gutter / 2;
                    out.push((Rect::new(area.x, gy, area.width, 1), false));
                }
            }
        }
    }
    for (c, r) in children.iter().zip(chunks) {
        node_dividers(c, r, gutter, out);
    }
}

/// Locate a draggable seam between two sibling panes at (col, row).
pub fn find_border(
    node: &Node,
    area: Rect,
    gutter: u16,
    col: u16,
    row: u16,
    path: &mut Vec<usize>,
) -> Option<(Vec<usize>, usize, Dir)> {
    let Node::Split {
        dir,
        children,
        weights,
    } = node
    else {
        return None;
    };
    let chunks = split_chunks(*dir, children.len(), weights, area, gutter);
    let grab = gutter + 1;
    for i in 0..children.len().saturating_sub(1) {
        match dir {
            Dir::Right => {
                let b = chunks[i + 1].x;
                if row >= area.y
                    && row < area.y + area.height
                    && (b.saturating_sub(grab)..=b).contains(&col)
                {
                    return Some((path.clone(), i, Dir::Right));
                }
            }
            Dir::Down => {
                let b = chunks[i + 1].y;
                if col >= area.x
                    && col < area.x + area.width
                    && (b.saturating_sub(grab)..=b).contains(&row)
                {
                    return Some((path.clone(), i, Dir::Down));
                }
            }
        }
    }
    for (i, (c, r)) in children.iter().zip(chunks.iter()).enumerate() {
        if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
            path.push(i);
            if let Some(hit) = find_border(c, *r, gutter, col, row, path) {
                return Some(hit);
            }
            path.pop();
        }
    }
    None
}

pub fn node_at_path_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    if path.is_empty() {
        return Some(node);
    }
    match node {
        Node::Split { children, .. } => children
            .get_mut(path[0])
            .and_then(|c| node_at_path_mut(c, &path[1..])),
        _ => None,
    }
}

pub fn area_at_path(node: &Node, area: Rect, gutter: u16, path: &[usize]) -> Option<Rect> {
    if path.is_empty() {
        return Some(area);
    }
    let Node::Split {
        dir,
        children,
        weights,
    } = node
    else {
        return None;
    };
    let chunks = split_chunks(*dir, children.len(), weights, area, gutter);
    let i = path[0];
    children
        .get(i)
        .and_then(|c| area_at_path(c, *chunks.get(i)?, gutter, &path[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_fills_area() {
        let n = Node::Leaf { pane: 7 };
        let mut out = Vec::new();
        node_rects(&n, Rect::new(0, 0, 80, 24), 1, &mut out);
        assert_eq!(out, vec![(7, Rect::new(0, 0, 80, 24))]);
    }

    #[test]
    fn split_two_right_yields_two_rects() {
        let n = Node::Split {
            dir: Dir::Right,
            children: vec![Node::Leaf { pane: 1 }, Node::Leaf { pane: 2 }],
            weights: vec![1, 1],
        };
        let mut out = Vec::new();
        node_rects(&n, Rect::new(0, 0, 80, 24), 1, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[1].0, 2);
        // Gutters consume space; each pane should still be non-empty.
        assert!(out[0].1.width > 0 && out[1].1.width > 0);
        assert_eq!(out[0].1.height, 24);
    }
}
