//! Defense-in-depth around Chromium:
//! hardened flags, a scrubbed environment, URL policy, output-path confinement,
//! and optional bubblewrap isolation.

use crate::config::{CdpTransport, Isolation, SessionConfig};
use crate::errors::{Result, RpError};
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use url::Url;

/// Chromium flags applied to every session.
pub fn chrome_args(cfg: &SessionConfig, profile: &Path, cache: &Path) -> Vec<String> {
    let (w, h) = cfg.viewport;
    let mut a: Vec<String> = match cfg.cdp_transport {
        // fds 3/4 are installed by browser.rs before exec. No socket is created.
        CdpTransport::Pipe => vec!["--remote-debugging-pipe".to_string()],
        CdpTransport::Tcp => {
            vec!["--remote-debugging-port=0".to_string(), "--remote-debugging-address=127.0.0.1".to_string()]
        }
    };
    a.extend(
        [
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-component-extensions-with-background-pages",
        "--disable-extensions",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--disable-domain-reliability",
        "--disable-client-side-phishing-detection",
        "--disable-background-timer-throttling",
        "--disable-renderer-backgrounding",
        "--disable-backgrounding-occluded-windows",
        "--disable-dev-shm-usage",
        "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,CertificateTransparencyComponentUpdater,InterestFeedContentSuggestions,PrivacySandboxSettings4,LensOverlay",
        "--metrics-recording-only",
        "--no-pings",
        "--password-store=basic",
        "--use-mock-keychain",
        "--mute-audio",
        "--hide-scrollbars",
        "--deny-permission-prompts",
        "--disable-notifications",
        "--block-new-web-contents",
        "--disable-gpu",
        // Startup cost: no throwaway first tab (rustplay opens its own page over
        // CDP), no field-trial/experiment setup, no background service autorun.
        "--no-startup-window",
        "--disable-field-trial-config",
        "--no-service-autorun",
        "--disable-search-engine-choice-screen",
        "--disable-back-forward-cache",
    ]
        .iter()
        .map(|s| s.to_string()),
    );

    if cfg.headless {
        a.push("--headless=new".into());
    }
    if !cfg.chrome_sandbox {
        a.push("--no-sandbox".into());
    }
    a.push(format!("--user-data-dir={}", profile.display()));
    a.push(format!("--disk-cache-dir={}", cache.display()));
    a.push(format!("--window-size={w},{h}"));
    if let Some(rules) = host_resolver_rules(cfg.allowed_hosts.as_deref()) {
        a.push(format!("--host-resolver-rules={rules}"));
    }
    a
}

/// Invariant: every launch uses exactly one `--user-data-dir`, and it lives
/// inside this session's private temp root. Chromium would otherwise fall back
/// to the real user's profile (cookies, saved passwords). Checked right before
/// spawn so a refactor cannot silently drop it.
pub fn ensure_session_profile(args: &[String], session_root: &Path) -> Result<()> {
    let dirs: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix("--user-data-dir=")).collect();
    match dirs.as_slice() {
        [one] if Path::new(one).starts_with(session_root) && Path::new(one) != session_root => Ok(()),
        [] => Err(RpError::config("internal: browser launch without --user-data-dir refused")),
        [one] => Err(RpError::config(format!("internal: --user-data-dir {one} is outside the session temp dir"))),
        _ => Err(RpError::config("internal: multiple --user-data-dir flags refused")),
    }
}

/// Resolver-level allowlist: every hostname fails to resolve unless listed.
/// Applies to subresources and redirects too, not just top-level navigation.
/// Limit: IP-literal URLs bypass the resolver; `check_url` blocks those at
/// navigation time, but a page could still embed an IP-literal subresource.
pub fn host_resolver_rules(hosts: Option<&[String]>) -> Option<String> {
    let hosts = hosts?;
    let mut r = String::from("MAP * ~NOTFOUND");
    for h in hosts {
        r.push_str(" , EXCLUDE ");
        r.push_str(h);
    }
    Some(r)
}

