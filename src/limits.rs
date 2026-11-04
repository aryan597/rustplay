//! Per-session resource limits.
//!
//! | Limit  | Mechanism                                    | Platforms |
//! |--------|----------------------------------------------|-----------|
//! | wall   | OS-thread watchdog, SIGKILLs the process group | all unix |
//! | cpu    | cgroup v2 `cpu.stat` or v1 `cpuacct.usage`, polled by the watchdog; total CPU of the whole browser tree | Linux |
//! | memory | cgroup v2 `memory.max` (+`memory.oom.group`), else v1 `memory.limit_in_bytes`; kernel enforces | Linux |
//!
//! Nothing is silently ignored: if a requested limit cannot be enforced the
//! session fails to start with `LIMIT_UNSUPPORTED` and the reason.
//!
//! The browser joins its cgroups in the forked child *before exec*
//! (`attach_fds` + [`write_pid_to`]), so every helper process Chromium spawns
//! is accounted and capped from its first instruction.

use crate::errors::{Result, RpError};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

impl ResourceLimits {
    pub fn is_empty(&self) -> bool {
        self.wall_ms.is_none() && self.cpu_ms.is_none() && self.memory_bytes.is_none()
    }
    pub fn needs_cgroup(&self) -> bool {
        self.cpu_ms.is_some() || self.memory_bytes.is_some()
    }
    /// Combine operator and task limits: a task can add or lower a limit, never raise or remove one.
    pub fn tightened(&self, task: &ResourceLimits) -> ResourceLimits {
        ResourceLimits {
            wall_ms: min_opt(self.wall_ms, task.wall_ms),
            cpu_ms: min_opt(self.cpu_ms, task.cpu_ms),
            memory_bytes: min_opt(self.memory_bytes, task.memory_bytes),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.wall_ms == Some(0) || self.cpu_ms == Some(0) {
            return Err(RpError::config("wall and cpu limits must be > 0 ms"));
        }
        if let Some(m) = self.memory_bytes {
            if m < 64 * 1024 * 1024 {
                return Err(RpError::config("memory limit below 64 MiB cannot start Chromium"));
            }
        }
        Ok(())
    }
}

/// Parse `512M`, `1G`, `1.5GiB`, `268435456`.
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        u => return Err(format!("unknown size unit `{u}` (use K, M, G)")),
    };
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("size must be positive: `{s}`"));
    }
    Ok((n * mult as f64) as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Wall,
    Cpu,
    Memory,
}

impl LimitKind {
    pub fn name(self) -> &'static str {
        match self {
            LimitKind::Wall => "wall",
            LimitKind::Cpu => "cpu",
            LimitKind::Memory => "memory",
        }
    }
}

/// A limit that fired. `limit`/`observed` are ms for wall/cpu, bytes for memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitTrip {
    pub kind: LimitKind,
    pub limit: u64,
    pub observed: u64,
}

impl From<LimitTrip> for RpError {
    fn from(t: LimitTrip) -> Self {
        RpError::LimitExceeded { kind: t.kind, limit: t.limit, observed: t.observed }
    }
}

/// How each requested limit is enforced; shown in the task report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Enforcement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<&'static str>,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Where this process sits in each relevant cgroup hierarchy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupHost {
    /// Our own cgroup v2 directory (mount point + path from /proc/self/cgroup).
    pub v2_dir: Option<PathBuf>,
    /// Our own directory in the v1 `memory` hierarchy.
    pub v1_memory_dir: Option<PathBuf>,
    /// Our own directory in the v1 `cpuacct` hierarchy.
    pub v1_cpuacct_dir: Option<PathBuf>,
}

impl CgroupHost {
    pub fn discover() -> CgroupHost {
        if !cfg!(target_os = "linux") {
            return CgroupHost::default();
        }
        let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let cg = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        Self::from_proc(&mi, &cg, Path::new("/"))
    }

