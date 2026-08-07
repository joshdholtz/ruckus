use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn conv_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Render a vt100 screen into ratatui lines. `cursor` draws the cursor cell
/// inverted; `dim` renders everything dimmed (for unfocused panes).
pub fn screen_to_lines(screen: &vt100::Screen, cursor: bool, dim: bool) -> Vec<Line<'static>> {
    let (rows, cols) = screen.size();
    let cur = if cursor && !screen.hide_cursor() {
        Some(screen.cursor_position())
    } else {
        None
    };
    let mut lines = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols as usize);
        let mut c = 0;
        while c < cols {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                c += 1;
                continue;
            }
            let mut text = cell.contents();
            if text.is_empty() {
                text = " ".to_string();
            }
            let mut style = Style::default();
            if let Some(fg) = conv_color(cell.fgcolor()) {
                style = style.fg(fg);
            }
            if let Some(bg) = conv_color(cell.bgcolor()) {
                style = style.bg(bg);
            }
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if cur == Some((r, c)) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if dim {
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled(text, style));
            c += 1;
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Encode a key event as the bytes a terminal would send to the PTY.
pub fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let lc = c.to_ascii_lowercase();
                match lc {
                    'a'..='z' => out.push(lc as u8 - b'a' + 1),
                    ' ' | '@' => out.push(0),
                    '[' => out.push(0x1b),
                    '\\' => out.push(0x1c),
                    ']' => out.push(0x1d),
                    '^' => out.push(0x1e),
                    '_' => out.push(0x1f),
                    _ => return None,
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend(b"\x1b[A"),
        KeyCode::Down => out.extend(b"\x1b[B"),
        KeyCode::Right => out.extend(b"\x1b[C"),
        KeyCode::Left => out.extend(b"\x1b[D"),
        KeyCode::Home => out.extend(b"\x1b[H"),
        KeyCode::End => out.extend(b"\x1b[F"),
        KeyCode::PageUp => out.extend(b"\x1b[5~"),
        KeyCode::PageDown => out.extend(b"\x1b[6~"),
        KeyCode::Delete => out.extend(b"\x1b[3~"),
        KeyCode::Insert => out.extend(b"\x1b[2~"),
        KeyCode::F(n) => out.extend(match n {
            1 => b"\x1bOP".as_slice(),
            2 => b"\x1bOQ".as_slice(),
            3 => b"\x1bOR".as_slice(),
            4 => b"\x1bOS".as_slice(),
            5 => b"\x1b[15~".as_slice(),
            6 => b"\x1b[17~".as_slice(),
            7 => b"\x1b[18~".as_slice(),
            8 => b"\x1b[19~".as_slice(),
            9 => b"\x1b[20~".as_slice(),
            10 => b"\x1b[21~".as_slice(),
            11 => b"\x1b[23~".as_slice(),
            12 => b"\x1b[24~".as_slice(),
            _ => return None,
        }),
        _ => return None,
    }
    Some(out)
}
