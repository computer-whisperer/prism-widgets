//! Data providers for prism-widgets.
//!
//! Provider dependencies belong here or in narrower provider crates, not
//! in the common host runner. This keeps `prism-bar` free to reuse the
//! host path without inheriting API-client dependencies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use prism_widgets_core::{
    clock_snapshot, CommandSpec, CpuSpec, Gauge, GaugeGroup, GitHubSpec, GpuSpec, MemorySpec,
    ModuleSnapshot, ModuleSpec, ModuleStatus, ModuleUpdate, ModuleValue, PanelId, PanelSnapshot,
    PanelSpec, UsageSpec,
};
use prism_widgets_host::{PanelDataSource, ProviderHandle, SnapshotSender};
use serde_json::Value;

const COMMAND_TIMEOUT: &str = "10s";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared HTTP agent with bounded timeouts. Without these a hung connection
/// would keep a worker thread (and the snapshot it owes) alive indefinitely,
/// including past a config reload that tried to retire it.
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::AgentBuilder::new()
        .timeout_connect(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
});
const CLAUDE_USAGE_BASE_URL: &str = "https://api.anthropic.com";
const CLAUDE_USAGE_BETA: &str = "oauth-2025-04-20";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_REFRESH_SAFETY_SECS: i64 = 60;

#[derive(Debug)]
pub struct SnapshotStore {
    panels: HashMap<String, PanelSpec>,
    cache: Mutex<HashMap<String, CachedModule>>,
}

#[derive(Clone, Debug)]
struct CachedModule {
    snapshot: ModuleSnapshot,
    updated_at: SystemTime,
}

impl SnapshotStore {
    pub fn from_specs(specs: &[PanelSpec]) -> Self {
        let panels = specs
            .iter()
            .map(|panel| (panel.id.0.clone(), panel.clone()))
            .collect();
        Self {
            panels,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl PanelDataSource for SnapshotStore {
    fn snapshot_for(&self, panel_id: &PanelId) -> PanelSnapshot {
        self.panels
            .get(&panel_id.0)
            .map(|panel| PanelSnapshot {
                panel_id: panel.id.clone(),
                modules: panel
                    .modules
                    .iter()
                    .map(|module| self.module_snapshot(panel, module))
                    .collect(),
            })
            .unwrap_or_else(|| PanelSnapshot::empty(panel_id.clone()))
    }
}

/// A running provider generation: one worker thread per polled module.
///
/// Per-module threads (rather than a shared pool) give the best isolation for
/// a status surface's handful of modules — a slow or hung fetch only delays
/// its own module, never another. If module counts ever grow large enough to
/// make a thread-per-module wasteful, this handle is the seam to swap in a
/// bounded pool without touching the host.
///
/// Dropping the handle signals shutdown and detaches: workers exit after their
/// current fetch returns (bounded by the HTTP/command timeout), and any
/// snapshot pushed afterwards is dropped by the host as it carries the retired
/// epoch.
pub struct SchedulerHandle {
    shutdown: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl ProviderHandle for SchedulerHandle {}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for worker in &self.workers {
            worker.thread().unpark();
        }
        // Intentionally not joined: a worker mid-fetch must not block the host
        // (and thus config reload) until its network call returns.
    }
}

/// Spawn a worker per polled module, pushing snapshots into `sender` tagged
/// with `epoch`. Clock modules are skipped — the host renders them locally.
pub fn start_scheduler(specs: &[PanelSpec], sender: SnapshotSender, epoch: u64) -> SchedulerHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for panel in specs {
        for module in &panel.modules {
            let Some(interval) = module.poll_interval() else {
                continue;
            };
            let panel_id = panel.id.clone();
            let module_id = module.id().to_string();
            let spec = module.clone();
            let sender = sender.clone();
            let shutdown = Arc::clone(&shutdown);
            workers.push(thread::spawn(move || {
                poll_module(
                    &spec, panel_id, module_id, interval, epoch, &sender, &shutdown,
                );
            }));
        }
    }
    SchedulerHandle { shutdown, workers }
}

fn poll_module(
    spec: &ModuleSpec,
    panel: PanelId,
    module: String,
    interval: Duration,
    epoch: u64,
    sender: &SnapshotSender,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::Acquire) {
        let snapshot = fetch_module(spec);
        let update = ModuleUpdate {
            epoch,
            panel: panel.clone(),
            module: module.clone(),
            snapshot,
        };
        if sender.send(update).is_err() {
            return; // host event loop is gone
        }
        if !park_until(Instant::now() + interval, shutdown) {
            return;
        }
    }
}

/// Sleep until `deadline`, waking promptly on shutdown. Returns `false` when
/// shutdown was requested, `true` when the deadline elapsed normally.
fn park_until(deadline: Instant, shutdown: &AtomicBool) -> bool {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::park_timeout(deadline - now);
    }
}

/// Synchronously fetch one module. Runs on a worker thread, never the host.
fn fetch_module(spec: &ModuleSpec) -> ModuleSnapshot {
    match spec {
        ModuleSpec::Clock(spec) => clock_snapshot(spec),
        ModuleSpec::Command(spec) => command_snapshot(spec),
        ModuleSpec::GitHub(spec) => github_snapshot(spec),
        ModuleSpec::Usage(spec) => usage_snapshot(spec),
        ModuleSpec::Cpu(spec) => cpu_snapshot(spec),
        ModuleSpec::Memory(spec) => memory_snapshot(spec),
        ModuleSpec::Gpu(spec) => gpu_snapshot(spec),
    }
}

impl SnapshotStore {
    fn module_snapshot(&self, panel: &PanelSpec, spec: &ModuleSpec) -> ModuleSnapshot {
        if let ModuleSpec::Clock(spec) = spec {
            return clock_snapshot(spec);
        }

        let (id, interval) = module_id_interval(spec);
        let cache_key = format!("{}:{id}", panel.id.0);
        if let Some(snapshot) = self.cached_snapshot(&cache_key, interval) {
            return snapshot;
        }

        let snapshot = match spec {
            ModuleSpec::Clock(spec) => clock_snapshot(spec),
            ModuleSpec::Command(spec) => command_snapshot(spec),
            ModuleSpec::GitHub(spec) => github_snapshot(spec),
            ModuleSpec::Usage(spec) => usage_snapshot(spec),
            ModuleSpec::Cpu(spec) => cpu_snapshot(spec),
            ModuleSpec::Memory(spec) => memory_snapshot(spec),
            ModuleSpec::Gpu(spec) => gpu_snapshot(spec),
        };

        self.cache.lock().expect("snapshot cache").insert(
            cache_key,
            CachedModule {
                snapshot: snapshot.clone(),
                updated_at: SystemTime::now(),
            },
        );
        snapshot
    }

    fn cached_snapshot(&self, key: &str, interval: Duration) -> Option<ModuleSnapshot> {
        let cached = self.cache.lock().expect("snapshot cache").get(key)?.clone();
        let age = SystemTime::now()
            .duration_since(cached.updated_at)
            .unwrap_or_default();
        (age < interval).then_some(cached.snapshot)
    }
}