    /// Pure parser over /proc/self/mountinfo and /proc/self/cgroup. `root`
    /// prefixes every mount point (tests use a temp dir as a fake filesystem).
    pub fn from_proc(mountinfo: &str, self_cgroup: &str, root: &Path) -> CgroupHost {
        let mut v2_mount = None;
        let mut v1_mounts: Vec<(Vec<String>, String)> = Vec::new();
        for line in mountinfo.lines() {
            let Some((pre, post)) = line.split_once(" - ") else { continue };
            let pre: Vec<&str> = pre.split_whitespace().collect();
            let post: Vec<&str> = post.split_whitespace().collect();
            if pre.len() < 5 || post.len() < 3 {
                continue;
            }
            let mnt = pre[4].replace("\\040", " ");
            match post[0] {
                "cgroup2" => {
                    v2_mount.get_or_insert(mnt);
                }
                "cgroup" => v1_mounts.push((post[2].split(',').map(String::from).collect(), mnt)),
                _ => {}
            }
        }
        let mut host = CgroupHost::default();
        let join = |mnt: &str, rel: &str| root.join(mnt.trim_start_matches('/')).join(rel.trim_start_matches('/'));
        for line in self_cgroup.lines() {
            let mut f = line.splitn(3, ':');
            let (Some(id), Some(ctrls), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
            if id == "0" && ctrls.is_empty() {
                if let Some(m) = &v2_mount {
                    // After self-relocation we live in <scope>/rustplay-supervisor;
                    // session cgroups still belong next to it, under <scope>.
                    let path = path.strip_suffix(&format!("/{SUPERVISOR_CGROUP}")).unwrap_or(path);
                    host.v2_dir = Some(join(m, path));
                }
                continue;
            }
            let ctrls: Vec<&str> = ctrls.split(',').collect();
            for (opts, mnt) in &v1_mounts {
                let has = |c: &str| ctrls.contains(&c) && opts.iter().any(|o| o == c);
                if has("memory") && host.v1_memory_dir.is_none() {
                    host.v1_memory_dir = Some(join(mnt, path));
                }
                if has("cpuacct") && host.v1_cpuacct_dir.is_none() {
                    host.v1_cpuacct_dir = Some(join(mnt, path));
                }
            }
        }
        host
    }
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn write_file(p: &Path, v: &str) -> std::io::Result<()> {
    std::fs::write(p, v)
}

/// `key value` lines (cpu.stat, memory.events, memory.oom_control).
pub fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next()? == key).then(|| f.next()?.parse().ok()).flatten()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemMode {
    V2,
    V1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuMode {
    V2,
    V1,
}

/// The cgroups created for one session. Removed by `remove()`.
#[derive(Debug)]
pub struct SessionCgroups {
    v2: Option<PathBuf>,
    v1_mem: Option<PathBuf>,
    v1_cpu: Option<PathBuf>,
    mem: Option<MemMode>,
    cpu: Option<CpuMode>,
    procs: Mutex<Vec<File>>,
}

fn unsupported(limit: &str, reasons: Vec<String>) -> RpError {
    RpError::LimitUnsupported { limit: limit.into(), reason: reasons.join("; ") }
}

/// Can we use v2 `memory` in children of `parent`? Tries to enable it in
/// `cgroup.subtree_control`, which fails if `parent` holds processes
/// (cgroup v2's no-internal-processes rule) or is not delegated to us.
fn v2_memory_usable(parent: &Path) -> std::result::Result<(), String> {
    enable_controller(parent, "memory", std::process::id())
}

/// Leaf cgroup rustplay moves itself into, so its own cgroup can delegate controllers.
pub const SUPERVISOR_CGROUP: &str = "rustplay-supervisor";

const SCOPE_HINT: &str = "start the rustplay binary as the only process of a delegated cgroup, e.g. \
     `systemd-run --user --scope -p Delegate=yes ./target/release/rustplay ...` \
     (not via `cargo run`, which would share the scope)";

fn comm(pid: u32) -> String {
    read_trim(Path::new(&format!("/proc/{pid}/comm"))).unwrap_or_else(|| "?".into())
}

/// Enable `ctrl` in `parent/cgroup.subtree_control`.
///
/// cgroup v2's no-internal-processes rule makes that write fail with EBUSY
/// while `parent` itself holds processes. If the only process there is
/// `self_pid` (rustplay started in its own scope), move it into the leaf
/// `parent/rustplay-supervisor` and retry. Other processes are never moved:
/// if any are present the error names them.
pub fn enable_controller(parent: &Path, ctrl: &str, self_pid: u32) -> std::result::Result<(), String> {
    let ctrls = read_trim(&parent.join("cgroup.controllers"))
        .ok_or_else(|| format!("{} is not a cgroup v2 directory", parent.display()))?;
    if !ctrls.split_whitespace().any(|c| c == ctrl) {
        return Err(format!(
            "cgroup v2 {ctrl} controller not available in {} (controllers: `{ctrls}`; on hybrid hosts it may be bound to cgroup v1)",
            parent.display()
        ));
    }
    let sub_path = parent.join("cgroup.subtree_control");
    if read_trim(&sub_path).unwrap_or_default().split_whitespace().any(|c| c == ctrl) {
        return Ok(());
    }
    let first = match write_file(&sub_path, &format!("+{ctrl}")) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if first.raw_os_error() != Some(libc::EBUSY) {
        return Err(format!(
            "cannot enable {ctrl} in {} ({first}); the cgroup is not delegated to this user; {SCOPE_HINT}",
            sub_path.display()
        ));
    }
    let procs: Vec<u32> = std::fs::read_to_string(parent.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    let others: Vec<String> = procs.iter().filter(|p| **p != self_pid).map(|p| format!("{}({p})", comm(*p))).collect();
    if !others.is_empty() {
        return Err(format!(
            "{} also holds other processes [{}], so cgroup v2 will not enable {ctrl} there (EBUSY); {SCOPE_HINT}",
            parent.display(),
            others.join(", ")
        ));
    }
    if !procs.contains(&self_pid) {
        return Err(format!("cannot enable {ctrl} in {} ({first})", sub_path.display()));
    }
    let leaf = parent.join(SUPERVISOR_CGROUP);
    match std::fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("mkdir {}: {e}", leaf.display())),
    }
    write_file(&leaf.join("cgroup.procs"), &self_pid.to_string())
        .map_err(|e| format!("move rustplay into {}: {e}", leaf.display()))?;
    tracing::info!(leaf = %leaf.display(), "moved rustplay into a leaf cgroup to delegate {ctrl}");
    write_file(&sub_path, &format!("+{ctrl}"))
        .map_err(|e| format!("cannot enable {ctrl} in {} after moving rustplay to a leaf ({e})", sub_path.display()))
}

impl SessionCgroups {
    /// Create cgroups for `limits` or explain exactly why not.
    pub fn create(
        host: &CgroupHost,
        session_id: &str,
        limits: &ResourceLimits,
        v2_parent: Option<&Path>,
    ) -> Result<Option<(SessionCgroups, Enforcement)>> {
        if !limits.needs_cgroup() {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") {
            let which = if limits.memory_bytes.is_some() { "memory" } else { "cpu" };
            return Err(unsupported(which, vec!["cpu and memory limits need Linux cgroups".into()]));
        }
        let name = format!("rustplay-{session_id}");
        let v2_parent: Option<PathBuf> = v2_parent.map(Path::to_path_buf).or_else(|| host.v2_dir.clone());

        // Choose a memory mechanism.
        let mut reasons = Vec::new();
        let mem = match limits.memory_bytes {
            None => None,
            Some(_) => {
                let v2 = match &v2_parent {
                    Some(p) => v2_memory_usable(p).map_err(|e| reasons.push(e)).is_ok(),
                    None => {
                        reasons.push("no cgroup v2 mount".into());
                        false
                    }
                };
                if v2 {
                    Some(MemMode::V2)
                } else if host.v1_memory_dir.as_ref().is_some_and(|d| d.join("memory.limit_in_bytes").exists()) {
                    Some(MemMode::V1)
                } else {
                    reasons.push("no writable cgroup v1 memory hierarchy".into());
                    return Err(unsupported("memory", reasons));
                }
            }
        };
        // Choose a CPU accounting mechanism. v2 cpu.stat usage_usec exists
        // without enabling the cpu controller.
        let cpu = match limits.cpu_ms {
            None => None,
            Some(_) => {
                if mem == Some(MemMode::V2) || v2_parent.as_ref().is_some_and(|p| p.join("cgroup.procs").exists()) {
                    Some(CpuMode::V2)
                } else if host.v1_cpuacct_dir.as_ref().is_some_and(|d| d.join("cpuacct.usage").exists()) {
                    Some(CpuMode::V1)
                } else {
                    return Err(unsupported(
                        "cpu",
                        vec!["no cgroup v2 mount and no cgroup v1 cpuacct hierarchy".into()],
                    ));
                }
            }
        };

        let mut cg = SessionCgroups { v2: None, v1_mem: None, v1_cpu: None, mem, cpu, procs: Mutex::new(vec![]) };
        let res = (|| -> std::result::Result<(), String> {
            let mk = |dir: PathBuf| -> std::result::Result<PathBuf, String> {
                std::fs::create_dir(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
                Ok(dir)
            };
            if mem == Some(MemMode::V2) || cpu == Some(CpuMode::V2) {
                cg.v2 = Some(mk(v2_parent.as_ref().expect("checked").join(&name))?);
            }
            if mem == Some(MemMode::V1) {
                cg.v1_mem = Some(mk(host.v1_memory_dir.as_ref().expect("checked").join(&name))?);
            }
            if cpu == Some(CpuMode::V1) {
                cg.v1_cpu = Some(mk(host.v1_cpuacct_dir.as_ref().expect("checked").join(&name))?);
            }
            if let (Some(bytes), Some(m)) = (limits.memory_bytes, mem) {
                match m {
                    MemMode::V2 => {
                        let d = cg.v2.as_ref().expect("v2 dir");
                        write_file(&d.join("memory.max"), &bytes.to_string())
                            .map_err(|e| format!("memory.max: {e}"))?;
                        // Do not let the cap be bypassed by swapping; kill the whole tree on OOM.
                        let _ = write_file(&d.join("memory.swap.max"), "0");
                        let _ = write_file(&d.join("memory.oom.group"), "1");
                    }
                    MemMode::V1 => {
                        let d = cg.v1_mem.as_ref().expect("v1 dir");
                        write_file(&d.join("memory.limit_in_bytes"), &bytes.to_string())
                            .map_err(|e| format!("memory.limit_in_bytes: {e}"))?;
                        let _ = write_file(&d.join("memory.memsw.limit_in_bytes"), &bytes.to_string());
                    }
                }
            }
            for d in cg.dirs().into_iter().cloned().collect::<Vec<_>>() {
                let f = OpenOptions::new()
                    .write(true)
                    .open(d.join("cgroup.procs"))
                    .map_err(|e| format!("open {}/cgroup.procs: {e}", d.display()))?;
                cg.procs.get_mut().unwrap_or_else(|p| p.into_inner()).push(f);
            }
            Ok(())
        })();
        if let Err(e) = res {
            let which = if limits.memory_bytes.is_some() { "memory" } else { "cpu" };
            let _ = cg.remove(Duration::from_millis(200));
            return Err(unsupported(which, vec![e]));
        }

        let enforcement = Enforcement {
            wall: None,
            cpu: cpu.map(|m| if m == CpuMode::V2 { "cgroup2 cpu.stat" } else { "cgroup1 cpuacct" }),
            memory: mem.map(|m| if m == MemMode::V2 { "cgroup2 memory.max" } else { "cgroup1 memory.limit_in_bytes" }),
        };
        Ok(Some((cg, enforcement)))
    }

    fn dirs(&self) -> Vec<&PathBuf> {
        [&self.v2, &self.v1_mem, &self.v1_cpu].into_iter().flatten().collect()
    }

    /// Open `cgroup.procs` fds the child writes its pid into before exec.
    pub fn attach_fds(&self) -> Vec<RawFd> {
        self.procs.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|f| f.as_raw_fd()).collect()
    }

