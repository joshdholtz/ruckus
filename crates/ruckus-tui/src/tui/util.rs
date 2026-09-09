use super::*;

/// Subsequence fuzzy match: a score (rewards early + contiguous matches) plus
/// the char indices in `cand` that matched, for highlighting. `None` if `q`
/// isn't a subsequence of `cand`.
pub(crate) fn fuzzy_indices(cand: &str, q: &str) -> Option<(i32, Vec<usize>)> {
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
pub(crate) fn hl_spans(
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
pub(crate) fn link_argv(run: &str, full: &str, groups: &[&str]) -> Vec<String> {
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
pub(crate) fn pane_name(p: Option<&PaneInfo>) -> String {
    match p {
        Some(p) if !p.title.is_empty() => p.title.clone(),
        Some(p) if !p.cmd.is_empty() => p.cmd.join(" "),
        Some(p) => format!("pane {}", p.id),
        None => "pane".to_string(),
    }
}

/// Linear blend between two colors (RGB only; non-RGB returns `b`).
pub(crate) fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => b,
    }
}

/// Extract `#(command)` segments from a status format string.
pub(crate) fn extract_commands(template: &str) -> Vec<String> {
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
pub(crate) fn hostname() -> String {
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
pub(crate) fn home_relative(path: &str) -> String {
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
pub(crate) fn pane_loc(p: &PaneInfo) -> String {
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
pub(crate) fn copy_to_clipboard(text: &str) {
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

pub(crate) fn agg_activity<I: Iterator<Item = Activity>>(iter: I) -> Activity {
    iter.max_by_key(|a| a.urgency()).unwrap_or(Activity::Idle)
}

pub(crate) fn tab_activity(snap: &Snapshot, tab: &TabInfo) -> Activity {
    let mut leaves = Vec::new();
    tab.layout.leaves(&mut leaves);
    agg_activity(
        leaves
            .iter()
            .filter_map(|p| snap.pane(*p))
            .map(|p| p.activity),
    )
}

pub(crate) fn space_activity(snap: &Snapshot, space: &SpaceInfo) -> Activity {
    agg_activity(space.tabs.iter().map(|t| tab_activity(snap, t)))
}

/// Lowercase one-word label for an activity state (headers, info bars).
pub(crate) fn activity_word(a: Activity) -> &'static str {
    match a {
        Activity::Waiting => "waiting",
        Activity::Working => "working",
        Activity::Done => "done",
        Activity::Idle => "idle",
    }
}

/// Expand a row template like "{icon} {title}" into styled spans.
pub(crate) fn template_spans(
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

pub(crate) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Forward the local daemon's event stream into the loop channel. Sends `None`
/// when the stream ends so the loop knows the daemon dropped and can reconnect.
pub(crate) fn spawn_forwarder(
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
pub(crate) async fn reconnect(
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
