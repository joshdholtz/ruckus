use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::client::{connect, ensure_daemon, resolve_pane, Client};
use crate::config::{
    normalize_key, Action, BarPos, CommandBind, Config, FooterMode, LinkClick, Placement,
    SidebarPos, ToastPos, WorkingStyle,
};
use crate::layout::{
    area_at_path, find_border, node_at_path_mut, node_dividers, node_rects, split_chunks,
};
use crate::protocol::*;
use crate::remote;
use crate::render::{encode_key, encode_mouse_wheel, screen_to_lines};

/// Below this width the footer switches to compact tap-first chips.
const FOOTER_COMPACT: u16 = 70;
/// How long a sidebar/title row stays flashed after an activity change.
const FLASH_MS: u128 = 900;

/// Every action the command palette can run, with a human label.
const PALETTE_ITEMS: &[(Action, &str)] = &[
    (Action::JumpWaiting, "jump to next pane that needs you"),
    (Action::SplitRight, "split pane right"),
    (Action::SplitDown, "split pane down"),
    (Action::NewTab, "new tab"),
    (Action::NewSpace, "new space"),
    (Action::ClosePane, "close pane"),
    (Action::Zoom, "zoom focused pane"),
    (Action::Search, "search scrollback"),
    (Action::NextPane, "focus next pane"),
    (Action::PrevPane, "focus previous pane"),
    (Action::NextTab, "next tab"),
    (Action::PrevTab, "previous tab"),
    (Action::NextSpace, "next space"),
    (Action::PrevSpace, "previous space"),
    (Action::LastPane, "last pane (jump back)"),
    (Action::LastSpace, "last space (jump back)"),
    (
        Action::ConnectRemote,
        "connect remote (mirror a box over SSH)",
    ),
    (
        Action::DisconnectRemote,
        "disconnect remote (the space you're on)",
    ),
    (Action::ToggleSidebar, "toggle sidebar"),
    (Action::ShowHelp, "keyboard help"),
    (Action::Quit, "quit (daemon keeps running)"),
];

/// Subsequence fuzzy match: a score (rewards early + contiguous matches) plus
/// the char indices in `cand` that matched, for highlighting. `None` if `q`
/// isn't a subsequence of `cand`.
fn fuzzy_indices(cand: &str, q: &str) -> Option<(i32, Vec<usize>)> {
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    let cand: Vec<char> = cand.chars().collect();
    let mut ci = 0usize;
    let mut score = 0i32;
    let mut prev: Option<usize> = None;
    let mut hits = Vec::new();
    for qc in q.chars() {
        let mut matched = false;
        while ci < cand.len() {
            if cand[ci].eq_ignore_ascii_case(&qc) {
                score += if prev == Some(ci.wrapping_sub(1)) {
                    3
                } else {
                    1
                };
                if ci == 0 {
                    score += 2;
                }
                prev = Some(ci);
                hits.push(ci);
                ci += 1;
                matched = true;
                break;
            }
            ci += 1;
        }
        if !matched {
            return None;
        }
    }
    Some((score, hits))
}

/// Split `label` into styled spans, accenting the matched (`hl`) chars.
fn hl_spans(
    label: &str,
    hl: &[usize],
    base: Style,
    normal_fg: Color,
    match_fg: Color,
    bold: bool,
) -> Vec<Span<'static>> {
    let hlset: HashSet<usize> = hl.iter().copied().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_match = false;
    let flush = |spans: &mut Vec<Span<'static>>, text: &mut String, is_match: bool| {
        if text.is_empty() {
            return;
        }
        let mut st = base.fg(if is_match { match_fg } else { normal_fg });
        if bold || is_match {
            st = st.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(std::mem::take(text), st));
    };
    for (i, ch) in label.chars().enumerate() {
        let m = hlset.contains(&i);
        if !cur.is_empty() && m != cur_match {
            flush(&mut spans, &mut cur, cur_match);
        }
        cur.push(ch);
        cur_match = m;
    }
    flush(&mut spans, &mut cur, cur_match);
    spans
}

/// Substitute a link handler's `run` template into argv: `${url}`/`${match}`/
/// `${0}` = the whole match, `${1}`.. = capture groups. Each whitespace token
/// becomes exactly one argv element (the matched text is never shell-split).
fn link_argv(run: &str, full: &str, groups: &[&str]) -> Vec<String> {
    run.split_whitespace()
        .map(|tok| {
            let mut t = tok
                .replace("${url}", full)
                .replace("${match}", full)
                .replace("${0}", full);
            for (i, g) in groups.iter().enumerate() {
                t = t.replace(&format!("${{{}}}", i + 1), g);
            }
            t
        })
        .collect()
}

/// A pane's display name: its title, else its command, else a fallback.
fn pane_name(p: Option<&PaneInfo>) -> String {
    match p {
        Some(p) if !p.title.is_empty() => p.title.clone(),
        Some(p) if !p.cmd.is_empty() => p.cmd.join(" "),
        Some(p) => format!("pane {}", p.id),
        None => "pane".to_string(),
    }
}

/// Linear blend between two colors (RGB only; non-RGB returns `b`).
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => b,
    }
}

struct PaneView {
    parser: vt100::Parser,
    scroll: usize,
    rows: u16,
    cols: u16,
}

/// A client-local floating command (display-popup style): its own PTY, rendered
/// as a centered overlay that captures all keys until the command exits.
struct Popup {
    parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    title: String,
    rows: u16,
    cols: u16,
}

/// Spawn a command in a fresh PTY, streaming its output to `tx`. Ephemeral —
/// unlike daemon panes, a popup dies with the client.
fn spawn_popup(
    cmd: Vec<String>,
    cwd: String,
    rows: u16,
    cols: u16,
    tx: UnboundedSender<Vec<u8>>,
) -> anyhow::Result<Popup> {
    use anyhow::anyhow;
    use std::io::Read;
    let cmdline = if cmd.is_empty() {
        vec![crate::protocol::default_shell()]
    } else {
        cmd
    };
    let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut builder = portable_pty::CommandBuilder::new(&cmdline[0]);
    builder.args(&cmdline[1..]);
    builder.env("TERM", "xterm-256color");
    builder.env(
        "RUCKUS_SOCK",
        crate::protocol::socket_path().display().to_string(),
    );
    builder.env(
        "RUCKUS_DIR",
        crate::protocol::ruckus_dir().display().to_string(),
    );
    builder.cwd(&cwd);
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow!("spawn: {e}"))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(Vec::new()); // EOF sentinel → close the popup
    });
    Ok(Popup {
        parser: vt100::Parser::new(rows, cols, 0),
        master: pair.master,
        writer,
        child,
        title: cmdline.join(" "),
        rows,
        cols,
    })
}

#[derive(Clone, Copy)]
enum Target {
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
    Pane(u64),
}

/// The command palette is a choose-tree navigator: an indented Space → Tab →
/// Pane tree you fuzzy-filter and jump into. Typing `>` switches to a flat
/// command list. Navigation is arrows / Ctrl-n·p (typed keys feed the filter).
#[derive(Clone)]
struct Palette {
    query: String,
    /// Selected index into the current visible rows.
    sel: usize,
    /// Scroll offset (top visible row).
    scroll: usize,
    /// Space/tab ids whose children are folded away. Defaults collapse every
    /// space but the active one, and every tab (panes hidden until expanded).
    collapsed: HashSet<u64>,
}

impl Palette {
    fn new(snap: &Snapshot) -> Palette {
        let mut collapsed = HashSet::new();
        for s in &snap.spaces {
            if s.id != snap.active_space {
                collapsed.insert(s.id);
            }
            for t in &s.tabs {
                collapsed.insert(t.id);
            }
        }
        Palette {
            query: String::new(),
            sel: 0,
            scroll: 0,
            collapsed,
        }
    }
    /// `>`-prefixed query means the flat command list, not the tree.
    fn is_commands(&self) -> bool {
        self.query.starts_with('>')
    }
    /// The active filter text (without the `>` mode prefix), lowercased.
    fn filter(&self) -> String {
        self.query
            .strip_prefix('>')
            .unwrap_or(&self.query)
            .trim()
            .to_lowercase()
    }
}

/// What a visible palette row targets.
#[derive(Clone, Copy)]
enum PKind {
    Space(u64),
    Jump { space: u64, tab: u64, pane: u64 },
    Cmd(Action),
}

/// A rendered palette row, rebuilt from the snapshot + query each frame.
struct PRow {
    kind: PKind,
    depth: u8,
    glyph: (String, Color),
    label: String,
    meta: String,
    /// Char indices in `label` that matched the query (for highlighting).
    hl: Vec<usize>,
    expandable: bool,
    expanded: bool,
    /// Shown only as context for a matching descendant/ancestor — render muted.
    dim: bool,
}

/// The pure structural result of the tree navigator: which rows are visible,
/// their depth, fold state, and match highlights — no styling/time. Extracted
/// so the filter/fold logic is unit-testable independent of the App.
struct PTreeRow {
    kind: PKind,
    depth: u8,
    label: String,
    hl: Vec<usize>,
    expandable: bool,
    expanded: bool,
    dim: bool,
}

