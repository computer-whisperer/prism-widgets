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
    let status = status_badge(module.status);
    let value = match &module.value {
        ModuleValue::Text(value) => text(ellipsize(value, 32)).label(),
        ModuleValue::Percent(frac) => text(format!("{:.0}%", frac * 100.0)).label(),
        ModuleValue::Count { current, total } => match total {
            Some(total) => text(format!("{current}/{total}")).label(),
            None => text(current.to_string()).label(),
        },
        ModuleValue::State { label, detail } => {
            let mut cells = vec![text(ellipsize(label, 24)).label()];
            if let Some(detail) = detail {
                cells.push(text(ellipsize(detail, 24)).caption().muted());
            }
            row(cells).gap(tokens::SPACE_1).align(Align::Center)
        }
    };
    row([
        status,
        text(ellipsize(&module.title, 28)).caption().muted(),
        value,
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .padding(Sides::x(tokens::SPACE_2))
    .radius(tokens::RADIUS_SM)
}

fn sidebar_module_item(module: &ModuleSnapshot) -> El {
    item([
        item_content([
            item_title(ellipsize(&module.title, 30)),
            item_description(module_description(module)),
        ]),
        item_actions([status_badge(module.status)]),
    ])
}

fn module_description(module: &ModuleSnapshot) -> String {
    match &module.value {
        ModuleValue::Text(value) => ellipsize(value, 48),
        ModuleValue::Percent(frac) => format!("{:.0}%", frac * 100.0),
        ModuleValue::Count { current, total } => match total {
            Some(total) => format!("{current}/{total}"),
            None => current.to_string(),
        },
        ModuleValue::State { label, detail } => match detail {
            Some(detail) => format!("{} - {}", ellipsize(label, 24), ellipsize(detail, 24)),
            None => ellipsize(label, 48),
        },
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
    let label = match status {
        ModuleStatus::Ok => "ok",
        ModuleStatus::Info => "info",
        ModuleStatus::Warning => "warn",
        ModuleStatus::Critical => "crit",
        ModuleStatus::Unknown => "idle",
    };
    match status {
        ModuleStatus::Ok => badge(label).success(),
        ModuleStatus::Info => badge(label).info(),
        ModuleStatus::Warning => badge(label).warning(),
        ModuleStatus::Critical => badge(label).destructive(),
        ModuleStatus::Unknown => badge(label).muted(),
    }
}