fn module_id_interval(spec: &ModuleSpec) -> (&str, Duration) {
    (spec.id(), spec.poll_interval().unwrap_or(Duration::ZERO))
}

fn command_snapshot(spec: &CommandSpec) -> ModuleSnapshot {
    let output = run_shell_command(&spec.exec, &[]);
    match output {
        Ok(text) => {
            let trimmed = text.trim();
            json_snapshot(trimmed, &spec.id, &spec.id, spec.interval).unwrap_or_else(|| {
                ModuleSnapshot {
                    id: spec.id.clone(),
                    title: spec.id.clone(),
                    value: ModuleValue::Text(first_line(trimmed)),
                    status: ModuleStatus::Ok,
                    updated_at: Some(SystemTime::now()),
                    stale_after: Some(spec.interval),
                }
            })
        }
        Err(err) => command_error_snapshot(spec, err),
    }
}

fn usage_snapshot(spec: &UsageSpec) -> ModuleSnapshot {
    if spec.source == "codex" {
        return codex_usage_snapshot(spec);
    }
    if spec.source == "claude" {
        return claude_usage_snapshot(spec);
    }

    let command = usage_command(&spec.source);
    let output = run_shell_command(&command, &[]);
    match output {
        Ok(text) => {
            let trimmed = text.trim();
            json_snapshot(trimmed, &spec.id, &spec.source, spec.interval).unwrap_or_else(|| {
                ModuleSnapshot {
                    id: spec.id.clone(),
                    title: spec.source.clone(),
                    value: ModuleValue::Text(first_line(trimmed)),
                    status: ModuleStatus::Ok,
                    updated_at: Some(SystemTime::now()),
                    stale_after: Some(spec.interval),
                }
            })
        }
        Err(err) => usage_error_snapshot(spec, &command, err),
    }
}

// ---- Local system metrics (CPU / memory / GPU) ----
//
// All read directly from `/proc` and `/sys` with no subprocess or crate
// dependency. They run on the same worker threads as every other polled
// module; CPU utilization needs a delta, which `read_cpu_util` takes by
// sampling `/proc/stat` twice around a short sleep entirely within the worker.

/// Build a gauge snapshot for a local system metric. `headline_percent` drives
/// the status colour through the same thresholds as quota usage, so a saturated
/// CPU/RAM/GPU meter reads warning/critical without a quota of its own.
fn system_snapshot(
    id: &str,
    title: &str,
    gauges: Vec<Gauge>,
    detail: Option<String>,
    headline_percent: f64,
    interval: Duration,
) -> ModuleSnapshot {
    ModuleSnapshot {
        id: id.to_string(),
        title: title.to_string(),
        value: ModuleValue::Gauges(GaugeGroup { gauges, detail }),
        status: usage_status(headline_percent),
        updated_at: Some(SystemTime::now()),
        stale_after: Some(interval),
    }
}

fn system_error(id: &str, title: &str, err: String, interval: Duration) -> ModuleSnapshot {
    ModuleSnapshot {
        id: id.to_string(),
        title: title.to_string(),
        value: ModuleValue::State {
            label: "unavailable".into(),
            detail: Some(first_line(&err)),
        },
        status: ModuleStatus::Warning,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(interval),
    }
}

fn cpu_snapshot(spec: &CpuSpec) -> ModuleSnapshot {
    let util = match read_cpu_util() {
        Ok(util) => util,
        Err(err) => return system_error(&spec.id, "cpu", err, spec.interval),
    };
    let cores = thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);
    let load1 = read_loadavg().unwrap_or(0.0);
    // Load relative to core count: 100% means every core has one runnable task
    // on average; above that the machine is oversubscribed (common mid-build).
    let load_pct = load1 / cores * 100.0;

    let gauges = vec![gauge("util", util), gauge("load", load_pct)];
    let mut details = Vec::new();
    if let Some(temp) = cpu_temp_celsius() {
        details.push(format!("{temp:.0}°C"));
    }
    details.push(format!("load {load1:.2}"));
    let detail = Some(details.join(" · "));

    system_snapshot(
        &spec.id,
        "cpu",
        gauges,
        detail,
        util.max(load_pct),
        spec.interval,
    )
}

fn memory_snapshot(spec: &MemorySpec) -> ModuleSnapshot {
    let mem = match read_meminfo() {
        Ok(mem) => mem,
        Err(err) => return system_error(&spec.id, "memory", err, spec.interval),
    };
    let used_kb = mem.total_kb.saturating_sub(mem.available_kb);
    let ram_pct = percent_of(used_kb, mem.total_kb);
    let swap_used_kb = mem.swap_total_kb.saturating_sub(mem.swap_free_kb);
    let swap_pct = percent_of(swap_used_kb, mem.swap_total_kb);

    let gauges = vec![gauge("ram", ram_pct), gauge("swap", swap_pct)];
    let detail = Some(format!(
        "{:.0} / {:.0} GiB",
        kb_to_gib(used_kb),
        kb_to_gib(mem.total_kb)
    ));

    system_snapshot(
        &spec.id,
        "memory",
        gauges,
        detail,
        ram_pct.max(swap_pct),
        spec.interval,
    )
}

fn gpu_snapshot(spec: &GpuSpec) -> ModuleSnapshot {
    let title = format!("gpu{}", spec.card);
    let base = PathBuf::from(format!("/sys/class/drm/card{}/device", spec.card));

    let busy = match read_u64(base.join("gpu_busy_percent")) {
        Some(busy) => busy as f64,
        None => {
            return system_error(
                &spec.id,
                &title,
                format!("no amdgpu card{}", spec.card),
                spec.interval,
            )
        }
    };

    let mut gauges = vec![gauge("busy", busy)];
    if let (Some(used), Some(total)) = (
        read_u64(base.join("mem_info_vram_used")),
        read_u64(base.join("mem_info_vram_total")),
    ) {
        gauges.push(gauge("vram", percent_of(used, total)));
    }

    let hwmon = amdgpu_hwmon(&base);
    // Power draw is only a gauge when the card reports both draw and its cap;
    // integrated parts expose neither, so the meter simply doesn't appear.
    let watts = hwmon
        .as_ref()
        .and_then(|hwmon| read_u64(hwmon.join("power1_average")));
    if let (Some(avg), Some(cap)) = (
        watts,
        hwmon
            .as_ref()
            .and_then(|hwmon| read_u64(hwmon.join("power1_cap"))),
    ) {
        gauges.push(gauge("power", percent_of(avg, cap)));
    }

    let mut details = Vec::new();
    if let Some(milli) = hwmon
        .as_ref()
        .and_then(|hwmon| read_f64(hwmon.join("temp1_input")))
    {
        details.push(format!("{:.0}°C", milli / 1000.0));
    }
    if let Some(avg) = watts {
        details.push(format!("{}W", avg / 1_000_000));
    }
    let detail = (!details.is_empty()).then(|| details.join(" · "));

    system_snapshot(&spec.id, &title, gauges, detail, busy, spec.interval)
}

