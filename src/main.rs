mod client;
mod config;
mod daemon;
mod layout;
mod protocol;
mod remote;
mod render;
mod tui;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::{Parser, Subcommand};

use client::{connect, ensure_daemon, locate_pane, resolve_pane};
use protocol::*;

#[derive(Parser)]
#[command(
    name = "ruckus",
    version,
    about = "a persistent runtime for your coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (normally started automatically)
    Daemon,
    /// Internal: relay this box's daemon socket over stdio (used over SSH)
    #[command(name = "__proxy", hide = true)]
    Proxy,
    /// List spaces, tabs, and panes
    Ls,
    /// Create a new tab running CMD (defaults to your shell) and open the TUI on it
    New {
        /// Tab name
        #[arg(short, long)]
        name: Option<String>,
        /// Don't open the TUI, just create the tab
        #[arg(short, long)]
        detach: bool,
        /// Command to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Create a new space (optionally named); its first tab runs your shell
    NewSpace {
        /// Space name
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Open the TUI focused on a pane (by id or title)
    Attach { target: String },
    /// Kill a pane's process and remove it (by id or title)
    Kill { target: String },
    /// Print a pane's scrollback and follow its output (by id or title)
    Tail { target: String },
    /// Stream ruckus lifecycle events as JSON lines (for plugins and agents)
    Events,
    /// Full state as pretty text or JSON (for scripts and agents)
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Type text into a pane (Enter appended unless --no-enter)
    Send {
        target: String,
        #[arg(long)]
        no_enter: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Split a pane, running CMD (defaults to your shell) in the new half
    Split {
        target: String,
        #[arg(value_parser = ["right", "down"])]
        direction: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Focus a pane (active space/tab/pane for every attached client)
    Focus { target: String },
    /// Rename a space or tab
    Rename {
        #[arg(value_parser = ["space", "tab"])]
        kind: String,
        id: u64,
        name: String,
    },
    /// Respawn an exited pane's command in place (scrollback kept)
    Restart { target: String },
    /// Close a tab and its panes by id (removes the space if it empties)
    CloseTab { id: u64 },
    /// Close a space and all its panes by id
    CloseSpace { id: u64 },
    /// Reorder a tab to index TO within its space (0-based)
    MoveTab { id: u64, to: usize },
    /// Reorder a space to index TO in the space list (0-based)
    MoveSpace { id: u64, to: usize },
    /// Report an authoritative pane activity (for detectors/plugins)
    ReportActivity {
        pane: u64,
        #[arg(value_parser = ["working", "waiting", "idle", "auto"])]
        state: String,
    },
    /// Report the agent running in a pane (omit NAME to clear)
    ReportAgent { pane: u64, name: Option<String> },
    /// Read or edit config.toml (comment-preserving)
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage plugins (a folder with a ruckus-plugin.toml adding binds/links)
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },
    /// List built-in themes, or switch to one: `ruckus theme nord`
    Theme { name: Option<String> },
    /// Show or switch the base keymap: `ruckus keymap tmux` (or alt / both)
    Keymap { name: Option<String> },
    /// Tell the daemon and every attached TUI to reload config.toml now
    Reload,
    /// Upgrade the running daemon to the current binary WITHOUT killing panes
    /// (re-execs in place; your agents keep running)
    Upgrade,
    /// Mirror a remote ruckus over SSH into this daemon: `ruckus connect-remote workbox`
    ConnectRemote {
        host: String,
        /// Extra ssh options, e.g. -- -p 2222
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Drop a mirrored remote by host or origin — CLI escape hatch. No target
    /// disconnects them all: `ruckus disconnect-remote` / `… workbox` / `… 1`.
    /// `--force` kills the ssh proxy processes directly WITHOUT contacting the
    /// daemon — the recovery path when a bad remote has wedged it.
    DisconnectRemote {
        target: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the whole config file
    List,
    /// Print one value, e.g. `ruckus config get ui.gutter`
    Get { key: String },
    /// Set a value, e.g. `ruckus config set ui.gutter 2` or `set keys.quit '["alt-q","ctrl-q"]'`
    Set { key: String, value: String },
    /// Delete a key (falls back to the built-in default)
    Unset { key: String },
    /// Print the config file path
    Path,
}

#[derive(Subcommand)]
enum PluginCmd {
    /// List installed plugins
    List,
    /// Symlink a local plugin directory into the plugins folder (dev)
    Link { path: String },
    /// Install a plugin from a GitHub repo: `ruckus plugin install owner/repo`
    Install { repo: String },
    /// Install every plugin declared in config.toml (for a fresh machine)
    Sync,
    /// Update installed plugins (git pull); NAME updates just one
    Update { name: Option<String> },
    /// Remove an installed plugin by name
    Remove { name: String },
    /// Print the plugins directory path
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Rust ignores SIGPIPE, which turns `ruckus status | head` into a panic on
    // the broken pipe. Restore default so piped CLI output terminates quietly.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match cli.cmd {
        None => tui::run(None).await,
        Some(Cmd::Daemon) => daemon::run().await,
        Some(Cmd::Proxy) => client::proxy().await,
        Some(Cmd::Ls) => ls().await,
        Some(Cmd::New { name, detach, cmd }) => new_tab(name, detach, cmd).await,
        Some(Cmd::NewSpace { name }) => new_space(name).await,
        Some(Cmd::Attach { target }) => tui::run(Some(target)).await,
        Some(Cmd::Kill { target }) => kill(target).await,
        Some(Cmd::Tail { target }) => tail(target).await,
        Some(Cmd::Events) => events_cmd().await,
        Some(Cmd::Status { json }) => status(json).await,
        Some(Cmd::Send {
            target,
            no_enter,
            text,
        }) => send(target, no_enter, text).await,
        Some(Cmd::Split {
            target,
            direction,
            cmd,
        }) => split(target, direction, cmd).await,
        Some(Cmd::Focus { target }) => focus(target).await,
        Some(Cmd::Rename { kind, id, name }) => rename(kind, id, name).await,
        Some(Cmd::Restart { target }) => restart(target).await,
        Some(Cmd::CloseTab { id }) => simple_req(Request::CloseTab { tab: id }, "closed tab").await,
        Some(Cmd::CloseSpace { id }) => {
            simple_req(Request::CloseSpace { space: id }, "closed space").await
        }
        Some(Cmd::MoveTab { id, to }) => {
            simple_req(Request::MoveTab { tab: id, to }, "moved tab").await
        }
        Some(Cmd::MoveSpace { id, to }) => {
            simple_req(Request::MoveSpace { space: id, to }, "moved space").await
        }
        Some(Cmd::ReportActivity { pane, state }) => {
            simple_req(Request::ReportActivity { pane, state }, "reported").await
        }
        Some(Cmd::ReportAgent { pane, name }) => {
            simple_req(Request::ReportAgent { pane, name }, "reported").await
        }
        Some(Cmd::Config { cmd }) => config_cmd(cmd).await,
        Some(Cmd::Plugin { cmd }) => plugin_cmd(cmd).await,
        Some(Cmd::Theme { name }) => theme_cmd(name).await,
        Some(Cmd::Keymap { name }) => keymap_cmd(name).await,
        Some(Cmd::Reload) => reload().await,
        Some(Cmd::Upgrade) => upgrade().await,
        Some(Cmd::ConnectRemote { host, args }) => connect_remote_cmd(host, args).await,
        Some(Cmd::DisconnectRemote { target, force }) => disconnect_remote_cmd(target, force).await,
    }
}

/// The caller's live SSH env, forwarded so the detached daemon authenticates as
/// the user (agent auth / hardware-key touch are agent-side).
fn ssh_env() -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    for k in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.to_string(), v);
        }
    }
    env
}

