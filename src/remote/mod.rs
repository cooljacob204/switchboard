//! Working on another computer over SSH, as VS Code's remote window does:
//! `switchboard ssh <host>` puts this build on the host when it isn't
//! there, starts the host's workspace, and attaches this terminal to it
//! through one SSH connection; the workspace's links, clipboard and
//! notifications come to the computer you're at, and the ports its panes
//! listen on are forwarded to the same ports here. New with the Rust
//! version; the tmux setup had nothing like it.
//!
//! - `ssh`: the SSH connection, through the system's `ssh`.
//! - `install`: what the host needs, and the scripts that put it there.
//! - `desktop`: where a link, a copy or a notification lands.
//! - `ports`: what the panes listen on, and which ports to forward.
//! - `serve`: the server's side: its remote clients, the ports it reads
//!   and forwards through them, the Ports view and its keys.
//!
//! The command itself is src/cmd/ssh.rs.

pub mod desktop;
pub mod install;
pub mod ports;
pub mod serve;
pub mod ssh;

pub use serve::{Remote, RemoteEvent};

/// M-p's commands for a remote workspace's ports
/// (`mux::keys::Action::Remote`), both in the palette alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// "Forward a port": a port typed, forwarded to the computer you're at.
    ForwardPort,
    /// "Stop forwarding a port": one of those forwarded, picked.
    StopForwarding,
}

impl Command {
    pub const ALL: [Command; 2] = [Command::ForwardPort, Command::StopForwarding];

    /// Its name in keys.json.
    pub fn name(self) -> &'static str {
        match self {
            Command::ForwardPort => "forward-port",
            Command::StopForwarding => "stop-forwarding-port",
        }
    }

    pub fn parse(name: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.name() == name)
    }

    pub fn about(self) -> &'static str {
        match self {
            Command::ForwardPort => "Forward a port of this workspace's computer to yours",
            Command::StopForwarding => "Stop forwarding a port to your computer",
        }
    }

    /// More on what it does than `about`, for the palette's preview.
    pub fn detail(self) -> &'static str {
        match self {
            Command::ForwardPort => {
                "In a workspace attached with switchboard ssh, a port typed (one no pane's program holds, such as a container's) is reached at the same port on localhost on the computer you're at, or the next free one. Kept until stopped."
            }
            Command::StopForwarding => {
                "Picks one of the forwarded ports and stops it; a port a pane listens on isn't forwarded again until it closes and opens again."
            }
        }
    }

    pub fn tags(self) -> &'static [&'static str] {
        &["remote", "ssh", "ports"]
    }

    /// Its key by default: none, both in the palette alone.
    pub fn key(self) -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_reads_back_by_its_name() {
        for c in Command::ALL {
            assert_eq!(Command::parse(c.name()), Some(c));
            assert!(c.tags().contains(&"remote"));
            assert_eq!(c.key(), None);
        }
    }
}
