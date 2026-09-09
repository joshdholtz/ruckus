use super::*;

/// A clickable affordance rendered in the sidebar.
#[derive(Clone, Copy)]
pub(crate) enum SidebarBtn {
    CloseSpace(u64),
}

impl App {
    /// Split-aware sidebar: stacked (single region) or, when `sidebar_split > 0`,
    /// the last section pinned to a fixed bottom region (herdr-style).
    /// Mouse-wheel inside a sidebar region → scroll that region. Returns true if
    /// handled (so it doesn't also scroll a pane).
    pub(crate) fn sidebar_wheel(&mut self, col: u16, row: u16, up: bool) -> bool {
        let Some(sb) = self.frame.sidebar else {
            return false;
        };
        if col < sb.x || col >= sb.x + sb.width {
            return false;
        }
        let hit = self
            .sidebar_scrollbars
            .iter()
            .find(|(_, t, _)| row >= t.y && row < t.y + t.height)
            .map(|(k, _, m)| (k.clone(), *m));
        if let Some((key, max_off)) = hit {
            let cur = self.sidebar_scroll.get(&key).copied().unwrap_or(0);
            let next = if up {
                cur.saturating_sub(3)
            } else {
                (cur + 3).min(max_off)
            };
            self.sidebar_scroll.insert(key, next);
            true
        } else {
            false
        }
    }

    /// Begin (start=true) or continue dragging a sidebar scrollbar thumb, mapping
    /// the row to a scroll offset. Returns true if handled.
    pub(crate) fn sidebar_scrollbar_drag(&mut self, col: u16, row: u16, start: bool) -> bool {
        let entry = if start {
            self.sidebar_scrollbars
                .iter()
                .find(|(_, t, _)| col == t.x && row >= t.y && row < t.y + t.height)
                .cloned()
        } else if let Some(key) = self.sidebar_dragbar.clone() {
            self.sidebar_scrollbars
                .iter()
                .find(|(k, _, _)| *k == key)
                .cloned()
        } else {
            None
        };
        let Some((key, track, max_off)) = entry else {
            return false;
        };
        if max_off == 0 || track.height <= 1 {
            return false;
        }
        let rel = row.saturating_sub(track.y).min(track.height - 1) as usize;
        let off = (rel * max_off) / (track.height as usize - 1);
        self.sidebar_scroll.insert(key.clone(), off.min(max_off));
        if start {
            self.sidebar_dragbar = Some(key);
        }
        true
    }

    pub(crate) fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let sections = self.cfg.ui.sidebar_sections.clone();
        let split = self.cfg.ui.sidebar_split;
        self.sidebar_rows.clear();
        self.sidebar_buttons.clear();
        self.sidebar_scrollbars.clear();
        // On a space switch, reveal the newly-active row for one frame — but only
        // nudge if it's off-screen (see draw_sidebar_region), so clicking a
        // visible row doesn't jerk the scroll.
        if self.snap.active_space != self.last_active_space {
            self.last_active_space = self.snap.active_space;
            self.sidebar_follow_once = true;
        }
        if split <= 0.0 || sections.len() < 2 || area.height < 8 {
            self.draw_sidebar_region(f, area, &sections, true);
            self.sidebar_follow_once = false;
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
        self.sidebar_follow_once = false;
    }

