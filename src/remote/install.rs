//! Putting Switchboard on the computer `switchboard ssh` reaches, as VS
//! Code puts its server on a remote before it opens a window there: what
//! the host says of itself (`Host`, from `PROBE`), whether it needs this
//! build (`decide`), and the scripts that put one in `~/.local/bin` there.
//!
//! Both ends must be the same build, since the client and the server speak
//! a protocol that changes with it (src/mux/proto.rs). A host on this
//! computer's system gets this very binary, copied over the SSH connection,
//! so a build from a clone works too; one on another system gets the
//! release of this version for its target, downloaded on this computer and
//! refused unless its SHA-256 and its signature by Switchboard's release key
//! check out, as `switchboard update` checks one (`update::release_binary`),
//! then copied over the same way. Either way it is written beside the old
//! one under a name of its own, run once (`--version`, named `switchboard`
//! in a folder of its own, as install.sh runs it) to show it starts there
//! and is the build meant, and only then renamed into place, so a
//! half-copied binary is never in its place.
//!
//! The scripts are POSIX sh, run by the host's `sh -s` with their few
//! arguments after `--`, since the command line ssh hands the host is read
//! by the host's own shell, which may be anything; every argument is a
//! plain word (a version, a target, a path Switchboard made).

use crate::update::release;

/// What the host says of itself, as `PROBE` prints it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Host {
    /// Its home, where everything of Switchboard's goes.
    pub home: String,
    /// Its system and machine as Rust names them (`linux`, `aarch64`),
    /// from `uname -sm`; `None` for one Switchboard doesn't run on.
    pub os: Option<&'static str>,
    pub arch: Option<&'static str>,
    /// What its `~/.local/bin/switchboard --version` says after the name
    /// (`0.4.1 (1a2b3c4d5e6f)`); `None` when there is none, or it won't
    /// start there.
    pub binary: Option<String>,
    /// Whether a Switchboard server runs there, and its build when it says
    /// (`list`'s `version`; a server a version behind doesn't).
    pub server: Option<Option<String>>,
}

/// What the host is asked first, in one round trip: its home, its system,
/// its binary's version and its server's.
pub const PROBE: &str = r#"printf '%s\n' "$HOME"
uname -sm
echo '--- binary'
"$HOME/.local/bin/switchboard" --version 2>/dev/null || true
echo '--- server'
"$HOME/.local/bin/switchboard" list 2>/dev/null || true
"#;

/// `PROBE`'s answer read.
pub fn parse_probe(out: &str) -> Host {
    let mut lines = out.lines();
    let home = lines.next().unwrap_or("").trim().to_string();
    let (os, arch) = system_of(lines.next().unwrap_or(""));
    let rest: Vec<&str> = lines.collect();
    let section = |name: &str| -> Vec<&str> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in &rest {
            if let Some(n) = line.strip_prefix("--- ") {
                inside = n.trim() == name;
                continue;
            }
            if inside {
                out.push(*line);
            }
        }
        out
    };
    let binary = section("binary")
        .first()
        .and_then(|l| l.trim().strip_prefix("switchboard "))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let listed = section("server").join("\n");
    let server = serde_json::from_str::<serde_json::Value>(listed.trim())
        .ok()
        .filter(|v| v.is_object())
        .map(|v| v["version"].as_str().map(str::to_string));
    Host {
        home,
        os,
        arch,
        binary,
        server,
    }
}

/// A system and machine from `uname -sm`, as Rust names them.
pub fn system_of(uname: &str) -> (Option<&'static str>, Option<&'static str>) {
    let mut words = uname.split_whitespace();
    let os = match words.next() {
        Some("Linux") => Some("linux"),
        Some("Darwin") => Some("macos"),
        _ => None,
    };
    let arch = match words.next() {
        Some("x86_64" | "amd64") => Some("x86_64"),
        Some("arm64" | "aarch64") => Some("aarch64"),
        _ => None,
    };
    (os, arch)
}

/// What the host needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// It has this build.
    Nothing,
    /// This binary, copied over: the host runs this computer's system.
    ThisBinary,
    /// This version's release for the host's target.
    Release(&'static str),
    /// Nothing can be put there, for this reason.
    Cant(String),
}

/// What the host needs to run this build: `this` is this computer's
/// system and machine, `line` this build's `--version` (its version and
/// commit), `version` its version alone. A host on this system needs
/// exactly this build; one on another needs this version's release, since
/// that is the build there is for it.
pub fn decide(host: &Host, this: (&str, &str), line: &str, version: &str) -> Need {
    let (Some(os), Some(arch)) = (host.os, host.arch) else {
        return Need::Cant("Switchboard runs on Linux and macOS, on x86_64 or arm64".into());
    };
    if (os, arch) == this {
        return if host.binary.as_deref() == Some(line) {
            Need::Nothing
        } else {
            Need::ThisBinary
        };
    }
    let Some(target) = release::target_for(os, arch) else {
        return Need::Cant(format!(
            "there is no Switchboard release for {} on {arch}; build it there from source with cargo",
            system_name(os)
        ));
    };
    if host.binary.as_deref().map(number_of) == Some(version) {
        Need::Nothing
    } else {
        Need::Release(target)
    }
}