async fn connect_remote_cmd(host: String, args: Vec<String>) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    client
        .request(Request::ConnectRemote {
            host: host.clone(),
            args,
            env: ssh_env(),
        })
        .await?;
    println!("connecting {host} — mirroring in the background");
    Ok(())
}

async fn disconnect_remote_cmd(target: Option<String>, force: bool) -> Result<()> {
    if force {
        // Lock-free recovery: kill the local `ssh … ruckus __proxy` processes
        // directly, without touching the (possibly wedged) daemon. This drops
        // every mirror and stops the remote data flooding in; if the daemon is
        // still stuck afterward, `ruckus upgrade` re-execs it (panes survive).
        match std::process::Command::new("pkill")
            .arg("-f")
            .arg("ruckus __proxy")
            .status()
        {
            Ok(s) if s.success() => {
                println!("killed remote proxy connections — run `ruckus upgrade` if the daemon is still wedged")
            }
            Ok(_) => println!("no remote proxy connections found"),
            Err(e) => println!("could not run pkill: {e}"),
        }
        return Ok(());
    }
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    // origin -> host, from the daemon's merged snapshot.
    let hosts = &snap.remote_hosts;
    if hosts.is_empty() {
        println!("no remotes connected");
        return Ok(());
    }
    // Resolve which origins to drop: all, an exact origin number, or a host match.
    let origins: Vec<u16> = match target.as_deref() {
        None => hosts.keys().copied().collect(),
        Some(t) => {
            if let Ok(n) = t.parse::<u16>() {
                if hosts.contains_key(&n) {
                    vec![n]
                } else {
                    vec![]
                }
            } else {
                hosts
                    .iter()
                    .filter(|(_, h)| h.contains(t))
                    .map(|(o, _)| *o)
                    .collect()
            }
        }
    };
    if origins.is_empty() {
        println!("no remote matching {:?}", target.unwrap_or_default());
        return Ok(());
    }
    for o in origins {
        let host = hosts.get(&o).cloned().unwrap_or_default();
        client
            .request(Request::DisconnectRemote { origin: o })
            .await?;
        println!("disconnected {host} (origin {o})");
    }
    Ok(())
}

