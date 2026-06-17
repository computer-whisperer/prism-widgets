//! Damascene projection for panel snapshots.

use std::sync::LazyLock;

use damascene_core::prelude::*;
use prism_widgets_core::{
    ModuleSnapshot, ModuleStatus, ModuleValue, PanelAnchor, PanelAppearance, PanelLayout,
    PanelSnapshot, ThemeName, UsageMetric, UsageValue,
};

const GITHUB_SVG: &str = include_str!("../assets/icons/github.svg");
const OPENAI_SVG: &str = include_str!("../assets/icons/openai.svg");
const ANTHROPIC_SVG: &str = include_str!("../assets/icons/anthropic.svg");

static ICON_GITHUB: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(GITHUB_SVG).expect("parse github.svg"));
static ICON_OPENAI: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(OPENAI_SVG).expect("parse openai.svg"));
static ICON_ANTHROPIC: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(ANTHROPIC_SVG).expect("parse anthropic.svg"));

#[derive(Clone, Debug)]
pub struct PanelView {
    appearance: PanelAppearance,
    anchor: PanelAnchor,
    layout: PanelLayout,
    width: Option<u32>,
    snapshot: PanelSnapshot,
}

impl PanelView {
    pub fn new(
        appearance: PanelAppearance,
        anchor: PanelAnchor,
        layout: PanelLayout,
        width: Option<u32>,
        snapshot: PanelSnapshot,
    ) -> Self {
        Self {
            appearance,
            anchor,
            layout,
            width,
            snapshot,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WidgetsBandApp {
    layout: PanelLayout,
    views: Vec<PanelView>,
}

impl WidgetsBandApp {
    pub fn new(layout: PanelLayout, views: Vec<PanelView>) -> Self {
        Self { layout, views }
    }

    pub fn set_views(&mut self, views: Vec<PanelView>) {
        if let Some(view) = views.first() {
            self.layout = view.layout;
        }
        self.views = views;
    }
}

impl App for WidgetsBandApp {
    fn theme(&self) -> Theme {
        self.views
            .first()
            .map(|view| theme_of(view.appearance.theme))
            .unwrap_or_else(Theme::damascene_dark)
    }

    fn build(&self, _cx: &BuildCx) -> El {
        overlays(
            cluster_shell(self.layout, &self.views),
            Vec::<Option<El>>::new(),
        )
    }
}

pub fn theme_of(name: ThemeName) -> Theme {
    match name {
        ThemeName::Dark => Theme::damascene_dark(),
        ThemeName::Light => Theme::damascene_light(),
        ThemeName::SlateBlueDark => Theme::radix_slate_blue_dark(),
        ThemeName::SlateBlueLight => Theme::radix_slate_blue_light(),
        ThemeName::SandAmberDark => Theme::radix_sand_amber_dark(),
        ThemeName::SandAmberLight => Theme::radix_sand_amber_light(),
        ThemeName::MauveVioletDark => Theme::radix_mauve_violet_dark(),
        ThemeName::MauveVioletLight => Theme::radix_mauve_violet_light(),
    }
}

fn cluster_shell(layout: PanelLayout, views: &[PanelView]) -> El {
    match layout {
        PanelLayout::Bar => bar_shell(views),
        PanelLayout::Sidebar => sidebar_shell(views),
    }
}

fn bar_shell(views: &[PanelView]) -> El {
    let mut start = Vec::new();
    let mut center = Vec::new();
    let mut end = Vec::new();
    for view in views {
        match view.anchor {
            PanelAnchor::TopLeft | PanelAnchor::BottomLeft | PanelAnchor::Left => {
                start.push(bar_panel_card(view))
            }
            PanelAnchor::Top | PanelAnchor::Bottom => center.push(bar_panel_card(view)),
            PanelAnchor::TopRight | PanelAnchor::BottomRight | PanelAnchor::Right => {
                end.push(bar_panel_card(view))
            }
        }
    }

    row([
        row(start)
            .fill_width()
            .justify(Justify::Start)
            .align(Align::Center),
        row(center)
            .fill_width()
            .justify(Justify::Center)
            .align(Align::Center),
        row(end)
            .fill_width()
            .justify(Justify::End)
            .align(Align::Center),
    ])
    .fill_width()
    .height(Size::Fill(1.0))
    .align(Align::Center)
}

fn sidebar_shell(views: &[PanelView]) -> El {
    column(views.iter().map(sidebar_panel_card))
        .fill_size()
        .gap(tokens::SPACE_2)
        .padding(Sides::all(tokens::SPACE_2))
        .align(Align::Stretch)
        .clip()
}

fn bar_panel_card(view: &PanelView) -> El {
    let modules = view
        .snapshot
        .modules
        .iter()
        .map(module_chip)
        .collect::<Vec<_>>();

    let panel = card([toolbar([toolbar_group(modules)])])
        .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2))
        .opacity(view.appearance.opacity)
        .clip();
    apply_panel_width(panel, view)
}

fn sidebar_panel_card(view: &PanelView) -> El {
    let modules = view
        .snapshot
        .modules
        .iter()
        .map(sidebar_module_item)
        .collect::<Vec<_>>();

    let mut sections = Vec::new();
    if view.appearance.show_header {
        let title = ellipsize(&view.snapshot.panel_id.0, 32);
        sections.push(
            card_header([card_title(title)]).padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2)),
        );
    }
    sections.push(
        card_content([item_group(modules)]).padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_2)),
    );

    let panel = card(sections).opacity(view.appearance.opacity).clip();
    apply_panel_width(panel, view)
}