/// CPU utilization over a short window, sampling `/proc/stat` twice. Returns a
/// 0–100 percentage of non-idle jiffies across the delta.
fn read_cpu_util() -> Result<f64, String> {
    let (busy1, total1) = read_proc_stat_totals()?;
    thread::sleep(Duration::from_millis(200));
    let (busy2, total2) = read_proc_stat_totals()?;
    let total = total2.saturating_sub(total1);
    if total == 0 {
        return Ok(0.0);
    }
    let busy = busy2.saturating_sub(busy1);
    Ok((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

fn read_proc_stat_totals() -> Result<(u64, u64), String> {
    let text = std::fs::read_to_string("/proc/stat").map_err(|err| err.to_string())?;
    parse_proc_stat_totals(&text)
}

/// `(busy_jiffies, total_jiffies)` from the aggregate `cpu` line of
/// `/proc/stat`. Fields: user nice system idle iowait irq softirq steal …;
/// idle is `idle + iowait`.
fn parse_proc_stat_totals(text: &str) -> Result<(u64, u64), String> {
    let line = text
        .lines()
        .next()
        .filter(|line| line.starts_with("cpu "))
        .ok_or("missing cpu line in /proc/stat")?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 4 {
        return Err("malformed cpu line in /proc/stat".into());
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Ok((total.saturating_sub(idle), total))
}

fn read_loadavg() -> Result<f64, String> {
    let text = std::fs::read_to_string("/proc/loadavg").map_err(|err| err.to_string())?;
    text.split_whitespace()
        .next()
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| "malformed /proc/loadavg".into())
}

struct MemInfo {
    total_kb: u64,
    available_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
}

fn read_meminfo() -> Result<MemInfo, String> {
    let text = std::fs::read_to_string("/proc/meminfo").map_err(|err| err.to_string())?;
    parse_meminfo(&text)
}

fn parse_meminfo(text: &str) -> Result<MemInfo, String> {
    let mut total = None;
    let mut available = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        match key {
            "MemTotal" => total = value,
            "MemAvailable" => available = value,
            "SwapTotal" => swap_total = value,
            "SwapFree" => swap_free = value,
            _ => {}
        }
    }
    Ok(MemInfo {
        total_kb: total.ok_or("missing MemTotal")?,
        available_kb: available.ok_or("missing MemAvailable")?,
        swap_total_kb: swap_total.unwrap_or(0),
        swap_free_kb: swap_free.unwrap_or(0),
    })
}

/// The `amdgpu` hwmon directory for a DRM card, where temperature and power
/// live (e.g. `…/device/hwmon/hwmon3`).
fn amdgpu_hwmon(base: &Path) -> Option<PathBuf> {
    let dir = std::fs::read_dir(base.join("hwmon")).ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        if read_trimmed(path.join("name")).as_deref() == Some("amdgpu") {
            return Some(path);
        }
    }
    None
}

fn cpu_temp_celsius() -> Option<f64> {
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        // k10temp's temp1 is Tctl (AMD); coretemp's is the package (Intel).
        if let Some("k10temp" | "coretemp") = read_trimmed(path.join("name")).as_deref() {
            if let Some(milli) = read_f64(path.join("temp1_input")) {
                return Some(milli / 1000.0);
            }
        }
    }
    None
}

fn percent_of(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

fn kb_to_gib(kb: u64) -> f64 {
    kb as f64 / (1024.0 * 1024.0)
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path).and_then(|text| text.parse().ok())
}

fn read_f64(path: impl AsRef<Path>) -> Option<f64> {
    read_trimmed(path).and_then(|text| text.parse().ok())
}

fn codex_usage_snapshot(spec: &UsageSpec) -> ModuleSnapshot {
    match fetch_codex_usage(spec) {
        Ok(usage) => usage.into_snapshot(spec),
        Err(err) => ModuleSnapshot {
            id: spec.id.clone(),
            title: usage_title(spec),
            value: ModuleValue::State {
                label: "error".into(),
                detail: Some(first_line(&err)),
            },
            status: ModuleStatus::Warning,
            updated_at: Some(SystemTime::now()),
            stale_after: Some(spec.interval),
        },
    }
}

fn claude_usage_snapshot(spec: &UsageSpec) -> ModuleSnapshot {
    match fetch_claude_usage(spec) {
        Ok(usage) => usage.into_snapshot(spec),
        Err(err) => ModuleSnapshot {
            id: spec.id.clone(),
            title: usage_title(spec),
            value: ModuleValue::State {
                label: "error".into(),
                detail: Some(first_line(&err)),
            },
            status: ModuleStatus::Warning,
            updated_at: Some(SystemTime::now()),
            stale_after: Some(spec.interval),
        },
    }
}

fn github_snapshot(spec: &GitHubSpec) -> ModuleSnapshot {
    let mut endpoint = match &spec.workflow {
        Some(workflow) if workflow_is_api_id(workflow) => format!(
            "repos/{}/actions/workflows/{}/runs?per_page=1",
            spec.repo, workflow
        ),
        Some(_) => format!("repos/{}/actions/runs?per_page=10", spec.repo),
        None => format!("repos/{}/actions/runs?per_page=1", spec.repo),
    };
    if let Some(branch) = &spec.branch {
        endpoint.push_str("&branch=");
        endpoint.push_str(branch);
    }

    let mut envs = Vec::new();
    if let Some(token_env) = &spec.token_env {
        if let Ok(token) = std::env::var(token_env) {
            envs.push(("GH_TOKEN", token));
        }
    }

    match run_command("gh", &["api", &endpoint], &envs) {
        Ok(text) => parse_github_runs(spec, &text).unwrap_or_else(|| {
            error_snapshot(&spec.id, github_title(spec), "no workflow runs", spec.interval)
        }),
        Err(err) => error_snapshot(&spec.id, github_title(spec), err, spec.interval),
    }
}

/// Display title for a github module: the configured `title`, else the repo.
fn github_title(spec: &GitHubSpec) -> &str {
    spec.title.as_deref().unwrap_or(&spec.repo)
}

fn parse_github_runs(spec: &GitHubSpec, text: &str) -> Option<ModuleSnapshot> {
    let json: Value = serde_json::from_str(text).ok()?;
    let runs = json.get("workflow_runs")?.as_array()?;
    let run = match &spec.workflow {
        Some(workflow) if !workflow_is_api_id(workflow) => runs
            .iter()
            .find(|run| run.get("name").and_then(Value::as_str) == Some(workflow.as_str()))
            .or_else(|| runs.first())?,
        _ => runs.first()?,
    };
    let status = run
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let conclusion = run.get("conclusion").and_then(Value::as_str);
    let branch = run
        .get("head_branch")
        .and_then(Value::as_str)
        .or(spec.branch.as_deref());
    let workflow = run
        .get("name")
        .and_then(Value::as_str)
        .or(spec.workflow.as_deref());

    let (label, module_status) = match (status, conclusion) {
        ("completed", Some("success")) => ("success".to_string(), ModuleStatus::Ok),
        ("completed", Some("failure" | "timed_out" | "action_required")) => (
            conclusion.unwrap_or("failure").to_string(),
            ModuleStatus::Critical,
        ),
        ("completed", Some("cancelled" | "skipped" | "neutral")) => (
            conclusion.unwrap_or("completed").to_string(),
            ModuleStatus::Warning,
        ),
        ("completed", Some(other)) => (other.to_string(), ModuleStatus::Warning),
        ("in_progress" | "queued" | "requested" | "waiting" | "pending", _) => {
            (status.replace('_', " "), ModuleStatus::Info)
        }
        (other, _) => (other.replace('_', " "), ModuleStatus::Unknown),
    };

    let detail = match (workflow, branch) {
        (Some(workflow), Some(branch)) => Some(format!("{workflow} @ {branch}")),
        (Some(workflow), None) => Some(workflow.to_string()),
        (None, Some(branch)) => Some(branch.to_string()),
        (None, None) => None,
    };

    Some(ModuleSnapshot {
        id: spec.id.clone(),
        title: github_title(spec).to_string(),
        value: ModuleValue::State { label, detail },
        status: module_status,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(spec.interval),
    })
}