async fn upgrade() -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    // The daemon re-execs and drops this connection, so don't wait for a reply.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.request(Request::Upgrade),
    )
    .await;
    println!("ruckus daemon upgraded in place — panes and agents kept running");
    Ok(())
}

async fn theme_cmd(name: Option<String>) -> Result<()> {
    let path = config::ensure_config_file();
    let current = {
        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
        doc.get("theme")
            .and_then(|t| t.get("preset"))
            .and_then(|v| v.as_str())
            .unwrap_or("macchiato")
            .to_string()
    };
    let user = config::list_user_themes();
    let Some(name) = name else {
        println!("themes:");
        for n in config::THEME_NAMES {
            let mark = if *n == current { "●" } else { " " };
            println!("  {mark} {n}");
        }
        if !user.is_empty() {
            println!("\ncustom (~/.ruckus/themes):");
            for n in &user {
                let mark = if *n == current { "●" } else { " " };
                println!("  {mark} {n}");
            }
        }
        println!("\nswitch with: ruckus theme <name>");
        return Ok(());
    };
    if config::resolve_theme(&name).is_none() {
        let mut all: Vec<String> = config::THEME_NAMES.iter().map(|s| s.to_string()).collect();
        all.extend(user);
        anyhow::bail!("unknown theme '{name}' — try: {}", all.join(", "));
    }
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    if doc.get("theme").is_none() {
        doc["theme"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["theme"]["preset"] = toml_edit::value(name.as_str());
    std::fs::write(&path, doc.to_string())?;
    reload_if_running().await;
    println!("theme set to {name}");
    Ok(())
}

async fn keymap_cmd(name: Option<String>) -> Result<()> {
    let path = config::ensure_config_file();
    let current = {
        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
        doc.get("keymap")
            .and_then(|v| v.as_str())
            .unwrap_or("tmux")
            .to_string()
    };
    let Some(name) = name else {
        println!("keymaps:");
        for n in config::KEYMAP_NAMES {
            let mark = if *n == current { "●" } else { " " };
            let desc = match *n {
                "tmux" => "ctrl-b then a key (tmux prefix)",
                "alt" => "one-step alt+key",
                _ => "both at once",
            };
            println!("  {mark} {n:<6} {desc}");
        }
        println!("\nswitch with: ruckus keymap <name>");
        return Ok(());
    };
    if !config::KEYMAP_NAMES.contains(&name.as_str()) {
        anyhow::bail!(
            "unknown keymap '{name}' — try: {}",
            config::KEYMAP_NAMES.join(", ")
        );
    }
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    doc["keymap"] = toml_edit::value(name.as_str());
    std::fs::write(&path, doc.to_string())?;
    reload_if_running().await;
    println!("keymap set to {name}");
    Ok(())
}

async fn reload() -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    client.request(Request::Reload).await?;
    println!("config reloaded");
    Ok(())
}

/// Send a fire-and-forget request, print `ok_msg` on success or bail on error.
async fn simple_req(req: Request, ok_msg: &str) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    match client.request(req).await? {
        ServerMsg::Error { message } => anyhow::bail!(message),
        _ => println!("{ok_msg}"),
    }
    Ok(())
}

