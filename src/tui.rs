use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::unbounded_channel;

use crate::client::{connect, ensure_daemon, resolve_pane, Client};
use crate::config::{normalize_key, Action, BarPos, Config, SidebarPos, ToastPos, WorkingStyle};
use crate::protocol::*;
use crate::render::{encode_key, screen_to_lines};

/// Below this width the footer switches to compact tap-first chips.
const FOOTER_COMPACT: u16 = 70;

struct PaneView {
    parser: vt100::Parser,
    scroll: usize,
    rows: u16,
    cols: u16,
}

#[derive(Clone, Copy)]
enum Target {
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
    Pane(u64),
}

/// A decoded image to display in a pane, with a cached terminal-encoded protocol.
struct ImageState {
    img: image::DynamicImage,
    proto: Option<Protocol>,
    area: Option<Rect>,
}

/// A clickable affordance rendered in the sidebar.
#[derive(Clone, Copy)]
enum SidebarBtn {
    NewSpace,
    CloseSpace(u64),
}

/// What a context menu acts on.
#[derive(Clone, Copy)]
enum MenuTarget {
    Pane(u64),
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
}

#[derive(Clone)]
struct Menu {
    x: u16,
    y: u16,
    target: MenuTarget,
    items: Vec<(&'static str, MenuAction)>,
}

#[derive(Clone, Copy, PartialEq)]
enum MenuAction {
    SplitRight,
    SplitDown,
    NewTab,
    NewSpace,
    RenameTab,
    RenameSpace,
    CloseTab,
    CloseSpace,
    MoveTabLeft,
    MoveTabRight,
    MoveSpaceUp,
    MoveSpaceDown,
    Zoom,
    Restart,
    ClosePane,
}

struct Drag {
    tab: u64,
    path: Vec<usize>,
    index: usize,
    dir: Dir,
}

/// A text selection within one pane, in that pane's content-cell coordinates
/// (row, col). `anchor` is where the drag began, `head` where it is now.
#[derive(Clone, Copy)]
struct Sel {
    pane: u64,
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Sel {
    /// (start, end) ordered top-to-bottom, left-to-right.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PromptKind {
    NewTab,
    NewSpace,
    RenameSpace(u64),
    RenameTab(u64),
}

/// A modal single-line text input (naming a new/renamed space or tab).
struct Prompt {
    kind: PromptKind,
    label: &'static str,
    buffer: String,
}

/// Where every chrome element lives this frame, derived from config + size.
#[derive(Clone, Copy, Default)]
struct FrameLayout {
    header: Option<u16>,
    footer: Option<u16>,
    tabs: Option<u16>,
    /// Full-width region between the horizontal bars.
    body: Rect,
    sidebar: Option<Rect>,
    main: Rect,
    /// Main minus the tab strip: where panes render.
    panes: Rect,
}

struct App {
    cfg: Config,
    client: Client,
    snap: Snapshot,
    views: HashMap<u64, PaneView>,
    focused: u64,
    seen: HashSet<u64>,
    /// Panes that changed to a notable state (finished / needs input) while
    /// unfocused. Cleared when you view the pane. Drives the "unread" badge.
    unread: HashSet<u64>,
    cwd: String,
    running: bool,
    toast: Option<(String, Instant)>,
    sidebar: bool,
    zoomed: bool,
    drawer: bool,
    help: bool,
    menu: Option<Menu>,
    prompt: Option<Prompt>,
    drag: Option<Drag>,
    select: Option<Sel>,
    selecting: bool,
    hover: Option<(u16, u16)>,
    tick: usize,
    size: (u16, u16),
    frame: FrameLayout,
    sidebar_rows: Vec<(u16, Target)>,
    sidebar_buttons: Vec<(u16, std::ops::Range<u16>, SidebarBtn)>,
    tab_hits: Vec<(Option<u64>, std::ops::Range<u16>)>,
    tab_close_hits: Vec<(u64, std::ops::Range<u16>)>,
    /// Reorder-drag state: a tab or space being dragged in the strip/sidebar.
    tab_drag: Option<u64>,
    space_drag: Option<u64>,
    /// Alt+drag a pane onto another to swap them.
    swap_from: Option<u64>,
    footer_hits: Vec<(Action, std::ops::Range<u16>)>,
    pane_rects: Vec<(u64, Rect)>,
    picker: Picker,
    images: HashMap<u64, ImageState>,
}

/// Collapse the user's home prefix to `~` for compact display.
fn home_relative(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if path == home {
                return "~".to_string();
            }
            if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
                return format!("~/{rest}");
            }
        }
    }
    path.to_string()
}

