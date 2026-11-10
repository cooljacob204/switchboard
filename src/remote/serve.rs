//! The server's side of a remote workspace: the clients attached through
//! `switchboard ssh`, where its links, copies and notifications go, the
//! ports its panes listen on, read every few seconds while such a client is
//! attached and forwarded through it, the Ports view, and M-p's two port
//! commands.
//!
//! A link, a copy or a notification goes to the client used last when that
//! one is remote and `remote_local_desktop` is on, else is done here
//! (src/remote/desktop.rs). Ports are forwarded through the remote client
//! used last, one forward a port; when that client leaves, its forwards go
//! with it and the next reading asks the next remote client, if any.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Command;
use super::ports::{self, Port};
use crate::items::{Item, State, Target, ViewData};
use crate::mission::ask::Ask;
use crate::mux::TabId;
use crate::mux::proto::{Desk, ServerMsg};
use crate::mux::server::{ClientId, Event, Server};
use crate::settings;
use crate::ui::{Outcome, PickItem, Picker};

/// How often the panes' ports are read while a remote client is attached:
/// a dev server just started is reached from your computer within a tick
/// or two after.
const READ_EVERY: Duration = Duration::from_secs(3);

/// What the server keeps of its remote clients' ports.
#[derive(Debug, Default)]
pub struct Remote {
    /// What the last reading found.
    pub ports: Vec<Port>,
    /// Each port forwarded, or asked to be, by the pane's port.
    pub forwards: BTreeMap<u16, Forward>,
    /// Ports whose forward was stopped by hand, left alone until they close.
    held: BTreeSet<u16>,
    reading: bool,
    read_at: Option<Instant>,
}

/// One port's forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forward {
    /// The remote client forwarding it.
    pub client: ClientId,
    /// What the host's end connects to.
    pub to: String,
    /// The port it is at on the computer you're at, once forwarded.
    pub local: Option<u16>,
    /// Why it couldn't be forwarded.
    pub error: Option<String>,
    /// Forwarded with M-p's "Forward a port", so kept until stopped.
    pub by_hand: bool,
}

/// What this part's work off the task, and a remote client, answer.
#[derive(Debug)]
pub enum RemoteEvent {
    /// The panes' ports, read.
    Read(Vec<Port>),
    /// A remote client's answer to `Desk::Forward`.
    Forwarded {
        client: ClientId,
        port: u16,
        local: Option<u16>,
        error: Option<String>,
    },
    /// A link, copy or notification made in the server's process
    /// (src/remote/desktop.rs).
    Desk(Desk),
}

impl Server {
    /// The remote client used last, the one ports are forwarded through.
    pub fn remote_client(&self) -> Option<ClientId> {
        self.clients
            .values()
            .filter(|c| c.remote)
            .max_by_key(|c| c.activity)
            .map(|c| c.id)
    }

    /// Where a link, a copy or a notification goes: the client used last,
    /// when it is remote and `remote_local_desktop` is on.
    pub fn desk_client(&self) -> Option<ClientId> {
        if !settings::is_on("remote_local_desktop") {
            return None;
        }
        let id = self.latest_client()?;
        self.clients.get(&id).filter(|c| c.remote).map(|c| c.id)
    }

    /// Does `ask` where it belongs: on the remote client's computer, or here.
    pub fn remote_desk(&mut self, ask: Desk) {
        match self.desk_client() {
            Some(id) => self.send_desk(id, ask),
            None => super::desktop::here_in_background(ask),
        }
    }

    /// Hands `ask` to the computer a remote client is on, for `switchboard
    /// desktop`; false when there is none to hand it to.
    pub fn hand_to_desk(&mut self, ask: Desk) -> bool {
        match self.desk_client() {
            Some(id) => {
                self.send_desk(id, ask);
                true
            }
            None => false,
        }
    }