/// Ask a *running* daemon to reload, without starting one. Silent no-op if the
/// daemon isn't up — editing config while nothing's running needs no reload.
async fn reload_if_running() {
    if let Ok((client, _events)) = connect().await {
        let _ = client.request(Request::Reload).await;
    }
}

async fn new_space(name: Option<String>) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let msg = client.request(Request::NewSpace { name, cwd }).await?;
    if let ServerMsg::Created { space, pane, .. } = msg {
        println!("space {space} created (pane {pane})");
    }
    Ok(())
}

async fn status(json: bool) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    print_tree(&snap);
    Ok(())
}

async fn send(target: String, no_enter: bool, text: Vec<String>) -> Result<()> {
    use base64::Engine as _;
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let mut bytes = text.join(" ").into_bytes();
    if !no_enter {
        bytes.push(b'\r');
    }
    client
        .request(Request::Input {
            pane: pane.id,
            data: B64.encode(&bytes),
        })
        .await?;
    println!("sent to pane {} ({})", pane.id, pane.title);
    Ok(())
}

async fn split(target: String, direction: String, cmd: Vec<String>) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let dir = if direction == "right" {
        Dir::Right
    } else {
        Dir::Down
    };
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let msg = client
        .request(Request::Split {
            pane: pane.id,
            dir,
            cmd,
            cwd,
        })
        .await?;
    if let ServerMsg::Created { pane, .. } = msg {
        println!("pane {pane} created");
    }
    Ok(())
}

async fn focus(target: String) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let Some((space, tab)) = locate_pane(&snap, pane.id) else {
        anyhow::bail!("pane {} is not in any tab", pane.id);
    };
    client
        .request(Request::SetActive {
            space,
            tab,
            pane: pane.id,
        })
        .await?;
    println!("focused pane {} ({})", pane.id, pane.title);
    Ok(())
}

async fn rename(kind: String, id: u64, name: String) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let req = if kind == "space" {
        Request::RenameSpace {
            space: id,
            name: name.clone(),
        }
    } else {
        Request::RenameTab {
            tab: id,
            name: name.clone(),
        }
    };
    client.request(req).await?;
    println!("{kind} {id} renamed to {name}");
    Ok(())
}

async fn restart(target: String) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    client.request(Request::Restart { pane: pane.id }).await?;
    println!("pane {} restarted", pane.id);
    Ok(())
}

/// Walk up from `start` (following symlinks) to the enclosing git repo, if any.
fn git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut p = start.canonicalize().ok()?;
    loop {
        if p.join(".git").exists() {
            return Some(p);
        }
        p = p.parent()?.to_path_buf();
    }
}