/// Put text on the clipboard two ways for wide coverage: OSC 52 (works over SSH
/// and in iTerm2 / WezTerm / kitty / …) and a local `pbcopy` fallback.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", B64.encode(text.as_bytes()));
    let _ = out.flush();
    if let Ok(mut child) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(si) = child.stdin.as_mut() {
            let _ = si.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn agg_activity<I: Iterator<Item = Activity>>(iter: I) -> Activity {
    iter.max_by_key(|a| a.urgency()).unwrap_or(Activity::Idle)
}

fn tab_activity(snap: &Snapshot, tab: &TabInfo) -> Activity {
    let mut leaves = Vec::new();
    tab.layout.leaves(&mut leaves);
    agg_activity(leaves.iter().filter_map(|p| snap.pane(*p)).map(|p| p.activity))
}

fn space_activity(snap: &Snapshot, space: &SpaceInfo) -> Activity {
    agg_activity(space.tabs.iter().map(|t| tab_activity(snap, t)))
}

fn split_chunks(dir: Dir, n: usize, weights: &[u16], area: Rect, gutter: u16) -> Vec<Rect> {
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

fn node_rects(node: &Node, area: Rect, gutter: u16, out: &mut Vec<(u64, Rect)>) {
    match node {
        Node::Leaf { pane } => out.push((*pane, area)),
        Node::Split { dir, children, weights } => {
            for (c, r) in children
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
fn node_dividers(node: &Node, area: Rect, gutter: u16, out: &mut Vec<(Rect, bool)>) {
    let Node::Split { dir, children, weights } = node else { return };
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
fn find_border(
    node: &Node,
    area: Rect,
    gutter: u16,
    col: u16,
    row: u16,
    path: &mut Vec<usize>,
) -> Option<(Vec<usize>, usize, Dir)> {
    let Node::Split { dir, children, weights } = node else { return None };
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

fn node_at_path_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
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

fn area_at_path(node: &Node, area: Rect, gutter: u16, path: &[usize]) -> Option<Rect> {
    if path.is_empty() {
        return Some(area);
    }
    let Node::Split { dir, children, weights } = node else { return None };
    let chunks = split_chunks(*dir, children.len(), weights, area, gutter);
    let i = path[0];
    children
        .get(i)
        .and_then(|c| area_at_path(c, *chunks.get(i)?, gutter, &path[1..]))
}

/// Expand a row template like "{icon} {title}" into styled spans.
fn template_spans(
    tpl: &str,
    icon: &(String, Color),
    vars: &[(&str, String)],
    row_style: Style,
    text_fg: Color,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = tpl;
    while let Some(start) = rest.find('{') {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), row_style.fg(text_fg)));
        }
        match rest[start..].find('}') {
            Some(endrel) => {
                let token = &rest[start + 1..start + endrel];
                if token == "icon" {
                    spans.push(Span::styled(icon.0.clone(), row_style.fg(icon.1)));
                } else if let Some((_, v)) = vars.iter().find(|(k, _)| *k == token) {
                    spans.push(Span::styled(v.clone(), row_style.fg(text_fg)));
                } else {
                    spans.push(Span::styled(
                        format!("{{{token}}}"),
                        row_style.fg(text_fg),
                    ));
                }
                rest = &rest[start + endrel + 1..];
            }
            None => {
                spans.push(Span::styled(rest[start..].to_string(), row_style.fg(text_fg)));
                rest = "";
            }
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), row_style.fg(text_fg)));
    }
    spans
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

impl App {
    fn active_space(&self) -> Option<SpaceInfo> {
        self.snap
            .spaces
            .iter()
            .find(|s| s.id == self.snap.active_space)
            .or(self.snap.spaces.first())
            .cloned()
    }

    fn active_tab(&self) -> Option<TabInfo> {
        let s = self.active_space()?;
        s.tabs
            .iter()
            .find(|t| t.id == s.active_tab)
            .or(s.tabs.first())
            .cloned()
    }

    fn visible(&self) -> Vec<u64> {
        let mut v = Vec::new();
        if let Some(t) = self.active_tab() {
            t.layout.leaves(&mut v);
        }
        v
    }

    fn locate(&self, pane: u64) -> Option<(u64, u64)> {
        for s in &self.snap.spaces {
            for t in &s.tabs {
                if t.layout.contains(pane) {
                    return Some((s.id, t.id));
                }
            }
        }
        None
    }

    fn narrow(&self) -> bool {
        self.cfg.ui.narrow_below > 0 && self.size.0 < self.cfg.ui.narrow_below
    }

    fn sidebar_shown(&self) -> bool {
        self.sidebar && !self.narrow()
    }

    fn compute_frame(&self) -> FrameLayout {
        let (w, h) = self.size;
        let ui = &self.cfg.ui;
        let mut top: u16 = ui.top_margin.min(h.saturating_sub(2));
        let mut bot: u16 = h;
        let mut header = None;
        let mut footer = None;
        if ui.header == BarPos::Top {
            header = Some(top);
            top += 1;
        }
        if ui.footer == BarPos::Top {
            footer = Some(top);
            top += 1;
        }
        if ui.footer == BarPos::Bottom && bot > top {
            bot -= 1;
            footer = Some(bot);
        }
        if ui.header == BarPos::Bottom && bot > top {
            bot -= 1;
            header = Some(bot);
        }
        let body = Rect::new(0, top, w, bot.saturating_sub(top));
        let shown = self.sidebar_shown();
        let sw = if shown { ui.sidebar_width.min(w.saturating_sub(20)) } else { 0 };
        let (sidebar, main_x, main_w) = if shown {
            match ui.sidebar_pos {
                SidebarPos::Left => {
                    (Some(Rect::new(0, body.y, sw, body.height)), sw, w.saturating_sub(sw))
                }
                SidebarPos::Right => (
                    Some(Rect::new(w.saturating_sub(sw), body.y, sw, body.height)),
                    0,
                    w.saturating_sub(sw),
                ),
            }
        } else {
            (None, 0, w)
        };
        let main = Rect::new(main_x, body.y, main_w, body.height);
        let (tabs, panes) = if ui.tab_strip && main.height > 1 {
            (
                Some(main.y),
                Rect::new(main.x, main.y + 1, main.width, main.height - 1),
            )
        } else {
            (None, main)
        };
        FrameLayout { header, footer, tabs, body, sidebar, main, panes }
    }

    fn compute_rects(&self) -> Vec<(u64, Rect)> {
        let mut out = Vec::new();
        if let Some(t) = self.active_tab() {
            if self.zoomed && t.layout.contains(self.focused) {
                out.push((self.focused, self.frame.panes));
            } else {
                node_rects(&t.layout, self.frame.panes, self.cfg.ui.gutter, &mut out);
            }
        }
        out
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// The content sub-rect of a pane rect (inside title bar + padding).
    fn pane_content_rect(&self, rect: Rect) -> Rect {
        let title = self.cfg.ui.pane_titles as u16;
        let pad = self.cfg.ui.pane_padding;
        Rect::new(
            rect.x + pad,
            rect.y + title + pad,
            rect.width.saturating_sub(2 * pad),
            rect.height.saturating_sub(title + 2 * pad),
        )
    }

    /// Absolute (col,row) -> (pane id, content cell (row,col)) if it lands in a pane.
    fn cell_at(&self, col: u16, row: u16) -> Option<(u64, (u16, u16))> {
        for (pid, rect) in &self.pane_rects {
            let c = self.pane_content_rect(*rect);
            if c.width == 0 || c.height == 0 {
                continue;
            }
            if col >= c.x && col < c.x + c.width && row >= c.y && row < c.y + c.height {
                return Some((*pid, (row - c.y, col - c.x)));
            }
        }
        None
    }

    /// Absolute (col,row) clamped into `pane`'s content -> cell (row,col).
    fn cell_in_pane(&self, pane: u64, col: u16, row: u16) -> Option<(u16, u16)> {
        let rect = self.pane_rects.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)?;
        let c = self.pane_content_rect(rect);
        if c.width == 0 || c.height == 0 {
            return None;
        }
        let cc = col.clamp(c.x, c.x + c.width - 1) - c.x;
        let cr = row.clamp(c.y, c.y + c.height - 1) - c.y;
        Some((cr, cc))
    }

    /// Extract the current selection's text and put it on the clipboard.
    fn copy_selection(&mut self) {
        let Some(sel) = self.select else { return };
        if sel.is_empty() {
            return;
        }
        let ((sr, sc), (er, ec)) = sel.ordered();
        let Some(view) = self.views.get(&sel.pane) else { return };
        let screen = view.parser.screen();
        let (_, cols) = screen.size();
        let ec2 = ec.saturating_add(1).min(cols); // include the cell under the cursor
        let text = screen.contents_between(sr, sc, er, ec2);
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        let n = text.chars().count();
        copy_to_clipboard(&text);
        self.toast(format!("copied {n} chars"));
    }

    fn pane_size(&self, rect: Rect) -> (u16, u16) {
        let title: u16 = self.cfg.ui.pane_titles as u16;
        let pad = self.cfg.ui.pane_padding;
        let rows = rect.height.saturating_sub(title + 2 * pad).max(1);
        let cols = rect.width.saturating_sub(2 * pad).max(1);
        (rows, cols)
    }

    async fn sync(&mut self) {
        self.size = crossterm::terminal::size().unwrap_or((80, 24));
        self.frame = self.compute_frame();
        let rects = self.compute_rects();
        self.pane_rects = rects.clone();
        let visible: Vec<u64> = rects.iter().map(|(p, _)| *p).collect();

        let stale: Vec<u64> = self
            .views
            .keys()
            .filter(|p| !visible.contains(p))
            .copied()
            .collect();
        for p in stale {
            self.views.remove(&p);
            let _ = self.client.request(Request::Detach { pane: p }).await;
        }

        for (pane, rect) in rects {
            let (rows, cols) = self.pane_size(rect);
            if !self.views.contains_key(&pane) {
                match self.client.request(Request::Attach { pane, rows, cols }).await {
                    Ok(ServerMsg::Attached { scrollback, .. }) => {
                        let mut parser = vt100::Parser::new(rows, cols, 10_000);
                        if let Ok(bytes) = B64.decode(scrollback.as_bytes()) {
                            parser.process(&bytes);
                        }
                        self.views.insert(pane, PaneView { parser, scroll: 0, rows, cols });
                    }
                    Err(e) => self.toast(e.to_string()),
                    _ => {}
                }
            } else if let Some(v) = self.views.get_mut(&pane) {
                if v.rows != rows || v.cols != cols {
                    v.rows = rows;
                    v.cols = cols;
                    v.parser.set_size(rows, cols);
                    let _ = self.client.request(Request::Resize { pane, rows, cols }).await;
                }
            }
        }

        if !visible.contains(&self.focused) {
            let next = self
                .active_tab()
                .map(|t| t.active_pane)
                .filter(|p| visible.contains(p))
                .or_else(|| visible.first().copied());
            if let Some(p) = next {
                self.focused = p;
            }
        }
    }

    async fn set_active(&mut self, space: u64, tab: u64, pane: u64) {
        self.snap.active_space = space;
        if let Some(s) = self.snap.spaces.iter_mut().find(|s| s.id == space) {
            s.active_tab = tab;
            if let Some(t) = s.tabs.iter_mut().find(|t| t.id == tab) {
                t.active_pane = pane;
            }
        }
        self.focused = pane;
        self.mark_seen();
        let _ = self.client.request(Request::SetActive { space, tab, pane }).await;
        self.sync().await;
    }

    async fn goto_pane(&mut self, pane: u64) {
        if let Some((s, t)) = self.locate(pane) {
            self.set_active(s, t, pane).await;
        }
    }

    async fn handle_sidebar_target(&mut self, target: Target) {
        match target {
            Target::Space(id) => {
                let t = self.snap.spaces.iter().find(|s| s.id == id).and_then(|s| {
                    s.tabs
                        .iter()
                        .find(|t| t.id == s.active_tab)
                        .or(s.tabs.first())
                        .map(|t| (s.id, t.id, t.active_pane))
                });
                if let Some((s, t, p)) = t {
                    self.set_active(s, t, p).await;
                }
            }
            Target::Tab { space, tab, pane } => self.set_active(space, tab, pane).await,
            Target::Pane(p) => self.goto_pane(p).await,
        }
    }

    fn drawer_width(&self) -> u16 {
        self.cfg.ui.sidebar_width.min(self.size.0.saturating_sub(4))
    }

    fn drawer_rect(&self) -> Rect {
        let dw = self.drawer_width();
        let x = match self.cfg.ui.sidebar_pos {
            SidebarPos::Left => 0,
            SidebarPos::Right => self.size.0.saturating_sub(dw),
        };
        Rect::new(x, self.frame.body.y, dw, self.frame.body.height)
    }

    fn scroll_by(&mut self, pane: u64, delta: isize) {
        if let Some(v) = self.views.get_mut(&pane) {
            v.scroll = (v.scroll as isize + delta).clamp(0, 10_000) as usize;
            v.parser.set_scrollback(v.scroll);
        }
    }

    fn attention(&self) -> Vec<&PaneInfo> {
        let mut list: Vec<&PaneInfo> = self
            .snap
            .panes
            .iter()
            .filter(|p| match p.activity {
                Activity::Waiting => true,
                Activity::Done => !self.seen.contains(&p.id),
                _ => self.unread.contains(&p.id),
            })
            .collect();
        list.sort_by_key(|p| std::cmp::Reverse(p.activity.urgency()));
        list
    }

    /// Every pane running a non-shell command (an "agent"), across all spaces,
    /// most-attention-first: (pane id, tab name, space name).
    fn agent_rows(&self) -> Vec<(u64, String, String)> {
        const SHELLS: &[&str] =
            &["zsh", "bash", "sh", "fish", "dash", "tcsh", "ksh", "nu", "pwsh"];
        let mut out = Vec::new();
        for s in &self.snap.spaces {
            for t in &s.tabs {
                let mut leaves = Vec::new();
                t.layout.leaves(&mut leaves);
                for pid in leaves {
                    if let Some(p) = self.snap.pane(pid) {
                        let base = p
                            .cmd
                            .first()
                            .and_then(|c| c.rsplit('/').next())
                            .unwrap_or("");
                        // An agent = a detector reported one, or it was spawned
                        // as a known agent command (allowlist; empty = any non-shell).
                        let allow = &self.cfg.ui.agent_commands;
                        let spawn_agent = !base.is_empty()
                            && !SHELLS.contains(&base)
                            && (allow.is_empty()
                                || allow.iter().any(|a| a.eq_ignore_ascii_case(base)));
                        let is_agent = p.agent.is_some() || spawn_agent;
                        if is_agent {
                            let label = p.agent.clone().unwrap_or_else(|| t.name.clone());
                            out.push((pid, label, s.name.clone()));
                        }
                    }
                }
            }
        }
        out.sort_by_key(|(pid, _, _)| {
            let a = self.snap.pane(*pid).map(|p| p.activity).unwrap_or(Activity::Idle);
            std::cmp::Reverse(a.urgency())
        });
        out
    }

    fn mark_seen(&mut self) {
        // Viewing a pane clears its unread badge.
        self.unread.remove(&self.focused);
        if let Some(p) = self.snap.pane(self.focused) {
            if p.activity == Activity::Done {
                self.seen.insert(p.id);
            }
        }
    }

    fn tab_unread(&self, t: &TabInfo) -> bool {
        let mut leaves = Vec::new();
        t.layout.leaves(&mut leaves);
        leaves.iter().any(|p| self.unread.contains(p))
    }

    fn space_unread(&self, s: &SpaceInfo) -> bool {
        s.tabs.iter().any(|t| self.tab_unread(t))
    }

    async fn split_action(&mut self, pane: u64, dir: Dir) {
        let req = Request::Split { pane, dir, cmd: Vec::new(), cwd: Some(self.cwd.clone()) };
        match self.client.request(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    async fn new_tab_action(&mut self, name: Option<String>) {
        let Some(space) = self.active_space().map(|s| s.id) else { return };
        let req =
            Request::NewTab { space, name, cmd: Vec::new(), cwd: Some(self.cwd.clone()) };
        match self.client.request(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    async fn new_space_action(&mut self, name: Option<String>) {
        let req = Request::NewSpace { name, cwd: Some(self.cwd.clone()) };
        match self.client.request(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    /// Open the modal text input for `kind`.
    fn open_prompt(&mut self, kind: PromptKind) {
        let (label, buffer) = match kind {
            PromptKind::NewTab => ("name new tab (blank = default)", String::new()),
            PromptKind::NewSpace => ("name new space (blank = default)", String::new()),
            PromptKind::RenameSpace(id) => (
                "rename space",
                self.snap.spaces.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default(),
            ),
            PromptKind::RenameTab(id) => (
                "rename tab",
                self.snap.spaces.iter().flat_map(|s| &s.tabs).find(|t| t.id == id).map(|t| t.name.clone()).unwrap_or_default(),
            ),
        };
        self.prompt = Some(Prompt { kind, label, buffer });
    }

    /// Commit the active prompt: create or rename with the typed name.
    async fn submit_prompt(&mut self) {
        let Some(p) = self.prompt.take() else { return };
        let name = p.buffer.trim().to_string();
        let opt = if name.is_empty() { None } else { Some(name.clone()) };
        match p.kind {
            PromptKind::NewTab => self.new_tab_action(opt).await,
            PromptKind::NewSpace => self.new_space_action(opt).await,
            PromptKind::RenameSpace(space) => {
                if let Some(name) = opt {
                    if let Err(e) = self.client.request(Request::RenameSpace { space, name }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            PromptKind::RenameTab(tab) => {
                if let Some(name) = opt {
                    if let Err(e) = self.client.request(Request::RenameTab { tab, name }).await {
                        self.toast(e.to_string());
                    }
                }
            }
        }
    }

    async fn close_pane_action(&mut self, pane: u64) {
        if let Err(e) = self.client.request(Request::ClosePane { pane }).await {
            self.toast(e.to_string());
        }
    }

    async fn do_action(&mut self, a: Action) {
        match a {
            Action::Quit => self.running = false,
            Action::ShowHelp => self.help = !self.help,
            Action::ToggleSidebar => {
                if self.narrow() {
                    self.drawer = !self.drawer;
                } else {
                    self.sidebar = !self.sidebar;
                    self.sync().await;
                }
            }
            Action::JumpWaiting => {
                let next = self.attention().first().map(|p| p.id);
                match next {
                    Some(p) => self.goto_pane(p).await,
                    None => self.toast("nothing needs you"),
                }
            }
            Action::SplitRight => self.split_action(self.focused, Dir::Right).await,
            Action::SplitDown => self.split_action(self.focused, Dir::Down).await,
            Action::ClosePane => self.close_pane_action(self.focused).await,
            Action::NextPane | Action::PrevPane => {
                let vis = self.visible();
                if vis.is_empty() {
                    return;
                }
                let idx = vis.iter().position(|p| *p == self.focused).unwrap_or(0);
                let next = if a == Action::NextPane {
                    vis[(idx + 1) % vis.len()]
                } else {
                    vis[(idx + vis.len() - 1) % vis.len()]
                };
                self.goto_pane(next).await;
            }
            Action::NewTab => self.open_prompt(PromptKind::NewTab),
            Action::NewSpace => self.open_prompt(PromptKind::NewSpace),
            Action::NextTab | Action::PrevTab => {
                let Some(s) = self.active_space() else { return };
                if s.tabs.is_empty() {
                    return;
                }
                let idx = s.tabs.iter().position(|t| t.id == s.active_tab).unwrap_or(0);
                let next = if a == Action::NextTab {
                    &s.tabs[(idx + 1) % s.tabs.len()]
                } else {
                    &s.tabs[(idx + s.tabs.len() - 1) % s.tabs.len()]
                };
                self.set_active(s.id, next.id, next.active_pane).await;
            }
            Action::NextSpace | Action::PrevSpace => {
                if self.snap.spaces.is_empty() {
                    return;
                }
                let idx = self
                    .snap
                    .spaces
                    .iter()
                    .position(|s| s.id == self.snap.active_space)
                    .unwrap_or(0);
                let n = self.snap.spaces.len();
                let next = if a == Action::NextSpace {
                    self.snap.spaces[(idx + 1) % n].clone()
                } else {
                    self.snap.spaces[(idx + n - 1) % n].clone()
                };
                let tab = next
                    .tabs
                    .iter()
                    .find(|t| t.id == next.active_tab)
                    .or(next.tabs.first());
                if let Some(t) = tab {
                    self.set_active(next.id, t.id, t.active_pane).await;
                }
            }
            Action::ScrollUp => self.scroll_by(self.focused, 5),
            Action::ScrollDown => self.scroll_by(self.focused, -5),
            Action::Zoom => {
                self.zoomed = !self.zoomed;
                self.sync().await;
            }
        }
    }

    async fn restart_action(&mut self, pane: u64) {
        match self.client.request(Request::Restart { pane }).await {
            Ok(_) => {
                self.seen.remove(&pane);
                self.views.remove(&pane); // fresh attach picks up seeded scrollback
                self.sync().await;
            }
            Err(e) => self.toast(e.to_string()),
        }
    }

    async fn run_menu_item(&mut self, action: MenuAction, target: MenuTarget) {
        // Resolve the (space, tab, pane) this menu acts on.
        let (space, tab, pane) = match target {
            MenuTarget::Pane(p) => match self.locate(p) {
                Some((s, t)) => (Some(s), Some(t), Some(p)),
                None => (None, None, Some(p)),
            },
            MenuTarget::Space(s) => (Some(s), None, None),
            MenuTarget::Tab { space, tab, pane } => (Some(space), Some(tab), Some(pane)),
        };
        match action {
            MenuAction::SplitRight => {
                if let Some(p) = pane {
                    self.split_action(p, Dir::Right).await;
                }
            }
            MenuAction::SplitDown => {
                if let Some(p) = pane {
                    self.split_action(p, Dir::Down).await;
                }
            }
            MenuAction::NewTab => self.open_prompt(PromptKind::NewTab),
            MenuAction::NewSpace => self.open_prompt(PromptKind::NewSpace),
            MenuAction::RenameTab => {
                if let Some(t) = tab {
                    self.open_prompt(PromptKind::RenameTab(t));
                }
            }
            MenuAction::RenameSpace => {
                if let Some(s) = space {
                    self.open_prompt(PromptKind::RenameSpace(s));
                }
            }
            MenuAction::CloseTab => {
                if let Some(t) = tab {
                    if let Err(e) = self.client.request(Request::CloseTab { tab: t }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            MenuAction::CloseSpace => {
                if let Some(s) = space {
                    if let Err(e) = self.client.request(Request::CloseSpace { space: s }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            MenuAction::MoveTabLeft | MenuAction::MoveTabRight => {
                if let (Some(sp), Some(t)) = (space, tab) {
                    let idx = self
                        .snap
                        .spaces
                        .iter()
                        .find(|s| s.id == sp)
                        .and_then(|s| s.tabs.iter().position(|x| x.id == t));
                    if let Some(i) = idx {
                        let to = if action == MenuAction::MoveTabLeft {
                            i.saturating_sub(1)
                        } else {
                            i + 1
                        };
                        if let Err(e) = self.client.request(Request::MoveTab { tab: t, to }).await {
                            self.toast(e.to_string());
                        }
                    }
                }
            }
            MenuAction::MoveSpaceUp | MenuAction::MoveSpaceDown => {
                if let Some(sp) = space {
                    if let Some(i) = self.snap.spaces.iter().position(|s| s.id == sp) {
                        let to = if action == MenuAction::MoveSpaceUp {
                            i.saturating_sub(1)
                        } else {
                            i + 1
                        };
                        if let Err(e) =
                            self.client.request(Request::MoveSpace { space: sp, to }).await
                        {
                            self.toast(e.to_string());
                        }
                    }
                }
            }
            MenuAction::Zoom => {
                self.zoomed = !self.zoomed;
                self.sync().await;
            }
            MenuAction::Restart => {
                if let Some(p) = pane {
                    self.restart_action(p).await;
                }
            }
            MenuAction::ClosePane => {
                if let Some(p) = pane {
                    self.close_pane_action(p).await;
                }
            }
        }
    }

    async fn on_key(&mut self, ev: KeyEvent) {
        if ev.kind == KeyEventKind::Release {
            return;
        }
        let ev = normalize_key(&ev, self.cfg.ui.mac_option_fallback);

        // Modal text input swallows every key until Enter (submit) or Esc (cancel).
        if self.prompt.is_some() {
            match ev.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Enter => self.submit_prompt().await,
                KeyCode::Backspace => {
                    if let Some(p) = self.prompt.as_mut() {
                        p.buffer.pop();
                    }
                }
                KeyCode::Char(c) if !ev.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(p) = self.prompt.as_mut() {
                        p.buffer.push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        if self.menu.is_some() {
            self.menu = None;
            if ev.code == KeyCode::Esc {
                return;
            }
        }
        if self.help {
            self.help = false;
            return;
        }

        if ev.modifiers.contains(KeyModifiers::ALT) {
            if let KeyCode::Char(c @ '1'..='9') = ev.code {
                let idx = c as usize - '1' as usize;
                let target = self
                    .active_space()
                    .and_then(|s| s.tabs.get(idx).map(|t| (s.id, t.id, t.active_pane)));
                if let Some((s, t, p)) = target {
                    self.set_active(s, t, p).await;
                }
                return;
            }
        }
        if let Some(a) = self.cfg.action_for(&ev) {
            self.do_action(a).await;
            return;
        }
        // Enter on a dead pane restarts it in place (zellij-style).
        if ev.code == KeyCode::Enter
            && self
                .snap
                .pane(self.focused)
                .map(|p| p.status != PaneStatus::Running)
                .unwrap_or(false)
        {
            self.restart_action(self.focused).await;
            return;
        }
        if let Some(bytes) = encode_key(&ev) {
            if let Some(v) = self.views.get_mut(&self.focused) {
                if v.scroll != 0 {
                    v.scroll = 0;
                    v.parser.set_scrollback(0);
                }
            }
            let req = Request::Input { pane: self.focused, data: B64.encode(&bytes) };
            if let Err(e) = self.client.request(req).await {
                self.toast(e.to_string());
            }
        }
    }

    fn pane_at(&self, col: u16, row: u16) -> Option<u64> {
        self.pane_rects
            .iter()
            .find(|(_, r)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
            .map(|(p, _)| *p)
    }

    fn menu_rect(&self, m: &Menu) -> Rect {
        let w = (m.items.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(10) + 4) as u16;
        let h = m.items.len() as u16 + 2;
        let x = m.x.min(self.size.0.saturating_sub(w + 1));
        let y = m.y.min(self.size.1.saturating_sub(h + 1));
        Rect::new(x, y, w, h)
    }

    fn border_hit(&self, col: u16, row: u16) -> Option<(Vec<usize>, usize, Dir)> {
        let t = self.active_tab()?;
        let mut path = Vec::new();
        find_border(&t.layout, self.frame.panes, self.cfg.ui.gutter, col, row, &mut path)
    }

    fn apply_drag(&mut self, col: u16, row: u16) {
        let Some(drag) = &self.drag else { return };
        let (tab_id, path, index, dir) = (drag.tab, drag.path.clone(), drag.index, drag.dir);
        let panes_area = self.frame.panes;
        let gutter = self.cfg.ui.gutter;
        let Some(space) = self
            .snap
            .spaces
            .iter_mut()
            .find(|s| s.tabs.iter().any(|t| t.id == tab_id))
        else {
            return;
        };
        let Some(tab) = space.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        let Some(split_area) = area_at_path(&tab.layout, panes_area, gutter, &path) else {
            return;
        };
        let Some(Node::Split { dir: d, children, weights }) =
            node_at_path_mut(&mut tab.layout, &path)
        else {
            return;
        };
        if *d != dir || index + 1 >= children.len() {
            return;
        }
        let chunks = split_chunks(dir, children.len(), weights, split_area, gutter);
        let mut sizes: Vec<u16> = chunks
            .iter()
            .map(|r| match dir {
                Dir::Right => r.width,
                Dir::Down => r.height,
            })
            .collect();
        let start: u16 = match dir {
            Dir::Right => chunks[index].x,
            Dir::Down => chunks[index].y,
        };
        let pos = match dir {
            Dir::Right => col,
            Dir::Down => row,
        };
        let pair = sizes[index] + sizes[index + 1];
        let first = pos
            .saturating_sub(start)
            .clamp(3, pair.saturating_sub(3).max(3));
        sizes[index] = first;
        sizes[index + 1] = pair.saturating_sub(first);
        *weights = sizes.iter().map(|s| (*s).max(1)).collect();
        self.pane_rects = self.compute_rects();
    }

    async fn finish_drag(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        let layout = self
            .snap
            .spaces
            .iter()
            .flat_map(|s| s.tabs.iter())
            .find(|t| t.id == drag.tab)
            .map(|t| t.layout.clone());
        if let Some(layout) = layout {
            if let Err(e) = self.client.request(Request::SetLayout { tab: drag.tab, layout }).await
            {
                self.toast(e.to_string());
            }
        }
        self.sync().await;
    }

    async fn on_mouse(&mut self, ev: MouseEvent) {
        let (col, row) = (ev.column, ev.row);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.kind {
            MouseEventKind::Moved => {
                self.hover = Some((col, row));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.hover = Some((col, row));
                if self.drag.is_some() {
                    self.apply_drag(col, row);
                } else if let Some(tab) = self.tab_drag {
                    // Reorder: move the dragged tab into the slot under the cursor.
                    if Some(row) == self.frame.tabs {
                        let over = self
                            .tab_hits
                            .iter()
                            .find(|(_, r)| r.contains(&col))
                            .and_then(|(id, _)| *id);
                        if let Some(target) = over {
                            if target != tab {
                                if let Some(to) = self
                                    .active_space()
                                    .and_then(|s| s.tabs.iter().position(|t| t.id == target))
                                {
                                    let _ = self.client.request(Request::MoveTab { tab, to }).await;
                                }
                            }
                        }
                    }
                } else if let Some(space) = self.space_drag {
                    // Reorder spaces: move into the space row under the cursor.
                    let over = self
                        .sidebar_rows
                        .iter()
                        .find(|(r, _)| *r == row)
                        .and_then(|(_, t)| match t {
                            Target::Space(id) => Some(*id),
                            _ => None,
                        });
                    if let Some(target) = over {
                        if target != space {
                            if let Some(to) =
                                self.snap.spaces.iter().position(|s| s.id == target)
                            {
                                let _ =
                                    self.client.request(Request::MoveSpace { space, to }).await;
                            }
                        }
                    }
                } else if self.selecting {
                    if let Some(pane) = self.select.map(|s| s.pane) {
                        if let Some(cell) = self.cell_in_pane(pane, col, row) {
                            if let Some(sel) = self.select.as_mut() {
                                sel.head = cell;
                            }
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.tab_drag = None;
                self.space_drag = None;
                if let Some(from) = self.swap_from.take() {
                    if let Some(to) = self.pane_at(col, row) {
                        if to != from {
                            if let Some(t) = self.active_tab() {
                                let mut layout = t.layout.clone();
                                layout.swap_leaves(from, to);
                                let tab = t.id;
                                if let Err(e) =
                                    self.client.request(Request::SetLayout { tab, layout }).await
                                {
                                    self.toast(e.to_string());
                                }
                            }
                        }
                    }
                    return;
                }
                if self.drag.is_some() {
                    self.finish_drag().await;
                } else if self.selecting {
                    self.selecting = false;
                    match self.select {
                        Some(sel) if !sel.is_empty() => self.copy_selection(),
                        _ => self.select = None, // plain click, not a drag
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                // Middle-click a tab to close it.
                if Some(row) == self.frame.tabs && col >= self.frame.main.x {
                    if let Some(tab) = self
                        .tab_hits
                        .iter()
                        .find(|(_, r)| r.contains(&col))
                        .and_then(|(id, _)| *id)
                    {
                        if let Err(e) = self.client.request(Request::CloseTab { tab }).await {
                            self.toast(e.to_string());
                        }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Right-click a sidebar row → space/tab menu.
                let sidebar_hit = self.frame.sidebar.map(|sb| {
                    col >= sb.x && col < sb.x + sb.width && row >= sb.y
                }).unwrap_or(false)
                    || (self.drawer && {
                        let dr = self.drawer_rect();
                        col >= dr.x && col < dr.x + dr.width && row >= dr.y
                    });
                if sidebar_hit {
                    if let Some(target) = self.sidebar_rows.iter().find(|(r, _)| *r == row).map(|(_, t)| *t) {
                        let (mt, items) = match target {
                            Target::Space(id) => (
                                MenuTarget::Space(id),
                                vec![
                                    ("new tab", MenuAction::NewTab),
                                    ("new space", MenuAction::NewSpace),
                                    ("rename space", MenuAction::RenameSpace),
                                    ("move up", MenuAction::MoveSpaceUp),
                                    ("move down", MenuAction::MoveSpaceDown),
                                    ("close space", MenuAction::CloseSpace),
                                ],
                            ),
                            Target::Tab { space, tab, pane } => (
                                MenuTarget::Tab { space, tab, pane },
                                vec![
                                    ("rename tab", MenuAction::RenameTab),
                                    ("move left", MenuAction::MoveTabLeft),
                                    ("move right", MenuAction::MoveTabRight),
                                    ("close tab", MenuAction::CloseTab),
                                    ("new tab", MenuAction::NewTab),
                                    ("new space", MenuAction::NewSpace),
                                ],
                            ),
                            Target::Pane(p) => (MenuTarget::Pane(p), vec![]),
                        };
                        if !items.is_empty() {
                            self.menu = Some(Menu { x: col, y: row, target: mt, items });
                        }
                    }
                    return;
                }
                if let Some(pane) = self.pane_at(col, row) {
                    self.goto_pane(pane).await;
                    self.menu = Some(Menu {
                        x: col,
                        y: row,
                        target: MenuTarget::Pane(pane),
                        items: vec![
                            ("split right", MenuAction::SplitRight),
                            ("split down", MenuAction::SplitDown),
                            ("zoom", MenuAction::Zoom),
                            ("new tab", MenuAction::NewTab),
                            ("rename tab", MenuAction::RenameTab),
                            ("rename space", MenuAction::RenameSpace),
                            ("restart", MenuAction::Restart),
                            ("close pane", MenuAction::ClosePane),
                        ],
                    });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Any fresh left-press clears a prior selection.
                self.select = None;
                self.selecting = false;
                // Alt+drag a pane onto another swaps them.
                if alt {
                    if let Some(pane) = self.pane_at(col, row) {
                        self.swap_from = Some(pane);
                        return;
                    }
                }
                if self.help {
                    self.help = false;
                    return;
                }
                if self.drawer {
                    let dr = self.drawer_rect();
                    if col >= dr.x && col < dr.x + dr.width && row >= dr.y {
                        let target = self
                            .sidebar_rows
                            .iter()
                            .find(|(r, _)| *r == row)
                            .map(|(_, t)| *t);
                        if let Some(t) = target {
                            self.drawer = false;
                            self.handle_sidebar_target(t).await;
                        }
                        return;
                    }
                    self.drawer = false;
                    return;
                }
                if let Some(m) = self.menu.clone() {
                    let r = self.menu_rect(&m);
                    self.menu = None;
                    if col > r.x && col < r.x + r.width - 1 && row > r.y && row < r.y + r.height - 1
                    {
                        let idx = (row - r.y - 1) as usize;
                        if let Some((_, action)) = m.items.get(idx) {
                            self.run_menu_item(*action, m.target).await;
                        }
                    }
                    return;
                }
                let pr = self.frame.panes;
                if row >= pr.y && row < pr.y + pr.height && col >= pr.x && col < pr.x + pr.width {
                    if let Some((path, index, dir)) = self.border_hit(col, row) {
                        if let Some(t) = self.active_tab() {
                            self.drag = Some(Drag { tab: t.id, path, index, dir });
                        }
                        return;
                    }
                }
                if Some(row) == self.frame.header {
                    if col < 10 {
                        self.do_action(Action::ToggleSidebar).await;
                    } else if col > self.size.0.saturating_sub(40) {
                        self.do_action(Action::JumpWaiting).await;
                    }
                    return;
                }
                if Some(row) == self.frame.footer {
                    let hit = self
                        .footer_hits
                        .iter()
                        .find(|(_, r)| r.contains(&col))
                        .map(|(a, _)| *a);
                    if let Some(a) = hit {
                        self.do_action(a).await;
                    }
                    return;
                }
                if let Some(sb) = self.frame.sidebar {
                    if col >= sb.x && col < sb.x + sb.width && row >= sb.y {
                        // Visible buttons (+ new space, × close space) win over row-select.
                        if let Some(btn) = self
                            .sidebar_buttons
                            .iter()
                            .find(|(r, cr, _)| *r == row && cr.contains(&col))
                            .map(|(_, _, b)| *b)
                        {
                            match btn {
                                SidebarBtn::NewSpace => self.open_prompt(PromptKind::NewSpace),
                                SidebarBtn::CloseSpace(id) => {
                                    if let Err(e) =
                                        self.client.request(Request::CloseSpace { space: id }).await
                                    {
                                        self.toast(e.to_string());
                                    }
                                }
                            }
                            return;
                        }
                        let target = self
                            .sidebar_rows
                            .iter()
                            .find(|(r, _)| *r == row)
                            .map(|(_, t)| *t);
                        if let Some(t) = target {
                            if let Target::Space(id) = t {
                                self.space_drag = Some(id); // arm reorder-drag
                            }
                            self.handle_sidebar_target(t).await;
                        }
                        return;
                    }
                }
                if Some(row) == self.frame.tabs && col >= self.frame.main.x {
                    // Close button (×) takes precedence over switching.
                    if let Some(tab) =
                        self.tab_close_hits.iter().find(|(_, r)| r.contains(&col)).map(|(id, _)| *id)
                    {
                        if let Err(e) = self.client.request(Request::CloseTab { tab }).await {
                            self.toast(e.to_string());
                        }
                        return;
                    }
                    let hit = self
                        .tab_hits
                        .iter()
                        .find(|(_, r)| r.contains(&col))
                        .map(|(id, _)| *id);
                    match hit {
                        Some(Some(tab)) => {
                            // Arm a potential reorder-drag and switch to the tab.
                            self.tab_drag = Some(tab);
                            if let Some(s) = self.active_space() {
                                let target = s
                                    .tabs
                                    .iter()
                                    .find(|t| t.id == tab)
                                    .map(|t| (s.id, t.id, t.active_pane));
                                if let Some((sp, t, p)) = target {
                                    self.set_active(sp, t, p).await;
                                }
                            }
                        }
                        Some(None) => self.open_prompt(PromptKind::NewTab),
                        None => {}
                    }
                } else if let Some(pane) = self.pane_at(col, row) {
                    if let Some((s, t)) = self.locate(pane) {
                        self.set_active(s, t, pane).await;
                    }
                    // Begin a text selection anchored at the clicked cell.
                    if self.cfg.ui.mouse_select {
                        if let Some((pid, cell)) = self.cell_at(col, row) {
                            self.select = Some(Sel { pane: pid, anchor: cell, head: cell });
                            self.selecting = true;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(pane) = self.pane_at(col, row) {
                    self.scroll_by(pane, 3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(pane) = self.pane_at(col, row) {
                    self.scroll_by(pane, -3);
                }
            }
            _ => {}
        }
    }

    async fn on_server(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::State { snapshot } => {
                self.drag = None;
                self.snap = snapshot;
                self.sync().await;
            }
            ServerMsg::Output { pane, data } => {
                if let Some(v) = self.views.get_mut(&pane) {
                    if let Ok(bytes) = B64.decode(data.as_bytes()) {
                        v.parser.process(&bytes);
                    }
                }
            }
            ServerMsg::Activity { pane, activity } => {
                let prev = self.snap.pane(pane).map(|p| p.activity);
                if let Some(p) = self.snap.pane_mut(pane) {
                    p.activity = activity;
                }
                // Flag as unread if it finished / now needs input while unfocused.
                if pane != self.focused {
                    let notable = matches!(activity, Activity::Waiting | Activity::Done)
                        || (prev == Some(Activity::Working) && activity == Activity::Idle);
                    if notable {
                        self.unread.insert(pane);
                    }
                }
            }
            ServerMsg::Exited { pane, code } => {
                if let Some(p) = self.snap.pane_mut(pane) {
                    p.status = PaneStatus::Exited { code };
                    p.activity = Activity::Done;
                }
                if pane == self.focused {
                    self.seen.insert(pane);
                } else {
                    self.unread.insert(pane);
                }
            }
            ServerMsg::PaneImage { pane, data } => {
                if data.is_empty() {
                    self.images.remove(&pane);
                } else if let Ok(bytes) = B64.decode(data.as_bytes()) {
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        self.images.insert(
                            pane,
                            ImageState { img, proto: None, area: None },
                        );
                    }
                }
            }
            ServerMsg::ConfigChanged => self.reload_config().await,
            _ => {}
        }
    }

    /// Re-read config.toml live. Theme/glyphs/keys/templates apply on the next
    /// render; layout-affecting settings re-fit panes via sync(); a mouse-capture
    /// change is applied against the terminal immediately.
    async fn reload_config(&mut self) {
        let was_mouse = self.cfg.ui.mouse;
        self.cfg = Config::load();
        if self.cfg.ui.mouse != was_mouse {
            let mut out = std::io::stdout();
            if self.cfg.ui.mouse {
                let _ = crossterm::execute!(out, EnableMouseCapture);
            } else {
                let _ = crossterm::execute!(out, DisableMouseCapture);
            }
        }
        self.sync().await;
        self.toast("config reloaded");
    }

    async fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.on_key(k).await,
            Event::Mouse(m) => self.on_mouse(m).await,
            Event::Resize(_, _) => self.sync().await,
            _ => {}
        }
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Some((_, at)) = &self.toast {
            if at.elapsed().as_secs() >= self.cfg.ui.toast_seconds {
                self.toast = None;
            }
        }
    }

    fn spin(&self) -> String {
        let s = &self.cfg.glyphs.spinner;
        s[self.tick % s.len()].clone()
    }

    /// The working-state indicator (glyph + color), per `ui.working_style`.
    fn working_indicator(&self) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match self.cfg.ui.working_style {
            WorkingStyle::Spinner => (self.spin(), th.working),
            WorkingStyle::Dot => (g.working.clone(), th.working),
            WorkingStyle::Pulse => {
                // gentle two-step color pulse on a steady dot
                let bright = (self.tick / 3) % 2 == 0;
                (g.working.clone(), if bright { th.working } else { th.idle })
            }
        }
    }

    fn glyph(&self, info: Option<&PaneInfo>) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match info.map(|i| (i.activity, i.status)) {
            Some((Activity::Working, _)) => self.working_indicator(),
            Some((Activity::Waiting, _)) => {
                let s = if (self.tick / 4) % 2 == 0 { &g.waiting } else { &g.idle };
                (s.clone(), th.waiting)
            }
            Some((Activity::Done, PaneStatus::Exited { code })) => (
                g.done.clone(),
                if code == 0 { th.done_ok } else { th.done_err },
            ),
            Some((Activity::Done, _)) => (g.done.clone(), th.done_ok),
            _ => (g.idle.clone(), th.idle),
        }
    }

    fn state_glyph(&self, a: Activity) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match a {
            Activity::Working => self.working_indicator(),
            Activity::Waiting => {
                let s = if (self.tick / 4) % 2 == 0 { &g.waiting } else { &g.idle };
                (s.clone(), th.waiting)
            }
            Activity::Done => (g.done.clone(), th.done_ok),
            Activity::Idle => (g.idle.clone(), th.idle),
        }
    }

    fn hover_at(&self, col_range: &std::ops::Range<u16>, row: u16) -> bool {
        self.hover
            .map(|(c, r)| r == row && col_range.contains(&c))
            .unwrap_or(false)
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let th = &self.cfg.theme;
        let logo_text = if self.narrow() { " ☰ ruckus " } else { "  ruckus  " };
        let logo = Span::styled(
            logo_text,
            Style::default().bg(th.accent).fg(th.sidebar_bg).add_modifier(Modifier::BOLD),
        );
        let crumb = match (self.active_space(), self.active_tab()) {
            (Some(s), Some(t)) => format!("  {}  ›  {}", s.name, t.name),
            (Some(s), None) => format!("  {}", s.name),
            _ => String::new(),
        };
        let crumb_span = Span::styled(crumb.clone(), Style::default().fg(th.bar_active_fg));

        let waiting = self.snap.panes.iter().filter(|p| p.activity == Activity::Waiting).count();
        let working = self.snap.panes.iter().filter(|p| p.activity == Activity::Working).count();
        let done = self
            .snap
            .panes
            .iter()
            .filter(|p| p.activity == Activity::Done && !self.seen.contains(&p.id))
            .count();
        let mut right: Vec<Span> = Vec::new();
        let mut right_len = 0usize;
        for (n, label, activity) in [
            (waiting, "waiting", Activity::Waiting),
            (working, "working", Activity::Working),
            (done, "done", Activity::Done),
        ] {
            if n > 0 {
                let (g, color) = self.state_glyph(activity);
                let text = format!("{n} {label}   ");
                right_len += g.chars().count() + 1 + text.chars().count();
                right.push(Span::styled(format!("{g} "), Style::default().fg(color)));
                right.push(Span::styled(text, Style::default().fg(th.bar_active_fg)));
            }
        }

        let used = 10 + crumb.chars().count() + right_len;
        let pad = (area.width as usize).saturating_sub(used);
        let mut spans = vec![logo, crumb_span, Span::raw(" ".repeat(pad))];
        spans.extend(right);
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let th = self.cfg.theme.clone();
        let inner = area;
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(th.sidebar_bg)),
            area,
        );

        let w = inner.width as usize;
        let hover_row = self
            .hover
            .filter(|(c, _)| *c >= area.x && *c < area.x + area.width)
            .map(|(_, r)| r);
        let mut lines: Vec<Line> = Vec::new();
        let mut rows: Vec<(u16, Target)> = Vec::new();
        let mut buttons: Vec<(u16, std::ops::Range<u16>, SidebarBtn)> = Vec::new();
        let mut y = inner.y;

        macro_rules! push {
            ($line:expr, $target:expr) => {{
                if let Some(t) = $target {
                    rows.push((y, t));
                }
                lines.push($line);
                #[allow(unused_assignments)]
                {
                    y += 1;
                }
            }};
        }

        push!(Line::raw(""), None::<Target>);

        for section in self.cfg.ui.sidebar_sections.clone() {
            match section.as_str() {
                "needs_you" => {
                    let attention: Vec<(u64, Vec<(&str, String)>, (String, Color))> = self
                        .attention()
                        .iter()
                        .map(|p| {
                            (
                                p.id,
                                vec![
                                    ("title", p.title.clone()),
                                    ("name", p.title.clone()),
                                    ("id", p.id.to_string()),
                                    ("cmd", p.cmd.join(" ")),
                                    ("cwd", p.cwd.clone()),
                                ],
                                self.glyph(Some(p)),
                            )
                        })
                        .collect();
                    if attention.is_empty() {
                        continue;
                    }
                    push!(
                        Line::from(Span::styled(
                            " NEEDS YOU",
                            Style::default().fg(th.status_fg).add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
                    let tpl = self.cfg.ui.queue_row.clone();
                    for (id, vars, icon) in attention {
                        let selected = id == self.focused;
                        let hovered = hover_row == Some(y);
                        let row_style = if selected || hovered {
                            Style::default().bg(th.select_bg)
                        } else {
                            Style::default()
                        };
                        let mut spans =
                            vec![Span::styled("  ".to_string(), row_style)];
                        spans.extend(template_spans(
                            &tpl,
                            &icon,
                            &vars,
                            row_style,
                            th.bar_active_fg,
                        ));
                        let pad = w.saturating_sub(spans_width(&spans));
                        spans.push(Span::styled(" ".repeat(pad), row_style));
                        push!(Line::from(spans), Some(Target::Pane(id)));
                    }
                    push!(Line::raw(""), None::<Target>);
                }
                "agents" => {
                    let agents = self.agent_rows();
                    if agents.is_empty() {
                        continue;
                    }
                    push!(
                        Line::from(Span::styled(
                            " AGENTS",
                            Style::default().fg(th.status_fg).add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
                    for (pid, tab_name, space_name) in agents {
                        let (g, mut color) = self
                            .snap
                            .pane(pid)
                            .map(|p| self.glyph(Some(p)))
                            .unwrap_or_else(|| (self.cfg.glyphs.idle.clone(), th.idle));
                        if self.unread.contains(&pid) {
                            color = th.accent;
                        }
                        let selected = pid == self.focused;
                        let hovered = hover_row == Some(y);
                        let row_style = if selected || hovered {
                            Style::default().bg(th.select_bg)
                        } else {
                            Style::default()
                        };
                        let name_fg = if selected { th.bar_active_fg } else { th.bar_active_fg };
                        let mut spans = vec![
                            Span::styled("  ".to_string(), row_style),
                            Span::styled(format!("{g} "), row_style.fg(color)),
                            Span::styled(tab_name, row_style.fg(name_fg)),
                            Span::styled(format!("  {space_name}"), row_style.fg(th.status_fg)),
                        ];
                        let pad = w.saturating_sub(spans_width(&spans));
                        spans.push(Span::styled(" ".repeat(pad), row_style));
                        push!(Line::from(spans), Some(Target::Pane(pid)));
                    }
                    push!(Line::raw(""), None::<Target>);
                }
                "spaces" => {
                    push!(
                        Line::from(Span::styled(
                            " SPACES",
                            Style::default().fg(th.status_fg).add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
                    // Persistent, discoverable "new space" button.
                    {
                        let hov = hover_row == Some(y);
                        let style = if hov {
                            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(th.status_fg)
                        };
                        buttons.push((y, inner.x + 1..inner.x + 13, SidebarBtn::NewSpace));
                        push!(Line::from(Span::styled("  + new space", style)), None::<Target>);
                    }
                    let spaces = self.snap.spaces.clone();
                    let space_tpl = self.cfg.ui.space_row.clone();
                    let tab_tpl = self.cfg.ui.tab_row.clone();
                    let row_gap = self.cfg.ui.sidebar_row_gap;
                    let marker_on = self.cfg.ui.sidebar_marker;
                    let marker = self.cfg.glyphs.focus.clone();
                    let space_sub = self.cfg.ui.space_subtitle.clone();
                    let tab_sub = self.cfg.ui.tab_subtitle.clone();
                    let tab_numbers = self.cfg.ui.tab_numbers;
                    let show_tabs = self.cfg.ui.sidebar_tabs;
                    let no_icon = (String::new(), th.status_fg);
                    for (_si, s) in spaces.iter().enumerate() {
                        for _ in 0..row_gap {
                            push!(Line::raw(""), None::<Target>);
                        }
                        let s_active = s.id == self.snap.active_space;
                        let mut icon = self.state_glyph(space_activity(&self.snap, s));
                        if self.space_unread(s) {
                            icon.1 = th.accent; // unread badge
                        }
                        let hovered = hover_row == Some(y);
                        let row_style = if s_active || hovered {
                            Style::default().bg(th.select_bg)
                        } else {
                            Style::default()
                        };
                        let text_fg = if s_active { th.accent } else { th.bar_active_fg };
                        let vars = vec![
                            ("name", s.name.clone()),
                            ("title", s.name.clone()),
                            ("id", s.id.to_string()),
                            ("tabs", s.tabs.len().to_string()),
                            (
                                "active",
                                s.tabs
                                    .iter()
                                    .find(|t| t.id == s.active_tab)
                                    .map(|t| t.name.clone())
                                    .unwrap_or_default(),
                            ),
                        ];
                        let lead = if marker_on && s_active {
                            Span::styled(marker.clone(), row_style.fg(th.accent))
                        } else {
                            Span::styled(" ".to_string(), row_style)
                        };
                        let mut spans = vec![lead];
                        spans.extend(
                            template_spans(
                                &space_tpl,
                                &icon,
                                &vars,
                                row_style.add_modifier(Modifier::BOLD),
                                text_fg,
                            ),
                        );
                        // Hover reveals a × close button at the row's right edge.
                        let row_hovered = hover_row == Some(y);
                        if row_hovered && w >= 3 {
                            let pad = w.saturating_sub(spans_width(&spans) + 2);
                            spans.push(Span::styled(" ".repeat(pad), row_style));
                            spans.push(Span::styled(" ×".to_string(), row_style.fg(th.done_err)));
                            let x = inner.x + (w as u16).saturating_sub(2);
                            buttons.push((y, x..x + 2, SidebarBtn::CloseSpace(s.id)));
                        } else {
                            let pad = w.saturating_sub(spans_width(&spans));
                            spans.push(Span::styled(" ".repeat(pad), row_style));
                        }
                        push!(Line::from(spans), Some(Target::Space(s.id)));

                        if !space_sub.is_empty() {
                            let sub_fg = if s_active { th.bar_fg } else { th.status_fg };
                            let mut sub = vec![Span::styled("   ".to_string(), row_style)];
                            sub.extend(template_spans(
                                &space_sub, &no_icon, &vars, row_style, sub_fg,
                            ));
                            let pad = w.saturating_sub(spans_width(&sub));
                            sub.push(Span::styled(" ".repeat(pad), row_style));
                            push!(Line::from(sub), Some(Target::Space(s.id)));
                        }

                        for (ti, t) in s.tabs.iter().enumerate() {
                            if !show_tabs {
                                break;
                            }
                            for _ in 0..row_gap {
                                push!(Line::raw(""), None::<Target>);
                            }
                            let t_active = s_active && t.id == s.active_tab;
                            let mut icon = self.state_glyph(tab_activity(&self.snap, t));
                            if self.tab_unread(t) {
                                icon.1 = th.accent; // unread badge
                            }
                            let hovered = hover_row == Some(y);
                            let row_style = if t_active || hovered {
                                Style::default().bg(th.select_bg)
                            } else {
                                Style::default()
                            };
                            let text_fg = if t_active { th.bar_active_fg } else { th.bar_fg };
                            let active_pane = self.snap.pane(t.active_pane);
                            let vars = vec![
                                ("title", t.name.clone()),
                                ("name", t.name.clone()),
                                ("id", t.id.to_string()),
                                (
                                    "cmd",
                                    active_pane.map(|p| p.cmd.join(" ")).unwrap_or_default(),
                                ),
                                (
                                    "cwd",
                                    active_pane.map(|p| home_relative(&p.cwd)).unwrap_or_default(),
                                ),
                            ];
                            let mut spans = if marker_on && t_active {
                                vec![
                                    Span::styled("  ".to_string(), row_style),
                                    Span::styled(marker.clone(), row_style.fg(th.accent)),
                                    Span::styled(" ".to_string(), row_style),
                                ]
                            } else {
                                vec![Span::styled("    ".to_string(), row_style)]
                            };
                            if tab_numbers {
                                spans.push(Span::styled(
                                    format!("{} ", ti + 1),
                                    row_style.fg(th.status_fg),
                                ));
                            }
                            spans.extend(template_spans(
                                &tab_tpl, &icon, &vars, row_style, text_fg,
                            ));
                            let pad = w.saturating_sub(spans_width(&spans));
                            spans.push(Span::styled(" ".repeat(pad), row_style));
                            let tab_target = Target::Tab {
                                space: s.id,
                                tab: t.id,
                                pane: t.active_pane,
                            };
                            push!(Line::from(spans), Some(tab_target.clone()));

                            if !tab_sub.is_empty() {
                                let sub_fg = if t_active { th.bar_fg } else { th.status_fg };
                                let mut sub =
                                    vec![Span::styled("      ".to_string(), row_style)];
                                sub.extend(template_spans(
                                    &tab_sub, &no_icon, &vars, row_style, sub_fg,
                                ));
                                let pad = w.saturating_sub(spans_width(&sub));
                                sub.push(Span::styled(" ".repeat(pad), row_style));
                                push!(Line::from(sub), Some(tab_target.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        lines.truncate(inner.height as usize);
        self.sidebar_rows = rows;
        self.sidebar_buttons = buttons;
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(th.sidebar_bg)),
            inner,
        );
    }

    fn draw_tab_strip(&mut self, f: &mut Frame, area: Rect) {
        let th = self.cfg.theme.clone();
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        let mut hits: Vec<(Option<u64>, std::ops::Range<u16>)> = Vec::new();
        let mut close_hits: Vec<(u64, std::ops::Range<u16>)> = Vec::new();
        let mut x = area.x + 1;
        let pad = " ".repeat(self.cfg.ui.tab_pad as usize);
        let plen = pad.chars().count();
        if let Some(s) = self.active_space() {
            for (i, t) in s.tabs.iter().enumerate() {
                let active = t.id == s.active_tab;
                let (g, mut color) = self.state_glyph(tab_activity(&self.snap, t));
                if self.tab_unread(t) {
                    color = th.accent; // unread badge
                }
                let text = if self.cfg.ui.tab_numbers {
                    format!("{} {}", i + 1, t.name)
                } else {
                    t.name.clone()
                };
                let tlen = text.chars().count();
                // Pill: pad + "icon " + text + " ×" + pad, then a gap.
                let width = (plen * 2 + tlen + 4) as u16;
                let range = x..x + width;
                let hovered = self.hover_at(&range, area.y);
                let (bg, fg, bold) = if active {
                    (th.select_bg, th.bar_active_fg, true)
                } else if hovered {
                    (th.select_bg, th.bar_active_fg, false)
                } else {
                    (th.surface, th.bar_fg, false)
                };
                let mut base = Style::default().bg(bg).fg(fg);
                if bold {
                    base = base.add_modifier(Modifier::BOLD);
                }
                let close_col = x + (plen + tlen + 3) as u16;
                let close_hover = self.hover_at(&(close_col..close_col + 1), area.y);
                spans.push(Span::styled(pad.clone(), base));
                spans.push(Span::styled(format!("{g} "), base.fg(color)));
                spans.push(Span::styled(text, base));
                spans.push(Span::styled(" ".to_string(), base));
                spans.push(Span::styled(
                    "×".to_string(),
                    base.fg(if close_hover { th.done_err } else { th.status_fg }),
                ));
                spans.push(Span::styled(pad.clone(), base));
                spans.push(Span::raw(" ")); // gap between pills
                hits.push((Some(t.id), range));
                close_hits.push((t.id, close_col..close_col + 1));
                x += width + 1;
            }
        }
        let plus_range = x..x + 3;
        let plus_hover = self.hover_at(&plus_range, area.y);
        spans.push(Span::styled(
            " + ",
            if plus_hover {
                Style::default().bg(th.select_bg).fg(th.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.status_fg)
            },
        ));
        hits.push((None, plus_range));
        self.tab_hits = hits;
        self.tab_close_hits = close_hits;
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        let th = self.cfg.theme.clone();
        let c = &self.cfg;
        let compact = area.width < FOOTER_COMPACT;
        let mut chips: Vec<(Action, String, &str)> = vec![
            (Action::JumpWaiting, c.label(Action::JumpWaiting), "next"),
            (Action::SplitRight, c.label(Action::SplitRight), "split"),
            (Action::SplitDown, c.label(Action::SplitDown), "split↓"),
            (Action::NewTab, c.label(Action::NewTab), "tab"),
            (Action::NewSpace, c.label(Action::NewSpace), "space"),
            (Action::ClosePane, c.label(Action::ClosePane), "close"),
            (Action::ToggleSidebar, c.label(Action::ToggleSidebar), "bar"),
            (Action::ShowHelp, c.label(Action::ShowHelp), "help"),
            (Action::Quit, c.label(Action::Quit), "quit"),
        ];
        if compact {
            chips = vec![
                (Action::JumpWaiting, "".into(), "next"),
                (Action::SplitRight, "".into(), "split"),
                (Action::SplitDown, "".into(), "split↓"),
                (Action::NewTab, "".into(), "+tab"),
                (Action::ClosePane, "".into(), "close"),
                (Action::ShowHelp, "".into(), "?"),
                (Action::Quit, "".into(), "quit"),
            ];
        }
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        let mut hits: Vec<(Action, std::ops::Range<u16>)> = Vec::new();
        let mut x: u16 = 1;
        for (action, key, label) in chips {
            let text = if key.is_empty() {
                format!("[{label}]")
            } else {
                format!("{key} {label}")
            };
            let width = text.chars().count() as u16;
            let range = x..x + width;
            let hovered = self.hover_at(&range, area.y);
            let key_style = if hovered {
                Style::default().bg(th.select_bg).fg(th.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
            };
            let label_style = if hovered {
                Style::default().bg(th.select_bg).fg(th.bar_active_fg)
            } else {
                Style::default().fg(th.status_fg)
            };
            if key.is_empty() {
                spans.push(Span::styled(text.clone(), key_style));
            } else {
                spans.push(Span::styled(key.clone(), key_style));
                spans.push(Span::styled(format!(" {label}"), label_style));
            }
            spans.push(Span::raw("   "));
            hits.push((action, range));
            x += width + 3;
        }
        self.footer_hits = hits;
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    fn draw_panes(&mut self, f: &mut Frame) {
        let th = self.cfg.theme.clone();
        let rects = self.pane_rects.clone();
        let many = rects.len() > 1;
        let titles = self.cfg.ui.pane_titles;
        let pad = self.cfg.ui.pane_padding;
        for (pane, rect) in &rects {
            if rect.width < 3 || rect.height < 2 {
                continue;
            }
            let focused = *pane == self.focused;
            let info = self.snap.pane(*pane);
            let (g, dot) = self.glyph(info);
            let title_text = info.map(|i| i.title.clone()).unwrap_or_else(|| pane.to_string());
            let scroll = self.views.get(pane).map(|v| v.scroll).unwrap_or(0);

            // Whole pane sits on the surface layer (padding shows as surface).
            f.render_widget(
                Paragraph::new("").style(Style::default().bg(th.surface)),
                *rect,
            );

            let mut content_y = rect.y;
            let mut content_h = rect.height;
            if titles {
                let bar_bg = if focused { th.select_bg } else { th.bar_bg };
                let mut title_spans = vec![
                    Span::styled(
                        if focused { self.cfg.glyphs.focus.clone() } else { " ".to_string() },
                        Style::default().fg(th.accent).bg(bar_bg),
                    ),
                    Span::styled(format!("{g} "), Style::default().fg(dot).bg(bar_bg)),
                    Span::styled(
                        format!("{title_text} "),
                        if focused {
                            Style::default()
                                .fg(th.bar_active_fg)
                                .bg(bar_bg)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(th.bar_fg).bg(bar_bg)
                        },
                    ),
                ];
                if scroll > 0 {
                    title_spans.push(Span::styled(
                        format!(" ↑{scroll} "),
                        Style::default().fg(th.waiting).bg(bar_bg),
                    ));
                }
                if let Some(PaneInfo { status: PaneStatus::Exited { code }, .. }) = info {
                    title_spans.push(Span::styled(
                        format!(" exit {code} "),
                        Style::default()
                            .fg(if *code == 0 { th.done_ok } else { th.done_err })
                            .bg(bar_bg),
                    ));
                }
                f.render_widget(
                    Paragraph::new(Line::from(title_spans))
                        .style(Style::default().bg(bar_bg)),
                    Rect::new(rect.x, rect.y, rect.width, 1),
                );
                content_y += 1;
                content_h = content_h.saturating_sub(1);
            }

            let content = Rect::new(
                rect.x + pad,
                content_y + pad,
                rect.width.saturating_sub(2 * pad),
                content_h.saturating_sub(2 * pad),
            );
            if content.width == 0 || content.height == 0 {
                continue;
            }
            // Image pane: if a frame was pushed and the terminal supports graphics,
            // draw the image (kitty/iterm2/sixel) instead of the text grid.
            if self.picker.protocol_type() != ProtocolType::Halfblocks
                && self.images.contains_key(pane)
            {
                let need = self
                    .images
                    .get(pane)
                    .map(|s| s.proto.is_none() || s.area != Some(content))
                    .unwrap_or(false);
                if need {
                    let img = self.images.get(pane).unwrap().img.clone();
                    if let Ok(proto) = self.picker.new_protocol(img, content, Resize::Fit(None)) {
                        let s = self.images.get_mut(pane).unwrap();
                        s.proto = Some(proto);
                        s.area = Some(content);
                    }
                }
                if let Some(proto) = self.images.get(pane).and_then(|s| s.proto.as_ref()) {
                    f.render_widget(Image::new(proto), content);
                }
                continue;
            }
            let dimmed = many && !focused;
            if let Some(v) = self.views.get(pane) {
                let lines = screen_to_lines(v.parser.screen(), focused && scroll == 0, dimmed);
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(th.surface)),
                    content,
                );
            }
        }
    }

    /// Draw a subtle line in each split gutter so seams read as intentional.
    fn draw_dividers(&self, f: &mut Frame) {
        if !self.cfg.ui.pane_divider || self.zoomed || self.cfg.ui.gutter == 0 {
            return;
        }
        let Some(t) = self.active_tab() else { return };
        let mut segs = Vec::new();
        node_dividers(&t.layout, self.frame.panes, self.cfg.ui.gutter, &mut segs);
        let style = Style::default().fg(self.cfg.theme.border).bg(self.cfg.theme.bg);
        for (rect, vertical) in segs {
            if vertical {
                let lines: Vec<Line> = (0..rect.height)
                    .map(|_| Line::from(Span::styled("│", style)))
                    .collect();
                f.render_widget(Paragraph::new(lines), rect);
            } else {
                let bar = "─".repeat(rect.width as usize);
                f.render_widget(Paragraph::new(Line::from(Span::styled(bar, style))), rect);
            }
        }
    }

    /// Tint the selected cells so a drag-selection is visible.
    fn draw_selection(&self, f: &mut Frame) {
        let Some(sel) = self.select else { return };
        let Some(rect) = self.pane_rects.iter().find(|(p, _)| *p == sel.pane).map(|(_, r)| *r)
        else {
            return;
        };
        let c = self.pane_content_rect(rect);
        if c.width == 0 || c.height == 0 {
            return;
        }
        let ((sr, sc), (er, ec)) = sel.ordered();
        let bg = self.cfg.theme.select_bg;
        let last_row = c.height.saturating_sub(1);
        let last_col = c.width.saturating_sub(1);
        let buf = f.buffer_mut();
        for r in sr..=er.min(last_row) {
            let (cs, ce) = if sr == er {
                (sc, ec)
            } else if r == sr {
                (sc, last_col)
            } else if r == er {
                (0, ec)
            } else {
                (0, last_col)
            };
            for cc in cs..=ce.min(last_col) {
                if let Some(cell) = buf.cell_mut((c.x + cc, c.y + r)) {
                    cell.set_bg(bg);
                }
            }
        }
    }

    fn draw_menu(&self, f: &mut Frame) {
        let Some(m) = &self.menu else { return };
        let th = &self.cfg.theme;
        let r = self.menu_rect(m);
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.sidebar_bg));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let lines: Vec<Line> = m
            .items
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                let row = r.y + 1 + i as u16;
                let hovered = self
                    .hover
                    .map(|(c, rr)| rr == row && c > r.x && c < r.x + r.width - 1)
                    .unwrap_or(false);
                let style = if hovered {
                    Style::default().bg(th.select_bg).fg(th.bar_active_fg)
                } else {
                    Style::default().fg(th.bar_active_fg)
                };
                Line::from(Span::styled(
                    format!(
                        " {label}{} ",
                        " ".repeat((inner.width as usize).saturating_sub(label.chars().count() + 2))
                    ),
                    style,
                ))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_help(&self, f: &mut Frame) {
        if !self.help {
            return;
        }
        let th = &self.cfg.theme;
        let entries: Vec<(String, &str)> = vec![
            (self.cfg.label(Action::JumpWaiting), "jump to next pane that needs you"),
            (self.cfg.label(Action::SplitRight), "split pane right"),
            (self.cfg.label(Action::SplitDown), "split pane down"),
            (self.cfg.label(Action::ClosePane), "close pane"),
            (self.cfg.label(Action::NextPane), "focus next pane"),
            (self.cfg.label(Action::PrevPane), "focus previous pane"),
            (self.cfg.label(Action::NewTab), "new tab"),
            (self.cfg.label(Action::NextTab), "next tab"),
            (self.cfg.label(Action::PrevTab), "previous tab"),
            (self.cfg.label(Action::NewSpace), "new space"),
            (self.cfg.label(Action::NextSpace), "next space"),
            (self.cfg.label(Action::PrevSpace), "previous space"),
            (self.cfg.label(Action::ScrollUp), "scroll history up"),
            (self.cfg.label(Action::ScrollDown), "scroll history down"),
            (self.cfg.label(Action::ToggleSidebar), "toggle sidebar"),
            (self.cfg.label(Action::Zoom), "zoom focused pane"),
            ("alt+1..9".into(), "jump to tab"),
            ("enter".into(), "restart a finished pane"),
            (self.cfg.label(Action::Quit), "quit (daemon keeps running)"),
            ("".into(), ""),
            ("mouse".into(), "click to focus · right-click for menu"),
            ("".into(), "drag pane gutters to resize · wheel scrolls"),
        ];
        let w: u16 = 56;
        let h = entries.len() as u16 + 4;
        let x = self.size.0.saturating_sub(w) / 2;
        let y = self.size.1.saturating_sub(h) / 2;
        let r = Rect::new(x, y, w.min(self.size.0), h.min(self.size.1));
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.sidebar_bg))
            .title(Line::from(Span::styled(
                " keys ",
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let lines: Vec<Line> = std::iter::once(Line::raw(""))
            .chain(entries.iter().map(|(k, d)| {
                Line::from(vec![
                    Span::styled(
                        format!("  {k:>12}  "),
                        Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(d.to_string(), Style::default().fg(th.bar_active_fg)),
                ])
            }))
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_prompt(&self, f: &mut Frame) {
        let Some(p) = &self.prompt else { return };
        let th = &self.cfg.theme;
        let w: u16 = 52;
        let h: u16 = 6;
        let x = self.size.0.saturating_sub(w) / 2;
        let y = self.size.1.saturating_sub(h) / 3;
        let r = Rect::new(x, y, w.min(self.size.0), h.min(self.size.1));
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.sidebar_bg))
            .title(Line::from(Span::styled(
                format!(" {} ", p.label),
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled("  › ", Style::default().fg(th.accent)),
                Span::styled(p.buffer.clone(), Style::default().fg(th.bar_active_fg)),
                Span::styled("▎", Style::default().fg(th.accent)),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "  enter confirm · esc cancel",
                Style::default().fg(th.status_fg),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_toast(&self, f: &mut Frame) {
        let Some((msg, _)) = &self.toast else { return };
        let th = &self.cfg.theme;
        let (sw, sh) = self.size;
        let w = (msg.chars().count() as u16 + 4).min(sw.saturating_sub(2));
        let (x, y) = match self.cfg.ui.toast_pos {
            ToastPos::BottomRight => (sw.saturating_sub(w + 2), sh.saturating_sub(4)),
            ToastPos::BottomLeft => (1, sh.saturating_sub(4)),
            ToastPos::TopRight => (sw.saturating_sub(w + 2), 1),
            ToastPos::TopLeft => (1, 1),
        };
        let r = Rect::new(x, y, w, 3);
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.bar_bg));
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(th.bar_active_fg),
            ))),
            inner,
        );
    }

    fn draw(&mut self, f: &mut Frame) {
        self.size = (f.area().width, f.area().height);
        self.frame = self.compute_frame();
        let area = f.area();
        if area.height < 4 || area.width < 24 {
            f.render_widget(Paragraph::new("window too small"), area);
            return;
        }

        // Root layer: darkest background, visible as gutters between panes.
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(self.cfg.theme.bg)),
            area,
        );

        if let Some(r) = self.frame.header {
            self.draw_header(f, Rect::new(0, r, area.width, 1));
        }
        if let Some(sb) = self.frame.sidebar {
            self.draw_sidebar(f, sb);
        } else if !self.drawer {
            self.sidebar_rows.clear();
        }
        if let Some(r) = self.frame.tabs {
            let m = self.frame.main;
            self.draw_tab_strip(f, Rect::new(m.x, r, m.width, 1));
        }
        self.draw_panes(f);
        self.draw_dividers(f);
        self.draw_selection(f);
        if let Some(r) = self.frame.footer {
            self.draw_footer(f, Rect::new(0, r, area.width, 1));
        }
        if self.drawer && self.narrow() {
            let r = self.drawer_rect();
            f.render_widget(Clear, r);
            self.draw_sidebar(f, r);
        } else if !self.narrow() {
            self.drawer = false;
        }
        self.draw_menu(f);
        self.draw_help(f);
        self.draw_prompt(f);
        self.draw_toast(f);
    }
}

pub async fn run(initial: Option<String>) -> Result<()> {
    ensure_daemon().await?;
    let cfg = Config::load();
    let (client, mut events) = connect().await?;
    let snap = client.snapshot().await?;

    let focused = snap
        .spaces
        .iter()
        .find(|s| s.id == snap.active_space)
        .or(snap.spaces.first())
        .and_then(|s| s.tabs.iter().find(|t| t.id == s.active_tab).or(s.tabs.first()))
        .map(|t| t.active_pane)
        .unwrap_or(0);

    let sidebar = cfg.ui.sidebar_start_visible;
    let spinner_ms = cfg.ui.spinner_ms;
    let mouse = cfg.ui.mouse;
    // Detect the terminal's graphics protocol (kitty/iterm2/sixel) before we take
    // over the screen. Falls back to halfblocks (treated as "no graphics").
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((10, 20)));
    let mut app = App {
        cfg,
        client,
        snap,
        views: HashMap::new(),
        focused,
        seen: HashSet::new(),
        unread: HashSet::new(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".to_string()),
        running: true,
        toast: None,
        sidebar,
        zoomed: false,
        drawer: false,
        help: false,
        menu: None,
        prompt: None,
        drag: None,
        select: None,
        selecting: false,
        hover: None,
        tick: 0,
        size: crossterm::terminal::size().unwrap_or((80, 24)),
        frame: FrameLayout::default(),
        sidebar_rows: Vec::new(),
        sidebar_buttons: Vec::new(),
        tab_hits: Vec::new(),
        tab_close_hits: Vec::new(),
        tab_drag: None,
        space_drag: None,
        swap_from: None,
        footer_hits: Vec::new(),
        pane_rects: Vec::new(),
        picker,
        images: HashMap::new(),
    };

    if let Some(target) = initial {
        let pane = resolve_pane(&app.snap, &target)?;
        if let Some((s, t)) = app.locate(pane.id) {
            app.set_active(s, t, pane.id).await;
        }
    }

    enable_raw_mode()?;
    if mouse {
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    } else {
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    }
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        hook(info);
    }));

    app.sync().await;

    let (in_tx, mut in_rx) = unbounded_channel::<Event>();
    std::thread::spawn(move || loop {
        match crossterm::event::poll(Duration::from_millis(50)) {
            Ok(true) => match crossterm::event::read() {
                Ok(ev) => {
                    if in_tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {
                if in_tx.is_closed() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(spinner_ms));
    while app.running {
        terminal.draw(|f| app.draw(f))?;
        tokio::select! {
            ev = in_rx.recv() => match ev {
                Some(e) => app.on_term_event(e).await,
                None => app.running = false,
            },
            msg = events.recv() => match msg {
                Some(m) => app.on_server(m).await,
                None => {
                    app.running = false;
                    eprintln!("daemon connection lost");
                }
            },
            _ = ticker.tick() => app.on_tick(),
        }
        while let Ok(e) = in_rx.try_recv() {
            app.on_term_event(e).await;
        }
        while let Ok(m) = events.try_recv() {
            app.on_server(m).await;
        }
    }

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn templates_expand_tokens() {
        let icon = ("◉".to_string(), Color::Yellow);
        let vars = vec![("title", "claude·3".to_string()), ("cwd", "/tmp".to_string())];
        let spans = template_spans("{icon} {title} · {cwd}", &icon, &vars, Style::default(), Color::White);
        assert_eq!(text_of(&spans), "◉ claude·3 · /tmp");
        // unknown tokens render literally rather than vanishing
        let spans = template_spans("{icon} {nope}", &icon, &vars, Style::default(), Color::White);
        assert_eq!(text_of(&spans), "◉ {nope}");
    }

    #[test]
    fn zoom_ignores_missing_pane() {
        // compute_rects with zoom on a pane not in the tab falls back to layout
        // (covered indirectly: Node::contains gate). Just exercise node_rects gutters.
        let layout = Node::Split {
            dir: Dir::Right,
            children: vec![Node::Leaf { pane: 1 }, Node::Leaf { pane: 2 }],
            weights: vec![],
        };
        let mut out = Vec::new();
        node_rects(&layout, Rect::new(0, 0, 81, 20), 1, &mut out);
        assert_eq!(out.len(), 2);
        // gutter cell exists between the chunks
        assert!(out[1].1.x > out[0].1.x + out[0].1.width);
    }
}
