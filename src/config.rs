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
        // Compare chars case-insensitively so shift doesn't break alt bindings.
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
    /// Overlay (menu/modal/toast) borders — panes use background layers, not lines.
    pub border: Color,
    /// Root background, darkest layer; shows as gutters between panes.
    pub bg: Color,
    /// Pane content background.
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

#[derive(Debug, Clone)]
pub struct UiConfig {
    pub show_sidebar: bool,
    pub sidebar_width: u16,
    pub mac_option_fallback: bool,
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { show_sidebar: true, sidebar_width: 26, mac_option_fallback: true, mouse: true }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub keys: HashMap<Action, Vec<Binding>>,
    pub theme: Theme,
    pub ui: UiConfig,
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
struct RawUi {
    show_sidebar: Option<bool>,
    sidebar_width: Option<u16>,
    mac_option_fallback: Option<bool>,
    mouse: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    keys: HashMap<String, KeySpec>,
    #[serde(default)]
    theme: HashMap<String, String>,
    #[serde(default)]
    ui: RawUi,
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

impl Config {
    pub fn load() -> Config {
        let path = ruckus_dir().join("config.toml");
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_CONFIG);
        }
        let raw: RawConfig = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

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

        let ui = UiConfig {
            show_sidebar: raw.ui.show_sidebar.unwrap_or(true),
            sidebar_width: raw.ui.sidebar_width.unwrap_or(26).clamp(16, 60),
            mac_option_fallback: raw.ui.mac_option_fallback.unwrap_or(true),
            mouse: raw.ui.mouse.unwrap_or(true),
        };

        Config { keys, theme, ui }
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

const DEFAULT_CONFIG: &str = r##"# ruckus config — every key below is rebindable, every color is yours.
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
toggle_sidebar = "alt-b"  # show/hide the sidebar
jump_waiting = "alt-a"    # jump to the next pane that needs you
show_help = "alt-/"       # keybinding overlay

# alt-1 .. alt-9 jump straight to a tab (not yet rebindable)

[ui]
show_sidebar = true
sidebar_width = 26
mac_option_fallback = true  # treat Option-typed characters (œ, ß, …) as alt bindings
mouse = true                # set false to leave the mouse to your terminal (select/copy)

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
working = "#8aadf4"       # pane state dots
waiting = "#eed49f"
idle = "#5b6078"
done_ok = "#a6da95"
done_err = "#ed8796"
"##;