/// Minimal environment for the browser. Nothing from the host leaks through:
/// no tokens, proxies, cloud credentials, SSH agents or DISPLAY.
pub fn child_env(session_root: &Path) -> Vec<(String, String)> {
    let home = session_root.join("home");
    let tmp = session_root.join("tmp");
    vec![
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("HOME".into(), home.display().to_string()),
        ("TMPDIR".into(), tmp.display().to_string()),
        ("XDG_CONFIG_HOME".into(), home.join(".config").display().to_string()),
        ("XDG_CACHE_HOME".into(), home.join(".cache").display().to_string()),
        ("LANG".into(), "C.UTF-8".into()),
        ("TZ".into(), "UTC".into()),
    ]
}

/// Build the final program + argv, wrapping in bubblewrap if requested.
pub fn wrap_command(
    cfg: &SessionConfig,
    chrome: &Path,
    args: Vec<String>,
    session_root: &Path,
    downloads: Option<&Path>,
) -> Result<(PathBuf, Vec<String>)> {
    match cfg.isolation {
        Isolation::None => Ok((chrome.to_path_buf(), args)),
        Isolation::Bwrap => {
            if !cfg!(target_os = "linux") {
                return Err(RpError::SandboxUnavailable { reason: "bubblewrap is Linux-only".into() });
            }
            let bwrap = which("bwrap")
                .ok_or_else(|| RpError::SandboxUnavailable { reason: "`bwrap` not found on PATH".into() })?;
            // Resolve symlinks (/usr/bin/google-chrome -> /etc/alternatives -> /opt/...).
            let real = std::fs::canonicalize(chrome).map_err(|e| RpError::io("resolve chrome path", e))?;
            let mut b: Vec<String> = vec![
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--unshare-cgroup-try",
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/lib",
                "/lib",
                "--ro-bind-try",
                "/lib64",
                "/lib64",
                "--ro-bind-try",
                "/bin",
                "/bin",
                "--ro-bind-try",
                "/opt",
                "/opt",
                "--ro-bind-try",
                "/etc/fonts",
                "/etc/fonts",
                "--ro-bind-try",
                "/etc/ssl",
                "/etc/ssl",
                "--ro-bind-try",
                "/etc/ca-certificates",
                "/etc/ca-certificates",
                "--ro-bind-try",
                "/etc/pki",
                "/etc/pki",
                "--ro-bind-try",
                "/etc/resolv.conf",
                "/etc/resolv.conf",
                "--ro-bind-try",
                "/etc/hosts",
                "/etc/hosts",
                "--ro-bind-try",
                "/etc/nsswitch.conf",
                "/etc/nsswitch.conf",
                "--ro-bind-try",
                "/etc/ld.so.cache",
                "/etc/ld.so.cache",
                "--ro-bind-try",
                "/etc/alternatives",
                "/etc/alternatives",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
            ]
            .into_iter()
            .map(String::from)
            .collect();
            let root = session_root.display().to_string();
            b.extend(["--bind".into(), root.clone(), root.clone(), "--chdir".into(), root]);
            if let Some(d) = downloads {
                let d = d.display().to_string();
                b.extend(["--bind".into(), d.clone(), d]);
            }
            b.push("--".into());
            b.push(real.display().to_string());
            b.extend(args);
            Ok((bwrap, b))
        }
    }
}

/// Navigation policy: only http(s) by default; file:// opt-in; no chrome://,
/// javascript:, data: or other schemes that bypass the network allowlist.
pub fn check_url(raw: &str, cfg: &SessionConfig) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| RpError::config(format!("invalid URL `{raw}`: {e}")))?;
    match url.scheme() {
        "http" | "https" => {}
        "file" if cfg.allow_file_urls => return Ok(url),
        "about" if url.as_str() == "about:blank" => return Ok(url),
        s => {
            return Err(RpError::policy(format!(
                "scheme `{s}:` is not allowed (only http/https{})",
                if cfg.allow_file_urls { "/file" } else { "; use --allow-file-urls for local files" }
            )))
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RpError::policy("credentials in URLs are not allowed"));
    }
    if let Some(allowed) = &cfg.allowed_hosts {
        let host = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']');
        let ok = allowed.iter().any(|a| {
            let a = a.trim_start_matches('[').trim_end_matches(']');
            a.eq_ignore_ascii_case(host)
                || a.strip_prefix("*.").is_some_and(|suffix| {
                    host.len() > suffix.len()
                        && host.to_ascii_lowercase().ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
                })
        });
        if !ok {
            return Err(RpError::policy(format!("host `{host}` is not in the allowed hosts list")));
        }
    }
    Ok(url)
}

