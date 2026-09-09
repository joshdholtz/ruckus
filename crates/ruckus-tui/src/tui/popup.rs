use super::*;

/// A client-local floating command (display-popup style): its own PTY, rendered
/// as a centered overlay that captures all keys until the command exits.
pub(crate) struct Popup {
    pub(crate) parser: vt100::Parser,
    pub(crate) master: Box<dyn portable_pty::MasterPty + Send>,
    pub(crate) writer: Box<dyn std::io::Write + Send>,
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(crate) title: String,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

/// Spawn a command in a fresh PTY, streaming its output to `tx`. Ephemeral —
/// unlike daemon panes, a popup dies with the client.
pub(crate) fn spawn_popup(
    cmd: Vec<String>,
    cwd: String,
    rows: u16,
    cols: u16,
    tx: UnboundedSender<Vec<u8>>,
) -> anyhow::Result<Popup> {
    use anyhow::anyhow;
    use std::io::Read;
    let cmdline = if cmd.is_empty() {
        vec![ruckus_core::protocol::default_shell()]
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
        ruckus_core::protocol::socket_path().display().to_string(),
    );
    builder.env(
        "RUCKUS_DIR",
        ruckus_core::protocol::ruckus_dir().display().to_string(),
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

impl App {
    /// Extract the current selection's text and put it on the clipboard.
    /// Screen (col,row) → (row,col) cell inside the popup's content area, if any.
    pub(crate) fn popup_cell_at(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        let r = self.popup_rect();
        let (ix, iy) = (r.x + 1, r.y + 1); // inside the 1-cell border
        let (cols, rows) = (r.width.saturating_sub(2), r.height.saturating_sub(2));
        if col < ix || row < iy {
            return None;
        }
        let (cx, cy) = (col - ix, row - iy);
        (cx < cols && cy < rows).then_some((cy, cx))
    }

    /// Copy the popup's current text selection to the clipboard.
    pub(crate) fn copy_popup_selection(&mut self) {
        let Some(sel) = self.popup_sel else { return };
        if sel.is_empty() {
            return;
        }
        let Some(p) = self.popup.as_ref() else { return };
        let screen = p.parser.screen();
        let (rows, cols) = screen.size();
        let ((sr, sc), (er, ec)) = sel.ordered();
        let last_row = rows.saturating_sub(1);
        let (sr, er) = (sr.min(last_row), er.min(last_row));
        let sc = sc.min(cols);
        let ec2 = ec.saturating_add(1).min(cols).max(sc);
        let text = screen.contents_between(sr, sc, er, ec2);
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        let n = text.chars().count();
        copy_to_clipboard(&text);
        self.notify(format!("copied {n} chars"));
        self.copied_at = Some(Instant::now());
    }

    /// Centered ~5/6-screen rect for a floating popup.
    pub(crate) fn popup_rect(&self) -> Rect {
        let (w0, h0) = self.size;
        let w = ((w0 as u32 * 5 / 6) as u16).clamp(20, w0);
        let h = ((h0 as u32 * 5 / 6) as u16).clamp(6, h0);
        Rect::new(w0.saturating_sub(w) / 2, h0.saturating_sub(h) / 2, w, h)
    }

    /// Open a command in a floating popup (one at a time).
    pub(crate) fn open_popup(&mut self, cmd: Vec<String>) {
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

    pub(crate) fn close_popup(&mut self) {
        self.popup_sel = None;
        self.popup_selecting = false;
        if let Some(mut p) = self.popup.take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    /// Draw the floating command popup on top of everything.
    pub(crate) fn draw_popup(&mut self, f: &mut Frame) {
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
        let default_fg = if is_light(th.surface) {
            Some(th.bar_active_fg)
        } else {
            None
        };
        let lines = screen_to_lines(screen, true, false, default_fg);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(th.surface)),
            inner,
        );
        // Highlight the active text selection over the rendered content.
        if let Some(sel) = self.popup_sel.filter(|s| !s.is_empty() || self.popup_selecting) {
            let ((sr, sc), (er, ec)) = sel.ordered();
            let buf = f.buffer_mut();
            for gy in sr..=er {
                let (c0, c1) = if sr == er {
                    (sc, ec)
                } else if gy == sr {
                    (sc, inner.width.saturating_sub(1))
                } else if gy == er {
                    (0, ec)
                } else {
                    (0, inner.width.saturating_sub(1))
                };
                for gx in c0..=c1 {
                    let x = inner.x + gx;
                    let y = inner.y + gy;
                    if x < inner.x + inner.width && y < inner.y + inner.height {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_bg(th.select_bg);
                        }
                    }
                }
            }
        }
        if !screen.hide_cursor() {
            let (crow, ccol) = screen.cursor_position();
            let cx = inner.x + ccol.min(inner.width.saturating_sub(1));
            let cy = inner.y + crow.min(inner.height.saturating_sub(1));
            f.set_cursor_position((cx, cy));
        }
    }
}