/// Whether the host's server runs a build other than the one it should:
/// the same build as this one where the host runs this system, else the
/// same version. A server that doesn't say runs a build before servers
/// said, so another.
pub fn server_differs(host: &Host, this: (&str, &str), line: &str, version: &str) -> bool {
    let Some(server) = &host.server else {
        return false;
    };
    let Some(said) = server else {
        return true;
    };
    if (host.os, host.arch) == (Some(this.0), Some(this.1)) {
        said != line
    } else {
        number_of(said) != version
    }
}

/// The version alone from a `--version` line's `0.4.1 (1a2b3c4d5e6f)`.
pub fn number_of(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

fn system_name(os: &str) -> &str {
    match os {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// Makes a folder of its own beside the host's binary for the new one,
/// and prints it, for the binary to be copied into as `switchboard`.
/// `$1` makes its name the host's own (this process's id).
pub const PREPARE: &str = r#"set -eu
bin="$HOME/.local/bin"
mkdir -p "$bin"
new="$bin/.switchboard.new.$1"
rm -rf "$new"
mkdir "$new"
printf '%s\n' "$new"
"#;

/// What puts a binary written into `$1/switchboard` in place: run once
/// under its own name, it must say it is version `$2` (any commit when
/// `$3` is `any`, else exactly `$2`), then it is renamed over the old one.
/// Prints what it said.
const PLACE_BODY: &str = r#"place() {
  new=$1
  chmod 755 "$new/switchboard"
  said=$("$new/switchboard" --version 2>/dev/null) || {
    rm -rf "$new"
    echo "the new switchboard won't start there" >&2
    exit 12
  }
  said=${said#switchboard }
  case $3:$said in
    any:"$2" | any:"$2 "* | exact:"$2") ;;
    *)
      rm -rf "$new"
      echo "the new switchboard says it is $said, not $2" >&2
      exit 13
      ;;
  esac
  mv "$new/switchboard" "$HOME/.local/bin/switchboard"
  rmdir "$new" 2>/dev/null || true
  printf '%s\n' "$said"
}
"#;

/// `PLACE_BODY` on its own, for a binary copied over: `$1` the folder
/// `PREPARE` made, `$2` the build it must say, `$3` `exact` or `any`.
pub fn place_script() -> String {
    format!("set -eu\n{PLACE_BODY}place \"$1\" \"$2\" \"$3\"\n")
}

/// The host's server restarted on the binary just put in place, keeping
/// every tab (src/mux/upgrade.rs).
pub const UPGRADE: &str = r#""$HOME/.local/bin/switchboard" upgrade </dev/null"#;

/// The host's server started when none runs (`switchboard remote-server`),
/// printing the socket it listens on.
pub const START: &str = r#""$HOME/.local/bin/switchboard" remote-server </dev/null"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(binary: &str, server: &str) -> String {
        format!("/home/jo\nLinux x86_64\n--- binary\n{binary}\n--- server\n{server}\n")
    }

    #[test]
    fn the_host_says_its_home_system_binary_and_server() {
        let host = parse_probe(&probe(
            "switchboard 0.4.1 (1a2b3c4d5e6f)",
            r#"{"sessions":[],"version":"0.4.1 (1a2b3c4d5e6f)"}"#,
        ));
        assert_eq!(
            host,
            Host {
                home: "/home/jo".into(),
                os: Some("linux"),
                arch: Some("x86_64"),
                binary: Some("0.4.1 (1a2b3c4d5e6f)".into()),
                server: Some(Some("0.4.1 (1a2b3c4d5e6f)".into())),
            }
        );
        let bare = parse_probe(&probe("", ""));
        assert_eq!((bare.binary, bare.server), (None, None));
        let old_server = parse_probe(&probe("switchboard 0.4.1 (a)", r#"{"sessions":[]}"#));
        assert_eq!(old_server.server, Some(None), "a server that doesn't say");
    }

    #[test]
    fn a_system_is_named_as_rust_names_it() {
        assert_eq!(system_of("Linux x86_64"), (Some("linux"), Some("x86_64")));
        assert_eq!(system_of("Darwin arm64"), (Some("macos"), Some("aarch64")));
        assert_eq!(system_of("Linux aarch64"), (Some("linux"), Some("aarch64")));
        assert_eq!(system_of("FreeBSD amd64"), (None, Some("x86_64")));
        assert_eq!(system_of(""), (None, None));
    }

    const LINE: &str = "0.4.1 (1a2b3c4d5e6f)";
    const LINUX: (&str, &str) = ("linux", "x86_64");

    fn host(os: &'static str, arch: &'static str, binary: Option<&str>) -> Host {
        Host {
            home: "/home/jo".into(),
            os: Some(os),
            arch: Some(arch),
            binary: binary.map(str::to_string),
            server: None,
        }
    }

    #[test]
    fn a_host_on_this_system_needs_exactly_this_build() {
        let d = |b| decide(&host("linux", "x86_64", b), LINUX, LINE, "0.4.1");
        assert_eq!(d(None), Need::ThisBinary, "missing");
        assert_eq!(d(Some(LINE)), Need::Nothing, "the same");
        assert_eq!(
            d(Some("0.4.1 (ffffffffffff)")),
            Need::ThisBinary,
            "another commit"
        );
        assert_eq!(d(Some("0.3.0 (1a2b3c4d5e6f)")), Need::ThisBinary);
    }

    #[test]
    fn a_host_on_another_system_needs_this_versions_release() {
        let d = |b| decide(&host("macos", "aarch64", b), LINUX, LINE, "0.4.1");
        assert_eq!(d(None), Need::Release("aarch64-apple-darwin"));
        assert_eq!(d(Some("0.4.1 (abcdefabcdef)")), Need::Nothing);
        assert_eq!(
            d(Some("0.4.0 (abcdefabcdef)")),
            Need::Release("aarch64-apple-darwin")
        );
        // An arm Linux has no release; only this binary, from an arm Linux.
        let arm = decide(&host("linux", "aarch64", None), LINUX, LINE, "0.4.1");
        assert!(matches!(arm, Need::Cant(why) if why.contains("from source")));
        let same_arm = decide(
            &host("linux", "aarch64", None),
            ("linux", "aarch64"),
            LINE,
            "0.4.1",
        );
        assert_eq!(same_arm, Need::ThisBinary);
        let unknown = Host {
            os: None,
            ..host("linux", "x86_64", None)
        };
        assert!(matches!(
            decide(&unknown, LINUX, LINE, "0.4.1"),
            Need::Cant(_)
        ));
    }

    #[test]
    fn a_server_on_another_build_is_told_apart() {
        let with = |server: Option<Option<&str>>, os| Host {
            server: server.map(|s| s.map(str::to_string)),
            ..host(os, "x86_64", Some(LINE))
        };
        let differs = |h: &Host| server_differs(h, LINUX, LINE, "0.4.1");
        assert!(!differs(&with(None, "linux")), "none runs");
        assert!(!differs(&with(Some(Some(LINE)), "linux")));
        assert!(differs(&with(Some(Some("0.4.1 (ffffffffffff)")), "linux")));
        assert!(differs(&with(Some(None), "linux")), "one that doesn't say");
        assert!(!differs(&with(Some(Some("0.4.1 (ffffffffffff)")), "macos")));
        assert!(differs(&with(Some(Some("0.4.0 (1a2b3c4d5e6f)")), "macos")));
    }

    /// Runs a script as the host would, `sh -s` with its arguments, in a
    /// home of its own.
    fn run(home: &std::path::Path, script: &str, args: &[&str]) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .arg("--")
            .args(args)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn temp_home() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::path::PathBuf::from(format!(
            "/tmp/sb-install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in for a release's binary: a script saying its version.
    fn fake_binary(said: &str) -> String {
        format!(
            "#!/bin/sh\n[ \"${{0##*/}}\" = switchboard ] || exit 9\necho 'switchboard {said}'\n"
        )
    }

    #[test]
    fn a_binary_copied_over_is_put_in_place_once_it_says_the_build_meant() {
        let home = temp_home();
        let out = run(&home, PREPARE, &["77"]);
        assert!(out.status.success());
        let new = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(
            new,
            format!("{}/.local/bin/.switchboard.new.77", home.display())
        );
        std::fs::write(format!("{new}/switchboard"), fake_binary(LINE)).unwrap();
        let out = run(&home, &place_script(), &[&new, LINE, "exact"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), LINE);
        assert!(home.join(".local/bin/switchboard").exists());
        assert!(!std::path::Path::new(&new).exists(), "its folder is gone");

        // One that says another build is refused and the old one kept.
        let out = run(&home, PREPARE, &["78"]);
        let new = String::from_utf8(out.stdout).unwrap().trim().to_string();
        std::fs::write(format!("{new}/switchboard"), fake_binary("0.9.9 (x)")).unwrap();
        let out = run(&home, &place_script(), &[&new, LINE, "exact"]);
        assert_eq!(out.status.code(), Some(13));
        assert!(!std::path::Path::new(&new).exists());
        let kept = std::fs::read_to_string(home.join(".local/bin/switchboard")).unwrap();
        assert!(kept.contains(LINE));
        let _ = std::fs::remove_dir_all(&home);
    }
}
