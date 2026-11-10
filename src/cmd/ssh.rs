//! `switchboard ssh <host> [-- <ssh options>]`: the workspace on another
//! computer, as VS Code's remote window opens one (src/remote).
//!
//! It opens one SSH connection to the host with the system's `ssh`
//! (src/remote/ssh.rs), so your SSH configuration applies; asks the host
//! what it has (src/remote/install.rs) and, when it lacks this build,
//! offers to put it in `~/.local/bin` there; on a first install, offers to
//! run the host's guided setup in this terminal; offers to restart a server
//! already running there on another build; starts the
//! host's server when none runs (`switchboard remote-server` there),
//! forwards its socket to one here, private to you, and attaches this
//! terminal to it as a remote client, which opens the workspace's links,
//! takes its copies and notifications, and forwards its panes' ports here
//! (`SshDesk`).
//!
//! When the connection drops it says so and reconnects, with the same
//! master connection when it is still up; the host's server and panes go
//! on meanwhile, as when a terminal closes. Detaching, or the server
//! ending, ends it, stopping the forwards it made; the master connection
//! stays up ten minutes for the next one.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::mux::client::{self, Desktop, Ended};
use crate::mux::proto::{ClientMsg, Desk};
use crate::remote::install::{self, Need};
use crate::remote::ports;
use crate::remote::ssh::{self, Ssh};
use crate::update::{self, release};

/// How long it goes on trying to reconnect before giving up.
const RECONNECT_FOR: Duration = Duration::from_secs(300);
/// The longest wait between two tries.
const RECONNECT_GAP: Duration = Duration::from_secs(30);

const USAGE: &str = "Usage: switchboard ssh <host> [-- <ssh options>]";

