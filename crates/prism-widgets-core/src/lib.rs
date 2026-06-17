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
}

impl ModuleSpec {
    pub fn id(&self) -> &str {
        match self {
            ModuleSpec::Clock(spec) => &spec.id,
            ModuleSpec::Command(spec) => &spec.id,
            ModuleSpec::GitHub(spec) => &spec.id,
            ModuleSpec::Usage(spec) => &spec.id,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleStatus {
    Ok,
    Info,
    Warning,
    Critical,
    Unknown,
}
