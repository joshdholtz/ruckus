mod client;
mod config;
mod daemon;
mod protocol;
mod render;
mod tui;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::{Parser, Subcommand};

use client::{connect, ensure_daemon, locate_pane, resolve_pane};
use protocol::*;

#[derive(Parser)]
#[command(name = "ruckus", version, about = "a persistent runtime for your coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (normally started automatically)
    Daemon,
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
    /// Open the TUI focused on a pane (by id or title)
    Attach { target: String },
    /// Kill a pane's process and remove it (by id or title)
    Kill { target: String },
    /// Print a pane's scrollback and follow its output (by id or title)
    Tail { target: String },
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
    /// Read or edit config.toml (comment-preserving)
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => tui::run(None).await,
        Some(Cmd::Daemon) => daemon::run().await,
        Some(Cmd::Ls) => ls().await,
        Some(Cmd::New { name, detach, cmd }) => new_tab(name, detach, cmd).await,
        Some(Cmd::Attach { target }) => tui::run(Some(target)).await,
        Some(Cmd::Kill { target }) => kill(target).await,
        Some(Cmd::Tail { target }) => tail(target).await,
        Some(Cmd::Status { json }) => status(json).await,
        Some(Cmd::Send { target, no_enter, text }) => send(target, no_enter, text).await,
        Some(Cmd::Split { target, direction, cmd }) => split(target, direction, cmd).await,
        Some(Cmd::Focus { target }) => focus(target).await,
        Some(Cmd::Rename { kind, id, name }) => rename(kind, id, name).await,
        Some(Cmd::Restart { target }) => restart(target).await,
        Some(Cmd::Config { cmd }) => config_cmd(cmd),
    }
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
        .request(Request::Input { pane: pane.id, data: B64.encode(&bytes) })
        .await?;
    println!("sent to pane {} ({})", pane.id, pane.title);
    Ok(())
}

async fn split(target: String, direction: String, cmd: Vec<String>) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let dir = if direction == "right" { Dir::Right } else { Dir::Down };
    let cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
    let msg = client
        .request(Request::Split { pane: pane.id, dir, cmd, cwd })
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
    client.request(Request::SetActive { space, tab, pane: pane.id }).await?;
    println!("focused pane {} ({})", pane.id, pane.title);
    Ok(())
}

async fn rename(kind: String, id: u64, name: String) -> Result<()> {
    ensure_daemon().await?;
    let (client, _events) = connect().await?;
    let req = if kind == "space" {
        Request::RenameSpace { space: id, name: name.clone() }
    } else {
        Request::RenameTab { tab: id, name: name.clone() }
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

fn parse_toml_value(v: &str) -> toml_edit::Item {
    let attempt = format!("v = {v}");
    if let Ok(doc) = attempt.parse::<toml_edit::DocumentMut>() {
        return doc["v"].clone();
    }
    toml_edit::value(v)
}

fn config_cmd(cmd: ConfigCmd) -> Result<()> {
    let path = config::ensure_config_file();
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
        }
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
    let cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
    let msg = client
        .request(Request::NewTab { space, name, cmd, cwd })
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

async fn tail(target: String) -> Result<()> {
    use std::io::Write;
    ensure_daemon().await?;
    let (client, mut events) = connect().await?;
    let snap = client.snapshot().await?;
    let pane = resolve_pane(&snap, &target)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let msg = client
        .request(Request::Attach { pane: pane.id, rows, cols })
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
