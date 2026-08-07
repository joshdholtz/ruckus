//! Integration tests: spawn the real daemon on an isolated RUCKUS_DIR and
//! drive it over the unix socket exactly like a client/plugin would.

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
        let child = Command::new(bin())
            .arg("daemon")
            .env("RUCKUS_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
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
    stream
        .write_all(format!("{frame}\n").as_bytes())
        .unwrap();
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
    assert_eq!(pane_by_id(&snap, pane).unwrap()["status"]["state"], "running");

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

    rpc(&d.dir, json!({"type": "rename_space", "space": space, "name": "workbench"}));
    let snap = snapshot(&d.dir);
    assert_eq!(snap["spaces"][0]["name"], "workbench");

    let msg = rpc(
        &d.dir,
        json!({"type": "new_tab", "space": space, "name": null,
               "cmd": ["sh", "-c", "exit 5"], "cwd": null}),
    );
    let pane = msg["pane"].as_u64().unwrap();
    let tab = msg["tab"].as_u64().unwrap();

    rpc(&d.dir, json!({"type": "rename_tab", "tab": tab, "name": "flaky"}));
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
    assert_eq!(pane_by_id(&snap, pane).unwrap()["status"]["state"], "running");
}

#[test]
fn restart_refuses_running_pane() {
    let d = Daemon::start("restart-guard");
    let snap = snapshot(&d.dir);
    let pane = snap["spaces"][0]["tabs"][0]["active_pane"].as_u64().unwrap();
    let msg = rpc(&d.dir, json!({"type": "restart", "pane": pane}));
    assert_eq!(msg["type"], "error");
}

#[test]
fn layout_and_weights_survive_set_layout() {
    let d = Daemon::start("layout");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();
    let tab = snap["spaces"][0]["tabs"][0]["id"].as_u64().unwrap();
    let first = snap["spaces"][0]["tabs"][0]["active_pane"].as_u64().unwrap();

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
    let msg = rpc(&d.dir, json!({"type": "set_layout", "tab": tab, "layout": layout}));
    assert_eq!(msg["type"], "done", "{msg}");
    let snap = snapshot(&d.dir);
    assert_eq!(snap["spaces"][0]["tabs"][0]["layout"]["weights"], json!([25, 75]));

    // wrong pane set is rejected
    let bad = json!({"kind": "leaf", "pane": 9999});
    let msg = rpc(&d.dir, json!({"type": "set_layout", "tab": tab, "layout": bad}));
    assert_eq!(msg["type"], "error");
}

#[test]
fn state_survives_daemon_restart() {
    let mut d = Daemon::start("persist");
    let snap = snapshot(&d.dir);
    let space = snap["spaces"][0]["id"].as_u64().unwrap();

    rpc(&d.dir, json!({"type": "rename_space", "space": space, "name": "keeper"}));
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
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
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
    listener.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
