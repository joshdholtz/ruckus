//! Integration tests: spawn the real daemon on an isolated RUCKUS_DIR and
//! drive it over the unix socket exactly like a client/plugin would.
#![allow(clippy::zombie_processes)] // tests kill the daemon in teardown

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ruckus")
}

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    fn start(name: &str) -> Daemon {
        let dir = std::env::temp_dir().join(format!("ruckus-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Daemon::start_in(dir)
    }

    fn start_in(dir: PathBuf) -> Daemon {
        Daemon::start_in_env(dir, &[])
    }

    fn start_in_env(dir: PathBuf, env: &[(&str, &str)]) -> Daemon {
        let mut cmd = Command::new(bin());
        cmd.arg("daemon")
            .env("RUCKUS_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let sock = dir.join("ruckus.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if UnixStream::connect(&sock).is_ok() {
                return Daemon { child, dir };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not come up at {}", sock.display());
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One request over a fresh connection; returns the matching response `msg`.
fn rpc(dir: &Path, req: Value) -> Value {
    let mut stream = UnixStream::connect(dir.join("ruckus.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let frame = json!({ "seq": 1, "req": req });
    stream.write_all(format!("{frame}\n").as_bytes()).unwrap();
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let line = line.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("seq").and_then(Value::as_u64) == Some(1) {
            return v["msg"].clone();
        }
    }
    panic!("no response");
}

fn snapshot(dir: &Path) -> Value {
    let msg = rpc(dir, json!({"type": "snapshot"}));
    assert_eq!(msg["type"], "state");
    msg["snapshot"].clone()
}

fn pane_by_id(snap: &Value, id: u64) -> Option<Value> {
    snap["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_u64() == Some(id))
        .cloned()
}

fn wait_for<F: Fn(&Value) -> bool>(dir: &Path, pred: F, what: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let snap = snapshot(dir);
        if pred(&snap) {
            return snap;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn boots_with_default_space() {
    let d = Daemon::start("boot");
    let snap = snapshot(&d.dir);
    let spaces = snap["spaces"].as_array().unwrap();
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0]["name"], "main");
    assert_eq!(spaces[0]["tabs"].as_array().unwrap().len(), 1);
}

#[test]
fn tab_lifecycle_and_exit_detection() {
    let d = Daemon::start("lifecycle");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();

    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": null,
               "cmd": ["sh", "-c", "printf ready; sleep 30"], "cwd": null}),
    );
    assert_eq!(msg["type"], "created");
    let pane = msg["pane"].as_u64().unwrap();

    // shows up running
    let snap = wait_for(
        &d.dir,
        |s| pane_by_id(s, pane).is_some(),
        "new pane in snapshot",
    );
    assert_eq!(
        pane_by_id(&snap, pane).unwrap()["status"]["state"],
        "running"
    );

    // a quick script exits and is detected with its code
    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": null,
               "cmd": ["sh", "-c", "exit 3"], "cwd": null}),
    );
    let quick = msg["pane"].as_u64().unwrap();
    let snap = wait_for(
        &d.dir,
        |s| {
            pane_by_id(s, quick)
                .map(|p| p["status"]["state"] == "exited")
                .unwrap_or(false)
        },
        "quick pane to exit",
    );
    let p = pane_by_id(&snap, quick).unwrap();
    assert_eq!(p["status"]["code"], 3);
    assert_eq!(p["activity"], "done");

    // close removes the pane and its tab
    rpc(&d.dir, json!({"type": "close_pane", "pane": pane}));
    let snap = snapshot(&d.dir);
    assert!(pane_by_id(&snap, pane).is_none());
}

#[test]
fn rename_and_restart() {
    let d = Daemon::start("rename");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();

    rpc(
        &d.dir,
        json!({"type": "rename_space", "space": space, "name": "workbench"}),
    );
    let snap = snapshot(&d.dir);
    assert_eq!(snap["spaces"][0]["name"], "workbench");

    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": null,
               "cmd": ["sh", "-c", "exit 5"], "cwd": null}),
    );
    let pane = msg["pane"].as_u64().unwrap();
    let tab = msg["tab"].as_u64().unwrap();

    rpc(
        &d.dir,
        json!({"type": "rename_tab", "tab": tab, "name": "flaky"}),
    );
    let snap = snapshot(&d.dir);
    let tabs = snap["spaces"][0]["tabs"].as_array().unwrap();
    assert!(tabs.iter().any(|t| t["name"] == "flaky"));

    wait_for(
        &d.dir,
        |s| {
            pane_by_id(s, pane)
                .map(|p| p["status"]["state"] == "exited")
                .unwrap_or(false)
        },
        "pane exit before restart",
    );
    let msg = rpc(&d.dir, json!({"type": "restart", "pane": pane}));
    assert_eq!(msg["type"], "done", "restart failed: {msg}");
    let snap = snapshot(&d.dir);
    assert_eq!(
        pane_by_id(&snap, pane).unwrap()["status"]["state"],
        "running"
    );
}

