//! Damascene projection for panel snapshots.

use damascene_core::prelude::*;
use prism_widgets_core::{
    ModuleSnapshot, ModuleStatus, ModuleValue, PanelAnchor, PanelAppearance, PanelLayout,
    PanelSnapshot, ThemeName,
};

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
    let mut cells = vec![text(ellipsize(&module.title, 22)).caption().muted()];
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
    let mut content = vec![row([
        text(ellipsize(&module.title, 30)).label(),
        spacer(),
        module_value_summary(module),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)];
    if let Some(detail) = module_detail_text(module) {
        content.push(text(detail).caption().muted());
    }
    if let Some(fraction) = module_fraction(module) {
        content.push(
            progress_with_color(fraction, status_color(module.status))
                .height(Size::Fixed(5.0))
                .width(Size::Fill(1.0)),
        );
    }

    item([
        item_content(content).gap(tokens::SPACE_1),
        item_actions([status_badge(module.status)]),
    ])
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
        } => Some(ellipsize(detail, 56)),
        _ => None,
    }
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
}
