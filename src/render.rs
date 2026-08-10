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

/// Encode a mouse-wheel notch as the bytes a mouse-aware app expects, at the
/// 0-based (col,row) within the pane. Wheel-up is button 64, wheel-down 65.
pub fn encode_mouse_wheel(
    up: bool,
    col0: u16,
    row0: u16,
    enc: vt100::MouseProtocolEncoding,
) -> Vec<u8> {
    let btn: u16 = if up { 64 } else { 65 };
    let cx = col0 + 1; // reports are 1-based
    let cy = row0 + 1;
    match enc {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{btn};{cx};{cy}M").into_bytes(),
        // X10 / UTF-8: a printable-offset triplet, capped so it stays one byte.
        _ => {
            let b = |v: u16| (32 + v.min(223)) as u8;
            vec![0x1b, b'[', b'M', (32 + btn) as u8, b(cx), b(cy)]
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_hyperlink_stamped_on_cells() {
        // Our vendored vt100 patch: OSC 8 marks cells with the real URL, so a
        // click resolves the link even when the visible label isn't a URL.
        let mut p = vt100::Parser::new(2, 40, 0);
        p.process(b"\x1b]8;;https://ex.com/x\x07click\x1b]8;;\x07 more");
        let s = p.screen();
        assert_eq!(s.cell(0, 0).unwrap().hyperlink(), Some("https://ex.com/x")); // 'c'
        assert_eq!(s.cell(0, 4).unwrap().hyperlink(), Some("https://ex.com/x")); // 'k'
        assert!(s.cell(0, 5).unwrap().hyperlink().is_none()); // after the close
    }

    #[test]
    fn wheel_encodes_sgr_1based() {
        // wheel-up at content cell (0,0) → button 64 at col 1, row 1, press (M)
        assert_eq!(
            encode_mouse_wheel(true, 0, 0, vt100::MouseProtocolEncoding::Sgr),
            b"\x1b[<64;1;1M".to_vec()
        );
        // wheel-down at (col=4,row=2) → button 65 at col 5, row 3
        assert_eq!(
            encode_mouse_wheel(false, 4, 2, vt100::MouseProtocolEncoding::Sgr),
            b"\x1b[<65;5;3M".to_vec()
        );
    }

    #[test]
    fn wheel_encodes_x10_offset() {
        // X10: ESC [ M, then 32+button, 32+col, 32+row
        assert_eq!(
            encode_mouse_wheel(true, 0, 0, vt100::MouseProtocolEncoding::Default),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }
}
