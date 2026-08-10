use std::collections::HashMap;

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde::Deserialize;

use crate::protocol::ruckus_dir;

/// Everything a user can rebind. Names match the keys in [keys] of config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    SplitRight,
    SplitDown,
    ClosePane,
    NextPane,
    PrevPane,
    NewTab,
    NextTab,
    PrevTab,
    NewSpace,
    NextSpace,
    PrevSpace,
    ScrollUp,
    ScrollDown,
    ToggleSidebar,
    JumpWaiting,
    ShowHelp,
    Zoom,
    Search,
    Palette,
    Deck,
    /// Jump back to the previously-focused pane (tmux `prefix ;`).
    LastPane,
    /// Jump back to the previously-active space (tmux last-window).
    LastSpace,
    /// Prompt for a host and mirror its ruckus over SSH (ephemeral).
    ConnectRemote,
    /// Disconnect the remote whose space you're currently on.
    DisconnectRemote,
    /// Open the theme picker (built-ins + ~/.ruckus/themes) with live preview.
    Theme,
}

pub const ACTIONS: &[(Action, &str, &[&str])] = &[
    (Action::Quit, "quit", &["alt-q", "ctrl-q"]),
    (Action::SplitRight, "split_right", &["alt-v"]),
    (Action::SplitDown, "split_down", &["alt-s"]),
    (Action::ClosePane, "close_pane", &["alt-x"]),
    (Action::NextPane, "next_pane", &["alt-o"]),
    (Action::PrevPane, "prev_pane", &["alt-i"]),
    (Action::NewTab, "new_tab", &["alt-t"]),
    (Action::NextTab, "next_tab", &["alt-]"]),
    (Action::PrevTab, "prev_tab", &["alt-["]),
    (Action::NewSpace, "new_space", &["alt-n"]),
    (Action::NextSpace, "next_space", &["alt-."]),
    (Action::PrevSpace, "prev_space", &["alt-,"]),
    (Action::ScrollUp, "scroll_up", &["alt-pageup"]),
    (Action::ScrollDown, "scroll_down", &["alt-pagedown"]),
    (Action::ToggleSidebar, "toggle_sidebar", &["alt-b"]),
    (Action::JumpWaiting, "jump_waiting", &["alt-a"]),
    (Action::ShowHelp, "show_help", &["alt-/"]),
    (Action::Zoom, "zoom", &["alt-z"]),
    (Action::Search, "search", &["alt-f"]),
    (Action::Palette, "palette", &["alt-p"]),
    (Action::Deck, "deck", &["alt-d"]),
    (Action::LastPane, "last_pane", &["alt-;"]),
    (Action::LastSpace, "last_space", &["alt-l"]),
    (Action::ConnectRemote, "connect_remote", &[]),
    (Action::DisconnectRemote, "disconnect_remote", &[]),
    (Action::Theme, "theme", &[]),
];

/// macOS terminals without "Option as Meta" type a special character instead of
/// sending alt+key. Map those characters back to the key they came from (US layout).
pub fn mac_option_char(c: char) -> Option<char> {
    Some(match c {
        'å' => 'a',
        '∫' => 'b',
        'ç' => 'c',
        '∂' => 'd',
        'ƒ' => 'f',
        '©' => 'g',
        '˙' => 'h',
        '∆' => 'j',
        '˚' => 'k',
        '¬' => 'l',
        'µ' => 'm',
        'ø' => 'o',
        'π' => 'p',
        'œ' => 'q',
        '®' => 'r',
        'ß' => 's',
        '†' => 't',
        '√' => 'v',
        '∑' => 'w',
        '≈' => 'x',
        '¥' => 'y',
        'Ω' => 'z',
        '≤' => ',',
        '≥' => '.',
        '“' => '[',
        '‘' => ']',
        '–' => '-',
        '≠' => '=',
        '…' => ';',
        'æ' => '\'',
        '«' => '\\',
        '÷' => '/',
        '¡' => '1',
        '™' => '2',
        '£' => '3',
        '¢' => '4',
        '∞' => '5',
        '§' => '6',
        '¶' => '7',
        '•' => '8',
        'ª' => '9',
        _ => return None,
    })
}

/// Rewrite an Option-typed special character into the alt+key it stands for.
pub fn normalize_key(ev: &KeyEvent, enabled: bool) -> KeyEvent {
    let mut ev = *ev;
    if !enabled {
        return ev;
    }
    if let KeyCode::Char(c) = ev.code {
        if !ev.modifiers.contains(KeyModifiers::ALT) {
            if let Some(base) = mac_option_char(c) {
                ev.code = KeyCode::Char(base);
                ev.modifiers |= KeyModifiers::ALT;
            }
        }
    }
    ev
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Binding {
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let code_eq = match (self.code, ev.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        code_eq && ev.modifiers.contains(self.mods)
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl".to_string());
        }
        if self.mods.contains(KeyModifiers::ALT) {
            parts.push("alt".to_string());
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            parts.push("shift".to_string());
        }
        parts.push(match self.code {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::PageUp => "pgup".to_string(),
            KeyCode::PageDown => "pgdn".to_string(),
            other => format!("{other:?}").to_lowercase(),
        });
        parts.join("+")
    }

    /// Compact glyph form for tight UI: ⌃⌥⇧ + key, e.g. "⌥z", "⌃b".
    pub fn compact(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push('⌃');
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push('⌥');
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push('⇧');
        }
        s.push_str(&match self.code {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::PageUp => "pgup".to_string(),
            KeyCode::PageDown => "pgdn".to_string(),
            other => format!("{other:?}").to_lowercase(),
        });
        s
    }
}

pub fn parse_binding(spec: &str) -> Result<Binding> {
    let mut mods = KeyModifiers::NONE;
    let mut code = None;
    for part in spec.split('-') {
        match part.to_lowercase().as_str() {
            "ctrl" | "c" => mods |= KeyModifiers::CONTROL,
            "alt" | "opt" | "meta" | "m" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "enter" => code = Some(KeyCode::Enter),
            "tab" => code = Some(KeyCode::Tab),
            "space" => code = Some(KeyCode::Char(' ')),
            "esc" => code = Some(KeyCode::Esc),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "pageup" | "pgup" => code = Some(KeyCode::PageUp),
            "pagedown" | "pgdn" => code = Some(KeyCode::PageDown),
            "home" => code = Some(KeyCode::Home),
            "end" => code = Some(KeyCode::End),
            f if f.starts_with('f') && f.len() > 1 => {
                let n: u8 = f[1..].parse().map_err(|_| anyhow!("bad key: {spec}"))?;
                code = Some(KeyCode::F(n));
            }
            c if c.chars().count() == 1 => code = Some(KeyCode::Char(c.chars().next().unwrap())),
            other => return Err(anyhow!("unknown key part '{other}' in '{spec}'")),
        }
    }
    Ok(Binding {
        code: code.ok_or_else(|| anyhow!("no key in '{spec}'"))?,
        mods,
    })
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub accent: Color,
    pub border: Color,
    pub bg: Color,
    pub surface: Color,
    pub bar_bg: Color,
    pub bar_fg: Color,
    pub bar_active_fg: Color,
    pub status_fg: Color,
    pub sidebar_bg: Color,
    pub select_bg: Color,
    pub working: Color,
    pub waiting: Color,
    pub idle: Color,
    pub done_ok: Color,
    pub done_err: Color,
    /// Optional accent for mirrored *remote* space rows in the sidebar. `None`
    /// (the default) leaves them looking like local rows (still `host:`-tagged);
    /// set `theme.remote = "#rrggbb"` to colour them distinctly.
    pub remote: Option<Color>,
}

