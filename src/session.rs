//! One isolated browser session: its own process, profile, temp dirs and page.
//! Cleanup happens in `close()` (graceful, reports errors) and again in `Drop`
//! (best effort, covers panics, timeouts and Ctrl-C cancellation).

use crate::browser::{BrowserProcess, Endpoint};
use crate::config::{CdpTransport, SessionConfig};
use crate::errors::{ErrorReport, NotActionable, Result, RpError};
use crate::limits::{CgroupHost, Enforcement, LimitTrip, ResourceLimits, SessionCgroups, Watchdog};
use crate::sandbox;
use chromiumoxide::cdp::browser_protocol::browser::{SetDownloadBehaviorBehavior, SetDownloadBehaviorParams};
use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType, InsertTextParams};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, Page};
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const POLL: Duration = Duration::from_millis(100);

/// Element states for `wait_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    Attached,
    #[default]
    Visible,
    Hidden,
    Detached,
}

pub struct Session {
    pub id: String,
    cfg: SessionConfig,
    browser: Option<Browser>,
    page: Option<Page>,
    handler: Option<JoinHandle<()>>,
    process: BrowserProcess,
    root: Option<TempDir>,
    profile: PathBuf,
    downloads: Option<PathBuf>,
    watchdog: Option<Watchdog>,
    cgroups: Option<Arc<SessionCgroups>>,
    enforcement: Enforcement,
    warnings: Vec<ErrorReport>,
}

/// Limits, how they were enforced, and what the session actually used.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResourceReport {
    pub limits: ResourceLimits,
    pub enforcement: Enforcement,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_used_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
}