/// Build the Space → Tab → Pane navigator rows for `q` + `collapsed`. Panes
/// appear only under split tabs; while filtering we force-expand along matches
/// and dim rows shown purely as context. Pure over the snapshot.
fn palette_tree(snap: &Snapshot, q: &str, collapsed: &HashSet<u64>) -> Vec<PTreeRow> {
    let filtering = !q.is_empty();
    let mut rows = Vec::new();
    for s in &snap.spaces {
        let space_hl = fuzzy_indices(&s.name, q);
        let space_match = space_hl.is_some();
        struct TabData {
            idx: usize,
            hl: Option<Vec<usize>>,
            panes: Vec<(u64, String, Option<Vec<usize>>)>,
        }
        let mut tabs: Vec<TabData> = Vec::new();
        for (ti, t) in s.tabs.iter().enumerate() {
            let hl = fuzzy_indices(&t.name, q).map(|(_, h)| h);
            let mut leaves = Vec::new();
            t.layout.leaves(&mut leaves);
            let panes = if leaves.len() > 1 {
                leaves
                    .iter()
                    .map(|pid| {
                        let name = pane_name(snap.pane(*pid));
                        let m = fuzzy_indices(&name, q).map(|(_, h)| h);
                        (*pid, name, m)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            tabs.push(TabData { idx: ti, hl, panes });
        }
        let any_tab_match = tabs.iter().any(|t| t.hl.is_some());
        let any_pane_match = tabs
            .iter()
            .any(|t| t.panes.iter().any(|(_, _, m)| m.is_some()));
        if filtering && !space_match && !any_tab_match && !any_pane_match {
            continue;
        }
        let space_expanded = if filtering {
            true
        } else {
            !collapsed.contains(&s.id)
        };
        rows.push(PTreeRow {
            kind: PKind::Space(s.id),
            depth: 0,
            label: s.name.clone(),
            hl: space_hl
                .as_ref()
                .map(|(_, h)| h.clone())
                .unwrap_or_default(),
            expandable: !s.tabs.is_empty(),
            expanded: space_expanded,
            dim: filtering && space_hl.as_ref().map(|(_, h)| h.is_empty()).unwrap_or(true),
        });
        if !space_expanded {
            continue;
        }
        for tr in &tabs {
            let t = &s.tabs[tr.idx];
            let tab_match = tr.hl.is_some();
            let pane_match = tr.panes.iter().any(|(_, _, m)| m.is_some());
            if filtering && !space_match && !tab_match && !pane_match {
                continue;
            }
            let split = !tr.panes.is_empty();
            let tab_expanded = if filtering {
                pane_match
            } else {
                !collapsed.contains(&t.id)
            };
            rows.push(PTreeRow {
                kind: PKind::Jump {
                    space: s.id,
                    tab: t.id,
                    pane: t.active_pane,
                },
                depth: 1,
                label: t.name.clone(),
                hl: tr.hl.clone().unwrap_or_default(),
                expandable: split,
                expanded: tab_expanded,
                dim: filtering && !tab_match,
            });
            if !split || !tab_expanded {
                continue;
            }
            for (pid, name, m) in &tr.panes {
                if filtering && !space_match && !tab_match && m.is_none() {
                    continue;
                }
                rows.push(PTreeRow {
                    kind: PKind::Jump {
                        space: s.id,
                        tab: t.id,
                        pane: *pid,
                    },
                    depth: 2,
                    label: name.clone(),
                    hl: m.clone().unwrap_or_default(),
                    expandable: false,
                    expanded: false,
                    dim: filtering && m.is_none(),
                });
            }
        }
    }
    rows
}

/// A NEEDS-YOU sidebar row: (pane id, template vars, state glyph).
type AttnRow = (u64, Vec<(&'static str, String)>, (String, Color));

/// A clickable affordance rendered in the sidebar.
#[derive(Clone, Copy)]
enum SidebarBtn {
    CloseSpace(u64),
}

/// A tappable region in the mobile deck view.
#[derive(Clone, Copy)]
enum DeckHit {
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
    NewTab,
    NewSpace,
    Jump,
    ScrollUp,
    ScrollDown,
    Menu,
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
    Search,
    /// Free-text reply into the focused pane (mobile triage / soft keyboard).
    Reply,
    /// Host to mirror over SSH (runtime `connect remote`).
    ConnectRemote,
}

/// A modal single-line text input (naming a new/renamed space or tab).
struct Prompt {
    kind: PromptKind,
    label: &'static str,
    buffer: String,
}

/// Tap targets on the triage action bar (waiting / exited / narrow).
#[derive(Clone)]
enum ChipAction {
    /// Bytes to type into the focused pane (usually includes trailing newline).
    Send(Vec<u8>),
    Restart,
    Close,
    JumpWaiting,
    Zoom,
    Reply,
    ToggleSidebar,
    Palette,
    Back,
    Next,
    Search,
}

/// Where every chrome element lives this frame, derived from config + size.
#[derive(Clone, Copy, Default)]
struct FrameLayout {
    header: Option<u16>,
    footer: Option<u16>,
    /// Row of big tappable chips (approve / restart) above the footer.
    action: Option<u16>,
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
    /// The one connection to the LOCAL daemon. Remote mirrors live in the daemon
    /// now (the hybrid hub): it merges their spaces into the snapshot and routes
    /// requests by the id's origin, so the client sends origin-encoded ids as-is.
    client: Client,
    snap: Snapshot,
    views: HashMap<u64, PaneView>,
    focused: u64,
    /// Previously-focused pane / previously-active space, for "jump back".
    last_pane: Option<u64>,
    last_space: Option<u64>,
    seen: HashSet<u64>,
    /// Panes that changed to a notable state (finished / needs input) while
    /// unfocused. Cleared when you view the pane. Drives the "unread" badge.
    unread: HashSet<u64>,
    /// pane id -> when its activity last changed, for the brief row-flash motion.
    flash: HashMap<u64, Instant>,
    cwd: String,
    running: bool,
    toast: Option<(String, Instant)>,
    sidebar: bool,
    zoomed: bool,
    /// Last-frame narrow state — used to auto-zoom when the window shrinks.
    was_narrow: bool,
    drawer: bool,
    help: bool,
    prefix_pending: bool,
    menu: Option<Menu>,
    prompt: Option<Prompt>,
    /// Active scrollback search query (focused pane); drives n/N and highlight.
    search: Option<String>,
    /// Command palette / choose-tree navigator.
    palette: Option<Palette>,
    /// Clickable palette rows: (rect, visible-row index).
    palette_hits: Vec<(Rect, usize)>,
    /// Touch-first deck view (big cards) — the mobile home screen. Gated by narrow().
    deck: bool,
    deck_scroll: usize,
    /// Keyboard selection cursor over the deck's focusable items: 0..n = cards,
    /// then the +tab and +space buttons. Touch taps sync it too.
    deck_sel: usize,
    /// Tap regions in the deck: (rect, what it does).
    deck_hits: Vec<(Rect, DeckHit)>,
    drag: Option<Drag>,
    select: Option<Sel>,
    selecting: bool,
    /// When the last selection was copied — flashes the selection green briefly
    /// as confirmation (toasts are gone), then clears it.
    copied_at: Option<Instant>,
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
    /// Last reorder target sent during the active tab/space drag — dedups the
    /// per-mouse-move MoveTab/MoveSpace round-trips so a drag can't flood the socket.
    drag_target: Option<u64>,
    /// Alt+drag a pane onto another to swap them.
    swap_from: Option<u64>,
    /// Live sidebar width from dragging its edge; None = use the configured width.
    sidebar_w: Option<u16>,
    /// True while dragging the sidebar's edge to resize it.
    sidebar_resizing: bool,
    /// A floating command popup (display-popup style), if one is open.
    popup: Option<Popup>,
    /// Channel a popup's reader thread pushes output to.
    popup_tx: UnboundedSender<Vec<u8>>,
    footer_hits: Vec<(Action, std::ops::Range<u16>)>,
    /// Triage chips: (action, row, col range).
    action_hits: Vec<(ChipAction, u16, std::ops::Range<u16>)>,
    pane_rects: Vec<(u64, Rect)>,
    /// Cached output of `#(command)` status segments, refreshed on an interval.
    status_cmds: HashMap<String, String>,
}

/// Extract `#(command)` segments from a status format string.
fn extract_commands(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find("#(") {
        rest = &rest[i + 2..];
        if let Some(end) = rest.find(')') {
            out.push(rest[..end].to_string());
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// Short hostname (up to the first dot), for the {host} status token.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let full = String::from_utf8_lossy(&buf[..end]).to_string();
    full.split('.').next().unwrap_or(&full).to_string()
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

/// Compact location for a pane: last folder + git branch, e.g. "ruckus·main".
fn pane_loc(p: &PaneInfo) -> String {
    let rel = home_relative(&p.cwd);
    let folder = rel.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&rel);
    if p.git_branch.is_empty() {
        folder.to_string()
    } else {
        format!("{folder}·{}", p.git_branch)
    }
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
        // Take the pipe so it drops (closing stdin) once we've written — pbcopy
        // reads until EOF and only commits the clipboard then. Leaving it open
        // (the old `as_mut`) made pbcopy block on read forever and never copy.
        if let Some(mut si) = child.stdin.take() {
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
    agg_activity(
        leaves
            .iter()
            .filter_map(|p| snap.pane(*p))
            .map(|p| p.activity),
    )
}

fn space_activity(snap: &Snapshot, space: &SpaceInfo) -> Activity {
    agg_activity(space.tabs.iter().map(|t| tab_activity(snap, t)))
}

/// Lowercase one-word label for an activity state (headers, info bars).
fn activity_word(a: Activity) -> &'static str {
    match a {
        Activity::Waiting => "waiting",
        Activity::Working => "working",
        Activity::Done => "done",
        Activity::Idle => "idle",
    }
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
            spans.push(Span::styled(
                rest[..start].to_string(),
                row_style.fg(text_fg),
            ));
        }
        match rest[start..].find('}') {
            Some(endrel) => {
                let token = &rest[start + 1..start + endrel];
                if token == "icon" {
                    spans.push(Span::styled(icon.0.clone(), row_style.fg(icon.1)));
                } else if let Some((_, v)) = vars.iter().find(|(k, _)| *k == token) {
                    spans.push(Span::styled(v.clone(), row_style.fg(text_fg)));
                } else {
                    spans.push(Span::styled(format!("{{{token}}}"), row_style.fg(text_fg)));
                }
                rest = &rest[start + endrel + 1..];
            }
            None => {
                spans.push(Span::styled(
                    rest[start..].to_string(),
                    row_style.fg(text_fg),
                ));
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

    /// The deck (mobile card home) shows only when narrow and the toggle is on.
    fn deck_active(&self) -> bool {
        self.deck && self.narrow()
    }

    /// Viewing a single pane on mobile (entered from the deck): strip the tab
    /// strip / footer / pane-title chrome so the pane is near-fullscreen. Only
    /// when the deck is the home to return to (☰ goes back).
    fn mobile_focus(&self) -> bool {
        self.cfg.ui.deck && self.narrow() && !self.deck
    }

    /// Point the deck cursor at the active tab's card (when returning to the deck).
    fn sync_deck_sel(&mut self) {
        if let Some(sp) = self.active_space() {
            if let Some(i) = sp.tabs.iter().position(|t| t.id == sp.active_tab) {
                self.deck_sel = i;
            }
        }
    }

    /// Move the active space by `dir` (deck h/l) and reset the card cursor.
    async fn deck_switch_space(&mut self, dir: i32) {
        if self.snap.spaces.is_empty() {
            return;
        }
        let n = self.snap.spaces.len();
        let idx = self
            .snap
            .spaces
            .iter()
            .position(|s| s.id == self.snap.active_space)
            .unwrap_or(0);
        let ni = ((idx as i32 + dir).rem_euclid(n as i32)) as usize;
        let s = self.snap.spaces[ni].clone();
        if let Some(t) = s
            .tabs
            .iter()
            .find(|t| t.id == s.active_tab)
            .or(s.tabs.first())
        {
            self.deck_sel = 0;
            self.deck_scroll = 0;
            self.set_active(s.id, t.id, t.active_pane).await;
        }
    }

    /// Activate the deck item at `idx`: a card enters its pane; the trailing two
    /// indices are the +tab / +space buttons.
    async fn deck_activate(&mut self, idx: usize) {
        let Some(sp) = self.active_space() else {
            return;
        };
        let ncards = sp.tabs.len();
        if idx < ncards {
            let t = &sp.tabs[idx];
            let (s, tab, pane) = (sp.id, t.id, t.active_pane);
            self.set_active(s, tab, pane).await;
            self.deck = false;
            self.sync().await;
        } else if idx == ncards {
            self.open_prompt(PromptKind::NewTab);
        } else {
            self.open_prompt(PromptKind::NewSpace);
        }
    }

    fn sidebar_shown(&self) -> bool {
        self.sidebar && !self.narrow()
    }

    /// Focused pane wants a touch action/triage bar. This is a phone affordance
    /// only — on desktop you drive a waiting/exited pane straight from the
    /// keyboard (type into it, Enter restarts a dead pane), so no bottom bar.
    fn action_bar_kind(&self) -> Option<&'static str> {
        if !self.narrow() {
            return None;
        }
        let p = self.snap.pane(self.focused)?;
        match p.status {
            PaneStatus::Exited { .. } => Some("exited"),
            PaneStatus::Running if p.activity == Activity::Waiting => Some("waiting"),
            // Narrow idle/working still gets a slim nav strip for phone thumbs.
            PaneStatus::Running => Some("narrow"),
        }
    }

    fn compute_frame(&self) -> FrameLayout {
        let (w, h) = self.size;
        let ui = &self.cfg.ui;
        // On mobile, a pane entered from the deck is near-fullscreen: no tab
        // strip, no footer status bar (the deck is the nav; ☰ goes home).
        let minimal = self.mobile_focus();
        let mut top: u16 = ui.top_margin.min(h.saturating_sub(2));
        let mut bot: u16 = h;
        let mut header = None;
        let mut footer = None;
        let mut action = None;
        if ui.header == BarPos::Top {
            header = Some(top);
            top += 1;
        }
        if ui.footer == BarPos::Top && !minimal {
            footer = Some(top);
            top += 1;
        }
        if ui.footer == BarPos::Bottom && bot > top && !minimal {
            bot -= 1;
            footer = Some(bot);
        }
        // Action/triage bar sits just above the footer (or at the bottom if no footer).
        if self.action_bar_kind().is_some() && bot > top {
            bot -= 1;
            action = Some(bot);
        }
        if ui.header == BarPos::Bottom && bot > top {
            bot -= 1;
            header = Some(bot);
        }
        let body = Rect::new(0, top, w, bot.saturating_sub(top));
        let shown = self.sidebar_shown();
        let sw = if shown {
            self.sidebar_width().min(w.saturating_sub(20))
        } else {
            0
        };
        let (sidebar, main_x, main_w) = if shown {
            match ui.sidebar_pos {
                SidebarPos::Left => (
                    Some(Rect::new(0, body.y, sw, body.height)),
                    sw,
                    w.saturating_sub(sw),
                ),
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
        let (tabs, panes) = if ui.tab_strip && main.height > 1 && !minimal {
            // Reserve one extra row under the tab strip for a divider line.
            let border = if ui.tab_border && main.height > 2 {
                1
            } else {
                0
            };
            (
                Some(main.y),
                Rect::new(
                    main.x,
                    main.y + 1 + border,
                    main.width,
                    main.height - 1 - border,
                ),
            )
        } else {
            (None, main)
        };
        FrameLayout {
            header,
            footer,
            action,
            tabs,
            body,
            sidebar,
            main,
            panes,
        }
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

    fn toast(&mut self, _msg: impl Into<String>) {
        // Automatic toasts stay silent — the unsolicited popups (errors,
        // reconnecting, activity) were more annoying than useful; pane/deck state
        // colors already show what's happening. User-initiated confirmations go
        // through `notify` instead (see `copy_selection`).
    }

    /// A popup for something the user just did on purpose (e.g. copy) — worth an
    /// explicit confirmation, unlike the automatic toasts above.
    fn notify(&mut self, msg: impl Into<String>) {
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
        let rect = self
            .pane_rects
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)?;
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
        let Some(view) = self.views.get(&sel.pane) else {
            return;
        };
        let screen = view.parser.screen();
        let (rows, cols) = screen.size();
        // Clamp every coordinate to the grid: a client/daemon size desync can put
        // the selection outside the pane's vt100 screen, and contents_between does
        // an unguarded `cols - start_col` that would underflow-panic in debug.
        let last_row = rows.saturating_sub(1);
        let (sr, er) = (sr.min(last_row), er.min(last_row));
        let sc = sc.min(cols);
        let ec2 = ec.saturating_add(1).min(cols).max(sc); // include the cell under the cursor
        let text = screen.contents_between(sr, sc, er, ec2);
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        let n = text.chars().count();
        let lines = text.lines().count().max(1);
        copy_to_clipboard(&text);
        self.copied_at = Some(Instant::now()); // flash the selection green
        self.notify(if lines > 1 {
            format!("✓ copied {n} chars · {lines} lines")
        } else {
            format!("✓ copied {n} chars")
        });
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
        // Phone / narrow: auto-zoom so triage is one pane full-bleed. Only force
        // when *entering* narrow so the user can still un-zoom if they want.
        let n = self.narrow();
        if n && !self.was_narrow {
            self.zoomed = true;
        }
        self.was_narrow = n;
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
            let _ = self.route(Request::Detach { pane: p }).await;
        }

        for (pane, rect) in rects {
            let (rows, cols) = self.pane_size(rect);
            if !self.views.contains_key(&pane) {
                match self.route(Request::Attach { pane, rows, cols }).await {
                    Ok(ServerMsg::Attached { scrollback, .. }) => {
                        let mut parser = vt100::Parser::new(rows, cols, 10_000);
                        if let Ok(bytes) = B64.decode(scrollback.as_bytes()) {
                            parser.process(&bytes);
                        }
                        self.views.insert(
                            pane,
                            PaneView {
                                parser,
                                scroll: 0,
                                rows,
                                cols,
                            },
                        );
                    }
                    Err(e) => self.toast(e.to_string()),
                    _ => {}
                }
            } else if let Some(v) = self.views.get_mut(&pane) {
                if v.rows != rows || v.cols != cols {
                    v.rows = rows;
                    v.cols = cols;
                    v.parser.set_size(rows, cols);
                    let _ = self.route(Request::Resize { pane, rows, cols }).await;
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

    /// Send a request to the daemon that owns its ids (routing by origin), and
    /// re-prefix the reply's ids back to global form. The client's single point
    /// of contact with all daemons.
    /// The host label for an id's daemon ("" = local). Remote hosts come from the
    /// merged snapshot the daemon sends (it owns the connections now).
    fn host_of(&self, id: u64) -> &str {
        let origin = remote::origin_of(id);
        if origin == remote::LOCAL {
            return "";
        }
        self.snap
            .remote_hosts
            .get(&origin)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Send a request to the local daemon. Ids are origin-encoded; the daemon
    /// routes remote ones to the owning daemon and re-prefixes the reply, so the
    /// client neither strips nor re-tags anything.
    async fn route(&self, req: Request) -> anyhow::Result<ServerMsg> {
        self.client.request(req).await
    }

    /// The client's live SSH env, handed to the daemon so its detached `ssh` can
    /// authenticate as the user (agent auth / hardware-key touch are agent-side).
    fn ssh_env() -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::new();
        for k in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
            if let Ok(v) = std::env::var(k) {
                env.insert(k.to_string(), v);
            }
        }
        env
    }

    /// Ask the daemon to connect a remote (runtime or config), passing live SSH env.
    async fn connect_remote(&mut self, host: String, args: Vec<String>) {
        self.notify(format!("connecting {host}…"));
        let req = Request::ConnectRemote {
            host,
            args,
            env: Self::ssh_env(),
        };
        if let Err(e) = self.route(req).await {
            self.notify(format!("connect failed: {e}"));
        }
    }

    async fn set_active(&mut self, space: u64, tab: u64, pane: u64) {
        // Remember where we were so "last pane / last space" can jump back.
        if pane != self.focused {
            self.last_pane = Some(self.focused);
        }
        if space != self.snap.active_space {
            self.last_space = Some(self.snap.active_space);
        }
        self.snap.active_space = space;
        if let Some(s) = self.snap.spaces.iter_mut().find(|s| s.id == space) {
            s.active_tab = tab;
            if let Some(t) = s.tabs.iter_mut().find(|t| t.id == tab) {
                t.active_pane = pane;
            }
        }
        self.focused = pane;
        self.mark_seen();
        let _ = self.route(Request::SetActive { space, tab, pane }).await;
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

    /// Sidebar width in cells — the live dragged width if any, else configured.
    fn sidebar_width(&self) -> u16 {
        self.sidebar_w.unwrap_or(self.cfg.ui.sidebar_width)
    }

    /// Resize the sidebar so its edge follows the cursor column, then refit panes.
    async fn resize_sidebar(&mut self, col: u16) {
        let w = self.size.0;
        let new = match self.cfg.ui.sidebar_pos {
            SidebarPos::Left => col,
            SidebarPos::Right => w.saturating_sub(col),
        };
        let new = new.clamp(12, w.saturating_sub(24).max(12));
        if self.sidebar_w != Some(new) {
            self.sidebar_w = Some(new);
            self.sync().await; // panes refit to the new main area
        }
    }

    /// Persist the dragged sidebar width to config.toml so it survives relaunch
    /// (comment-preserving, like `ruckus config`).
    fn persist_sidebar_width(&self) {
        let Some(w) = self.sidebar_w else { return };
        let path = crate::config::ensure_config_file();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
            return;
        };
        if !doc.contains_key("ui") {
            doc["ui"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        doc["ui"]["sidebar_width"] = toml_edit::value(w as i64);
        let _ = std::fs::write(&path, doc.to_string());
    }

    fn drawer_width(&self) -> u16 {
        self.sidebar_width().min(self.size.0.saturating_sub(4))
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
        const SHELLS: &[&str] = &[
            "zsh", "bash", "sh", "fish", "dash", "tcsh", "ksh", "nu", "pwsh",
        ];
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
            let a = self
                .snap
                .pane(*pid)
                .map(|p| p.activity)
                .unwrap_or(Activity::Idle);
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

    /// The cwd to seed a new pane with: the reference pane's working directory
    /// (so a split/new tab opens where you already are), else ruckus's launch dir.
    fn seed_cwd(&self, from: u64) -> String {
        self.snap
            .pane(from)
            .map(|p| p.cwd.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// If a link handler matches the text at pane cell (row,col), return the
    /// matched text, the resolved argv, and its placement. `${url}`/`${match}`/
    /// `${0}` are the whole match; `${1}`.. are regex capture groups — each
    /// substituted as a SINGLE argv element (never shell-interpreted).
    fn link_at(
        &self,
        pane: u64,
        row: u16,
        col: u16,
    ) -> Option<(String, Vec<String>, Option<Placement>)> {
        let screen = self.views.get(&pane)?.parser.screen();
        // OSC 8 hyperlink stamped on the cell itself: the real URL, even when the
        // visible label isn't one (e.g. claude's links). Opens via the default.
        if let Some(url) = screen.cell(row, col).and_then(|c| c.hyperlink()) {
            let url = url.to_string();
            return Some((url.clone(), vec!["open".to_string(), url], None));
        }
        let contents = screen.contents();
        let line = contents.lines().nth(row as usize)?;
        for rule in &self.cfg.links {
            for caps in rule.pattern.captures_iter(line) {
                let Some(m) = caps.get(0) else { continue };
                let start = line[..m.start()].chars().count();
                let end = start + m.as_str().chars().count();
                if (col as usize) >= start && (col as usize) < end {
                    let full = m.as_str();
                    let groups: Vec<&str> = (1..caps.len())
                        .map(|i| caps.get(i).map_or("", |g| g.as_str()))
                        .collect();
                    let argv = link_argv(&rule.run, full, &groups);
                    return Some((full.to_string(), argv, rule.placement));
                }
            }
        }
        None
    }

    /// Run a link handler's resolved argv detached (e.g. `open <url>`).
    fn run_link(&mut self, argv: &[String], matched: &str) {
        let Some((prog, args)) = argv.split_first() else {
            return;
        };
        let _ = std::process::Command::new(prog)
            .args(args)
            .current_dir(self.seed_cwd(self.focused)) // so `gh` etc. see the right repo
            .env("RUCKUS_SOCK", crate::protocol::socket_path())
            .env("RUCKUS_DIR", crate::protocol::ruckus_dir())
            .env("RUCKUS_MATCH", matched)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.notify(format!("↗ {matched}"));
    }

    async fn run_command_bind(&mut self, cb: CommandBind) {
        self.open_placement(cb.cmd, cb.placement).await;
    }

    /// Open `cmd` in a split / tab / popup, seeded with the focused pane's cwd.
    async fn open_placement(&mut self, cmd: Vec<String>, placement: Placement) {
        if placement == Placement::Popup {
            self.open_popup(cmd);
            return;
        }
        let cwd = Some(self.seed_cwd(self.focused));
        let req = match placement {
            Placement::SplitRight => Request::Split {
                pane: self.focused,
                dir: Dir::Right,
                cmd,
                cwd,
            },
            Placement::SplitDown => Request::Split {
                pane: self.focused,
                dir: Dir::Down,
                cmd,
                cwd,
            },
            Placement::Tab => {
                let Some(space) = self.active_space().map(|s| s.id) else {
                    return;
                };
                Request::NewTab {
                    space,
                    name: None,
                    cmd,
                    cwd,
                }
            }
            Placement::Popup => return, // handled above
        };
        match self.route(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    /// Centered ~5/6-screen rect for a floating popup.
    fn popup_rect(&self) -> Rect {
        let (w0, h0) = self.size;
        let w = ((w0 as u32 * 5 / 6) as u16).clamp(20, w0);
        let h = ((h0 as u32 * 5 / 6) as u16).clamp(6, h0);
        Rect::new(w0.saturating_sub(w) / 2, h0.saturating_sub(h) / 2, w, h)
    }

    /// Open a command in a floating popup (one at a time).
    fn open_popup(&mut self, cmd: Vec<String>) {
        if self.popup.is_some() {
            return;
        }
        let r = self.popup_rect();
        let cols = r.width.saturating_sub(2).max(1);
        let rows = r.height.saturating_sub(2).max(1);
        let cwd = self.seed_cwd(self.focused);
        match spawn_popup(cmd, cwd, rows, cols, self.popup_tx.clone()) {
            Ok(p) => self.popup = Some(p),
            Err(e) => self.notify(format!("popup failed: {e}")),
        }
    }

    fn close_popup(&mut self) {
        if let Some(mut p) = self.popup.take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    async fn split_action(&mut self, pane: u64, dir: Dir) {
        let cwd = Some(self.seed_cwd(pane));
        let req = Request::Split {
            pane,
            dir,
            cmd: Vec::new(),
            cwd,
        };
        match self.route(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    async fn new_tab_action(&mut self, name: Option<String>) {
        let Some(space) = self.active_space().map(|s| s.id) else {
            return;
        };
        let cwd = Some(self.seed_cwd(self.focused));
        let req = Request::NewTab {
            space,
            name,
            cmd: Vec::new(),
            cwd,
        };
        match self.route(req).await {
            Ok(ServerMsg::Created { space, tab, pane }) => self.set_active(space, tab, pane).await,
            Err(e) => self.toast(e.to_string()),
            _ => {}
        }
    }

    async fn new_space_action(&mut self, name: Option<String>) {
        let req = Request::NewSpace {
            name,
            cwd: Some(self.seed_cwd(self.focused)),
        };
        match self.route(req).await {
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
            PromptKind::Reply => ("type a reply (enter to send)", String::new()),
            PromptKind::RenameSpace(id) => (
                "rename space",
                self.snap
                    .spaces
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
            ),
            PromptKind::RenameTab(id) => (
                "rename tab",
                self.snap
                    .spaces
                    .iter()
                    .flat_map(|s| &s.tabs)
                    .find(|t| t.id == id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default(),
            ),
            PromptKind::Search => ("search scrollback", String::new()),
            PromptKind::ConnectRemote => ("mirror host (ssh alias / user@host)", String::new()),
        };
        self.prompt = Some(Prompt {
            kind,
            label,
            buffer,
        });
    }

    /// Scan the focused pane's scrollback for the active query and scroll to a
    /// match. `older` searches back into history, else toward the live tail.
    fn run_search(&mut self, older: bool) {
        let Some(q) = self.search.clone() else { return };
        if q.is_empty() {
            return;
        }
        let ql = q.to_lowercase();
        let (found, missed) = {
            let Some(v) = self.views.get_mut(&self.focused) else {
                return;
            };
            let step = (v.rows.max(1) as usize).max(1);
            let max_off = 10_000usize;
            let mut off = v.scroll;
            let mut hit = None;
            for _ in 0..(max_off / step + 2) {
                off = if older {
                    (off + step).min(max_off)
                } else {
                    off.saturating_sub(step)
                };
                v.parser.set_scrollback(off);
                if v.parser.screen().contents().to_lowercase().contains(&ql) {
                    hit = Some(off);
                    break;
                }
                if (older && off == max_off) || (!older && off == 0) {
                    break;
                }
            }
            match hit {
                Some(o) => {
                    v.scroll = o;
                    v.parser.set_scrollback(o);
                    (true, false)
                }
                None => {
                    v.parser.set_scrollback(v.scroll); // restore view
                    (false, true)
                }
            }
        };
        if missed && !found {
            self.toast(format!("no match for “{q}”"));
        }
    }

    /// Commit the active prompt: create or rename with the typed name.
    async fn submit_prompt(&mut self) {
        let Some(p) = self.prompt.take() else { return };
        let name = p.buffer.trim().to_string();
        let opt = if name.is_empty() {
            None
        } else {
            Some(name.clone())
        };
        match p.kind {
            PromptKind::NewTab => self.new_tab_action(opt).await,
            PromptKind::NewSpace => self.new_space_action(opt).await,
            PromptKind::RenameSpace(space) => {
                if let Some(name) = opt {
                    if let Err(e) = self.route(Request::RenameSpace { space, name }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            PromptKind::RenameTab(tab) => {
                if let Some(name) = opt {
                    if let Err(e) = self.route(Request::RenameTab { tab, name }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            PromptKind::Search => {
                if name.is_empty() {
                    self.search = None;
                } else {
                    self.search = Some(name);
                    self.run_search(true);
                }
            }
            PromptKind::Reply => {
                if !name.is_empty() {
                    let mut bytes = name.into_bytes();
                    bytes.push(b'\n');
                    self.send_bytes(&bytes).await;
                }
            }
            PromptKind::ConnectRemote => {
                if !name.is_empty() {
                    self.connect_remote(name, Vec::new()).await;
                }
            }
        }
    }

    async fn send_bytes(&mut self, bytes: &[u8]) {
        if let Some(v) = self.views.get_mut(&self.focused) {
            if v.scroll != 0 {
                v.scroll = 0;
                v.parser.set_scrollback(0);
            }
        }
        let req = Request::Input {
            pane: self.focused,
            data: B64.encode(bytes),
        };
        if let Err(e) = self.route(req).await {
            self.toast(e.to_string());
        }
    }

    async fn run_chip(&mut self, chip: ChipAction) {
        match chip {
            ChipAction::Send(bytes) => self.send_bytes(&bytes).await,
            ChipAction::Restart => self.restart_action(self.focused).await,
            ChipAction::Close => self.close_pane_action(self.focused).await,
            ChipAction::JumpWaiting => self.do_action(Action::JumpWaiting).await,
            ChipAction::Zoom => self.do_action(Action::Zoom).await,
            ChipAction::Reply => self.open_prompt(PromptKind::Reply),
            ChipAction::ToggleSidebar => self.do_action(Action::ToggleSidebar).await,
            ChipAction::Palette => self.do_action(Action::Palette).await,
            ChipAction::Back => {
                if self.cfg.ui.deck {
                    self.deck = true;
                    self.sync_deck_sel();
                    self.sync().await;
                }
            }
            ChipAction::Next => self.do_action(Action::NextTab).await,
            ChipAction::Search => self.do_action(Action::Search).await,
        }
    }

    async fn close_pane_action(&mut self, pane: u64) {
        if let Err(e) = self.route(Request::ClosePane { pane }).await {
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
                    None => self.toast("🐏 all quiet — nothing needs you"),
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
                let idx = s
                    .tabs
                    .iter()
                    .position(|t| t.id == s.active_tab)
                    .unwrap_or(0);
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
            Action::Search => self.open_prompt(PromptKind::Search),
            Action::Palette => self.open_palette(),
            Action::Deck => {
                self.deck = !self.deck;
                if self.deck {
                    self.sync_deck_sel();
                }
                self.sync().await;
            }
            Action::LastPane => {
                if let Some(p) = self.last_pane {
                    if self.snap.pane(p).is_some() {
                        self.goto_pane(p).await;
                    }
                }
            }
            Action::LastSpace => {
                if let Some(sid) = self.last_space {
                    let target = self.snap.spaces.iter().find(|s| s.id == sid).and_then(|s| {
                        s.tabs
                            .iter()
                            .find(|t| t.id == s.active_tab)
                            .or_else(|| s.tabs.first())
                            .map(|t| (s.id, t.id, t.active_pane))
                    });
                    if let Some((s, t, p)) = target {
                        self.set_active(s, t, p).await;
                    }
                }
            }
            Action::ConnectRemote => self.open_prompt(PromptKind::ConnectRemote),
            Action::DisconnectRemote => {
                let origin = remote::origin_of(self.snap.active_space);
                if origin == remote::LOCAL {
                    self.toast("that's the local daemon");
                } else {
                    let host = self.host_of(self.snap.active_space).to_string();
                    self.notify(format!("disconnecting {host}…"));
                    // The daemon kills the ssh + drops the mirror, then broadcasts
                    // a new snapshot; on_server refocuses us locally automatically.
                    let _ = self.route(Request::DisconnectRemote { origin }).await;
                }
            }
        }
    }

    fn open_palette(&mut self) {
        self.palette = Some(Palette::new(&self.snap));
    }

    /// Build the visible palette rows for the current query + fold state.
    fn palette_rows(&self) -> Vec<PRow> {
        let Some(pal) = &self.palette else {
            return Vec::new();
        };
        if pal.is_commands() {
            return self.palette_command_rows(&pal.filter());
        }
        self.palette_tree_rows(&pal.filter(), &pal.collapsed)
    }

    /// Flat command list (the `>` mode), fuzzy-ranked with match highlights.
    fn palette_command_rows(&self, q: &str) -> Vec<PRow> {
        let mut scored: Vec<(i32, usize, PRow)> = Vec::new();
        for (i, (a, d)) in PALETTE_ITEMS.iter().enumerate() {
            if let Some((score, hl)) = fuzzy_indices(d, q) {
                scored.push((
                    score,
                    i,
                    PRow {
                        kind: PKind::Cmd(*a),
                        depth: 0,
                        glyph: (String::new(), self.cfg.theme.accent),
                        label: d.to_string(),
                        meta: self.cfg.hint(*a),
                        hl,
                        expandable: false,
                        expanded: false,
                        dim: false,
                    },
                ));
            }
        }
        scored.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        scored.into_iter().map(|(_, _, r)| r).collect()
    }

    /// The Space → Tab → Pane tree (via `palette_tree`), each row enriched with
    /// its state glyph and metadata (tab counts / elapsed / folder·branch).
    fn palette_tree_rows(&self, q: &str, collapsed: &HashSet<u64>) -> Vec<PRow> {
        palette_tree(&self.snap, q, collapsed)
            .into_iter()
            .map(|r| {
                let (glyph, meta) = match (r.kind, r.depth) {
                    (PKind::Space(id), _) => {
                        let s = self.snap.spaces.iter().find(|s| s.id == id);
                        let g = self.static_glyph(
                            s.map(|s| space_activity(&self.snap, s))
                                .unwrap_or(Activity::Idle),
                        );
                        let n = s.map(|s| s.tabs.len()).unwrap_or(0);
                        let waiting = s
                            .map(|s| {
                                s.tabs
                                    .iter()
                                    .filter(|t| tab_activity(&self.snap, t) == Activity::Waiting)
                                    .count()
                            })
                            .unwrap_or(0);
                        let meta = if waiting > 0 {
                            format!("{n} tabs · ◉{waiting}")
                        } else {
                            format!("{n} tab{}", if n == 1 { "" } else { "s" })
                        };
                        (g, meta)
                    }
                    (PKind::Jump { tab, pane, .. }, 1) => {
                        let tabinfo = self
                            .snap
                            .spaces
                            .iter()
                            .flat_map(|s| &s.tabs)
                            .find(|t| t.id == tab);
                        let g = self.static_glyph(
                            tabinfo
                                .map(|t| tab_activity(&self.snap, t))
                                .unwrap_or(Activity::Idle),
                        );
                        let loc = self.snap.pane(pane).map(pane_loc).unwrap_or_default();
                        let el = self.elapsed_label(pane);
                        let meta = [el, loc]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("  ");
                        (g, meta)
                    }
                    (PKind::Jump { pane, .. }, _) => {
                        let p = self.snap.pane(pane);
                        (self.glyph(p), p.map(pane_loc).unwrap_or_default())
                    }
                    (PKind::Cmd(_), _) => ((String::new(), self.cfg.theme.accent), String::new()),
                };
                PRow {
                    kind: r.kind,
                    depth: r.depth,
                    glyph,
                    label: r.label,
                    meta,
                    hl: r.hl,
                    expandable: r.expandable,
                    expanded: r.expanded,
                    dim: r.dim,
                }
            })
            .collect()
    }

    /// Collapse (`collapse=true`) or expand the selected tree row; when there's
    /// nothing to fold, move to the parent / first child instead.
    fn palette_fold(&mut self, collapse: bool) {
        let rows = self.palette_rows();
        let Some(pal) = self.palette.as_mut() else {
            return;
        };
        let Some(row) = rows.get(pal.sel) else { return };
        let id = match row.kind {
            PKind::Space(id) => Some(id),
            PKind::Jump { tab, .. } if row.expandable => Some(tab),
            _ => None,
        };
        if collapse {
            if row.expandable && row.expanded {
                if let Some(id) = id {
                    pal.collapsed.insert(id);
                }
            } else if row.depth > 0 {
                for i in (0..pal.sel).rev() {
                    if rows[i].depth < row.depth {
                        pal.sel = i;
                        break;
                    }
                }
            }
        } else if row.expandable && !row.expanded {
            if let Some(id) = id {
                pal.collapsed.remove(&id);
            }
        } else if row.expandable && row.expanded && pal.sel + 1 < rows.len() {
            pal.sel += 1;
        }
    }

    async fn palette_run(&mut self) {
        let rows = self.palette_rows();
        let sel = self.palette.as_ref().map(|p| p.sel).unwrap_or(0);
        let kind = rows.get(sel).map(|r| r.kind);
        self.palette = None;
        let jump = match kind {
            Some(PKind::Cmd(a)) => {
                self.do_action(a).await;
                return;
            }
            Some(PKind::Space(id)) => self.snap.spaces.iter().find(|s| s.id == id).and_then(|s| {
                s.tabs
                    .iter()
                    .find(|t| t.id == s.active_tab)
                    .or_else(|| s.tabs.first())
                    .map(|t| (s.id, t.id, t.active_pane))
            }),
            Some(PKind::Jump { space, tab, pane }) => Some((space, tab, pane)),
            None => None,
        };
        if let Some((space, tab, pane)) = jump {
            self.set_active(space, tab, pane).await;
            self.deck = false; // if we jumped from the deck, enter the pane
            self.sync().await;
        }
    }

    async fn restart_action(&mut self, pane: u64) {
        match self.route(Request::Restart { pane }).await {
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
                    if let Err(e) = self.route(Request::CloseTab { tab: t }).await {
                        self.toast(e.to_string());
                    }
                }
            }
            MenuAction::CloseSpace => {
                if let Some(s) = space {
                    if let Err(e) = self.route(Request::CloseSpace { space: s }).await {
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
                        if let Err(e) = self.route(Request::MoveTab { tab: t, to }).await {
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
                        if let Err(e) = self.route(Request::MoveSpace { space: sp, to }).await {
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

        // A floating popup owns all input while open; ctrl-q force-closes it.
        if self.popup.is_some() {
            if ev.code == KeyCode::Char('q') && ev.modifiers.contains(KeyModifiers::CONTROL) {
                self.close_popup();
                return;
            }
            if let Some(bytes) = encode_key(&ev) {
                if let Some(p) = self.popup.as_mut() {
                    use std::io::Write;
                    let _ = p.writer.write_all(&bytes);
                    let _ = p.writer.flush();
                }
            }
            return;
        }

        // Deck view: keyboard selection cursor (vim + arrows) plus action bindings.
        if self.deck_active() && self.palette.is_none() && self.prompt.is_none() {
            let ncards = self.active_space().map(|s| s.tabs.len()).unwrap_or(0);
            let nitems = ncards + 2; // + tab, + space
            let bare = ev.modifiers.is_empty() || ev.modifiers == KeyModifiers::SHIFT;
            match ev.code {
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if bare => {
                    self.deck_sel = (self.deck_sel + 1).min(nitems.saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab if bare => {
                    self.deck_sel = self.deck_sel.saturating_sub(1);
                }
                KeyCode::Left | KeyCode::Char('h') if bare => {
                    self.deck_switch_space(-1).await;
                }
                KeyCode::Right | KeyCode::Char('l') if bare => {
                    self.deck_switch_space(1).await;
                }
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('o') if bare => {
                    self.deck_activate(self.deck_sel).await;
                }
                _ => {
                    if let Some(a) = self.cfg.action_for(&ev) {
                        self.do_action(a).await;
                    }
                }
            }
            return;
        }

        // Command palette / choose-tree. It's a live filter, so typed keys feed
        // the query — navigation is on the arrows (and Ctrl-n/p), fold on ←/→.
        if self.palette.is_some() {
            let len = self.palette_rows().len();
            let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
            let up = |p: &mut Palette| p.sel = p.sel.saturating_sub(1);
            let down = |p: &mut Palette| p.sel = (p.sel + 1).min(len.saturating_sub(1));
            match ev.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Enter => self.palette_run().await,
                KeyCode::Up | KeyCode::BackTab => {
                    self.palette.as_mut().map(up);
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.palette.as_mut().map(down);
                }
                KeyCode::Char('p') if ctrl => {
                    self.palette.as_mut().map(up);
                }
                KeyCode::Char('n') if ctrl => {
                    self.palette.as_mut().map(down);
                }
                KeyCode::Left => self.palette_fold(true),
                KeyCode::Right => self.palette_fold(false),
                KeyCode::Backspace => {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.pop();
                        p.sel = 0;
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.push(c);
                        p.sel = 0;
                    }
                }
                _ => {}
            }
            return;
        }

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

        // Search-nav: while a query is active, n/N cycle matches and Esc clears.
        // Only bare (or shift-only) keys, so modified bindings still reach the pane.
        // Skip it while the prefix is armed, or `prefix n`/`N` would run search-nav
        // instead of NextTab and leave the prefix stuck (the reset lives below).
        if self.search.is_some()
            && !self.prefix_pending
            && (ev.modifiers.is_empty() || ev.modifiers == KeyModifiers::SHIFT)
        {
            match ev.code {
                KeyCode::Esc => {
                    self.search = None;
                    return;
                }
                KeyCode::Char('n') => {
                    self.run_search(true);
                    return;
                }
                KeyCode::Char('N') => {
                    self.run_search(false);
                    return;
                }
                _ => {}
            }
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

        // tmux-style prefix: consume the prefix, then resolve the next key.
        if self.prefix_pending {
            self.prefix_pending = false;
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
            // tmux tab commands that aren't in the Action enum.
            match ev.code {
                KeyCode::Char(',') => {
                    if let Some(tab) = self.active_space().map(|s| s.active_tab) {
                        self.open_prompt(PromptKind::RenameTab(tab));
                    }
                    return;
                }
                KeyCode::Char('&') => {
                    if let Some(tab) = self.active_space().map(|s| s.active_tab) {
                        if let Err(e) = self.route(Request::CloseTab { tab }).await {
                            self.toast(e.to_string());
                        }
                    }
                    return;
                }
                _ => {}
            }
            if let Some(a) = self.cfg.prefix_action_for(&ev) {
                self.do_action(a).await;
            }
            return;
        }
        if self.cfg.is_prefix(&ev) {
            self.prefix_pending = true;
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
        // User command shortcuts win over built-in actions on the same key.
        if let Some(cb) = self
            .cfg
            .commands
            .iter()
            .find(|c| c.binding.matches(&ev))
            .cloned()
        {
            self.run_command_bind(cb).await;
            return;
        }
        if let Some(a) = self.cfg.action_for(&ev) {
            self.do_action(a).await;
            return;
        }
        // Dead-pane chrome: Enter restarts in place, Esc closes (zellij-style).
        let dead = self
            .snap
            .pane(self.focused)
            .map(|p| p.status != PaneStatus::Running)
            .unwrap_or(false);
        if dead {
            match ev.code {
                KeyCode::Enter => {
                    self.restart_action(self.focused).await;
                    return;
                }
                KeyCode::Esc => {
                    self.close_pane_action(self.focused).await;
                    return;
                }
                _ => {}
            }
        }
        if let Some(bytes) = encode_key(&ev) {
            self.search = None; // typing into the pane leaves search mode
            if let Some(v) = self.views.get_mut(&self.focused) {
                if v.scroll != 0 {
                    v.scroll = 0;
                    v.parser.set_scrollback(0);
                }
            }
            let req = Request::Input {
                pane: self.focused,
                data: B64.encode(&bytes),
            };
            if let Err(e) = self.route(req).await {
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
        let w = (m
            .items
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(10)
            + 4) as u16;
        let h = m.items.len() as u16 + 2;
        let x = m.x.min(self.size.0.saturating_sub(w + 1));
        let y = m.y.min(self.size.1.saturating_sub(h + 1));
        Rect::new(x, y, w, h)
    }

    fn border_hit(&self, col: u16, row: u16) -> Option<(Vec<usize>, usize, Dir)> {
        let t = self.active_tab()?;
        let mut path = Vec::new();
        find_border(
            &t.layout,
            self.frame.panes,
            self.cfg.ui.gutter,
            col,
            row,
            &mut path,
        )
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
        let Some(tab) = space.tabs.iter_mut().find(|t| t.id == tab_id) else {
            return;
        };
        let Some(split_area) = area_at_path(&tab.layout, panes_area, gutter, &path) else {
            return;
        };
        let Some(Node::Split {
            dir: d,
            children,
            weights,
        }) = node_at_path_mut(&mut tab.layout, &path)
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
            if let Err(e) = self
                .route(Request::SetLayout {
                    tab: drag.tab,
                    layout,
                })
                .await
            {
                self.toast(e.to_string());
            }
        }
        self.sync().await;
    }

    async fn on_mouse(&mut self, ev: MouseEvent) {
        let (col, row) = (ev.column, ev.row);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        // A popup owns the screen — ignore mouse on the panes behind it.
        if self.popup.is_some() {
            return;
        }
        // Palette overlay owns the mouse while open: wheel moves the cursor, a
        // click on a row selects and activates it (jump / run / enter a space).
        if self.palette.is_some() {
            match ev.kind {
                MouseEventKind::Moved => self.hover = Some((col, row)),
                MouseEventKind::ScrollDown => {
                    if let Some(p) = self.palette.as_mut() {
                        p.sel = p.sel.saturating_add(3);
                    }
                }
                MouseEventKind::ScrollUp => {
                    if let Some(p) = self.palette.as_mut() {
                        p.sel = p.sel.saturating_sub(3);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let hit = self
                        .palette_hits
                        .iter()
                        .find(|(r, _)| {
                            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
                        })
                        .map(|(_, i)| *i);
                    if let Some(idx) = hit {
                        if let Some(p) = self.palette.as_mut() {
                            p.sel = idx;
                        }
                        self.palette_run().await;
                    }
                }
                _ => {}
            }
            return;
        }
        // Deck view has its own tap/scroll model.
        if self.deck_active() {
            match ev.kind {
                MouseEventKind::Moved => self.hover = Some((col, row)),
                MouseEventKind::ScrollDown => self.deck_scroll = self.deck_scroll.saturating_add(1),
                MouseEventKind::ScrollUp => self.deck_scroll = self.deck_scroll.saturating_sub(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    let hit = self
                        .deck_hits
                        .iter()
                        .find(|(r, _)| {
                            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
                        })
                        .map(|(_, h)| *h);
                    match hit {
                        Some(DeckHit::Space(id)) => {
                            let t = self.snap.spaces.iter().find(|s| s.id == id).and_then(|s| {
                                s.tabs
                                    .iter()
                                    .find(|t| t.id == s.active_tab)
                                    .or(s.tabs.first())
                                    .map(|t| (s.id, t.id, t.active_pane))
                            });
                            if let Some((s, t, p)) = t {
                                self.deck_sel = 0;
                                self.deck_scroll = 0;
                                self.set_active(s, t, p).await;
                            }
                        }
                        Some(DeckHit::Tab { space, tab, pane }) => {
                            self.set_active(space, tab, pane).await;
                            self.deck = false; // enter the pane fullscreen
                            self.sync().await;
                        }
                        Some(DeckHit::NewTab) => self.open_prompt(PromptKind::NewTab),
                        Some(DeckHit::NewSpace) => self.open_prompt(PromptKind::NewSpace),
                        Some(DeckHit::Jump) => {
                            if let Some(p) = self.attention().first().map(|p| p.id) {
                                self.goto_pane(p).await;
                                self.deck = false;
                                self.sync().await;
                            }
                        }
                        Some(DeckHit::ScrollUp) => {
                            self.deck_scroll = self.deck_scroll.saturating_sub(3)
                        }
                        Some(DeckHit::ScrollDown) => {
                            self.deck_scroll = self.deck_scroll.saturating_add(3)
                        }
                        Some(DeckHit::Menu) => self.open_palette(),
                        None => {}
                    }
                }
                _ => {}
            }
            return;
        }
        match ev.kind {
            MouseEventKind::Moved => {
                self.hover = Some((col, row));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.hover = Some((col, row));
                if self.sidebar_resizing {
                    self.resize_sidebar(col).await;
                } else if self.drag.is_some() {
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
                            if target != tab && self.drag_target != Some(target) {
                                if let Some(to) = self
                                    .active_space()
                                    .and_then(|s| s.tabs.iter().position(|t| t.id == target))
                                {
                                    self.drag_target = Some(target);
                                    let _ = self.route(Request::MoveTab { tab, to }).await;
                                }
                            }
                        }
                    }
                } else if let Some(space) = self.space_drag {
                    // Reorder spaces: move into the space row under the cursor.
                    let over = self.sidebar_rows.iter().find(|(r, _)| *r == row).and_then(
                        |(_, t)| match t {
                            Target::Space(id) => Some(*id),
                            _ => None,
                        },
                    );
                    if let Some(target) = over {
                        if target != space && self.drag_target != Some(target) {
                            if let Some(to) = self.snap.spaces.iter().position(|s| s.id == target) {
                                self.drag_target = Some(target);
                                let _ = self.route(Request::MoveSpace { space, to }).await;
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
                self.drag_target = None;
                if self.sidebar_resizing {
                    self.sidebar_resizing = false;
                    self.persist_sidebar_width(); // remember the new width next launch
                }
                if let Some(from) = self.swap_from.take() {
                    if let Some(to) = self.pane_at(col, row) {
                        if to != from {
                            if let Some(t) = self.active_tab() {
                                let mut layout = t.layout.clone();
                                layout.swap_leaves(from, to);
                                let tab = t.id;
                                if let Err(e) = self.route(Request::SetLayout { tab, layout }).await
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
                        if let Err(e) = self.route(Request::CloseTab { tab }).await {
                            self.toast(e.to_string());
                        }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Right-click a sidebar row → space/tab menu.
                let sidebar_hit = self
                    .frame
                    .sidebar
                    .map(|sb| col >= sb.x && col < sb.x + sb.width && row >= sb.y)
                    .unwrap_or(false)
                    || (self.drawer && {
                        let dr = self.drawer_rect();
                        col >= dr.x && col < dr.x + dr.width && row >= dr.y
                    });
                if sidebar_hit {
                    if let Some(target) = self
                        .sidebar_rows
                        .iter()
                        .find(|(r, _)| *r == row)
                        .map(|(_, t)| *t)
                    {
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
                            self.menu = Some(Menu {
                                x: col,
                                y: row,
                                target: mt,
                                items,
                            });
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
                // Grab the sidebar's edge to resize it (the two columns straddling
                // the sidebar/main boundary).
                if let Some(sb) = self.frame.sidebar {
                    let div = match self.cfg.ui.sidebar_pos {
                        SidebarPos::Left => sb.x + sb.width,
                        SidebarPos::Right => sb.x,
                    };
                    if row >= sb.y && row < sb.y + sb.height && (col == div || col + 1 == div) {
                        self.sidebar_resizing = true;
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
                            self.drag = Some(Drag {
                                tab: t.id,
                                path,
                                index,
                                dir,
                            });
                        }
                        return;
                    }
                }
                // Mobile focus: tapping the info header's right side (the ↓ live
                // control) jumps the pane back to live. Back lives in the cmd bar.
                if self.mobile_focus() && Some(row) == self.frame.header {
                    if col > self.size.0.saturating_sub(12) {
                        if let Some(v) = self.views.get_mut(&self.focused) {
                            v.scroll = 0;
                            v.parser.set_scrollback(0);
                        }
                    }
                    return;
                }
                if Some(row) == self.frame.header {
                    if col < 10 {
                        // On mobile the ☰ is the home button — back to the deck.
                        if self.cfg.ui.deck && self.narrow() {
                            self.deck = true;
                            self.sync_deck_sel();
                            self.sync().await;
                        } else {
                            self.do_action(Action::ToggleSidebar).await;
                        }
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
                if Some(row) == self.frame.action {
                    let hit = self
                        .action_hits
                        .iter()
                        .find(|(_, r, range)| *r == row && range.contains(&col))
                        .map(|(a, _, _)| a.clone());
                    if let Some(a) = hit {
                        self.run_chip(a).await;
                    }
                    return;
                }
                if let Some(sb) = self.frame.sidebar {
                    if col >= sb.x && col < sb.x + sb.width && row >= sb.y {
                        // Visible buttons (× close space) win over row-select.
                        if let Some(btn) = self
                            .sidebar_buttons
                            .iter()
                            .find(|(r, cr, _)| *r == row && cr.contains(&col))
                            .map(|(_, _, b)| *b)
                        {
                            match btn {
                                SidebarBtn::CloseSpace(id) => {
                                    if let Err(e) =
                                        self.route(Request::CloseSpace { space: id }).await
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
                    if let Some(tab) = self
                        .tab_close_hits
                        .iter()
                        .find(|(_, r)| r.contains(&col))
                        .map(|(id, _)| *id)
                    {
                        if let Err(e) = self.route(Request::CloseTab { tab }).await {
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
                    // Link handler: a (modified) click on matched text runs its rule.
                    let link_trigger = match self.cfg.ui.link_click {
                        LinkClick::Ctrl => ev.modifiers.contains(KeyModifiers::CONTROL),
                        LinkClick::Shift => ev.modifiers.contains(KeyModifiers::SHIFT),
                        LinkClick::Plain => true,
                    };
                    if link_trigger {
                        if let Some((p, (r, c))) = self.cell_at(col, row) {
                            if let Some((matched, argv, placement)) = self.link_at(p, r, c) {
                                match placement {
                                    // Open the link's tool in a ruckus split/tab/popup.
                                    Some(pl) => {
                                        self.notify(format!("↗ {matched}"));
                                        self.open_placement(argv, pl).await;
                                    }
                                    // Detached (e.g. `open <url>`).
                                    None => self.run_link(&argv, &matched),
                                }
                                return;
                            }
                        }
                    }
                    if let Some((s, t)) = self.locate(pane) {
                        self.set_active(s, t, pane).await;
                    }
                    // Begin a text selection anchored at the clicked cell.
                    if self.cfg.ui.mouse_select {
                        if let Some((pid, cell)) = self.cell_at(col, row) {
                            self.select = Some(Sel {
                                pane: pid,
                                anchor: cell,
                                head: cell,
                            });
                            self.selecting = true;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => self.pane_wheel(col, row, true).await,
            MouseEventKind::ScrollDown => self.pane_wheel(col, row, false).await,
            _ => {}
        }
    }

    /// Route a wheel notch over a pane. If the app inside enabled mouse tracking
    /// (claude, vim, htop…) forward the event so *it* scrolls; if it's just on
    /// the alternate screen (a pager) translate the wheel to arrow keys; only a
    /// plain shell falls through to ruckus's own scrollback.
    async fn pane_wheel(&mut self, col: u16, row: u16, up: bool) {
        // Content-relative cell; falls back to raw scrollback off the content area.
        let Some((pane, (crow, ccol))) = self.cell_at(col, row) else {
            if let Some(pane) = self.pane_at(col, row) {
                self.scroll_by(pane, if up { 3 } else { -3 });
            }
            return;
        };
        let (mode, enc, altscreen, appcursor) = match self.views.get(&pane) {
            Some(v) => {
                let s = v.parser.screen();
                (
                    s.mouse_protocol_mode(),
                    s.mouse_protocol_encoding(),
                    s.alternate_screen(),
                    s.application_cursor(),
                )
            }
            None => (
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Default,
                false,
                false,
            ),
        };
        let bytes = if mode != vt100::MouseProtocolMode::None {
            Some(encode_mouse_wheel(up, ccol, crow, enc))
        } else if altscreen {
            // Alternate-scroll: most terminals send arrow keys here.
            let seq: &[u8] = match (up, appcursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            Some(seq.repeat(3))
        } else {
            None
        };
        match bytes {
            Some(bytes) => {
                let req = Request::Input {
                    pane,
                    data: B64.encode(&bytes),
                };
                if let Err(e) = self.route(req).await {
                    self.toast(e.to_string());
                }
            }
            None => self.scroll_by(pane, if up { 3 } else { -3 }),
        }
    }

    async fn on_server(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::State { snapshot } => {
                self.drag = None;
                // If a background update shrinks the tab list, keep the deck cursor
                // on a real card. Without this a cursor that was on the last card
                // slides onto the +tab/+space slot and Enter opens a dialog. Leave
                // it alone if the user had deliberately selected a button.
                let was_on_button = self
                    .active_space()
                    .map(|s| self.deck_sel >= s.tabs.len())
                    .unwrap_or(false);
                // The daemon sends the full merged snapshot (local + all remotes).
                // Focus is client-owned: keep our active space if it still exists
                // (it may be a remote one the daemon doesn't track focus for),
                // otherwise adopt the daemon's.
                let keep = self.snap.active_space;
                self.snap = snapshot;
                if self.snap.spaces.iter().any(|s| s.id == keep) {
                    self.snap.active_space = keep;
                }
                if self.deck && !was_on_button {
                    let ncards = self.active_space().map(|s| s.tabs.len()).unwrap_or(0);
                    self.deck_sel = self.deck_sel.min(ncards.saturating_sub(1));
                }
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
                if prev != Some(activity) {
                    self.flash.insert(pane, Instant::now()); // brief row flash
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
                self.flash.insert(pane, Instant::now());
                if pane == self.focused {
                    self.seen.insert(pane);
                } else {
                    self.unread.insert(pane);
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
        // Pick up any newly-declared plugins (cheap when all are already present).
        if !self.cfg.plugins.is_empty()
            && !crate::config::ensure_declared(&self.cfg.plugins).is_empty()
        {
            self.cfg = Config::load();
        }
        if self.cfg.ui.mouse != was_mouse {
            let mut out = std::io::stdout();
            if self.cfg.ui.mouse {
                let _ = crossterm::execute!(out, EnableMouseCapture);
            } else {
                let _ = crossterm::execute!(out, DisableMouseCapture);
            }
        }
        // Pick up newly config-declared remotes — ask the daemon to connect them
        // (already-connected hosts are a no-op on the daemon side).
        let specs: Vec<crate::config::RemoteSpec> = self.cfg.remotes.clone();
        for spec in specs {
            self.connect_remote(spec.host, spec.args).await;
        }
        self.sync().await;
        self.toast("config reloaded");
    }

    async fn on_term_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.on_key(k).await,
            Event::Mouse(m) => self.on_mouse(m).await,
            Event::Paste(text) => self.on_paste(text).await,
            Event::Resize(_, _) => self.sync().await,
            _ => {}
        }
    }

    /// Forward a clipboard paste to the focused pane. If the app enabled
    /// bracketed paste (shells, claude, editors) we wrap it in the 200~/201~
    /// markers so multi-line text lands as literal text instead of running each
    /// line. Newlines go on the wire as CR (what Enter sends).
    async fn on_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        // Overlays that take text input get the paste as typed characters.
        if let Some(p) = self.palette.as_mut() {
            p.query.push_str(&text.replace(['\r', '\n'], ""));
            p.sel = 0;
            return;
        }
        if let Some(p) = self.prompt.as_mut() {
            p.buffer.push_str(&text.replace(['\r', '\n'], " "));
            return;
        }
        let pane = self.focused;
        let bracketed = self
            .views
            .get(&pane)
            .map(|v| v.parser.screen().bracketed_paste())
            .unwrap_or(false);
        let body = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut data = Vec::new();
        if bracketed {
            data.extend_from_slice(b"\x1b[200~");
        }
        data.extend_from_slice(body.as_bytes());
        if bracketed {
            data.extend_from_slice(b"\x1b[201~");
        }
        let req = Request::Input {
            pane,
            data: B64.encode(&data),
        };
        if let Err(e) = self.route(req).await {
            self.toast(e.to_string());
        }
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Some((_, at)) = &self.toast {
            if at.elapsed().as_secs() >= self.cfg.ui.toast_seconds {
                self.toast = None;
            }
        }
        self.flash.retain(|_, t| t.elapsed().as_millis() < FLASH_MS);
        // Clear the copied selection once its green flash has run.
        if let Some(t) = self.copied_at {
            if t.elapsed().as_millis() >= 1000 {
                self.copied_at = None;
                self.select = None;
            }
        }
        // Reap a popup whose command has exited.
        let popup_dead = self
            .popup
            .as_mut()
            .map(|p| matches!(p.child.try_wait(), Ok(Some(_))))
            .unwrap_or(false);
        if popup_dead {
            self.close_popup();
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
                let bright = (self.tick / 3).is_multiple_of(2);
                (g.working.clone(), if bright { th.working } else { th.idle })
            }
        }
    }

    fn glyph(&self, info: Option<&PaneInfo>) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match info.map(|i| (i.activity, i.status)) {
            Some((Activity::Working, _)) => self.working_indicator(),
            // Waiting is a steady dot (no blink) — blinking in several sections at
            // once was too noisy.
            Some((Activity::Waiting, _)) => (g.waiting.clone(), th.waiting),
            Some((Activity::Done, PaneStatus::Exited { code })) => (
                g.done.clone(),
                if code == 0 { th.done_ok } else { th.done_err },
            ),
            Some((Activity::Done, _)) => (g.done.clone(), th.done_ok),
            _ => (g.idle.clone(), th.idle),
        }
    }

    /// Animated state glyph (spinner for working) — for tabs and the agents list.
    fn state_glyph(&self, a: Activity) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match a {
            Activity::Working => self.working_indicator(),
            Activity::Waiting => (g.waiting.clone(), th.waiting),
            Activity::Done => (g.done.clone(), th.done_ok),
            Activity::Idle => (g.idle.clone(), th.idle),
        }
    }

    /// Blend a row's background toward its state color for ~900ms after the
    /// pane's activity changed — a brief flash that says "this just changed".
    fn flash_bg(&self, pane: u64, base: Color) -> Color {
        let Some(t0) = self.flash.get(&pane) else {
            return base;
        };
        let e = t0.elapsed().as_millis();
        if e >= FLASH_MS {
            return base;
        }
        let toward = self
            .snap
            .pane(pane)
            .map(|p| self.static_glyph(p.activity).1)
            .unwrap_or(base);
        let intensity = (1.0 - e as f32 / FLASH_MS as f32) * 0.5;
        lerp_color(base, toward, intensity)
    }

    /// A tab flashes if any of its panes just changed.
    fn tab_flash_bg(&self, tab: &TabInfo, base: Color) -> Color {
        let mut leaves = Vec::new();
        tab.layout.leaves(&mut leaves);
        leaves
            .iter()
            .filter_map(|p| self.flash.get(p).map(|t| (*p, *t)))
            .min_by_key(|(_, t)| t.elapsed().as_millis())
            .map(|(p, _)| self.flash_bg(p, base))
            .unwrap_or(base)
    }

    /// Non-animated state glyph — for the SPACES aggregate so the space list
    /// stays calm (only the actual agent/tab rows animate).
    fn static_glyph(&self, a: Activity) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match a {
            Activity::Working => (g.working.clone(), th.working),
            Activity::Waiting => (g.waiting.clone(), th.waiting),
            Activity::Done => (g.done.clone(), th.done_ok),
            Activity::Idle => (g.idle.clone(), th.idle),
        }
    }

    fn hover_at(&self, col_range: &std::ops::Range<u16>, row: u16) -> bool {
        self.hover
            .map(|(c, r)| r == row && col_range.contains(&c))
            .unwrap_or(false)
    }

    /// Compact elapsed since a pane last changed state: "45s" / "12m" / "3h".
    fn elapsed_label(&self, pane: u64) -> String {
        let since = self.snap.pane(pane).map(|p| p.activity_since).unwrap_or(0);
        if since == 0 {
            return String::new();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let s = now.saturating_sub(since);
        if s < 60 {
            format!("{s}s")
        } else if s < 3600 {
            format!("{}m", s / 60)
        } else {
            format!("{}h", s / 3600)
        }
    }

    /// One clean info row for the mobile focus view (no back button — that lives
    /// in the bottom command bar now): name · loc … state elapsed / ↓ live.
    fn draw_mobile_header(&self, f: &mut Frame, area: Rect) {
        let th = &self.cfg.theme;
        let w = area.width;
        let bg = Style::default().bg(th.bg);
        let info = self.snap.pane(self.focused);
        let act = info.map(|p| p.activity).unwrap_or(Activity::Idle);
        let (g, scol) = self.state_glyph(act);
        let sword = activity_word(act);
        let name = self.active_tab().map(|t| t.name).unwrap_or_default();
        let loc = info.map(pane_loc).unwrap_or_default();
        let el = self.elapsed_label(self.focused);
        let scroll = self.views.get(&self.focused).map(|v| v.scroll).unwrap_or(0);

        let right = if scroll > 0 {
            format!("↑{scroll}  ↓ live ")
        } else if el.is_empty() {
            format!("{g} {sword} ")
        } else {
            format!("{g} {sword} {el} ")
        };
        let rcol = if scroll > 0 { th.accent } else { scol };
        let loc_disp = if loc.is_empty() {
            String::new()
        } else {
            format!("   {loc}")
        };
        let used = 1 + name.chars().count() + loc_disp.chars().count() + right.chars().count();
        let pad = (w as usize).saturating_sub(used);
        let line = Line::from(vec![
            Span::styled(
                format!(" {name}"),
                Style::default()
                    .fg(th.bar_active_fg)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(loc_disp, Style::default().fg(th.status_fg).bg(th.bg)),
            Span::styled(" ".repeat(pad), bg),
            Span::styled(
                right,
                Style::default()
                    .fg(rcol)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(
            Paragraph::new(line).style(bg),
            Rect::new(area.x, area.y, w, 1),
        );
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let th = &self.cfg.theme;
        // draw_mobile_header owns the mobile-focus case; here narrow shows ☰, wide
        // shows the plain logo.
        let logo_text = if self.narrow() {
            " ☰ ruckus "
        } else {
            "  ruckus  "
        };
        let logo = Span::styled(
            logo_text,
            Style::default()
                .bg(th.accent)
                .fg(th.sidebar_bg)
                .add_modifier(Modifier::BOLD),
        );
        // Narrow names the space (the tab strip shows tabs); wide shows the full
        // space › tab breadcrumb.
        let crumb = if self.narrow() {
            self.active_space()
                .map(|s| format!("  {}", s.name))
                .unwrap_or_default()
        } else {
            match (self.active_space(), self.active_tab()) {
                (Some(s), Some(t)) => format!("  {}  ›  {}", s.name, t.name),
                (Some(s), None) => format!("  {}", s.name),
                _ => String::new(),
            }
        };
        let crumb_span = Span::styled(crumb.clone(), Style::default().fg(th.bar_active_fg));

        let waiting = self
            .snap
            .panes
            .iter()
            .filter(|p| p.activity == Activity::Waiting)
            .count();
        let working = self
            .snap
            .panes
            .iter()
            .filter(|p| p.activity == Activity::Working)
            .count();
        let done = self
            .snap
            .panes
            .iter()
            .filter(|p| p.activity == Activity::Done && !self.seen.contains(&p.id))
            .count();
        let mut right: Vec<Span> = Vec::new();
        let mut right_len = 0usize;
        for (n, activity) in [
            (waiting, Activity::Waiting),
            (working, Activity::Working),
            (done, Activity::Done),
        ] {
            if n > 0 {
                let (g, color) = self.state_glyph(activity);
                let text = format!("{n} {}   ", activity_word(activity));
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

    /// Split-aware sidebar: stacked (single region) or, when `sidebar_split > 0`,
    /// the last section pinned to a fixed bottom region (herdr-style).
    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let sections = self.cfg.ui.sidebar_sections.clone();
        let split = self.cfg.ui.sidebar_split;
        self.sidebar_rows.clear();
        self.sidebar_buttons.clear();
        if split <= 0.0 || sections.len() < 2 || area.height < 8 {
            self.draw_sidebar_region(f, area, &sections, true);
            return;
        }
        let (top, pinned) = sections.split_at(sections.len() - 1);
        let bottom_h =
            ((area.height as f64 * split) as u16).clamp(3, area.height.saturating_sub(3));
        let top_h = area.height.saturating_sub(bottom_h + 1); // -1 for divider
        let top_rect = Rect::new(area.x, area.y, area.width, top_h);
        let div_row = area.y + top_h;
        let bot_rect = Rect::new(area.x, div_row + 1, area.width, bottom_h);
        self.draw_sidebar_region(f, top_rect, top, true);
        // Flat: a blank gap row (no line) separates the two regions; the section
        // header does the labeling.
        f.render_widget(
            Paragraph::new(" ").style(Style::default().bg(self.cfg.theme.sidebar_bg)),
            Rect::new(area.x, div_row, area.width, 1),
        );
        self.draw_sidebar_region(f, bot_rect, pinned, true);
    }

    fn draw_sidebar_region(
        &mut self,
        f: &mut Frame,
        area: Rect,
        sections: &[String],
        append: bool,
    ) {
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
        // Line index of the active space, for scroll-follow when the list overflows.
        let mut focus_idx: Option<usize> = None;

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

        for section in sections {
            match section.as_str() {
                "needs_you" => {
                    let attention: Vec<AttnRow> = self
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
                            Style::default()
                                .fg(th.status_fg)
                                .add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
                    let tpl = self.cfg.ui.queue_row.clone();
                    let gap = self.cfg.ui.sidebar_row_gap;
                    for (id, vars, icon) in attention {
                        for _ in 0..gap {
                            push!(Line::raw(""), None::<Target>);
                        }
                        let selected = id == self.focused;
                        let hovered = hover_row == Some(y);
                        // Subtle selection: fill only on hover; the focused row is
                        // marked with the accent bar instead of a full block. A
                        // fresh activity change briefly flashes the row background.
                        let base_bg = if hovered { th.select_bg } else { th.sidebar_bg };
                        let row_style = Style::default().bg(self.flash_bg(id, base_bg));
                        let lead = if selected {
                            Span::styled(self.cfg.glyphs.focus.clone(), row_style.fg(th.accent))
                        } else {
                            Span::styled(" ".to_string(), row_style)
                        };
                        let mut spans = vec![lead, Span::styled(" ".to_string(), row_style)];
                        spans.extend(template_spans(
                            &tpl,
                            &icon,
                            &vars,
                            row_style,
                            if selected {
                                th.bar_active_fg
                            } else {
                                th.bar_fg
                            },
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
                            Style::default()
                                .fg(th.status_fg)
                                .add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
                    let gap = self.cfg.ui.sidebar_row_gap;
                    for (pid, tab_name, space_name) in agents {
                        for _ in 0..gap {
                            push!(Line::raw(""), None::<Target>);
                        }
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
                        let row_style = if hovered {
                            Style::default().bg(th.select_bg)
                        } else {
                            Style::default()
                        };
                        // Line 1: accent marker (if focused) + state dot + agent name (bold).
                        let lead = if selected {
                            Span::styled(self.cfg.glyphs.focus.clone(), row_style.fg(th.accent))
                        } else {
                            Span::styled(" ".to_string(), row_style)
                        };
                        let mut spans = vec![
                            lead,
                            Span::styled(" ".to_string(), row_style),
                            Span::styled(format!("{g} "), row_style.fg(color)),
                            Span::styled(
                                tab_name,
                                row_style.fg(th.bar_active_fg).add_modifier(Modifier::BOLD),
                            ),
                        ];
                        let pad = w.saturating_sub(spans_width(&spans));
                        spans.push(Span::styled(" ".repeat(pad), row_style));
                        push!(Line::from(spans), Some(Target::Pane(pid)));
                        // Line 2: dimmed space it lives in.
                        let sub_fg = if selected { th.bar_fg } else { th.status_fg };
                        let mut sub = vec![
                            Span::styled("      ".to_string(), row_style),
                            Span::styled(space_name, row_style.fg(sub_fg)),
                        ];
                        let pad = w.saturating_sub(spans_width(&sub));
                        sub.push(Span::styled(" ".repeat(pad), row_style));
                        push!(Line::from(sub), Some(Target::Pane(pid)));
                    }
                    push!(Line::raw(""), None::<Target>);
                }
                "spaces" => {
                    push!(
                        Line::from(Span::styled(
                            " SPACES",
                            Style::default()
                                .fg(th.status_fg)
                                .add_modifier(Modifier::BOLD),
                        )),
                        None::<Target>
                    );
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
                    for s in spaces.iter() {
                        for _ in 0..row_gap {
                            push!(Line::raw(""), None::<Target>);
                        }
                        let s_active = s.id == self.snap.active_space;
                        let mut icon = self.static_glyph(space_activity(&self.snap, s));
                        if self.space_unread(s) {
                            icon.1 = th.accent; // unread badge
                        }
                        let hovered = hover_row == Some(y);
                        let row_style = if hovered {
                            Style::default().bg(th.select_bg)
                        } else {
                            Style::default()
                        };
                        // Tag remote spaces with their host (e.g. "workbox: api").
                        let host = self.host_of(s.id);
                        let text_fg = if s_active {
                            th.accent
                        } else if !host.is_empty() {
                            // Remote row: use the configured remote accent if set.
                            th.remote.unwrap_or(th.bar_active_fg)
                        } else {
                            th.bar_active_fg
                        };
                        // Configurable remote label (default "☁ {host}: {name}").
                        let disp = if host.is_empty() {
                            s.name.clone()
                        } else {
                            self.cfg
                                .ui
                                .remote_label
                                .replace("{host}", host)
                                .replace("{name}", &s.name)
                        };
                        let vars = vec![
                            ("name", disp.clone()),
                            ("title", disp),
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
                        spans.extend(template_spans(
                            &space_tpl,
                            &icon,
                            &vars,
                            row_style.add_modifier(Modifier::BOLD),
                            text_fg,
                        ));
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
                        // Remember the active space's line so an overflowing list
                        // scrolls to keep it (and thus remote spaces you jump to)
                        // in view instead of truncating it off the bottom.
                        if s_active {
                            focus_idx = Some(lines.len());
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
                            let base_bg = if t_active || hovered {
                                th.select_bg
                            } else {
                                th.sidebar_bg
                            };
                            let row_style = Style::default().bg(self.tab_flash_bg(t, base_bg));
                            let text_fg = if t_active {
                                th.bar_active_fg
                            } else {
                                th.bar_fg
                            };
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
                                    active_pane
                                        .map(|p| home_relative(&p.cwd))
                                        .unwrap_or_default(),
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
                            spans
                                .extend(template_spans(&tab_tpl, &icon, &vars, row_style, text_fg));
                            let pad = w.saturating_sub(spans_width(&spans));
                            spans.push(Span::styled(" ".repeat(pad), row_style));
                            let tab_target = Target::Tab {
                                space: s.id,
                                tab: t.id,
                                pane: t.active_pane,
                            };
                            push!(Line::from(spans), Some(tab_target));

                            if !tab_sub.is_empty() {
                                let sub_fg = if t_active { th.bar_fg } else { th.status_fg };
                                let mut sub = vec![Span::styled("      ".to_string(), row_style)];
                                sub.extend(template_spans(
                                    &tab_sub, &no_icon, &vars, row_style, sub_fg,
                                ));
                                let pad = w.saturating_sub(spans_width(&sub));
                                sub.push(Span::styled(" ".repeat(pad), row_style));
                                push!(Line::from(sub), Some(tab_target));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Scroll the region so the active space stays visible instead of being
        // truncated off the bottom (matters once you have more spaces — local or
        // remote — than fit). No overflow → no scroll → unchanged behaviour.
        let h = inner.height as usize;
        if lines.len() > h {
            let off = focus_idx
                .map(|fi| fi.saturating_sub(h - 1))
                .unwrap_or(0)
                .min(lines.len() - h);
            if off > 0 {
                let off_u = off as u16;
                lines.drain(0..off);
                rows.retain(|(ry, _)| *ry >= inner.y + off_u);
                rows.iter_mut().for_each(|(ry, _)| *ry -= off_u);
                buttons.retain(|(by, _, _)| *by >= inner.y + off_u);
                buttons.iter_mut().for_each(|(by, _, _)| *by -= off_u);
            }
        }
        lines.truncate(h);
        if append {
            self.sidebar_rows.extend(rows);
            self.sidebar_buttons.extend(buttons);
        } else {
            self.sidebar_rows = rows;
            self.sidebar_buttons = buttons;
        }
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
                // Display width, not char count — a CJK/emoji tab name renders
                // wider than its char count, which would drift the × hit region.
                let tlen = Span::raw(text.as_str()).width();
                // Pill: pad + "icon " + text + " ×" + pad, then a gap.
                let width = (plen * 2 + tlen + 4) as u16;
                let range = x..x + width;
                let hovered = self.hover_at(&range, area.y);
                // Active tab uses the accent so "you are here" is obvious; the
                // strip sits on `surface` so it reads as distinct from the header.
                let (bg, fg, bold) = if active {
                    (th.select_bg, th.accent, true)
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
                    base.fg(if close_hover {
                        th.done_err
                    } else {
                        th.status_fg
                    }),
                ));
                spans.push(Span::styled(pad.clone(), base));
                spans.push(Span::raw(" ")); // gap between pills
                hits.push((Some(t.id), range));
                close_hits.push((t.id, close_col..close_col + 1));
                x += width + 1;
            }
        }
        // A labelled "+ tab" button, clearly tappable.
        let plus_label = " + tab ";
        let plus_range = x..x + plus_label.chars().count() as u16;
        let plus_hover = self.hover_at(&plus_range, area.y);
        spans.push(Span::styled(
            plus_label,
            if plus_hover {
                Style::default()
                    .bg(th.accent)
                    .fg(th.surface)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(th.select_bg)
                    .fg(th.accent)
                    .add_modifier(Modifier::BOLD)
            },
        ));
        hits.push((None, plus_range));
        self.tab_hits = hits;
        self.tab_close_hits = close_hits;
        // Strip background is `surface` (not the header's bar_bg) so the two top
        // rows read as clearly separate bands.
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.surface)),
            area,
        );
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        let show_hints = match self.cfg.ui.footer_mode {
            FooterMode::Help => true,
            FooterMode::Status => false,
            FooterMode::Auto => self.prefix_pending,
        };
        if show_hints {
            self.draw_footer_hints(f, area);
        } else {
            self.draw_footer_status(f, area);
        }
    }

    /// Re-run every `#(command)` in the status format and cache its first line.
    async fn refresh_status_cmds(&mut self) {
        let mut cmds = extract_commands(&self.cfg.ui.status_left);
        cmds.extend(extract_commands(&self.cfg.ui.status_right));
        cmds.sort();
        cmds.dedup();
        for cmd in cmds {
            let out = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .output(),
            )
            .await;
            let val = match out {
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                _ => String::new(),
            };
            self.status_cmds.insert(cmd, val);
        }
    }

    /// Render a status format string (`status_left`/`status_right`) into spans,
    /// expanding {tokens}, `#(command)` output, and `#[color]` markup.
    fn render_status(&self, template: &str) -> Vec<Span<'static>> {
        let th = &self.cfg.theme;
        let resolve_color = |name: &str| -> Color {
            if let Some(hex) = name.strip_prefix('#').filter(|h| h.len() == 6) {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                ) {
                    return Color::Rgb(r, g, b);
                }
            }
            match name {
                "accent" => th.accent,
                "bar_fg" => th.bar_fg,
                "bar_active_fg" => th.bar_active_fg,
                "status_fg" => th.status_fg,
                "working" => th.working,
                "waiting" => th.waiting,
                "idle" => th.idle,
                "done_ok" => th.done_ok,
                "done_err" => th.done_err,
                "select_bg" => th.select_bg,
                _ => th.status_fg,
            }
        };
        let val = |token: &str| -> String {
            let space = self.active_space();
            let tab = self.active_tab();
            let pane = self.snap.pane(self.focused);
            match token.split(':').next().unwrap_or(token) {
                "space" => space.map(|s| s.name).unwrap_or_default(),
                "tab" => tab.map(|t| t.name).unwrap_or_default(),
                "spaces" => self.snap.spaces.len().to_string(),
                "tabs" => space.map(|s| s.tabs.len()).unwrap_or(0).to_string(),
                "panes" => self.snap.panes.len().to_string(),
                "agents" => self.agent_rows().len().to_string(),
                "needs" => self.attention().len().to_string(),
                "host" => hostname(),
                "cwd" => pane.map(|p| home_relative(&p.cwd)).unwrap_or_default(),
                "clock" => {
                    let fmt = token.split_once(':').map(|x| x.1).unwrap_or("%H:%M");
                    chrono::Local::now().format(fmt).to_string()
                }
                _ => String::new(),
            }
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut fg = th.status_fg;
        let mut buf = String::new();
        let mut chars = template.chars().peekable();
        let flush = |spans: &mut Vec<Span>, buf: &mut String, fg: Color| {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(buf), Style::default().fg(fg)));
            }
        };
        while let Some(ch) = chars.next() {
            if ch == '#' && chars.peek() == Some(&'[') {
                flush(&mut spans, &mut buf, fg);
                chars.next(); // consume '['
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    name.push(c);
                }
                fg = if name.is_empty() {
                    th.status_fg
                } else {
                    resolve_color(&name)
                };
            } else if ch == '#' && chars.peek() == Some(&'(') {
                chars.next(); // consume '('
                let mut cmd = String::new();
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                    cmd.push(c);
                }
                if let Some(v) = self.status_cmds.get(&cmd) {
                    buf.push_str(v);
                }
            } else if ch == '{' {
                flush(&mut spans, &mut buf, fg);
                let mut token = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    token.push(c);
                }
                buf.push_str(&val(&token));
            } else {
                buf.push(ch);
            }
        }
        flush(&mut spans, &mut buf, fg);
        spans
    }

    fn draw_footer_status(&mut self, f: &mut Frame, area: Rect) {
        let th = self.cfg.theme.clone();
        self.footer_hits = Vec::new(); // status bar isn't clickable
        let left = self.render_status(&self.cfg.ui.status_left.clone());
        let right = self.render_status(&self.cfg.ui.status_right.clone());
        let w = area.width as usize;
        // Use display width (unicode-aware) so wide glyphs/emoji don't push the
        // right segment off the edge.
        let lw: usize = left.iter().map(|s| s.width()).sum();
        let rw: usize = right.iter().map(|s| s.width()).sum();
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        spans.extend(left);
        // pad so `right` is flush to the edge (leave 1 col so the last cell
        // isn't at the very edge, which some terminals clip).
        let used = 1 + lw + rw + 1;
        if w > used {
            spans.push(Span::raw(" ".repeat(w - used)));
        }
        spans.extend(right);
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    fn draw_footer_hints(&mut self, f: &mut Frame, area: Rect) {
        let th = self.cfg.theme.clone();
        let c = &self.cfg;
        let compact = area.width < FOOTER_COMPACT || self.narrow();
        let mut chips: Vec<(Action, String, &str)> = vec![
            (Action::JumpWaiting, c.hint(Action::JumpWaiting), "next"),
            (Action::SplitRight, c.hint(Action::SplitRight), "split"),
            (Action::SplitDown, c.hint(Action::SplitDown), "split↓"),
            (Action::NewTab, c.hint(Action::NewTab), "tab"),
            (Action::NewSpace, c.hint(Action::NewSpace), "space"),
            (Action::ClosePane, c.hint(Action::ClosePane), "close"),
            (Action::ToggleSidebar, c.hint(Action::ToggleSidebar), "bar"),
            (Action::ShowHelp, c.hint(Action::ShowHelp), "help"),
            (Action::Quit, c.hint(Action::Quit), "quit"),
        ];
        if compact {
            // Triage-first: jump / zoom / nav / more. Layout chrome lives in palette.
            chips = vec![
                (Action::JumpWaiting, "".into(), "next"),
                (Action::Zoom, "".into(), "zoom"),
                (Action::ToggleSidebar, "".into(), "bar"),
                (Action::Palette, "".into(), "···"),
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
                Style::default()
                    .bg(th.select_bg)
                    .fg(th.accent)
                    .add_modifier(Modifier::BOLD)
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
        // No per-pane title band in the mobile focus view — the header names the pane.
        let titles = self.cfg.ui.pane_titles && !self.mobile_focus();
        let pad = self.cfg.ui.pane_padding;
        for (pane, rect) in &rects {
            if rect.width < 3 || rect.height < 2 {
                continue;
            }
            let focused = *pane == self.focused;
            let info = self.snap.pane(*pane);
            let (g, dot) = self.glyph(info);
            let title_text = info
                .map(|i| i.title.clone())
                .unwrap_or_else(|| pane.to_string());
            let scroll = self.views.get(pane).map(|v| v.scroll).unwrap_or(0);

            // Whole pane sits on the surface layer (padding shows as surface).
            f.render_widget(
                Paragraph::new("").style(Style::default().bg(th.surface)),
                *rect,
            );

            let mut content_y = rect.y;
            let mut content_h = rect.height;
            if titles {
                let bar_bg = self.flash_bg(*pane, if focused { th.select_bg } else { th.bar_bg });
                let mut title_spans = vec![
                    Span::styled(
                        if focused {
                            self.cfg.glyphs.focus.clone()
                        } else {
                            " ".to_string()
                        },
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
                if let Some(PaneInfo {
                    status: PaneStatus::Exited { code },
                    ..
                }) = info
                {
                    title_spans.push(Span::styled(
                        format!(" exit {code} "),
                        Style::default()
                            .fg(if *code == 0 { th.done_ok } else { th.done_err })
                            .bg(bar_bg),
                    ));
                }
                f.render_widget(
                    Paragraph::new(Line::from(title_spans)).style(Style::default().bg(bar_bg)),
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
            let dimmed = many && !focused;
            let exited = matches!(info.map(|i| &i.status), Some(PaneStatus::Exited { .. }));
            if let Some(v) = self.views.get(pane) {
                let screen = v.parser.screen();
                let live = focused && scroll == 0 && !exited;
                // Draw the synthetic (inverted-cell) cursor only when we can't place
                // the real one — otherwise place the terminal's native blinking cursor
                // at the focused pane so it blinks and mobile IMEs track it.
                let mut placed_real = false;
                if live && !screen.hide_cursor() {
                    let (crow, ccol) = screen.cursor_position();
                    let cx = content.x + ccol.min(content.width.saturating_sub(1));
                    let cy = content.y + crow.min(content.height.saturating_sub(1));
                    f.set_cursor_position((cx, cy));
                    placed_real = true;
                }
                let mut lines = screen_to_lines(screen, live && !placed_real, dimmed || exited);
                // Highlight scrollback-search matches on the focused pane: tint any
                // visible row that contains the query.
                if focused {
                    if let Some(q) = self.search.as_ref().filter(|q| !q.is_empty()) {
                        let ql = q.to_lowercase();
                        let row_texts: Vec<String> = screen
                            .contents()
                            .split('\n')
                            .map(|s| s.to_lowercase())
                            .collect();
                        for (i, line) in lines.iter_mut().enumerate() {
                            if row_texts.get(i).is_some_and(|t| t.contains(&ql)) {
                                for sp in &mut line.spans {
                                    sp.style = sp.style.bg(th.waiting).fg(th.bg);
                                }
                            }
                        }
                    }
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(th.surface)),
                    content,
                );
                // A blank pane that just launched is probably a slow tool loading
                // (e.g. gh-dash fetching) — show a spinner so it doesn't read as
                // frozen. Only for the first ~20s; after that a blank pane is just
                // blank.
                let starting = !exited
                    && content.height >= 1
                    && screen.contents().trim().is_empty()
                    && info
                        .map(|i| crate::protocol::unix_now().saturating_sub(i.created) < 20)
                        .unwrap_or(false);
                if starting {
                    let cmd = info
                        .and_then(|i| i.cmd.first().cloned())
                        .unwrap_or_default();
                    let msg = if cmd.is_empty() {
                        format!("{} starting…", self.spin())
                    } else {
                        format!("{} starting {cmd}…", self.spin())
                    };
                    let mw = (msg.chars().count() as u16).min(content.width);
                    let mx = content.x + content.width.saturating_sub(mw) / 2;
                    let my = content.y + content.height / 2;
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            msg,
                            Style::default().fg(th.status_fg).bg(th.surface),
                        ))),
                        Rect::new(mx, my, mw, 1),
                    );
                }
            }
            // Dead-pane frame: status banner + restart/close hints over the content.
            if let Some(PaneInfo {
                status: PaneStatus::Exited { code },
                ..
            }) = info
            {
                if content.height >= 2 && content.width >= 12 {
                    let ok = *code == 0;
                    let status_fg = if ok { th.done_ok } else { th.done_err };
                    let banner = format!("[ EXITED {code} ]");
                    let hints = if focused {
                        "enter restart · esc close · or tap chips"
                    } else {
                        "exited"
                    };
                    let ban_w = banner.chars().count() as u16;
                    let hint_w = hints.chars().count() as u16;
                    let mid_y = content.y + content.height / 2;
                    let ban_x = content.x + content.width.saturating_sub(ban_w) / 2;
                    let hint_x = content.x + content.width.saturating_sub(hint_w) / 2;
                    f.render_widget(
                        Paragraph::new(Span::styled(
                            banner,
                            Style::default()
                                .fg(status_fg)
                                .bg(th.surface)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Rect::new(ban_x, mid_y.saturating_sub(1).max(content.y), ban_w, 1),
                    );
                    if content.height >= 3 {
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                hints,
                                Style::default().fg(th.status_fg).bg(th.surface),
                            )),
                            Rect::new(hint_x, mid_y.min(content.y + content.height - 1), hint_w, 1),
                        );
                    }
                }
            }
        }
    }

    fn draw_palette(&mut self, f: &mut Frame) {
        if self.palette.is_none() {
            self.palette_hits.clear();
            return;
        }
        let th = self.cfg.theme.clone();
        let rows = self.palette_rows();
        let (query, commands) = {
            let p = self.palette.as_ref().unwrap();
            (p.query.clone(), p.is_commands())
        };

        // Large centered overlay.
        let w = self.size.0.saturating_sub(4).min(100);
        let h = ((self.size.1 as u32 * 4 / 5) as u16)
            .max(8)
            .min(self.size.1.saturating_sub(2));
        let x = self.size.0.saturating_sub(w) / 2;
        let y = self.size.1.saturating_sub(h) / 2;
        let r = Rect::new(x, y, w, h);
        f.render_widget(Clear, r);
        let title = if commands { " run command " } else { " go to " };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.sidebar_bg))
            .title(Line::from(Span::styled(
                title,
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(r);
        f.render_widget(block, r);
        if inner.height < 3 || inner.width < 8 {
            self.palette_hits.clear();
            return;
        }

        // Inner layout: row 0 = query, row 1 = blank, [list…], last row = hint.
        let list_top = inner.y + 2;
        let hint_row = inner.y + inner.height - 1;
        let cap = hint_row.saturating_sub(list_top) as usize;

        // Keep the selection in view.
        let len = rows.len();
        let (sel, scroll) = {
            let p = self.palette.as_mut().unwrap();
            if p.sel >= len {
                p.sel = len.saturating_sub(1);
            }
            if p.sel < p.scroll {
                p.scroll = p.sel;
            } else if cap > 0 && p.sel >= p.scroll + cap {
                p.scroll = p.sel + 1 - cap;
            }
            p.scroll = if len > cap {
                p.scroll.min(len - cap)
            } else {
                0
            };
            (p.sel, p.scroll)
        };

        let iw = inner.width as usize;
        let bg = Style::default().bg(th.sidebar_bg);

        // Query line.
        let cursor = if (self.tick / 4).is_multiple_of(2) {
            "▏"
        } else {
            " "
        };
        let mut qspans = vec![Span::styled(" › ", bg.fg(th.accent))];
        if commands {
            qspans.push(Span::styled(
                "> ",
                bg.fg(th.accent).add_modifier(Modifier::BOLD),
            ));
        }
        let shown_q = if commands {
            query.strip_prefix('>').unwrap_or(&query).to_string()
        } else {
            query.clone()
        };
        qspans.push(Span::styled(shown_q, bg.fg(th.bar_active_fg)));
        qspans.push(Span::styled(cursor.to_string(), bg.fg(th.accent)));
        f.render_widget(
            Paragraph::new(Line::from(qspans)).style(bg),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        // Rows.
        let mut hits = Vec::new();
        let mut yy = list_top;
        for (i, row) in rows.iter().enumerate().skip(scroll).take(cap) {
            let selected = i == sel;
            let base = if selected {
                Style::default().bg(th.select_bg)
            } else {
                bg
            };
            let marker = if selected { "❯" } else { " " };
            let indent = "  ".repeat(row.depth as usize);
            let twisty = if row.expandable {
                if row.expanded {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
            let (g, gcol) = &row.glyph;
            let glyph_part = if g.is_empty() {
                String::new()
            } else {
                format!("{g} ")
            };
            let name_fg = if row.dim {
                th.status_fg
            } else if selected {
                th.bar_active_fg
            } else {
                th.bar_fg
            };
            let mut spans = vec![
                Span::styled(
                    format!("{marker} "),
                    base.fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(indent.clone(), base),
                Span::styled(format!("{twisty} "), base.fg(th.status_fg)),
            ];
            if !glyph_part.is_empty() {
                spans.push(Span::styled(glyph_part.clone(), base.fg(*gcol)));
            }
            spans.extend(hl_spans(
                &row.label, &row.hl, base, name_fg, th.accent, !row.dim,
            ));
            let lead = 2
                + indent.chars().count()
                + 2
                + glyph_part.chars().count()
                + row.label.chars().count();
            let mw = row.meta.chars().count();
            let pad = iw.saturating_sub(lead + mw + 1);
            spans.push(Span::styled(" ".repeat(pad), base));
            spans.push(Span::styled(
                format!("{} ", row.meta),
                base.fg(th.status_fg),
            ));
            f.render_widget(
                Paragraph::new(Line::from(spans)).style(base),
                Rect::new(inner.x, yy, inner.width, 1),
            );
            hits.push((Rect::new(inner.x, yy, inner.width, 1), i));
            yy += 1;
        }
        self.palette_hits = hits;

        if rows.is_empty() {
            let msg = if commands {
                "  no command"
            } else {
                "  no match"
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, bg.fg(th.status_fg)))).style(bg),
                Rect::new(inner.x, list_top, inner.width, 1),
            );
        }

        // Footer: key hints + scroll arrows.
        let nav = if commands {
            "↵ run   ↑↓ move   ⌫ back to tree   esc"
        } else {
            "↵ jump   ←/→ fold   ↑↓ move   > commands   esc"
        };
        let arrows = format!(
            "{}{}",
            if scroll > 0 { "▲" } else { " " },
            if scroll + cap < len { "▼" } else { " " }
        );
        let hpad = iw.saturating_sub(nav.chars().count() + 1 + arrows.chars().count() + 1);
        let hint = Line::from(vec![
            Span::styled(format!(" {nav}"), bg.fg(th.status_fg)),
            Span::styled(" ".repeat(hpad), bg),
            Span::styled(format!("{arrows} "), bg.fg(th.accent)),
        ]);
        f.render_widget(
            Paragraph::new(hint).style(bg),
            Rect::new(inner.x, hint_row, inner.width, 1),
        );
    }

    fn draw_action_bar(&mut self, f: &mut Frame, area: Rect) {
        // Mobile focus: tappable command bar showing the REAL keys for the active
        // keymap. tmux → a single "⌃b" indicator then bare keys (which-key style);
        // alt → the ⌥key per command. Tapping and pressing agree.
        if self.mobile_focus() {
            let th = self.cfg.theme.clone();
            let cmds: [(&str, ChipAction, Action); 5] = [
                ("back", ChipAction::Back, Action::Deck),
                ("next", ChipAction::Next, Action::NextTab),
                ("zoom", ChipAction::Zoom, Action::Zoom),
                ("find", ChipAction::Search, Action::Search),
                ("close", ChipAction::Close, Action::ClosePane),
            ];
            // Whenever a prefix exists (tmux or both), render which-key style: one
            // leading "⌃b" indicator, then the bare prefix keys. This matches the
            // help overlay's hint() and never shows "⌥d" for back (which collides
            // with prefix+d = detach). Only pure-alt mode shows the ⌥ chords.
            let prefixed = self.cfg.prefix.is_some();
            let bar_bg = Style::default().bg(th.bar_bg);
            let mut spans: Vec<Span> = vec![Span::styled(" ", bar_bg)];
            let mut hits = Vec::new();
            let mut x = area.x + 1;
            if prefixed {
                let pfx = self.cfg.prefix.map(|b| b.compact()).unwrap_or_default();
                let t = format!("{pfx} ");
                x += t.chars().count() as u16;
                spans.push(Span::styled(
                    t,
                    Style::default().fg(th.status_fg).bg(th.bar_bg),
                ));
            }
            for (label, chip, action) in cmds {
                let key = if prefixed {
                    self.cfg
                        .prefix_keys
                        .get(&action)
                        .and_then(|b| b.first())
                        .map(|b| b.compact())
                } else {
                    self.cfg
                        .keys
                        .get(&action)
                        .and_then(|b| b.first())
                        .map(|b| b.compact())
                };
                let width = (key.as_ref().map(|k| k.chars().count() + 1).unwrap_or(0)
                    + label.chars().count()) as u16;
                if x + width + 2 > area.x + area.width {
                    break;
                }
                let range = x..x + width;
                let hovered = self.hover_at(&range, area.y);
                let kbg = if hovered { th.select_bg } else { th.bar_bg };
                if let Some(k) = &key {
                    spans.push(Span::styled(
                        format!("{k} "),
                        Style::default()
                            .fg(th.accent)
                            .bg(kbg)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(if hovered {
                            th.bar_active_fg
                        } else {
                            th.status_fg
                        })
                        .bg(kbg),
                ));
                spans.push(Span::styled("   ", bar_bg));
                hits.push((chip, area.y, range));
                x += width + 3;
            }
            self.action_hits = hits;
            f.render_widget(Paragraph::new(Line::from(spans)).style(bar_bg), area);
            return;
        }
        let Some(kind) = self.action_bar_kind() else {
            self.action_hits.clear();
            return;
        };
        let th = self.cfg.theme.clone();
        // label → chip action
        let chips: Vec<(&str, ChipAction)> = match kind {
            "waiting" => vec![
                ("y", ChipAction::Send(b"y\n".to_vec())),
                ("n", ChipAction::Send(b"n\n".to_vec())),
                ("enter", ChipAction::Send(b"\n".to_vec())),
                ("type…", ChipAction::Reply),
                ("next", ChipAction::JumpWaiting),
            ],
            "exited" => vec![
                ("restart", ChipAction::Restart),
                ("close", ChipAction::Close),
                ("next", ChipAction::JumpWaiting),
            ],
            // narrow idle/working: navigation only
            _ => vec![
                ("next", ChipAction::JumpWaiting),
                ("zoom", ChipAction::Zoom),
                ("bar", ChipAction::ToggleSidebar),
                ("···", ChipAction::Palette),
            ],
        };

        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        let mut hits = Vec::new();
        let mut x: u16 = 1;
        let accent = match kind {
            "waiting" => th.waiting,
            "exited" => th.done_err,
            _ => th.accent,
        };
        // Leading status glyph so the bar reads as a state, not random buttons.
        let tag = match kind {
            "waiting" => " needs you ",
            "exited" => " exited ",
            _ => " ",
        };
        if kind != "narrow" {
            let tw = tag.chars().count() as u16;
            spans.push(Span::styled(
                tag,
                Style::default()
                    .fg(th.bg)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            x += tw + 1;
        }
        for (label, action) in chips {
            let text = format!("[{label}]");
            let width = text.chars().count() as u16;
            let range = x..x + width;
            let hovered = self.hover_at(&range, area.y);
            let style = if hovered {
                Style::default()
                    .bg(th.select_bg)
                    .fg(accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(accent)
                    .bg(th.bar_bg)
                    .add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw(" "));
            hits.push((action, area.y, range));
            x += width + 1;
            if x >= area.width.saturating_sub(2) {
                break;
            }
        }
        self.action_hits = hits;
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    /// The touch-first mobile deck: big state-colored cards for each tab in the
    /// current space, attention-sorted. Tap a card to enter it fullscreen.
    fn draw_deck(&mut self, f: &mut Frame) {
        let area = f.area();
        let th = self.cfg.theme.clone();
        self.deck_hits.clear();
        f.render_widget(Paragraph::new("").style(Style::default().bg(th.bg)), area);

        let inset = 2u16;
        let cx = area.x + inset;
        let cw = area.width.saturating_sub(inset * 2);
        let bgstyle = Style::default().bg(th.bg);

        // ---- Title row: ☰  ruckus … ● N needs you ----
        let waiting = self
            .snap
            .panes
            .iter()
            .filter(|p| p.activity == Activity::Waiting)
            .count();
        let (sumtxt, sumcol) = if waiting > 0 {
            (format!("● {waiting} needs you"), th.waiting)
        } else {
            ("all quiet".to_string(), th.status_fg)
        };
        let used = 3 + "ruckus".len() + sumtxt.chars().count();
        let tpad = (cw as usize).saturating_sub(used);
        let title = Line::from(vec![
            Span::styled("☰  ", Style::default().fg(th.accent).bg(th.bg)),
            Span::styled(
                "ruckus",
                Style::default()
                    .fg(th.accent)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(tpad), bgstyle),
            Span::styled(
                sumtxt.clone(),
                Style::default()
                    .fg(sumcol)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(
            Paragraph::new(title).style(bgstyle),
            Rect::new(cx, area.y, cw, 1),
        );
        self.deck_hits
            .push((Rect::new(area.x, area.y, inset + 4, 1), DeckHit::Menu));
        if waiting > 0 {
            let sw = sumtxt.chars().count() as u16;
            self.deck_hits.push((
                Rect::new(cx + cw.saturating_sub(sw), area.y, sw, 1),
                DeckHit::Jump,
            ));
        }

        // ---- Space selector (row 2) ----
        let spaces = self.snap.spaces.clone();
        let mut spans: Vec<Span> = Vec::new();
        let mut x = cx;
        for s in &spaces {
            let active = s.id == self.snap.active_space;
            let w = s.name.chars().count() as u16;
            let style = if active {
                Style::default()
                    .fg(th.accent)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(th.status_fg).bg(th.bg)
            };
            spans.push(Span::styled(s.name.clone(), style));
            spans.push(Span::styled("    ", bgstyle));
            self.deck_hits
                .push((Rect::new(x, area.y + 2, w, 1), DeckHit::Space(s.id)));
            x += w + 4;
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(bgstyle),
            Rect::new(cx, area.y + 2, cw, 1),
        );

        // Selection cursor: 0..ncards = cards, then +tab, +space. Clamp it.
        let ncards = self.active_space().map(|s| s.tabs.len()).unwrap_or(0);
        self.deck_sel = self.deck_sel.min(ncards + 1);
        let sel = self.deck_sel;

        // ---- Bottom action row: subtle 1-row "+ tab" / "+ space" pills ----
        let brow = area.y + area.height - 1;
        let mut bx = cx;
        for (i, (lbl, hit)) in [
            (" + tab ", DeckHit::NewTab),
            (" + space ", DeckHit::NewSpace),
        ]
        .into_iter()
        .enumerate()
        {
            let focused = sel == ncards + i;
            let st = if focused {
                Style::default()
                    .bg(th.accent)
                    .fg(th.bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(th.surface).fg(th.accent)
            };
            let bwid = lbl.chars().count() as u16;
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(lbl, st))).style(st),
                Rect::new(bx, brow, bwid, 1),
            );
            self.deck_hits.push((Rect::new(bx, brow, bwid, 1), hit));
            bx += bwid + 2;
        }

        // ---- Cards: floating inset surface blocks, smart height ----
        let Some(sp) = self.active_space() else {
            return;
        };
        let sp_id = sp.id;
        let tabs = sp.tabs.clone();
        let list_top = area.y + 4;
        let hint_row = brow.saturating_sub(1);
        let list_bottom = hint_row.saturating_sub(1);
        let region = (list_bottom + 1).saturating_sub(list_top);
        // Worst case a card is 2 rows + a 1-row gap = 3 cells (WAITING/DONE); using
        // /2 overestimated capacity so the max-scroll clamp pinned tail cards
        // off-screen and the selection cursor could sit below the fold. /3 is the
        // conservative bound: it may scroll a hair early for 1-row cards but never
        // hides the selected card or strands one.
        let capacity_loose = (region / 3).max(1) as usize;
        // Keep the selected card in view (auto-scroll).
        if sel < ncards {
            if sel < self.deck_scroll {
                self.deck_scroll = sel;
            } else if sel >= self.deck_scroll + capacity_loose {
                self.deck_scroll = sel + 1 - capacity_loose;
            }
        }
        self.deck_scroll = self
            .deck_scroll
            .min(tabs.len().saturating_sub(capacity_loose));
        let start = self.deck_scroll;

        let mut y = list_top;
        let mut shown = 0usize;
        for (ai, t) in tabs.iter().enumerate().skip(start) {
            let selected = ai == sel;
            let act = tab_activity(&self.snap, t);
            let has_preview = matches!(act, Activity::Waiting | Activity::Done);
            let rows = if has_preview { 2 } else { 1 };
            if y + rows - 1 > list_bottom {
                break;
            }
            let (g, color) = self.state_glyph(act);
            let (label, lcol) = match act {
                Activity::Waiting => ("WAITING", th.waiting),
                Activity::Working => ("working", th.working),
                Activity::Done => ("done", th.done_ok),
                Activity::Idle => ("idle", th.idle),
            };
            let pane = self.snap.pane(t.active_pane);
            let loc = pane.map(pane_loc).unwrap_or_default();
            let preview = pane.map(|p| p.preview.clone()).unwrap_or_default();
            let el = self.elapsed_label(t.active_pane);
            let statelbl = if el.is_empty() {
                label.to_string()
            } else {
                format!("{label} {el}")
            };

            // Selected card: raised bg + accent ❯ marker (the app's "selected" cue).
            let sb = Style::default().bg(if selected { th.select_bg } else { th.surface });
            let (marker, mcol) = if selected {
                ("❯", th.accent)
            } else {
                ("▎", color)
            };
            let name = t.name.clone();
            let lead_w = 4usize; // marker + " g "
            let name_w = name.chars().count();
            let right = format!("{statelbl} ");
            let right_w = right.chars().count();
            let room = (cw as usize).saturating_sub(lead_w + name_w + right_w + 4);
            let loc_disp = if loc.is_empty() || room < 3 {
                String::new()
            } else {
                format!("  {}", loc.chars().take(room).collect::<String>())
            };
            let pad =
                (cw as usize).saturating_sub(lead_w + name_w + loc_disp.chars().count() + right_w);
            let line1 = Line::from(vec![
                Span::styled(marker, sb.fg(mcol).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {g} "), sb.fg(color)),
                Span::styled(name, sb.fg(th.bar_active_fg).add_modifier(Modifier::BOLD)),
                Span::styled(loc_disp, sb.fg(th.status_fg)),
                Span::styled(" ".repeat(pad), sb),
                Span::styled(right, sb.fg(lcol).add_modifier(Modifier::BOLD)),
            ]);
            f.render_widget(Paragraph::new(line1).style(sb), Rect::new(cx, y, cw, 1));
            if has_preview {
                let body: String = format!("      {preview}")
                    .chars()
                    .take(cw as usize)
                    .collect();
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        body,
                        Style::default().fg(th.bar_fg),
                    )))
                    .style(sb),
                    Rect::new(cx, y + 1, cw, 1),
                );
            }
            self.deck_hits.push((
                Rect::new(cx, y, cw, rows),
                DeckHit::Tab {
                    space: sp_id,
                    tab: t.id,
                    pane: t.active_pane,
                },
            ));
            y += rows + 1;
            shown += 1;
        }

        // ---- Scroll hint ----
        let below = tabs.len().saturating_sub(start + shown);
        let mut hint: Vec<Span> = Vec::new();
        if start > 0 {
            self.deck_hits.push((
                Rect::new(area.x, hint_row, area.width / 2, 1),
                DeckHit::ScrollUp,
            ));
            hint.push(Span::styled("  ▲ more", Style::default().fg(th.accent)));
        }
        if below > 0 {
            self.deck_hits.push((
                Rect::new(area.x + area.width / 2, hint_row, area.width / 2, 1),
                DeckHit::ScrollDown,
            ));
            let pad = (area.width as usize).saturating_sub(
                hint.iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
                    + format!("▼ {below} more  ").chars().count(),
            );
            hint.push(Span::raw(" ".repeat(pad)));
            hint.push(Span::styled(
                format!("▼ {below} more  "),
                Style::default().fg(th.accent),
            ));
        }
        if !hint.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(hint)).style(bgstyle),
                Rect::new(area.x, hint_row, area.width, 1),
            );
        }
    }

    fn draw_search_bar(&self, f: &mut Frame, area: Rect) {
        let th = &self.cfg.theme;
        let q = self.search.as_deref().unwrap_or("");
        let spans = vec![
            Span::styled(" 🔍 ", Style::default().bg(th.bar_bg).fg(th.accent)),
            Span::styled(
                format!("{q}  "),
                Style::default()
                    .bg(th.bar_bg)
                    .fg(th.bar_active_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "n/N next·prev   esc clear",
                Style::default().bg(th.bar_bg).fg(th.status_fg),
            ),
        ];
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(th.bar_bg)),
            area,
        );
    }

    /// Draw a subtle line in each split gutter so seams read as intentional.
    fn draw_dividers(&self, f: &mut Frame) {
        if !self.cfg.ui.pane_divider || self.zoomed || self.cfg.ui.gutter == 0 {
            return;
        }
        let Some(t) = self.active_tab() else { return };
        let mut segs = Vec::new();
        node_dividers(&t.layout, self.frame.panes, self.cfg.ui.gutter, &mut segs);
        let style = Style::default()
            .fg(self.cfg.theme.border)
            .bg(self.cfg.theme.bg);
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
        let Some(rect) = self
            .pane_rects
            .iter()
            .find(|(p, _)| *p == sel.pane)
            .map(|(_, r)| *r)
        else {
            return;
        };
        let c = self.pane_content_rect(rect);
        if c.width == 0 || c.height == 0 {
            return;
        }
        let ((sr, sc), (er, ec)) = sel.ordered();
        // Flash green briefly right after a copy, else the normal selection tint.
        let copied = self
            .copied_at
            .map(|t| t.elapsed().as_millis() < 1000)
            .unwrap_or(false);
        let bg = if copied {
            self.cfg.theme.done_ok
        } else {
            self.cfg.theme.select_bg
        };
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
                        " ".repeat(
                            (inner.width as usize).saturating_sub(label.chars().count() + 2)
                        )
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
            (
                self.cfg.hint(Action::JumpWaiting),
                "jump to next pane that needs you",
            ),
            (self.cfg.hint(Action::SplitRight), "split pane right"),
            (self.cfg.hint(Action::SplitDown), "split pane down"),
            (self.cfg.hint(Action::ClosePane), "close pane"),
            (self.cfg.hint(Action::NextPane), "focus next pane"),
            (self.cfg.hint(Action::PrevPane), "focus previous pane"),
            (self.cfg.hint(Action::NewTab), "new tab"),
            (self.cfg.hint(Action::NextTab), "next tab"),
            (self.cfg.hint(Action::PrevTab), "previous tab"),
            (self.cfg.hint(Action::NewSpace), "new space"),
            (self.cfg.hint(Action::NextSpace), "next space"),
            (self.cfg.hint(Action::PrevSpace), "previous space"),
            (self.cfg.hint(Action::LastPane), "last pane (jump back)"),
            (self.cfg.hint(Action::LastSpace), "last space (jump back)"),
            (self.cfg.hint(Action::ScrollUp), "scroll history up"),
            (self.cfg.hint(Action::ScrollDown), "scroll history down"),
            (self.cfg.hint(Action::ToggleSidebar), "toggle sidebar"),
            (self.cfg.hint(Action::Zoom), "zoom focused pane"),
            (
                self.cfg.hint(Action::Search),
                "search scrollback (n/N cycle)",
            ),
            (self.cfg.hint(Action::Palette), "command palette"),
            (self.cfg.hint(Action::Deck), "mobile deck view (cards)"),
            ("alt+1..9".into(), "jump to tab"),
            ("enter".into(), "restart a finished pane"),
            (self.cfg.hint(Action::Quit), "quit (daemon keeps running)"),
            ("".into(), ""),
            ("mouse".into(), "click to focus · right-click for menu"),
            ("".into(), "drag pane gutters to resize · wheel scrolls"),
        ];
        let key_line = |k: &str, d: &str| {
            Line::from(vec![
                Span::styled(
                    format!("  {k:>12}  "),
                    Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(d.to_string(), Style::default().fg(th.bar_active_fg)),
            ])
        };
        let mut lines: Vec<Line> = std::iter::once(Line::raw(""))
            .chain(entries.iter().map(|(k, d)| key_line(k, d)))
            .collect();
        // Discoverability: surface plugin/custom command shortcuts + link rules.
        if !self.cfg.commands.is_empty() || self.cfg.links.len() > 1 {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  PLUGINS & CUSTOM",
                Style::default()
                    .fg(th.status_fg)
                    .add_modifier(Modifier::BOLD),
            )));
            for c in &self.cfg.commands {
                let w = match c.placement {
                    Placement::SplitRight => "split →",
                    Placement::SplitDown => "split ↓",
                    Placement::Tab => "new tab",
                    Placement::Popup => "popup",
                };
                lines.push(key_line(
                    &c.binding.compact(),
                    &format!("{} [{w}]", c.cmd.join(" ")),
                ));
            }
            let click = match self.cfg.ui.link_click {
                LinkClick::Ctrl => "⌃click",
                LinkClick::Shift => "⇧click",
                LinkClick::Plain => "click",
            };
            for l in &self.cfg.links {
                lines.push(key_line(
                    click,
                    &format!("{}  →  {}", l.pattern.as_str(), l.run),
                ));
            }
        }
        let w: u16 = 60;
        let h = lines.len() as u16 + 3;
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

    /// Persistent badge at the top-right while the tmux prefix is armed.
    fn draw_prefix_indicator(&self, f: &mut Frame) {
        if !self.prefix_pending {
            return;
        }
        let th = &self.cfg.theme;
        let text = " ‹prefix› ";
        let w = text.chars().count() as u16;
        let row = self.frame.tabs.or(self.frame.header).unwrap_or(0);
        let x = self.size.0.saturating_sub(w);
        let rect = Rect::new(x, row, w.min(self.size.0), 1);
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default()
                    .bg(th.accent)
                    .fg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ))),
            rect,
        );
    }

    /// Draw the floating command popup on top of everything.
    fn draw_popup(&mut self, f: &mut Frame) {
        if self.popup.is_none() {
            return;
        }
        let th = self.cfg.theme.clone();
        let r = self.popup_rect();
        f.render_widget(Clear, r);
        let cols = r.width.saturating_sub(2).max(1);
        let rows = r.height.saturating_sub(2).max(1);
        // Keep the PTY sized to the overlay.
        let title = {
            let p = self.popup.as_mut().unwrap();
            if p.cols != cols || p.rows != rows {
                let _ = p.master.resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                p.parser.set_size(rows, cols);
                p.rows = rows;
                p.cols = cols;
            }
            p.title.clone()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.surface))
            .title(Line::from(Span::styled(
                format!(" {title} "),
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            )))
            .title_bottom(Line::from(Span::styled(
                " ⌃q close ",
                Style::default().fg(th.status_fg),
            )));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let p = self.popup.as_ref().unwrap();
        let screen = p.parser.screen();
        let lines = screen_to_lines(screen, true, false);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(th.surface)),
            inner,
        );
        if !screen.hide_cursor() {
            let (crow, ccol) = screen.cursor_position();
            let cx = inner.x + ccol.min(inner.width.saturating_sub(1));
            let cy = inner.y + crow.min(inner.height.saturating_sub(1));
            f.set_cursor_position((cx, cy));
        }
    }

    fn draw_toast(&self, f: &mut Frame) {
        let Some((msg, at)) = &self.toast else { return };
        let th = &self.cfg.theme;
        let (sw, sh) = self.size;
        // Fade in on arrival and out near expiry via the DIM attribute — the
        // closest a terminal gets to an alpha fade.
        let elapsed = at.elapsed().as_millis();
        let total = self.cfg.ui.toast_seconds as u128 * 1000;
        let fade = elapsed < 160 || elapsed + 450 > total;
        let dim = if fade {
            Modifier::DIM
        } else {
            Modifier::empty()
        };
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
            .border_style(Style::default().fg(th.accent).add_modifier(dim))
            .style(Style::default().bg(th.bar_bg));
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(th.bar_active_fg).add_modifier(dim),
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

        // Mobile deck view: a self-contained screen of cards. Overlays still stack.
        if self.deck_active() {
            self.draw_deck(f);
            self.draw_menu(f);
            self.draw_palette(f);
            self.draw_prompt(f);
            self.draw_toast(f);
            self.draw_popup(f);
            return;
        }

        // Root layer: darkest background, visible as gutters between panes.
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(self.cfg.theme.bg)),
            area,
        );

        if let Some(r) = self.frame.header {
            if self.mobile_focus() {
                self.draw_mobile_header(f, Rect::new(0, r, area.width, 1));
            } else {
                self.draw_header(f, Rect::new(0, r, area.width, 1));
            }
        }
        if let Some(sb) = self.frame.sidebar {
            self.draw_sidebar(f, sb);
        } else if !self.drawer {
            self.sidebar_rows.clear();
        }
        if let Some(r) = self.frame.tabs {
            let m = self.frame.main;
            self.draw_tab_strip(f, Rect::new(m.x, r, m.width, 1));
            // Divider under the tab strip so it reads as its own bar.
            if self.cfg.ui.tab_border && self.frame.panes.y > r + 1 {
                let th = &self.cfg.theme;
                let bar = "─".repeat(m.width as usize);
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        bar,
                        Style::default().fg(th.border).bg(th.bg),
                    ))),
                    Rect::new(m.x, r + 1, m.width, 1),
                );
            }
        }
        self.draw_panes(f);
        self.draw_dividers(f);
        self.draw_selection(f);
        if let Some(r) = self.frame.action {
            self.draw_action_bar(f, Rect::new(0, r, area.width, 1));
        } else {
            self.action_hits.clear();
        }
        if let Some(r) = self.frame.footer {
            // A search bar replaces the footer while a query is active.
            if self.search.is_some() && self.prompt.is_none() {
                self.draw_search_bar(f, Rect::new(0, r, area.width, 1));
            } else {
                self.draw_footer(f, Rect::new(0, r, area.width, 1));
            }
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
        self.draw_palette(f);
        self.draw_prompt(f);
        self.draw_prefix_indicator(f);
        self.draw_toast(f);
        self.draw_popup(f);
    }
}

pub async fn run(initial: Option<String>) -> Result<()> {
    crate::client::init_client_log();
    tracing::info!("ruckus tui starting");
    ensure_daemon().await?;
    let mut cfg = Config::load();
    // Install any config-declared plugins that aren't present yet (fresh machine),
    // then reload so their binds/links merge in. Cheap when nothing's missing.
    if !cfg.plugins.is_empty() && !crate::config::ensure_declared(&cfg.plugins).is_empty() {
        cfg = Config::load();
    }
    let (client, events) = connect().await?;
    let snap = client.snapshot().await?;

    let focused = snap
        .spaces
        .iter()
        .find(|s| s.id == snap.active_space)
        .or(snap.spaces.first())
        .and_then(|s| {
            s.tabs
                .iter()
                .find(|t| t.id == s.active_tab)
                .or(s.tabs.first())
        })
        .map(|t| t.active_pane)
        .unwrap_or(0);

    let sidebar = cfg.ui.sidebar_start_visible;
    let deck_default = cfg.ui.deck;
    let spinner_ms = cfg.ui.spinner_ms;
    let mouse = cfg.ui.mouse;
    let (popup_tx, mut popup_rx) = unbounded_channel::<Vec<u8>>();
    // The local daemon's event stream; a `None` payload means it dropped
    // (restart/upgrade) and we reconnect. Remotes are the daemon's concern now.
    let (mev_tx, mut mev_rx) = unbounded_channel::<Option<ServerMsg>>();
    spawn_forwarder(events, mev_tx.clone());
    // Config-declared remotes are connected by asking the daemon (with live SSH
    // env) once we're up — see below.
    let config_remotes = cfg.remotes.clone();
    let mut app = App {
        cfg,
        client,
        snap,
        views: HashMap::new(),
        focused,
        last_pane: None,
        last_space: None,
        seen: HashSet::new(),
        unread: HashSet::new(),
        flash: HashMap::new(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".to_string()),
        running: true,
        toast: None,
        sidebar,
        zoomed: false,
        was_narrow: false,
        drawer: false,
        help: false,
        prefix_pending: false,
        menu: None,
        prompt: None,
        search: None,
        palette: None,
        palette_hits: Vec::new(),
        deck: deck_default,
        deck_scroll: 0,
        deck_sel: 0,
        deck_hits: Vec::new(),
        drag: None,
        select: None,
        selecting: false,
        copied_at: None,
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
        drag_target: None,
        swap_from: None,
        sidebar_w: None,
        sidebar_resizing: false,
        popup: None,
        popup_tx,
        footer_hits: Vec::new(),
        action_hits: Vec::new(),
        pane_rects: Vec::new(),
        status_cmds: HashMap::new(),
    };
    // Start zoomed on phones / narrow terminals.
    if app.narrow() {
        app.zoomed = true;
        app.was_narrow = true;
    }

    if let Some(target) = initial {
        let pane = resolve_pane(&app.snap, &target)?;
        if let Some((s, t)) = app.locate(pane.id) {
            app.set_active(s, t, pane.id).await;
        }
    }

    enable_raw_mode()?;
    // Bracketed paste lets us receive a paste as one Event::Paste and forward it
    // wrapped, so multi-line pastes go in as text instead of executing line by line.
    if mouse {
        crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
    } else {
        crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste
        )?;
    }
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        hook(info);
    }));

    // Ask the daemon to (re)connect any config-declared remotes, handing it our
    // live SSH env. Idempotent — already-connected ones are a no-op on the daemon.
    for r in &config_remotes {
        app.connect_remote(r.host.clone(), r.args.clone()).await;
    }
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
    let mut status_ticker = tokio::time::interval(Duration::from_secs(5));
    app.refresh_status_cmds().await; // populate #(command) segments up front
    while app.running {
        terminal.draw(|f| app.draw(f))?;
        tokio::select! {
            ev = in_rx.recv() => match ev {
                Some(e) => app.on_term_event(e).await,
                None => app.running = false,
            },
            got = mev_rx.recv() => match got {
                Some(Some(m)) => app.on_server(m).await,
                Some(None) => {
                    // Local daemon dropped (restart/upgrade) — reconnect and
                    // reattach instead of exiting. Panes persist across daemon
                    // restarts under the same ids, so the view comes right back.
                    app.toast("reconnecting…");
                    terminal.draw(|f| app.draw(f))?;
                    if !reconnect(&mut app, &mut in_rx, &mev_tx).await {
                        app.running = false;
                    }
                }
                None => app.running = false,
            },
            bytes = popup_rx.recv() => match bytes {
                Some(b) if b.is_empty() => app.close_popup(),   // reader EOF
                Some(b) => {
                    if let Some(p) = app.popup.as_mut() {
                        p.parser.process(&b);
                    }
                }
                None => {}
            },
            _ = ticker.tick() => app.on_tick(),
            _ = status_ticker.tick() => app.refresh_status_cmds().await,
        }
        while let Ok(b) = popup_rx.try_recv() {
            if b.is_empty() {
                app.close_popup();
            } else if let Some(p) = app.popup.as_mut() {
                p.parser.process(&b);
            }
        }
        while let Ok(e) = in_rx.try_recv() {
            app.on_term_event(e).await;
        }
        while let Ok(Some(m)) = mev_rx.try_recv() {
            app.on_server(m).await;
        }
    }

    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Forward the local daemon's event stream into the loop channel. Sends `None`
/// when the stream ends so the loop knows the daemon dropped and can reconnect.
fn spawn_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    tx: tokio::sync::mpsc::UnboundedSender<Option<ServerMsg>>,
) {
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if tx.send(Some(m)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
}

/// Re-establish the LOCAL daemon connection after it drops. Restarts the daemon
/// if it died, reconnects with backoff, re-fetches state, re-attaches panes, and
/// respawns its forwarder. Returns false only if the user quits / it gives up.
async fn reconnect(
    app: &mut App,
    in_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    mev_tx: &tokio::sync::mpsc::UnboundedSender<Option<ServerMsg>>,
) -> bool {
    for _ in 0..240 {
        // Let the user bail out while we retry.
        while let Ok(ev) = in_rx.try_recv() {
            if let Event::Key(k) = ev {
                let k = normalize_key(&k, app.cfg.ui.mac_option_fallback);
                if app.cfg.action_for(&k) == Some(Action::Quit) {
                    return false;
                }
            }
        }
        if ensure_daemon().await.is_ok() {
            if let Ok((client, events)) = connect().await {
                if let Ok(snap) = client.snapshot().await {
                    app.snap = snap;
                    app.client = client;
                    spawn_forwarder(events, mev_tx.clone());
                    app.views.clear(); // force re-attach of every visible pane
                    app.sync().await;
                    app.toast("🐏 reconnected");
                    return true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
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
        let vars = vec![
            ("title", "claude·3".to_string()),
            ("cwd", "/tmp".to_string()),
        ];
        let spans = template_spans(
            "{icon} {title} · {cwd}",
            &icon,
            &vars,
            Style::default(),
            Color::White,
        );
        assert_eq!(text_of(&spans), "◉ claude·3 · /tmp");
        // unknown tokens render literally rather than vanishing
        let spans = template_spans(
            "{icon} {nope}",
            &icon,
            &vars,
            Style::default(),
            Color::White,
        );
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

    #[test]
    fn link_argv_substitutes_and_keeps_single_args() {
        // ${url}/${match}/${0} = full match; the matched text stays ONE arg even
        // with spaces (substituted after the template is split → no shell-split).
        assert_eq!(
            link_argv("open ${url}", "https://x.dev/a b", &[]),
            vec!["open", "https://x.dev/a b"]
        );
        // ${1}.. = capture groups
        assert_eq!(
            link_argv("gh issue view ${1} --web", "#42", &["42"]),
            vec!["gh", "issue", "view", "42", "--web"]
        );
        assert_eq!(link_argv("echo ${0}", "abc", &[]), vec!["echo", "abc"]);
    }

    #[test]
    fn hl_spans_split_matched_from_unmatched() {
        let base = Style::default();
        // "ag" matched, "ent" not → two spans, text preserved
        let spans = hl_spans("agent", &[0, 1], base, Color::White, Color::Red, false);
        assert_eq!(text_of(&spans), "agent");
        assert_eq!(spans.len(), 2);
        // no matches → a single span
        let spans = hl_spans("agent", &[], base, Color::White, Color::Red, false);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn activity_word_labels() {
        assert_eq!(activity_word(Activity::Waiting), "waiting");
        assert_eq!(activity_word(Activity::Working), "working");
        assert_eq!(activity_word(Activity::Done), "done");
        assert_eq!(activity_word(Activity::Idle), "idle");
    }
}

#[cfg(test)]
mod motion_tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn lerp_endpoints_and_midpoint() {
        let a = Color::Rgb(0, 0, 0);
        let b = Color::Rgb(255, 255, 255);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
        assert_eq!(lerp_color(a, b, 0.5), Color::Rgb(128, 128, 128));
        // clamps out-of-range t
        assert_eq!(lerp_color(a, b, 2.0), b);
        // non-rgb falls back to b
        assert_eq!(lerp_color(Color::Red, b, 0.5), b);
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence_and_ranks() {
        let score = |c, q| fuzzy_indices(c, q).map(|(s, _)| s);
        assert!(score("split pane right", "split").is_some());
        assert!(score("split pane right", "spr").is_some()); // subsequence
        assert!(score("split pane right", "xyz").is_none());
        // contiguous prefix scores higher than a mid-string match
        let a = score("split", "sp").unwrap(); // prefix + contiguous
        let b = score("crispy plum", "sp").unwrap(); // contiguous, mid-string
        assert!(a > b, "prefix {a} should beat mid-string {b}");
        assert_eq!(score("anything", ""), Some(0));
        // the matched char indices come back for highlighting
        assert_eq!(fuzzy_indices("split", "sp").unwrap().1, vec![0, 1]);
    }

    fn pane(id: u64, title: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: title.into(),
            cmd: Vec::new(),
            cwd: String::new(),
            status: PaneStatus::Running,
            activity: Activity::Idle,
            created: 0,
            agent: None,
            preview: String::new(),
            activity_since: 0,
            git_branch: String::new(),
        }
    }
    fn tab(id: u64, name: &str, pane_id: u64) -> TabInfo {
        TabInfo {
            id,
            name: name.into(),
            active_pane: pane_id,
            layout: Node::Leaf { pane: pane_id },
        }
    }
    fn fixture() -> Snapshot {
        let s1 = SpaceInfo {
            id: 1,
            name: "work".into(),
            active_tab: 11,
            tabs: vec![tab(11, "agent", 10), tab(12, "build", 20)],
        };
        let s2 = SpaceInfo {
            id: 2,
            name: "personal".into(),
            active_tab: 21,
            tabs: vec![tab(21, "notes", 30)],
        };
        Snapshot {
            spaces: vec![s1, s2],
            active_space: 1,
            panes: vec![pane(10, "p10"), pane(20, "p20"), pane(30, "p30")],
            ..Default::default()
        }
    }

    #[test]
    fn pane_name_prefers_title_then_cmd() {
        let mut p = pane(1, "my-title");
        assert_eq!(pane_name(Some(&p)), "my-title");
        p.title.clear();
        p.cmd = vec!["claude".into(), "--foo".into()];
        assert_eq!(pane_name(Some(&p)), "claude --foo");
        p.cmd.clear();
        assert_eq!(pane_name(Some(&p)), "pane 1");
        assert_eq!(pane_name(None), "pane");
    }

    #[test]
    fn palette_tree_default_folds_inactive_spaces() {
        let snap = fixture();
        // Mimic Palette::new's default collapse: every space but active + every tab.
        let collapsed: HashSet<u64> = [2u64, 11, 12, 21].into_iter().collect();
        let rows = palette_tree(&snap, "", &collapsed);
        // work (expanded) + its two tabs + personal (collapsed, no children)
        let kinds: Vec<(u8, bool)> = rows.iter().map(|r| (r.depth, r.expanded)).collect();
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0].kind, PKind::Space(1)));
        assert!(rows[0].expanded);
        assert_eq!(rows[1].label, "agent");
        assert_eq!(rows[2].label, "build");
        assert!(matches!(rows[3].kind, PKind::Space(2)));
        assert!(!rows[3].expanded); // personal stays folded
        assert!(!kinds.iter().any(|(d, _)| *d == 2)); // no pane rows (single-pane tabs)
    }

    #[test]
    fn palette_tree_filter_keeps_matches_and_dims_context() {
        let snap = fixture();
        let empty = HashSet::new();
        // Query a tab name: only that branch survives; its space shows as context.
        let rows = palette_tree(&snap, "agent", &empty);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].kind, PKind::Space(1)));
        assert!(rows[0].dim, "space shown only as context is dimmed");
        assert_eq!(rows[1].label, "agent");
        assert!(!rows[1].dim, "the actual match is not dimmed");
    }

    #[test]
    fn palette_tree_filter_by_space_shows_all_its_tabs() {
        let snap = fixture();
        let empty = HashSet::new();
        let rows = palette_tree(&snap, "work", &empty);
        // matching the space name reveals its tabs (dimmed, as context)
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].dim); // "work" itself matched
        assert_eq!(rows[1].label, "agent");
        assert_eq!(rows[2].label, "build");
        assert!(rows[1].dim && rows[2].dim);
    }
}