const fn rgb(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

impl Default for Theme {
    fn default() -> Self {
        theme_preset("macchiato").expect("default preset exists")
    }
}

/// Built-in theme names, in the order `ruckus theme list` shows them.
pub const THEME_NAMES: &[&str] = &[
    "macchiato",
    "latte",
    "gruvbox",
    "nord",
    "tokyonight",
    "dracula",
    "rosepine",
];

/// A named built-in palette. Fields map: accent, border, bg, surface, bar_bg,
/// bar_fg, bar_active_fg, status_fg, sidebar_bg, select_bg, working, waiting,
/// idle, done_ok, done_err.
pub fn theme_preset(name: &str) -> Option<Theme> {
    let t = |accent,
             border,
             bg,
             surface,
             bar_bg,
             bar_fg,
             bar_active_fg,
             status_fg,
             sidebar_bg,
             select_bg,
             working,
             waiting,
             idle,
             done_ok,
             done_err| Theme {
        accent: rgb(accent),
        border: rgb(border),
        bg: rgb(bg),
        surface: rgb(surface),
        bar_bg: rgb(bar_bg),
        bar_fg: rgb(bar_fg),
        bar_active_fg: rgb(bar_active_fg),
        status_fg: rgb(status_fg),
        sidebar_bg: rgb(sidebar_bg),
        select_bg: rgb(select_bg),
        working: rgb(working),
        waiting: rgb(waiting),
        idle: rgb(idle),
        done_ok: rgb(done_ok),
        done_err: rgb(done_err),
        remote: None,
    };
    Some(
        match name.to_lowercase().replace(['-', '_', ' '], "").as_str() {
            "macchiato" | "default" | "catppuccin" => t(
                0xf5a97f, 0x3b4058, 0x0f1117, 0x1c2030, 0x161925, 0x8f93a2, 0xcad3f5, 0x6e738d,
                0x161925, 0x2e3348, 0x8aadf4, 0xeed49f, 0x5b6078, 0xa6da95, 0xed8796,
            ),
            "latte" => t(
                0xfe640b, 0xbcc0cc, 0xeff1f5, 0xe6e9ef, 0xdce0e8, 0x6c6f85, 0x4c4f69, 0x8c8fa1,
                0xdce0e8, 0xccd0da, 0x1e66f5, 0xdf8e1d, 0x9ca0b0, 0x40a02b, 0xd20f39,
            ),
            "gruvbox" => t(
                0xfe8019, 0x504945, 0x1d2021, 0x282828, 0x1d2021, 0x928374, 0xebdbb2, 0x7c6f64,
                0x1d2021, 0x3c3836, 0x83a598, 0xfabd2f, 0x665c54, 0xb8bb26, 0xfb4934,
            ),
            "nord" => t(
                0x88c0d0, 0x434c5e, 0x2e3440, 0x3b4252, 0x2e3440, 0x616e88, 0xeceff4, 0x4c566a,
                0x2e3440, 0x434c5e, 0x81a1c1, 0xebcb8b, 0x4c566a, 0xa3be8c, 0xbf616a,
            ),
            "tokyonight" => t(
                0xff9e64, 0x3b4261, 0x1a1b26, 0x24283b, 0x16161e, 0x565f89, 0xc0caf5, 0x545c7e,
                0x16161e, 0x2f334d, 0x7aa2f7, 0xe0af68, 0x565f89, 0x9ece6a, 0xf7768e,
            ),
            "dracula" => t(
                0xff79c6, 0x44475a, 0x282a36, 0x1e2029, 0x191a21, 0x6272a4, 0xf8f8f2, 0x6272a4,
                0x191a21, 0x44475a, 0xbd93f9, 0xf1fa8c, 0x6272a4, 0x50fa7b, 0xff5555,
            ),
            "rosepine" => t(
                0xebbcba, 0x403d52, 0x191724, 0x1f1d2e, 0x15131f, 0x6e6a86, 0xe0def4, 0x6e6a86,
                0x15131f, 0x26233a, 0x31748f, 0xf6c177, 0x6e6a86, 0x9ccfd8, 0xeb6f92,
            ),
            _ => return None,
        },
    )
}

/// Directory holding user-defined theme files (`~/.ruckus/themes/<name>.toml`).
pub fn themes_dir() -> std::path::PathBuf {
    ruckus_dir().join("themes")
}

/// Overlay hex-string colour keys from a flat map onto `theme`. Recognised keys
/// match the `[theme]` block: accent, border, bg, surface, bar_bg, bar_fg,
/// bar_active_fg, status_fg, sidebar_bg, select_bg, working, waiting, idle,
/// done_ok, done_err, and the optional remote.
fn apply_theme_map(theme: &mut Theme, map: &HashMap<String, String>) {
    fn hex(map: &HashMap<String, String>, k: &str) -> Option<Color> {
        map.get(k).and_then(|s| parse_hex(s))
    }
    if let Some(c) = hex(map, "accent") { theme.accent = c; }
    if let Some(c) = hex(map, "border") { theme.border = c; }
    if let Some(c) = hex(map, "bg") { theme.bg = c; }
    if let Some(c) = hex(map, "surface") { theme.surface = c; }
    if let Some(c) = hex(map, "bar_bg") { theme.bar_bg = c; }
    if let Some(c) = hex(map, "bar_fg") { theme.bar_fg = c; }
    if let Some(c) = hex(map, "bar_active_fg") { theme.bar_active_fg = c; }
    if let Some(c) = hex(map, "status_fg") { theme.status_fg = c; }
    if let Some(c) = hex(map, "sidebar_bg") { theme.sidebar_bg = c; }
    if let Some(c) = hex(map, "select_bg") { theme.select_bg = c; }
    if let Some(c) = hex(map, "working") { theme.working = c; }
    if let Some(c) = hex(map, "waiting") { theme.waiting = c; }
    if let Some(c) = hex(map, "idle") { theme.idle = c; }
    if let Some(c) = hex(map, "done_ok") { theme.done_ok = c; }
    if let Some(c) = hex(map, "done_err") { theme.done_err = c; }
    if let Some(c) = hex(map, "remote") { theme.remote = Some(c); }
}

/// Resolve a theme by name: a built-in preset first, otherwise a user theme file
/// at `~/.ruckus/themes/<name>.toml`. User files are a flat list of `key = "#hex"`
/// lines and may set `extends = "<builtin>"` (alias `preset`) to inherit a base
/// palette, then override individual colours.
pub fn resolve_theme(name: &str) -> Option<Theme> {
    if let Some(t) = theme_preset(name) {
        return Some(t);
    }
    load_user_theme(name)
}

fn load_user_theme(name: &str) -> Option<Theme> {
    // Bare file name only — no path traversal.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let text = std::fs::read_to_string(themes_dir().join(format!("{name}.toml"))).ok()?;
    let map: HashMap<String, String> = toml::from_str(&text).ok()?;
    // Base palette: `extends`/`preset` names a built-in; macchiato default otherwise.
    let mut theme = map
        .get("extends")
        .or_else(|| map.get("preset"))
        .and_then(|b| theme_preset(b))
        .unwrap_or_default();
    apply_theme_map(&mut theme, &map);
    Some(theme)
}

/// User theme names (file stems) found in `~/.ruckus/themes`, sorted.
pub fn list_user_themes() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(themes_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("toml") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarPos {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarPos {
    Top,
    Bottom,
    Off,
}

/// What the footer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterMode {
    /// Command hint chips (the classic footer).
    Help,
    /// Customizable status bar (clock, names, counts…).
    Status,
    /// Status bar normally; command hints while the prefix is armed.
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPos {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// How a "working" pane is indicated in the dots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingStyle {
    /// Animated braille spinner (default).
    Spinner,
    /// Steady dot whose color gently pulses.
    Pulse,
    /// Steady colored dot, no motion.
    Dot,
}

#[derive(Debug, Clone)]
pub struct Glyphs {
    pub waiting: String,
    pub done: String,
    pub idle: String,
    pub working: String,
    pub focus: String,
    pub spinner: Vec<String>,
}

impl Default for Glyphs {
    fn default() -> Self {
        Glyphs {
            waiting: "◉".to_string(),
            done: "◍".to_string(),
            idle: "○".to_string(),
            working: "●".to_string(),
            focus: "▎".to_string(),
            spinner: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiConfig {
    pub sidebar_pos: SidebarPos,
    pub sidebar_start_visible: bool,
    pub sidebar_width: u16,
    pub sidebar_sections: Vec<String>,
    /// 0 = sections stacked top-to-bottom. >0 splits the sidebar: the last
    /// section is pinned to the bottom `sidebar_split` fraction of the height,
    /// the rest fill the top (herdr-style fixed agents panel).
    pub sidebar_split: f64,
    /// Blank lines inserted between space groups in the sidebar (0 = dense).
    pub sidebar_row_gap: u16,
    /// Show an accent marker on the active sidebar row.
    pub sidebar_marker: bool,
    /// Show 1-based jump numbers (alt+1..9) on tabs — in the strip and sidebar.
    pub tab_numbers: bool,
    /// List each space's tabs under it in the sidebar (false = spaces only).
    pub sidebar_tabs: bool,
    /// How a working pane is shown: spinner | pulse | dot.
    pub working_style: WorkingStyle,
    /// Milliseconds of output silence before a pane drops working -> idle/waiting.
    pub activity_quiet_ms: u64,
    /// Opt-in: read OSC 133 shell-integration marks (FinalTerm FTCS) for exact
    /// command start/finish, overriding the output heuristic. Needs shell integration.
    pub detect_osc133: bool,
    /// Opt-in: identify the agent in each pane by inspecting its foreground
    /// process (like tmux's pane_current_command), so agents started inside a
    /// shell still show up in the AGENTS list.
    pub detect_foreground: bool,
    /// Foreground commands treated as "agents" (populate the AGENTS list). Keeps
    /// transient commands like git/ls out of it. Empty = any non-shell command.
    pub agent_commands: Vec<String>,
    /// Blank rows above the top bar / tab strip.
    pub top_margin: u16,
    /// Horizontal padding inside each tab "pill" in the strip.
    pub tab_pad: u16,
    /// Draw a subtle divider line in the gutter between split panes.
    pub pane_divider: bool,
    /// Dimmed second line under each space row (highlight spans both). Empty = single line.
    pub space_subtitle: String,
    /// Dimmed second line under each tab row (highlight spans both). Empty = single line.
    pub tab_subtitle: String,
    pub gutter: u16,
    pub pane_padding: u16,
    pub pane_titles: bool,
    /// Sidebar auto-collapses below this width; 0 = never.
    pub narrow_below: u16,
    pub spinner_ms: u64,
    pub toast_pos: ToastPos,
    pub toast_seconds: u64,
    pub header: BarPos,
    pub footer: BarPos,
    /// What the footer renders: help hints, a status bar, or auto (status +
    /// which-key on prefix).
    pub footer_mode: FooterMode,
    /// tmux-style status format strings (tokens: {clock} {space} {tab} {tabs}
    /// {panes} {agents} {needs} {host} {cwd}; color markup: `#[accent]…#[]`).
    pub status_left: String,
    pub status_right: String,
    pub tab_strip: bool,
    /// Draw a divider line under the tab strip so it reads as its own bar.
    pub tab_border: bool,
    pub mouse: bool,
    /// Use the touch-first deck view (big cards) when the screen is narrow.
    pub deck: bool,
    /// Drag to select text in a pane and copy it (OSC 52 + local clipboard).
    pub mouse_select: bool,
    pub mac_option_fallback: bool,
    /// What click fires a link handler: ctrl | shift | plain.
    pub link_click: LinkClick,
    pub space_row: String,
    pub tab_row: String,
    pub queue_row: String,
    /// How a mirrored *remote* space's name is decorated in the sidebar. Tokens:
    /// `{host}` (the ssh host you connected) and `{name}` (the space name). The
    /// default glyph + prefix make remote spaces obvious; set to `"{name}"` to
    /// hide the distinction, or customise freely.
    pub remote_label: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            sidebar_pos: SidebarPos::Left,
            sidebar_start_visible: true,
            sidebar_width: 26,
            sidebar_sections: vec!["needs_you".to_string(), "spaces".to_string()],
            sidebar_split: 0.0,
            sidebar_row_gap: 1,
            sidebar_marker: true,
            tab_numbers: true,
            sidebar_tabs: true,
            working_style: WorkingStyle::Spinner,
            activity_quiet_ms: 900,
            detect_osc133: false,
            detect_foreground: false,
            agent_commands: [
                "claude",
                "codex",
                "aider",
                "goose",
                "opencode",
                "amp",
                "gemini",
                "cursor-agent",
                "cline",
                "cody",
                "continue",
                "q",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            top_margin: 0,
            tab_pad: 1,
            pane_divider: false,
            space_subtitle: String::new(),
            tab_subtitle: "{cmd}".to_string(),
            gutter: 1,
            pane_padding: 0,
            pane_titles: true,
            narrow_below: 70,
            spinner_ms: 120,
            toast_pos: ToastPos::BottomRight,
            toast_seconds: 4,
            header: BarPos::Top,
            footer: BarPos::Bottom,
            footer_mode: FooterMode::Auto,
            status_left: "{space} › {tab}".to_string(),
            status_right: "#[accent]⚡{agents}#[] ◉{needs}  #[bar_active_fg]{clock:%H:%M}"
                .to_string(),
            tab_strip: true,
            tab_border: false,
            mouse: true,
            deck: true,
            mouse_select: true,
            mac_option_fallback: true,
            link_click: LinkClick::Plain,
            space_row: "{icon} {name}".to_string(),
            tab_row: "{icon} {title}".to_string(),
            queue_row: "{icon} {title}".to_string(),
            remote_label: "☁ {host}: {name}".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotifyConfig {
    /// Fire OS notifications (macOS) when a pane needs you and nobody's attached.
    pub system: bool,
    /// Which transitions notify: "waiting", "done".
    pub events: Vec<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            system: true,
            events: vec!["waiting".to_string()],
        }
    }
}

/// Which base keymap drives commands. Presets you can switch with one setting;
/// individual [keys]/[prefix_keys] still override on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keymap {
    /// One-step alt+key (alt-z zoom…); no tmux prefix.
    Alt,
    /// tmux prefix (ctrl-b then key); alt keys stay as a fallback.
    Tmux,
    /// Both active at once (the original behavior).
    Both,
}

/// Where a command shortcut opens its pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    SplitRight,
    SplitDown,
    Tab,
    /// A floating popup (display-popup style) that closes when the command exits.
    Popup,
}

/// A user-defined key → run-a-command shortcut (config `[[bind]]`). The first
/// step toward plugin-defined command bindings.
#[derive(Debug, Clone)]
pub struct CommandBind {
    pub binding: Binding,
    pub cmd: Vec<String>,
    pub placement: Placement,
}

/// A link handler (config `[[link]]`): text matching `pattern` becomes clickable
/// and runs `run` (argv; `${url}` / `${match}` are replaced with the matched
/// text as a single argument — never shell-interpreted). `placement` = None runs
/// it detached (e.g. `open`); Some opens it in a ruckus split/tab/popup.
#[derive(Debug, Clone)]
pub struct LinkRule {
    pub pattern: regex::Regex,
    pub run: String,
    pub placement: Option<Placement>,
}

/// What click opens a link. `Plain` = any click; else a held modifier is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkClick {
    Ctrl,
    Shift,
    Plain,
}

/// A remote daemon to mirror into the sidebar (config `[[remote]]`), reached via
/// `ssh [args] <host> ruckus __proxy`.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSpec {
    pub host: String,
    /// Extra ssh options (e.g. `["-p", "2222"]`).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub keys: HashMap<Action, Vec<Binding>>,
    /// tmux-style prefix key (e.g. ctrl-b). When set, pressing it arms a pending
    /// state and the next key is looked up in `prefix_keys`. None = disabled.
    pub prefix: Option<Binding>,
    /// Command keys used after the prefix (bare keys, tmux-style: c, %, ", …).
    pub prefix_keys: HashMap<Action, Vec<Binding>>,
    /// User-defined key → command shortcuts.
    pub commands: Vec<CommandBind>,
    /// Link handlers: pattern → command. Defaults to opening URLs.
    pub links: Vec<LinkRule>,
    /// Declared plugin refs (`owner/repo[/subpath]`) — installed on startup so a
    /// copied config.toml reproduces your setup on a new machine.
    pub plugins: Vec<String>,
    /// Remote daemons to mirror into the sidebar over SSH.
    pub remotes: Vec<RemoteSpec>,
    pub theme: Theme,
    pub ui: UiConfig,
    pub glyphs: Glyphs,
    pub notify: NotifyConfig,
}

pub const KEYMAP_NAMES: &[&str] = &["tmux", "alt", "both"];

/// tmux-style defaults for keys pressed *after* the prefix.
pub const PREFIX_DEFAULTS: &[(Action, &str)] = &[
    (Action::NewTab, "c"),
    (Action::SplitRight, "%"),
    (Action::SplitDown, "\""),
    (Action::ClosePane, "x"),
    (Action::Zoom, "z"),
    (Action::NextTab, "n"),
    (Action::PrevTab, "p"),
    (Action::NextPane, "o"),
    (Action::PrevPane, "i"),
    (Action::NextSpace, "."),
    (Action::PrevSpace, ","),
    (Action::NewSpace, "C"),
    (Action::ToggleSidebar, "b"),
    (Action::JumpWaiting, "a"),
    (Action::ScrollUp, "["),
    (Action::Search, "/"),
    // Deck / overview — like tmux's prefix+w window tree.
    (Action::Deck, "w"),
    (Action::LastPane, ";"),
    (Action::LastSpace, "l"),
    (Action::ShowHelp, "?"),
    (Action::Quit, "d"),
];

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    fn specs(&self) -> Vec<&str> {
        match self {
            KeySpec::One(s) => vec![s.as_str()],
            KeySpec::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawToast {
    position: Option<String>,
    seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUi {
    // legacy key, still honored
    show_sidebar: Option<bool>,
    sidebar: Option<String>,
    sidebar_width: Option<u16>,
    sidebar_sections: Option<Vec<String>>,
    sidebar_split: Option<f64>,
    sidebar_row_gap: Option<u16>,
    sidebar_marker: Option<bool>,
    tab_numbers: Option<bool>,
    sidebar_tabs: Option<bool>,
    working_style: Option<String>,
    activity_quiet_ms: Option<u64>,
    detect_osc133: Option<bool>,
    detect_foreground: Option<bool>,
    agent_commands: Option<Vec<String>>,
    top_margin: Option<u16>,
    tab_pad: Option<u16>,
    pane_divider: Option<bool>,
    space_subtitle: Option<String>,
    tab_subtitle: Option<String>,
    gutter: Option<u16>,
    pane_padding: Option<u16>,
    pane_titles: Option<bool>,
    narrow_below: Option<u16>,
    spinner_ms: Option<u64>,
    toast: Option<RawToast>,
    header: Option<String>,
    footer: Option<String>,
    footer_mode: Option<String>,
    status_left: Option<String>,
    status_right: Option<String>,
    tab_strip: Option<bool>,
    tab_border: Option<bool>,
    mouse: Option<bool>,
    deck: Option<bool>,
    mouse_select: Option<bool>,
    mac_option_fallback: Option<bool>,
    link_click: Option<String>,
    space_row: Option<String>,
    remote_label: Option<String>,
    tab_row: Option<String>,
    queue_row: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawGlyphs {
    waiting: Option<String>,
    done: Option<String>,
    idle: Option<String>,
    working: Option<String>,
    focus: Option<String>,
    spinner: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawNotify {
    system: Option<bool>,
    events: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawBind {
    key: String,
    run: String,
    #[serde(rename = "where")]
    place: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLink {
    pattern: String,
    run: String,
    #[serde(rename = "where")]
    place: Option<String>,
}

/// Map a `where` value to a placement (defaults to a right split).
fn parse_placement(s: &str) -> Placement {
    match s.to_lowercase().as_str() {
        "down" | "split-down" | "v" => Placement::SplitDown,
        "tab" | "window" => Placement::Tab,
        "popup" | "float" => Placement::Popup,
        _ => Placement::SplitRight,
    }
}

/// Lower `[[bind]]` entries (config or plugin manifest) into command shortcuts.
fn lower_binds(raw: &[RawBind]) -> Vec<CommandBind> {
    raw.iter()
        .filter_map(|b| {
            let binding = parse_binding(&b.key).ok()?;
            let cmd: Vec<String> = b.run.split_whitespace().map(String::from).collect();
            if cmd.is_empty() {
                return None;
            }
            let placement = b
                .place
                .as_deref()
                .map(parse_placement)
                .unwrap_or(Placement::SplitRight);
            Some(CommandBind {
                binding,
                cmd,
                placement,
            })
        })
        .collect()
}

/// Lower `[[link]]` entries (config or plugin manifest) into link handlers.
fn lower_links(raw: &[RawLink]) -> Vec<LinkRule> {
    raw.iter()
        .filter_map(|l| {
            let pattern = regex::Regex::new(&l.pattern).ok()?;
            Some(LinkRule {
                pattern,
                run: l.run.clone(),
                placement: l.place.as_deref().map(parse_placement),
            })
        })
        .collect()
}

/// A plugin manifest (`ruckus-plugin.toml`). Reuses the `[[bind]]` / `[[link]]`
/// shapes so a plugin adds command shortcuts and link handlers.
#[derive(Debug, Default, Deserialize)]
struct RawManifest {
    plugin: Option<RawPluginMeta>,
    #[serde(default)]
    bind: Vec<RawBind>,
    #[serde(default)]
    link: Vec<RawLink>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPluginMeta {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    /// Declared capability scopes (surfaced by `ruckus plugin list`; not yet
    /// enforced — binds/links are user-installed).
    #[serde(default)]
    capabilities: Vec<String>,
}

/// An installed plugin, summarized for `ruckus plugin list`.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// On-disk directory name — the handle for `remove`.
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub path: std::path::PathBuf,
    pub binds: usize,
    pub links: usize,
}

/// Directory holding installed plugins (one subdirectory each).
pub fn plugins_dir() -> std::path::PathBuf {
    ruckus_dir().join("plugins")
}

/// Read + parse a plugin's manifest, if the directory has one.
fn read_manifest(dir: &std::path::Path) -> Option<RawManifest> {
    let text = std::fs::read_to_string(dir.join("ruckus-plugin.toml")).ok()?;
    toml::from_str(&text).ok()
}

/// Enumerate installed plugins (each subdir of the plugins dir with a manifest).
pub fn list_plugins() -> Vec<PluginInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugins_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Skip hidden dirs (e.g. the .cache of cloned monorepos).
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let Some(m) = read_manifest(&path) else {
            continue;
        };
        let meta = m.plugin.unwrap_or_default();
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        let name = meta.name.unwrap_or_else(|| id.clone());
        out.push(PluginInfo {
            id,
            name,
            version: meta.version.unwrap_or_default(),
            description: meta.description.unwrap_or_default(),
            capabilities: meta.capabilities,
            path,
            binds: m.bind.len(),
            links: m.link.len(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Parse a plugin reference into (clone url, owner/repo cache key, subpath).
/// Accepts `owner/repo`, `owner/repo/sub/dir`, or a full git URL (no subpath).
pub fn parse_plugin_ref(repo: &str) -> Result<(String, String, String)> {
    if repo.starts_with("http") || repo.contains("://") || repo.starts_with("git@") {
        let tail: Vec<&str> = repo.trim_end_matches(".git").rsplit('/').take(2).collect();
        let key = tail.into_iter().rev().collect::<Vec<_>>().join("/");
        return Ok((repo.to_string(), key, String::new()));
    }
    let parts: Vec<&str> = repo.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return Err(anyhow!("expected owner/repo or owner/repo/subpath"));
    }
    let owner_repo = format!("{}/{}", parts[0], parts[1]);
    Ok((
        format!("https://github.com/{owner_repo}"),
        owner_repo,
        parts[2..].join("/"),
    ))
}

/// Install a plugin ref (idempotent). Clones the repo once into a shared cache
/// (only if missing — `ruckus plugin update` pulls), then symlinks the target
/// into the plugins dir. A subpath that is a *folder of plugins* installs each.
/// Returns each `(handle, freshly_installed)`.
pub fn install_plugin(repo: &str) -> Result<Vec<(String, bool)>> {
    let dir = plugins_dir();
    std::fs::create_dir_all(&dir).ok();
    let (url, owner_repo, subpath) = parse_plugin_ref(repo)?;
    let cache = dir.join(".cache").join(owner_repo.replace('/', "__"));
    if !cache.join(".git").exists() {
        std::fs::create_dir_all(cache.parent().unwrap()).ok();
        let ok = std::process::Command::new("git")
            .args(["clone", "--depth", "1", &url])
            .arg(&cache)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(anyhow!("git clone failed for {url}"));
        }
    }
    let root = if subpath.is_empty() {
        cache.clone()
    } else {
        cache.join(&subpath)
    };
    // A single plugin, or a directory of them.
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    if root.join("ruckus-plugin.toml").exists() {
        sources.push(root.clone());
    } else if root.is_dir() {
        for e in std::fs::read_dir(&root)?.flatten() {
            if e.path().join("ruckus-plugin.toml").exists() {
                sources.push(e.path());
            }
        }
    }
    if sources.is_empty() {
        return Err(anyhow!(
            "{}: no ruckus-plugin.toml (or plugin subdirs)",
            root.display()
        ));
    }
    sources.sort();
    let mut out = Vec::new();
    for s in sources {
        let name = s
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("plugin")
            .to_string();
        let dst = dir.join(&name);
        if dst.symlink_metadata().is_ok() {
            out.push((name, false)); // already installed
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&s, &dst)?;
        out.push((name, true));
    }
    Ok(out)
}

/// Ensure every config-declared plugin ref is installed (best-effort — offline
/// or bad refs are skipped). Returns the handles freshly installed this call.
pub fn ensure_declared(refs: &[String]) -> Vec<String> {
    let mut fresh = Vec::new();
    for r in refs {
        if let Ok(installed) = install_plugin(r) {
            fresh.extend(installed.into_iter().filter(|(_, f)| *f).map(|(n, _)| n));
        }
    }
    fresh
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    /// Base keymap preset: "tmux" | "alt" | "both".
    keymap: Option<String>,
    #[serde(default)]
    keys: HashMap<String, KeySpec>,
    /// Key → run-a-command shortcuts.
    #[serde(default)]
    bind: Vec<RawBind>,
    /// Link handlers: pattern → command.
    #[serde(default)]
    link: Vec<RawLink>,
    /// Declared plugin refs installed on startup.
    #[serde(default)]
    plugins: Vec<String>,
    /// Remote daemons to mirror over SSH.
    #[serde(default)]
    remote: Vec<RemoteSpec>,
    /// tmux prefix key, e.g. "ctrl-b". "" or "off" disables it.
    prefix: Option<String>,
    #[serde(default)]
    prefix_keys: HashMap<String, KeySpec>,
    #[serde(default)]
    theme: HashMap<String, String>,
    #[serde(default)]
    ui: RawUi,
    #[serde(default)]
    glyphs: RawGlyphs,
    #[serde(default)]
    notify: RawNotify,
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn parse_bar(s: Option<&str>, default: BarPos) -> BarPos {
    match s.map(|s| s.to_lowercase()) {
        Some(s) if s == "top" => BarPos::Top,
        Some(s) if s == "bottom" => BarPos::Bottom,
        Some(s) if s == "off" => BarPos::Off,
        _ => default,
    }
}

/// Path to config.toml, writing the commented default on first touch.
pub fn ensure_config_file() -> std::path::PathBuf {
    let path = ruckus_dir().join("config.toml");
    if !path.exists() {
        let _ = std::fs::write(&path, DEFAULT_CONFIG);
    }
    path
}

impl Config {
    pub fn load() -> Config {
        let path = ensure_config_file();
        let mut cfg = Self::from_toml_str(&std::fs::read_to_string(&path).unwrap_or_default());
        cfg.load_plugins();
        cfg
    }

    /// Merge installed plugins' `[[bind]]` / `[[link]]` into the config. User
    /// config takes precedence (it's lowered first, and lookups are first-match).
    fn load_plugins(&mut self) {
        for dir in list_plugins().into_iter().map(|p| p.path) {
            if let Some(m) = read_manifest(&dir) {
                self.commands.extend(lower_binds(&m.bind));
                self.links.extend(lower_links(&m.link));
            }
        }
    }

    pub fn from_toml_str(s: &str) -> Config {
        let raw: RawConfig = toml::from_str(s).unwrap_or_default();

        let mut keys = HashMap::new();
        for (action, name, defaults) in ACTIONS {
            let specs: Vec<&str> = raw
                .keys
                .get(*name)
                .map(|s| s.specs())
                .unwrap_or_else(|| defaults.to_vec());
            let mut bindings: Vec<Binding> =
                specs.iter().filter_map(|s| parse_binding(s).ok()).collect();
            if bindings.is_empty() {
                bindings = defaults
                    .iter()
                    .map(|s| parse_binding(s).expect("default binding must parse"))
                    .collect();
            }
            keys.insert(*action, bindings);
        }

        // Keymap preset (default tmux). "alt" turns the prefix off; "tmux"/"both"
        // keep it on. An explicit `prefix = …` always wins over the preset.
        let keymap = match raw.keymap.as_deref().map(|s| s.to_lowercase()).as_deref() {
            Some("alt") => Keymap::Alt,
            Some("both") => Keymap::Both,
            _ => Keymap::Tmux,
        };
        let prefix = match raw.prefix.as_deref() {
            Some("") | Some("off") | Some("none") => None,
            Some(spec) => parse_binding(spec).ok(),
            None if keymap == Keymap::Alt => None,
            None => parse_binding("ctrl-b").ok(),
        };
        // Command shortcuts + link handlers (shared with plugin manifests).
        let commands = lower_binds(&raw.bind);
        let mut links = lower_links(&raw.link);
        if links.is_empty() {
            if let Ok(pattern) = regex::Regex::new(r#"https?://[^\s"'`)\]}>]+"#) {
                links.push(LinkRule {
                    pattern,
                    run: "open ${url}".to_string(),
                    placement: None,
                });
            }
        }

        let mut prefix_keys = HashMap::new();
        for (action, default) in PREFIX_DEFAULTS {
            let name = ACTIONS
                .iter()
                .find(|(a, _, _)| a == action)
                .map(|(_, n, _)| *n);
            let specs: Vec<&str> = name
                .and_then(|n| raw.prefix_keys.get(n))
                .map(|s| s.specs())
                .unwrap_or_else(|| vec![*default]);
            let bindings: Vec<Binding> =
                specs.iter().filter_map(|s| parse_binding(s).ok()).collect();
            if !bindings.is_empty() {
                prefix_keys.insert(*action, bindings);
            }
        }

        // Base palette: `preset` names a built-in OR a user theme
        // (~/.ruckus/themes/<name>.toml); then layer any [theme] overrides on top.
        let mut theme = raw
            .theme
            .get("preset")
            .and_then(|n| resolve_theme(n))
            .unwrap_or_default();
        apply_theme_map(&mut theme, &raw.theme);

        let d = UiConfig::default();
        let sidebar_raw = raw.ui.sidebar.as_deref().map(|s| s.to_lowercase());
        let (sidebar_pos, mut sidebar_start_visible) = match sidebar_raw.as_deref() {
            Some("right") => (SidebarPos::Right, true),
            Some("off") => (SidebarPos::Left, false),
            Some(_) | None => (SidebarPos::Left, true),
        };
        if raw.ui.show_sidebar == Some(false) {
            sidebar_start_visible = false;
        }
        let ui = UiConfig {
            sidebar_pos,
            sidebar_start_visible,
            sidebar_width: raw
                .ui
                .sidebar_width
                .unwrap_or(d.sidebar_width)
                .clamp(16, 60),
            sidebar_sections: raw.ui.sidebar_sections.unwrap_or(d.sidebar_sections),
            sidebar_split: raw
                .ui
                .sidebar_split
                .unwrap_or(d.sidebar_split)
                .clamp(0.0, 0.9),
            sidebar_row_gap: raw.ui.sidebar_row_gap.unwrap_or(d.sidebar_row_gap).min(3),
            sidebar_marker: raw.ui.sidebar_marker.unwrap_or(d.sidebar_marker),
            tab_numbers: raw.ui.tab_numbers.unwrap_or(d.tab_numbers),
            sidebar_tabs: raw.ui.sidebar_tabs.unwrap_or(d.sidebar_tabs),
            working_style: match raw
                .ui
                .working_style
                .as_deref()
                .map(|s| s.to_lowercase())
                .as_deref()
            {
                Some("pulse") => WorkingStyle::Pulse,
                Some("dot") => WorkingStyle::Dot,
                _ => WorkingStyle::Spinner,
            },
            activity_quiet_ms: raw
                .ui
                .activity_quiet_ms
                .unwrap_or(d.activity_quiet_ms)
                .clamp(200, 10_000),
            detect_osc133: raw.ui.detect_osc133.unwrap_or(d.detect_osc133),
            detect_foreground: raw.ui.detect_foreground.unwrap_or(d.detect_foreground),
            agent_commands: raw.ui.agent_commands.unwrap_or(d.agent_commands),
            top_margin: raw.ui.top_margin.unwrap_or(d.top_margin).min(8),
            tab_pad: raw.ui.tab_pad.unwrap_or(d.tab_pad).min(4),
            pane_divider: raw.ui.pane_divider.unwrap_or(d.pane_divider),
            space_subtitle: raw.ui.space_subtitle.unwrap_or(d.space_subtitle),
            tab_subtitle: raw.ui.tab_subtitle.unwrap_or(d.tab_subtitle),
            gutter: raw.ui.gutter.unwrap_or(d.gutter).min(4),
            pane_padding: raw.ui.pane_padding.unwrap_or(d.pane_padding).min(4),
            pane_titles: raw.ui.pane_titles.unwrap_or(d.pane_titles),
            narrow_below: raw.ui.narrow_below.unwrap_or(d.narrow_below),
            spinner_ms: raw.ui.spinner_ms.unwrap_or(d.spinner_ms).clamp(30, 2000),
            toast_pos: match raw
                .ui
                .toast
                .as_ref()
                .and_then(|t| t.position.as_deref())
                .map(|s| s.to_lowercase())
                .as_deref()
            {
                Some("top-left") => ToastPos::TopLeft,
                Some("top-right") => ToastPos::TopRight,
                Some("bottom-left") => ToastPos::BottomLeft,
                _ => ToastPos::BottomRight,
            },
            toast_seconds: raw
                .ui
                .toast
                .as_ref()
                .and_then(|t| t.seconds)
                .unwrap_or(d.toast_seconds)
                .clamp(1, 60),
            header: parse_bar(raw.ui.header.as_deref(), BarPos::Top),
            footer: parse_bar(raw.ui.footer.as_deref(), BarPos::Bottom),
            footer_mode: match raw
                .ui
                .footer_mode
                .as_deref()
                .map(|s| s.to_lowercase())
                .as_deref()
            {
                Some("help") => FooterMode::Help,
                Some("status") => FooterMode::Status,
                _ => FooterMode::Auto,
            },
            status_left: raw.ui.status_left.unwrap_or(d.status_left),
            status_right: raw.ui.status_right.unwrap_or(d.status_right),
            tab_strip: raw.ui.tab_strip.unwrap_or(d.tab_strip),
            tab_border: raw.ui.tab_border.unwrap_or(d.tab_border),
            mouse: raw.ui.mouse.unwrap_or(d.mouse),
            deck: raw.ui.deck.unwrap_or(d.deck),
            mouse_select: raw.ui.mouse_select.unwrap_or(d.mouse_select),
            mac_option_fallback: raw.ui.mac_option_fallback.unwrap_or(d.mac_option_fallback),
            link_click: match raw
                .ui
                .link_click
                .as_deref()
                .map(|s| s.to_lowercase())
                .as_deref()
            {
                Some("ctrl") => LinkClick::Ctrl,
                Some("shift") => LinkClick::Shift,
                _ => LinkClick::Plain,
            },
            space_row: raw.ui.space_row.unwrap_or(d.space_row),
            remote_label: raw.ui.remote_label.unwrap_or(d.remote_label),
            tab_row: raw.ui.tab_row.unwrap_or(d.tab_row),
            queue_row: raw.ui.queue_row.unwrap_or(d.queue_row),
        };

        let dg = Glyphs::default();
        let spinner = raw
            .glyphs
            .spinner
            .filter(|v| !v.is_empty())
            .unwrap_or(dg.spinner);
        let glyphs = Glyphs {
            waiting: raw.glyphs.waiting.unwrap_or(dg.waiting),
            done: raw.glyphs.done.unwrap_or(dg.done),
            idle: raw.glyphs.idle.unwrap_or(dg.idle),
            working: raw.glyphs.working.unwrap_or(dg.working),
            focus: raw.glyphs.focus.unwrap_or(dg.focus),
            spinner,
        };

        let dn = NotifyConfig::default();
        let notify = NotifyConfig {
            system: raw.notify.system.unwrap_or(dn.system),
            events: raw.notify.events.unwrap_or(dn.events),
        };

        Config {
            keys,
            prefix,
            prefix_keys,
            commands,
            links,
            plugins: raw.plugins,
            remotes: raw.remote,
            theme,
            ui,
            glyphs,
            notify,
        }
    }

    pub fn action_for(&self, ev: &KeyEvent) -> Option<Action> {
        let ev = normalize_key(ev, self.ui.mac_option_fallback);
        self.keys
            .iter()
            .find(|(_, bs)| bs.iter().any(|b| b.matches(&ev)))
            .map(|(a, _)| *a)
    }

    /// Does this key event match the tmux prefix?
    pub fn is_prefix(&self, ev: &KeyEvent) -> bool {
        let ev = normalize_key(ev, self.ui.mac_option_fallback);
        self.prefix
            .as_ref()
            .map(|b| b.matches(&ev))
            .unwrap_or(false)
    }

    /// Resolve a post-prefix key to an action. Matches on the key code alone
    /// (ignoring shift) so `%`, `"`, etc. resolve regardless of layout.
    pub fn prefix_action_for(&self, ev: &KeyEvent) -> Option<Action> {
        let ev = normalize_key(ev, self.ui.mac_option_fallback);
        self.prefix_keys
            .iter()
            .find(|(_, bs)| bs.iter().any(|b| b.code == ev.code))
            .map(|(a, _)| *a)
    }

    /// Footer/help hint for an action: the tmux prefix form (e.g. "^b c") when a
    /// prefix is set and the action has a prefix key, else the direct chord.
    pub fn hint(&self, action: Action) -> String {
        if let Some(pfx) = &self.prefix {
            if let Some(b) = self.prefix_keys.get(&action).and_then(|bs| bs.first()) {
                let p = pfx.label().replace("ctrl+", "^").replace("alt+", "M-");
                return format!("{p} {}", b.label());
            }
        }
        self.label(action)
    }

    pub fn label(&self, action: Action) -> String {
        self.keys
            .get(&action)
            .and_then(|b| b.first())
            .map(|b| b.label())
            .unwrap_or_default()
    }
}

const DEFAULT_CONFIG: &str = r##"# ruckus config — make it feel like yours.
# Key specs look like: "alt-v", "ctrl-shift-p", "f5", "alt-pageup".
# An action can have several bindings: quit = ["alt-q", "ctrl-q"]
#
# macOS: stock Terminal/iTerm send Option+key as a special character (œ, ß, √…).
# ruckus maps those back automatically (mac_option_fallback below). For dead
# keys (alt-i, alt-n) enable "Use Option as Meta" (Terminal: Profiles →
# Keyboard) or "Esc+" (iTerm: Profiles → Keys) for the full experience.

# keymap: the base scheme. "tmux" = ctrl-b prefix (n next, z zoom, x close…);
# "alt" = one-step alt+key; "both" = both at once. Switch with `ruckus keymap
# <name>`. The [keys] / [prefix_keys] overrides below still win over the preset.
keymap = "tmux"

# Plugins to install on startup (owner/repo, or owner/repo/subfolder for a
# monorepo). Copy this config to another machine and they're fetched for you.
# Run `ruckus plugin sync` to install them now.
# plugins = [
#   "joshdholtz/ruckus/plugins/gh-dash",
#   "joshdholtz/ruckus/plugins/pr-review",
# ]

# Remote daemons to mirror into your sidebar over SSH — their spaces show up
# tagged by host and are fully read/write (needs `ruckus` on the remote's PATH).
# [[remote]]
# host = "workbox"           # anything `ssh` accepts (alias, user@host, …)
# args = []                  # extra ssh options, e.g. ["-p", "2222"]

[keys]
quit = ["alt-q", "ctrl-q"]  # leave the TUI (everything keeps running in the daemon)
split_right = "alt-v"   # split the focused pane to the right
split_down = "alt-s"    # split the focused pane downward
close_pane = "alt-x"    # kill + remove the focused pane
next_pane = "alt-o"     # cycle focus through panes
prev_pane = "alt-i"
new_tab = "alt-t"       # new tab in the current space
next_tab = "alt-]"
prev_tab = "alt-["
new_space = "alt-n"     # new space
next_space = "alt-."
prev_space = "alt-,"
scroll_up = "alt-pageup"
scroll_down = "alt-pagedown"
toggle_sidebar = "alt-b"  # show/hide the sidebar (drawer on narrow screens)
jump_waiting = "alt-a"    # jump to the next pane that needs you
show_help = "alt-/"       # keybinding overlay
zoom = "alt-z"            # focused pane fills the whole area (toggle)
search = "alt-f"          # search the focused pane's scrollback (n/N to cycle)
palette = "alt-p"         # command palette: fuzzy-search every action
last_pane = "alt-;"       # jump back to the previously-focused pane
last_space = "alt-l"      # jump back to the previously-active space

# alt-1 .. alt-9 jump straight to a tab (not yet rebindable)

# Command shortcuts: bind a key to open a pane running a command.
# where = "right" (split →, default) | "down" (split ↓) | "tab" | "popup"
# ("popup" = a floating window that closes when the command exits; ⌃q force-closes)
# [[bind]]
# key = "alt-g"
# run = "lazygit"
# where = "popup"
#
# [[bind]]
# key = "alt-e"
# run = "htop"
# where = "tab"

# Link handlers: text matching `pattern` (Rust regex) becomes clickable and
# runs `run` — ${url}/${match} are replaced with the matched text as ONE
# argument (never shell-interpreted). Defaults to opening URLs if none set.
# link_click below picks the trigger: plain (default) | ctrl | shift.
# Optional `where` opens the tool in a ruckus pane (right|down|tab|popup)
# instead of running it detached — great for opening a reviewer on a PR link.
# [[link]]
# pattern = 'https?://\S+'
# run = "open ${url}"          # no `where` = run detached
#
# [[link]]                       # click a PR URL → review it in a split
# pattern = 'https://github.com/[^/]+/[^/]+/pull/[0-9]+'
# run = "gh pr view ${url}"
# where = "right"

[ui]
link_click = "plain"         # plain | ctrl | shift — how a click fires a link
sidebar = "left"            # left | right | off (off = hidden until toggled)
sidebar_width = 26
sidebar_sections = ["needs_you", "spaces"]  # order them, or drop one
gutter = 1                  # cells between panes: 0 = dense, 2 = airy
pane_padding = 0            # cells of breathing room inside each pane
pane_titles = true          # false = pure grid, no per-pane title bars
narrow_below = 70           # sidebar auto-collapses under this width; 0 = never
deck = true                 # touch-first card view when narrow (alt-d toggles); false = panes
spinner_ms = 120            # working-spinner speed
toast = { position = "bottom-right", seconds = 4 }  # also: top-left/right, bottom-left
header = "top"              # top | bottom | off
footer = "bottom"           # bottom | top | off
tab_strip = true            # false hides the tab row (tabs still in the sidebar)
mouse = true                # false leaves the mouse to your terminal (select/copy)
mac_option_fallback = true  # treat Option-typed characters (œ, ß, …) as alt bindings

# Sidebar row templates. Tokens: {icon} {title} {name} {id} {cmd} {cwd}
space_row = "{icon} {name}"
tab_row = "{icon} {title}"
queue_row = "{icon} {title}"
# How mirrored remote spaces are labelled. Tokens: {host} {name}
remote_label = "☁ {host}: {name}"

[notify]
system = true               # macOS notification when a pane needs you and nobody's watching
events = ["waiting"]        # add "done" to also notify on finished panes

[glyphs]
waiting = "◉"
done = "◍"
idle = "○"
focus = "▎"                 # focused pane marker in its title bar
spinner = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]

[theme]
# preset = "macchiato"    # macchiato latte gruvbox nord tokyonight dracula rosepine
                          # (or `ruckus theme <name>`); overrides below still win
accent = "#f5a97f"        # focus marker, active tab, buttons
border = "#3b4058"        # popup/dialog borders (panes are borderless layers)
bg = "#0f1117"            # root background — the gutters between panes
surface = "#1c2030"       # pane content background
bar_bg = "#161925"        # header/footer/tab bar background
bar_fg = "#8f93a2"        # inactive bar text
bar_active_fg = "#cad3f5" # active bar text
status_fg = "#6e738d"     # hint text
sidebar_bg = "#161925"    # sidebar background
select_bg = "#2e3348"     # selected/hovered row + pane title bars
working = "#8aadf4"       # state colors
waiting = "#eed49f"
idle = "#5b6078"
done_ok = "#a6da95"
done_err = "#ed8796"
# remote = "#f5bde6" # optional: tint mirrored remote space rows in the sidebar
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_specs_parse() {
        let b = parse_binding("alt-v").unwrap();
        assert_eq!(b.code, KeyCode::Char('v'));
        assert_eq!(b.mods, KeyModifiers::ALT);
        let b = parse_binding("ctrl-shift-p").unwrap();
        assert!(b.mods.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert!(parse_binding("f5").is_ok());
        assert!(parse_binding("alt-pageup").is_ok());
        assert!(parse_binding("bogus-key").is_err());
    }

    #[test]
    fn all_named_themes_resolve() {
        for name in THEME_NAMES {
            assert!(theme_preset(name).is_some(), "preset {name} missing");
        }
        assert!(theme_preset("nope").is_none());
        // preset selects the base palette; explicit keys still override it
        let cfg = Config::from_toml_str("[theme]\npreset = \"nord\"\naccent = \"#ffffff\"\n");
        assert_eq!(cfg.theme.bg, theme_preset("nord").unwrap().bg);
        assert_eq!(cfg.theme.accent, Color::Rgb(0xff, 0xff, 0xff));
    }

    #[test]
    fn mac_option_chars_map_back() {
        assert_eq!(mac_option_char('œ'), Some('q'));
        assert_eq!(mac_option_char('√'), Some('v'));
        assert_eq!(mac_option_char('™'), Some('2'));
        assert_eq!(mac_option_char('x'), None);
    }

    #[test]
    fn config_defaults_and_overrides() {
        let cfg = Config::from_toml_str(
            r##"
[keys]
quit = ["alt-w", "ctrl-w"]
[ui]
sidebar = "right"
gutter = 3
narrow_below = 0
header = "bottom"
[glyphs]
idle = "▪"
[theme]
accent = "#ff0000"
[notify]
system = false
"##,
        );
        assert_eq!(cfg.keys[&Action::Quit].len(), 2);
        assert_eq!(cfg.ui.sidebar_pos, SidebarPos::Right);
        assert_eq!(cfg.ui.gutter, 3);
        assert_eq!(cfg.ui.narrow_below, 0);
        assert_eq!(cfg.ui.header, BarPos::Bottom);
        assert_eq!(cfg.glyphs.idle, "▪");
        assert_eq!(cfg.theme.accent, Color::Rgb(0xff, 0, 0));
        assert!(!cfg.notify.system);
        // untouched values fall back to defaults
        assert_eq!(cfg.ui.footer, BarPos::Bottom);
        assert!(cfg.ui.pane_titles);
        assert_eq!(cfg.keys[&Action::SplitRight].len(), 1);
    }

    #[test]
    fn quit_is_a_plain_overridable_binding() {
        // Default: both alt-q and ctrl-q quit (two bindings on one command).
        let cfg = Config::from_toml_str("");
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let alt_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT);
        assert_eq!(cfg.action_for(&ctrl_q), Some(Action::Quit));
        assert_eq!(cfg.action_for(&alt_q), Some(Action::Quit));
        // Override it and ruckus honors you — no hardwired ctrl-q behind your back.
        let cfg = Config::from_toml_str("[keys]\nquit = \"alt-q\"\n");
        assert_eq!(cfg.action_for(&ctrl_q), None);
        assert_eq!(cfg.action_for(&alt_q), Some(Action::Quit));
    }

    #[test]
    fn option_typed_char_triggers_alt_binding() {
        let cfg = Config::from_toml_str("");
        let ev = KeyEvent::new(KeyCode::Char('œ'), KeyModifiers::NONE);
        assert_eq!(cfg.action_for(&ev), Some(Action::Quit));
        let ev = KeyEvent::new(KeyCode::Char('√'), KeyModifiers::NONE);
        assert_eq!(cfg.action_for(&ev), Some(Action::SplitRight));
    }

    #[test]
    fn command_shortcuts_parse() {
        let cfg = Config::from_toml_str(
            r#"
[[bind]]
key = "alt-g"
run = "lazygit --help"
where = "right"

[[bind]]
key = "alt-e"
run = "htop"
where = "tab"

[[bind]]
key = "alt-l"
run = "lazygit"
where = "popup"

[[bind]]
key = "bogus-key"
run = "nope"
"#,
        );
        assert_eq!(cfg.commands.len(), 3); // the bogus binding is dropped
        assert_eq!(cfg.commands[0].cmd, vec!["lazygit", "--help"]);
        assert_eq!(cfg.commands[0].placement, Placement::SplitRight);
        assert_eq!(cfg.commands[1].placement, Placement::Tab);
        assert_eq!(cfg.commands[2].placement, Placement::Popup);
        let ev = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT);
        assert!(cfg.commands[0].binding.matches(&ev));
    }

    #[test]
    fn link_rules_default_and_custom() {
        // No [[link]] → a built-in URL opener; plain-click by default.
        let cfg = Config::from_toml_str("");
        assert_eq!(cfg.links.len(), 1);
        assert!(cfg.links[0]
            .pattern
            .is_match("see https://example.com/x now"));
        assert_eq!(cfg.ui.link_click, LinkClick::Plain);
        // Custom rules replace the default; link_click is overridable.
        let cfg = Config::from_toml_str(
            r#"
[ui]
link_click = "ctrl"
[[link]]
pattern = '[A-Z]{2,}-[0-9]+'
run = "open https://linear.app/issue/${match}"
"#,
        );
        assert_eq!(cfg.links.len(), 1);
        let m = cfg.links[0].pattern.find("fix FIS-42 today").unwrap();
        assert_eq!(m.as_str(), "FIS-42");
        assert_eq!(cfg.ui.link_click, LinkClick::Ctrl);
    }

    #[test]
    fn plugin_manifest_lowers_binds_and_links() {
        let m: RawManifest = toml::from_str(
            r#"
[plugin]
name = "lazygit"
version = "0.1.0"
capabilities = ["run-commands"]

[[bind]]
key = "alt-g"
run = "lazygit"
where = "popup"

[[link]]
pattern = 'https?://\S+'
run = "open ${url}"
"#,
        )
        .unwrap();
        assert_eq!(m.plugin.as_ref().unwrap().name.as_deref(), Some("lazygit"));
        let binds = lower_binds(&m.bind);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].placement, Placement::Popup);
        let links = lower_links(&m.link);
        assert_eq!(links.len(), 1);
        assert!(links[0].pattern.is_match("go https://x.dev now"));
    }

    #[test]
    fn bad_config_falls_back_to_defaults() {
        let cfg = Config::from_toml_str("this is not [valid toml");
        assert_eq!(cfg.ui.sidebar_width, 26);
        assert!(!cfg.keys[&Action::Quit].is_empty());
    }
}
