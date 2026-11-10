//! A process's working directory, command name and process group's members,
//! every process's CPU time and memory, and the TCP ports a process listens
//! on, read from `/proc` on Linux and
//! from the kernel's process info (libc's `proc_pidinfo`, `proc_listpids`,
//! `sysctl`) on macOS, without starting a process: the server asks for every pane on every focus change, and the
//! tmux version's `ps` and `lsof` calls there were what made it sluggish
//! under a security agent that inspects every process started.
//!
//! What tmux gave as `pane_current_path` and `pane_current_command` came from
//! the same places (its `osdep-linux.c` and `osdep-darwin.c`), so a pane's
//! directory and program read as they did. A process that is gone, or one
//! we may not look at, gives `None` or nothing, never an error: the caller
//! falls back to the pane's own program.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

/// The process's working directory.
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    imp::cwd_of(pid)
}

/// The process's command name (`zsh`, `claude`, `2.1.3`, `node`), as `ps -o
/// comm` shows it without its path.
pub fn command_of(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    imp::command_of(pid).filter(|name| !name.is_empty())
}

/// The process's arguments, its program first.
pub fn args_of(pid: u32) -> Option<Vec<String>> {
    if pid == 0 {
        return None;
    }
    imp::args_of(pid).filter(|args| !args.is_empty())
}

/// Every process in a process group.
pub fn group_members(pgid: u32) -> Vec<u32> {
    // Group 0 is no group: on Linux it is where the kernel's own threads
    // sit, which no pane's program is among.
    if pgid == 0 {
        return Vec::new();
    }
    let mut pids = imp::group_members(pgid);
    pids.sort_unstable();
    pids
}

/// The process's children, for summing a tab's CPU and memory.
pub fn children_of(pid: u32) -> Vec<u32> {
    if pid == 0 {
        return Vec::new();
    }
    let mut pids = imp::children_of(pid);
    pids.sort_unstable();
    pids
}

/// The process group a process is in.
pub fn group_of(pid: u32) -> Option<u32> {
    if pid == 0 {
        return None;
    }
    imp::group_of(pid).filter(|&g| g > 0)
}

/// When a process started: what tells a process from an earlier one that
/// had its pid, since the kernel hands an ended process's pid out again.
pub fn started_at(pid: u32) -> Option<std::time::SystemTime> {
    if pid == 0 {
        return None;
    }
    imp::started_at(pid)
}

/// One process as the Resources view counts it: its parent, its name, the
/// CPU time it has used so far and the memory it holds now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcUsage {
    pub pid: u32,
    pub ppid: u32,
    /// Its command's name, as `ps -o comm` shows it without its path.
    pub name: String,
    /// User and system time together, in nanoseconds since it started.
    pub cpu_ns: u64,
    /// Resident memory, in bytes: memory two processes share counts in both,
    /// as ps's `rss` did.
    pub memory: u64,
}

/// Every process we may look at, with its CPU time and memory: what the
/// sidebar daemon's `ps -A -o pid=,ppid=,rss=,%cpu=,comm=` gave, read from the
/// kernel instead. A few hundred small reads, so it runs off the server's
/// task. A process another user owns is listed with what can be read of it
/// (its parent, so the tree under a pane stays whole), and no time or memory.
pub fn all_usage() -> Vec<ProcUsage> {
    let mut list = imp::all_usage();
    list.sort_unstable_by_key(|p| p.pid);
    list
}

/// The whole computer as the Resources view's top shows it: the CPU time
/// all its cores have spent busy and in all (so two readings give the
/// share used between them), how many cores, and its memory used and
/// installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct System {
    pub cpu_ns_busy: u64,
    pub cpu_ns_total: u64,
    pub cores: usize,
    /// In bytes: on macOS app memory, wired and compressed, as Activity
    /// Monitor's "Memory Used" counts it; on Linux what isn't available.
    pub memory_used: u64,
    pub memory_total: u64,
}