async fn plugin_cmd(cmd: PluginCmd) -> Result<()> {
    use config::{list_plugins, plugins_dir};
    let dir = plugins_dir();
    std::fs::create_dir_all(&dir).ok();
    match cmd {
        PluginCmd::Path => println!("{}", dir.display()),
        PluginCmd::List => {
            let plugins = list_plugins();
            if plugins.is_empty() {
                println!("no plugins installed ({}) ", dir.display());
                println!("install one: ruckus plugin install owner/repo");
            }
            for p in plugins {
                let caps = if p.capabilities.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", p.capabilities.join(", "))
                };
                let ver = if p.version.is_empty() {
                    String::new()
                } else {
                    format!(" v{}", p.version)
                };
                let alias = if p.name != p.id {
                    format!(" ({})", p.name)
                } else {
                    String::new()
                };
                println!(
                    "• {}{alias}{ver}  — {} binds, {} links{caps}",
                    p.id, p.binds, p.links
                );
                if !p.description.is_empty() {
                    println!("    {}", p.description);
                }
            }
        }
        PluginCmd::Link { path } => {
            let src = std::fs::canonicalize(&path)?;
            // A single plugin, or a directory of them (link each subfolder).
            let mut sources: Vec<std::path::PathBuf> = Vec::new();
            if src.join("ruckus-plugin.toml").exists() {
                sources.push(src.clone());
            } else {
                for e in std::fs::read_dir(&src)?.flatten() {
                    if e.path().join("ruckus-plugin.toml").exists() {
                        sources.push(e.path());
                    }
                }
                if sources.is_empty() {
                    anyhow::bail!(
                        "{}: no ruckus-plugin.toml (or plugin subdirs)",
                        src.display()
                    );
                }
            }
            sources.sort();
            for s in sources {
                let name = s
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("plugin")
                    .to_string();
                let dst = dir.join(&name);
                if dst.symlink_metadata().is_ok() {
                    println!("· {name} already installed");
                    continue;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&s, &dst)?;
                println!("linked {name}");
            }
            println!("run `ruckus reload` to pick it up");
        }
        PluginCmd::Install { repo } => {
            // owner/repo, owner/repo/sub/dir (single plugin OR a folder of them),
            // or a full git URL.
            let mut any_fresh = false;
            for (name, fresh) in config::install_plugin(&repo)? {
                if fresh {
                    println!("installed {name}");
                    any_fresh = true;
                } else {
                    println!("· {name} already installed");
                }
            }
            if any_fresh {
                println!("run `ruckus reload` to pick it up");
            }
        }
        PluginCmd::Sync => {
            let cfg = config::Config::load();
            if cfg.plugins.is_empty() {
                println!("no plugins declared in config.toml (add a `plugins = [...]` list)");
            } else {
                let fresh = config::ensure_declared(&cfg.plugins);
                for n in &fresh {
                    println!("installed {n}");
                }
                println!(
                    "{} declared, {} newly installed — `ruckus reload` to apply",
                    cfg.plugins.len(),
                    fresh.len()
                );
            }
        }
        PluginCmd::Update { name } => {
            use std::collections::HashSet;
            let targets: Vec<(String, std::path::PathBuf)> = match name {
                Some(n) if dir.join(&n).symlink_metadata().is_ok() => {
                    vec![(n.clone(), dir.join(&n))]
                }
                Some(n) => match list_plugins().into_iter().find(|p| p.name == n) {
                    Some(p) => vec![(p.id, p.path)],
                    None => anyhow::bail!("no plugin {n}"),
                },
                None => list_plugins().into_iter().map(|p| (p.id, p.path)).collect(),
            };
            let base = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            // Resolve each plugin to its backing git repo; pull ones we own (the
            // clone cache / standalone installs), skip dev links to your own repos.
            let mut roots: HashSet<std::path::PathBuf> = HashSet::new();
            for (handle, path) in targets {
                match git_root(&path) {
                    Some(root) if root.starts_with(&base) => {
                        roots.insert(root);
                    }
                    Some(_) => println!("· {handle} (dev link — edit its source)"),
                    None => println!("· {handle} (not a git repo)"),
                }
            }
            for root in roots {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["pull", "--ff-only"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let n = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
                println!("{} {n}", if ok { "✓" } else { "✗" });
            }
            println!("run `ruckus reload` to apply");
        }
        PluginCmd::Remove { name } => {
            // Accept the directory handle or the manifest name.
            let target = if dir.join(&name).symlink_metadata().is_ok() {
                dir.join(&name)
            } else if let Some(p) = list_plugins().into_iter().find(|p| p.name == name) {
                p.path
            } else {
                anyhow::bail!("no plugin {name}");
            };
            // symlink (dev link) vs real dir (installed)
            if target.symlink_metadata()?.file_type().is_symlink() {
                std::fs::remove_file(&target)?;
            } else {
                std::fs::remove_dir_all(&target)?;
            }
            println!("removed {name}");
        }
    }
    Ok(())
}

