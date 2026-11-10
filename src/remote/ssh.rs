//! The one SSH connection `switchboard ssh` makes, through the system's
//! own `ssh` (the one external program this part runs, allowed for it), so
//! your `~/.ssh/config`, keys, agent and jump hosts all apply as they do
//! for any `ssh` you type.
//!
//! It opens a master connection (`-M`, its control socket under
//! `~/.cache/switchboard/ssh/` with a short hashed name, since a socket's
//! path can't be over 104 bytes on macOS and ssh adds to it while it makes
//! it), which stays up ten minutes after the last use (`ControlPersist`),
//! so a reconnect, or a second terminal on the same host, needs no new
//! login. Everything after goes through it: the commands run there, the
//! server's socket forwarded to one here, each port forwarded (`-O
//! forward`) and stopped (`-O cancel`). Those are run with no
//! configuration (`-F /dev/null`) and never ask anything (`BatchMode`),
//! since the master already holds the login, and a `LocalForward` or
//! `RemoteCommand` in your configuration asked again on each would fail.
//!
//! The commands run there are read by the host's own shell, which may be
//! any, ssh joining its words with spaces; so each is `sh -s -- '<word>'
//! ...` with its script on stdin, or `cat > '<path>'` to copy a file over
//! (src/remote/install.rs), every word in single quotes, which sh, bash,
//! zsh and fish all read the same while it holds no quote or backslash.
//!
//! Every connection made and command run there is counted in the request
//! audit (src/requests.rs), as `SSH`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context, Result, bail};

use crate::paths;

/// How long the master stays up after its last use.
const PERSIST: &str = "10m";

/// One host's connection.
#[derive(Clone, Debug)]
pub struct Ssh {
    /// The host as it was given (an alias of your SSH configuration, or
    /// `user@host`).
    pub host: String,
    /// The options given after `--`, for the master alone.
    pub options: Vec<String>,
    /// The master's control socket.
    pub control: PathBuf,
}

/// Where the control sockets and forwarded server sockets are: readable by
/// you alone, since anything that reaches a forwarded server's socket can
/// type into every pane there.
pub fn dir() -> PathBuf {
    paths::cache_dir().join("ssh")
}

impl Ssh {
    pub fn new(host: &str, options: &[String]) -> Ssh {
        let mut key = host.to_string();
        for o in options {
            key.push('\0');
            key.push_str(o);
        }
        let hash = crate::update::install::sha256_hex(key.as_bytes());
        Ssh {
            host: host.to_string(),
            options: options.to_vec(),
            control: dir().join(&hash[..12]),
        }
    }

    /// The local socket the host's server is forwarded to for this process.
    pub fn local_socket(&self) -> PathBuf {
        let name = self
            .control
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        dir().join(format!("{name}-{}.sock", std::process::id()))
    }

    /// `ssh` through the master, with no configuration and never asking.
    fn through(&self) -> Command {
        let mut c = Command::new("ssh");
        c.arg("-F")
            .arg("/dev/null")
            .arg("-S")
            .arg(&self.control)
            .args(["-o", "BatchMode=yes", "-o", "ControlMaster=no"]);
        c
    }

    /// Whether the master is up.
    pub fn up(&self) -> bool {
        self.control.exists()
            && self
                .through()
                .args(["-O", "check", &self.host])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    }

