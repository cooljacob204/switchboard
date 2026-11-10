//! A workspace on another computer (src/remote): a client attached as
//! remote gets the links, copies and notifications made in the workspace
//! and is asked to forward the ports its panes listen on; and `switchboard
//! ssh`, through a strict stand-in for ssh whose host is a second home of
//! the test's own, installs this build there, runs its guided setup in the
//! terminal, starts the host's server and attaches to it, forwarding a
//! port, opening a link here, and stopping its forwards as it detaches.

use crate::common;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{BIN, TestClient, TestServer, stand_in};
use serde_json::{Value, json};
use switchboard::mux::proto::{ClientMsg, Desk};

/// Not a test of its own: what a pane runs to listen on a port, when
/// `SB_TEST_LISTEN` names a file to write the port to.
#[test]
fn listener() {
    let Some(file) = std::env::var_os("SB_TEST_LISTEN") else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(&file, port.to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(300));
}

/// The command for a pane that listens on a port, writing it to `file`.
fn listen_command(file: &Path) -> Vec<String> {
    let me = std::env::current_exe().unwrap();
    vec![
        "env".into(),
        format!("SB_TEST_LISTEN={}", file.display()),
        me.to_string_lossy().into_owned(),
        "--exact".into(),
        "remote::listener".into(),
        "--nocapture".into(),
        "--test-threads".into(),
        "1".into(),
    ]
}

