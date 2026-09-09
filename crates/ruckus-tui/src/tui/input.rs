use super::*;

/// What a context menu acts on.
#[derive(Clone, Copy)]
pub(crate) enum MenuTarget {
    Pane(u64),
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
}

#[derive(Clone)]
pub(crate) struct Menu {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) target: MenuTarget,
    pub(crate) items: Vec<(&'static str, MenuAction)>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MenuAction {
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

pub(crate) struct Drag {
    pub(crate) tab: u64,
    pub(crate) path: Vec<usize>,
    pub(crate) index: usize,
    pub(crate) dir: Dir,
}

/// A text selection within one pane, in that pane's content-cell coordinates
/// (row, col). `anchor` is where the drag began, `head` where it is now.
#[derive(Clone, Copy)]
pub(crate) struct Sel {
    pub(crate) pane: u64,
    pub(crate) anchor: (u16, u16),
    pub(crate) head: (u16, u16),
}

impl Sel {
    /// (start, end) ordered top-to-bottom, left-to-right.
    pub(crate) fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

impl App {
    pub(crate) async fn on_key(&mut self, ev: KeyEvent) {
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

        // Theme picker: arrow / j-k preview the whole UI, Enter commits, Esc reverts.
        if self.theme_pick.is_some() {
            let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
            match ev.code {
                KeyCode::Esc => self.theme_pick_cancel(),
                KeyCode::Enter => self.theme_pick_commit(),
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => self.theme_pick_move(-1),
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => self.theme_pick_move(1),
                KeyCode::Char('p') if ctrl => self.theme_pick_move(-1),
                KeyCode::Char('n') if ctrl => self.theme_pick_move(1),
                _ => {}
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

    pub(crate) fn pane_at(&self, col: u16, row: u16) -> Option<u64> {
        self.pane_rects
            .iter()
            .find(|(_, r)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
            .map(|(p, _)| *p)
    }

    pub(crate) fn menu_rect(&self, m: &Menu) -> Rect {
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

    pub(crate) fn border_hit(&self, col: u16, row: u16) -> Option<(Vec<usize>, usize, Dir)> {
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

    pub(crate) fn apply_drag(&mut self, col: u16, row: u16) {
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

    pub(crate) async fn finish_drag(&mut self) {
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

    pub(crate) async fn on_mouse(&mut self, ev: MouseEvent) {
        let (col, row) = (ev.column, ev.row);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        // A popup owns the screen — no pane interaction behind it, but you can
        // still drag-select its text and copy on release.
        if self.popup.is_some() {
            match ev.kind {
                MouseEventKind::Moved => self.hover = Some((col, row)),
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.cfg.ui.mouse_select {
                        if let Some(cell) = self.popup_cell_at(col, row) {
                            self.popup_sel = Some(Sel {
                                pane: 0,
                                anchor: cell,
                                head: cell,
                            });
                            self.popup_selecting = true;
                        }
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) if self.popup_selecting => {
                    if let Some(cell) = self.popup_cell_at(col, row) {
                        if let Some(s) = self.popup_sel.as_mut() {
                            s.head = cell;
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) if self.popup_selecting => {
                    self.popup_selecting = false;
                    match self.popup_sel {
                        Some(sel) if !sel.is_empty() => self.copy_popup_selection(),
                        _ => self.popup_sel = None,
                    }
                }
                _ => {}
            }
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
                if self.sidebar_dragbar.is_some() {
                    self.sidebar_scrollbar_drag(col, row, false);
                } else if self.sidebar_resizing {
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
                self.sidebar_dragbar = None;
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
                // Grabbing a sidebar scrollbar thumb starts a scroll-drag.
                if self.sidebar_scrollbar_drag(col, row, true) {
                    return;
                }
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
            MouseEventKind::ScrollUp => {
                if !self.sidebar_wheel(col, row, true) {
                    self.pane_wheel(col, row, true).await;
                }
            }
            MouseEventKind::ScrollDown => {
                if !self.sidebar_wheel(col, row, false) {
                    self.pane_wheel(col, row, false).await;
                }
            }
            _ => {}
        }
    }

    /// Route a wheel notch over a pane. If the app inside enabled mouse tracking
    /// (claude, vim, htop…) forward the event so *it* scrolls; if it's just on
    /// the alternate screen (a pager) translate the wheel to arrow keys; only a
    /// plain shell falls through to ruckus's own scrollback.
    pub(crate) async fn pane_wheel(&mut self, col: u16, row: u16, up: bool) {
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

    pub(crate) async fn on_term_event(&mut self, ev: Event) {
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
    pub(crate) async fn on_paste(&mut self, text: String) {
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
        // A popup (scratch shell / tool) owns input while open — paste into it.
        if let Some(p) = self.popup.as_mut() {
            use std::io::Write;
            let bracketed = p.parser.screen().bracketed_paste();
            let body = text.replace("\r\n", "\r").replace('\n', "\r");
            let mut data = Vec::new();
            if bracketed {
                data.extend_from_slice(b"\x1b[200~");
            }
            data.extend_from_slice(body.as_bytes());
            if bracketed {
                data.extend_from_slice(b"\x1b[201~");
            }
            let _ = p.writer.write_all(&data);
            let _ = p.writer.flush();
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
}