    /// Opens the master, unless it is up: on this terminal, so a password,
    /// a key's passphrase or a new host's key is asked as `ssh` asks it (on
    /// the terminal itself), then in the background. Your options and
    /// configuration apply. What it says otherwise goes to a log beside its
    /// socket, so nothing it says later lands on the workspace's screen;
    /// should it fail, the log is what is said.
    pub fn open(&self) -> Result<()> {
        if self.up() {
            return Ok(());
        }
        make_private_dir(&dir())?;
        let _ = std::fs::remove_file(&self.control);
        let log_path = self.control.with_extension("log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("couldn't write {}", paths::tilde(&log_path)))?;
        crate::requests::count("SSH", "ssh", "switchboard ssh");
        let status = Command::new("ssh")
            .arg("-M")
            .arg("-S")
            .arg(&self.control)
            .args([
                "-o",
                &format!("ControlPersist={PERSIST}"),
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "RemoteCommand=none",
                "-o",
                "RequestTTY=no",
                "-f",
                "-N",
            ])
            .args(&self.options)
            .arg(&self.host)
            .stdout(Stdio::null())
            .stderr(log)
            .status()
            .context("couldn't run ssh")?;
        if !status.success() {
            let said = std::fs::read_to_string(&log_path).unwrap_or_default();
            match said.trim().lines().last() {
                Some(why) => bail!("ssh couldn't connect to {}: {why}", self.host),
                None => bail!("ssh couldn't connect to {}", self.host),
            }
        }
        Ok(())
    }

    /// Runs `script` there in `sh`, with `args` after it, and gives what
    /// it printed and how it ended.
    pub fn script(&self, script: &str, args: &[&str]) -> Result<Output> {
        let mut c = self.through();
        c.arg("-T").arg(&self.host).args(["sh", "-s", "--"]);
        for a in args {
            c.arg(quoted(a)?);
        }
        self.run(c, script.as_bytes())
    }

    /// Runs `words` there on this terminal, with a terminal of its own there
    /// (`-t`), for what asks you questions: the host's guided setup.
    pub fn interactive(&self, words: &[&str]) -> Result<ExitStatus> {
        crate::requests::count("SSH", "ssh", "switchboard ssh");
        let mut c = self.through();
        c.arg("-t").arg(&self.host);
        for w in words {
            c.arg(quoted(w)?);
        }
        c.status().context("couldn't run ssh")
    }

    /// Copies `bytes` into the file at `path` there (one `PREPARE` made,
    /// src/remote/install.rs).
    pub fn copy_to(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let mut c = self.through();
        c.arg("-T")
            .arg(&self.host)
            .arg(format!("cat > {}", quoted(path)?));
        let out = self.run(c, bytes)?;
        if !out.status.success() {
            bail!(
                "couldn't copy it over: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn run(&self, mut c: Command, input: &[u8]) -> Result<Output> {
        crate::requests::count("SSH", "ssh", "switchboard ssh");
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't run ssh")?;
        let mut stdin = child.stdin.take().expect("piped");
        let input = input.to_vec();
        // Written from a thread of its own, so a host that answers before
        // it has read everything never holds this up.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().context("couldn't run ssh")?;
        let _ = writer.join();
        Ok(out)
    }

    /// Forwards `local` here to `remote` there: a socket's path to a
    /// socket's path, or `127.0.0.1:<port>` to `<host>:<port>`.
    pub fn forward(&self, local: &str, remote: &str) -> Result<()> {
        self.control_command("forward", local, remote)
    }

    /// Stops what `forward` started.
    pub fn cancel(&self, local: &str, remote: &str) -> Result<()> {
        self.control_command("cancel", local, remote)
    }

    fn control_command(&self, what: &str, local: &str, remote: &str) -> Result<()> {
        let out = self
            .through()
            .args(["-O", what, "-L", &format!("{local}:{remote}"), &self.host])
            .stdin(Stdio::null())
            .output()
            .context("couldn't run ssh")?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            bail!("{}", why.trim().lines().last().unwrap_or("ssh refused it"));
        }
        Ok(())
    }
}

/// A word for the host's shell, whichever it is: in single quotes, refused
/// when it holds what one of them reads otherwise there.
fn quoted(word: &str) -> Result<String> {
    if word.contains(['\'', '\\', '\n']) {
        bail!("{word} can't be passed to the host's shell");
    }
    Ok(format!("'{word}'"))
}

/// A port forward's two ends as `-L` takes them: this computer's loopback
/// at `local`, the host's `to` at `port`.
pub fn port_ends(local: u16, to: &str, port: u16) -> (String, String) {
    (format!("127.0.0.1:{local}"), format!("{to}:{port}"))
}

fn make_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("couldn't make {}", paths::tilde(dir)))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("couldn't make {} private", paths::tilde(dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_and_its_options_have_a_short_control_socket_of_their_own() {
        let a = Ssh::new("devbox", &[]);
        let b = Ssh::new("devbox", &["-p".into(), "2222".into()]);
        let c = Ssh::new("devbox", &[]);
        assert_ne!(a.control, b.control);
        assert_eq!(a.control, c.control);
        let name = a
            .control
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(name.len(), 12);
        assert!(a.control.starts_with(dir()));
        let local = a.local_socket();
        assert!(local.to_string_lossy().ends_with(".sock"));
        assert!(local.starts_with(dir()));
    }

    #[test]
    fn a_word_goes_to_the_hosts_shell_quoted_or_not_at_all() {
        assert_eq!(
            quoted("0.4.1 (1a2b3c4d5e6f)").unwrap(),
            "'0.4.1 (1a2b3c4d5e6f)'"
        );
        assert!(quoted("it's").is_err());
        assert!(quoted("a\\b").is_err());
        assert!(quoted("a\nb").is_err());
    }

    #[test]
    fn a_port_forward_names_both_ends() {
        assert_eq!(
            port_ends(3001, "localhost", 3000),
            ("127.0.0.1:3001".into(), "localhost:3000".into())
        );
        assert_eq!(port_ends(8080, "[fd00::5]", 8080).1, "[fd00::5]:8080");
    }
}
