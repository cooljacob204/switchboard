//! The TCP ports a remote workspace's panes listen on, and which of them
//! to forward to the computer you're at, as VS Code's remote window
//! forwards the ports its terminals' programs open: a dev server started in
//! a pane on the remote is at the same port on `localhost` on yours.
//!
//! What a pane listens on is every listening socket of every process under
//! its program (the shell, what it started, what that started), read from
//! the kernel without starting a process (`procinfo::listening`), off the
//! server's task, every few seconds and only while a remote client is
//! attached (src/remote/serve.rs). A port listened on twice (IPv4 and IPv6,
//! or by two panes) is one port, the first pane's.
//!
//! What to do after a reading is `plan`'s: forward each new port (while
//! `remote_forward_ports` is on), stop each forwarded port no pane listens
//! on any more, unless it was forwarded by hand, and leave alone a port
//! whose forward was stopped by hand until it closes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;

use crate::mux::PaneId;
use crate::mux::procinfo::{self, Listener};

/// A port a pane listens on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub port: u16,
    /// The address it listens on, the unspecified one for every address.
    pub addr: IpAddr,
    pub pane: PaneId,
    /// The process listening, and its name (empty where it wasn't read).
    pub pid: u32,
    pub name: String,
}

impl Port {
    /// Where the remote's end of a forward connects to reach it:
    /// `localhost` for a port on every address or on loopback, else the
    /// address it listens on alone, as `ssh -L` takes it.
    pub fn to(&self) -> String {
        if self.addr.is_unspecified() || self.addr.is_loopback() {
            return "localhost".into();
        }
        match self.addr {
            IpAddr::V4(a) => a.to_string(),
            IpAddr::V6(a) => format!("[{a}]"),
        }
    }
}

/// Every port the panes' processes listen on, one each, by port: `panes`
/// each with its program's pid, `parents` every process's parent, and
/// `listening` what each process listens on.
pub fn under_panes(
    panes: &[(PaneId, u32)],
    parents: &[(u32, u32)],
    listening: &HashMap<u32, Vec<Listener>>,
) -> Vec<Port> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid) in parents {
        if pid != ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut ports: BTreeMap<u16, Port> = BTreeMap::new();
    for &(pane, root) in panes {
        for pid in descendants(root, &children) {
            for l in listening.get(&pid).into_iter().flatten() {
                ports.entry(l.port).or_insert(Port {
                    port: l.port,
                    addr: l.addr,
                    pane,
                    pid,
                    name: String::new(),
                });
            }
        }
    }
    ports.into_values().collect()
}

/// A process and everything under it, itself first; a loop in what the
/// kernel said (a pid reused between two reads) is walked once.
fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut todo = vec![root];
    while let Some(pid) = todo.pop() {
        if pid == 0 || !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        if let Some(kids) = children.get(&pid) {
            todo.extend(kids.iter().rev());
        }
    }
    out
}

/// Every pane's ports now: the reading itself, blocking, for a task off the
/// server's.
pub fn read(panes: &[(PaneId, u32)]) -> Vec<Port> {
    let parents = procinfo::parents();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid) in &parents {
        children.entry(ppid).or_default().push(pid);
    }
    let pids: Vec<u32> = panes
        .iter()
        .flat_map(|&(_, root)| descendants(root, &children))
        .collect();
    let mut found = under_panes(panes, &parents, &procinfo::listening(&pids));
    for p in &mut found {
        p.name = procinfo::command_of(p.pid).unwrap_or_default();
    }
    found
}

/// What a reading calls for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Ports to forward, each with where the remote's end connects.
    pub forward: Vec<(u16, String)>,
    /// Forwarded ports to stop forwarding.
    pub stop: Vec<u16>,
    /// Ports stopped by hand that have closed, to forward again when they
    /// open next.
    pub release: Vec<u16>,
}

/// What to do after a reading, `now`: forward each new port when `auto`,
/// unless it is `held` (its forward stopped by hand) or already
/// `forwarded`; stop each port forwarded for a pane (`auto_forwarded`) that
/// nothing listens on now; let go of each held port that has closed. A port
/// forwarded by hand is never stopped here, since a reading can't see a
/// port that no pane's process holds (a container's, say).
pub fn plan(
    now: &[Port],
    forwarded: &BTreeSet<u16>,
    auto_forwarded: &BTreeSet<u16>,
    held: &BTreeSet<u16>,
    auto: bool,
) -> Plan {
    let open: BTreeSet<u16> = now.iter().map(|p| p.port).collect();
    Plan {
        forward: if auto {
            now.iter()
                .filter(|p| !forwarded.contains(&p.port) && !held.contains(&p.port))
                .map(|p| (p.port, p.to()))
                .collect()
        } else {
            Vec::new()
        },
        stop: auto_forwarded
            .iter()
            .filter(|p| !open.contains(p))
            .copied()
            .collect(),
        release: held.iter().filter(|p| !open.contains(p)).copied().collect(),
    }
}

/// The ports on the computer you're at to try for a pane's `port`, in
/// order: the same one, then the next twenty, then 0 for one the system
/// picks.
pub fn local_candidates(port: u16) -> Vec<u16> {
    let mut out: Vec<u16> = (0..=20u16)
        .filter_map(|n| port.checked_add(n))
        .filter(|&p| p > 0)
        .collect();
    out.push(0);
    out
}

