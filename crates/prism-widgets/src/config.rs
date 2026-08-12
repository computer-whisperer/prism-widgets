use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use prism_widgets_core::{
    ClipboardSpec, ClockSpec, CommandSpec, CpuSpec, GitHubSpec, GitLabSpec, GpuSpec, MemorySpec,
    ModuleSpec, PanelAnchor, PanelAppearance, PanelGeometry, PanelId, PanelLayer, PanelLayout,
    PanelSpec, StatusPageSpec, ThemeName, UsageSpec,
};

#[derive(Debug, knuffel::Decode)]
pub struct Config {
    #[knuffel(children(name = "panel"))]
    panels: Vec<PanelNode>,
}

#[derive(Debug, knuffel::Decode)]
struct PanelNode {
    #[knuffel(argument)]
    id: String,
    #[knuffel(child, unwrap(argument))]
    output: Option<String>,
    #[knuffel(child, unwrap(argument, str), default = AnchorName::TopRight)]
    anchor: AnchorName,
    #[knuffel(child, unwrap(argument, str))]
    layout: Option<LayoutName>,
    #[knuffel(child, unwrap(argument))]
    width: Option<u32>,
    #[knuffel(child, unwrap(argument), default = 56)]
    height: u32,
    #[knuffel(child, unwrap(argument), default = 6)]
    margin: i32,
    /// Whether to reserve compositor layout space automatically.
    #[knuffel(child)]
    reserve: Option<BoolNode>,
    /// Manual layer-shell exclusive-zone override.
    #[knuffel(child, unwrap(argument))]
    exclusive_zone: Option<i32>,
    #[knuffel(child, unwrap(argument, str), default = LayerName::Top)]
    layer: LayerName,
    #[knuffel(child, unwrap(argument), default = 0.82)]
    opacity: f32,
    #[knuffel(child, unwrap(argument), default = 12)]
    radius: u32,
    #[knuffel(child, unwrap(argument), default = true)]
    border: bool,
    #[knuffel(child, unwrap(argument), default = false)]
    show_header: bool,
    #[knuffel(child, unwrap(argument, str), default = ThemeConfigName::Dark)]
    theme: ThemeConfigName,
    #[knuffel(child)]
    modules: ModulesNode,
}

#[derive(Debug, knuffel::Decode)]
struct ModulesNode {
    #[knuffel(children)]
    list: Vec<ModuleNode>,
}

#[derive(Debug, knuffel::Decode)]
struct BoolNode {
    #[knuffel(argument)]
    value: bool,
}

#[derive(Debug, knuffel::Decode)]
enum ModuleNode {
    Clock(ClockNode),
    Command(CommandNode),
    Github(GitHubNode),
    Gitlab(GitLabNode),
    Statuspage(StatusPageNode),
    Usage(UsageNode),
    Cpu(CpuNode),
    Memory(MemoryNode),
    Gpu(GpuNode),
    Clipboard(ClipboardNode),
}

#[derive(Debug, knuffel::Decode)]
struct ClockNode {
    #[knuffel(property, default = String::from("clock"))]
    id: String,
    #[knuffel(property, default = String::from("%H:%M:%S"))]
    format: String,
}