/// The port the listener wrote, once it has.
fn port_in(file: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(port) = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            return port;
        }
        assert!(
            Instant::now() < deadline,
            "the listener never said its port"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pred() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs `line` in the pane in front, as typed.
fn type_line(server: &TestServer, line: &str) {
    server.ok(&["send-keys", "-l", line]);
    server.ok(&["send-keys", "Enter"]);
}

fn desktop_log(server: &TestServer) -> String {
    std::fs::read_to_string(server.home.join("desktop-log")).unwrap_or_default()
}

#[test]
fn a_link_a_copy_and_a_notification_made_in_a_pane_go_to_the_remote_client() {
    let server = TestServer::start();
    let mut remote = TestClient::attach_remote(&server.socket(), 120, 32);
    remote.wait_text("$");
    type_line(&server, &format!("{BIN} open-url https://example.com/a"));
    let opened = remote.wait_desk("the link", |d| matches!(d, Desk::Open { .. }));
    assert_eq!(
        opened,
        Desk::Open {
            url: "https://example.com/a".into(),
            as_site: None
        }
    );
    type_line(&server, &format!("printf copied | {BIN} platform copy"));
    remote.wait_desk("the copy", |d| {
        *d == Desk::Copy {
            text: "copied".into(),
        }
    });
    type_line(
        &server,
        &format!("{BIN} platform notify 'From the pane' 'it is done'"),
    );
    remote.wait_desk("the notification", |d| {
        *d == Desk::Notify {
            title: "From the pane".into(),
            text: "it is done".into(),
        }
    });
    let log = desktop_log(&server);
    assert!(
        !log.contains("example.com") && !log.contains("copied"),
        "nothing was done on the workspace's computer: {log}"
    );
    let list: Value = server.list();
    assert_eq!(list["clients"][0]["remote"], true);
}

#[test]
fn with_your_computers_desktop_off_a_link_opens_where_the_workspace_runs() {
    let server = TestServer::start_with_settings(r#"{"remote_local_desktop": false}"#);
    let mut remote = TestClient::attach_remote(&server.socket(), 120, 32);
    remote.wait_text("$");
    type_line(&server, &format!("{BIN} open-url https://example.com/here"));
    wait_for("the link opened on the workspace's computer", || {
        desktop_log(&server).contains("https://example.com/here")
    });
    remote.screen();
    assert!(remote.desk.is_empty(), "{:?}", remote.desk);
}

#[test]
fn with_no_remote_client_the_desktop_command_hands_nothing_on() {
    let server = TestServer::start();
    let mut local = server.attach(100, 30);
    local.wait_text("$");
    let (code, _, err) = server.run(&["desktop", "open", "https://example.com/"]);
    assert_eq!(code, 3, "{err}");
    let (code, _, err) = server.run(&["desktop", "paste"]);
    assert_eq!(code, 1);
    assert!(err.contains("usage"), "{err}");
    // A program in a pane then does it itself, as before.
    type_line(
        &server,
        &format!("{BIN} open-url https://example.com/local"),
    );
    wait_for("the link opened here", || {
        desktop_log(&server).contains("https://example.com/local")
    });
}

/// Waits until `list` satisfies `pred`, giving it.
fn wait_list(server: &TestServer, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list = server.list();
        if pred(&list) {
            return list;
        }
        assert!(Instant::now() < deadline, "never {what}: {list}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_port_a_pane_listens_on_is_forwarded_through_the_remote_client_and_stopped_as_it_closes() {
    let server = TestServer::start();
    let mut remote = TestClient::attach_remote(&server.socket(), 140, 36);
    remote.wait_text("$");
    let file = server.dir.path().join("port");
    let mut args = vec![
        "new-tab".to_string(),
        "-n".into(),
        "dev".into(),
        "--".into(),
    ];
    args.extend(listen_command(&file));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let made = server.ok(&args);
    let port = port_in(&file);
    let forward = remote.wait_desk(
        "the port forwarded",
        |d| matches!(d, Desk::Forward { port: p, .. } if *p == port),
    );
    assert_eq!(
        forward,
        Desk::Forward {
            port,
            to: "localhost".into()
        }
    );
    let local = port.wrapping_add(1).max(1);
    remote.send_msg(&ClientMsg::Forwarded {
        port,
        local: Some(local),
        error: None,
    });
    let list = wait_list(&server, "the forward taken up", |l| {
        l["ports"].as_array().is_some_and(|ports| {
            ports
                .iter()
                .any(|p| p["port"] == port && p["local"] == local)
        })
    });
    let shown = list["ports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["port"] == port)
        .unwrap()
        .clone();
    assert_eq!(shown["addr"], "127.0.0.1");
    // The tab's card says where it is here.
    remote.wait_text(&format!(":{port}→{local}"));
    // The tab closed, the port closes, and its forward is stopped.
    let tab = made.split_whitespace().next().unwrap().to_string();
    server.ok(&["kill-tab", "-t", &tab]);
    remote.wait_desk("the forward stopped", |d| *d == Desk::Unforward { port });
    wait_list(&server, "the port gone", |l| {
        l["ports"].as_array().is_some_and(Vec::is_empty)
    });
}

#[test]
fn a_port_is_left_alone_with_forwarding_off_and_forwarded_from_the_palette() {
    let server = TestServer::start_with_settings(r#"{"remote_forward_ports": false}"#);
    let mut remote = TestClient::attach_remote(&server.socket(), 140, 36);
    remote.wait_text("$");
    let file = server.dir.path().join("port");
    let mut args = vec!["new-tab".to_string(), "--".into()];
    args.extend(listen_command(&file));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    server.ok(&args);
    let port = port_in(&file);
    wait_list(&server, "the port read", |l| {
        l["ports"]
            .as_array()
            .is_some_and(|ports| ports.iter().any(|p| p["port"] == port))
    });
    remote.screen();
    assert!(
        !remote
            .desk
            .iter()
            .any(|d| matches!(d, Desk::Forward { .. })),
        "{:?}",
        remote.desk
    );
    // M-p's "Forward a port", by its name in keys.json, a port typed.
    server.ok(&["press", "forward-port"]);
    remote.wait_text("Forward a port");
    remote.type_text("9999\r");
    remote.wait_desk("the port typed forwarded", |d| {
        *d == Desk::Forward {
            port: 9999,
            to: "localhost".into(),
        }
    });
}

// ── switchboard ssh ─────────────────────────────────────────────────────────

/// The host's server, ended when the test is, however it ends.
struct HostServer(PathBuf);

impl Drop for HostServer {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("kill-server")
            .env("SWITCHBOARD", &self.0)
            .output();
    }
}

/// A command asked of the host's server.
fn on_host(socket: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .env("SWITCHBOARD", socket)
        .env_remove("SWITCHBOARD_PANE")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn host_list(socket: &Path) -> Option<Value> {
    let (code, out) = on_host(socket, &["list"]);
    (code == 0)
        .then(|| serde_json::from_str(&out).ok())
        .flatten()
}

/// A workspace here, a host for it to reach as `devbox` (a second home of
/// the test's own, its settings quiet), the ssh stand-in's log, and the
/// host's socket, its server ended with the test.
struct Remote {
    local: TestServer,
    host: PathBuf,
    log: PathBuf,
    socket: PathBuf,
    _server: HostServer,
}

fn with_a_host() -> Remote {
    let local = TestServer::start();
    let host = local.dir.path().join("host");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join(".profile"), "PS1='$ '\n").unwrap();
    common::write_settings(&host, json!({}));
    let log = local.dir.path().join("ssh-log");
    stand_in::ssh(&local.home.join("bin"), &["devbox"], &host, &log);
    let socket = host.join(".local/state/switchboard/sock");
    Remote {
        local,
        host,
        log,
        socket: socket.clone(),
        _server: HostServer(socket),
    }
}

/// Whether the host's server has this terminal attached as remote.
fn attached_remote(socket: &Path) -> bool {
    host_list(socket).is_some_and(|l| l["clients"][0]["remote"] == true)
}

#[test]
fn switchboard_ssh_installs_this_build_attaches_forwards_and_opens_links_here() {
    let Remote {
        local,
        host,
        log,
        socket: host_socket,
        _server,
    } = with_a_host();
    let mut client = local.attach(150, 40);
    client.wait_text("$");
    type_line(&local, &format!("{BIN} ssh devbox"));
    client.wait_text("install it in ~/.local/bin?");
    type_line(&local, "");
    // A first install: its guided setup, run there in this terminal.
    client.wait_text("Run Switchboard's setup on devbox now?");
    type_line(&local, "");
    client.wait_text("Add Switchboard's statusline and permissions");
    type_line(&local, "");
    // Installed, its server started, and this terminal attached to it as
    // remote.
    client.wait_for("the host's server with a remote client", |_| {
        attached_remote(&host_socket)
    });
    let installed = Command::new(host.join(".local/bin/switchboard"))
        .arg("--version")
        .output()
        .unwrap();
    let mine = Command::new(BIN).arg("--version").output().unwrap();
    assert_eq!(installed.stdout, mine.stdout, "this build, exactly");
    // Its guided setup ran there, the statusline said yes to.
    let claude_settings =
        std::fs::read_to_string(host.join(".claude/settings.json")).unwrap_or_default();
    assert!(
        claude_settings.contains("switchboard statusline"),
        "its setup ran there: {claude_settings}"
    );
    // What the pane shows is the host's workspace.
    let tab = host_list(&host_socket).unwrap()["sessions"][0]["tabs"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    on_host(&host_socket, &["rename-tab", "-t", &tab, "on-the-host"]);
    client.wait_text("on-the-host");

    // A port a pane there listens on is forwarded through the connection,
    // to the next port here, since this test's host is this computer.
    let file = local.dir.path().join("port");
    let mut args = vec!["new-tab".to_string(), "-d".into(), "--".into()];
    args.extend(listen_command(&file));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    on_host(&host_socket, &args);
    let port = port_in(&file);
    let forwarded = |log: &str| {
        log.lines()
            .find(|l| {
                l.contains("-O forward -L 127.0.0.1:") && l.contains(&format!(":localhost:{port} "))
            })
            .map(str::to_string)
    };
    wait_for("the port forwarded", || {
        forwarded(&std::fs::read_to_string(&log).unwrap_or_default()).is_some()
    });
    wait_for("the host's server told where it is here", || {
        host_list(&host_socket).is_some_and(|l| {
            l["ports"]
                .as_array()
                .is_some_and(|p| p.iter().any(|p| p["port"] == port && p["local"].is_u64()))
        })
    });

    // A link opened there opens here.
    on_host(
        &host_socket,
        &[
            "send-keys",
            "-t",
            &tab,
            "-l",
            &format!("{BIN} open-url https://example.com/from-the-host"),
        ],
    );
    on_host(&host_socket, &["send-keys", "-t", &tab, "Enter"]);
    wait_for("the link opened on this computer", || {
        desktop_log(&local).contains("https://example.com/from-the-host")
    });

    // Detached, it says so, and stops its forwards.
    on_host(&host_socket, &["detach"]);
    client.wait_text("[detached]");
    let log = std::fs::read_to_string(&log).unwrap();
    let forward = forwarded(&log).unwrap();
    let spec = forward
        .split("-L ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    assert!(
        log.contains(&format!("-O cancel -L {spec} ")),
        "the port's forward stopped: {log}"
    );
    assert!(
        log.contains("-O cancel -L /"),
        "the server's socket's too: {log}"
    );
    assert!(
        host_list(&host_socket).is_some(),
        "the host's server goes on"
    );
}

#[test]
fn a_first_install_whose_setup_is_said_no_to_attaches_without_it() {
    let r = with_a_host();
    let mut client = r.local.attach(150, 40);
    client.wait_text("$");
    type_line(&r.local, &format!("{BIN} ssh devbox"));
    client.wait_text("install it in ~/.local/bin?");
    type_line(&r.local, "");
    client.wait_text("Run Switchboard's setup on devbox now?");
    type_line(&r.local, "n");
    client.wait_for("attached without the setup", |_| attached_remote(&r.socket));
    // Detached, the pane shows again what it said before attaching.
    on_host(&r.socket, &["detach"]);
    client.wait_text("[detached]");
    client.wait_text("Run switchboard setup in a tab there when you want it.");
    assert!(
        !r.host.join(".claude/settings.json").exists(),
        "nothing of the setup's asked parts was done"
    );
}

#[test]
fn a_box_with_another_build_gets_this_one_without_its_setup_offered_again() {
    let r = with_a_host();
    // An older build there, which says its version and does nothing else.
    stand_in::install(
        &r.host.join(".local/bin/switchboard"),
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 'switchboard 0.0.1 (000000000000)'\nexit 1\n",
    );
    let mut client = r.local.attach(150, 40);
    client.wait_text("$");
    type_line(&r.local, &format!("{BIN} ssh devbox"));
    client.wait_text("devbox has Switchboard 0.0.1 (000000000000), not");
    type_line(&r.local, "");
    client.wait_for("attached on this build", |_| attached_remote(&r.socket));
    // Detached, the pane shows again what it said before attaching.
    on_host(&r.socket, &["detach"]);
    let screen = client.wait_text("[detached]").join("\n");
    assert!(
        screen.contains("Installed Switchboard"),
        "what it said is on the screen: {screen}"
    );
    assert!(
        !screen.contains("Run Switchboard's setup"),
        "an update offers no setup: {screen}"
    );
    let installed = Command::new(r.host.join(".local/bin/switchboard"))
        .arg("--version")
        .output()
        .unwrap();
    let mine = Command::new(BIN).arg("--version").output().unwrap();
    assert_eq!(installed.stdout, mine.stdout);
}