fn workflow_is_api_id(workflow: &str) -> bool {
    workflow.bytes().all(|byte| byte.is_ascii_digit())
        || workflow.ends_with(".yml")
        || workflow.ends_with(".yaml")
}

fn json_snapshot(text: &str, id: &str, title: &str, interval: Duration) -> Option<ModuleSnapshot> {
    let json: Value = serde_json::from_str(text).ok()?;
    let status = json_status(&json);
    let value = json_value(&json)?;
    let title = json
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(title)
        .to_string();

    Some(ModuleSnapshot {
        id: json
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        title,
        value,
        status,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(interval),
    })
}

fn json_value(json: &Value) -> Option<ModuleValue> {
    if let Some(value) = json.get("value").and_then(Value::as_str) {
        return Some(ModuleValue::Text(value.to_string()));
    }
    if let Some(text) = json.get("text").and_then(Value::as_str) {
        return Some(ModuleValue::Text(text.to_string()));
    }
    if let Some(label) = json.get("label").and_then(Value::as_str) {
        return Some(ModuleValue::State {
            label: label.to_string(),
            detail: json
                .get("detail")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        });
    }
    if let Some(percent) = json.get("percent").and_then(Value::as_f64) {
        return Some(ModuleValue::Percent(
            (percent as f32 / 100.0).clamp(0.0, 1.0),
        ));
    }
    if let Some(fraction) = json.get("fraction").and_then(Value::as_f64) {
        return Some(ModuleValue::Percent((fraction as f32).clamp(0.0, 1.0)));
    }

    let current = json
        .get("current")
        .or_else(|| json.get("used"))
        .and_then(Value::as_u64);
    let total = json
        .get("total")
        .or_else(|| json.get("limit"))
        .and_then(Value::as_u64);
    current.map(|current| ModuleValue::Count {
        current: current.min(u32::MAX as u64) as u32,
        total: total.map(|total| total.min(u32::MAX as u64) as u32),
    })
}

fn json_status(json: &Value) -> ModuleStatus {
    match json.get("status").and_then(Value::as_str).unwrap_or("ok") {
        "ok" | "success" | "healthy" => ModuleStatus::Ok,
        "info" | "running" | "pending" => ModuleStatus::Info,
        "warn" | "warning" | "degraded" => ModuleStatus::Warning,
        "critical" | "crit" | "error" | "failed" | "failure" => ModuleStatus::Critical,
        _ => ModuleStatus::Unknown,
    }
}

fn usage_command(source: &str) -> String {
    if source.contains(char::is_whitespace) || source.ends_with("-usage-json") {
        source.to_string()
    } else {
        format!("{source}-usage-json")
    }
}

#[derive(Debug)]
struct ClaudeUsage {
    plan: Option<String>,
    five_hour: Option<ClaudeUsageWindow>,
    seven_day: Option<ClaudeUsageWindow>,
    sonnet: Option<ClaudeUsageWindow>,
    opus: Option<ClaudeUsageWindow>,
}

#[derive(Debug)]
struct ClaudeUsageWindow {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Debug)]
struct CodexUsage {
    plan: Option<String>,
    primary: Option<CodexUsageWindow>,
    secondary: Option<CodexUsageWindow>,
    credits: Option<CodexUsageCredits>,
}

#[derive(Debug)]
struct CodexUsageWindow {
    used_percent: f64,
    window_minutes: Option<u64>,
    resets_at_epoch: Option<i64>,
}

#[derive(Debug)]
struct CodexUsageCredits {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

impl ClaudeUsage {
    fn into_snapshot(self, spec: &UsageSpec) -> ModuleSnapshot {
        let highest = [
            self.five_hour.as_ref().map(|window| window.utilization),
            self.seven_day.as_ref().map(|window| window.utilization),
            self.sonnet_pct(),
            self.opus_pct(),
        ]
        .into_iter()
        .flatten()
        .fold(0.0, f64::max);

        let mut gauges = Vec::new();
        if let Some(window) = &self.five_hour {
            gauges.push(gauge("5h", window.utilization));
        }
        if let Some(window) = &self.seven_day {
            gauges.push(gauge("7d", window.utilization));
        }

        let mut details = Vec::new();
        if let Some(plan) = self.plan {
            details.push(plan);
        }
        if let Some(reset) = self
            .seven_day
            .as_ref()
            .and_then(|window| window.resets_at.as_deref())
            .and_then(format_reset_time)
        {
            details.push(format!("resets {reset}"));
        }
        let detail = (!details.is_empty()).then(|| details.join(" · "));

        ModuleSnapshot {
            id: spec.id.clone(),
            title: usage_title(spec),
            value: ModuleValue::Gauges(GaugeGroup { gauges, detail }),
            status: usage_status(highest),
            updated_at: Some(SystemTime::now()),
            stale_after: Some(spec.interval),
        }
    }

    fn sonnet_pct(&self) -> Option<f64> {
        self.sonnet.as_ref().map(|window| window.utilization)
    }

    fn opus_pct(&self) -> Option<f64> {
        self.opus.as_ref().map(|window| window.utilization)
    }
}

impl CodexUsage {
    fn into_snapshot(self, spec: &UsageSpec) -> ModuleSnapshot {
        let highest = [
            self.primary.as_ref().map(|window| window.used_percent),
            self.secondary.as_ref().map(|window| window.used_percent),
        ]
        .into_iter()
        .flatten()
        .fold(0.0, f64::max);

        let mut gauges = Vec::new();
        if let Some(window) = &self.primary {
            gauges.push(gauge(window_label(window), window.used_percent));
        }
        if let Some(window) = &self.secondary {
            gauges.push(gauge(window_label(window), window.used_percent));
        }

        let mut details = Vec::new();
        if let Some(plan) = self.plan {
            details.push(plan);
        }
        if let Some(credits) = self.credits.as_ref().and_then(format_codex_credits) {
            details.push(credits);
        }
        if let Some(reset) = self
            .secondary
            .as_ref()
            .and_then(|window| window.resets_at_epoch)
            .and_then(format_epoch_reset_time)
        {
            details.push(format!("resets {reset}"));
        }
        let detail = (!details.is_empty()).then(|| details.join(" · "));

        ModuleSnapshot {
            id: spec.id.clone(),
            title: usage_title(spec),
            value: ModuleValue::Gauges(GaugeGroup { gauges, detail }),
            status: usage_status(highest),
            updated_at: Some(SystemTime::now()),
            stale_after: Some(spec.interval),
        }
    }
}

fn fetch_codex_usage(spec: &UsageSpec) -> Result<CodexUsage, String> {
    let mut auth = CodexAuthState::load(spec)?;
    auth.ensure_fresh()?;
    let account_header_sent = auth.account_id().is_some();
    let mut request = HTTP
        .get(CODEX_WHAM_USAGE_URL)
        .set("User-Agent", "prism-widgets")
        .set("Authorization", &format!("Bearer {}", auth.access_token()));
    if let Some(account_id) = auth.account_id() {
        request = request.set("chatgpt-account-id", account_id);
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(401, _)) => return Err("token expired; run codex login".into()),
        Err(ureq::Error::Status(429, response)) => {
            let detail = response
                .header("retry-after")
                .map(|seconds| format!("rate limited, retry after {seconds}s"))
                .unwrap_or_else(|| "rate limited".into());
            return Err(detail);
        }
        Err(ureq::Error::Status(status, _)) => return Err(format!("API error ({status})")),
        Err(err) => return Err(err.to_string()),
    };