#[derive(Debug, knuffel::Decode)]
struct CommandNode {
    #[knuffel(property)]
    id: String,
    #[knuffel(property)]
    exec: String,
    #[knuffel(property, default = 60)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct GitHubNode {
    #[knuffel(property)]
    repo: String,
    #[knuffel(property)]
    id: Option<String>,
    #[knuffel(property)]
    title: Option<String>,
    #[knuffel(property)]
    branch: Option<String>,
    #[knuffel(property)]
    workflow: Option<String>,
    #[knuffel(property(name = "token-env"))]
    token_env: Option<String>,
    #[knuffel(property, default = 60)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct GitLabNode {
    #[knuffel(property)]
    project: String,
    #[knuffel(property)]
    id: Option<String>,
    #[knuffel(property)]
    title: Option<String>,
    #[knuffel(property)]
    branch: Option<String>,
    #[knuffel(property)]
    host: Option<String>,
    #[knuffel(property(name = "token-env"))]
    token_env: Option<String>,
    #[knuffel(property, default = 60)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct StatusPageNode {
    #[knuffel(property, default = String::from("https://status.claude.com"))]
    url: String,
    #[knuffel(property)]
    id: Option<String>,
    #[knuffel(property)]
    title: Option<String>,
    #[knuffel(property, default = 300)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct UsageNode {
    #[knuffel(property)]
    source: String,
    #[knuffel(property)]
    id: Option<String>,
    #[knuffel(property)]
    account: Option<String>,
    #[knuffel(property(name = "claude-dir"))]
    claude_dir: Option<String>,
    #[knuffel(property(name = "codex-home"))]
    codex_home: Option<String>,
    #[knuffel(property(name = "auth-path"))]
    auth_path: Option<String>,
    #[knuffel(property(name = "base-url"))]
    base_url: Option<String>,
    #[knuffel(property(name = "api-key-env"))]
    api_key_env: Option<String>,
    /// Space/comma-separated usage windows to show as gauges (claude only),
    /// e.g. `windows="5h 7d fable"`. Unset shows `5h 7d`.
    #[knuffel(property)]
    windows: Option<String>,
    #[knuffel(property, default = 300)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct CpuNode {
    #[knuffel(property, default = String::from("cpu"))]
    id: String,
    #[knuffel(property, default = 3)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct MemoryNode {
    #[knuffel(property, default = String::from("memory"))]
    id: String,
    #[knuffel(property, default = 5)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct GpuNode {
    #[knuffel(property, default = 0)]
    card: u32,
    #[knuffel(property)]
    id: Option<String>,
    #[knuffel(property, default = 3)]
    interval: u64,
}

#[derive(Debug, knuffel::Decode)]
struct ClipboardNode {
    #[knuffel(property, default = String::from("clipboard"))]
    id: String,
    #[knuffel(property(name = "max-entries"), default = 8)]
    max_entries: usize,
}

#[derive(Debug, Clone, Copy, Default)]
enum AnchorName {
    TopLeft,
    Top,
    #[default]
    TopRight,
    BottomLeft,
    Bottom,
    BottomRight,
    Left,
    Right,
}

impl std::str::FromStr for AnchorName {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "top-left" => Self::TopLeft,
            "top" => Self::Top,
            "top-right" => Self::TopRight,
            "bottom-left" => Self::BottomLeft,
            "bottom" => Self::Bottom,
            "bottom-right" => Self::BottomRight,
            "left" => Self::Left,
            "right" => Self::Right,
            other => return Err(format!("unknown anchor {other:?}")),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum LayerName {
    Background,
    Bottom,
    #[default]
    Top,
    Overlay,
}

impl std::str::FromStr for LayerName {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "background" => Self::Background,
            "bottom" => Self::Bottom,
            "top" => Self::Top,
            "overlay" => Self::Overlay,
            other => return Err(format!("unknown layer {other:?}")),
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum LayoutName {
    Bar,
    Sidebar,
}

impl std::str::FromStr for LayoutName {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "bar" => Self::Bar,
            "sidebar" => Self::Sidebar,
            other => return Err(format!("unknown layout {other:?}")),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum ThemeConfigName {
    #[default]
    Dark,
    Light,
    SlateBlueDark,
    SlateBlueLight,
    SandAmberDark,
    SandAmberLight,
    MauveVioletDark,
    MauveVioletLight,
}

impl std::str::FromStr for ThemeConfigName {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "dark" => Self::Dark,
            "light" => Self::Light,
            "slate-blue-dark" => Self::SlateBlueDark,
            "slate-blue-light" => Self::SlateBlueLight,
            "sand-amber-dark" => Self::SandAmberDark,
            "sand-amber-light" => Self::SandAmberLight,
            "mauve-violet-dark" => Self::MauveVioletDark,
            "mauve-violet-light" => Self::MauveVioletLight,
            other => return Err(format!("unknown theme {other:?}")),
        })
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("PRISM_WIDGETS_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("prism-widgets").join("config.kdl"))
    }