#[test]
fn restart_refuses_running_pane() {
    let d = Daemon::start("restart-guard");
    let snap = snapshot(&d.dir);
    let pane = snap["spaces"][0]["tabs"][0]["active_pane"]
        .as_u64()
        .unwrap();
    let msg = rpc(&d.dir, json!({"type": "restart", "pane": pane}));
    assert_eq!(msg["type"], "error");
}

#[test]
fn layout_and_weights_survive_set_layout() {
    let d = Daemon::start("layout");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();
    let tab = snap["spaces"][0]["tabs"][0]["id"].as_u64().unwrap();
    let first = snap["spaces"][0]["tabs"][0]["active_pane"]
        .as_u64()
        .unwrap();

    let msg = rpc(
        &d.dir,
        json!({"type": "split", "pane": first, "dir": "right",
               "cmd": ["sleep", "30"], "cwd": null}),
    );
    let second = msg["pane"].as_u64().unwrap();
    let _ = space;

    let layout = json!({"kind": "split", "dir": "right",
        "children": [{"kind": "leaf", "pane": first}, {"kind": "leaf", "pane": second}],
        "weights": [25, 75]});
    let msg = rpc(
        &d.dir,
        json!({"type": "set_layout", "tab": tab, "layout": layout}),
    );
    assert_eq!(msg["type"], "done", "{msg}");
    let snap = snapshot(&d.dir);
    assert_eq!(
        snap["spaces"][0]["tabs"][0]["layout"]["weights"],
        json!([25, 75])
    );

    // wrong pane set is rejected
    let bad = json!({"kind": "leaf", "pane": 9999});
    let msg = rpc(
        &d.dir,
        json!({"type": "set_layout", "tab": tab, "layout": bad}),
    );
    assert_eq!(msg["type"], "error");
}

#[test]
fn state_survives_daemon_restart() {
    let mut d = Daemon::start("persist");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();

    rpc(
        &d.dir,
        json!({"type": "rename_space", "space": space, "name": "keeper"}),
    );
    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": "surviving-tab",
               "cmd": ["sleep", "300"], "cwd": null}),
    );
    let pane = msg["pane"].as_u64().unwrap();
    // wait until state.json reflects the tab
    wait_for(&d.dir, |s| pane_by_id(s, pane).is_some(), "pane created");

    let dir = d.dir.clone();
    d.kill();
    drop(d);
    std::thread::sleep(Duration::from_millis(200));

    let d2 = Daemon::start_in(dir);
    let snap = snapshot(&d2.dir);
    assert_eq!(snap["spaces"][0]["name"], "keeper", "space name persisted");
    let tabs = snap["spaces"][0]["tabs"].as_array().unwrap();
    assert!(
        tabs.iter().any(|t| t["name"] == "surviving-tab"),
        "tab persisted: {tabs:?}"
    );
    let p = pane_by_id(&snap, pane).expect("pane respawned under the same id");
    assert_eq!(p["status"]["state"], "running");
    assert_eq!(p["cmd"], json!(["sleep", "300"]));
}