    let header_usage = parse_codex_usage_headers(&response);
    let body = response.into_string().map_err(|err| err.to_string())?;
    if let Some(usage) =
        parse_codex_usage_response(&body).map_err(|err| format!("invalid usage response: {err}"))?
    {
        return Ok(usage);
    }
    header_usage.ok_or_else(|| {
        let summary = summarize_codex_usage_body(&body, account_header_sent);
        tracing::warn!(
            target: "prism_widgets",
            body_summary = %summary,
            account_header_sent,
            "codex usage response did not contain recognized usage fields"
        );
        summary
    })
}

fn parse_codex_usage_response(body: &str) -> Result<Option<CodexUsage>, serde_json::Error> {
    let json: Value = serde_json::from_str(body)?;
    if let Some(usage) = parse_codex_rate_limit_status_payload(&json) {
        return Ok(Some(usage));
    }
    if let Some(usage) = json
        .get("rateLimits")
        .and_then(parse_codex_rate_limits_object)
    {
        return Ok(Some(usage));
    }
    if let Some(usage) = json
        .get("rateLimitsByLimitId")
        .and_then(parse_codex_rate_limits_by_id)
    {
        return Ok(Some(usage));
    }
    if let Some(usage) = parse_codex_usage_nested(&json, 4) {
        return Ok(Some(usage));
    }
    Ok(None)
}

fn parse_codex_usage_headers(response: &ureq::Response) -> Option<CodexUsage> {
    let primary = codex_header_window(response, "x-codex-primary");
    let secondary = codex_header_window(response, "x-codex-secondary");
    let credits = codex_header_credits(response);
    if primary.is_none() && secondary.is_none() && credits.is_none() {
        return None;
    }
    Some(CodexUsage {
        plan: None,
        primary,
        secondary,
        credits,
    })
}

fn parse_codex_rate_limits_by_id(json: &Value) -> Option<CodexUsage> {
    let object = json.as_object()?;
    let preferred = object
        .iter()
        .find(|(key, value)| {
            key.eq_ignore_ascii_case("codex")
                || value
                    .get("limitId")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case("codex"))
        })
        .and_then(|(_, value)| parse_codex_rate_limits_object(value));
    preferred.or_else(|| object.values().find_map(parse_codex_rate_limits_object))
}

fn parse_codex_rate_limit_status_payload(json: &Value) -> Option<CodexUsage> {
    let primary = json
        .pointer("/rate_limit/primary_window")
        .and_then(parse_codex_usage_window);
    let secondary = json
        .pointer("/rate_limit/secondary_window")
        .and_then(parse_codex_usage_window);
    let credits = codex_usage_credits(json);
    let plan = codex_plan_type(json);
    if primary.is_none() && secondary.is_none() && credits.is_none() && plan.is_none() {
        return None;
    }
    Some(CodexUsage {
        plan,
        primary,
        secondary,
        credits,
    })
}

fn parse_codex_usage_nested(json: &Value, depth: usize) -> Option<CodexUsage> {
    if depth == 0 {
        return None;
    }
    match json {
        Value::Object(object) => {
            if object.contains_key("primary")
                || object.contains_key("secondary")
                || object.contains_key("credits")
            {
                if let Some(usage) = parse_codex_rate_limits_object(json) {
                    if usage.primary.is_some()
                        || usage.secondary.is_some()
                        || usage.credits.is_some()
                    {
                        return Some(usage);
                    }
                }
            }
            object
                .values()
                .find_map(|value| parse_codex_usage_nested(value, depth - 1))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| parse_codex_usage_nested(value, depth - 1)),
        _ => None,
    }
}

fn parse_codex_rate_limits_object(rate_limits: &Value) -> Option<CodexUsage> {
    let primary = codex_usage_window(rate_limits, "primary");
    let secondary = codex_usage_window(rate_limits, "secondary");
    let credits = codex_usage_credits(rate_limits);
    let plan = codex_plan_type(rate_limits);
    if primary.is_none() && secondary.is_none() && credits.is_none() && plan.is_none() {
        return None;
    }
    Some(CodexUsage {
        plan,
        primary,
        secondary,
        credits,
    })
}

fn summarize_codex_usage_body(body: &str, account_header_sent: bool) -> String {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return "no usage fields returned".into();
    };
    let Some(object) = json.as_object() else {
        return "no usage fields returned".into();
    };
    let summary = summarize_json_object(object, 2);
    if summary.is_empty() {
        "no usage fields returned".into()
    } else {
        let account = if account_header_sent {
            "account header sent"
        } else {
            "no account header"
        };
        format!("no usage fields; {account}; {summary}")
    }
}

fn summarize_json_object(object: &serde_json::Map<String, Value>, depth: usize) -> String {
    object
        .iter()
        .take(8)
        .map(|(key, value)| summarize_json_entry(key, value, depth))
        .collect::<Vec<_>>()
        .join(", ")
}

fn summarize_json_entry(key: &str, value: &Value, depth: usize) -> String {
    if depth == 0 {
        return key.to_string();
    }
    match value {
        Value::Object(object) if !object.is_empty() => {
            format!("{key}{{{}}}", summarize_json_object(object, depth - 1))
        }
        Value::Array(values) => {
            let Some(Value::Object(object)) = values.first() else {
                return format!("{key}[{}]", values.len());
            };
            format!(
                "{key}[{}]{{{}}}",
                values.len(),
                summarize_json_object(object, depth - 1)
            )
        }
        _ => key.to_string(),
    }
}

fn codex_header_window(response: &ureq::Response, prefix: &str) -> Option<CodexUsageWindow> {
    Some(CodexUsageWindow {
        used_percent: codex_header_f64(response, &format!("{prefix}-used-percent"))?,
        window_minutes: codex_header_u64(response, &format!("{prefix}-window-minutes")),
        resets_at_epoch: codex_header_i64(response, &format!("{prefix}-reset-at")),
    })
}

