//! Per-session resource limits against the real kernel and a real browser.
//! Where the host cannot enforce a limit, the test asserts the clear
//! LIMIT_UNSUPPORTED error instead; it never passes by ignoring the limit.

mod common;
use common::*;
use rustplay::access::Role;
use rustplay::limits::{CgroupHost, ResourceLimits};
use rustplay::{run_task, SessionConfig, Task, TaskReport};
use std::path::PathBuf;
use std::time::{Duration, Instant};

macro_rules! need_browser {
    () => {
        if !browser_available() {
            return;
        }
    };
}

fn limited(out: &std::path::Path, root: &std::path::Path, limits: ResourceLimits) -> SessionConfig {
    let mut c = cfg(out);
    c.temp_root = Some(root.to_path_buf());
    c.role = Role::Admin;
    c.limits = limits;
    c.timeouts.action_ms = 30_000;
    c.timeouts.task_ms = 60_000;
    c
}

fn task(base: &str, steps: &str) -> Task {
    Task::from_json(&format!(r#"{{"steps":[{{"action":"goto","url":"{base}/"}},{steps}]}}"#)).unwrap()
}

/// Every cgroup dir a session could have created.
fn session_cgroup_dirs(session_id: &str) -> Vec<PathBuf> {
    let h = CgroupHost::discover();
    let name = format!("rustplay-{session_id}");
    [h.v2_dir, h.v1_memory_dir, h.v1_cpuacct_dir].into_iter().flatten().map(|d| d.join(&name)).collect()
}

/// Either the limit was enforced (Some(report)) or the host said, clearly, that it cannot.
fn enforced_or_unsupported(r: TaskReport, limit: &str) -> Option<TaskReport> {
    if r.error.as_ref().map(|e| e.code) == Some("LIMIT_UNSUPPORTED") {
        let e = r.error.unwrap();
        assert!(e.message.contains(limit), "{}", e.message);
        assert!(e.hint.is_some());
        assert!(r.session_id.is_none(), "must fail before any browser starts");
        eprintln!("NOTE: {limit} limit unsupported on this host: {}", e.message);
        return None;
    }
    Some(r)
}

async fn assert_cleaned(root: &std::path::Path, session_id: Option<&String>) {
    let needle = root.display().to_string();
    for _ in 0..40 {
        let dirs_left =
            session_id.map(|id| session_cgroup_dirs(id).into_iter().filter(|d| d.exists()).count()).unwrap_or(0);
        if processes_mentioning(&needle).is_empty() && dirs_left == 0 && std::fs::read_dir(root).unwrap().count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "leftovers: pids={:?} cgroups={:?}",
        processes_mentioning(&needle),
        session_id.map(|id| session_cgroup_dirs(id).into_iter().filter(|d| d.exists()).collect::<Vec<_>>())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_limit_kills_the_browser() {
    need_browser!();
    let srv = fixture().await;
    let (out, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let c = limited(out.path(), root.path(), ResourceLimits { wall_ms: Some(2_000), ..Default::default() });
    let t0 = Instant::now();
    let r = run_task(&task(&srv.base_url(), r#"{"action":"sleep","ms":20000}"#), &c).await;
    let e = r.error.clone().expect("must fail");
    assert_eq!(e.code, "WALL_LIMIT_EXCEEDED", "{e:?}");
    assert!(t0.elapsed() < Duration::from_secs(6), "took {:?}", t0.elapsed());
    assert_eq!(r.resources.as_ref().unwrap().enforcement.wall, Some("watchdog SIGKILL of the process group"));
    assert_cleaned(root.path(), r.session_id.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_limit_kills_a_busy_page() {
    need_browser!();
    let srv = fixture().await;
    let (out, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let c = limited(out.path(), root.path(), ResourceLimits { cpu_ms: Some(4_000), ..Default::default() });
    let busy =
        r#"{"action":"eval","expression":"(() => { const end = Date.now() + 60000; while (Date.now() < end) {} })()"}"#;
    let t0 = Instant::now();
    let Some(r) = enforced_or_unsupported(run_task(&task(&srv.base_url(), busy), &c).await, "cpu") else { return };
    let e = r.error.clone().expect("must fail");
    assert_eq!(e.code, "CPU_LIMIT_EXCEEDED", "{e:?}");
    assert!(e.message.contains("limit 4000 ms"), "{}", e.message);
    assert!(t0.elapsed() < Duration::from_secs(20), "should die near 4 s of CPU, took {:?}", t0.elapsed());
    let res = r.resources.as_ref().unwrap();
    assert!(res.enforcement.cpu.unwrap().starts_with("cgroup"), "{res:?}");
    assert_cleaned(root.path(), r.session_id.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit_ooms_a_heavy_page() {
    need_browser!();
    let srv = fixture().await;
    let (out, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let c = limited(out.path(), root.path(), ResourceLimits { memory_bytes: Some(700 << 20), ..Default::default() });
    // Touch 4 GiB, 64 MiB at a time, so it is really charged to the cgroup.
    let hog = r#"{"action":"eval","expression":"(async () => { const keep = []; for (let i = 0; i < 64; i++) { const a = new Uint8Array(64 << 20); a.fill(1); keep.push(a); await new Promise(r => setTimeout(r, 5)); } return keep.length; })()"}"#;
    let Some(r) = enforced_or_unsupported(run_task(&task(&srv.base_url(), hog), &c).await, "memory") else { return };
    let e = r.error.clone().expect("must fail");
    assert_eq!(e.code, "MEMORY_LIMIT_EXCEEDED", "{e:?}");
    assert!(e.message.contains("700 MiB"), "{}", e.message);
    let res = r.resources.as_ref().unwrap();
    assert!(res.enforcement.memory.unwrap().contains("memory"), "{res:?}");
    if let Some(peak) = res.peak_memory_bytes {
        assert!(peak <= (700 << 20) + (16 << 20), "peak {peak} above the cap");
    }
    assert_cleaned(root.path(), r.session_id.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn normal_task_under_limits_succeeds_and_reports_usage() {
    need_browser!();
    let srv = fixture().await;
    let (out, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let lim = ResourceLimits { wall_ms: Some(60_000), cpu_ms: Some(60_000), memory_bytes: Some(2 << 30) };
    let c = limited(out.path(), root.path(), lim);
    let t = task(
        &srv.base_url(),
        r##"{"action":"click","selector":"#late"},{"action":"assert_text","selector":"#late","equals":"Clicked"}"##,
    );
    let Some(r) = enforced_or_unsupported(run_task(&t, &c).await, "memory") else { return };
    assert!(r.ok, "{:?}", r.error);
    let res = r.resources.as_ref().expect("resources reported when limits are set");
    assert_eq!(res.limits, lim);
    assert!(res.cpu_used_ms.unwrap() > 0, "{res:?}");
    assert!(res.peak_memory_bytes.unwrap() > 20 << 20, "{res:?}");
    assert!(res.enforcement.wall.is_some() && res.enforcement.cpu.is_some() && res.enforcement.memory.is_some());
    assert_cleaned(root.path(), r.session_id.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_are_opt_in_and_task_can_only_tighten() {
    need_browser!();
    let srv = fixture().await;
    let (out, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // No limits: nothing reported, no cgroup created.
    let r = run_task(&task(&srv.base_url(), r##"{"action":"text","selector":"#title"}"##), &cfg(out.path())).await;
    assert!(r.ok && r.resources.is_none());
    // Operator wall 2 s; task asks for 60 s -> still killed at ~2 s.
    let c = limited(out.path(), root.path(), ResourceLimits { wall_ms: Some(2_000), ..Default::default() });
    let t = Task::from_json(&format!(
        r#"{{"options":{{"wall_limit_ms":60000}},"steps":[{{"action":"goto","url":"{}/"}},{{"action":"sleep","ms":20000}}]}}"#,
        srv.base_url()
    ))
    .unwrap();
    let r = run_task(&t, &c).await;
    assert_eq!(r.error.unwrap().code, "WALL_LIMIT_EXCEEDED");
    assert_eq!(r.resources.unwrap().limits.wall_ms, Some(2_000));
}

/// Real-kernel check of the cgroup v2 no-internal-processes rule and the
/// supervisor-leaf fix. Needs root and a cgroup2 mount with any delegated
/// controller (memory on pure-v2 hosts, hugetlb on this hybrid dev VM).
#[test]
fn v2_leaf_relocation_against_kernel() {
    use rustplay::limits::{enable_controller, SUPERVISOR_CGROUP};
    if !rustplay::sandbox::is_root() {
        eprintln!("SKIP: needs root to create top-level cgroups");
        return;
    }
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let Some(mount) =
        mi.lines().find(|l| l.contains(" - cgroup2 ")).map(|l| PathBuf::from(l.split_whitespace().nth(4).unwrap()))
    else {
        eprintln!("SKIP: no cgroup2 mount");
        return;
    };
    // The no-internal-processes rule only binds controllers that distribute a
    // domain resource. `cpu` and `pids` are exempt: the kernel enables them in
    // a populated cgroup without complaint (checked on 6.x and 7.0). Picking
    // whatever the host happens to list first therefore makes this test depend
    // on the host's delegation: adding `+cpu` at the root is enough to select
    // an exempt controller and make the EBUSY assertion below bogus.
    let delegated = std::fs::read_to_string(mount.join("cgroup.subtree_control")).unwrap_or_default();
    let available: Vec<&str> = delegated.split_whitespace().collect();
    let Some(ctrl) = ["memory", "hugetlb", "io"].into_iter().find(|c| available.contains(c)).map(String::from) else {
        eprintln!("SKIP: no enforcing controller delegated at {} (have: `{delegated}`)", mount.display());
        return;
    };
    // Cleanup runs even if an assertion panics, so failures leave no cgroups behind.
    struct Cleanup {
        kids: Vec<std::process::Child>,
        dirs: Vec<PathBuf>,
        ctrl: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for c in &mut self.kids {
                let _ = c.kill();
                let _ = c.wait();
            }
            for d in self.dirs.iter().rev() {
                let _ = std::fs::write(d.join("cgroup.subtree_control"), format!("-{}", self.ctrl));
                let _ = std::fs::remove_dir(d);
            }
        }
    }
    let mut g = Cleanup { kids: vec![], dirs: vec![], ctrl: ctrl.clone() };
    let spawn = |g: &mut Cleanup, cg: &std::path::Path| -> u32 {
        let c = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = c.id();
        g.kids.push(c);
        std::fs::write(cg.join("cgroup.procs"), pid.to_string()).unwrap();
        pid
    };
    let mk = |g: &mut Cleanup, name: &str| {
        let d = mount.join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir(&d).unwrap();
        g.dirs.push(d.clone());
        d
    };

    // A: the only process is "us" -> moved to the leaf, controller enabled.
    let a = mk(&mut g, "rp-relocate");
    g.dirs.push(a.join(SUPERVISOR_CGROUP)); // removed before `a` (reverse order)
    let me = spawn(&mut g, &a);
    match std::fs::write(a.join("cgroup.subtree_control"), format!("+{ctrl}")) {
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::EBUSY), "kernel should refuse with a process inside")
        }
        // Everything below tests the workaround for that refusal. If a kernel
        // stops refusing, the workaround is simply unreachable here: skip
        // rather than fail, but say so, because it means `enable_controller`
        // never relocates on this host.
        Ok(()) => {
            eprintln!("SKIP: this kernel enables {ctrl} in a populated cgroup; no relocation to exercise");
            return;
        }
    }
    enable_controller(&a, &ctrl, me).expect("relocation should make it work");
    let sub = std::fs::read_to_string(a.join("cgroup.subtree_control")).unwrap();
    assert!(sub.split_whitespace().any(|c| c == ctrl), "{sub}");
    let mine = std::fs::read_to_string(format!("/proc/{me}/cgroup")).unwrap();
    assert!(mine.trim_end().ends_with(&format!("/{SUPERVISOR_CGROUP}")), "{mine}");
    // Idempotent: a second session finds it already enabled.
    enable_controller(&a, &ctrl, me).unwrap();

    // B: another process shares the cgroup -> refuse, name it, move nothing.
    // Assert on the pid, not the process name: the kernel releases a vfork
    // parent before the child's comm changes to `sleep`, so the name can
    // briefly still be the test thread's.
    let b = mk(&mut g, "rp-shared");
    let me_b = spawn(&mut g, &b);
    let other = spawn(&mut g, &b);
    let e = enable_controller(&b, &ctrl, me_b).unwrap_err();
    assert!(e.contains("also holds other processes") && e.contains(&format!("({other})")), "{e}");
    assert!(!e.contains(&format!("({me_b})")), "must not list itself: {e}");
    assert!(e.contains("systemd-run --user --scope -p Delegate=yes"), "{e}");
    assert!(!b.join(SUPERVISOR_CGROUP).exists(), "must not move anything when refusing");
    drop(g);
    assert!(!a.exists() && !b.exists(), "cleanup must remove the test cgroups");
}
