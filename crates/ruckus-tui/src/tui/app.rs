use super::*;

impl App {
    pub(crate) fn active_space(&self) -> Option<SpaceInfo> {
        self.snap
            .spaces
            .iter()
            .find(|s| s.id == self.snap.active_space)
            .or(self.snap.spaces.first())
            .cloned()
    }

    pub(crate) fn active_tab(&self) -> Option<TabInfo> {
        let s = self.active_space()?;
        s.tabs
            .iter()
            .find(|t| t.id == s.active_tab)
            .or(s.tabs.first())
            .cloned()
    }

    pub(crate) fn visible(&self) -> Vec<u64> {
        let mut v = Vec::new();
        if let Some(t) = self.active_tab() {
            t.layout.leaves(&mut v);
        }
        v
    }

    pub(crate) fn locate(&self, pane: u64) -> Option<(u64, u64)> {
        for s in &self.snap.spaces {
            for t in &s.tabs {
                if t.layout.contains(pane) {
                    return Some((s.id, t.id));
                }
            }
        }
        None
    }

    pub(crate) fn narrow(&self) -> bool {
        self.cfg.ui.narrow_below > 0 && self.size.0 < self.cfg.ui.narrow_below
    }

    pub(crate) fn sidebar_shown(&self) -> bool {
        self.sidebar && !self.narrow()
    }

    /// Focused pane wants a touch action/triage bar. This is a phone affordance
    /// only — on desktop you drive a waiting/exited pane straight from the
    /// keyboard (type into it, Enter restarts a dead pane), so no bottom bar.
    pub(crate) fn action_bar_kind(&self) -> Option<&'static str> {
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

