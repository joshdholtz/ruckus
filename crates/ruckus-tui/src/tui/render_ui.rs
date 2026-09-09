use super::*;

impl App {
    pub(crate) fn draw_panes(&mut self, f: &mut Frame) {
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
            let bar_h = self.pane_bar_h();
            if !self.cfg.ui.pane_status.is_empty() && !self.mobile_focus() {
                // Configurable per-pane bar (one row per template line).
                let bar_bg = self.flash_bg(*pane, if focused { th.select_bg } else { th.bar_bg });
                for (i, tpl) in self.cfg.ui.pane_status.lines().enumerate() {
                    let spans = self.render_pane_status(*pane, tpl);
                    f.render_widget(
                        Paragraph::new(Line::from(spans)).style(Style::default().bg(bar_bg)),
                        Rect::new(rect.x, rect.y + i as u16, rect.width, 1),
                    );
                }
                content_y += bar_h;
                content_h = content_h.saturating_sub(bar_h);
            } else if titles {
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
                // On a light theme the pane surface is light, so terminal-default
                // (light) text would be invisible — remap it to the theme's body fg.
                let default_fg = if is_light(th.surface) {
                    Some(th.bar_active_fg)
                } else {
                    None
                };
                let mut lines =
                    screen_to_lines(screen, live && !placed_real, dimmed || exited, default_fg);
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
                        .map(|i| ruckus_core::protocol::unix_now().saturating_sub(i.created) < 20)
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

    pub(crate) fn draw_action_bar(&mut self, f: &mut Frame, area: Rect) {
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

    pub(crate) fn draw_search_bar(&self, f: &mut Frame, area: Rect) {
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
    pub(crate) fn draw_dividers(&self, f: &mut Frame) {
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
    pub(crate) fn draw_selection(&self, f: &mut Frame) {
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

    pub(crate) fn draw_menu(&self, f: &mut Frame) {
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

    pub(crate) fn draw_help(&self, f: &mut Frame) {
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

    pub(crate) fn draw_prompt(&self, f: &mut Frame) {
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
    pub(crate) fn draw_prefix_indicator(&self, f: &mut Frame) {
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

    pub(crate) fn draw_toast(&self, f: &mut Frame) {
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

    pub(crate) fn draw(&mut self, f: &mut Frame) {
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
            self.draw_theme_pick(f);
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
        self.draw_theme_pick(f);
        self.draw_prefix_indicator(f);
        self.draw_toast(f);
        self.draw_popup(f);
    }
}