pub fn main(args: Vec<String>) -> ExitCode {
    let (host, options) = match parse(&args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{USAGE}");
            println!(
                "\nThe workspace on <host>, as VS Code's remote window: Switchboard put in ~/.local/bin there when it lacks this build (asked first), its server started, and this terminal attached to it. Its links, copies and notifications come to this computer, and the ports its panes listen on are forwarded to the same ports here. Your SSH configuration applies; the options after -- go to the connection's ssh."
            );
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("switchboard ssh: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("switchboard ssh needs a terminal");
        return ExitCode::FAILURE;
    }
    let ssh = Ssh::new(&host, &options);
    match run(&ssh) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("switchboard ssh: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The host and the options after `--`; `None` for `--help`.
fn parse(args: &[String]) -> Result<Option<(String, Vec<String>)>, String> {
    let (before, after) = match args.iter().position(|a| a == "--") {
        Some(at) => (&args[..at], args[at + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    match before {
        [flag] if flag == "-h" || flag == "--help" => Ok(None),
        [host] if !host.starts_with('-') && !host.is_empty() => Ok(Some((host.clone(), after))),
        [] => Err("which host?".into()),
        _ => Err(format!("didn't expect {}", before.join(" "))),
    }
}

fn run(ssh: &Ssh) -> Result<ExitCode> {
    println!("Connecting to {}…", ssh.host);
    ssh.open()?;
    prepare(ssh)?;
    let desk = Arc::new(SshDesk::new(ssh.clone()));
    let mut failing_since: Option<Instant> = None;
    let mut gap = Duration::from_secs(1);
    // A failure before the first attachment is said at once; one after a
    // connection was lost is tried again for a while.
    let mut attached_once = false;
    loop {
        let attached = attach_once(ssh, &desk);
        desk.stop_all();
        match attached {
            Ok(Ended::Exit(message)) => {
                if let Some(message) = message {
                    println!("{message}");
                }
                return Ok(ExitCode::SUCCESS);
            }
            Ok(Ended::Left(None)) => return Ok(ExitCode::SUCCESS),
            Ok(Ended::Left(Some(sig))) => {
                return Ok(ExitCode::from(128 + sig.clamp(0, 127) as u8));
            }
            Ok(Ended::Lost) => {
                attached_once = true;
                failing_since = None;
                gap = Duration::from_secs(1);
                println!(
                    "The connection to {} was lost; reconnecting (Ctrl-C stops)…",
                    ssh.host
                );
            }
            Err(e) if !attached_once => return Err(e),
            Err(e) => {
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > RECONNECT_FOR {
                    bail!(
                        "couldn't reconnect to {} for {} minutes: {e:#}",
                        ssh.host,
                        RECONNECT_FOR.as_secs() / 60
                    );
                }
                println!(
                    "Couldn't reach it ({e:#}); trying again in {}s…",
                    gap.as_secs()
                );
                std::thread::sleep(gap);
                gap = (gap * 2).min(RECONNECT_GAP);
            }
        }
    }
}

/// The host's server started, its socket forwarded here, and this terminal
/// attached until it ends; the socket's forward taken away after.
fn attach_once(ssh: &Ssh, desk: &Arc<SshDesk>) -> Result<Ended> {
    if !ssh.up() {
        ssh.open()?;
    }
    let out = ssh.script(install::START, &[])?;
    if !out.status.success() {
        bail!("its server didn't start: {}", said(&out.stderr));
    }
    let remote = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    if remote.is_empty() {
        bail!("its server didn't say where it listens");
    }
    let local = ssh.local_socket();
    let _ = std::fs::remove_file(&local);
    let local_text = local.to_string_lossy().into_owned();
    ssh.forward(&local_text, &remote)
        .context("couldn't forward its server's socket")?;
    let desktop: Arc<dyn Desktop> = desk.clone();
    let ended = client::attach_remote(&local, desktop);
    let _ = ssh.cancel(&local_text, &remote);
    let _ = std::fs::remove_file(&local);
    ended
}

/// What the host lacks, put there: this build, asked first, with its
/// setup; a server running another build, restarted on this one after a
/// yes.
fn prepare(ssh: &Ssh) -> Result<()> {
    let out = ssh.script(install::PROBE, &[])?;
    if !out.status.success() {
        bail!("{} didn't answer: {}", ssh.host, said(&out.stderr));
    }
    let host = install::parse_probe(&String::from_utf8_lossy(&out.stdout));
    let this = (std::env::consts::OS, std::env::consts::ARCH);
    let line = update::version_line();
    let version = env!("CARGO_PKG_VERSION");
    let name = &ssh.host;
    let mut installed = false;
    match install::decide(&host, this, &line, version) {
        Need::Nothing => {}
        Need::Cant(why) => bail!("Switchboard can't be put on {name}: {why}"),
        need => {
            let question = match &host.binary {
                None => {
                    format!("Switchboard {version} isn't on {name}: install it in ~/.local/bin?")
                }
                Some(there) => format!(
                    "{name} has Switchboard {there}, not {line}: install this one in ~/.local/bin?"
                ),
            };
            if !ask(&question) {
                bail!(
                    "nothing was installed; switchboard ssh needs the same build on both computers"
                );
            }
            let said = match need {
                Need::Release(target) => install_release(ssh, target, version)?,
                _ => {
                    println!("Copying this build to {name}…");
                    copy_over(ssh, &this_binary()?, &line, "exact")?
                }
            };
            println!("Installed Switchboard {said} in ~/.local/bin on {name}.");
            // A first install offers the host's guided setup, which asks
            // before each part as it does here. An update needs none: the
            // new build brings the parts already set up there up to date as
            // its `remote-server` runs (`setup::refresh`), as a new build
            // attaching does here. Nothing of your own files is copied: the
            // host's settings are its own.
            if host.binary.is_none() {
                guided_setup(ssh, &host.home)?;
            }
            installed = true;
        }
    }
    let host = install::Host {
        binary: if installed {
            Some(line.clone())
        } else {
            host.binary
        },
        ..host
    };
    if install::server_differs(&host, this, &line, version) {
        if ask(&format!(
            "Switchboard's server on {name} runs another build: restart it on this one, keeping every tab and what runs in it?"
        )) {
            let out = ssh.script(install::UPGRADE, &[])?;
            if !out.status.success() {
                println!(
                    "Its server couldn't restart ({}); attaching to it as it is.",
                    said(&out.stderr)
                );
            }
        } else {
            println!(
                "Attaching to it as it is: its links and ports stay on {name} until its server restarts on this build."
            );
        }
    }
    Ok(())
}

/// Asks whether to run the host's guided setup (`switchboard setup`) now,
/// and runs it in this terminal, its questions asked and answered here;
/// one that doesn't finish is said, and the attachment goes on.
fn guided_setup(ssh: &Ssh, home: &str) -> Result<()> {
    let name = &ssh.host;
    if !ask(&format!(
        "Run Switchboard's setup on {name} now? It asks before each part, as switchboard setup does here."
    )) {
        println!("Run switchboard setup in a tab there when you want it.");
        return Ok(());
    }
    if home.is_empty() {
        bail!("{name} didn't say where its home is");
    }
    let binary = format!("{home}/.local/bin/switchboard");
    let status = ssh.interactive(&[&binary, "setup"])?;
    if !status.success() {
        println!(
            "Its setup didn't finish; attaching anyway. Run switchboard setup in a tab there to finish it."
        );
    }
    Ok(())
}

/// The release of `version` for `target`, downloaded on the host and put
/// in place there, or, where the host can't download it, downloaded and
/// checked here and copied over. Gives what it said of its version.
fn install_release(ssh: &Ssh, target: &str, version: &str) -> Result<String> {
    println!(
        "Downloading Switchboard {version} for {target} on {}…",
        ssh.host
    );
    let base = install::release_base(version);
    let pid = std::process::id().to_string();
    let out = ssh.script(&install::download_script(), &[version, target, &base, &pid])?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    if out.status.code() != Some(install::CANT_DOWNLOAD) {
        bail!("couldn't install it: {}", said(&out.stderr));
    }
    println!(
        "{} can't download it; downloading it here and copying it over…",
        ssh.host
    );
    let current = release::Version::current();
    let name = release::archive_name(&current, target);
    let archive = release::Asset {
        name: name.clone(),
        url: format!("{base}/{name}"),
    };
    let checksum = release::Asset {
        name: format!("{name}.sha256"),
        url: format!("{base}/{name}.sha256"),
    };
    let (bytes, sum) = crate::util::block_on(async {
        let bytes = release::download(&archive, "switchboard ssh").await?;
        let sum = release::download(&checksum, "switchboard ssh").await?;
        anyhow::Ok((bytes, sum))
    })?;
    update::install::verify(&bytes, &String::from_utf8_lossy(&sum), &name)?;
    let binary = update::install::binary_from(&bytes)?;
    copy_over(ssh, &binary, version, "any")
}

/// `binary` put in place on the host: copied into a folder of its own
/// there, run once to show it is the build `expect` (`exact`ly, or `any`
/// commit of that version), renamed into place. Gives what it said.
fn copy_over(ssh: &Ssh, binary: &[u8], expect: &str, mode: &str) -> Result<String> {
    let pid = std::process::id().to_string();
    let out = ssh.script(install::PREPARE, &[&pid])?;
    if !out.status.success() {
        bail!(
            "couldn't write in ~/.local/bin there: {}",
            said(&out.stderr)
        );
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ssh.copy_to(&format!("{dir}/switchboard"), binary)?;
    let out = ssh.script(&install::place_script(), &[&dir, expect, mode])?;
    if !out.status.success() {
        bail!("couldn't put it in place: {}", said(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The running binary's bytes: on Linux from `/proc/self/exe`, which is the
/// running build even after another was put in its place.
fn this_binary() -> Result<Vec<u8>> {
    let proc_exe = Path::new("/proc/self/exe");
    let path: PathBuf = if proc_exe.exists() {
        proc_exe.to_path_buf()
    } else {
        std::env::current_exe().context("couldn't find this switchboard")?
    };
    std::fs::read(&path).with_context(|| format!("couldn't read {}", path.display()))
}

/// What a command there said went wrong, its last line.
fn said(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.trim()
        .lines()
        .last()
        .unwrap_or("it gave no reason")
        .to_string()
}

/// A yes/no question in the terminal: Enter is yes; a closed input is no.
fn ask(question: &str) -> bool {
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => {
            println!();
            false
        }
        Ok(_) => {
            let answer = answer.trim();
            answer.is_empty() || answer.starts_with(['y', 'Y'])
        }
    }
}

/// What the client does with what the host's server asks of this
/// computer: links, copies and notifications done here
/// (src/remote/desktop.rs), ports forwarded through the SSH connection to
/// the same port here, or the next free one.
struct SshDesk {
    ssh: Ssh,
    /// Each pane's port forwarded, with the port here and where the host's
    /// end connects.
    forwards: Mutex<BTreeMap<u16, (u16, String)>>,
}

impl SshDesk {
    fn new(ssh: Ssh) -> SshDesk {
        SshDesk {
            ssh,
            forwards: Mutex::new(BTreeMap::new()),
        }
    }

    fn forwards(&self) -> std::sync::MutexGuard<'_, BTreeMap<u16, (u16, String)>> {
        self.forwards.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `port` there forwarded to a free port here, the same one when it is
    /// free: one already forwarded (asked again by a server restarted on a
    /// new build) answers where it is.
    fn forward(&self, port: u16, to: &str) -> ClientMsg {
        let done =
            |local: Option<u16>, error: Option<String>| ClientMsg::Forwarded { port, local, error };
        if let Some((local, _)) = self.forwards().get(&port).filter(|(_, t)| t == to) {
            return done(Some(*local), None);
        }
        let mut why = "no port here is free for it".to_string();
        let mut tries = 0;
        for candidate in ports::local_candidates(port) {
            let Some(local) = ports::free_local(candidate) else {
                continue;
            };
            let (here, there) = ssh::port_ends(local, to, port);
            match self.ssh.forward(&here, &there) {
                Ok(()) => {
                    self.forwards().insert(port, (local, to.to_string()));
                    return done(Some(local), None);
                }
                Err(e) => why = e.to_string(),
            }
            tries += 1;
            if tries >= 3 {
                break;
            }
        }
        done(None, Some(why))
    }

    fn unforward(&self, port: u16) {
        let Some((local, to)) = self.forwards().remove(&port) else {
            return;
        };
        let (here, there) = ssh::port_ends(local, &to, port);
        let _ = self.ssh.cancel(&here, &there);
    }

    /// Every forward stopped, as the attachment ends.
    fn stop_all(&self) {
        let all: Vec<u16> = self.forwards().keys().copied().collect();
        for port in all {
            self.unforward(port);
        }
    }
}

impl Desktop for SshDesk {
    fn act(&self, ask: Desk) -> Option<ClientMsg> {
        match ask {
            Desk::Forward { port, to } => Some(self.forward(port, &to)),
            Desk::Unforward { port } => {
                self.unforward(port);
                None
            }
            other => {
                let _ = crate::remote::desktop::here(&other);
                None
            }
        }
    }
}

/// `switchboard remote-server`, which `switchboard ssh` runs on the host:
/// the workspace's server started there when none runs (with this build's
/// skills and plugin refreshed first, as `switchboard` does), and the
/// socket it listens on printed.
pub fn server_main(args: Vec<String>) -> ExitCode {
    if !args.is_empty() {
        eprintln!("Usage: switchboard remote-server   (run by switchboard ssh on the host)");
        return ExitCode::from(2);
    }
    crate::setup::refresh();
    match crate::mux::socket::connect_or_start() {
        Ok(_) => {
            println!("{}", crate::mux::socket::socket_path().display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("switchboard remote-server: {}", anyhow!(e));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn a_host_and_the_options_for_its_connection_are_read() {
        assert_eq!(
            parse(&args(&["devbox"])),
            Ok(Some(("devbox".into(), Vec::new())))
        );
        assert_eq!(
            parse(&args(&["me@devbox", "--", "-p", "2222", "-J", "jump"])),
            Ok(Some((
                "me@devbox".into(),
                args(&["-p", "2222", "-J", "jump"])
            )))
        );
        assert_eq!(parse(&args(&["--help"])), Ok(None));
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["-p", "22"])).is_err(), "options go after --");
        assert!(parse(&args(&["a", "b"])).is_err());
    }
}