impl System {
    /// The share of the whole computer's CPU used between `before` and
    /// this reading, in percent of all its cores.
    pub fn cpu_since(&self, before: &System) -> f64 {
        let total = self.cpu_ns_total.saturating_sub(before.cpu_ns_total);
        let busy = self.cpu_ns_busy.saturating_sub(before.cpu_ns_busy);
        if total == 0 {
            return 0.0;
        }
        (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// The computer's CPU and memory now, read from the kernel as `all_usage`
/// is (`/proc/stat` and `/proc/meminfo`, or Mach's host statistics);
/// `None` where it can't be read.
pub fn system() -> Option<System> {
    imp::system()
}

/// `/proc/stat`'s first line, `cpu  user nice system idle iowait irq
/// softirq steal ...` in clock ticks, as the ticks busy and in all: idle
/// and iowait are the time not busy.
#[cfg(any(test, target_os = "linux"))]
fn parse_proc_stat_cpu(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let ticks: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|t| t.parse().ok())
        .collect();
    if ticks.len() < 4 {
        return None;
    }
    let total: u64 = ticks.iter().sum();
    let idle = ticks[3] + ticks.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

/// `/proc/meminfo`'s MemTotal and MemAvailable, in bytes, as used and
/// installed.
#[cfg(any(test, target_os = "linux"))]
fn parse_meminfo(info: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = info.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        Some(kb * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    Some((total.saturating_sub(available), total))
}

/// Every process we may look at, with its parent: what finding everything
/// under a pane needs (src/remote/ports.rs), lighter than `all_usage`.
pub fn parents() -> Vec<(u32, u32)> {
    let mut list = imp::parents();
    list.sort_unstable();
    list
}

/// A TCP socket listening for connections: the address it listens on (the
/// unspecified one for every address) and its port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Listener {
    pub addr: IpAddr,
    pub port: u16,
}

/// The TCP sockets each of `pids` listens on, for the ports a remote
/// workspace's panes serve (src/remote): on Linux the LISTEN rows of
/// `/proc/net/tcp` and `/proc/net/tcp6`, matched by their sockets' inodes
/// to the processes' descriptors in `/proc/<pid>/fd`; on macOS each socket
/// descriptor's info from the kernel (`proc_pidfdinfo`). A pid that listens
/// on nothing, or that we may not look at, is left out. A few hundred small
/// reads, so it runs off the server's task.
pub fn listening(pids: &[u32]) -> HashMap<u32, Vec<Listener>> {
    let pids: Vec<u32> = pids.iter().copied().filter(|&p| p > 0).collect();
    let mut found = imp::listening(&pids);
    for list in found.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    found.retain(|_, list| !list.is_empty());
    found
}

/// The listening sockets in `/proc/net/tcp` (or `tcp6`, with `v6`), by
/// their inodes: the rows whose state is `0A`, LISTEN. Each address is
/// printed as the 32-bit words the kernel holds, in the computer's own byte
/// order, and each port in hex.
#[cfg(any(test, target_os = "linux"))]
fn parse_net_tcp(text: &str, v6: bool) -> Vec<(u64, Listener)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3) != Some(&"0A") {
                return None;
            }
            let (addr, port) = fields.get(1)?.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let words: Vec<u32> = (0..addr.len() / 8)
                .map(|i| u32::from_str_radix(addr.get(i * 8..i * 8 + 8)?, 16).ok())
                .collect::<Option<_>>()?;
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
            let addr = match (v6, bytes.len()) {
                (false, 4) => IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?),
                (true, 16) => IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?),
                _ => return None,
            };
            let inode = fields.get(9)?.parse().ok()?;
            Some((inode, Listener { addr, port }))
        })
        .collect()
}