fn apply_panel_width(mut panel: El, view: &PanelView) -> El {
    if !view.appearance.border {
        panel = panel.stroke_width(0.0);
    }
    match view.width {
        Some(width) => panel.width(Size::Fixed(width as f32)),
        None => panel.fill_width(),
    }
}

fn module_chip(module: &ModuleSnapshot) -> El {
    let mut cells = Vec::new();
    if let Some(glyph) = module_brand_icon(module) {
        cells.push(
            icon(glyph)
                .icon_size(tokens::ICON_XS)
                .color(tokens::MUTED_FOREGROUND),
        );
    }
    cells.push(text(ellipsize(&module.title, 22)).caption().muted());
    match &module.value {
        ModuleValue::State { label, detail } => {
            cells.push(status_badge_with_label(module.status, ellipsize(label, 20)));
            if let Some(detail) = detail {
                cells.push(text(ellipsize(detail, 28)).caption().muted());
            }
        }
        ModuleValue::Usage(usage) => {
            cells.push(status_badge_with_label(
                module.status,
                ellipsize(&usage_headline(usage), 20),
            ));
            if let Some(detail) = &usage.detail {
                cells.push(text(ellipsize(detail, 28)).caption().muted());
            }
        }
        _ => {
            cells.push(module_value_text(module).label());
            if !matches!(module.status, ModuleStatus::Ok) {
                cells.push(status_badge(module.status));
            }
        }
    }

    let content = row(cells).gap(tokens::SPACE_1).align(Align::Center);
    let chip = if let Some(fraction) = module_fraction(module) {
        column([
            content,
            progress_with_color(fraction, status_color(module.status))
                .height(Size::Fixed(3.0))
                .width(Size::Fill(1.0)),
        ])
        .gap(2.0)
        .align(Align::Stretch)
    } else {
        content
    };

    chip.padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .fill(tokens::MUTED.with_alpha_u8(36))
        .stroke(tokens::BORDER.with_alpha_u8(90))
        .radius(tokens::RADIUS_MD)
}

fn sidebar_module_item(module: &ModuleSnapshot) -> El {
    let metrics = module_usage_metrics(module);
    let mut header = vec![text(ellipsize(&module.title, 30)).label(), spacer()];
    if metrics.is_empty() {
        header.push(module_value_summary(module));
    }

    let mut content = vec![row(header).gap(tokens::SPACE_2).align(Align::Center)];
    if let Some(detail) = module_detail_text(module) {
        content.push(text(detail).caption().muted());
    }
    if metrics.is_empty() {
        if let Some(fraction) = module_fraction(module) {
            content.push(
                progress_with_color(fraction, status_color(module.status))
                    .height(Size::Fixed(5.0))
                    .width(Size::Fill(1.0)),
            );
        }
    } else {
        content.extend(metrics.iter().map(usage_metric_bar));
    }

    let mut children = Vec::new();
    if let Some(glyph) = module_brand_icon(module) {
        children.push(item_media_icon(glyph));
    }
    children.push(item_content(content).gap(tokens::SPACE_2));
    if !matches!(module.status, ModuleStatus::Ok) {
        children.push(item_actions([status_badge(module.status)]));
    }
    item(children)
}