    pub(crate) fn draw_sidebar_region(
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

        // Reserve a 2-col gutter on the right: a scrollbar column + a gap, so the
        // bar never clips row text, never steals row clicks, and keeps a
        // sidebar-bg margin against the pane (preserving the contrast edge).
        let w = (inner.width as usize).saturating_sub(2);
        let key = sections.first().cloned().unwrap_or_default();
        let pre_off = self.sidebar_scroll.get(&key).copied().unwrap_or(0);
        // Map the hovered screen row into build coordinates: rows below the pinned
        // header are shifted up by the scroll offset, so add it back before
        // comparing, or the highlight lands on the wrong row.
        let hover_row = self
            .hover
            .filter(|(c, _)| *c >= area.x && *c < area.x + area.width)
            .map(|(_, r)| r + pre_off as u16);
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

        // Keep the leading chrome (blank + section header) pinned, and scroll
        // only the rows beneath it — so "SPACES"/"AGENTS" stays put while the list
        // moves. `fixed_top` = index of the first clickable row (headers/blanks
        // carry no Target, so they're never in `rows`).
        let h = inner.height as usize;
        let fixed_top = rows
            .first()
            .map(|(y, _)| y.saturating_sub(inner.y) as usize)
            .unwrap_or(0)
            .min(h);
        let total = lines.len();
        let body_total = total.saturating_sub(fixed_top);
        let body_h = h.saturating_sub(fixed_top);
        let mut scrollbar: Option<(usize, usize, usize, usize)> = None; // off, body_total, max_off, fixed_top
        if body_total > body_h && body_h >= 1 {
            let max_off = body_total - body_h;
            // Honor the manual (wheel/drag) offset; default to top.
            let mut off = self.sidebar_scroll.get(&key).copied().unwrap_or(0).min(max_off);
            // Just after a space switch, reveal the active row — but only if it's
            // off-screen, and by the minimum needed (so a click on a visible row
            // leaves the scroll exactly where it was).
            if self.sidebar_follow_once {
                if let Some(fib) = focus_idx.filter(|fi| *fi >= fixed_top).map(|fi| fi - fixed_top) {
                    if fib < off {
                        off = fib;
                    } else if fib >= off + body_h {
                        off = fib + 1 - body_h;
                    }
                }
            }
            off = off.min(max_off);
            self.sidebar_scroll.insert(key.clone(), off);
            if off > 0 {
                let off_u = off as u16;
                let cut = inner.y + fixed_top as u16; // first body row
                lines.drain(fixed_top..fixed_top + off);
                rows.retain(|(ry, _)| *ry < cut || *ry >= cut + off_u);
                rows.iter_mut().for_each(|(ry, _)| {
                    if *ry >= cut {
                        *ry -= off_u;
                    }
                });
                buttons.retain(|(by, _, _)| *by < cut || *by >= cut + off_u);
                buttons.iter_mut().for_each(|(by, _, _)| {
                    if *by >= cut {
                        *by -= off_u;
                    }
                });
            }
            // Scrollbar track spans the body only (below the pinned header), in the
            // reserved gutter one column in from the edge.
            let bar_x = inner.x + inner.width.saturating_sub(2);
            let bar_y = inner.y + fixed_top as u16;
            let bar_h = inner.height.saturating_sub(fixed_top as u16);
            self.sidebar_scrollbars
                .push((key.clone(), Rect::new(bar_x, bar_y, 1, bar_h), max_off));
            scrollbar = Some((off, body_total, max_off, fixed_top));
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
        // ratatui's built-in Scrollbar for the visual; mouse wheel/drag is wired
        // separately (the widget itself isn't interactive).
        if let Some((off, body_total, max_off, fixed_top)) = scrollbar {
            // Scale the offset across the full range (ratatui maps position over
            // content_length-1) so the thumb reaches the bottom at max scroll.
            let pos = if max_off > 0 {
                off * (body_total - 1) / max_off
            } else {
                0
            };
            let body_h = h.saturating_sub(fixed_top);
            let mut state = ScrollbarState::new(body_total)
                .viewport_content_length(body_h)
                .position(pos);
            let bar_y = inner.y + fixed_top as u16;
            let bar_h = inner.height.saturating_sub(fixed_top as u16);
            let bar_area = Rect::new(inner.x, bar_y, inner.width.saturating_sub(1), bar_h);
            // Muted so it never competes with the selected row / content: thumb =
            // dim idle grey, track = darker border.
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(Style::default().fg(th.border))
                .thumb_symbol("▐")
                .thumb_style(Style::default().fg(th.idle));
            f.render_stateful_widget(bar, bar_area, &mut state);
        }
    }
}