    /// Close the `cgroup.procs` fds once the child has been spawned.
    pub fn release_attach_fds(&self) {
        self.procs.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn cpu_used_ms(&self) -> Option<u64> {
        match self.cpu? {
            CpuMode::V2 => {
                kv(&std::fs::read_to_string(self.v2.as_ref()?.join("cpu.stat")).ok()?, "usage_usec").map(|u| u / 1000)
            }
            CpuMode::V1 => {
                read_trim(&self.v1_cpu.as_ref()?.join("cpuacct.usage"))?.parse::<u64>().ok().map(|n| n / 1_000_000)
            }
        }
    }

    /// Number of OOM kills inside the session cgroup so far.
    pub fn oom_kills(&self) -> u64 {
        match self.mem {
            Some(MemMode::V2) => self
                .v2
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join("memory.events")).ok())
                .and_then(|t| kv(&t, "oom_kill"))
                .unwrap_or(0),
            Some(MemMode::V1) => self
                .v1_mem
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join("memory.oom_control")).ok())
                .and_then(|t| kv(&t, "oom_kill"))
                .unwrap_or(0),
            None => 0,
        }
    }

    pub fn peak_memory_bytes(&self) -> Option<u64> {
        match self.mem? {
            MemMode::V2 => read_trim(&self.v2.as_ref()?.join("memory.peak"))?.parse().ok(),
            MemMode::V1 => read_trim(&self.v1_mem.as_ref()?.join("memory.max_usage_in_bytes"))?.parse().ok(),
        }
    }

    /// SIGKILL everything in the session cgroups.
    pub fn kill_all(&self) {
        if let Some(d) = &self.v2 {
            let _ = write_file(&d.join("cgroup.kill"), "1");
        }
        for d in self.dirs() {
            if let Ok(t) = std::fs::read_to_string(d.join("cgroup.procs")) {
                for pid in t.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
                    // SAFETY: plain syscall on pids currently listed in our own cgroup.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }

    /// Kill stragglers and rmdir the cgroups (they must be empty).
    pub fn remove(&self, wait: Duration) -> std::result::Result<(), String> {
        self.release_attach_fds();
        let deadline = Instant::now() + wait;
        let mut last = String::new();
        for d in self.dirs().into_iter().cloned().collect::<Vec<_>>() {
            loop {
                match std::fs::remove_dir(&d) {
                    Ok(()) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                    Err(e) => {
                        if Instant::now() >= deadline {
                            last = format!("rmdir {}: {e}", d.display());
                            break;
                        }
                        self.kill_all();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        if last.is_empty() {
            Ok(())
        } else {
            Err(last)
        }
    }
}

/// Async-signal-safe: write the calling process's pid into each fd.
///
/// # Safety
/// Call only in the forked child before exec.
pub unsafe fn write_pid_to(fds: &[RawFd]) -> std::io::Result<()> {
    let mut n = libc::getpid() as u64;
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for fd in fds {
        if libc::write(*fd, buf[i..].as_ptr().cast(), buf.len() - i) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Watchdog
// ---------------------------------------------------------------------------

/// OS thread (not a tokio task) so limits fire even if the runtime is wedged.
pub struct Watchdog {
    stop: Arc<AtomicBool>,
    trip: Arc<Mutex<Option<LimitTrip>>>,
    notify: tokio::sync::watch::Receiver<Option<LimitTrip>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    pub fn start(
        pgid: u32,
        limits: ResourceLimits,
        cgroups: Option<Arc<SessionCgroups>>,
        started: Instant,
    ) -> Watchdog {
        let stop = Arc::new(AtomicBool::new(false));
        let trip = Arc::new(Mutex::new(None));
        let (s, t) = (stop.clone(), trip.clone());
        // Sync send from an OS thread; async waiters (the task runner) wake immediately.
        let (notify_tx, notify) = tokio::sync::watch::channel(None);
        let handle = std::thread::Builder::new()
            .name("rustplay-watchdog".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    let mut fired = None;
                    if let Some(w) = limits.wall_ms {
                        let el = started.elapsed().as_millis() as u64;
                        if el >= w {
                            fired = Some(LimitTrip { kind: LimitKind::Wall, limit: w, observed: el });
                        }
                    }
                    if let (None, Some(c), Some(cg)) = (fired, limits.cpu_ms, &cgroups) {
                        if let Some(used) = cg.cpu_used_ms() {
                            if used >= c {
                                fired = Some(LimitTrip { kind: LimitKind::Cpu, limit: c, observed: used });
                            }
                        }
                    }
                    if let (None, Some(m), Some(cg)) = (fired, limits.memory_bytes, &cgroups) {
                        if cg.oom_kills() > 0 {
                            let peak = cg.peak_memory_bytes().unwrap_or(m);
                            fired = Some(LimitTrip { kind: LimitKind::Memory, limit: m, observed: peak });
                        }
                    }
                    if let Some(f) = fired {
                        tracing::warn!(
                            kind = f.kind.name(),
                            limit = f.limit,
                            observed = f.observed,
                            "resource limit exceeded; killing browser"
                        );
                        *t.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
                        let _ = notify_tx.send(Some(f));
                        if let Some(cg) = &cgroups {
                            cg.kill_all();
                        }
                        // SAFETY: the session stops this thread before the leader is reaped.
                        unsafe {
                            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
                        }
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .expect("spawn watchdog thread");
        Watchdog { stop, trip, notify, handle: Some(handle) }
    }

    /// Resolves when a limit fires. Lets callers race work that never touches
    /// the browser (e.g. `sleep`) against the watchdog.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<LimitTrip>> {
        self.notify.clone()
    }

    pub fn trip(&self) -> Option<LimitTrip> {
        *self.trip.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stop and join. Must run before the browser process is reaped.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYBRID_MOUNTINFO: &str = "\
30 25 0:26 / /sys/fs/cgroup ro,nosuid - tmpfs tmpfs ro,mode=755
31 30 0:27 / /sys/fs/cgroup/unified rw,nosuid - cgroup2 cgroup2 rw
32 30 0:28 / /sys/fs/cgroup/memory rw,nosuid - cgroup cgroup rw,memory
33 30 0:29 / /sys/fs/cgroup/cpu,cpuacct rw,nosuid - cgroup cgroup rw,cpu,cpuacct
";
    const HYBRID_SELF: &str = "\
12:memory:/sess/a
4:cpu,cpuacct:/
0::/user.slice/x.scope
";

    #[test]
    fn discovers_hybrid_host() {
        let h = CgroupHost::from_proc(HYBRID_MOUNTINFO, HYBRID_SELF, Path::new("/"));
        assert_eq!(h.v2_dir.unwrap(), Path::new("/sys/fs/cgroup/unified/user.slice/x.scope"));
        assert_eq!(h.v1_memory_dir.unwrap(), Path::new("/sys/fs/cgroup/memory/sess/a"));
        assert_eq!(h.v1_cpuacct_dir.unwrap(), Path::new("/sys/fs/cgroup/cpu,cpuacct"));
    }

    #[test]
    fn supervisor_leaf_maps_back_to_its_scope() {
        let mi = "25 1 0:22 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";
        let h = CgroupHost::from_proc(mi, "0::/user.slice/run-r1.scope/rustplay-supervisor\n", Path::new("/"));
        assert_eq!(h.v2_dir.unwrap(), Path::new("/sys/fs/cgroup/user.slice/run-r1.scope"));
    }

    #[test]
    fn discovers_pure_v2_and_none() {
        let mi = "25 1 0:22 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw,nsdelegate\n";
        let h = CgroupHost::from_proc(mi, "0::/user.slice/u.service\n", Path::new("/"));
        assert_eq!(h.v2_dir.unwrap(), Path::new("/sys/fs/cgroup/user.slice/u.service"));
        assert!(h.v1_memory_dir.is_none());
        assert_eq!(CgroupHost::from_proc("", "", Path::new("/")), CgroupHost::default());
    }

    fn fake_v2(root: &Path, controllers: &str) -> PathBuf {
        let p = root.join("parent");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cgroup.controllers"), controllers).unwrap();
        std::fs::write(p.join("cgroup.subtree_control"), "").unwrap();
        std::fs::write(p.join("cgroup.procs"), "").unwrap();
        p
    }

    /// cgroupfs creates control files on mkdir; a plain fs does not, so pre-create them.
    fn precreate(dir: &Path, files: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), "").unwrap();
        }
    }

    /// A plain filesystem cannot emulate cgroupfs creating control files on
    /// mkdir, so this covers selection and fail-closed cleanup; the success
    /// path runs against the real kernel in tests/limits.rs.
    #[test]
    fn v2_memory_selected_and_cleaned_up_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = fake_v2(tmp.path(), "cpu memory pids");
        let host = CgroupHost { v2_dir: Some(parent.clone()), ..Default::default() };
        let lim = ResourceLimits { memory_bytes: Some(512 << 20), ..Default::default() };
        let e = SessionCgroups::create(&host, "t1", &lim, None).unwrap_err();
        assert_eq!(e.code(), "LIMIT_UNSUPPORTED", "no cgroup.procs in the child -> fail closed");
        assert!(e.to_string().contains("cgroup.procs"), "{e}");
        // On a plain fs the written memory.max blocks rmdir; cgroupfs would remove it
        // (tests/limits.rs checks real removal). Here we check what was written.
        assert_eq!(std::fs::read_to_string(parent.join("rustplay-t1/memory.max")).unwrap(), (512u64 << 20).to_string());
        assert_eq!(std::fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap(), "+memory");
    }

    #[test]
    fn explicit_v2_parent_overrides_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let good = fake_v2(&tmp.path().join("a"), "memory");
        let bad = fake_v2(&tmp.path().join("b"), "pids");
        let host = CgroupHost { v2_dir: Some(bad), ..Default::default() };
        let lim = ResourceLimits { memory_bytes: Some(512 << 20), ..Default::default() };
        let e = SessionCgroups::create(&host, "o", &lim, Some(&good)).unwrap_err();
        // Got past controller selection (would say "not available" otherwise).
        assert!(!e.to_string().contains("not available"), "{e}");
    }

    #[test]
    fn v2_without_memory_controller_falls_back_or_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = fake_v2(tmp.path(), "cpu pids");
        let lim = ResourceLimits { memory_bytes: Some(512 << 20), ..Default::default() };
        // No v1 either -> clear error naming both reasons.
        let host = CgroupHost { v2_dir: Some(parent.clone()), ..Default::default() };
        let e = SessionCgroups::create(&host, "x", &lim, None).unwrap_err();
        assert_eq!(e.code(), "LIMIT_UNSUPPORTED");
        let msg = e.to_string();
        assert!(msg.contains("memory controller not available") && msg.contains("v1"), "{msg}");

        // v1 memory present -> chosen.
        let v1 = tmp.path().join("v1mem");
        precreate(&v1, &["memory.limit_in_bytes"]);
        let host = CgroupHost { v2_dir: Some(parent), v1_memory_dir: Some(v1.clone()), ..Default::default() };
        let r = SessionCgroups::create(&host, "y", &lim, None);
        // cgroup.procs is not auto-created on a plain fs -> fail closed, but v1 was
        // chosen and the limit landed in the v1 file.
        assert_eq!(r.unwrap_err().code(), "LIMIT_UNSUPPORTED");
        assert_eq!(
            std::fs::read_to_string(v1.join("rustplay-y/memory.limit_in_bytes")).unwrap(),
            (512u64 << 20).to_string()
        );
    }

    #[test]
    fn nothing_requested_needs_no_cgroup() {
        let lim = ResourceLimits { wall_ms: Some(1000), ..Default::default() };
        assert!(SessionCgroups::create(&CgroupHost::default(), "z", &lim, None).unwrap().is_none());
    }

    #[test]
    fn cpu_without_any_hierarchy_is_unsupported() {
        let lim = ResourceLimits { cpu_ms: Some(1000), ..Default::default() };
        let e = SessionCgroups::create(&CgroupHost::default(), "z", &lim, None).unwrap_err();
        assert_eq!(e.code(), "LIMIT_UNSUPPORTED");
    }

    #[test]
    fn parses_kv_files() {
        assert_eq!(kv("usage_usec 123456\nuser_usec 1\n", "usage_usec"), Some(123456));
        assert_eq!(kv("low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n", "oom_kill"), Some(1));
        assert_eq!(kv("oom_kill_disable 0\nunder_oom 0\noom_kill 2\n", "oom_kill"), Some(2));
        assert_eq!(kv("", "x"), None);
    }

    #[test]
    fn sizes_and_tightening() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 << 29);
        assert_eq!(parse_size("1048576").unwrap(), 1 << 20);
        assert!(parse_size("12X").is_err() && parse_size("-1M").is_err() && parse_size("").is_err());
        let op = ResourceLimits { wall_ms: Some(10_000), cpu_ms: None, memory_bytes: Some(1 << 30) };
        let task = ResourceLimits { wall_ms: Some(60_000), cpu_ms: Some(500), memory_bytes: None };
        assert_eq!(
            op.tightened(&task),
            ResourceLimits { wall_ms: Some(10_000), cpu_ms: Some(500), memory_bytes: Some(1 << 30) }
        );
        assert!(ResourceLimits { memory_bytes: Some(1 << 20), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn wall_watchdog_fires_without_any_runtime() {
        // A process group we own: a sleeping child in its own group.
        let mut child = {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap()
        };
        let lim = ResourceLimits { wall_ms: Some(150), ..Default::default() };
        let mut w = Watchdog::start(child.id(), lim, None, Instant::now());
        let status = child.wait().unwrap();
        let trip = w.trip().expect("watchdog should have fired");
        assert_eq!(trip.kind, LimitKind::Wall);
        assert!(trip.observed >= 150);
        assert!(!status.success());
        w.stop();
        let e: RpError = trip.into();
        assert_eq!(e.code(), "WALL_LIMIT_EXCEEDED");
    }
}
