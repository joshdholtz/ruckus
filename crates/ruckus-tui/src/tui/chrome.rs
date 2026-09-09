use super::*;

impl App {
    pub(crate) fn spin(&self) -> String {
        let s = &self.cfg.glyphs.spinner;
        s[self.tick % s.len()].clone()
    }

    /// The working-state indicator (glyph + color), per `ui.working_style`.
    pub(crate) fn working_indicator(&self) -> (String, Color) {
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

    pub(crate) fn glyph(&self, info: Option<&PaneInfo>) -> (String, Color) {
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
    pub(crate) fn state_glyph(&self, a: Activity) -> (String, Color) {
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
    pub(crate) fn flash_bg(&self, pane: u64, base: Color) -> Color {
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
    pub(crate) fn tab_flash_bg(&self, tab: &TabInfo, base: Color) -> Color {
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
    pub(crate) fn static_glyph(&self, a: Activity) -> (String, Color) {
        let th = &self.cfg.theme;
        let g = &self.cfg.glyphs;
        match a {
            Activity::Working => (g.working.clone(), th.working),
            Activity::Waiting => (g.waiting.clone(), th.waiting),
            Activity::Done => (g.done.clone(), th.done_ok),
            Activity::Idle => (g.idle.clone(), th.idle),
        }
    }

    pub(crate) fn hover_at(&self, col_range: &std::ops::Range<u16>, row: u16) -> bool {
        self.hover
            .map(|(c, r)| r == row && col_range.contains(&c))
            .unwrap_or(false)
    }

    /// Compact elapsed since a pane last changed state: "45s" / "12m" / "3h".
    pub(crate) fn elapsed_label(&self, pane: u64) -> String {
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

    pub(crate) fn draw_header(&self, f: &mut Frame, area: Rect) {
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

    pub(crate) fn draw_tab_strip(&mut self, f: &mut Frame, area: Rect) {
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

    pub(crate) fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
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
    pub(crate) async fn refresh_status_cmds(&mut self) {
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

    /// Refresh per-pane `#(command)` output for *visible* panes, in each pane's
    /// cwd. A pane refreshes when: it's new, its cwd changed (a `cd`) — rate-
    /// limited to ~once/3s so rapid cds coalesce — or it's been parked longer
    /// than `pane_status_poll` (0 disables idle re-polls). Called on a short
    /// ticker; only actually spawns processes for panes that are *due*.
    pub(crate) async fn refresh_pane_status_cmds(&mut self) {
        let cmds = extract_commands(&self.cfg.ui.pane_status);
        if cmds.is_empty() {
            self.pane_status_cmds.clear();
            self.pane_cwd_seen.clear();
            self.pane_refreshed_at.clear();
            return;
        }
        const CD_RATE: Duration = Duration::from_secs(3); // min gap between cd-triggered refreshes
        let poll = self.cfg.ui.pane_status_poll;
        let now = Instant::now();
        let panes: Vec<(u64, String)> = self
            .pane_rects
            .iter()
            .filter_map(|(id, _)| self.snap.pane(*id).map(|p| (*id, p.cwd.clone())))
            .collect();
        let visible: std::collections::HashSet<u64> = panes.iter().map(|(id, _)| *id).collect();
        self.pane_status_cmds.retain(|(id, _), _| visible.contains(id));
        self.pane_cwd_seen.retain(|id, _| visible.contains(id));
        self.pane_refreshed_at.retain(|id, _| visible.contains(id));
        for (id, cwd) in panes {
            let last = self.pane_refreshed_at.get(&id).copied();
            let elapsed = last.map(|t| now.duration_since(t));
            let cwd_changed = self.pane_cwd_seen.get(&id).map(|s| s != &cwd).unwrap_or(true);
            let due = match elapsed {
                None => true,                                   // never run yet
                Some(e) if cwd_changed => e >= CD_RATE,         // cd: refresh, rate-limited
                Some(e) => poll > 0 && e >= Duration::from_secs(poll), // parked: idle re-poll
            };
            if !due {
                continue;
            }
            let cd = cwd.replace('\'', "'\\''");
            for c in &cmds {
                let full = format!("cd '{cd}' 2>/dev/null && {c}");
                let out = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::process::Command::new("sh").arg("-c").arg(&full).output(),
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
                self.pane_status_cmds.insert((id, c.clone()), val);
            }
            self.pane_cwd_seen.insert(id, cwd);
            self.pane_refreshed_at.insert(id, now);
        }
    }

    /// Render a status format string (`status_left`/`status_right`) into spans,
    /// expanding {tokens}, `#(command)` output, and `#[color]` markup.
    pub(crate) fn render_status(&self, template: &str) -> Vec<Span<'static>> {
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
        let cmd = |c: &str| -> String { self.status_cmds.get(c).cloned().unwrap_or_default() };
        self.render_template(template, &val, &cmd)
    }

    /// Resolve a per-pane status token for `pane_id`.
    pub(crate) fn pane_token(&self, pane_id: u64, token: &str) -> String {
        let p = self.snap.pane(pane_id);
        match token.split(':').next().unwrap_or(token) {
            "title" | "name" => p.map(|p| p.title.clone()).unwrap_or_default(),
            "cmd" => p.map(|p| p.cmd.join(" ")).unwrap_or_default(),
            "cwd" => p.map(|p| home_relative(&p.cwd)).unwrap_or_default(),
            "id" => pane_id.to_string(),
            "branch" => p.map(|p| p.git_branch.clone()).unwrap_or_default(),
            "agent" => p.and_then(|p| p.agent.clone()).unwrap_or_default(),
            "activity" => p
                .map(|p| {
                    match p.activity {
                        Activity::Working => "working",
                        Activity::Waiting => "waiting",
                        Activity::Idle => "idle",
                        Activity::Done => "done",
                    }
                    .to_string()
                })
                .unwrap_or_default(),
            "host" => hostname(),
            "clock" => {
                let fmt = token.split_once(':').map(|x| x.1).unwrap_or("%H:%M");
                chrono::Local::now().format(fmt).to_string()
            }
            _ => String::new(),
        }
    }

    /// Render one line of a pane's status template.
    pub(crate) fn render_pane_status(&self, pane_id: u64, template: &str) -> Vec<Span<'static>> {
        let val = |token: &str| -> String { self.pane_token(pane_id, token) };
        let cmd = |c: &str| -> String {
            self.pane_status_cmds
                .get(&(pane_id, c.to_string()))
                .cloned()
                .unwrap_or_default()
        };
        self.render_template(template, &val, &cmd)
    }

    /// Shared parser for status templates: `{tokens}` via `val`, `#(cmd)` output
    /// via `cmd`, plus `#[color]` markup, `{|}` dividers and `{sp:N}` spacers.
    pub(crate) fn render_template(
        &self,
        template: &str,
        val: &dyn Fn(&str) -> String,
        cmd: &dyn Fn(&str) -> String,
    ) -> Vec<Span<'static>> {
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
                let mut cmdstr = String::new();
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                    cmdstr.push(c);
                }
                buf.push_str(&cmd(&cmdstr));
            } else if ch == '{' {
                flush(&mut spans, &mut buf, fg);
                let mut token = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    token.push(c);
                }
                match token.split(':').next().unwrap_or(&token) {
                    // Themed divider: `{|}` → a dim │ with a space each side.
                    // `{|:accent}` recolours it.
                    "|" => {
                        let c = token
                            .split_once(':')
                            .map(|x| resolve_color(x.1))
                            .unwrap_or(th.status_fg);
                        spans.push(Span::styled(" │ ".to_string(), Style::default().fg(c)));
                    }
                    // Fixed-width spacer: `{sp}` = 2 cols, `{sp:8}` = 8.
                    "sp" | "gap" => {
                        let n = token
                            .split_once(':')
                            .and_then(|x| x.1.parse::<usize>().ok())
                            .unwrap_or(2);
                        buf.push_str(&" ".repeat(n));
                    }
                    _ => buf.push_str(&val(&token)),
                }
            } else {
                buf.push(ch);
            }
        }
        flush(&mut spans, &mut buf, fg);
        spans
    }

    pub(crate) fn draw_footer_status(&mut self, f: &mut Frame, area: Rect) {
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

    pub(crate) fn draw_footer_hints(&mut self, f: &mut Frame, area: Rect) {
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
}