fn codex_header_credits(response: &ureq::Response) -> Option<CodexUsageCredits> {
    let has_credits = codex_header_bool(response, "x-codex-credits-has-credits");
    let unlimited = codex_header_bool(response, "x-codex-credits-unlimited");
    let balance = response
        .header("x-codex-credits-balance")
        .map(ToOwned::to_owned);
    if has_credits.is_none() && unlimited.is_none() && balance.is_none() {
        return None;
    }
    Some(CodexUsageCredits {
        has_credits: has_credits.unwrap_or(false),
        unlimited: unlimited.unwrap_or(false),
        balance,
    })
}

fn codex_header_f64(response: &ureq::Response, name: &str) -> Option<f64> {
    response.header(name).and_then(|value| value.parse().ok())
}

fn codex_header_u64(response: &ureq::Response, name: &str) -> Option<u64> {
    response.header(name).and_then(|value| value.parse().ok())
}

fn codex_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|value| value.parse().ok())
}

fn codex_header_bool(response: &ureq::Response, name: &str) -> Option<bool> {
    match response.header(name)? {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn fetch_claude_usage(spec: &UsageSpec) -> Result<ClaudeUsage, String> {
    let (auth, plan) = claude_auth(spec)?;
    let base_url = spec
        .base_url
        .as_deref()
        .unwrap_or(CLAUDE_USAGE_BASE_URL)
        .trim_end_matches('/');
    let url = format!("{base_url}/api/oauth/usage");
    let mut request = HTTP
        .get(&url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "prism-widgets")
        .set("anthropic-beta", CLAUDE_USAGE_BETA);

    match auth {
        ClaudeAuth::Oauth(token) => {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        ClaudeAuth::ApiKey(key) => {
            request = request.set("x-api-key", &key);
        }
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(401, _)) if spec.base_url.is_none() => {
            return Err("token expired".into());
        }
        Err(ureq::Error::Status(401, _)) => return Err("invalid API key".into()),
        Err(ureq::Error::Status(404, _)) => return Err("endpoint not found".into()),
        Err(ureq::Error::Status(429, response)) => {
            let detail = response
                .header("retry-after")
                .map(|seconds| format!("rate limited, retry after {seconds}s"))
                .unwrap_or_else(|| "rate limited".into());
            return Err(detail);
        }
        Err(ureq::Error::Status(status, _)) => return Err(format!("API error ({status})")),
        Err(err) => return Err(err.to_string()),
    };

    let text = response.into_string().map_err(|err| err.to_string())?;
    let json: Value = serde_json::from_str(&text).map_err(|err| format!("invalid JSON: {err}"))?;
    Ok(ClaudeUsage {
        plan,
        five_hour: claude_window(&json, "five_hour"),
        seven_day: claude_window(&json, "seven_day"),
        sonnet: claude_window(&json, "seven_day_sonnet"),
        opus: claude_window(&json, "seven_day_opus"),
    })
}

struct CodexAuthState {
    path: PathBuf,
    json: Value,
    exp: Option<i64>,
    account_id: Option<String>,
}

impl CodexAuthState {
    fn load(spec: &UsageSpec) -> Result<Self, String> {
        let path = codex_auth_path(spec)?;
        let text = std::fs::read_to_string(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                "not logged in".to_string()
            } else {
                format!("{}: {err}", path.display())
            }
        })?;
        let json: Value = serde_json::from_str(&text)
            .map_err(|err| format!("invalid codex auth file {}: {err}", path.display()))?;
        let access_token = token_str(&json, "access_token")?;
        let exp = parse_jwt_i64_claim(access_token, "exp").ok().flatten();
        let account_id = token_str(&json, "account_id")
            .ok()
            .map(ToOwned::to_owned)
            .or_else(|| {
                token_str(&json, "id_token")
                    .ok()
                    .and_then(parse_chatgpt_account_id)
            });
        Ok(Self {
            path,
            json,
            exp,
            account_id,
        })
    }

    fn access_token(&self) -> &str {
        token_str(&self.json, "access_token").expect("validated access token")
    }

    fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    fn ensure_fresh(&mut self) -> Result<(), String> {
        if !self.is_expired() {
            return Ok(());
        }
        self.refresh()
    }

    fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.exp
            .map(|exp| now + CODEX_REFRESH_SAFETY_SECS >= exp)
            .unwrap_or(true)
    }

    fn refresh(&mut self) -> Result<(), String> {
        let refresh_token = token_str(&self.json, "refresh_token")?.to_string();
        let body = serde_json::json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        let response = match HTTP
            .post(CODEX_REFRESH_TOKEN_URL)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(format!(
                    "codex refresh returned {status}: {body}; run codex login"
                ));
            }
            Err(err) => return Err(format!("codex refresh: {err}")),
        };
        let text = response
            .into_string()
            .map_err(|err| format!("codex refresh: {err}"))?;
        let fresh: Value =
            serde_json::from_str(&text).map_err(|err| format!("codex refresh JSON: {err}"))?;
        for key in ["id_token", "access_token", "refresh_token"] {
            if let Some(value) = fresh.get(key).and_then(Value::as_str) {
                self.json["tokens"][key] = Value::String(value.to_string());
            }
        }
        if let Ok(access_token) = token_str(&self.json, "access_token") {
            self.exp = parse_jwt_i64_claim(access_token, "exp").ok().flatten();
        }
        if let Ok(id_token) = token_str(&self.json, "id_token") {
            if let Some(account_id) = parse_chatgpt_account_id(id_token) {
                self.account_id = Some(account_id);
            }
        }
        self.json["last_refresh"] = Value::String(chrono::Utc::now().to_rfc3339());
        let text = serde_json::to_string_pretty(&self.json)
            .map_err(|err| format!("serialize codex auth: {err}"))?;
        std::fs::write(&self.path, text)
            .map_err(|err| format!("write codex auth file {}: {err}", self.path.display()))?;
        Ok(())
    }
}

fn codex_auth_path(spec: &UsageSpec) -> Result<PathBuf, String> {
    if let Some(path) = &spec.auth_path {
        return expand_home(path);
    }
    let home = spec.codex_home.as_deref().unwrap_or("$HOME/.codex");
    Ok(expand_home(home)?.join("auth.json"))
}

fn token_str<'a>(json: &'a Value, key: &str) -> Result<&'a str, String> {
    json.pointer(&format!("/tokens/{key}"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("codex auth file has no tokens.{key}; run codex login"))
}

fn parse_jwt_payload(jwt: &str) -> Result<Value, String> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| "invalid JWT: missing payload".to_string())?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|err| format!("invalid JWT payload: {err}"))?;
    serde_json::from_slice(&bytes).map_err(|err| format!("invalid JWT JSON: {err}"))
}

fn parse_jwt_i64_claim(jwt: &str, claim: &str) -> Result<Option<i64>, String> {
    Ok(parse_jwt_payload(jwt)?.get(claim).and_then(Value::as_i64))
}