fn module_value_text(module: &ModuleSnapshot) -> El {
    text(module_value_plain(module))
}

fn module_value_summary(module: &ModuleSnapshot) -> El {
    match &module.value {
        ModuleValue::State { label, .. } => {
            status_badge_with_label(module.status, ellipsize(label, 24))
        }
        ModuleValue::Usage(usage) => {
            status_badge_with_label(module.status, ellipsize(&usage_headline(usage), 24))
        }
        _ => text(module_value_plain(module)).label(),
    }
}

fn module_value_plain(module: &ModuleSnapshot) -> String {
    match &module.value {
        ModuleValue::Text(value) => ellipsize(value, 36),
        ModuleValue::Percent(frac) => format!("{:.0}%", frac * 100.0),
        ModuleValue::Count { current, total } => match total {
            Some(total) => format!("{current}/{total}"),
            None => current.to_string(),
        },
        ModuleValue::State { label, .. } => ellipsize(label, 36),
        ModuleValue::Usage(usage) => ellipsize(&usage_headline(usage), 36),
    }
}

fn module_detail_text(module: &ModuleSnapshot) -> Option<String> {
    let detail = match &module.value {
        ModuleValue::Usage(usage) => usage.detail.as_deref(),
        ModuleValue::State { detail, .. } => detail.as_deref(),
        _ => None,
    }?;
    (!detail.is_empty()).then(|| ellipsize(detail, 56))
}

/// The metric gauges to draw as bars: a usage value's windows, or none for
/// other module kinds (which fall back to a single fraction bar).
fn module_usage_metrics(module: &ModuleSnapshot) -> &[UsageMetric] {
    match &module.value {
        ModuleValue::Usage(usage) => &usage.metrics,
        _ => &[],
    }
}

/// Compact one-line summary of a usage value: its headline (first) gauge.
fn usage_headline(usage: &UsageValue) -> String {
    usage
        .metrics
        .first()
        .map(|metric| format!("{} {}", metric.label, format_percent(metric.percent)))
        .unwrap_or_else(|| "usage".into())
}

fn format_percent(percent: f32) -> String {
    format!("{:.0}%", percent.clamp(0.0, 999.0))
}

fn usage_metric_bar(metric: &UsageMetric) -> El {
    let fraction = (metric.percent / 100.0).clamp(0.0, 1.0);
    column([
        row([
            text(ellipsize(&metric.label, 18)).caption().muted(),
            spacer(),
            text(format_percent(metric.percent)).caption().muted(),
        ])
        .align(Align::Center),
        progress_with_color(fraction, metric_color(metric.percent))
            .height(Size::Fixed(7.0))
            .width(Size::Fill(1.0)),
    ])
    .gap(3.0)
    .align(Align::Stretch)
}

