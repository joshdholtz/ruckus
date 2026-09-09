use super::*;

/// Every action the command palette can run, with a human label.
pub(crate) const PALETTE_ITEMS: &[(Action, &str)] = &[
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
    (Action::Theme, "change theme (pick + live preview)"),
    (Action::ToggleSidebar, "toggle sidebar"),
    (Action::ShowHelp, "keyboard help"),
    (Action::Quit, "quit (daemon keeps running)"),
];

/// The command palette is a choose-tree navigator: an indented Space → Tab →
/// Pane tree you fuzzy-filter and jump into. Typing `>` switches to a flat
/// command list. Navigation is arrows / Ctrl-n·p (typed keys feed the filter).
#[derive(Clone)]
pub(crate) struct Palette {
    pub(crate) query: String,
    /// Selected index into the current visible rows.
    pub(crate) sel: usize,
    /// Scroll offset (top visible row).
    pub(crate) scroll: usize,
    /// Space/tab ids whose children are folded away. Defaults collapse every
    /// space but the active one, and every tab (panes hidden until expanded).
    pub(crate) collapsed: HashSet<u64>,
}

impl Palette {
    pub(crate) fn new(snap: &Snapshot) -> Palette {
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
    pub(crate) fn is_commands(&self) -> bool {
        self.query.starts_with('>')
    }
    /// The active filter text (without the `>` mode prefix), lowercased.
    pub(crate) fn filter(&self) -> String {
        self.query
            .strip_prefix('>')
            .unwrap_or(&self.query)
            .trim()
            .to_lowercase()
    }
}

/// What a visible palette row targets.
#[derive(Clone, Copy)]
pub(crate) enum PKind {
    Space(u64),
    Jump { space: u64, tab: u64, pane: u64 },
    Cmd(Action),
}

/// A rendered palette row, rebuilt from the snapshot + query each frame.
pub(crate) struct PRow {
    pub(crate) kind: PKind,
    pub(crate) depth: u8,
    pub(crate) glyph: (String, Color),
    pub(crate) label: String,
    pub(crate) meta: String,
    /// Char indices in `label` that matched the query (for highlighting).
    pub(crate) hl: Vec<usize>,
    pub(crate) expandable: bool,
    pub(crate) expanded: bool,
    /// Shown only as context for a matching descendant/ancestor — render muted.
    pub(crate) dim: bool,
}

/// The pure structural result of the tree navigator: which rows are visible,
/// their depth, fold state, and match highlights — no styling/time. Extracted
/// so the filter/fold logic is unit-testable independent of the App.
pub(crate) struct PTreeRow {
    pub(crate) kind: PKind,
    pub(crate) depth: u8,
    pub(crate) label: String,
    pub(crate) hl: Vec<usize>,
    pub(crate) expandable: bool,
    pub(crate) expanded: bool,
    pub(crate) dim: bool,
}

/// Build the Space → Tab → Pane navigator rows for `q` + `collapsed`. Panes
/// appear only under split tabs; while filtering we force-expand along matches
/// and dim rows shown purely as context. Pure over the snapshot.
pub(crate) fn palette_tree(snap: &Snapshot, q: &str, collapsed: &HashSet<u64>) -> Vec<PTreeRow> {
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

/// Modal theme picker: arrow/j-k through the list, live-previewing each theme on
/// the whole UI; Enter persists (`[theme].preset`), Esc reverts to `original`.
pub(crate) struct ThemePick {
    pub(crate) names: Vec<String>,
    pub(crate) sel: usize,
    pub(crate) original: Theme,
}

impl App {
    pub(crate) fn open_palette(&mut self) {
        self.palette = Some(Palette::new(&self.snap));
    }

    /// Open the theme picker: built-ins + user themes, cursor on the active theme,
    /// remembering the live palette so Esc can revert.
    pub(crate) fn open_theme_pick(&mut self) {
        let mut names: Vec<String> = ruckus_core::config::THEME_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect();
        names.extend(ruckus_core::config::list_user_themes());
        // Current preset from config.toml so the cursor lands on the active theme.
        let current = std::fs::read_to_string(ruckus_core::config::ensure_config_file())
            .ok()
            .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
            .and_then(|d| {
                d.get("theme")
                    .and_then(|t| t.get("preset"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        let sel = current
            .and_then(|c| names.iter().position(|n| *n == c))
            .unwrap_or(0);
        self.theme_pick = Some(ThemePick {
            names,
            sel,
            original: self.cfg.theme.clone(),
        });
        self.preview_theme();
    }

    /// Apply the highlighted theme to the live UI (preview only — not persisted).
    pub(crate) fn preview_theme(&mut self) {
        let Some(tp) = &self.theme_pick else { return };
        if let Some(name) = tp.names.get(tp.sel) {
            if let Some(t) = ruckus_core::config::resolve_theme(name) {
                self.cfg.theme = t;
            }
        }
    }

    /// Move the picker cursor by `delta` (wrapping) and preview.
    pub(crate) fn theme_pick_move(&mut self, delta: i32) {
        if let Some(tp) = self.theme_pick.as_mut() {
            let len = tp.names.len() as i32;
            if len > 0 {
                tp.sel = (((tp.sel as i32 + delta) % len + len) % len) as usize;
            }
        }
        self.preview_theme();
    }

    /// Commit the highlighted theme: persist `[theme].preset`, keep it applied live.
    pub(crate) fn theme_pick_commit(&mut self) {
        let Some(tp) = self.theme_pick.take() else {
            return;
        };
        let Some(name) = tp.names.get(tp.sel).cloned() else {
            return;
        };
        if let Some(t) = ruckus_core::config::resolve_theme(&name) {
            self.cfg.theme = t;
        }
        let path = ruckus_core::config::ensure_config_file();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() {
                if !doc.contains_key("theme") {
                    doc["theme"] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                doc["theme"]["preset"] = toml_edit::value(name.as_str());
                let _ = std::fs::write(&path, doc.to_string());
            }
        }
        self.toast(format!("theme: {name}"));
    }

    /// Close the picker, reverting the live preview to what it was before.
    pub(crate) fn theme_pick_cancel(&mut self) {
        if let Some(tp) = self.theme_pick.take() {
            self.cfg.theme = tp.original;
        }
    }

    /// Build the visible palette rows for the current query + fold state.
    pub(crate) fn palette_rows(&self) -> Vec<PRow> {
        let Some(pal) = &self.palette else {
            return Vec::new();
        };
        if pal.is_commands() {
            return self.palette_command_rows(&pal.filter());
        }
        self.palette_tree_rows(&pal.filter(), &pal.collapsed)
    }

    /// Flat command list (the `>` mode), fuzzy-ranked with match highlights.
    pub(crate) fn palette_command_rows(&self, q: &str) -> Vec<PRow> {
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
    pub(crate) fn palette_tree_rows(&self, q: &str, collapsed: &HashSet<u64>) -> Vec<PRow> {
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
    pub(crate) fn palette_fold(&mut self, collapse: bool) {
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

    pub(crate) async fn palette_run(&mut self) {
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

    pub(crate) fn draw_palette(&mut self, f: &mut Frame) {
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

    /// The theme picker popup: a bordered list with a live colour swatch per row.
    /// Selection is a subtle `›` marker + bold name (no heavy highlight bar).
    pub(crate) fn draw_theme_pick(&self, f: &mut Frame) {
        let Some(tp) = &self.theme_pick else { return };
        let th = &self.cfg.theme;
        let w: u16 = 40;
        let h: u16 = tp.names.len() as u16 + 4;
        let x = self.size.0.saturating_sub(w) / 2;
        let y = self.size.1.saturating_sub(h) / 3;
        let r = Rect::new(x, y, w.min(self.size.0), h.min(self.size.1));
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.accent))
            .style(Style::default().bg(th.sidebar_bg))
            .title(Line::from(Span::styled(
                " theme ",
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines: Vec<Line> = Vec::new();
        for (i, name) in tp.names.iter().enumerate() {
            let selected = i == tp.sel;
            let sw = ruckus_core::config::resolve_theme(name).unwrap_or_default();
            let mut spans = vec![
                Span::styled(
                    if selected { " › " } else { "   " },
                    Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{name:<12} "),
                    if selected {
                        Style::default()
                            .fg(th.bar_active_fg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(th.bar_fg)
                    },
                ),
            ];
            for c in [sw.accent, sw.working, sw.waiting, sw.done_ok, sw.done_err] {
                spans.push(Span::styled("█", Style::default().fg(c)));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  ↑/↓ preview · enter set · esc cancel",
            Style::default().fg(th.status_fg),
        )));
        f.render_widget(Paragraph::new(lines), inner);
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