    fn send_desk(&self, id: ClientId, ask: Desk) {
        if let Some(c) = self.clients.get(&id) {
            let _ = c.tx.send(ServerMsg::Desk(ask));
        }
    }

    /// This part's share of the tick: the ports read again, off the task,
    /// once `READ_EVERY` has passed, while a remote client is attached.
    pub fn remote_tick(&mut self) {
        // Where a link opened off the task goes, for its wait on the
        // browser (`desktop::went_elsewhere`).
        crate::remote::desktop::set_elsewhere(self.desk_client().is_some());
        if self.remote_client().is_none() || self.remote.reading {
            return;
        }
        if self
            .remote
            .read_at
            .is_some_and(|at| at.elapsed() < READ_EVERY)
        {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.remote.reading = true;
        self.remote.read_at = Some(Instant::now());
        let panes: Vec<_> = self
            .panes
            .iter()
            .filter_map(|(id, slot)| Some((*id, slot.pane.pid()?)))
            .collect();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let found = ports::read(&panes);
            let _ = tx.send(Event::Remote(RemoteEvent::Read(found)));
        });
    }

    pub fn remote_event(&mut self, event: RemoteEvent) {
        match event {
            RemoteEvent::Read(found) => self.ports_read(found),
            RemoteEvent::Forwarded {
                client,
                port,
                local,
                error,
            } => self.port_forwarded(client, port, local, error),
            RemoteEvent::Desk(ask) => self.remote_desk(ask),
        }
    }

    fn ports_read(&mut self, found: Vec<Port>) {
        self.remote.reading = false;
        // Read for a client that has gone since: nothing to forward through.
        if self.remote_client().is_none() {
            return;
        }
        let forwarded: BTreeSet<u16> = self.remote.forwards.keys().copied().collect();
        let auto: BTreeSet<u16> = self
            .remote
            .forwards
            .iter()
            .filter(|(_, f)| !f.by_hand)
            .map(|(p, _)| *p)
            .collect();
        let plan = ports::plan(
            &found,
            &forwarded,
            &auto,
            &self.remote.held,
            settings::is_on("remote_forward_ports"),
        );
        let changed = found != self.remote.ports;
        self.remote.ports = found;
        for port in &plan.release {
            self.remote.held.remove(port);
        }
        for port in &plan.stop {
            self.stop_forwarding(*port, false);
        }
        for (port, to) in plan.forward {
            let _ = self.forward_port(port, &to, false);
        }
        if changed {
            self.remote_changed();
        }
    }

    /// Asks the remote client used last to forward `port`, reached at `to`
    /// here; an error, in words, when no remote client is attached.
    pub fn forward_port(&mut self, port: u16, to: &str, by_hand: bool) -> Result<(), String> {
        let Some(client) = self.remote_client() else {
            return Err("Forwarding a port needs a computer attached with switchboard ssh.".into());
        };
        self.remote.held.remove(&port);
        self.remote.forwards.insert(
            port,
            Forward {
                client,
                to: to.to_string(),
                local: None,
                error: None,
                by_hand,
            },
        );
        self.send_desk(
            client,
            Desk::Forward {
                port,
                to: to.to_string(),
            },
        );
        self.remote_changed();
        Ok(())
    }

    /// Stops forwarding `port`; with `hold`, a port a pane still listens on
    /// isn't forwarded again until it closes.
    pub fn stop_forwarding(&mut self, port: u16, hold: bool) {
        if let Some(f) = self.remote.forwards.remove(&port) {
            self.send_desk(f.client, Desk::Unforward { port });
        }
        if hold && self.remote.ports.iter().any(|p| p.port == port) {
            self.remote.held.insert(port);
        }
        self.remote_changed();
    }

    fn port_forwarded(
        &mut self,
        client: ClientId,
        port: u16,
        local: Option<u16>,
        error: Option<String>,
    ) {
        let Some(f) = self
            .remote
            .forwards
            .get_mut(&port)
            .filter(|f| f.client == client)
        else {
            // Stopped meanwhile, or asked of another client since.
            if local.is_some() {
                self.send_desk(client, Desk::Unforward { port });
            }
            return;
        };
        f.local = local;
        f.error = error.clone();
        let by_hand = f.by_hand;
        if by_hand {
            let said = match (local, &error) {
                (Some(l), _) => format!("Port {port} is at localhost:{l} on your computer."),
                (None, Some(why)) => format!("Couldn't forward port {port}: {why}"),
                (None, None) => format!("Couldn't forward port {port}."),
            };
            self.say(Some(client), &said);
        }
        self.remote_changed();
    }

    /// A client gone: its forwards go with it. With no remote client left,
    /// nothing is read or forwarded any more.
    pub fn remote_gone(&mut self) {
        crate::remote::desktop::set_elsewhere(self.desk_client().is_some());
        let here: BTreeSet<ClientId> = self.clients.keys().copied().collect();
        let before = self.remote.forwards.len();
        self.remote.forwards.retain(|_, f| here.contains(&f.client));
        let mut changed = before != self.remote.forwards.len();
        if self.remote_client().is_none() && !self.remote.ports.is_empty() {
            self.remote.ports.clear();
            self.remote.held.clear();
            changed = true;
        }
        if changed {
            self.remote_changed();
        }
    }

    /// Every client drawn again: the sidebar's cards and the Ports view
    /// show the ports.
    fn remote_changed(&mut self) {
        for c in self.clients.values_mut() {
            c.dirty = true;
        }
    }

    /// A tab's ports for its sidebar card: `⇄ :3000 :5173→5174`, or `None`.
    pub fn tab_ports(&self, tab: TabId) -> Option<String> {
        let mut list: Vec<(u16, Option<u16>)> = self
            .remote
            .ports
            .iter()
            .filter(|p| self.panes.get(&p.pane).is_some_and(|s| s.tab == tab))
            .map(|p| {
                let local = self.remote.forwards.get(&p.port).and_then(|f| f.local);
                (p.port, local)
            })
            .collect();
        list.sort_unstable();
        ports::card_words(&list)
    }

    /// The ports as `list` gives them, for scripts and the tests.
    pub fn ports_json(&self) -> Value {
        let mut out: Vec<Value> = self
            .remote
            .ports
            .iter()
            .map(|p| {
                let f = self.remote.forwards.get(&p.port);
                json!({
                    "port": p.port,
                    "addr": p.addr.to_string(),
                    "pane": format!("%{}", p.pane.0),
                    "program": p.name,
                    "local": f.and_then(|f| f.local),
                    "error": f.and_then(|f| f.error.clone()),
                })
            })
            .collect();
        for (port, f) in &self.remote.forwards {
            if !self.remote.ports.iter().any(|p| p.port == *port) {
                out.push(json!({
                    "port": port,
                    "by_hand": f.by_hand,
                    "local": f.local,
                    "error": f.error,
                }));
            }
        }
        Value::Array(out)
    }

    /// Mission control's Ports view: each port a pane listens on, with its
    /// tab, its program and where it is on your computer, then the ports
    /// forwarded by hand. Enter opens a forwarded one in your browser.
    pub fn ports_view(&self) -> ViewData {
        let attached = self.remote_client().is_some();
        let mut items: Vec<Item> = Vec::new();
        for p in &self.remote.ports {
            let tab = self.panes.get(&p.pane).map(|s| s.tab);
            let number = tab
                .and_then(|t| self.session_of_tab(t)?.tabs.iter().position(|x| *x == t))
                .map(|i| i + 1);
            let label = tab.map(|t| self.tab_label(t)).unwrap_or_default();
            let f = self.remote.forwards.get(&p.port);
            let (state, where_) = forward_words(f);
            let mut detail = Vec::new();
            if let Some(n) = number {
                detail.push(format!("tab {n} {label}"));
            }
            if !p.name.is_empty() {
                detail.push(p.name.clone());
            }
            detail.push(format!("on {}", p.addr));
            detail.push(where_);
            let target = match (f.and_then(|f| f.local), tab) {
                (Some(local), _) => Target::Url(format!("http://localhost:{local}")),
                (None, Some(t)) => Target::Tab(t),
                (None, None) => Target::None,
            };
            items.push(
                Item::new(format!("port:{}", p.port), format!(":{}", p.port))
                    .detail(detail.join(" · "))
                    .state(state)
                    .target(target)
                    .data(json!({
                        "port": p.port,
                        "to": p.to(),
                        "forwarded": f.is_some(),
                    })),
            );
        }
        for (port, f) in &self.remote.forwards {
            if self.remote.ports.iter().any(|p| p.port == *port) {
                continue;
            }
            let (state, where_) = forward_words(Some(f));
            let target = f.local.map_or(Target::None, |l| {
                Target::Url(format!("http://localhost:{l}"))
            });
            items.push(
                Item::new(format!("port:{port}"), format!(":{port}"))
                    .detail(format!("forwarded by hand · {where_}"))
                    .state(state)
                    .target(target)
                    .data(json!({ "port": port, "to": f.to, "forwarded": true })),
            );
        }
        let empty = if attached {
            "No pane listens on a port."
        } else {
            "Ports are read while a computer is attached with switchboard ssh <host>."
        };
        ViewData {
            items,
            empty: Some(empty.into()),
            at: Some(1),
            ..ViewData::default()
        }
    }

    /// M-p's port commands.
    pub fn remote_command(&mut self, client: ClientId, command: Command) {
        match command {
            Command::ForwardPort => {
                if self.remote_client().is_none() {
                    self.say(
                        Some(client),
                        "Forwarding a port needs a computer attached with switchboard ssh.",
                    );
                    return;
                }
                let ask = Ask::new(
                    "Forward a port",
                    "A port on this workspace's computer, reached at localhost on yours.",
                );
                self.open(
                    client,
                    Box::new(ask),
                    Some(Box::new(|server: &mut Server, client, outcome| {
                        let Outcome::Picked(p) = outcome else {
                            return;
                        };
                        match p.value.trim().trim_start_matches(':').parse::<u16>() {
                            Ok(port) if port > 0 => {
                                if let Err(why) = server.forward_port(port, "localhost", true) {
                                    server.say(Some(client), &why);
                                }
                            }
                            _ => server.say(Some(client), &format!("Not a port: {}", p.value)),
                        }
                    })),
                );
            }
            Command::StopForwarding => {
                let items: Vec<PickItem> = self
                    .remote
                    .forwards
                    .iter()
                    .map(|(port, f)| {
                        let (_, where_) = forward_words(Some(f));
                        PickItem::new(format!(":{port} · {where_}"), port.to_string())
                    })
                    .collect();
                if items.is_empty() {
                    self.say(Some(client), "No port is forwarded.");
                    return;
                }
                self.pick(
                    client,
                    Picker::new("Stop forwarding a port", items),
                    |server, client, picked| {
                        let Some(port) = picked.and_then(|p| p.value.parse::<u16>().ok()) else {
                            return;
                        };
                        server.stop_forwarding(port, true);
                        server.say(Some(client), &format!("Stopped forwarding port {port}."));
                    },
                );
            }
        }
    }
}

/// A forward's state and where it is, in words.
fn forward_words(f: Option<&Forward>) -> (State, String) {
    match f {
        None => (State::Idle, "not forwarded".into()),
        Some(Forward { local: Some(l), .. }) => {
            (State::Active, format!("localhost:{l} on your computer"))
        }
        Some(Forward {
            error: Some(why), ..
        }) => (State::Attention, format!("couldn't forward: {why}")),
        Some(_) => (State::Idle, "forwarding…".into()),
    }
}