fn parse_toml_value(v: &str) -> toml_edit::Item {
    let attempt = format!("v = {v}");
    if let Ok(doc) = attempt.parse::<toml_edit::DocumentMut>() {
        return doc["v"].clone();
    }
    toml_edit::value(v)
}

async fn config_cmd(cmd: ConfigCmd) -> Result<()> {
    let path = config::ensure_config_file();
    let mut changed = false;
    match cmd {
        ConfigCmd::Path => {
            println!("{}", path.display());
        }
        ConfigCmd::List => {
            print!("{}", std::fs::read_to_string(&path)?);
        }
        ConfigCmd::Get { key } => {
            let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
            let mut item: &toml_edit::Item = doc.as_item();
            for part in key.split('.') {
                item = match item.get(part) {
                    Some(i) => i,
                    None => {
                        println!("unset");
                        return Ok(());
                    }
                };
            }
            println!("{}", item.to_string().trim());
        }
        ConfigCmd::Set { key, value } => {
            let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
            let parts: Vec<&str> = key.split('.').collect();
            let mut item = doc.as_item_mut();
            for part in &parts[..parts.len() - 1] {
                if item.get(part).is_none() {
                    item[part] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                item = &mut item[part];
            }
            item[parts[parts.len() - 1]] = parse_toml_value(&value);
            std::fs::write(&path, doc.to_string())?;
            println!("{key} = {}", value.trim());
            changed = true;
        }
        ConfigCmd::Unset { key } => {
            let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
            let parts: Vec<&str> = key.split('.').collect();
            let mut item = doc.as_item_mut();
            for part in &parts[..parts.len() - 1] {
                item = match item.get_mut(part) {
                    Some(i) => i,
                    None => return Ok(()),
                };
            }
            if let Some(t) = item.as_table_like_mut() {
                t.remove(parts[parts.len() - 1]);
            }
            std::fs::write(&path, doc.to_string())?;
            println!("{key} unset (default applies)");
            changed = true;
        }
    }
    if changed {
        reload_if_running().await;
    }
    Ok(())
}

fn print_tree(snap: &Snapshot) {
    for s in &snap.spaces {
        let active_s = if s.id == snap.active_space { "*" } else { " " };
        println!("{active_s} space {} — {}", s.id, s.name);
        for t in &s.tabs {
            let active_t = if t.id == s.active_tab { "*" } else { " " };
            println!("  {active_t} tab {} — {}", t.id, t.name);
            let mut leaves = Vec::new();
            t.layout.leaves(&mut leaves);
            for pane in leaves {
                if let Some(p) = snap.pane(pane) {
                    println!(
                        "      pane {:<4} {:<10} {:<20} {}",
                        p.id,
                        activity_label(p),
                        p.title,
                        p.cmd.join(" "),
                    );
                }
            }
        }
    }
}

fn activity_label(p: &PaneInfo) -> &'static str {
    match (p.activity, p.status) {
        (_, PaneStatus::Exited { code }) => {
            if code == 0 {
                "done"
            } else {
                "done(err)"
            }
        }
        (Activity::Working, _) => "working",
        (Activity::Waiting, _) => "waiting",
        (Activity::Idle, _) => "idle",
        (Activity::Done, _) => "done",
    }
}

