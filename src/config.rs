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
];

/// macOS terminals without "Option as Meta" type a special character instead of
/// sending alt+key. Map those characters back to the key they came from (US layout).
pub fn mac_option_char(c: char) -> Option<char> {
    Some(match c {
        'å' => 'a', '∫' => 'b', 'ç' => 'c', '∂' => 'd', 'ƒ' => 'f', '©' => 'g',
        '˙' => 'h', '∆' => 'j', '˚' => 'k', '¬' => 'l', 'µ' => 'm', 'ø' => 'o',
        'π' => 'p', 'œ' => 'q', '®' => 'r', 'ß' => 's', '†' => 't', '√' => 'v',
        '∑' => 'w', '≈' => 'x', '¥' => 'y', 'Ω' => 'z',
        '≤' => ',', '≥' => '.', '“' => '[', '‘' => ']', '–' => '-', '≠' => '=',
        '…' => ';', 'æ' => '\'', '«' => '\\', '÷' => '/',
        '¡' => '1', '™' => '2', '£' => '3', '¢' => '4', '∞' => '5', '§' => '6',
        '¶' => '7', '•' => '8', 'ª' => '9',
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
    Ok(Binding { code: code.ok_or_else(|| anyhow!("no key in '{spec}'"))?, mods })
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
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            accent: Color::Rgb(0xf5, 0xa9, 0x7f),
            border: Color::Rgb(0x3b, 0x40, 0x58),
            bg: Color::Rgb(0x0f, 0x11, 0x17),
            surface: Color::Rgb(0x1c, 0x20, 0x30),
            bar_bg: Color::Rgb(0x16, 0x19, 0x25),
            bar_fg: Color::Rgb(0x8f, 0x93, 0xa2),
            bar_active_fg: Color::Rgb(0xca, 0xd3, 0xf5),
            status_fg: Color::Rgb(0x6e, 0x73, 0x8d),
            sidebar_bg: Color::Rgb(0x16, 0x19, 0x25),
            select_bg: Color::Rgb(0x2e, 0x33, 0x48),
            working: Color::Rgb(0x8a, 0xad, 0xf4),
            waiting: Color::Rgb(0xee, 0xd4, 0x9f),
            idle: Color::Rgb(0x5b, 0x60, 0x78),
            done_ok: Color::Rgb(0xa6, 0xda, 0x95),
            done_err: Color::Rgb(0xed, 0x87, 0x96),
        }
    }
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
    pub tab_strip: bool,
    pub mouse: bool,
    pub mac_option_fallback: bool,
    pub space_row: String,
    pub tab_row: String,
    pub queue_row: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            sidebar_pos: SidebarPos::Left,
            sidebar_start_visible: true,
            sidebar_width: 26,
            sidebar_sections: vec!["needs_you".to_string(), "spaces".to_string()],
            sidebar_row_gap: 1,
            sidebar_marker: true,
            tab_numbers: true,
            sidebar_tabs: true,
            working_style: WorkingStyle::Spinner,
            activity_quiet_ms: 900,
            detect_osc133: false,
            top_margin: 0,
            tab_pad: 1,
            pane_divider: true,
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
            tab_strip: true,
            mouse: true,
            mac_option_fallback: true,
            space_row: "{icon} {name}".to_string(),
            tab_row: "{icon} {title}".to_string(),
            queue_row: "{icon} {title}".to_string(),
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
        NotifyConfig { system: true, events: vec!["waiting".to_string()] }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub keys: HashMap<Action, Vec<Binding>>,
    pub theme: Theme,
    pub ui: UiConfig,
    pub glyphs: Glyphs,
    pub notify: NotifyConfig,
}

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
    sidebar_row_gap: Option<u16>,
    sidebar_marker: Option<bool>,
    tab_numbers: Option<bool>,
    sidebar_tabs: Option<bool>,
    working_style: Option<String>,
    activity_quiet_ms: Option<u64>,
    detect_osc133: Option<bool>,
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
    tab_strip: Option<bool>,
    mouse: Option<bool>,
    mac_option_fallback: Option<bool>,
    space_row: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    keys: HashMap<String, KeySpec>,
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
        Self::from_toml_str(&std::fs::read_to_string(&path).unwrap_or_default())
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

        let mut theme = Theme::default();
        let t = &raw.theme;
        let set = |field: &mut Color, name: &str| {
            if let Some(c) = t.get(name).and_then(|s| parse_hex(s)) {
                *field = c;
            }
        };
        set(&mut theme.accent, "accent");
        set(&mut theme.border, "border");
        set(&mut theme.bg, "bg");
        set(&mut theme.surface, "surface");
        set(&mut theme.bar_bg, "bar_bg");
        set(&mut theme.bar_fg, "bar_fg");
        set(&mut theme.bar_active_fg, "bar_active_fg");
        set(&mut theme.status_fg, "status_fg");
        set(&mut theme.sidebar_bg, "sidebar_bg");
        set(&mut theme.select_bg, "select_bg");
        set(&mut theme.working, "working");
        set(&mut theme.waiting, "waiting");
        set(&mut theme.idle, "idle");
        set(&mut theme.done_ok, "done_ok");
        set(&mut theme.done_err, "done_err");

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
            sidebar_width: raw.ui.sidebar_width.unwrap_or(d.sidebar_width).clamp(16, 60),
            sidebar_sections: raw.ui.sidebar_sections.unwrap_or(d.sidebar_sections),
            sidebar_row_gap: raw.ui.sidebar_row_gap.unwrap_or(d.sidebar_row_gap).min(3),
            sidebar_marker: raw.ui.sidebar_marker.unwrap_or(d.sidebar_marker),
            tab_numbers: raw.ui.tab_numbers.unwrap_or(d.tab_numbers),
            sidebar_tabs: raw.ui.sidebar_tabs.unwrap_or(d.sidebar_tabs),
            working_style: match raw.ui.working_style.as_deref().map(|s| s.to_lowercase()).as_deref() {
                Some("pulse") => WorkingStyle::Pulse,
                Some("dot") => WorkingStyle::Dot,
                _ => WorkingStyle::Spinner,
            },
            activity_quiet_ms: raw.ui.activity_quiet_ms.unwrap_or(d.activity_quiet_ms).clamp(200, 10_000),
            detect_osc133: raw.ui.detect_osc133.unwrap_or(d.detect_osc133),
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
            tab_strip: raw.ui.tab_strip.unwrap_or(d.tab_strip),
            mouse: raw.ui.mouse.unwrap_or(d.mouse),
            mac_option_fallback: raw.ui.mac_option_fallback.unwrap_or(d.mac_option_fallback),
            space_row: raw.ui.space_row.unwrap_or(d.space_row),
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

        Config { keys, theme, ui, glyphs, notify }
    }

    pub fn action_for(&self, ev: &KeyEvent) -> Option<Action> {
        let ev = normalize_key(ev, self.ui.mac_option_fallback);
        if let Some(a) = self
            .keys
            .iter()
            .find(|(_, bs)| bs.iter().any(|b| b.matches(&ev)))
            .map(|(a, _)| *a)
        {
            return Some(a);
        }
        // Escape hatch: ctrl+q always quits unless the user bound it to something else.
        if ev.modifiers.contains(KeyModifiers::CONTROL) && ev.code == KeyCode::Char('q') {
            return Some(Action::Quit);
        }
        None
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

# alt-1 .. alt-9 jump straight to a tab (not yet rebindable)

[ui]
sidebar = "left"            # left | right | off (off = hidden until toggled)
sidebar_width = 26
sidebar_sections = ["needs_you", "spaces"]  # order them, or drop one
gutter = 1                  # cells between panes: 0 = dense, 2 = airy
pane_padding = 0            # cells of breathing room inside each pane
pane_titles = true          # false = pure grid, no per-pane title bars
narrow_below = 70           # sidebar auto-collapses under this width; 0 = never
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
    fn ctrl_q_escape_hatch_survives_user_config() {
        let cfg = Config::from_toml_str("[keys]\nquit = \"alt-q\"\n");
        let ev = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(cfg.action_for(&ev), Some(Action::Quit));
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
    fn bad_config_falls_back_to_defaults() {
        let cfg = Config::from_toml_str("this is not [valid toml");
        assert_eq!(cfg.ui.sidebar_width, 26);
        assert!(!cfg.keys[&Action::Quit].is_empty());
    }
}
