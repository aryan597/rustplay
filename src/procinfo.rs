//! Linux /proc inspection: the browser's process tree and the TCP sockets it
//! is listening on. Used to *prove* pipe mode exposes no DevTools port, both in
//! tests and as a launch-time check.
//!
//! On non-Linux platforms these return `None`, meaning "cannot inspect", which
//! callers must not treat as "no listeners".

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// All live pids descended from `root` (inclusive), plus any process in
/// `root`'s process group (helpers re-parented to init stay in the group).
pub fn process_tree(root: u32) -> Option<Vec<u32>> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut in_group = Vec::new();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        if let Some((ppid, pgid, state)) = parse_stat(&stat) {
            if state == 'Z' {
                continue;
            }
            children.entry(ppid).or_default().push(pid);
            if pgid == root {
                in_group.push(pid);
            }
        }
    }
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut stack = vec![root];
    stack.extend(in_group);
    while let Some(p) = stack.pop() {
        if seen.insert(p) {
            if let Some(c) = children.get(&p) {
                stack.extend(c);
            }
        }
    }
    // The root itself may already be gone.
    seen.retain(|p| std::path::Path::new(&format!("/proc/{p}")).exists());
    Some(seen.into_iter().collect())
}

/// (ppid, pgid, state) from /proc/<pid>/stat. The comm field can contain
/// spaces and parens, so parse from the last ')'.
pub fn parse_stat(stat: &str) -> Option<(u32, u32, char)> {
    let rest = &stat[stat.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    Some((ppid, pgid, state))
}

/// Socket inodes held open by `pids`.
fn socket_inodes(pids: &[u32]) -> HashSet<u64> {
    let mut out = HashSet::new();
    for pid in pids {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in rd.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                let l = link.to_string_lossy();
                if let Some(n) = l.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                    if let Ok(i) = n.parse() {
                        out.insert(i);
                    }
                }
            }
        }
    }
    out
}

/// TCP sockets in LISTEN state owned by any process in the tree rooted at `root`.
/// Reads /proc/<root>/net/* so the browser's own network namespace is used.
pub fn tcp_listeners(root: u32) -> Option<Vec<SocketAddr>> {
    let pids = process_tree(root)?;
    let inodes = socket_inodes(&pids);
    let mut out = Vec::new();
    for (file, v6) in [("tcp", false), ("tcp6", true)] {
        // A missing tcp6 table just means no IPv6 in that namespace.
        let path = format!("/proc/{root}/net/{file}");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => match std::fs::read_to_string(format!("/proc/net/{file}")) {
                Ok(t) => t,
                Err(_) => continue,
            },
        };
        out.extend(parse_listeners(&text, v6, &inodes));
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// Parse a /proc/net/tcp{,6} table, keeping LISTEN rows whose inode is in `inodes`.
pub fn parse_listeners(table: &str, v6: bool, inodes: &HashSet<u64>) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for line in table.lines().skip(1) {
        let c: Vec<&str> = line.split_whitespace().collect();
        if c.len() < 10 || c[3] != "0A" {
            continue;
        }
        let Ok(inode) = c[9].parse::<u64>() else { continue };
        if !inodes.contains(&inode) {
            continue;
        }
        if let Some(addr) = parse_addr(c[1], v6) {
            out.push(addr);
        }
    }
    out
}

fn parse_addr(s: &str, v6: bool) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = if v6 {
        let mut b = [0u8; 16];
        for (i, chunk) in b.chunks_mut(4).enumerate() {
            let w = u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok()?;
            chunk.copy_from_slice(&w.to_le_bytes());
        }
        IpAddr::V6(Ipv6Addr::from(b))
    } else {
        IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(ip, 16).ok()?.to_le_bytes()))
    };
    Some(SocketAddr::new(ip, port))
}

/// The command line of a process, split on NULs.
pub fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_weird_comm() {
        let s = "1234 (chrome (renderer) x) S 1000 1000 1000 0 -1 4194560 0";
        assert_eq!(parse_stat(s), Some((1000, 1000, 'S')));
    }

    #[test]
    fn parses_listen_rows() {
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:A1B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0\n\
   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 9999 1 0\n\
   2: 0100007F:1F91 0100007F:C000 01 00000000:00000000 00:00000000 00000000     0        0 4243 1 0\n";
        let inodes: HashSet<u64> = [4242, 4243].into_iter().collect();
        let l = parse_listeners(t, false, &inodes);
        assert_eq!(l, vec!["127.0.0.1:41394".parse().unwrap()], "only LISTEN rows we own");
    }

    #[test]
    fn parses_v6_loopback() {
        let a = parse_addr("00000000000000000000000001000000:1F90", true).unwrap();
        assert_eq!(a, "[::1]:8080".parse().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let found = tcp_listeners(std::process::id()).unwrap();
        assert!(found.iter().any(|a| a.port() == port), "scanner must see a known listener: {found:?}");
        drop(l);
        let found = tcp_listeners(std::process::id()).unwrap();
        assert!(!found.iter().any(|a| a.port() == port));
    }
}