fn parse_chatgpt_account_id(jwt: &str) -> Option<String> {
    let payload = parse_jwt_payload(jwt).ok()?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| payload.get("https://api.openai.com/auth.chatgpt_account_id"))
        .or_else(|| payload.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn codex_usage_window(json: &Value, key: &str) -> Option<CodexUsageWindow> {
    parse_codex_usage_window(json.get(key)?)
}

fn parse_codex_usage_window(window: &Value) -> Option<CodexUsageWindow> {
    Some(CodexUsageWindow {
        used_percent: codex_json_f64(window, &["usedPercent", "used_percent"])?,
        window_minutes: codex_window_minutes(window),
        resets_at_epoch: codex_json_i64(window, &["resetsAt", "reset_at"]),
    })
}

fn codex_usage_credits(json: &Value) -> Option<CodexUsageCredits> {
    let credits = json.get("credits")?;
    let has_credits = credits.get("hasCredits").and_then(Value::as_bool);
    let unlimited = credits.get("unlimited").and_then(Value::as_bool);
    let balance = credits
        .get("balance")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if has_credits.is_none() && unlimited.is_none() && balance.is_none() {
        return None;
    }
    Some(CodexUsageCredits {
        has_credits: has_credits.unwrap_or(false),
        unlimited: unlimited.unwrap_or(false),
        balance,
    })
}

fn codex_plan_type(json: &Value) -> Option<String> {
    json.get("planType")
        .or_else(|| json.get("plan_type"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn codex_window_minutes(window: &Value) -> Option<u64> {
    codex_json_u64(window, &["windowDurationMins", "window_minutes"]).or_else(|| {
        let seconds = codex_json_u64(window, &["limitWindowSeconds", "limit_window_seconds"])?;
        Some(seconds.saturating_add(59) / 60)
    })
}

fn codex_json_f64(json: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = json.get(*key)?;
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|n| n as f64))
            .or_else(|| value.as_u64().map(|n| n as f64))
    })
}

fn codex_json_i64(json: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        let value = json.get(*key)?;
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
    })
}

fn codex_json_u64(json: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| json.get(*key)?.as_u64())
}

enum ClaudeAuth {
    Oauth(String),
    ApiKey(String),
}

fn claude_auth(spec: &UsageSpec) -> Result<(ClaudeAuth, Option<String>), String> {
    if let Some(api_key_env) = &spec.api_key_env {
        return std::env::var(api_key_env)
            .map(|key| {
                (
                    ClaudeAuth::ApiKey(key),
                    claude_plan_name(spec, Some("API Key")),
                )
            })
            .map_err(|_| format!("{api_key_env} is not set"));
    }
    if spec.base_url.is_some() {
        return Err("api-key-env is required with base-url".into());
    }

    let credentials_path = claude_credentials_path(spec)?;
    let text = std::fs::read_to_string(&credentials_path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            "not logged in".to_string()
        } else {
            format!("{}: {err}", credentials_path.display())
        }
    })?;
    let json: Value =
        serde_json::from_str(&text).map_err(|err| format!("invalid credentials: {err}"))?;
    let token = json
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "not logged in".to_string())?;
    let tier = json
        .pointer("/claudeAiOauth/rateLimitTier")
        .and_then(Value::as_str);
    Ok((
        ClaudeAuth::Oauth(token.to_string()),
        claude_plan_name(spec, tier),
    ))
}

fn claude_credentials_path(spec: &UsageSpec) -> Result<PathBuf, String> {
    let base = spec.claude_dir.as_deref().unwrap_or("$HOME/.claude");
    Ok(expand_home(base)?.join(".credentials.json"))
}

fn claude_plan_name(spec: &UsageSpec, tier: Option<&str>) -> Option<String> {
    spec.account.clone().or_else(|| {
        tier.map(|tier| match tier {
            "default_claude_pro" => "Pro".into(),
            "default_claude_max_5x" => "Max 5x".into(),
            "default_claude_max_20x" => "Max 20x".into(),
            other => other.to_string(),
        })
    })
}

fn claude_window(json: &Value, key: &str) -> Option<ClaudeUsageWindow> {
    let window = json.get(key)?;
    Some(ClaudeUsageWindow {
        utilization: window.get("utilization").and_then(Value::as_f64)?,
        resets_at: window
            .get("resets_at")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn usage_title(spec: &UsageSpec) -> String {
    match (&spec.source[..], spec.account.as_deref()) {
        ("claude", Some(account)) => format!("claude {account}"),
        ("codex", Some(account)) => format!("codex {account}"),
        (_, Some(account)) => format!("{} {account}", spec.source),
        _ => spec.source.clone(),
    }
}

fn usage_status(percent: f64) -> ModuleStatus {
    if percent >= 80.0 {
        ModuleStatus::Critical
    } else if percent >= 50.0 {
        ModuleStatus::Warning
    } else {
        ModuleStatus::Ok
    }
}

fn gauge(label: &str, percent: f64) -> Gauge {
    Gauge {
        label: label.to_string(),
        percent: percent as f32,
    }
}

fn format_reset_time(value: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%a %H:%M")
                .to_string()
        })
}

fn format_epoch_reset_time(value: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(value, 0).map(|time| {
        time.with_timezone(&chrono::Local)
            .format("%a %H:%M")
            .to_string()
    })
}

fn window_label(window: &CodexUsageWindow) -> &'static str {
    match window.window_minutes {
        Some(300) => "5h",
        Some(10080) => "7d",
        _ => "usage",
    }
}

fn format_codex_credits(credits: &CodexUsageCredits) -> Option<String> {
    if credits.unlimited {
        Some("credits unlimited".into())
    } else if let Some(balance) = &credits.balance {
        Some(format!("credits {balance}"))
    } else if credits.has_credits {
        Some("credits enabled".into())
    } else {
        None
    }
}

fn expand_home(path: &str) -> Result<PathBuf, String> {
    if path == "$HOME" || path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("$HOME/") {
        return Ok(home_dir()?.join(rest));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(Path::new(path).to_path_buf())
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "$HOME is not set".into())
}

fn command_error_snapshot(spec: &CommandSpec, err: String) -> ModuleSnapshot {
    let (label, detail) = command_error_value(&spec.exec, &err);
    ModuleSnapshot {
        id: spec.id.clone(),
        title: spec.id.clone(),
        value: ModuleValue::State { label, detail },
        status: ModuleStatus::Warning,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(spec.interval),
    }
}

fn usage_error_snapshot(spec: &UsageSpec, command: &str, err: String) -> ModuleSnapshot {
    let (label, detail) = command_error_value(command, &err);
    ModuleSnapshot {
        id: spec.id.clone(),
        title: spec.source.clone(),
        value: ModuleValue::State { label, detail },
        status: ModuleStatus::Warning,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(spec.interval),
    }
}

fn command_error_value(command: &str, err: &str) -> (String, Option<String>) {
    if shell_command_not_found(err) {
        (
            "unavailable".into(),
            Some(format!("{} not found", command_name(command))),
        )
    } else {
        ("error".into(), Some(first_line(err)))
    }
}

fn shell_command_not_found(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("not found") || lower.contains("command not found")
}