/// The inode of a socket from what its descriptor's link in `/proc/<pid>/fd`
/// reads: `socket:[12345]`.
#[cfg(any(test, target_os = "linux"))]
fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Where `struct socket_fdinfo` (macOS's `sys/proc_info.h`, which the
/// libc crate doesn't carry) keeps what is read of a TCP socket: its
/// `proc_fileinfo` (24 bytes) then its `socket_info`, whose `soi_kind` is
/// at 232 and protocol union at 240; in the union's `tcp_sockinfo`, the
/// `in_sockinfo` first (its local port at 4, its IP version flags at 24,
/// its local address at 48), then the TCP state at 80.
#[cfg(any(test, target_os = "macos"))]
mod socket_fdinfo {
    /// `sizeof(struct socket_fdinfo)`.
    pub const SIZE: usize = 792;
    pub const KIND: usize = 24 + 232;
    const PROTO: usize = 24 + 240;
    pub const LOCAL_PORT: usize = PROTO + 4;
    pub const VERSION: usize = PROTO + 24;
    pub const LOCAL_ADDR: usize = PROTO + 48;
    pub const TCP_STATE: usize = PROTO + 80;
    /// `SOCKINFO_TCP`, `TSI_S_LISTEN`, `INI_IPV6`.
    pub const KIND_TCP: i32 = 2;
    pub const LISTEN: i32 = 1;
    pub const IPV6: u8 = 0x2;
}

/// A listening TCP socket from what `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`
/// filled; `None` for any other socket. The port is kept in network byte
/// order in the low half of an int, as lsof reads it.
#[cfg(any(test, target_os = "macos"))]
fn parse_socket_fdinfo(buf: &[u8]) -> Option<Listener> {
    use socket_fdinfo as f;
    if buf.len() < f::SIZE {
        return None;
    }
    let int = |at: usize| i32::from_ne_bytes(buf[at..at + 4].try_into().expect("four bytes"));
    if int(f::KIND) != f::KIND_TCP || int(f::TCP_STATE) != f::LISTEN {
        return None;
    }
    let port = u16::from_be(int(f::LOCAL_PORT) as u16);
    let addr = if buf[f::VERSION] & f::IPV6 != 0 {
        let bytes: [u8; 16] = buf[f::LOCAL_ADDR..f::LOCAL_ADDR + 16].try_into().ok()?;
        IpAddr::from(bytes)
    } else {
        // An IPv4 address is the last four bytes of its `in4in6_addr`.
        let bytes: [u8; 4] = buf[f::LOCAL_ADDR + 12..f::LOCAL_ADDR + 16]
            .try_into()
            .ok()?;
        IpAddr::from(bytes)
    };
    Some(Listener { addr, port })
}

/// What `/proc/<pid>/stat` says of a process for the Resources view: its
/// name (between the first `(` and the last `)`), parent, and user and
/// system time in clock ticks (fields 14 and 15, counting from one).
#[cfg(any(test, target_os = "linux"))]
fn parse_stat_usage(stat: &str) -> Option<(String, u32, u64, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    Some((name, ppid, utime, stime))
}

/// When a process started, in clock ticks since the computer booted:
/// `/proc/<pid>/stat`'s field 22, counting from one.
#[cfg(any(test, target_os = "linux"))]
fn parse_stat_start(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// When the computer booted, in seconds since the epoch: `/proc/stat`'s
/// `btime` line.
#[cfg(any(test, target_os = "linux"))]
fn parse_boot_time(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// The resident pages in `/proc/<pid>/statm`, its second field.
#[cfg(any(test, target_os = "linux"))]
fn parse_statm_resident(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// The parent and process group of a process, from the fields of
/// `/proc/<pid>/stat` after its command. The command is in parentheses and
/// may itself hold spaces and parentheses (a process can name itself
/// anything), so the fields are read after the last `)`, never by splitting
/// the whole line.
#[cfg(any(test, target_os = "linux"))]
fn parse_stat(stat: &str) -> Option<(u32, u32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((ppid, pgrp))
}

/// A deleted directory reads as its old path with " (deleted)" after it;
/// tmux cut that off too, so a tab named from it keeps its name.
#[cfg(any(test, target_os = "linux"))]
fn strip_deleted(path: PathBuf) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    const SUFFIX: &[u8] = b" (deleted)";
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > SUFFIX.len() && bytes.ends_with(SUFFIX) {
        let mut bytes = path.into_os_string().into_vec();
        bytes.truncate(bytes.len() - SUFFIX.len());
        return PathBuf::from(std::ffi::OsString::from_vec(bytes));
    }
    path
}

/// The arguments in what `sysctl(KERN_PROCARGS2)` gives on macOS: the
/// argument count as a native int, the executable's path, NULs padding it
/// out, then the arguments one after another, each ending in a NUL, and the
/// environment after them, which is left alone.
#[cfg(any(test, target_os = "macos"))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let count = buf.get(..4)?;
    let argc = i32::from_ne_bytes(count.try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    let rest = &buf[4..];
    // Past the executable's path and the NULs after it.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let start = rest[path_end..].iter().position(|&b| b != 0)? + path_end;
    let mut args = Vec::new();
    let mut fields = rest[start..].split(|&b| b == 0);
    for _ in 0..argc {
        match fields.next() {
            Some(arg) => args.push(String::from_utf8_lossy(arg).into_owned()),
            None => break,
        }
    }
    Some(args)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    pub fn cwd_of(pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(super::strip_deleted)
    }

    /// `comm` is the name the kernel keeps, the executable's own (cut to
    /// fifteen bytes) unless the program renamed itself: Claude Code's
    /// executable is named after its version, so it reads `2.1.3` there.
    pub fn command_of(pid: u32) -> Option<String> {
        let comm = fs::read(format!("/proc/{pid}/comm")).ok()?;
        let comm = String::from_utf8_lossy(&comm);
        Some(comm.trim_end_matches('\n').to_string())
    }

    pub fn args_of(pid: u32) -> Option<Vec<String>> {
        let line = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let line = line.strip_suffix(b"\0").unwrap_or(&line);
        if line.is_empty() {
            return None;
        }
        Some(
            line.split(|&b| b == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect(),
        )
    }

    pub fn group_members(pgid: u32) -> Vec<u32> {
        processes()
            .filter(|&(_, _, pgrp)| pgrp == pgid)
            .map(|(pid, _, _)| pid)
            .collect()
    }

    pub fn children_of(pid: u32) -> Vec<u32> {
        processes()
            .filter(|&(_, ppid, _)| ppid == pid)
            .map(|(child, _, _)| child)
            .collect()
    }

    pub fn group_of(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        super::parse_stat(&stat).map(|(_, pgrp)| pgrp)
    }

    pub fn started_at(pid: u32) -> Option<std::time::SystemTime> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = super::parse_stat_start(&stat)?;
        let boot = super::parse_boot_time(&fs::read_to_string("/proc/stat").ok()?)?;
        // SAFETY: sysconf only reads the system's configuration.
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let tick = if tick > 0 { tick as u64 } else { 100 };
        let since_boot = std::time::Duration::from_nanos(ticks * (1_000_000_000 / tick));
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(boot) + since_boot)
    }

    pub fn all_usage() -> Vec<super::ProcUsage> {
        // SAFETY: sysconf only reads the system's configuration.
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        // SAFETY: as above.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let ns_per_tick = if tick > 0 {
            1_000_000_000 / tick as u64
        } else {
            10_000_000
        };
        let page = if page > 0 { page as u64 } else { 4096 };
        pids()
            .filter_map(|pid| {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let (name, ppid, utime, stime) = super::parse_stat_usage(&stat)?;
                let resident = fs::read_to_string(format!("/proc/{pid}/statm"))
                    .ok()
                    .and_then(|m| super::parse_statm_resident(&m))
                    .unwrap_or(0);
                Some(super::ProcUsage {
                    pid,
                    ppid,
                    name,
                    cpu_ns: (utime + stime) * ns_per_tick,
                    memory: resident * page,
                })
            })
            .collect()
    }

    pub fn system() -> Option<super::System> {
        // SAFETY: sysconf only reads the system's configuration.
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let ns_per_tick = if tick > 0 {
            1_000_000_000 / tick as u64
        } else {
            10_000_000
        };
        let (busy, total) = super::parse_proc_stat_cpu(&fs::read_to_string("/proc/stat").ok()?)?;
        let (used, installed) = super::parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)?;
        Some(super::System {
            cpu_ns_busy: busy * ns_per_tick,
            cpu_ns_total: total * ns_per_tick,
            cores: std::thread::available_parallelism().map_or(1, usize::from),
            memory_used: used,
            memory_total: installed,
        })
    }

    pub fn parents() -> Vec<(u32, u32)> {
        processes().map(|(pid, ppid, _)| (pid, ppid)).collect()
    }

    pub fn listening(pids: &[u32]) -> HashMap<u32, Vec<super::Listener>> {
        let mut sockets: HashMap<u64, super::Listener> = HashMap::new();
        for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
            if let Ok(text) = fs::read_to_string(file) {
                sockets.extend(super::parse_net_tcp(&text, v6));
            }
        }
        if sockets.is_empty() {
            return HashMap::new();
        }
        pids.iter()
            .filter_map(|&pid| {
                let found: Vec<super::Listener> = fs::read_dir(format!("/proc/{pid}/fd"))
                    .ok()?
                    .flatten()
                    .filter_map(|entry| fs::read_link(entry.path()).ok())
                    .filter_map(|link| super::socket_inode(&link.to_string_lossy()))
                    .filter_map(|inode| sockets.get(&inode).copied())
                    .collect();
                Some((pid, found))
            })
            .collect()
    }

    fn pids() -> impl Iterator<Item = u32> {
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    }

    /// Every process as (pid, parent, process group). Linux has no list of a
    /// group's members, so this reads each process's `stat`, a few hundred
    /// small reads, cheaper than the one process `ps` would have been. A
    /// process that ends while it is read is left out.
    fn processes() -> impl Iterator<Item = (u32, u32, u32)> {
        pids().filter_map(|pid| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (ppid, pgrp) = super::parse_stat(&stat)?;
            Some((pid, ppid, pgrp))
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::{CStr, OsStr, c_int, c_void};
    use std::mem::{size_of, size_of_val};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;

    /// `proc_listpids`'s filters, from `sys/proc_info.h` (the libc crate
    /// doesn't carry them).
    const PROC_PGRP_ONLY: u32 = 2;
    const PROC_PPID_ONLY: u32 = 6;

    fn c_pid(pid: u32) -> Option<c_int> {
        c_int::try_from(pid).ok()
    }

    pub fn cwd_of(pid: u32) -> Option<PathBuf> {
        let pid = c_pid(pid)?;
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
        // SAFETY: the buffer is the struct the flavor fills, of the size given.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if filled != size {
            return None;
        }
        let path = &info.pvi_cdir.vip_path;
        // SAFETY: `vip_path` is MAXPATHLEN chars laid out one after another
        // (the libc crate splits the array in two only for old compilers).
        let bytes =
            unsafe { std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), size_of_val(path)) };
        let path = CStr::from_bytes_until_nul(bytes).ok()?;
        (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }

    /// The kernel's name for the process (`p_name`, else `p_comm`), which is
    /// the executable's name, as `ps -o comm` shows it without its path;
    /// else the executable's path's last part.
    pub fn command_of(pid: u32) -> Option<String> {
        let pid = c_pid(pid)?;
        let mut name = [0u8; 256];
        // SAFETY: the buffer is as long as the length given.
        let len = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        if len > 0 {
            let name = &name[..(len as usize).min(name.len())];
            return Some(String::from_utf8_lossy(name).into_owned());
        }
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is as long as the length given.
        let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if len <= 0 {
            return None;
        }
        path.truncate(len as usize);
        let path = Path::new(OsStr::from_bytes(&path));
        Some(path.file_name()?.to_string_lossy().into_owned())
    }

    pub fn args_of(pid: u32) -> Option<Vec<String>> {
        let pid = c_pid(pid)?;
        let mut argmax: c_int = 0;
        let mut size = size_of::<c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: the out-pointer is an int and the size says so.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                (&raw mut argmax).cast::<c_void>(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        };
        if ok != 0 || argmax <= 0 {
            return None;
        }
        let mut buf = vec![0u8; argmax as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: the buffer is as long as the size given, which the kernel
        // sets to what it wrote.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr().cast::<c_void>(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        };
        if ok != 0 {
            return None;
        }
        buf.truncate(size);
        super::parse_procargs2(&buf)
    }

    pub fn group_members(pgid: u32) -> Vec<u32> {
        list_pids(PROC_PGRP_ONLY, pgid)
    }

    pub fn children_of(pid: u32) -> Vec<u32> {
        list_pids(PROC_PPID_ONLY, pid)
    }

    /// The kernel's BSD info on a process: its parent, group and name.
    fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        let pid = c_pid(pid)?;
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: the buffer is the struct the flavor fills, of the size given.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (filled == size).then_some(info)
    }

    pub fn group_of(pid: u32) -> Option<u32> {
        bsd_info(pid).map(|info| info.pbi_pgid)
    }

    pub fn started_at(pid: u32) -> Option<std::time::SystemTime> {
        let info = bsd_info(pid)?;
        let micros = u32::try_from(info.pbi_start_tvusec).unwrap_or(0);
        Some(std::time::UNIX_EPOCH + std::time::Duration::new(info.pbi_start_tvsec, micros * 1000))
    }

    /// The task's resident memory and CPU time, which the kernel gives only
    /// for our own processes.
    fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
        let pid = c_pid(pid)?;
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_taskinfo>() as c_int;
        // SAFETY: the buffer is the struct the flavor fills, of the size given.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (filled == size).then_some(info)
    }

    /// The task's times are in Mach's absolute time units, which are
    /// nanoseconds on an Intel Mac and 125/3 of one on Apple silicon; the
    /// timebase says which. (The libc crate marks the call deprecated in
    /// favor of the mach2 crate, for this one number.)
    #[allow(deprecated)]
    fn timebase() -> (u64, u64) {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: the out-pointer is the struct the call fills.
        let ok = unsafe { libc::mach_timebase_info(&raw mut info) };
        if ok != 0 || info.numer == 0 || info.denom == 0 {
            return (1, 1);
        }
        (info.numer as u64, info.denom as u64)
    }

    /// A C string in a fixed array, up to its first NUL.
    fn c_text(chars: &[std::ffi::c_char]) -> String {
        // SAFETY: c_char and u8 have the same size and layout.
        let bytes = unsafe { std::slice::from_raw_parts(chars.as_ptr().cast::<u8>(), chars.len()) };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    pub fn all_usage() -> Vec<super::ProcUsage> {
        let (numer, denom) = timebase();
        all_pids()
            .into_iter()
            .filter_map(|pid| {
                let bsd = bsd_info(pid)?;
                let name = match c_text(&bsd.pbi_name) {
                    n if n.is_empty() => c_text(&bsd.pbi_comm),
                    n => n,
                };
                let task = task_info(pid);
                let ticks = task.map_or(0, |t| t.pti_total_user + t.pti_total_system);
                Some(super::ProcUsage {
                    pid,
                    ppid: bsd.pbi_ppid,
                    name,
                    cpu_ns: (ticks as u128 * numer as u128 / denom as u128) as u64,
                    memory: task.map_or(0, |t| t.pti_resident_size),
                })
            })
            .collect()
    }

    /// The host's CPU ticks over every core (`HOST_CPU_LOAD_INFO`, at 100 a
    /// second) and its pages by kind (`HOST_VM_INFO64`), with the memory
    /// installed (`hw.memsize`). (The libc crate marks `mach_host_self`
    /// deprecated in favor of the mach2 crate.)
    #[allow(deprecated)]
    pub fn system() -> Option<super::System> {
        // SAFETY: returns a send right to the host port; nothing is passed.
        let host = unsafe { libc::mach_host_self() };
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut load: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: the out-pointer is the struct the flavor fills, its size
        // in ints given in `count`.
        let ok = unsafe {
            libc::host_statistics(
                host,
                libc::HOST_CPU_LOAD_INFO,
                (&raw mut load).cast(),
                &mut count,
            )
        };
        if ok != libc::KERN_SUCCESS {
            return None;
        }
        let ticks = load.cpu_ticks.map(u64::from);
        let total: u64 = ticks.iter().sum();
        let idle = ticks[libc::CPU_STATE_IDLE as usize];
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: as above, for the 64-bit VM statistics.
        let ok = unsafe {
            libc::host_statistics64(host, libc::HOST_VM_INFO64, (&raw mut vm).cast(), &mut count)
        };
        if ok != libc::KERN_SUCCESS {
            return None;
        }
        // SAFETY: sysconf only reads the system's configuration.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = if page > 0 { page as u64 } else { 16384 };
        // App memory (anonymous pages less purgeable ones), wired and
        // compressed: Activity Monitor's "Memory Used".
        let app = u64::from(vm.internal_page_count).saturating_sub(u64::from(vm.purgeable_count));
        let pages = app + u64::from(vm.wire_count) + u64::from(vm.compressor_page_count);
        let mut installed: u64 = 0;
        let mut size = size_of::<u64>();
        // SAFETY: the out-pointer is a u64 and the size says so.
        let ok = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut installed).cast::<c_void>(),
                &mut size,
                ptr::null_mut(),
                0,
            )
        };
        if ok != 0 || installed == 0 {
            return None;
        }
        // A tick is a hundredth of a second.
        const NS_PER_TICK: u64 = 10_000_000;
        Some(super::System {
            cpu_ns_busy: (total - idle) * NS_PER_TICK,
            cpu_ns_total: total * NS_PER_TICK,
            cores: std::thread::available_parallelism().map_or(1, usize::from),
            memory_used: (pages * page).min(installed),
            memory_total: installed,
        })
    }

    pub fn parents() -> Vec<(u32, u32)> {
        all_pids()
            .into_iter()
            .filter_map(|pid| Some((pid, bsd_info(pid)?.pbi_ppid)))
            .collect()
    }

    /// `proc_pidfdinfo`'s flavor for a socket's info, from `sys/proc_info.h`.
    const PROC_PIDFDSOCKETINFO: c_int = 3;

    pub fn listening(pids: &[u32]) -> HashMap<u32, Vec<super::Listener>> {
        pids.iter().map(|&pid| (pid, listening_of(pid))).collect()
    }

    /// The listening TCP sockets among a process's descriptors: its
    /// descriptors listed (asked once for the room needed, then filled),
    /// each socket's info read.
    fn listening_of(pid: u32) -> Vec<super::Listener> {
        let Some(pid) = c_pid(pid) else {
            return Vec::new();
        };
        // SAFETY: a null buffer asks only for the size.
        let need = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let one = size_of::<libc::proc_fdinfo>();
        let room = need as usize / one + 16;
        // SAFETY: a plain C struct, for which all zeroes is a valid value.
        let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; room];
        // SAFETY: the buffer holds `room` of the structs the flavor fills, of
        // the size given in bytes.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast::<c_void>(),
                (room * one) as c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        fds.truncate((filled as usize / one).min(room));
        fds.iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .filter_map(|fd| {
                // Room past the struct's size, aligned for its 64-bit fields.
                let mut buf = [0u64; 128];
                // SAFETY: the buffer is larger than the struct the flavor
                // fills, and its size is given.
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDSOCKETINFO,
                        buf.as_mut_ptr().cast::<c_void>(),
                        size_of_val(&buf) as c_int,
                    )
                };
                if got <= 0 {
                    return None;
                }
                // SAFETY: u64s seen as the bytes they are made of.
                let bytes = unsafe {
                    std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size_of_val(&buf))
                };
                super::parse_socket_fdinfo(&bytes[..(got as usize).min(bytes.len())])
            })
            .collect()
    }

    /// Every pid: asked once with no buffer for how many, then filled, with
    /// room for a few more started between the two calls.
    fn all_pids() -> Vec<u32> {
        // SAFETY: a null buffer asks only for the count.
        let need = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let room = need as usize + 64;
        let mut pids: Vec<c_int> = vec![0; room];
        // SAFETY: the buffer holds `room` ints, the size given in bytes.
        let filled = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast::<c_void>(),
                (room * size_of::<c_int>()) as c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        pids.truncate((filled as usize).min(room));
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| pid as u32)
            .collect()
    }

    /// The kernel's list of pids that match a filter: asked once with no
    /// buffer for how much room it needs, then filled. Room for a few more
    /// is added, since a process can start between the two calls.
    fn list_pids(filter: u32, arg: u32) -> Vec<u32> {
        // SAFETY: a null buffer asks only for the size.
        let need = unsafe { libc::proc_listpids(filter, arg, ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let room = need as usize / size_of::<c_int>() + 16;
        let mut pids: Vec<c_int> = vec![0; room];
        // SAFETY: the buffer holds `room` ints, the size given in bytes.
        let filled = unsafe {
            libc::proc_listpids(
                filter,
                arg,
                pids.as_mut_ptr().cast::<c_void>(),
                (room * size_of::<c_int>()) as c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        pids.truncate(filled as usize / size_of::<c_int>());
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| pid as u32)
            .collect()
    }
}

/// Elsewhere there is nothing to read; the pane's own program stands in.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::path::PathBuf;

    pub fn cwd_of(_pid: u32) -> Option<PathBuf> {
        None
    }
    pub fn command_of(_pid: u32) -> Option<String> {
        None
    }
    pub fn args_of(_pid: u32) -> Option<Vec<String>> {
        None
    }
    pub fn group_members(_pgid: u32) -> Vec<u32> {
        Vec::new()
    }
    pub fn children_of(_pid: u32) -> Vec<u32> {
        Vec::new()
    }
    pub fn group_of(_pid: u32) -> Option<u32> {
        None
    }
    pub fn started_at(_pid: u32) -> Option<std::time::SystemTime> {
        None
    }
    pub fn all_usage() -> Vec<super::ProcUsage> {
        Vec::new()
    }
    pub fn system() -> Option<super::System> {
        None
    }
    pub fn parents() -> Vec<(u32, u32)> {
        Vec::new()
    }
    pub fn listening(_pids: &[u32]) -> std::collections::HashMap<u32, Vec<super::Listener>> {
        std::collections::HashMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The addresses as this computer's kernel prints them: each 32-bit word
    /// in its own byte order, as `/proc/net/tcp` writes it.
    fn word(bytes: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(bytes))
    }

    #[test]
    fn listening_sockets_are_read_from_proc_net_tcp() {
        let v4 = format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             \x20  0: {}:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0\n\
             \x20  1: {}:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 17 1 0000000000000000 100 0 0 10 0\n\
             \x20  2: {}:9C40 {}:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 99 1 0000000000000000 20 4 30 10 -1\n",
            word([127, 0, 0, 1]),
            word([0, 0, 0, 0]),
            word([10, 0, 0, 5]),
            word([93, 184, 216, 34]),
        );
        let found = parse_net_tcp(&v4, false);
        assert_eq!(
            found,
            vec![
                (
                    4242,
                    Listener {
                        addr: "127.0.0.1".parse().unwrap(),
                        port: 8080
                    }
                ),
                (
                    17,
                    Listener {
                        addr: "0.0.0.0".parse().unwrap(),
                        port: 22
                    }
                ),
            ],
            "an established connection is no listener"
        );
        let loopback6 = [
            word([0, 0, 0, 0]),
            word([0, 0, 0, 0]),
            word([0, 0, 0, 0]),
            word([0, 0, 0, 1]),
        ]
        .concat();
        let v6 = format!(
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             \x20  0: {loopback6}:1538 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555 1 0000000000000000 100 0 0 10 0\n"
        );
        assert_eq!(
            parse_net_tcp(&v6, true),
            vec![(
                555,
                Listener {
                    addr: "::1".parse().unwrap(),
                    port: 5432
                }
            )]
        );
        assert!(parse_net_tcp("header only\n", false).is_empty());
        assert!(parse_net_tcp("h\n  0: nonsense 0A\n", false).is_empty());
    }

    #[test]
    fn a_descriptor_names_its_socket_by_inode() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(socket_inode("pipe:[12345]"), None);
        assert_eq!(socket_inode("/dev/pts/3"), None);
        assert_eq!(socket_inode("socket:[x]"), None);
    }

    /// A `socket_fdinfo` as macOS fills it for a TCP socket.
    fn socket_info(state: i32, v6: bool, addr: &[u8], port: u16) -> Vec<u8> {
        use socket_fdinfo as f;
        let mut buf = vec![0u8; f::SIZE];
        buf[f::KIND..f::KIND + 4].copy_from_slice(&f::KIND_TCP.to_ne_bytes());
        buf[f::TCP_STATE..f::TCP_STATE + 4].copy_from_slice(&state.to_ne_bytes());
        // The port in network order, in the low half of a native int.
        let lport = i32::from(u16::from_ne_bytes(port.to_be_bytes()));
        buf[f::LOCAL_PORT..f::LOCAL_PORT + 4].copy_from_slice(&lport.to_ne_bytes());
        buf[f::VERSION] = if v6 { f::IPV6 } else { 0x1 };
        let at = if v6 {
            f::LOCAL_ADDR
        } else {
            f::LOCAL_ADDR + 12
        };
        buf[at..at + addr.len()].copy_from_slice(addr);
        buf
    }

    #[test]
    fn a_listening_socket_is_read_from_macos_socket_info() {
        assert_eq!(
            parse_socket_fdinfo(&socket_info(1, false, &[127, 0, 0, 1], 3000)),
            Some(Listener {
                addr: "127.0.0.1".parse().unwrap(),
                port: 3000
            })
        );
        let mut any6 = [0u8; 16];
        any6[15] = 0;
        assert_eq!(
            parse_socket_fdinfo(&socket_info(1, true, &any6, 5173)),
            Some(Listener {
                addr: "::".parse().unwrap(),
                port: 5173
            })
        );
        // Established, not listening.
        assert_eq!(
            parse_socket_fdinfo(&socket_info(4, false, &[127, 0, 0, 1], 3000)),
            None
        );
        // Cut short.
        assert_eq!(
            parse_socket_fdinfo(&socket_info(1, false, &[127, 0, 0, 1], 3000)[..400]),
            None
        );
        // Another kind of socket.
        let mut udp = socket_info(1, false, &[127, 0, 0, 1], 3000);
        udp[socket_fdinfo::KIND] = 1;
        assert_eq!(parse_socket_fdinfo(&udp), None);
    }

    #[test]
    fn this_process_is_seen_listening_on_its_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let found = listening(&[me]);
        let mine = found.get(&me).cloned().unwrap_or_default();
        assert!(
            mine.contains(&Listener {
                addr: "127.0.0.1".parse().unwrap(),
                port
            }),
            "{port} among {mine:?}"
        );
        drop(listener);
        assert!(parents().iter().any(|&(pid, _)| pid == me));
    }

    #[test]
    fn the_whole_computer_reads_its_memory_and_cpu() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let before = system().expect("the computer's figures");
            assert!(before.memory_total > 0);
            assert!(before.memory_used <= before.memory_total);
            assert!(before.cores >= 1);
            assert!(before.cpu_ns_busy <= before.cpu_ns_total);
            let after = system().expect("the computer's figures");
            let cpu = after.cpu_since(&before);
            assert!((0.0..=100.0).contains(&cpu));
        }
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_stat_cpu(stat), Some((150, 1000)));
        let info = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:   4000 kB\n";
        assert_eq!(parse_meminfo(info), Some((12000 * 1024, 16000 * 1024)));
        let at = |busy, total| System {
            cpu_ns_busy: busy,
            cpu_ns_total: total,
            ..System::default()
        };
        assert_eq!(at(300, 1000).cpu_since(&at(100, 600)), 50.0);
        assert_eq!(at(100, 600).cpu_since(&at(100, 600)), 0.0);
    }

    #[test]
    fn stat_is_read_after_the_last_parenthesis() {
        let stat = "4242 (a (b) c) d) S 17 4240 17 34816 4240 4194304 0 0";
        assert_eq!(parse_stat(stat), Some((17, 4240)));
        assert_eq!(parse_stat("1 (init) S 0 1 1"), Some((0, 1)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S"), None);
    }

    #[test]
    fn a_process_s_start_is_read_from_stat_and_the_boot_time() {
        let stat = "4242 (my (odd) name) S 17 4240 17 34816 4240 4194304 120 0 3 0 250 50 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(parse_stat_start(stat), Some(98765));
        assert_eq!(parse_stat_start("1 (x) S 0 1"), None);
        assert_eq!(
            parse_boot_time("cpu  1 2 3\nintr 5\nbtime 1762750000\nprocesses 9\n"),
            Some(1762750000)
        );
        assert_eq!(parse_boot_time("cpu 1\n"), None);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn this_process_started_in_the_past() {
        let started = started_at(std::process::id()).unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(started)
            .unwrap();
        assert!(age < std::time::Duration::from_secs(3600), "{age:?}");
    }

    #[test]
    fn usage_is_read_from_stat_and_statm() {
        let stat = "4242 (my (odd) name) S 17 4240 17 34816 4240 4194304 120 0 3 0 250 50 0 0 20 0";
        assert_eq!(
            parse_stat_usage(stat),
            Some(("my (odd) name".to_string(), 17, 250, 50))
        );
        assert_eq!(parse_stat_usage("1 (x) S 0"), None);
        assert_eq!(parse_statm_resident("1000 256 30 1 0 90 0\n"), Some(256));
        assert_eq!(parse_statm_resident("1000"), None);
    }

    #[test]
    fn a_deleted_directory_reads_as_its_old_path() {
        assert_eq!(
            strip_deleted(PathBuf::from("/tmp/gone (deleted)")),
            PathBuf::from("/tmp/gone")
        );
        assert_eq!(
            strip_deleted(PathBuf::from("/tmp/here")),
            PathBuf::from("/tmp/here")
        );
        assert_eq!(
            strip_deleted(PathBuf::from(" (deleted)")),
            PathBuf::from(" (deleted)")
        );
    }

    #[test]
    fn procargs2_gives_the_arguments_not_the_path_or_environment() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0\0");
        buf.extend_from_slice(b"node\0/opt/claude/cli.js\0--resume\0HOME=/Users/x\0PATH=/bin\0");
        assert_eq!(
            parse_procargs2(&buf),
            Some(vec![
                "node".to_string(),
                "/opt/claude/cli.js".to_string(),
                "--resume".to_string()
            ])
        );
        assert_eq!(parse_procargs2(&[1, 0]), None);
        assert_eq!(parse_procargs2(&0i32.to_ne_bytes()), None);
        let mut short = 2i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/bin/sh\0sh\0");
        assert_eq!(
            parse_procargs2(&short),
            Some(vec!["sh".to_string(), String::new()])
        );
    }

    #[test]
    fn nothing_for_pid_zero_or_a_process_that_is_not_there() {
        assert_eq!(cwd_of(0), None);
        assert_eq!(command_of(0), None);
        assert_eq!(args_of(0), None);
        assert!(group_members(0).is_empty());
        assert!(children_of(0).is_empty());
        assert_eq!(group_of(0), None);
        assert_eq!(group_of(u32::MAX - 1), None);
        assert_eq!(started_at(0), None);
        assert_eq!(started_at(u32::MAX - 1), None);
        // Past any pid the kernel hands out (Linux's limit is 2^22).
        assert_eq!(cwd_of(u32::MAX - 1), None);
        assert_eq!(command_of(u32::MAX - 1), None);
        assert!(group_members(u32::MAX - 1).is_empty());
    }

    #[cfg(target_os = "linux")]
    mod live {
        use super::super::*;
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "sb-procinfo-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            dir.canonicalize().expect("canonical scratch dir")
        }

        /// Ends the process (and its group, when it leads one) however the
        /// test ends.
        struct Reaped(Child);
        impl Drop for Reaped {
            fn drop(&mut self) {
                // SAFETY: signals only the test's own process group.
                unsafe { libc::killpg(self.0.id() as i32, libc::SIGKILL) };
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn until<T>(mut check: impl FnMut() -> Option<T>) -> T {
            let start = Instant::now();
            loop {
                if let Some(found) = check() {
                    return found;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "timed out");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn a_sleeps_directory_command_and_arguments() {
            let dir = scratch("sleep");
            let child = Command::new("sleep")
                .arg("30")
                .current_dir(&dir)
                .stdin(Stdio::null())
                .spawn()
                .expect("sleep");
            let child = Reaped(child);
            let pid = child.0.id();
            assert_eq!(cwd_of(pid), Some(dir.clone()));
            // The name and arguments change only once `sleep` has been
            // exec'd (the arguments a moment after the name), so wait for it.
            assert_eq!(until(|| command_of(pid).filter(|c| c == "sleep")), "sleep");
            assert_eq!(
                until(|| args_of(pid).filter(|a| a[0] == "sleep")),
                vec!["sleep".to_string(), "30".to_string()]
            );
            drop(child);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_deleted_directory_is_still_named() {
            let dir = scratch("deleted");
            let child = Command::new("sleep")
                .arg("30")
                .current_dir(&dir)
                .stdin(Stdio::null())
                .spawn()
                .expect("sleep");
            let child = Reaped(child);
            std::fs::remove_dir(&dir).expect("remove dir");
            assert_eq!(cwd_of(child.0.id()), Some(dir));
        }

        #[test]
        fn a_groups_members_and_a_parents_children() {
            let child = Command::new("/bin/sh")
                .args(["-c", "sleep 30 & sleep 30 & wait"])
                .process_group(0)
                .stdin(Stdio::null())
                .spawn()
                .expect("sh");
            let child = Reaped(child);
            let pid = child.0.id();
            let children = until(|| {
                let kids = children_of(pid);
                (kids.len() == 2).then_some(kids)
            });
            let members = until(|| {
                let members = group_members(pid);
                (members.len() == 3).then_some(members)
            });
            assert!(members.contains(&pid));
            for kid in &children {
                assert!(members.contains(kid), "{kid} in {members:?}");
                assert_eq!(until(|| command_of(*kid).filter(|c| c == "sleep")), "sleep");
            }
            // SAFETY: getpgrp only reads the test's own process group.
            let ours = unsafe { libc::getpgrp() } as u32;
            assert!(!group_members(ours).contains(&pid));
            assert_eq!(group_of(pid), Some(pid));
            assert_eq!(group_of(std::process::id()), Some(ours));
        }

        #[test]
        fn every_process_is_listed_with_its_parent_and_memory() {
            let list = all_usage();
            let me = list
                .iter()
                .find(|p| p.pid == std::process::id())
                .expect("this test's own process");
            assert!(me.memory > 0, "{me:?}");
            // SAFETY: getppid only reads the test's own parent.
            assert_eq!(me.ppid, unsafe { libc::getppid() } as u32);
        }
    }
}
