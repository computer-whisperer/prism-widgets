//! Damascene projection for panel snapshots.

use std::sync::LazyLock;

use damascene_core::prelude::*;
use prism_widgets_core::{
    ModuleSnapshot, ModuleStatus, ModuleValue, PanelAnchor, PanelAppearance, PanelLayout,
    PanelSnapshot, ThemeName,
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
pub struct WidgetsApp {
    view: PanelView,
}

impl WidgetsApp {
    pub fn new(
        appearance: PanelAppearance,
        anchor: PanelAnchor,
        width: Option<u32>,
        snapshot: PanelSnapshot,
    ) -> Self {
        Self {
            view: PanelView::new(appearance, anchor, PanelLayout::Bar, width, snapshot),
        }
    }

    pub fn set_snapshot(&mut self, snapshot: PanelSnapshot) {
        self.view.snapshot = snapshot;
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

impl App for WidgetsApp {
    fn theme(&self) -> Theme {
        theme_of(self.view.appearance.theme)
    }

    fn build(&self, _cx: &BuildCx) -> El {
        overlays(
            cluster_shell(self.view.layout, std::slice::from_ref(&self.view)),
            Vec::<Option<El>>::new(),
        )
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
    let title = ellipsize(&view.snapshot.panel_id.0, 32);

    let panel = card([
        card_header([card_title(title)]).padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2)),
        card_content([item_group(modules)]).padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_2)),
    ])
    .opacity(view.appearance.opacity)
    .clip();
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
    }
}

fn module_detail_text(module: &ModuleSnapshot) -> Option<String> {
    match &module.value {
        ModuleValue::State {
            detail: Some(detail),
            ..
        } => {
            let detail = strip_percent_segments(detail);
            (!detail.is_empty()).then(|| ellipsize(&detail, 56))
        }
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UsageMetric {
    label: String,
    percent: f32,
}

fn module_usage_metrics(module: &ModuleSnapshot) -> Vec<UsageMetric> {
    let ModuleValue::State { label, detail } = &module.value else {
        return Vec::new();
    };
    let mut metrics = Vec::new();
    collect_usage_metrics(label, &mut metrics);
    if let Some(detail) = detail {
        collect_usage_metrics(detail, &mut metrics);
    }
    metrics
}

fn collect_usage_metrics(value: &str, metrics: &mut Vec<UsageMetric>) {
    let mut offset = 0;
    while let Some(relative_percent_index) = value[offset..].find('%') {
        let percent_index = offset + relative_percent_index;
        let mut number_start = percent_index;
        while let Some((previous_index, ch)) = value[..number_start].char_indices().next_back() {
            if ch.is_ascii_digit() || ch == '.' || ch.is_whitespace() {
                number_start = previous_index;
            } else {
                break;
            }
        }

        let number = value[number_start..percent_index].trim();
        if let Ok(percent) = number.parse::<f32>() {
            let label = metric_label(&value[offset..number_start]);
            metrics.push(UsageMetric { label, percent });
        }

        offset = percent_index + 1;
    }
}

fn metric_label(prefix: &str) -> String {
    let label = prefix
        .rsplit(metric_separator)
        .next()
        .unwrap_or(prefix)
        .trim();
    if label.is_empty() {
        "usage".into()
    } else {
        ellipsize(label, 18)
    }
}

fn usage_metric_bar(metric: &UsageMetric) -> El {
    let fraction = (metric.percent / 100.0).clamp(0.0, 1.0);
    column([
        row([
            text(ellipsize(&metric.label, 18)).caption().muted(),
            spacer(),
            text(format!("{:.0}%", metric.percent.clamp(0.0, 999.0)))
                .caption()
                .muted(),
        ])
        .align(Align::Center),
        progress_with_color(fraction, metric_color(metric.percent))
            .height(Size::Fixed(7.0))
            .width(Size::Fill(1.0)),
    ])
    .gap(3.0)
    .align(Align::Stretch)
}

fn strip_percent_segments(detail: &str) -> String {
    detail
        .split(metric_separator)
        .map(str::trim)
        .filter(|segment| !segment.is_empty() && !segment.contains('%'))
        .collect::<Vec<_>>()
        .join(" - ")
}

fn metric_separator(ch: char) -> bool {
    matches!(ch, '|' | ',' | ';' | '/' | '-') || ch == '\u{00b7}'
}

fn module_fraction(module: &ModuleSnapshot) -> Option<f32> {
    match &module.value {
        ModuleValue::Percent(frac) => Some(frac.clamp(0.0, 1.0)),
        ModuleValue::Count {
            current,
            total: Some(total),
        } if *total > 0 => Some((*current as f32 / *total as f32).clamp(0.0, 1.0)),
        ModuleValue::State { label, detail } => percent_in_text(label)
            .or_else(|| detail.as_deref().and_then(percent_in_text))
            .map(|percent| (percent / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn percent_in_text(value: &str) -> Option<f32> {
    let percent_index = value.find('%')?;
    let prefix = &value[..percent_index];
    let number = prefix
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    number.parse().ok()
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

    #[test]
    fn extracts_percent_from_usage_labels() {
        assert_eq!(percent_in_text("5h 18%"), Some(18.0));
        assert_eq!(percent_in_text("7d 7% - pro"), Some(7.0));
        assert_eq!(percent_in_text("idle"), None);
    }

    #[test]
    fn derives_module_fraction_from_state_detail() {
        let module = ModuleSnapshot {
            id: "usage".into(),
            title: "codex".into(),
            value: ModuleValue::State {
                label: "usage".into(),
                detail: Some("7d 42% - pro".into()),
            },
            status: ModuleStatus::Ok,
            updated_at: None,
            stale_after: None,
        };

        assert_eq!(module_fraction(&module), Some(0.42));
    }

    #[test]
    fn extracts_multiple_usage_metrics_from_state() {
        let module = ModuleSnapshot {
            id: "usage".into(),
            title: "codex".into(),
            value: ModuleValue::State {
                label: "5h 18%".into(),
                detail: Some("7d 7% - pro - credits 0".into()),
            },
            status: ModuleStatus::Ok,
            updated_at: None,
            stale_after: None,
        };

        assert_eq!(
            module_usage_metrics(&module),
            vec![
                UsageMetric {
                    label: "5h".into(),
                    percent: 18.0,
                },
                UsageMetric {
                    label: "7d".into(),
                    percent: 7.0,
                },
            ]
        );
    }

    #[test]
    fn strips_percent_segments_from_sidebar_detail() {
        assert_eq!(
            strip_percent_segments("7d 7% - pro - credits 0 - resets Thu 10:41"),
            "pro - credits 0 - resets Thu 10:41"
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
