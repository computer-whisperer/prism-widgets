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