async fn ls() -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    print_tree(&snap);
    Ok(())
}

async fn new_tab(name: Option<String>, detach: bool, cmd: Vec<String>) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let space = snap
        .spaces
        .iter()
        .find(|s| s.id == snap.active_space)
        .or(snap.spaces.first())
        .map(|s| s.id)
        .ok_or_else(|| anyhow::anyhow!("daemon has no spaces"))?;
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let msg = client
        .request(Request::NewTab {
            space,
            name,
            cmd,
            cwd,
        })
        .await?;
    if let ServerMsg::Created { pane, .. } = msg {
        if detach {
            println!("pane {pane} created");
            Ok(())
        } else {
            tui::run(Some(pane.to_string())).await
        }
    } else {
        anyhow::bail!("unexpected response")
    }
}

async fn kill(target: String) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    client.request(Request::ClosePane { pane: pane.id }).await?;
    println!("killed pane {} ({})", pane.id, pane.title);
    Ok(())
}

/// Stream granular lifecycle events (not the bulky full-state snapshots) as
/// newline-delimited JSON — the observe half of the plugin/agent API.
async fn events_cmd() -> Result<()> {
    use std::io::Write;
    ensure_daemon().await?;
    let (_client, mut events) = connect().await?;
    let mut out = std::io::stdout();
    while let Some(msg) = events.recv().await {
        if matches!(msg, ServerMsg::State { .. } | ServerMsg::Output { .. }) {
            continue;
        }
        writeln!(out, "{}", serde_json::to_string(&msg)?)?;
        out.flush()?;
    }
    Ok(())
}

async fn tail(target: String) -> Result<()> {
    use std::io::Write;
    ensure_daemon().await?;
    let (client, mut events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let msg = client
        .request(Request::Attach {
            pane: pane.id,
            rows,
            cols,
        })
        .await?;
    let mut out = std::io::stdout();
    if let ServerMsg::Attached { scrollback, .. } = msg {
        if let Ok(bytes) = B64.decode(scrollback.as_bytes()) {
            out.write_all(&bytes)?;
            out.flush()?;
        }
    }
    if pane.status != PaneStatus::Running {
        return Ok(());
    }
    while let Some(msg) = events.recv().await {
        match msg {
            ServerMsg::Output { pane: p, data } if p == pane.id => {
                if let Ok(bytes) = B64.decode(data.as_bytes()) {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
            }
            ServerMsg::Exited { pane: p, code } if p == pane.id => {
                eprintln!("\n[pane {p} exited with code {code}]");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::parse_plugin_ref;

    #[test]
    fn plugin_ref_parses_owner_repo_and_subpath() {
        let (url, key, sub) = parse_plugin_ref("owner/repo").unwrap();
        assert_eq!(url, "https://github.com/owner/repo");
        assert_eq!(key, "owner/repo");
        assert_eq!(sub, "");
        // monorepo subfolder
        let (url, key, sub) = parse_plugin_ref("owner/repo/plugins/gh-dash").unwrap();
        assert_eq!(url, "https://github.com/owner/repo");
        assert_eq!(key, "owner/repo");
        assert_eq!(sub, "plugins/gh-dash");
        // a bare name is not a valid ref
        assert!(parse_plugin_ref("justaname").is_err());
        // full URL: cloned as-is, no subpath, cache key from the tail
        let (url, key, sub) = parse_plugin_ref("https://gitlab.com/me/thing.git").unwrap();
        assert_eq!(url, "https://gitlab.com/me/thing.git");
        assert_eq!(key, "me/thing");
        assert_eq!(sub, "");
    }
}