impl Session {
    pub async fn launch(cfg: SessionConfig) -> Result<Session> {
        cfg.validate()?;
        let started = Instant::now();
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let prefix = format!("rustplay-{id}-");
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix);
        let root = match &cfg.temp_root {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| RpError::io("create temp root", e))?;
                builder.tempdir_in(dir)
            }
            None => builder.tempdir(),
        }
        .map_err(|e| RpError::io("create session temp dir", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700));
        }
        let profile = root.path().join("profile");
        let cache = root.path().join("cache");
        for d in [&profile, &cache, &root.path().join("home"), &root.path().join("tmp")] {
            std::fs::create_dir_all(d).map_err(|e| RpError::io("create session dirs", e))?;
        }
        let downloads = match &cfg.downloads_dir {
            Some(base) => {
                let d = base.join(&id);
                std::fs::create_dir_all(&d).map_err(|e| RpError::io("create downloads dir", e))?;
                Some(std::fs::canonicalize(&d).map_err(|e| RpError::io("resolve downloads dir", e))?)
            }
            None => None,
        };

        // Resource limits: cgroups exist before the browser does, or we fail closed.
        let (cgroups, mut enforcement) =
            match SessionCgroups::create(&CgroupHost::discover(), &id, &cfg.limits, cfg.cgroup_parent.as_deref())? {
                Some((c, e)) => (Some(c), e),
                None => (None, Enforcement::default()),
            };
        if cfg.limits.wall_ms.is_some() {
            enforcement.wall = Some("watchdog SIGKILL of the process group");
        }
        let attach = cgroups.as_ref().map(|c| c.attach_fds()).unwrap_or_default();
        let launched =
            match BrowserProcess::launch(&cfg, root.path(), &profile, &cache, downloads.as_deref(), attach).await {
                Ok(l) => l,
                Err(e) => {
                    if let Some(c) = &cgroups {
                        let _ = c.remove(Duration::from_millis(500));
                    }
                    return Err(e);
                }
            };
        let mut process = launched.process;
        let cgroups = cgroups.map(|c| {
            c.release_attach_fds();
            Arc::new(c)
        });
        let watchdog = match (cfg.limits.is_empty(), process.pid()) {
            (false, Some(pid)) => Some(Watchdog::start(pid, cfg.limits, cgroups.clone(), started)),
            _ => None,
        };

        // From here on, any failure must tear down process + cgroups.
        let fail = |process: &mut BrowserProcess, e: RpError| {
            process.kill_now();
            if let Some(c) = &cgroups {
                let c = c.clone();
                std::thread::spawn(move || {
                    c.kill_all();
                    let _ = c.remove(Duration::from_secs(2));
                });
            }
            e
        };

        let mut warnings = Vec::new();
        if cfg.cdp_transport == CdpTransport::Tcp {
            tracing::warn!("--cdp-transport tcp is deprecated; DevTools is reachable on a loopback port");
            warnings.push(RpError::TcpTransportDeprecated.report());
        }

        let hcfg = HandlerConfig {
            viewport: Some(Viewport {
                width: cfg.viewport.0,
                height: cfg.viewport.1,
                device_scale_factor: Some(1.0),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            }),
            request_timeout: Duration::from_millis(cfg.timeouts.navigation_ms.max(cfg.timeouts.action_ms) + 5_000),
            ignore_https_errors: false,
            ..Default::default()
        };
        let connect = async {
            let (browser, mut handler) = match launched.endpoint {
                #[cfg(unix)]
                Endpoint::Pipe(t) => Browser::connect_with_transport(t, "pipe", hcfg).await,
                Endpoint::Tcp(url) => Browser::connect_with_config(url, hcfg).await,
            }
            .map_err(|e| RpError::Launch { reason: format!("CDP connect: {e}"), hint: None })?;
            let task = tokio::spawn(async move {
                while let Some(ev) = handler.next().await {
                    if let Err(e) = ev {
                        tracing::trace!("cdp handler: {e}");
                    }
                }
                tracing::debug!("cdp handler finished");
            });
            Ok::<_, RpError>((browser, task))
        };
        let (browser, handler) =
            match tokio::time::timeout(Duration::from_millis(cfg.timeouts.launch_ms), connect).await {
                Ok(Ok(x)) => x,
                Ok(Err(e)) => return Err(fail(&mut process, e)),
                Err(_) => {
                    let e = process.startup_failure("CDP attach timed out");
                    return Err(fail(&mut process, e));
                }
            };

        let behavior = if downloads.is_some() {
            SetDownloadBehaviorBehavior::AllowAndName
        } else {
            SetDownloadBehaviorBehavior::Deny
        };
        let mut dl = SetDownloadBehaviorParams::builder().behavior(behavior);
        if let Some(d) = &downloads {
            dl = dl.download_path(d.display().to_string());
        }
        // The first command doubles as the readiness probe. Over a pipe there is
        // no "DevTools listening" line to wait for; the answer is the signal.
        let page = async {
            browser
                .execute(dl.build().map_err(RpError::config)?)
                .await
                .map_err(|e| RpError::from_cdp("set download policy", e))?;
            browser.new_page("about:blank").await.map_err(|e| RpError::from_cdp("open page", e))
        };
        let page = match tokio::time::timeout(Duration::from_millis(cfg.timeouts.launch_ms), page).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                handler.abort();
                let e = match e {
                    RpError::Disconnected { .. } => process.startup_failure("closed the CDP connection"),
                    other => other,
                };
                return Err(fail(&mut process, e));
            }
            Err(_) => {
                handler.abort();
                let e = process.startup_failure("did not answer the first CDP command");
                return Err(fail(&mut process, e));
            }
        };

        // Defense in depth: in pipe mode, nothing in the browser tree may listen on TCP.
        if cfg.cdp_transport == CdpTransport::Pipe {
            if let Some(listeners) = process.pid().and_then(crate::procinfo::tcp_listeners) {
                if !listeners.is_empty() {
                    handler.abort();
                    let addrs = listeners.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
                    return Err(fail(&mut process, RpError::CdpPortExposed { addrs }));
                }
            }
        }

        tracing::info!(session = %id, pid = ?process.pid(), transport = cfg.cdp_transport.name(), "session started");
        Ok(Session {
            id,
            cfg,
            browser: Some(browser),
            page: Some(page),
            handler: Some(handler),
            process,
            root: Some(root),
            profile,
            downloads,
            watchdog,
            cgroups,
            enforcement,
            warnings,
        })
    }

    /// Watch channel that yields `Some(trip)` when a limit fires (None without limits).
    pub fn limit_watch(&self) -> Option<tokio::sync::watch::Receiver<Option<LimitTrip>>> {
        self.watchdog.as_ref().map(Watchdog::subscribe)
    }

    /// A resource limit that has fired, if any.
    pub fn limit_trip(&self) -> Option<LimitTrip> {
        self.watchdog.as_ref().and_then(Watchdog::trip)
    }

    /// TCP sockets the browser tree is listening on (`None` = cannot inspect on this OS).
    pub fn tcp_listeners(&self) -> Option<Vec<std::net::SocketAddr>> {
        crate::procinfo::tcp_listeners(self.process.pid()?)
    }

    /// Non-fatal findings to surface in the report (e.g. deprecated transport).
    pub fn warnings(&self) -> &[ErrorReport] {
        &self.warnings
    }

    pub fn resource_report(&self) -> Option<ResourceReport> {
        if self.cfg.limits.is_empty() {
            return None;
        }
        Some(ResourceReport {
            limits: self.cfg.limits,
            enforcement: self.enforcement.clone(),
            cpu_used_ms: self.cgroups.as_ref().and_then(|c| c.cpu_used_ms()),
            peak_memory_bytes: self.cgroups.as_ref().and_then(|c| c.peak_memory_bytes()),
        })
    }

    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }
    pub fn profile_dir(&self) -> &Path {
        &self.profile
    }
    pub fn downloads_dir(&self) -> Option<&Path> {
        self.downloads.as_deref()
    }
    pub fn pid(&self) -> Option<u32> {
        self.process.pid()
    }

    fn page(&self) -> Result<&Page> {
        self.page.as_ref().ok_or_else(|| RpError::Disconnected { reason: "session is closed".into() })
    }

    /// Turn a CDP failure into a typed error, noticing if the browser died.
    fn cdp(&mut self, action: &str, e: chromiumoxide::error::CdpError) -> RpError {
        if let Some(t) = self.limit_trip() {
            return t.into();
        }
        if let Some(status) = self.process.exited() {
            return RpError::Disconnected { reason: format!("browser process exited ({status}) during {action}") };
        }
        RpError::from_cdp(action, e)
    }

    async fn with_timeout<T, F>(&mut self, action: &str, ms: u64, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, chromiumoxide::error::CdpError>>,
    {
        match tokio::time::timeout(Duration::from_millis(ms), fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(self.cdp(action, e)),
            Err(_) => {
                if let Some(t) = self.limit_trip() {
                    return Err(t.into());
                }
                if let Some(status) = self.process.exited() {
                    return Err(RpError::Disconnected { reason: format!("browser exited ({status})") });
                }
                Err(RpError::ActionTimeout { action: action.into(), ms })
            }
        }
    }

    // ---------- primitives ----------

    pub async fn goto(&mut self, raw: &str) -> Result<Value> {
        let url = sandbox::check_url(raw, &self.cfg)?;
        let ms = self.cfg.timeouts.navigation_ms;
        let page = self.page()?.clone();
        let res = tokio::time::timeout(Duration::from_millis(ms), page.goto(url.as_str())).await;
        if let Some(t) = self.limit_trip() {
            return Err(t.into());
        }
        match res {
            Err(_) => return Err(RpError::NavigationTimeout { url: url.to_string(), ms }),
            Ok(Err(chromiumoxide::error::CdpError::ChromeMessage(m))) if m.starts_with("net::") => {
                return Err(RpError::Network { url: url.to_string(), reason: m })
            }
            Ok(Err(chromiumoxide::error::CdpError::Timeout)) => {
                return Err(RpError::NavigationTimeout { url: url.to_string(), ms })
            }
            Ok(Err(e)) => return Err(self.cdp("goto", e)),
            Ok(Ok(_)) => {}
        }
        let info = self.eval_raw("({url: location.href, title: document.title})", "goto").await?;
        Ok(info)
    }

    /// Evaluate an expression and return its JSON value (undefined -> null).
    pub async fn eval_raw(&mut self, expr: &str, action: &str) -> Result<Value> {
        let page = self.page()?.clone();
        let ms = self.cfg.timeouts.action_ms;
        let r = self.with_timeout(action, ms, page.evaluate_expression(expr.to_string())).await?;
        Ok(r.value().cloned().unwrap_or(Value::Null))
    }

    /// Poll a JS probe until it reports `ok` or the deadline passes.
    /// Returns the last probe result on timeout so the caller can explain why.
    async fn poll(&mut self, script: &str, ms: u64, action: &str) -> Result<std::result::Result<Value, Value>> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            let v = self.eval_raw(script, action).await?;
            if v.get("state").and_then(Value::as_str) == Some("ok") {
                return Ok(Ok(v));
            }
            if v.get("state").and_then(Value::as_str) == Some("invalid_selector") {
                return Ok(Err(v));
            }
            if Instant::now() >= deadline {
                return Ok(Err(v));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub async fn wait_for(&mut self, selector: &str, state: WaitState, ms: u64) -> Result<Value> {
        let script = probe_js(selector, ProbeMode::Wait(state));
        match self.poll(&script, ms, "wait_for").await? {
            Ok(v) => Ok(json!({ "state": state, "elapsed_ms": v.get("t") })),
            Err(last) => Err(match state_of(&last) {
                "invalid_selector" => invalid_selector(selector, &last),
                "not_found" | "hidden_wait" if matches!(state, WaitState::Attached | WaitState::Visible) => {
                    if state_of(&last) == "not_found" {
                        RpError::SelectorNotFound { selector: selector.into(), ms }
                    } else {
                        RpError::ActionTimeout { action: format!("wait_for `{selector}` to be visible"), ms }
                    }
                }
                _ => RpError::ActionTimeout {
                    action: format!("wait_for `{selector}` to be {state:?}").to_lowercase(),
                    ms,
                },
            }),
        }
    }

    /// Wait until the element is actionable, then return its click point.
    async fn actionable(&mut self, selector: &str, editable: bool, ms: u64) -> Result<Point> {
        let script = probe_js(selector, ProbeMode::Act { editable });
        match self.poll(&script, ms, "probe").await? {
            Ok(v) => Ok(Point { x: v["x"].as_f64().unwrap_or_default(), y: v["y"].as_f64().unwrap_or_default() }),
            Err(last) => Err(not_actionable(selector, &last, ms)),
        }
    }

    async fn ensure_attached(&mut self) -> Result<()> {
        let v = self
            .eval_raw(
                &format!("(() => {{ const t = globalThis[{TARGET}]; return !!(t && t.isConnected); }})()"),
                "probe",
            )
            .await?;
        if v.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(RpError::NotActionable { selector: "<target>".into(), reason: NotActionable::Detached })
        }
    }

    pub async fn click(&mut self, selector: &str, ms: u64) -> Result<Value> {
        let pt = self.actionable(selector, false, ms).await?;
        self.ensure_attached()
            .await
            .map_err(|_| RpError::NotActionable { selector: selector.into(), reason: NotActionable::Detached })?;
        let page = self.page()?.clone();
        self.with_timeout("click", ms, async { page.click(pt).await.map(|_| ()) }).await?;
        Ok(json!({ "x": pt.x, "y": pt.y }))
    }

    pub async fn type_text(&mut self, selector: &str, text: &str, clear: bool, ms: u64) -> Result<Value> {
        self.actionable(selector, true, ms).await?;
        let prep = format!(
            "(() => {{ const t = globalThis[{TARGET}]; if (!t || !t.isConnected) return false; t.focus();
               if ({clear}) {{ if ('value' in t) {{ t.value = ''; }} else {{ t.textContent = ''; }}
                 t.dispatchEvent(new Event('input', {{bubbles: true}})); }}
               if ('value' in t && typeof t.setSelectionRange === 'function') {{ try {{ const n = t.value.length; t.setSelectionRange(n, n); }} catch (_) {{}} }}
               return true; }})()"
        );
        if self.eval_raw(&prep, "type").await?.as_bool() != Some(true) {
            return Err(RpError::NotActionable { selector: selector.into(), reason: NotActionable::Detached });
        }
        for ch in text.chars() {
            let s = if ch == '\n' { "Enter".to_string() } else { ch.to_string() };
            if chromiumoxide::keys::get_key_definition(&s).is_some() {
                self.key(&s, 0, ms).await?;
            } else {
                let page = self.page()?.clone();
                let p = InsertTextParams::new(ch.to_string());
                self.with_timeout("type", ms, async { page.execute(p).await.map(|_| ()) }).await?;
            }
        }
        Ok(json!({ "typed_chars": text.chars().count() }))
    }

    /// Press a key or combo such as `Enter`, `Tab`, `Shift+Tab`, `Control+a`.
    pub async fn press(&mut self, combo: &str, selector: Option<&str>, ms: u64) -> Result<Value> {
        let (mods, key) = parse_combo(combo)?;
        if let Some(sel) = selector {
            self.actionable(sel, false, ms).await?;
            self.eval_raw(&format!("globalThis[{TARGET}].focus()"), "press").await?;
        }
        self.key(&key, mods, ms).await?;
        Ok(json!({ "key": combo }))
    }

    async fn key(&mut self, key: &str, modifiers: i64, ms: u64) -> Result<()> {
        let def = chromiumoxide::keys::get_key_definition(key).ok_or_else(|| {
            RpError::config(format!("unknown key `{key}` (use DOM key names like Enter, Tab, ArrowDown)"))
        })?;
        let text = if modifiers & !8 != 0 {
            None // Ctrl/Alt/Meta chords produce no text
        } else {
            def.text.or(if def.key.chars().count() == 1 { Some(def.key) } else { None })
        };
        let mut b = DispatchKeyEventParams::builder()
            .key(def.key)
            .code(def.code)
            .windows_virtual_key_code(def.key_code)
            .native_virtual_key_code(def.key_code)
            .modifiers(modifiers);
        if let Some(t) = text {
            b = b.text(t);
        }
        let down_type = if text.is_some() { DispatchKeyEventType::KeyDown } else { DispatchKeyEventType::RawKeyDown };
        let down = b.clone().r#type(down_type).build().map_err(RpError::config)?;
        let up = b.r#type(DispatchKeyEventType::KeyUp).build().map_err(RpError::config)?;
        let page = self.page()?.clone();
        self.with_timeout("press", ms, async {
            page.execute(down).await?;
            page.execute(up).await.map(|_| ())
        })
        .await
    }

    pub async fn scroll(&mut self, selector: Option<&str>, x: f64, y: f64, ms: u64) -> Result<Value> {
        if let Some(sel) = selector {
            self.wait_for(sel, WaitState::Attached, ms).await?;
            self.eval_raw(
                &format!("document.querySelector({}).scrollIntoView({{block:'center', inline:'center', behavior:'instant'}})", js_str(sel)),
                "scroll",
            )
            .await?;
        } else {
            self.eval_raw(&format!("window.scrollBy({{left: {x}, top: {y}, behavior: 'instant'}})"), "scroll").await?;
        }
        self.eval_raw("({x: window.scrollX, y: window.scrollY})", "scroll").await
    }

    pub async fn text(&mut self, selector: &str, all: bool, ms: u64) -> Result<Value> {
        self.wait_for(selector, WaitState::Attached, ms).await?;
        let s = js_str(selector);
        let expr = if all {
            format!("Array.from(document.querySelectorAll({s}), e => e.innerText ?? e.textContent)")
        } else {
            format!("(() => {{ const e = document.querySelector({s}); return e ? (e.innerText ?? e.textContent) : null; }})()")
        };
        self.eval_raw(&expr, "text").await
    }

    pub async fn attribute(&mut self, selector: &str, name: &str, all: bool, ms: u64) -> Result<Value> {
        self.wait_for(selector, WaitState::Attached, ms).await?;
        let (s, n) = (js_str(selector), js_str(name));
        let expr = if all {
            format!("Array.from(document.querySelectorAll({s}), e => e.getAttribute({n}))")
        } else {
            format!("document.querySelector({s})?.getAttribute({n}) ?? null")
        };
        self.eval_raw(&expr, "attribute").await
    }

    pub async fn screenshot(
        &mut self,
        selector: Option<&str>,
        full_page: bool,
        format: CaptureScreenshotFormat,
        ms: u64,
    ) -> Result<Vec<u8>> {
        let page = self.page()?.clone();
        if let Some(sel) = selector {
            self.wait_for(sel, WaitState::Visible, ms).await?;
            let s = sel.to_string();
            return self
                .with_timeout("screenshot", ms, async move {
                    let el = page.find_element(s).await?;
                    el.scroll_into_view().await?;
                    el.screenshot(format).await
                })
                .await;
        }
        let params = ScreenshotParams::builder().format(format).full_page(full_page).build();
        self.with_timeout("screenshot", ms, async move { page.screenshot(params).await }).await
    }

    pub async fn eval(&mut self, expression: &str) -> Result<Value> {
        self.eval_raw(expression, "eval").await
    }

    /// Graceful shutdown. Always kills the process and removes the profile,
    /// and reports anything that went wrong while doing so.
    pub async fn close(mut self) -> Result<()> {
        // Stop the watchdog before the leader is reaped so it can never signal a recycled pgid.
        if let Some(mut w) = self.watchdog.take() {
            w.stop();
        }
        let mut browser = self.browser.take();
        self.page.take();
        if let Some(b) = browser.as_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(2), b.close()).await;
        }
        drop(browser);
        self.process.shutdown(Duration::from_secs(2)).await;
        if let Some(h) = self.handler.take() {
            h.abort();
        }
        if let Some(root) = self.root.take() {
            let path = root.path().to_path_buf();
            // Chromium helpers can race the delete; retry briefly.
            let mut last = None;
            for _ in 0..5 {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {
                        last = None;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        last = None;
                        break;
                    }
                    Err(e) => {
                        last = Some(e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            std::mem::forget(root); // already removed (or reported below)
            if let Some(e) = last {
                return Err(RpError::Cleanup { reason: format!("{}: {e}", path.display()) });
            }
        }
        if let Some(cg) = self.cgroups.take() {
            cg.kill_all();
            if let Err(e) = cg.remove(Duration::from_secs(2)) {
                return Err(RpError::Cleanup { reason: e });
            }
        }
        tracing::info!(session = %self.id, "session closed");
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Runs on panic unwinding, timeout cancellation and Ctrl-C drops.
        if let Some(h) = self.handler.take() {
            h.abort();
        }
        if let Some(mut w) = self.watchdog.take() {
            w.stop();
        }
        self.process.kill_now();
        if let Some(cg) = self.cgroups.take() {
            // rmdir needs the processes gone; do not block the async runtime waiting.
            std::thread::spawn(move || {
                cg.kill_all();
                let _ = cg.remove(Duration::from_secs(3));
            });
        }
        if self.root.is_some() {
            tracing::warn!(session = %self.id, "session dropped without close(); force-killed browser");
        }
        // TempDir's own Drop removes the profile.
    }
}

// ---------- JS helpers ----------

/// Symbol key under which the probe stores the target element.
const TARGET: &str = "Symbol.for('rustplay.target')";

pub(crate) fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

enum ProbeMode {
    Wait(WaitState),
    Act { editable: bool },
}

fn probe_js(selector: &str, mode: ProbeMode) -> String {
    let sel = js_str(selector);
    let tail = match mode {
        ProbeMode::Wait(WaitState::Attached) => "return el ? {state:'ok'} : {state:'not_found'};".to_string(),
        ProbeMode::Wait(WaitState::Detached) => "return el ? {state:'still_attached'} : {state:'ok'};".to_string(),
        ProbeMode::Wait(WaitState::Visible) => {
            "if (!el) return {state:'not_found'}; return vis(el) ? {state:'ok'} : {state:'hidden_wait'};".to_string()
        }
        ProbeMode::Wait(WaitState::Hidden) => {
            "return (!el || !vis(el)) ? {state:'ok'} : {state:'still_visible'};".to_string()
        }
        ProbeMode::Act { editable } => format!(
            r#"if (!el) return {{state:'not_found'}};
            if (!el.isConnected) return {{state:'detached'}};
            if (!vis(el)) return {{state:'hidden'}};
            if (el.disabled === true || el.closest('fieldset[disabled]') || el.getAttribute('aria-disabled') === 'true') return {{state:'disabled'}};
            if ({editable}) {{
              const ok = (el instanceof HTMLTextAreaElement) || el.isContentEditable ||
                (el instanceof HTMLInputElement && !['checkbox','radio','button','submit','reset','file','image','hidden','range','color'].includes(el.type));
              if (!ok || el.readOnly) return {{state:'not_editable'}};
            }}
            el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}});
            const r = el.getBoundingClientRect();
            if (r.width < 1 || r.height < 1) return {{state:'zero_size'}};
            const x = r.left + r.width / 2, y = r.top + r.height / 2;
            // elementFromPoint stops at a shadow host, so descend through open
            // roots or every element inside one looks obscured by its host.
            let hit = document.elementFromPoint(x, y);
            while (hit && hit.shadowRoot) {{
              const inner = hit.shadowRoot.elementFromPoint(x, y);
              if (!inner || inner === hit) break;
              hit = inner;
            }}
            if (hit && hit !== el && !el.contains(hit)) {{
              const d = hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') +
                (typeof hit.className === 'string' && hit.className.trim() ? '.' + hit.className.trim().split(/\s+/).join('.') : '');
              return {{state:'obscured', by: d}};
            }}
            globalThis[{TARGET}] = el;
            return {{state:'ok', x, y}};"#
        ),
    };
    format!(
        r#"(() => {{
          const vis = (e) => {{
            const r = e.getBoundingClientRect();
            const cv = typeof e.checkVisibility === 'function'
              ? e.checkVisibility({{checkOpacity: true, checkVisibilityCSS: true}})
              : getComputedStyle(e).visibility !== 'hidden' && getComputedStyle(e).display !== 'none';
            return cv && r.width > 0 && r.height > 0;
          }};
          let el;
          try {{ el = document.querySelector({sel}); }}
          catch (err) {{ return {{state:'invalid_selector', message: String(err && err.message || err)}}; }}
          {tail}
        }})()"#
    )
}