#[test]
fn cli_config_roundtrip() {
    let dir = std::env::temp_dir().join(format!("ruckus-test-{}-cfg", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new(bin())
            .args(args)
            .env("RUCKUS_DIR", &dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    run(&["config", "set", "ui.gutter", "3"]);
    assert_eq!(run(&["config", "get", "ui.gutter"]).trim(), "3");
    run(&["config", "set", "keys.quit", r#"["alt-q","ctrl-q"]"#]);
    assert!(run(&["config", "get", "keys.quit"]).contains("ctrl-q"));
    run(&["config", "set", "theme.accent", "\"#ff00ff\""]);
    assert!(run(&["config", "get", "theme.accent"]).contains("#ff00ff"));
    run(&["config", "unset", "ui.gutter"]);
    assert_eq!(run(&["config", "get", "ui.gutter"]).trim(), "unset");
    // comments in the default config survive edits
    let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(text.contains("# ruckus config"));
}

#[test]
fn cli_resolves_pane_by_tab_name() {
    let d = Daemon::start("resolve");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();
    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": "named-tab",
               "cmd": ["sleep", "300"], "cwd": null}),
    );
    let pane = msg["pane"].as_u64().unwrap();
    let _ = pane;

    // `ruckus focus named-tab` should work even though the pane title is "sleep·N"
    let out = Command::new(bin())
        .args(["focus", "named-tab"])
        .env("RUCKUS_DIR", &d.dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "focus by tab name failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn reload_broadcasts_config_changed() {
    use std::io::Read;
    let d = Daemon::start("reload");

    // Listener connection: register it by sending a snapshot, then watch for the
    // unsolicited ConfigChanged event.
    let mut listener = UnixStream::connect(d.dir.join("ruckus.sock")).unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    listener
        .write_all(b"{\"seq\":99,\"req\":{\"type\":\"snapshot\"}}\n")
        .unwrap();

    // Trigger reload from a second connection.
    let msg = rpc(&d.dir, json!({"type": "reload"}));
    assert_eq!(msg["type"], "done");

    // The listener should receive a config_changed event (seq-less broadcast).
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while Instant::now() < deadline {
        match listener.read(&mut byte) {
            Ok(1) => {
                if byte[0] == b'\n' {
                    let line = String::from_utf8_lossy(&buf);
                    if line.contains("config_changed") {
                        saw = true;
                        break;
                    }
                    buf.clear();
                } else {
                    buf.push(byte[0]);
                }
            }
            _ => break,
        }
    }
    assert!(saw, "listener never received config_changed broadcast");
}

#[test]
fn report_activity_overrides_heuristic() {
    let d = Daemon::start("report-act");
    let snap = snapshot(&d.dir);
    let pane = snap["spaces"][0]["tabs"][0]["active_pane"]
        .as_u64()
        .unwrap();

    let msg = rpc(
        &d.dir,
        json!({"type": "report_activity", "pane": pane, "state": "waiting"}),
    );
    assert_eq!(msg["type"], "done", "{msg}");
    let snap = snapshot(&d.dir);
    assert_eq!(pane_by_id(&snap, pane).unwrap()["activity"], "waiting");

    let msg = rpc(
        &d.dir,
        json!({"type": "report_agent", "pane": pane, "name": "claude"}),
    );
    assert_eq!(msg["type"], "done", "{msg}");
    let snap = snapshot(&d.dir);
    assert_eq!(pane_by_id(&snap, pane).unwrap()["agent"], "claude");

    // auto hands control back (activity may stay until heuristic re-evaluates)
    let msg = rpc(
        &d.dir,
        json!({"type": "report_activity", "pane": pane, "state": "auto"}),
    );
    assert_eq!(msg["type"], "done", "{msg}");
}

#[test]
fn multiple_requests_on_one_connection() {
    // Exercises the framed reader: several newline-delimited requests sent
    // back-to-back on a single connection must each get their matching response.
    let d = Daemon::start("multiplex");
    let mut stream = UnixStream::connect(d.dir.join("ruckus.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // Send three snapshots at once (all in one write, no delays).
    let mut out = String::new();
    for seq in 1..=3 {
        out.push_str(&json!({"seq": seq, "req": {"type": "snapshot"}}).to_string());
        out.push('\n');
    }
    stream.write_all(out.as_bytes()).unwrap();

    let reader = BufReader::new(stream);
    let mut seen = std::collections::HashSet::new();
    for line in reader.lines() {
        let line = line.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if let Some(seq) = v.get("seq").and_then(Value::as_u64) {
            assert_eq!(v["msg"]["type"], "state");
            seen.insert(seq);
            if seen.len() == 3 {
                break;
            }
        }
    }
    assert_eq!(seen, [1, 2, 3].into_iter().collect());
}

/// Read frames from a proxy's stdout until one with `seq` arrives (skipping
/// interleaved seqless event frames).
fn read_seq<R: BufRead>(r: &mut R, seq: u64) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(r.read_line(&mut line).unwrap() > 0, "proxy closed");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        if v.get("seq").and_then(Value::as_u64) == Some(seq) {
            return v;
        }
    }
}

/// The SSH-mirror transport: `ruckus __proxy` relays the daemon socket over
/// stdio, so a client can read AND write through it (no real SSH needed here).
#[test]
fn proxy_relays_read_and_write() {
    let d = Daemon::start("proxy");
    let mut proxy = Command::new(bin())
        .arg("__proxy")
        .env("RUCKUS_DIR", &d.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pin = proxy.stdin.take().unwrap();
    let mut pout = BufReader::new(proxy.stdout.take().unwrap());

    // read: snapshot through the proxy
    pin.write_all(b"{\"seq\":1,\"req\":{\"type\":\"snapshot\"}}\n")
        .unwrap();
    assert_eq!(read_seq(&mut pout, 1)["msg"]["type"], "state");

    // write: create a space through the proxy...
    pin.write_all(
        b"{\"seq\":2,\"req\":{\"type\":\"new_space\",\"name\":\"viaproxy\",\"cwd\":null}}\n",
    )
    .unwrap();
    assert_eq!(read_seq(&mut pout, 2)["msg"]["type"], "created");

    // ...and confirm it landed by hitting the daemon socket directly.
    let direct = snapshot(&d.dir);
    assert!(direct["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "viaproxy"));

    let _ = proxy.kill();
}

fn space_by_name<'a>(snap: &'a Value, name: &str) -> Option<&'a Value> {
    snap["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
}

/// End-to-end hybrid remote mirror: daemon A connects daemon B (via a RUCKUS_SSH
/// shim that runs `ruckus __proxy` against B's dir — no real SSH), B's spaces
/// mirror into A with origin-encoded ids, A can write through to B (create a tab
/// on the remote space), and disconnect drops the mirror. This exercises the
/// whole daemon-side hub: connect, prefix, merge, route, disconnect.
#[test]
fn daemon_mirrors_remote_read_write_disconnect() {
    // B = the "remote" daemon, with a recognisable space.
    let b = Daemon::start("hub-remote");
    let bsnap = snapshot(&b.dir);
    let bspace = bsnap["spaces"][0]["id"].as_u64().unwrap();
    rpc(
        &b.dir,
        json!({"type": "rename_space", "space": bspace, "name": "remoteland"}),
    );

    // The transport shim: ignore all ssh args/host, just proxy B's socket.
    let shim = b.dir.join("ssh-shim.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nexec env RUCKUS_DIR='{}' '{}' __proxy\n",
            b.dir.display(),
            bin()
        ),
    )
    .unwrap();
    let mut perm = std::fs::metadata(&shim).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&shim, perm).unwrap();

    // A = the local daemon, told to use the shim as its ssh transport.
    let a = Daemon::start_in_env(
        std::env::temp_dir().join(format!("ruckus-test-{}-hub-local", std::process::id())),
        &[("RUCKUS_SSH", shim.to_str().unwrap())],
    );

    // Connect B into A.
    let msg = rpc(
        &a.dir,
        json!({"type": "connect_remote", "host": "bee", "args": [], "env": {}}),
    );
    assert_eq!(msg["type"], "done", "{msg}");

    // B's space mirrors into A with an origin-encoded id (id >> 48 != 0), and A
    // reports the host label for that origin.
    let snap = wait_for(
        &a.dir,
        |s| space_by_name(s, "remoteland").is_some(),
        "remote space mirrored into A",
    );
    let rspace = space_by_name(&snap, "remoteland").unwrap();
    let rid = rspace["id"].as_u64().unwrap();
    let origin = rid >> 48;
    assert_ne!(
        origin, 0,
        "mirrored space must carry a non-zero origin: {rid}"
    );
    assert_eq!(
        snap["remote_hosts"][origin.to_string()],
        "bee",
        "A exposes origin→host: {}",
        snap["remote_hosts"]
    );

    // Write through A to B: create a tab on the REMOTE space (origin-encoded id).
    let msg = rpc(
        &a.dir,
        json!({"type": "new_tab", "space": rid, "name": "made-remotely",
               "cmd": ["sleep", "300"], "cwd": null}),
    );
    assert_eq!(msg["type"], "created", "write-through failed: {msg}");
    let rpane = msg["pane"].as_u64().unwrap();
    assert_eq!(rpane >> 48, origin, "created pane keeps the remote origin");

    // It shows up mirrored in A...
    wait_for(
        &a.dir,
        |s| pane_by_id(s, rpane).is_some(),
        "remotely-created pane mirrored into A",
    );
    // ...and actually landed on B (its local id is the low bits).
    let blocal = rpane & 0x0000_FFFF_FFFF_FFFF;
    wait_for(
        &b.dir,
        |s| pane_by_id(s, blocal).is_some(),
        "the tab really exists on B",
    );

    // Disconnect drops the mirror from A (B keeps running).
    let msg = rpc(
        &a.dir,
        json!({"type": "disconnect_remote", "origin": origin}),
    );
    assert_eq!(msg["type"], "done", "{msg}");
    let snap = wait_for(
        &a.dir,
        |s| space_by_name(s, "remoteland").is_none(),
        "remote space removed after disconnect",
    );
    assert!(
        snap["remote_hosts"]
            .as_object()
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "no remote hosts after disconnect: {}",
        snap["remote_hosts"]
    );
    // B is unaffected.
    assert!(space_by_name(&snapshot(&b.dir), "remoteland").is_some());
}

/// Write an executable ssh-shim that ignores its args and proxies `dir`'s daemon.
fn write_ssh_shim(at: &Path, target_dir: &Path) {
    std::fs::write(
        at,
        format!(
            "#!/bin/sh\nexec env RUCKUS_DIR='{}' '{}' __proxy\n",
            target_dir.display(),
            bin()
        ),
    )
    .unwrap();
    let mut perm = std::fs::metadata(at).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(at, perm).unwrap();
}

/// Regression for the freeze bug: a mirrored remote streaming continuous output
/// must not starve the local request handler. Pre-fix, remote_event_loop held
/// the state mutex across save_state (fsync) and relocked per frame, so an
/// attached streaming remote wedged the daemon — local snapshots hung forever.
#[test]
fn remote_stream_does_not_freeze_local() {
    // B: a remote whose pane streams output as fast as it can.
    let b = Daemon::start("freeze-remote");
    let bspace = snapshot(&b.dir)["spaces"][0]["id"].as_u64().unwrap();
    rpc(
        &b.dir,
        json!({"type":"new_tab","space":bspace,"name":"streamer",
               "cmd":["sh","-c","while true; do printf 'xxxxxxxx'; done"],"cwd":null}),
    );

    let shim = b.dir.join("ssh-shim.sh");
    write_ssh_shim(&shim, &b.dir);
    let a = Daemon::start_in_env(
        std::env::temp_dir().join(format!("ruckus-test-{}-freeze-local", std::process::id())),
        &[("RUCKUS_SSH", shim.to_str().unwrap())],
    );
    rpc(
        &a.dir,
        json!({"type":"connect_remote","host":"bee","args":[],"env":{}}),
    );

    // The remote pane mirrors in with an origin-encoded id.
    let snap = wait_for(
        &a.dir,
        |s| {
            s["panes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"].as_u64().map(|id| id >> 48 != 0).unwrap_or(false))
        },
        "remote streaming pane mirrored into A",
    );
    let rpane = snap["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| p["id"].as_u64().filter(|id| id >> 48 != 0))
        .unwrap();

    // Attach it so B actually streams Output through the proxy into A's event loop
    // (that's the hot path that used to wedge the daemon).
    let att = rpc(
        &a.dir,
        json!({"type":"attach","pane":rpane,"rows":24,"cols":80}),
    );
    assert_eq!(att["type"], "attached", "{att}");

    // Now hammer local requests while the remote floods output. Each snapshot()
    // panics if A is frozen (its read times out), and the whole batch must finish
    // quickly — a starved handler would blow the wall-clock bound.
    let start = Instant::now();
    for _ in 0..20 {
        let s = snapshot(&a.dir);
        assert!(!s["spaces"].as_array().unwrap().is_empty());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "local requests starved under remote streaming: {elapsed:?}"
    );
}
