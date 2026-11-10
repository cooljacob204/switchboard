//! Where a link opens, text is copied and a notification shows when the
//! workspace runs on another computer: on the computer you're at. VS Code's
//! remote window opens a link in your own browser however far away the
//! code runs; a workspace attached through `switchboard ssh` does the same,
//! the client there doing it with its own `src/platform` (`Desk`, which the
//! server sends it), while the `remote_local_desktop` setting is on.
//!
//! Every link, copy and notification goes through src/links.rs (however a
//! link is opened there), `platform::copy`, `platform::notify` and
//! `platform::open` (a web link only: a file on the workspace's computer
//! can't open on yours), which ask `handed_on` first:
//!
//! - in the server's own process, the request goes to the server's task
//!   (`serve_from`), which gives it to the client used last when that one
//!   is remote, or does it here;
//! - in a program in a pane (Notes, the diff, the views, `switchboard
//!   platform`, `switchboard open-url`), the server is asked over its
//!   socket (`switchboard desktop`), which answers 0 when it handed the
//!   request on and 3 when there is nothing to hand it to, the program then
//!   doing it itself as before;
//! - anywhere else, it is done here.
//!
//! What does it here is `here`, which the remote client calls too, so
//! nothing is handed on twice.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::mux::proto::Desk;
use crate::mux::socket;

/// What the server's process hands a request to: set once, as the server
/// starts.
type Hand = Box<dyn Fn(Desk) + Send + Sync>;
static SERVER: OnceLock<Hand> = OnceLock::new();

/// In the server's process, whether a request handed on now goes to the
/// computer a remote client is on: kept by the server's task
/// (`set_elsewhere`), so a thread that opened a link knows not to wait for
/// this computer's browser to come to the front.
static ELSEWHERE: AtomicBool = AtomicBool::new(false);

/// The code `switchboard desktop` answers when there is no remote client to
/// hand the request to, so the program asking does it itself.
pub const NOT_HANDED: i32 = 3;

/// Has every request made in this process go to `hand`: the server's, which
/// sends it to its task.
pub fn serve_from(hand: impl Fn(Desk) + Send + Sync + 'static) {
    let _ = SERVER.set(Box::new(hand));
}

/// Whether `ask` was handed on, as the module's comment says: true when
/// nothing more is to be done with it here.
pub fn handed_on(ask: &Desk) -> bool {
    if let Some(hand) = SERVER.get() {
        hand(ask.clone());
        return true;
    }
    if !socket::in_pane() {
        return false;
    }
    let Some((args, stdin)) = command_for(ask) else {
        return false;
    };
    let pane = std::env::var("SWITCHBOARD_PANE")
        .ok()
        .filter(|p| !p.is_empty());
    matches!(socket::ask_quiet(args, pane, stdin), Some((0, _, _)))
}

/// Has the server's process say whether what is handed on goes to the
/// computer a remote client is on (`Server::desk_client`).
pub fn set_elsewhere(on: bool) {
    ELSEWHERE.store(on, Ordering::Relaxed);
}

/// After `handed_on` said true: whether the request went to another
/// computer, where nothing comes to the front here. In the server's process,
/// as its task last said; in a pane's program, always, since the server
/// takes a request from one only when it has a remote client to give it to.
pub fn went_elsewhere() -> bool {
    if SERVER.get().is_some() {
        return ELSEWHERE.load(Ordering::Relaxed);
    }
    true
}

/// Does `ask` on this computer.
pub fn here(ask: &Desk) -> Result<()> {
    match ask {
        Desk::Open { url, as_site } => crate::links::open_here(url, as_site.as_deref()),
        Desk::Copy { text } => crate::platform::copy_here(text),
        Desk::Notify { title, text } => {
            crate::platform::notify_here(title, text);
            Ok(())
        }
        // A port is forwarded by the client's SSH connection, never here.
        Desk::Forward { .. } | Desk::Unforward { .. } => Ok(()),
    }
}

/// `here` off the server's task, on the runtime's blocking pool; off a
/// runtime (a unit test driving the server's state) it does nothing, so no
/// test opens a browser.
pub fn here_in_background(ask: Desk) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(move || {
            let _ = here(&ask);
        });
    }
}

/// The server command a program in a pane asks for `ask`, and its stdin:
/// `desktop open [--as <site>] <url>`, `desktop copy` with the text on
/// stdin, `desktop notify <title> <text>`.
fn command_for(ask: &Desk) -> Option<(Vec<String>, Option<Vec<u8>>)> {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match ask {
        Desk::Open { url, as_site } => {
            let mut args = words(&["desktop", "open"]);
            if let Some(site) = as_site {
                args.extend(words(&["--as", site]));
            }
            args.push(url.clone());
            Some((args, None))
        }
        Desk::Copy { text } => Some((words(&["desktop", "copy"]), Some(text.as_bytes().to_vec()))),
        Desk::Notify { title, text } => Some((words(&["desktop", "notify", title, text]), None)),
        Desk::Forward { .. } | Desk::Unforward { .. } => None,
    }
}

/// What `desktop <what> ...` asks, read back from its arguments (after
/// `desktop`) and stdin.
pub fn parse_command(args: &[String], stdin: Option<&[u8]>) -> Result<Desk, String> {
    const USAGE: &str = "usage: desktop open [--as <site>] <url> | desktop copy (text on stdin) | desktop notify <title> <text>";
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["open", url] => Ok(Desk::Open {
            url: url.to_string(),
            as_site: None,
        }),
        ["open", "--as", site, url] => Ok(Desk::Open {
            url: url.to_string(),
            as_site: Some(site.to_string()),
        }),
        ["copy"] => match stdin {
            Some(text) => Ok(Desk::Copy {
                text: String::from_utf8_lossy(text).into_owned(),
            }),
            None => Err("desktop copy takes the text on stdin".into()),
        },
        ["notify", title, text] => Ok(Desk::Notify {
            title: title.to_string(),
            text: text.to_string(),
        }),
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_a_pane_makes_reads_back_as_it_was() {
        for ask in [
            Desk::Open {
                url: "https://example.com/x?y#z".into(),
                as_site: None,
            },
            Desk::Open {
                url: "https://signin.aws.amazon.com/".into(),
                as_site: Some("awsapps.com".into()),
            },
            Desk::Copy {
                text: "two\nlines é".into(),
            },
            Desk::Notify {
                title: "Switchboard".into(),
                text: "Tab 2 waits on you".into(),
            },
        ] {
            let (args, stdin) = command_for(&ask).unwrap();
            assert_eq!(args[0], "desktop");
            assert_eq!(parse_command(&args[1..], stdin.as_deref()), Ok(ask));
        }
        assert_eq!(command_for(&Desk::Unforward { port: 1 }), None);
    }

    #[test]
    fn a_request_it_doesnt_know_is_refused() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse_command(&args(&[]), None).is_err());
        assert!(parse_command(&args(&["open"]), None).is_err());
        assert!(parse_command(&args(&["copy"]), None).is_err(), "no text");
        assert!(parse_command(&args(&["notify", "only a title"]), None).is_err());
        assert!(parse_command(&args(&["paste"]), None).is_err());
    }
}
