//! Shared data types for prism-widgets.
//!
//! This crate intentionally has no Wayland, GPU, network, or provider
//! dependencies. It is the contract between panel configuration, data
//! providers, the layer-shell host, and Damascene UI projection.

use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelId(pub String);

impl PanelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelLayer {
    Background,
    Bottom,
    Top,
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelAnchor {
    TopLeft,
    Top,
    TopRight,
    BottomLeft,
    Bottom,
    BottomRight,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelLayout {
    Bar,
    Sidebar,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelGeometry {
    pub width: Option<u32>,
    pub height: u32,
    pub margin: i32,
    pub exclusive_zone: i32,
    pub anchor: PanelAnchor,
    pub layer: PanelLayer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelSpec {
    pub id: PanelId,
    pub output: Option<String>,
    pub layout: PanelLayout,
    pub geometry: PanelGeometry,
    pub appearance: PanelAppearance,
    pub modules: Vec<ModuleSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelAppearance {
    pub opacity: f32,
    pub radius: f32,
    pub border: bool,
    pub show_header: bool,
    pub theme: ThemeName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
    SlateBlueDark,
    SlateBlueLight,
    SandAmberDark,
    SandAmberLight,
    MauveVioletDark,
    MauveVioletLight,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModuleSpec {
    Clock(ClockSpec),
    Command(CommandSpec),
    GitHub(GitHubSpec),
    Usage(UsageSpec),
    Cpu(CpuSpec),
    Memory(MemorySpec),
    Gpu(GpuSpec),
}

impl ModuleSpec {
    pub fn id(&self) -> &str {
        match self {
            ModuleSpec::Clock(spec) => &spec.id,
            ModuleSpec::Command(spec) => &spec.id,
            ModuleSpec::GitHub(spec) => &spec.id,
            ModuleSpec::Usage(spec) => &spec.id,
            ModuleSpec::Cpu(spec) => &spec.id,
            ModuleSpec::Memory(spec) => &spec.id,
            ModuleSpec::Gpu(spec) => &spec.id,
        }
    }

    /// Background refresh interval for modules polled on worker threads.
    ///
    /// `None` for modules the host renders locally and synchronously (the
    /// clock), which are never scheduled on a worker.
    pub fn poll_interval(&self) -> Option<Duration> {
        match self {
            ModuleSpec::Clock(_) => None,
            ModuleSpec::Command(spec) => Some(spec.interval),
            ModuleSpec::GitHub(spec) => Some(spec.interval),
            ModuleSpec::Usage(spec) => Some(spec.interval),
            ModuleSpec::Cpu(spec) => Some(spec.interval),
            ModuleSpec::Memory(spec) => Some(spec.interval),
            ModuleSpec::Gpu(spec) => Some(spec.interval),
        }
    }
}

/// Render the clock locally from the current wall-clock time.
///
/// The clock is a pure function of its spec and the current time, so it
/// lives here rather than in a provider: both the live host and the
/// dry-run path render it without any worker thread or I/O.
pub fn clock_snapshot(spec: &ClockSpec) -> ModuleSnapshot {
    ModuleSnapshot {
        id: spec.id.clone(),
        title: "clock".into(),
        value: ModuleValue::Text(chrono::Local::now().format(&spec.format).to_string()),
        status: ModuleStatus::Ok,
        updated_at: Some(SystemTime::now()),
        stale_after: None,
    }
}

/// A freshly polled module snapshot pushed from a worker thread into the
/// host event loop.
///
/// `epoch` tags the provider generation that produced it; the host drops
/// updates whose epoch does not match the current configuration, so results
/// from workers still in flight across a config reload are ignored. `module`
/// is the spec module id used as the cache key, which can differ from
/// `snapshot.id` when a provider overrides the id in its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleUpdate {
    pub epoch: u64,
    pub panel: PanelId,
    pub module: String,
    pub snapshot: ModuleSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockSpec {
    pub id: String,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: String,
    pub exec: String,
    pub interval: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubSpec {
    pub id: String,
    pub repo: String,
    /// Display title; falls back to `repo` when unset. Lets a long
    /// `owner/name` show as a short label instead of being ellipsized.
    pub title: Option<String>,
    pub branch: Option<String>,
    pub workflow: Option<String>,
    pub interval: Duration,
    pub token_env: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageSpec {
    pub id: String,
    pub source: String,
    pub account: Option<String>,
    pub claude_dir: Option<String>,
    pub codex_home: Option<String>,
    pub auth_path: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub interval: Duration,
}

/// Aggregate CPU pressure: utilization sampled from `/proc/stat`, plus the
/// 1-minute load average normalized to core count. Temperature is carried in
/// the detail line, not as a gauge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuSpec {
    pub id: String,
    pub interval: Duration,
}

/// System memory pressure: RAM and swap utilization from `/proc/meminfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySpec {
    pub id: String,
    pub interval: Duration,
}

/// A single GPU's load: utilization, VRAM, and power draw read from the
/// `amdgpu` sysfs node for the given DRM card index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuSpec {
    pub id: String,
    pub card: u32,
    pub interval: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelSnapshot {
    pub panel_id: PanelId,
    pub modules: Vec<ModuleSnapshot>,
}

impl PanelSnapshot {
    pub fn empty(panel_id: PanelId) -> Self {
        Self {
            panel_id,
            modules: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleSnapshot {
    pub id: String,
    pub title: String,
    pub value: ModuleValue,
    pub status: ModuleStatus,
    pub updated_at: Option<SystemTime>,
    pub stale_after: Option<Duration>,
}

impl ModuleSnapshot {
    /// Equality on the fields that affect rendering, ignoring freshness
    /// timestamps. Two snapshots that paint identically are display-equal
    /// even when produced at different instants — this is what lets the host
    /// suppress redundant redraws (e.g. a clock whose minute has not changed).
    pub fn display_eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.title == other.title
            && self.value == other.value
            && self.status == other.status
    }

    /// Placeholder rendered before a module's first snapshot arrives.
    pub fn loading(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            value: ModuleValue::Text("…".into()),
            status: ModuleStatus::Unknown,
            updated_at: None,
            stale_after: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModuleValue {
    Text(String),
    Percent(f32),
    Count {
        current: u32,
        total: Option<u32>,
    },
    State {
        label: String,
        detail: Option<String>,
    },
    Gauges(GaugeGroup),
}

/// One or more named percentage gauges plus non-numeric context. Produced by
/// any module that wants to show a handful of meters: subscription/quota usage
/// (plan, credits, reset time in the detail) as well as CPU, memory, and GPU
/// load (temperature, clocks, watts in the detail).
///
/// Carried structured rather than pre-formatted into a label so the UI can
/// render gauges without re-parsing percentages back out of a string — the
/// provider owns the numbers, the UI owns their presentation.
#[derive(Clone, Debug, PartialEq)]
pub struct GaugeGroup {
    /// Ordered gauges; the first is the headline shown in compact layouts.
    pub gauges: Vec<Gauge>,
    /// Remaining context with no percentage of its own (plan, temp, watts).
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gauge {
    pub label: String,
    pub percent: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleStatus {
    Ok,
    Info,
    Warning,
    Critical,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(id: &str) -> ModuleSpec {
        ModuleSpec::Clock(ClockSpec {
            id: id.into(),
            format: "%H:%M".into(),
        })
    }

    fn command(id: &str, interval: Duration) -> ModuleSpec {
        ModuleSpec::Command(CommandSpec {
            id: id.into(),
            exec: "true".into(),
            interval,
        })
    }

    #[test]
    fn clock_modules_are_not_polled_but_others_are() {
        assert_eq!(clock("c").poll_interval(), None);
        assert_eq!(
            command("cmd", Duration::from_secs(30)).poll_interval(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(clock("c").id(), "c");
        assert_eq!(command("cmd", Duration::from_secs(1)).id(), "cmd");
    }

    #[test]
    fn display_eq_ignores_freshness_timestamps() {
        let base = ModuleSnapshot {
            id: "m".into(),
            title: "t".into(),
            value: ModuleValue::Text("19:04".into()),
            status: ModuleStatus::Ok,
            updated_at: Some(SystemTime::now()),
            stale_after: None,
        };
        let later = ModuleSnapshot {
            updated_at: Some(SystemTime::now() + Duration::from_secs(60)),
            stale_after: Some(Duration::from_secs(5)),
            ..base.clone()
        };
        assert!(base.display_eq(&later), "same paint, different timestamps");

        let changed = ModuleSnapshot {
            value: ModuleValue::Text("19:05".into()),
            ..base.clone()
        };
        assert!(!base.display_eq(&changed), "different value must differ");
    }

    #[test]
    fn loading_placeholder_is_unknown() {
        let placeholder = ModuleSnapshot::loading("m", "title");
        assert_eq!(placeholder.status, ModuleStatus::Unknown);
        assert!(placeholder.updated_at.is_none());
    }
}