    pub fn load() -> Result<Self> {
        let Some(path) = Self::path() else {
            tracing::warn!("no config path resolvable (no $HOME); using defaults");
            return Ok(Self::default());
        };
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &std::path::Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no config at {}; using defaults", path.display());
                return Ok(Self::default());
            }
            Err(err) => return Err(err).context(format!("reading {}", path.display())),
        };
        let config = match knuffel::parse::<Config>(&path.to_string_lossy(), &text) {
            Ok(config) => config,
            Err(err) => anyhow::bail!("config error:\n{:?}", miette::Report::new(err)),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn panel_specs(&self) -> Vec<PanelSpec> {
        self.panels.iter().map(PanelNode::to_spec).collect()
    }

    fn validate(&self) -> Result<()> {
        if self.panels.is_empty() {
            anyhow::bail!("config must contain at least one panel");
        }
        let mut panel_ids = HashSet::new();
        for panel in &self.panels {
            if !panel_ids.insert(panel.id.as_str()) {
                anyhow::bail!("duplicate panel id {:?}", panel.id);
            }
            let layout = panel.resolved_layout();
            if layout == PanelLayout::Sidebar
                && !matches!(panel.anchor, AnchorName::Left | AnchorName::Right)
            {
                anyhow::bail!(
                    "panel {:?}: sidebar layout requires anchor \"left\" or \"right\"",
                    panel.id
                );
            }
            if panel.height == 0 {
                anyhow::bail!("panel {:?}: height must be at least 1", panel.id);
            }
            if !(0.0..=1.0).contains(&panel.opacity) {
                anyhow::bail!("panel {:?}: opacity must be within 0.0..=1.0", panel.id);
            }
            // Snapshots are cached by (panel, module id), so two modules
            // sharing an id would silently display whichever fetch landed
            // last. Check resolved ids (defaults included), not just
            // explicit ones.
            let mut module_ids = HashSet::new();
            for module in &panel.modules.list {
                let id = module.to_spec().id().to_string();
                if !module_ids.insert(id.clone()) {
                    anyhow::bail!(
                        "panel {:?}: duplicate module id {id:?}; set id=\"...\" to disambiguate",
                        panel.id
                    );
                }
                match module {
                    ModuleNode::Clock(clock) => {
                        let _ = chrono::Local::now().format(&clock.format).to_string();
                    }
                    ModuleNode::Command(command) if command.interval == 0 => {
                        anyhow::bail!("command {:?}: interval must be at least 1", command.id);
                    }
                    ModuleNode::Github(github) if github.interval == 0 => {
                        anyhow::bail!("github {:?}: interval must be at least 1", github.repo);
                    }
                    ModuleNode::Gitlab(gitlab) if gitlab.interval == 0 => {
                        anyhow::bail!("gitlab {:?}: interval must be at least 1", gitlab.project);
                    }
                    ModuleNode::Statuspage(statuspage) if statuspage.interval == 0 => {
                        anyhow::bail!(
                            "statuspage {:?}: interval must be at least 1",
                            statuspage.url
                        );
                    }
                    ModuleNode::Usage(usage) if usage.interval == 0 => {
                        anyhow::bail!("usage {:?}: interval must be at least 1", usage.source);
                    }
                    ModuleNode::Cpu(cpu) if cpu.interval == 0 => {
                        anyhow::bail!("cpu {:?}: interval must be at least 1", cpu.id);
                    }
                    ModuleNode::Memory(memory) if memory.interval == 0 => {
                        anyhow::bail!("memory {:?}: interval must be at least 1", memory.id);
                    }
                    ModuleNode::Gpu(gpu) if gpu.interval == 0 => {
                        anyhow::bail!("gpu card={}: interval must be at least 1", gpu.card);
                    }
                    ModuleNode::Clipboard(clipboard) if clipboard.max_entries == 0 => {
                        anyhow::bail!(
                            "clipboard {:?}: max-entries must be at least 1",
                            clipboard.id
                        );
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            panels: vec![PanelNode {
                id: "top-right-status".into(),
                output: None,
                anchor: AnchorName::TopRight,
                layout: None,
                width: Some(720),
                height: 56,
                margin: 6,
                reserve: None,
                exclusive_zone: None,
                layer: LayerName::Top,
                opacity: 0.82,
                radius: 12,
                border: true,
                show_header: false,
                theme: ThemeConfigName::Dark,
                modules: ModulesNode {
                    list: vec![ModuleNode::Clock(ClockNode {
                        id: "clock".into(),
                        format: "%H:%M:%S".into(),
                    })],
                },
            }],
        }
    }
}

impl PanelNode {
    fn to_spec(&self) -> PanelSpec {
        let layout = self.resolved_layout();
        let width = self.width.or_else(|| {
            if layout == PanelLayout::Sidebar {
                Some(320)
            } else {
                None
            }
        });
        PanelSpec {
            id: PanelId::new(self.id.clone()),
            output: self.output.clone(),
            layout,
            geometry: PanelGeometry {
                width,
                height: self.height,
                margin: self.margin,
                exclusive_zone: self.exclusive_zone.unwrap_or_else(|| {
                    if self.reserve.as_ref().map(|node| node.value).unwrap_or(true) {
                        automatic_exclusive_zone(self.anchor, width, self.height)
                    } else {
                        -1
                    }
                }),
                anchor: self.anchor.into(),
                layer: self.layer.into(),
            },
            appearance: PanelAppearance {
                opacity: self.opacity,
                radius: self.radius as f32,
                border: self.border,
                show_header: self.show_header,
                theme: self.theme.into(),
            },
            modules: self.modules.list.iter().map(ModuleNode::to_spec).collect(),
        }
    }

    fn resolved_layout(&self) -> PanelLayout {
        self.layout.map(Into::into).unwrap_or(match self.anchor {
            AnchorName::Left | AnchorName::Right => PanelLayout::Sidebar,
            AnchorName::TopLeft
            | AnchorName::Top
            | AnchorName::TopRight
            | AnchorName::BottomLeft
            | AnchorName::Bottom
            | AnchorName::BottomRight => PanelLayout::Bar,
        })
    }
}

fn automatic_exclusive_zone(anchor: AnchorName, width: Option<u32>, height: u32) -> i32 {
    match anchor {
        AnchorName::Left | AnchorName::Right => width.unwrap_or(height) as i32,
        AnchorName::TopLeft
        | AnchorName::Top
        | AnchorName::TopRight
        | AnchorName::BottomLeft
        | AnchorName::Bottom
        | AnchorName::BottomRight => height as i32,
    }
}

impl ModuleNode {
    fn to_spec(&self) -> ModuleSpec {
        match self {
            ModuleNode::Clock(clock) => ModuleSpec::Clock(ClockSpec {
                id: clock.id.clone(),
                format: clock.format.clone(),
            }),
            ModuleNode::Command(command) => ModuleSpec::Command(CommandSpec {
                id: command.id.clone(),
                exec: command.exec.clone(),
                interval: Duration::from_secs(command.interval),
            }),
            ModuleNode::Github(github) => ModuleSpec::GitHub(GitHubSpec {
                id: github.id.clone().unwrap_or_else(|| github.repo.clone()),
                repo: github.repo.clone(),
                title: github.title.clone(),
                branch: github.branch.clone(),
                workflow: github.workflow.clone(),
                interval: Duration::from_secs(github.interval),
                token_env: github.token_env.clone(),
            }),
            ModuleNode::Gitlab(gitlab) => ModuleSpec::GitLab(GitLabSpec {
                // Default id carries a `gitlab:` marker so the UI shows the
                // GitLab brand icon (a bare `group/project` id is
                // indistinguishable from a GitHub `owner/repo`), plus the
                // host so the same project path on two GitLab servers
                // cannot collide in the snapshot cache.
                id: gitlab.id.clone().unwrap_or_else(|| match &gitlab.host {
                    Some(host) => format!("gitlab:{host}/{}", gitlab.project),
                    None => format!("gitlab:{}", gitlab.project),
                }),
                project: gitlab.project.clone(),
                title: gitlab.title.clone(),
                branch: gitlab.branch.clone(),
                host: gitlab.host.clone(),
                interval: Duration::from_secs(gitlab.interval),
                token_env: gitlab.token_env.clone(),
            }),
            ModuleNode::Statuspage(statuspage) => ModuleSpec::StatusPage(StatusPageSpec {
                id: statuspage
                    .id
                    .clone()
                    .unwrap_or_else(|| statuspage.url.clone()),
                url: statuspage.url.clone(),
                title: statuspage.title.clone(),
                interval: Duration::from_secs(statuspage.interval),
            }),
            ModuleNode::Usage(usage) => ModuleSpec::Usage(UsageSpec {
                id: usage.id.clone().unwrap_or_else(|| usage.source.clone()),
                source: usage.source.clone(),
                account: usage.account.clone(),
                claude_dir: usage.claude_dir.clone(),
                codex_home: usage.codex_home.clone(),
                auth_path: usage.auth_path.clone(),
                base_url: usage.base_url.clone(),
                api_key_env: usage.api_key_env.clone(),
                windows: usage.windows.as_ref().map(|raw| {
                    raw.split(|c: char| c.is_whitespace() || c == ',')
                        .filter(|token| !token.is_empty())
                        .map(str::to_string)
                        .collect()
                }),
                interval: Duration::from_secs(usage.interval),
            }),
            ModuleNode::Cpu(cpu) => ModuleSpec::Cpu(CpuSpec {
                id: cpu.id.clone(),
                interval: Duration::from_secs(cpu.interval),
            }),
            ModuleNode::Memory(memory) => ModuleSpec::Memory(MemorySpec {
                id: memory.id.clone(),
                interval: Duration::from_secs(memory.interval),
            }),
            ModuleNode::Gpu(gpu) => ModuleSpec::Gpu(GpuSpec {
                id: gpu.id.clone().unwrap_or_else(|| format!("gpu{}", gpu.card)),
                card: gpu.card,
                interval: Duration::from_secs(gpu.interval),
            }),
            ModuleNode::Clipboard(clipboard) => ModuleSpec::Clipboard(ClipboardSpec {
                id: clipboard.id.clone(),
                max_entries: clipboard.max_entries,
            }),
        }
    }
}

impl From<AnchorName> for PanelAnchor {
    fn from(value: AnchorName) -> Self {
        match value {
            AnchorName::TopLeft => Self::TopLeft,
            AnchorName::Top => Self::Top,
            AnchorName::TopRight => Self::TopRight,
            AnchorName::BottomLeft => Self::BottomLeft,
            AnchorName::Bottom => Self::Bottom,
            AnchorName::BottomRight => Self::BottomRight,
            AnchorName::Left => Self::Left,
            AnchorName::Right => Self::Right,
        }
    }
}

impl From<LayerName> for PanelLayer {
    fn from(value: LayerName) -> Self {
        match value {
            LayerName::Background => Self::Background,
            LayerName::Bottom => Self::Bottom,
            LayerName::Top => Self::Top,
            LayerName::Overlay => Self::Overlay,
        }
    }
}

impl From<LayoutName> for PanelLayout {
    fn from(value: LayoutName) -> Self {
        match value {
            LayoutName::Bar => Self::Bar,
            LayoutName::Sidebar => Self::Sidebar,
        }
    }
}

impl From<ThemeConfigName> for ThemeName {
    fn from(value: ThemeConfigName) -> Self {
        match value {
            ThemeConfigName::Dark => Self::Dark,
            ThemeConfigName::Light => Self::Light,
            ThemeConfigName::SlateBlueDark => Self::SlateBlueDark,
            ThemeConfigName::SlateBlueLight => Self::SlateBlueLight,
            ThemeConfigName::SandAmberDark => Self::SandAmberDark,
            ThemeConfigName::SandAmberLight => Self::SandAmberLight,
            ThemeConfigName::MauveVioletDark => Self::MauveVioletDark,
            ThemeConfigName::MauveVioletLight => Self::MauveVioletLight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_header_is_hidden_by_default() {
        let config = parse_config(
            r#"
            panel "right-sidebar" {
                anchor "right"
                layout "sidebar"
                modules {
                    clock
                }
            }
            "#,
        );

        assert!(!config.panel_specs()[0].appearance.show_header);
    }

    #[test]
    fn panel_header_can_be_enabled() {
        let config = parse_config(
            r#"
            panel "right-sidebar" {
                anchor "right"
                layout "sidebar"
                show-header true
                modules {
                    clock
                }
            }
            "#,
        );

        assert!(config.panel_specs()[0].appearance.show_header);
    }

    #[test]
    fn loads_config_from_explicit_path() {
        let path = std::env::temp_dir().join(format!(
            "prism-widgets-config-test-{}.kdl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            panel "top" {
                anchor "top-right"
                modules {
                    clock format="%H:%M"
                }
            }
            "#,
        )
        .unwrap();

        let config = Config::load_from_path(&path).unwrap();
        assert_eq!(config.panel_specs()[0].id.0, "top");

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn clipboard_module_parses_with_defaults_and_overrides() {
        let config = parse_config(
            r#"
            panel "right-sidebar" {
                anchor "right"
                layout "sidebar"
                modules {
                    clipboard
                    clipboard id="clips" max-entries=20
                }
            }
            "#,
        );

        let modules = &config.panel_specs()[0].modules;
        assert_eq!(
            modules[0],
            ModuleSpec::Clipboard(ClipboardSpec {
                id: "clipboard".into(),
                max_entries: 8,
            })
        );
        assert_eq!(
            modules[1],
            ModuleSpec::Clipboard(ClipboardSpec {
                id: "clips".into(),
                max_entries: 20,
            })
        );
    }

    #[test]
    fn clipboard_max_entries_zero_is_rejected() {
        let text = r#"
            panel "right-sidebar" {
                anchor "right"
                layout "sidebar"
                modules {
                    clipboard max-entries=0
                }
            }
            "#;
        let config = knuffel::parse::<Config>("test.kdl", text).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn gitlab_default_id_includes_host() {
        // The same project path on two servers must resolve to distinct
        // ids, or their snapshots would overwrite each other.
        let config = parse_config(
            r#"
            panel "top" {
                modules {
                    gitlab project="raven/fw" host="gitlab.a.example"
                    gitlab project="raven/fw" host="gitlab.b.example"
                    gitlab project="group/app"
                }
            }
            "#,
        );
        let modules = &config.panel_specs()[0].modules;
        assert_eq!(modules[0].id(), "gitlab:gitlab.a.example/raven/fw");
        assert_eq!(modules[1].id(), "gitlab:gitlab.b.example/raven/fw");
        assert_eq!(modules[2].id(), "gitlab:group/app");
    }

    #[test]
    fn duplicate_module_ids_in_panel_are_rejected() {
        let text = r#"
            panel "top" {
                modules {
                    gitlab project="raven/fw"
                    gitlab project="raven/fw"
                }
            }
            "#;
        let config = knuffel::parse::<Config>("test.kdl", text).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("duplicate module id"),
            "unexpected error {err:?}"
        );

        // The same id in different panels is fine: snapshots are keyed by
        // (panel, module id).
        parse_config(
            r#"
            panel "top" {
                modules { gitlab project="raven/fw"; }
            }
            panel "bottom" {
                anchor "bottom"
                modules { gitlab project="raven/fw"; }
            }
            "#,
        );
    }

    #[test]
    fn duplicate_panel_ids_are_rejected() {
        let text = r#"
            panel "top" {
                modules { clock; }
            }
            panel "top" {
                modules { clock; }
            }
            "#;
        let config = knuffel::parse::<Config>("test.kdl", text).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("duplicate panel id"),
            "unexpected error {err:?}"
        );
    }

    fn parse_config(text: &str) -> Config {
        let config = knuffel::parse::<Config>("test.kdl", text).unwrap();
        config.validate().unwrap();
        config
    }
}