    pub(crate) fn compute_frame(&self) -> FrameLayout {
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

    pub(crate) fn compute_rects(&self) -> Vec<(u64, Rect)> {
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

    pub(crate) fn toast(&mut self, _msg: impl Into<String>) {
        // Automatic toasts stay silent — the unsolicited popups (errors,
        // reconnecting, activity) were more annoying than useful; pane/deck state
        // colors already show what's happening. User-initiated confirmations go
        // through `notify` instead (see `copy_selection`).
    }

    /// A popup for something the user just did on purpose (e.g. copy) — worth an
    /// explicit confirmation, unlike the automatic toasts above.
    pub(crate) fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Rows the per-pane top bar occupies: the pane_status template's line count
    /// if set, else 1 when pane_titles is on, else 0.
    pub(crate) fn pane_bar_h(&self) -> u16 {
        if self.mobile_focus() {
            0 // the mobile header names the pane; no per-pane bar
        } else if self.cfg.ui.pane_status.is_empty() {
            self.cfg.ui.pane_titles as u16
        } else {
            self.cfg.ui.pane_status.lines().count().max(1) as u16
        }
    }

    /// The content sub-rect of a pane rect (inside title bar + padding).
    pub(crate) fn pane_content_rect(&self, rect: Rect) -> Rect {
        let title = self.pane_bar_h();
        let pad = self.cfg.ui.pane_padding;
        Rect::new(
            rect.x + pad,
            rect.y + title + pad,
            rect.width.saturating_sub(2 * pad),
            rect.height.saturating_sub(title + 2 * pad),
        )
    }

    /// Absolute (col,row) -> (pane id, content cell (row,col)) if it lands in a pane.
    pub(crate) fn cell_at(&self, col: u16, row: u16) -> Option<(u64, (u16, u16))> {
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
    pub(crate) fn cell_in_pane(&self, pane: u64, col: u16, row: u16) -> Option<(u16, u16)> {
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

    pub(crate) fn copy_selection(&mut self) {
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

    pub(crate) fn pane_size(&self, rect: Rect) -> (u16, u16) {
        let title: u16 = self.pane_bar_h();
        let pad = self.cfg.ui.pane_padding;
        let rows = rect.height.saturating_sub(title + 2 * pad).max(1);
        let cols = rect.width.saturating_sub(2 * pad).max(1);
        (rows, cols)
    }

    pub(crate) async fn sync(&mut self) {
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
            // Fire-and-forget: we don't use the reply, and awaiting it per stale
            // pane (each up to the request timeout for a remote pane) could stall
            // the event loop when leaving a multi-pane remote space.
            self.client.notify(Request::Detach { pane: p });
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
                    // Fire-and-forget (reply unused) so resizing with remote panes
                    // on-screen doesn't stall per pane.
                    self.client.notify(Request::Resize { pane, rows, cols });
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
    pub(crate) fn host_of(&self, id: u64) -> &str {
        let origin = remote::origin_of(id);
        if origin == remote::LOCAL {
            return "";
        }
        self.snap
            .remote_hosts
            .get(&origin.to_string())
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Send a request to the local daemon. Ids are origin-encoded; the daemon
    /// routes remote ones to the owning daemon and re-prefixes the reply, so the
    /// client neither strips nor re-tags anything.
    pub(crate) async fn route(&self, req: Request) -> anyhow::Result<ServerMsg> {
        // Bound every request so a remote-routed op (split/close/etc. forwarded to
        // a mirrored box) that never replies can't freeze the TUI's event loop —
        // it degrades to an error toast instead of a locked, un-typeable UI.
        // Local ops return in well under this.
        match tokio::time::timeout(Duration::from_secs(6), self.client.request(req)).await {
            Ok(res) => res,
            Err(_) => Err(anyhow::anyhow!("request timed out (remote unreachable?)")),
        }
    }

    /// The client's live SSH env, handed to the daemon so its detached `ssh` can
    /// authenticate as the user (agent auth / hardware-key touch are agent-side).
    pub(crate) fn ssh_env() -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::new();
        for k in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
            if let Ok(v) = std::env::var(k) {
                env.insert(k.to_string(), v);
            }
        }
        env
    }

    /// Ask the daemon to connect a remote (runtime or config), passing live SSH env.
    pub(crate) async fn connect_remote(&mut self, host: String, args: Vec<String>) {
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

    pub(crate) async fn set_active(&mut self, space: u64, tab: u64, pane: u64) {
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

    pub(crate) async fn goto_pane(&mut self, pane: u64) {
        if let Some((s, t)) = self.locate(pane) {
            self.set_active(s, t, pane).await;
        }
    }

    pub(crate) async fn handle_sidebar_target(&mut self, target: Target) {
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
    pub(crate) fn sidebar_width(&self) -> u16 {
        self.sidebar_w.unwrap_or(self.cfg.ui.sidebar_width)
    }

    /// Resize the sidebar so its edge follows the cursor column, then refit panes.
    pub(crate) async fn resize_sidebar(&mut self, col: u16) {
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
    pub(crate) fn persist_sidebar_width(&self) {
        let Some(w) = self.sidebar_w else { return };
        let path = ruckus_core::config::ensure_config_file();
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

    pub(crate) fn drawer_width(&self) -> u16 {
        self.sidebar_width().min(self.size.0.saturating_sub(4))
    }

    pub(crate) fn drawer_rect(&self) -> Rect {
        let dw = self.drawer_width();
        let x = match self.cfg.ui.sidebar_pos {
            SidebarPos::Left => 0,
            SidebarPos::Right => self.size.0.saturating_sub(dw),
        };
        Rect::new(x, self.frame.body.y, dw, self.frame.body.height)
    }

    pub(crate) fn scroll_by(&mut self, pane: u64, delta: isize) {
        if let Some(v) = self.views.get_mut(&pane) {
            v.scroll = (v.scroll as isize + delta).clamp(0, 10_000) as usize;
            v.parser.set_scrollback(v.scroll);
        }
    }

    pub(crate) fn attention(&self) -> Vec<&PaneInfo> {
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
    pub(crate) fn agent_rows(&self) -> Vec<(u64, String, String)> {
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

    pub(crate) fn mark_seen(&mut self) {
        // Viewing a pane clears its unread badge.
        self.unread.remove(&self.focused);
        if let Some(p) = self.snap.pane(self.focused) {
            if p.activity == Activity::Done {
                self.seen.insert(p.id);
            }
        }
    }

    pub(crate) fn tab_unread(&self, t: &TabInfo) -> bool {
        let mut leaves = Vec::new();
        t.layout.leaves(&mut leaves);
        leaves.iter().any(|p| self.unread.contains(p))
    }

    pub(crate) fn space_unread(&self, s: &SpaceInfo) -> bool {
        s.tabs.iter().any(|t| self.tab_unread(t))
    }

    /// The cwd to seed a new pane with: the reference pane's working directory
    /// (so a split/new tab opens where you already are), else ruckus's launch dir.
    pub(crate) fn seed_cwd(&self, from: u64) -> String {
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
    pub(crate) fn link_at(
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
    pub(crate) fn run_link(&mut self, argv: &[String], matched: &str) {
        let Some((prog, args)) = argv.split_first() else {
            return;
        };
        let _ = std::process::Command::new(prog)
            .args(args)
            .current_dir(self.seed_cwd(self.focused)) // so `gh` etc. see the right repo
            .env("RUCKUS_SOCK", ruckus_core::protocol::socket_path())
            .env("RUCKUS_DIR", ruckus_core::protocol::ruckus_dir())
            .env("RUCKUS_MATCH", matched)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.notify(format!("↗ {matched}"));
    }

    pub(crate) async fn run_command_bind(&mut self, cb: CommandBind) {
        self.open_placement(cb.cmd, cb.placement).await;
    }

    /// Open `cmd` in a split / tab / popup, seeded with the focused pane's cwd.
    pub(crate) async fn open_placement(&mut self, cmd: Vec<String>, placement: Placement) {
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

    pub(crate) async fn split_action(&mut self, pane: u64, dir: Dir) {
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

    pub(crate) async fn new_tab_action(&mut self, name: Option<String>) {
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

    pub(crate) async fn new_space_action(&mut self, name: Option<String>) {
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
    pub(crate) fn open_prompt(&mut self, kind: PromptKind) {
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
    pub(crate) fn run_search(&mut self, older: bool) {
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
    pub(crate) async fn submit_prompt(&mut self) {
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

    pub(crate) async fn send_bytes(&mut self, bytes: &[u8]) {
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

    pub(crate) async fn run_chip(&mut self, chip: ChipAction) {
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

    pub(crate) async fn close_pane_action(&mut self, pane: u64) {
        if let Err(e) = self.route(Request::ClosePane { pane }).await {
            self.toast(e.to_string());
        }
    }

    pub(crate) async fn do_action(&mut self, a: Action) {
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
            Action::Theme => self.open_theme_pick(),
        }
    }

    pub(crate) async fn restart_action(&mut self, pane: u64) {
        match self.route(Request::Restart { pane }).await {
            Ok(_) => {
                self.seen.remove(&pane);
                self.views.remove(&pane); // fresh attach picks up seeded scrollback
                self.sync().await;
            }
            Err(e) => self.toast(e.to_string()),
        }
    }

    pub(crate) async fn run_menu_item(&mut self, action: MenuAction, target: MenuTarget) {
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
}
