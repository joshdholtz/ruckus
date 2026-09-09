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
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use ruckus_core::client::{connect, ensure_daemon, resolve_pane, Client};
use ruckus_core::config::{
    normalize_key, Action, BarPos, CommandBind, Config, FooterMode, LinkClick, Placement,
    SidebarPos, Theme, ToastPos, WorkingStyle,
};
use ruckus_core::layout::{
    area_at_path, find_border, node_at_path_mut, node_dividers, node_rects, split_chunks,
};
use ruckus_core::protocol::*;
use ruckus_core::remote;
use crate::render::{encode_key, encode_mouse_wheel, is_light, screen_to_lines};

mod app;
mod chrome;
mod deck;
mod input;
mod palette;
mod popup;
mod render_ui;
mod sidebar;
mod util;

use deck::*;
use input::*;
use palette::*;
use popup::*;
use sidebar::*;
use util::*;

/// Below this width the footer switches to compact tap-first chips.
pub(crate) const FOOTER_COMPACT: u16 = 70;

/// How long a sidebar/title row stays flashed after an activity change.
pub(crate) const FLASH_MS: u128 = 900;

pub(crate) struct PaneView {
    pub(crate) parser: vt100::Parser,
    pub(crate) scroll: usize,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

#[derive(Clone, Copy)]
pub(crate) enum Target {
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
    Pane(u64),
}

/// A NEEDS-YOU sidebar row: (pane id, template vars, state glyph).
pub(crate) type AttnRow = (u64, Vec<(&'static str, String)>, (String, Color));

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PromptKind {
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
pub(crate) struct Prompt {
    pub(crate) kind: PromptKind,
    pub(crate) label: &'static str,
    pub(crate) buffer: String,
}

/// Tap targets on the triage action bar (waiting / exited / narrow).
#[derive(Clone)]
pub(crate) enum ChipAction {
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
pub(crate) struct FrameLayout {
    pub(crate) header: Option<u16>,
    pub(crate) footer: Option<u16>,
    /// Row of big tappable chips (approve / restart) above the footer.
    pub(crate) action: Option<u16>,
    pub(crate) tabs: Option<u16>,
    /// Full-width region between the horizontal bars.
    pub(crate) body: Rect,
    pub(crate) sidebar: Option<Rect>,
    pub(crate) main: Rect,
    /// Main minus the tab strip: where panes render.
    pub(crate) panes: Rect,
}

pub(crate) struct App {
    pub(crate) cfg: Config,
    /// The one connection to the LOCAL daemon. Remote mirrors live in the daemon
    /// now (the hybrid hub): it merges their spaces into the snapshot and routes
    /// requests by the id's origin, so the client sends origin-encoded ids as-is.
    pub(crate) client: Client,
    pub(crate) snap: Snapshot,
    pub(crate) views: HashMap<u64, PaneView>,
    pub(crate) focused: u64,
    /// Previously-focused pane / previously-active space, for "jump back".
    pub(crate) last_pane: Option<u64>,
    pub(crate) last_space: Option<u64>,
    pub(crate) seen: HashSet<u64>,
    /// Panes that changed to a notable state (finished / needs input) while
    /// unfocused. Cleared when you view the pane. Drives the "unread" badge.
    pub(crate) unread: HashSet<u64>,
    /// pane id -> when its activity last changed, for the brief row-flash motion.
    pub(crate) flash: HashMap<u64, Instant>,
    pub(crate) cwd: String,
    pub(crate) running: bool,
    pub(crate) toast: Option<(String, Instant)>,
    pub(crate) sidebar: bool,
    pub(crate) zoomed: bool,
    /// Last-frame narrow state — used to auto-zoom when the window shrinks.
    pub(crate) was_narrow: bool,
    pub(crate) drawer: bool,
    pub(crate) help: bool,
    pub(crate) prefix_pending: bool,
    pub(crate) menu: Option<Menu>,
    pub(crate) prompt: Option<Prompt>,
    /// Active scrollback search query (focused pane); drives n/N and highlight.
    pub(crate) search: Option<String>,
    /// Command palette / choose-tree navigator.
    pub(crate) palette: Option<Palette>,
    /// Clickable palette rows: (rect, visible-row index).
    pub(crate) palette_hits: Vec<(Rect, usize)>,
    /// Active theme picker modal (live preview + persist on Enter).
    pub(crate) theme_pick: Option<ThemePick>,
    /// Touch-first deck view (big cards) — the mobile home screen. Gated by narrow().
    pub(crate) deck: bool,
    pub(crate) deck_scroll: usize,
    /// Keyboard selection cursor over the deck's focusable items: 0..n = cards,
    /// then the +tab and +space buttons. Touch taps sync it too.
    pub(crate) deck_sel: usize,
    /// Tap regions in the deck: (rect, what it does).
    pub(crate) deck_hits: Vec<(Rect, DeckHit)>,
    pub(crate) drag: Option<Drag>,
    pub(crate) select: Option<Sel>,
    pub(crate) selecting: bool,
    /// Drag-to-select-and-copy inside an open popup (the `pane` field is unused).
    pub(crate) popup_sel: Option<Sel>,
    pub(crate) popup_selecting: bool,
    /// When the last selection was copied — flashes the selection green briefly
    /// as confirmation (toasts are gone), then clears it.
    pub(crate) copied_at: Option<Instant>,
    pub(crate) hover: Option<(u16, u16)>,
    pub(crate) tick: usize,
    pub(crate) size: (u16, u16),
    pub(crate) frame: FrameLayout,
    pub(crate) sidebar_rows: Vec<(u16, Target)>,
    pub(crate) sidebar_buttons: Vec<(u16, std::ops::Range<u16>, SidebarBtn)>,
    pub(crate) tab_hits: Vec<(Option<u64>, std::ops::Range<u16>)>,
    pub(crate) tab_close_hits: Vec<(u64, std::ops::Range<u16>)>,
    /// Reorder-drag state: a tab or space being dragged in the strip/sidebar.
    pub(crate) tab_drag: Option<u64>,
    pub(crate) space_drag: Option<u64>,
    /// Last reorder target sent during the active tab/space drag — dedups the
    /// per-mouse-move MoveTab/MoveSpace round-trips so a drag can't flood the socket.
    pub(crate) drag_target: Option<u64>,
    /// Alt+drag a pane onto another to swap them.
    pub(crate) swap_from: Option<u64>,
    /// Live sidebar width from dragging its edge; None = use the configured width.
    pub(crate) sidebar_w: Option<u16>,
    /// True while dragging the sidebar's edge to resize it.
    pub(crate) sidebar_resizing: bool,
    /// Manual scroll offset per sidebar region (keyed by the region's first
    /// section), combined with active-row follow. Absent = top.
    pub(crate) sidebar_scroll: HashMap<String, usize>,
    /// Scrollbar tracks drawn this frame — (region key, track rect, max offset)
    /// — for wheel + drag hit-testing.
    pub(crate) sidebar_scrollbars: Vec<(String, Rect, usize)>,
    /// Region whose scrollbar thumb is currently being dragged.
    pub(crate) sidebar_dragbar: Option<String>,
    /// Last active space id — used to detect a space switch.
    pub(crate) last_active_space: u64,
    /// Set for one frame after the active space changes: reveal the newly-active
    /// row *only if it's off-screen* (minimal nudge), without disturbing scroll
    /// when the clicked row was already visible.
    pub(crate) sidebar_follow_once: bool,
    /// A floating command popup (display-popup style), if one is open.
    pub(crate) popup: Option<Popup>,
    /// Channel a popup's reader thread pushes output to.
    pub(crate) popup_tx: UnboundedSender<Vec<u8>>,
    pub(crate) footer_hits: Vec<(Action, std::ops::Range<u16>)>,
    /// Triage chips: (action, row, col range).
    pub(crate) action_hits: Vec<(ChipAction, u16, std::ops::Range<u16>)>,
    pub(crate) pane_rects: Vec<(u64, Rect)>,
    /// Cached output of `#(command)` status segments, refreshed on an interval.
    pub(crate) status_cmds: HashMap<String, String>,
    /// Cached output of per-pane `#(command)` segments (keyed by pane id + the
    /// command), run in each pane's cwd for the per-pane status bar.
    pub(crate) pane_status_cmds: HashMap<(u64, String), String>,
    /// Cwd a pane's commands were last run in (to detect `cd`).
    pub(crate) pane_cwd_seen: HashMap<u64, String>,
    /// When a pane's commands last ran (rate-limit after cd + idle re-poll).
    pub(crate) pane_refreshed_at: HashMap<u64, Instant>,
}

impl App {
    pub(crate) async fn on_server(&mut self, msg: ServerMsg) {
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
    pub(crate) async fn reload_config(&mut self) {
        let was_mouse = self.cfg.ui.mouse;
        self.cfg = Config::load();
        // Pick up any newly-declared plugins (cheap when all are already present).
        if !self.cfg.plugins.is_empty()
            && !ruckus_core::config::ensure_declared(&self.cfg.plugins).is_empty()
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
        let specs: Vec<ruckus_core::config::RemoteSpec> = self.cfg.remotes.clone();
        for spec in specs {
            self.connect_remote(spec.host, spec.args).await;
        }
        self.sync().await;
        self.toast("config reloaded");
    }

    pub(crate) fn on_tick(&mut self) {
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
}

pub async fn run(initial: Option<String>) -> Result<()> {
    ruckus_core::client::init_client_log();
    tracing::info!("ruckus tui starting");
    ensure_daemon().await?;
    let mut cfg = Config::load();
    // Install any config-declared plugins that aren't present yet (fresh machine),
    // then reload so their binds/links merge in. Cheap when nothing's missing.
    if !cfg.plugins.is_empty() && !ruckus_core::config::ensure_declared(&cfg.plugins).is_empty() {
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
        theme_pick: None,
        deck: deck_default,
        deck_scroll: 0,
        deck_sel: 0,
        deck_hits: Vec::new(),
        drag: None,
        select: None,
        selecting: false,
        popup_sel: None,
        popup_selecting: false,
        copied_at: None,
        hover: None,
        tick: 0,
        size: crossterm::terminal::size().unwrap_or((80, 24)),
        frame: FrameLayout::default(),
        sidebar_rows: Vec::new(),
        sidebar_buttons: Vec::new(),
        sidebar_scroll: HashMap::new(),
        sidebar_scrollbars: Vec::new(),
        sidebar_dragbar: None,
        last_active_space: 0,
        sidebar_follow_once: false,
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
        pane_status_cmds: HashMap::new(),
        pane_cwd_seen: HashMap::new(),
        pane_refreshed_at: HashMap::new(),
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
        // Restore ALL modes we set (incl. bracketed paste) so a crash never leaves
        // the terminal unable to paste/select/click.
        let _ = crossterm::execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
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
    // Short decision tick: cheap (only spawns for panes whose cwd changed or
    // whose idle-poll window elapsed — see refresh_pane_status_cmds).
    let mut pane_status_ticker = tokio::time::interval(Duration::from_secs(2));
    app.refresh_status_cmds().await; // populate #(command) segments up front
    app.refresh_pane_status_cmds().await;
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
            _ = pane_status_ticker.tick() => app.refresh_pane_status_cmds().await,
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