fn state_of(v: &Value) -> &str {
    v.get("state").and_then(Value::as_str).unwrap_or("unknown")
}

fn invalid_selector(selector: &str, v: &Value) -> RpError {
    RpError::SelectorInvalid {
        selector: selector.into(),
        reason: v.get("message").and_then(Value::as_str).unwrap_or("syntax error").to_string(),
    }
}

fn not_actionable(selector: &str, last: &Value, ms: u64) -> RpError {
    let reason = match state_of(last) {
        "not_found" => return RpError::SelectorNotFound { selector: selector.into(), ms },
        "invalid_selector" => return invalid_selector(selector, last),
        "hidden" => NotActionable::Hidden,
        "disabled" => NotActionable::Disabled,
        "detached" => NotActionable::Detached,
        "zero_size" => NotActionable::ZeroSize,
        "not_editable" => NotActionable::NotEditable,
        "obscured" => NotActionable::Obscured {
            by: last.get("by").and_then(Value::as_str).unwrap_or("another element").to_string(),
        },
        other => return RpError::ActionTimeout { action: format!("probe ({other})"), ms },
    };
    RpError::NotActionable { selector: selector.into(), reason }
}

/// `Shift+Tab` -> (8, "Tab"). CDP modifier bits: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub fn parse_combo(combo: &str) -> Result<(i64, String)> {
    let parts: Vec<&str> = combo.split('+').collect();
    let (key, mods) = match parts.as_slice() {
        [] => return Err(RpError::config("empty key")),
        // A literal "+" key.
        [..] if combo == "+" => return Ok((0, "+".into())),
        [mods @ .., key] => (*key, mods),
    };
    if key.is_empty() {
        return Err(RpError::config(format!("invalid key combo `{combo}`")));
    }
    let mut bits = 0;
    for m in mods {
        bits |= match m.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            other => return Err(RpError::config(format!("unknown modifier `{other}` in `{combo}`"))),
        };
    }
    Ok((bits, key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse_combo("Enter").unwrap(), (0, "Enter".into()));
        assert_eq!(parse_combo("Shift+Tab").unwrap(), (8, "Tab".into()));
        assert_eq!(parse_combo("Ctrl+Shift+a").unwrap(), (10, "a".into()));
        assert_eq!(parse_combo("+").unwrap(), (0, "+".into()));
        assert!(parse_combo("Hyper+a").is_err());
        assert!(parse_combo("Ctrl+").is_err());
    }

    #[test]
    fn selector_is_safely_quoted() {
        let s = js_str(r#"a[title="x"]'); alert(1); ('"#);
        assert!(s.starts_with('"') && s.ends_with('"'));
        let js = probe_js(r#"');alert(1)//"#, ProbeMode::Act { editable: false });
        assert!(js.contains(r#""');alert(1)//""#));
    }
}