/// Whether a port on this computer's loopback is free to forward to, and
/// which it is (the system's pick for 0): bound for a moment and let go.
pub fn free_local(port: u16) -> Option<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).ok()?;
    listener.local_addr().ok().map(|a| a.port())
}

/// A pane's ports in a few words for its sidebar card: `⇄ :3000
/// :5173→5174`, a port at another number on your computer shown with the
/// one it is at there, one not forwarded with no arrow after it.
pub fn card_words(ports: &[(u16, Option<u16>)]) -> Option<String> {
    if ports.is_empty() {
        return None;
    }
    let words: Vec<String> = ports
        .iter()
        .map(|&(port, local)| match local {
            Some(l) if l != port => format!(":{port}→{l}"),
            _ => format!(":{port}"),
        })
        .collect();
    Some(format!("⇄ {}", words.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(addr: &str, port: u16) -> Listener {
        Listener {
            addr: addr.parse().unwrap(),
            port,
        }
    }

    #[test]
    fn a_pane_listens_on_whatever_runs_under_it() {
        // Pane 1's shell 100 started npm 101, which started node 102; pane
        // 2's shell 200 runs postgres 201. Process 300 is no pane's.
        let parents = [
            (100, 1),
            (101, 100),
            (102, 101),
            (200, 1),
            (201, 200),
            (300, 1),
        ];
        let listening = HashMap::from([
            (102, vec![listener("0.0.0.0", 5173), listener("::", 5173)]),
            (201, vec![listener("127.0.0.1", 5432)]),
            (300, vec![listener("0.0.0.0", 22)]),
        ]);
        let ports = under_panes(&[(PaneId(1), 100), (PaneId(2), 200)], &parents, &listening);
        assert_eq!(
            ports,
            vec![
                Port {
                    port: 5173,
                    addr: "0.0.0.0".parse().unwrap(),
                    pane: PaneId(1),
                    pid: 102,
                    name: String::new(),
                },
                Port {
                    port: 5432,
                    addr: "127.0.0.1".parse().unwrap(),
                    pane: PaneId(2),
                    pid: 201,
                    name: String::new(),
                },
            ],
            "one port each, the system's own left out"
        );
    }

    #[test]
    fn a_loop_in_the_process_tree_is_walked_once() {
        let parents = [(10, 11), (11, 10)];
        let listening = HashMap::from([(11, vec![listener("127.0.0.1", 8000)])]);
        let ports = under_panes(&[(PaneId(1), 10)], &parents, &listening);
        assert_eq!(ports.len(), 1);
    }

    #[test]
    fn a_forward_reaches_the_address_a_port_listens_on() {
        let port = |addr: &str| Port {
            port: 1,
            addr: addr.parse().unwrap(),
            pane: PaneId(1),
            pid: 1,
            name: String::new(),
        };
        assert_eq!(port("0.0.0.0").to(), "localhost");
        assert_eq!(port("::").to(), "localhost");
        assert_eq!(port("127.0.0.1").to(), "localhost");
        assert_eq!(port("::1").to(), "localhost");
        assert_eq!(port("10.0.0.5").to(), "10.0.0.5");
        assert_eq!(port("fd00::5").to(), "[fd00::5]");
    }

    fn ports(list: &[u16]) -> Vec<Port> {
        list.iter()
            .map(|&port| Port {
                port,
                addr: "0.0.0.0".parse().unwrap(),
                pane: PaneId(1),
                pid: 1,
                name: String::new(),
            })
            .collect()
    }

    fn set(list: &[u16]) -> BTreeSet<u16> {
        list.iter().copied().collect()
    }

    #[test]
    fn new_ports_are_forwarded_and_closed_ones_stopped() {
        let plan = plan(
            &ports(&[3000, 5173, 8080]),
            &set(&[3000, 9000, 9999]),
            &set(&[3000, 9000]),
            &set(&[8080, 4000]),
            true,
        );
        assert_eq!(plan.forward, vec![(5173, "localhost".to_string())]);
        assert_eq!(
            plan.stop,
            vec![9000],
            "9999 was forwarded by hand, and stays"
        );
        assert_eq!(plan.release, vec![4000], "8080 stays held while open");
    }

    #[test]
    fn with_forwarding_off_nothing_new_is_forwarded() {
        let plan = plan(
            &ports(&[3000]),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            false,
        );
        assert!(plan.forward.is_empty());
    }

    #[test]
    fn a_port_is_tried_as_it_is_then_the_next_ones() {
        let c = local_candidates(3000);
        assert_eq!(&c[..3], &[3000, 3001, 3002]);
        assert_eq!(c.len(), 22);
        assert_eq!(c.last(), Some(&0), "then the system's pick");
        let top = local_candidates(u16::MAX);
        assert_eq!(top, vec![u16::MAX, 0]);
    }

    #[test]
    fn a_free_port_is_found_and_a_taken_one_is_not() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        assert_eq!(free_local(port), None);
        assert!(free_local(0).is_some_and(|p| p > 0));
    }

    #[test]
    fn a_card_says_each_port_and_where_it_is_here() {
        assert_eq!(card_words(&[]), None);
        assert_eq!(
            card_words(&[(3000, Some(3000)), (5173, Some(5174)), (8080, None)]).as_deref(),
            Some("⇄ :3000 :5173→5174 :8080")
        );
    }
}