fn command_name(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

fn error_snapshot(
    id: &str,
    title: &str,
    err: impl Into<String>,
    interval: Duration,
) -> ModuleSnapshot {
    let err = err.into();
    ModuleSnapshot {
        id: id.to_string(),
        title: title.to_string(),
        value: ModuleValue::State {
            label: "error".into(),
            detail: Some(first_line(&err)),
        },
        status: ModuleStatus::Warning,
        updated_at: Some(SystemTime::now()),
        stale_after: Some(interval),
    }
}

fn run_shell_command(command: &str, envs: &[(&str, String)]) -> Result<String, String> {
    run_command("timeout", &[COMMAND_TIMEOUT, "sh", "-lc", command], envs)
}

fn run_command(program: &str, args: &[&str], envs: &[(&str, String)]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|err| format!("{program}: {err}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(if message.is_empty() {
            format!("{program} exited {}", output.status)
        } else {
            message.to_string()
        })
    }
}

fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&payload).unwrap());
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{payload}.{signature}")
    }

    #[test]
    fn proc_stat_totals_split_idle_from_busy() {
        // user nice system idle iowait irq softirq steal
        let (busy, total) = parse_proc_stat_totals("cpu  100 0 50 800 40 5 5 0\ncpu0 ...").unwrap();
        // idle = idle(800) + iowait(40) = 840; total = 1000; busy = 160.
        assert_eq!(total, 1000);
        assert_eq!(busy, 160);
    }

    #[test]
    fn proc_stat_totals_rejects_missing_cpu_line() {
        assert!(parse_proc_stat_totals("intr 1 2 3").is_err());
        assert!(parse_proc_stat_totals("cpu 1 2").is_err());
    }

    #[test]
    fn github_title_overrides_repo_else_falls_back() {
        let mut spec = GitHubSpec {
            id: "ci".into(),
            repo: "computer-whisperer/some-very-long-repo-name".into(),
            title: None,
            branch: None,
            workflow: None,
            interval: Duration::from_secs(60),
            token_env: None,
        };
        assert_eq!(github_title(&spec), "computer-whisperer/some-very-long-repo-name");
        spec.title = Some("shortname".into());
        assert_eq!(github_title(&spec), "shortname");
    }

    #[test]
    fn meminfo_reads_ram_and_swap_with_optional_swap() {
        let mem = parse_meminfo(
            "MemTotal:       131795128 kB\n\
             MemFree:          1000000 kB\n\
             MemAvailable:   115207540 kB\n\
             SwapTotal:      134217724 kB\n\
             SwapFree:       134102888 kB\n",
        )
        .unwrap();
        assert_eq!(mem.total_kb, 131_795_128);
        assert_eq!(mem.available_kb, 115_207_540);
        assert_eq!(mem.swap_total_kb, 134_217_724);
        assert_eq!(mem.swap_free_kb, 134_102_888);

        // Swap is optional (swapless hosts); RAM fields are required.
        let no_swap = parse_meminfo("MemTotal: 100 kB\nMemAvailable: 40 kB\n").unwrap();
        assert_eq!(no_swap.swap_total_kb, 0);
        assert!(parse_meminfo("MemFree: 10 kB\n").is_err());
    }

    #[test]
    fn percent_of_guards_zero_whole() {
        assert_eq!(percent_of(0, 0), 0.0);
        assert_eq!(percent_of(1, 4), 25.0);
    }

    #[test]
    fn parses_codex_wham_usage_response() {
        let usage = parse_codex_usage_response(
            r#"{
                "rateLimits": {
                    "planType": "pro",
                    "primary": {
                        "usedPercent": 12.5,
                        "windowDurationMins": 300,
                        "resetsAt": 1770000000
                    },
                    "secondary": {
                        "usedPercent": 64.0,
                        "windowDurationMins": 10080,
                        "resetsAt": 1770500000
                    },
                    "credits": {
                        "hasCredits": true,
                        "unlimited": false,
                        "balance": "42"
                    }
                }
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(usage.plan.as_deref(), Some("pro"));
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 12.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at_epoch, Some(1770000000));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 64.0);
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(secondary.resets_at_epoch, Some(1770500000));
        let credits = usage.credits.unwrap();
        assert!(credits.has_credits);
        assert!(!credits.unlimited);
        assert_eq!(credits.balance.as_deref(), Some("42"));
    }

    #[test]
    fn parses_codex_rate_limit_status_payload() {
        let usage = parse_codex_usage_response(
            r#"{
                "plan_type": "pro",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 42,
                        "limit_window_seconds": 18000,
                        "reset_at": 1770000000
                    },
                    "secondary_window": {
                        "used_percent": 84.0,
                        "limit_window_seconds": 604800,
                        "reset_at": 1770500000
                    }
                },
                "credits": {
                    "hasCredits": true,
                    "unlimited": false,
                    "balance": "9.99"
                }
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(usage.plan.as_deref(), Some("pro"));
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 42.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at_epoch, Some(1770000000));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 84.0);
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(secondary.resets_at_epoch, Some(1770500000));
        let credits = usage.credits.unwrap();
        assert!(credits.has_credits);
        assert!(!credits.unlimited);
        assert_eq!(credits.balance.as_deref(), Some("9.99"));
    }

    #[test]
    fn codex_wham_usage_response_without_user_fields_is_empty() {
        assert!(parse_codex_usage_response(r#"{"other":true}"#)
            .unwrap()
            .is_none());
        assert!(parse_codex_usage_response(r#"{"rateLimits":{}}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn codex_wham_usage_falls_back_to_rate_limits_by_id() {
        let usage = parse_codex_usage_response(
            r#"{
                "rateLimits": { "limitId": "codex" },
                "rateLimitsByLimitId": {
                    "codex": {
                        "plan_type": "plus",
                        "primary": {
                            "used_percent": 9.0,
                            "window_minutes": 300,
                            "reset_at": 1770000000
                        },
                        "secondary": {
                            "used_percent": 31.0,
                            "window_minutes": 10080,
                            "reset_at": 1770500000
                        }
                    }
                }
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(usage.plan.as_deref(), Some("plus"));
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 9.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at_epoch, Some(1770000000));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 31.0);
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(secondary.resets_at_epoch, Some(1770500000));
    }

    #[test]
    fn codex_wham_usage_finds_nested_usage_windows() {
        let usage = parse_codex_usage_response(
            r#"{
                "accounts": [
                    {
                        "email": "ignored@example.com",
                        "usage": {
                            "planType": "team",
                            "primary": {
                                "usedPercent": 22.0,
                                "windowDurationMins": 300
                            }
                        }
                    }
                ]
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(usage.plan.as_deref(), Some("team"));
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 22.0);
        assert_eq!(primary.window_minutes, Some(300));
    }

    #[test]
    fn extracts_codex_jwt_exp_and_account_id() {
        let access_token = jwt(serde_json::json!({ "exp": 1_770_000_000i64 }));
        let id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-123"
            }
        }));

        assert_eq!(
            parse_jwt_i64_claim(&access_token, "exp").unwrap(),
            Some(1_770_000_000)
        );
        assert_eq!(
            parse_chatgpt_account_id(&id_token).as_deref(),
            Some("acc-123")
        );

        let dotted_id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth.chatgpt_account_id": "acc-456"
        }));
        assert_eq!(
            parse_chatgpt_account_id(&dotted_id_token).as_deref(),
            Some("acc-456")
        );
    }
}
