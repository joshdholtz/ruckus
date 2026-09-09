use super::*;

/// A tappable region in the mobile deck view.
#[derive(Clone, Copy)]
pub(crate) enum DeckHit {
    Space(u64),
    Tab { space: u64, tab: u64, pane: u64 },
    NewTab,
    NewSpace,
    Jump,
    ScrollUp,
    ScrollDown,
    Menu,
}

impl App {
    /// The deck (mobile card home) shows only when narrow and the toggle is on.
    pub(crate) fn deck_active(&self) -> bool {
        self.deck && self.narrow()
    }

    /// Viewing a single pane on mobile (entered from the deck): strip the tab
    /// strip / footer / pane-title chrome so the pane is near-fullscreen. Only
    /// when the deck is the home to return to (☰ goes back).
    pub(crate) fn mobile_focus(&self) -> bool {
        self.cfg.ui.deck && self.narrow() && !self.deck
    }

    /// Point the deck cursor at the active tab's card (when returning to the deck).
    pub(crate) fn sync_deck_sel(&mut self) {
        if let Some(sp) = self.active_space() {
            if let Some(i) = sp.tabs.iter().position(|t| t.id == sp.active_tab) {
                self.deck_sel = i;
            }
        }
    }

    /// Move the active space by `dir` (deck h/l) and reset the card cursor.
    pub(crate) async fn deck_switch_space(&mut self, dir: i32) {
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
    pub(crate) async fn deck_activate(&mut self, idx: usize) {
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

    /// One clean info row for the mobile focus view (no back button — that lives
    /// in the bottom command bar now): name · loc … state elapsed / ↓ live.
    pub(crate) fn draw_mobile_header(&self, f: &mut Frame, area: Rect) {
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

    /// The touch-first mobile deck: big state-colored cards for each tab in the
    /// current space, attention-sorted. Tap a card to enter it fullscreen.
    pub(crate) fn draw_deck(&mut self, f: &mut Frame) {
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
}