fn module_fraction(module: &ModuleSnapshot) -> Option<f32> {
    match &module.value {
        ModuleValue::Percent(frac) => Some(frac.clamp(0.0, 1.0)),
        ModuleValue::Count {
            current,
            total: Some(total),
        } if *total > 0 => Some((*current as f32 / *total as f32).clamp(0.0, 1.0)),
        ModuleValue::Usage(usage) => usage
            .metrics
            .first()
            .map(|metric| (metric.percent / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn ellipsize(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let keep = max_chars - 3;
    let head = keep / 2;
    let tail = keep - head;
    let start = value.chars().take(head).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}

fn status_badge(status: ModuleStatus) -> El {
    status_badge_with_label(
        status,
        match status {
            ModuleStatus::Ok => "ok",
            ModuleStatus::Info => "info",
            ModuleStatus::Warning => "warn",
            ModuleStatus::Critical => "crit",
            ModuleStatus::Unknown => "idle",
        },
    )
}

fn status_badge_with_label(status: ModuleStatus, label: impl Into<String>) -> El {
    let badge = badge(label);
    match status {
        ModuleStatus::Ok => badge.success(),
        ModuleStatus::Info => badge.info(),
        ModuleStatus::Warning => badge.warning(),
        ModuleStatus::Critical => badge.destructive(),
        ModuleStatus::Unknown => badge.muted(),
    }
}

fn status_color(status: ModuleStatus) -> Color {
    match status {
        ModuleStatus::Ok => tokens::SUCCESS,
        ModuleStatus::Info => tokens::INFO,
        ModuleStatus::Warning => tokens::WARNING,
        ModuleStatus::Critical => tokens::DESTRUCTIVE,
        ModuleStatus::Unknown => tokens::MUTED_FOREGROUND,
    }
}

fn metric_color(percent: f32) -> Color {
    if percent >= 80.0 {
        tokens::DESTRUCTIVE
    } else if percent >= 50.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrandIconKind {
    GitHub,
    OpenAi,
    Anthropic,
}

fn module_brand_icon(module: &ModuleSnapshot) -> Option<SvgIcon> {
    match module_brand_icon_kind(module)? {
        BrandIconKind::GitHub => Some(ICON_GITHUB.clone()),
        BrandIconKind::OpenAi => Some(ICON_OPENAI.clone()),
        BrandIconKind::Anthropic => Some(ICON_ANTHROPIC.clone()),
    }
}

fn module_brand_icon_kind(module: &ModuleSnapshot) -> Option<BrandIconKind> {
    let id = module.id.to_ascii_lowercase();
    let title = module.title.to_ascii_lowercase();
    if id.starts_with("codex") || title.starts_with("codex") || title.contains("openai") {
        return Some(BrandIconKind::OpenAi);
    }
    if id.starts_with("claude") || title.starts_with("claude") || title.contains("anthropic") {
        return Some(BrandIconKind::Anthropic);
    }
    if id.contains('/') || title.contains('/') || title.contains("github") {
        return Some(BrandIconKind::GitHub);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_module(metrics: Vec<UsageMetric>, detail: Option<&str>) -> ModuleSnapshot {
        ModuleSnapshot {
            id: "usage".into(),
            title: "codex".into(),
            value: ModuleValue::Usage(UsageValue {
                metrics,
                detail: detail.map(ToOwned::to_owned),
            }),
            status: ModuleStatus::Ok,
            updated_at: None,
            stale_after: None,
        }
    }

    fn metric(label: &str, percent: f32) -> UsageMetric {
        UsageMetric {
            label: label.into(),
            percent,
        }
    }

    #[test]
    fn headline_is_the_first_gauge() {
        let usage = UsageValue {
            metrics: vec![metric("5h", 18.0), metric("7d", 7.0)],
            detail: Some("pro · resets Thu 10:41".into()),
        };
        assert_eq!(usage_headline(&usage), "5h 18%");
        assert_eq!(
            usage_headline(&UsageValue {
                metrics: vec![],
                detail: None,
            }),
            "usage"
        );
    }

    #[test]
    fn module_fraction_tracks_the_headline_gauge() {
        let module = usage_module(vec![metric("5h", 42.0)], None);
        assert_eq!(module_fraction(&module), Some(0.42));
        assert_eq!(module_fraction(&usage_module(vec![], None)), None);
    }

    #[test]
    fn usage_detail_passes_through_without_reparsing() {
        let module = usage_module(vec![metric("5h", 1.0)], Some("pro · credits 0"));
        assert_eq!(
            module_detail_text(&module).as_deref(),
            Some("pro · credits 0")
        );
    }

    #[test]
    fn classifies_brand_icons_from_module_identity() {
        assert_eq!(
            module_brand_icon_kind(&module_with_identity("codex", "codex default")),
            Some(BrandIconKind::OpenAi)
        );
        assert_eq!(
            module_brand_icon_kind(&module_with_identity("claude", "claude work")),
            Some(BrandIconKind::Anthropic)
        );
        assert_eq!(
            module_brand_icon_kind(&module_with_identity(
                "computer-whisperer/prism",
                "computer-whisperer/prism"
            )),
            Some(BrandIconKind::GitHub)
        );
        assert_eq!(
            module_brand_icon_kind(&module_with_identity("clock", "clock")),
            None
        );
    }

    #[test]
    fn parses_brand_icon_assets() {
        assert!(module_brand_icon(&module_with_identity("codex", "codex default")).is_some());
        assert!(module_brand_icon(&module_with_identity("claude", "claude work")).is_some());
        assert!(module_brand_icon(&module_with_identity(
            "computer-whisperer/prism",
            "computer-whisperer/prism"
        ))
        .is_some());
    }

    fn module_with_identity(id: &str, title: &str) -> ModuleSnapshot {
        ModuleSnapshot {
            id: id.into(),
            title: title.into(),
            value: ModuleValue::Text("value".into()),
            status: ModuleStatus::Ok,
            updated_at: None,
            stale_after: None,
        }
    }
}