/// Resolve a task-supplied relative output path inside `out_dir` and open it.
/// Rejects absolute paths, `..`, symlink escapes, unknown extensions and
/// (unless `overwrite`) existing files. Uses O_EXCL to avoid symlink races.
pub fn open_output(out_dir: &Path, rel: &str, allowed_ext: &[&str], overwrite: bool) -> Result<(PathBuf, File)> {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        return Err(RpError::policy(format!("output path `{rel}` must be relative to the output dir")));
    }
    if p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(RpError::policy(format!("output path `{rel}` may not contain `..`")));
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !allowed_ext.contains(&ext.as_str()) {
        return Err(RpError::policy(format!("output extension `.{ext}` not allowed (use {})", allowed_ext.join(", "))));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| RpError::io("create output dir", e))?;
    let base = std::fs::canonicalize(out_dir).map_err(|e| RpError::io("resolve output dir", e))?;
    let full = base.join(p);
    let parent = full.parent().unwrap_or(&base).to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| RpError::io("create output subdir", e))?;
    let real_parent = std::fs::canonicalize(&parent).map_err(|e| RpError::io("resolve output subdir", e))?;
    if !real_parent.starts_with(&base) {
        return Err(RpError::policy(format!("output path `{rel}` escapes the output dir via a symlink")));
    }
    let target = real_parent.join(full.file_name().unwrap_or_default());
    let mut o = OpenOptions::new();
    o.write(true);
    if overwrite {
        if let Ok(m) = std::fs::symlink_metadata(&target) {
            if m.file_type().is_symlink() {
                return Err(RpError::policy(format!("refusing to write through symlink `{rel}`")));
            }
        }
        o.create(true).truncate(true);
    } else {
        o.create_new(true);
    }
    let f = o.open(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            RpError::policy(format!("`{rel}` already exists; pass --overwrite to replace it"))
        } else {
            RpError::io(format!("open {}", target.display()), e)
        }
    })?;
    Ok((target, f))
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}

/// Locate a Chrome/Chromium binary.
pub fn find_chrome(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(RpError::Launch {
                reason: format!("--chrome path `{}` does not exist", p.display()),
                hint: Some("pass the full path to a Chrome or Chromium binary".into()),
            })
        };
    }
    if let Some(p) = std::env::var_os("RUSTPLAY_CHROME").map(PathBuf::from) {
        if p.is_file() {
            return Ok(p);
        }
    }
    for name in
        ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome", "chrome-headless-shell"]
    {
        if let Some(p) = which(name) {
            return Ok(p);
        }
    }
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        if Path::new(p).is_file() {
            return Ok(PathBuf::from(p));
        }
    }
    Err(RpError::Launch {
        reason: "no Chrome/Chromium binary found".into(),
        hint: Some("install chromium (`apt install chromium`) or pass --chrome / set RUSTPLAY_CHROME".into()),
    })
}

pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SessionConfig {
        SessionConfig::default()
    }

    #[test]
    fn hardened_flags_present() {
        let a = chrome_args(&cfg(), Path::new("/p"), Path::new("/c"));
        for f in [
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--disable-breakpad",
            "--headless=new",
        ] {
            assert!(a.iter().any(|x| x == f), "missing {f}");
        }
        assert!(a.iter().any(|x| x.starts_with("--disable-features=Translate")));
        assert!(!a.iter().any(|x| x == "--no-sandbox"), "chrome sandbox must be on by default");
        assert!(a.contains(&"--user-data-dir=/p".to_string()));
        assert!(a.iter().any(|x| x == "--no-startup-window"));
        assert!(!a.iter().any(|x| !x.starts_with("--")), "no positional URL: rustplay opens its page over CDP: {a:?}");
    }

    #[test]
    fn transport_flags() {
        let pipe = chrome_args(&cfg(), Path::new("/p"), Path::new("/c"));
        assert!(pipe.contains(&"--remote-debugging-pipe".to_string()));
        assert!(!pipe.iter().any(|a| a.starts_with("--remote-debugging-port")), "pipe mode must not open a port");
        let tcp =
            chrome_args(&SessionConfig { cdp_transport: CdpTransport::Tcp, ..cfg() }, Path::new("/p"), Path::new("/c"));
        assert!(tcp.contains(&"--remote-debugging-port=0".to_string()));
        assert!(!tcp.contains(&"--remote-debugging-pipe".to_string()));
    }

    #[test]
    fn profile_dir_is_mandatory_and_private() {
        let root = Path::new("/tmp/rustplay-abc");
        let a = chrome_args(&cfg(), &root.join("profile"), &root.join("cache"));
        assert!(ensure_session_profile(&a, root).is_ok());
        let mut none = a.clone();
        none.retain(|x| !x.starts_with("--user-data-dir="));
        assert_eq!(ensure_session_profile(&none, root).unwrap_err().code(), "CONFIG_INVALID");
        let outside = chrome_args(&cfg(), Path::new("/home/u/.config/google-chrome"), Path::new("/c"));
        assert!(ensure_session_profile(&outside, root).is_err());
        let mut twice = a.clone();
        twice.push("--user-data-dir=/tmp/rustplay-abc/other".into());
        assert!(ensure_session_profile(&twice, root).is_err());
        let rootdir = chrome_args(&cfg(), root, Path::new("/c"));
        assert!(ensure_session_profile(&rootdir, root).is_err(), "the root itself also holds HOME/TMPDIR");
    }

    #[test]
    fn resolver_rules() {
        let h = vec!["example.com".to_string(), "127.0.0.1".to_string()];
        assert_eq!(host_resolver_rules(Some(&h)).unwrap(), "MAP * ~NOTFOUND , EXCLUDE example.com , EXCLUDE 127.0.0.1");
        assert!(host_resolver_rules(None).is_none());
    }

    #[test]
    fn env_is_scrubbed() {
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "leak");
        let env = child_env(Path::new("/s"));
        assert!(env.iter().all(|(k, _)| k != "AWS_SECRET_ACCESS_KEY"));
        assert!(env.iter().any(|(k, v)| k == "HOME" && v == "/s/home"));
    }

    #[test]
    fn url_policy() {
        let c = cfg();
        assert!(check_url("https://example.com", &c).is_ok());
        for bad in
            ["file:///etc/passwd", "chrome://settings", "javascript:alert(1)", "data:text/html,hi", "https://u:p@x.com"]
        {
            assert_eq!(check_url(bad, &c).unwrap_err().code(), "POLICY_VIOLATION", "{bad}");
        }
        assert_eq!(check_url("not a url", &c).unwrap_err().code(), "CONFIG_INVALID");
        let c = SessionConfig { allowed_hosts: Some(vec!["*.example.com".into(), "127.0.0.1".into()]), ..cfg() };
        assert!(check_url("https://a.example.com/x", &c).is_ok());
        assert!(check_url("http://127.0.0.1:8080/", &c).is_ok());
        assert!(check_url("https://example.com.evil.io", &c).is_err());
        assert!(check_url("https://evil.io", &c).is_err());
    }

    #[test]
    fn output_confinement() {
        let d = tempfile::tempdir().unwrap();
        let ok = open_output(d.path(), "shots/a.png", &["png"], false).unwrap();
        assert!(ok.0.starts_with(std::fs::canonicalize(d.path()).unwrap()));
        assert_eq!(open_output(d.path(), "shots/a.png", &["png"], false).unwrap_err().code(), "POLICY_VIOLATION");
        assert!(open_output(d.path(), "shots/a.png", &["png"], true).is_ok());
        for bad in ["../x.png", "/tmp/x.png", "a/../../x.png", "x.sh", ""] {
            assert_eq!(open_output(d.path(), bad, &["png"], false).unwrap_err().code(), "POLICY_VIOLATION", "{bad}");
        }
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
            assert_eq!(open_output(d.path(), "link/x.png", &["png"], false).unwrap_err().code(), "POLICY_VIOLATION");
        }
    }
}
