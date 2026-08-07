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

use client::{connect, ensure_daemon, resolve_pane};
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
