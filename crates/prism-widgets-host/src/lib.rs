//! Provider-free host abstractions.
//!
//! This crate is where the `prism-bar`/`prism-widgets` common runner can
//! eventually live. Keep application integrations out of this crate: no
//! GitHub, no subscription APIs, no command-specific parsing.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};

use damascene_core::prelude::{App, Rect, Theme};
use damascene_core::BuildCx;
use damascene_wgpu::{MsaaTarget, Runner, RunnerCaps};

use prism_widgets_core::{PanelAnchor, PanelId, PanelLayer, PanelLayout, PanelSnapshot, PanelSpec};
use prism_widgets_ui::{PanelView, WidgetsBandApp};

const MSAA_SAMPLES: u32 = 4;
const SNAPSHOT_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq)]
pub struct HostConfig {
    pub panels: Vec<PanelSpec>,
}

impl HostConfig {
    pub fn panel_ids(&self) -> impl Iterator<Item = &PanelId> {
        self.panels.iter().map(|panel| &panel.id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    PanelSnapshot(PanelSnapshot),
    ConfigReloaded(HostConfig),
    Quit,
}

pub trait PanelDataSource {
    fn snapshot_for(&self, panel_id: &PanelId) -> PanelSnapshot;
}

/// Minimal synchronous runner used by the dry-run binary and tests.
///
/// The real layer-shell runner should keep this boundary: it consumes
/// panel specs and snapshots, while provider scheduling stays above it.
#[derive(Clone, Debug)]
pub struct PanelRunner {
    config: HostConfig,
}

impl PanelRunner {
    pub fn new(config: HostConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    pub fn snapshots(&self, source: &impl PanelDataSource) -> Vec<PanelSnapshot> {
        self.config
            .panel_ids()
            .map(|panel_id| source.snapshot_for(panel_id))
            .collect()
    }
}

/// Run configured panels as `wlr-layer-shell` surfaces.
///
/// This is intentionally provider-free: callers supply a data source,
/// and this host only turns panel specs plus snapshots into surfaces.
pub fn run_layer_shell(config: HostConfig, source: Box<dyn PanelDataSource>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) =
        registry_queue_init::<LayerHost>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let mut host = LayerHost {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        conn: conn.clone(),
        config,
        source,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
        gpu: None,
        surfaces: Vec::new(),
        dirty: false,
        next_snapshot_poll: Instant::now() + SNAPSHOT_POLL,
        exit: false,
    };

    let mut event_loop: EventLoop<LayerHost> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;

    while !host.exit {
        let now = Instant::now();
        let mut timeout = host.next_snapshot_poll.saturating_duration_since(now);
        for surface in &host.surfaces {
            if let Some(deadline) = surface.anim_deadline {
                timeout = timeout.min(deadline.saturating_duration_since(now));
            }
        }

        event_loop
            .dispatch(Some(timeout), &mut host)
            .context("event loop dispatch")?;

        let now = Instant::now();
        if host.next_snapshot_poll <= now {
            host.next_snapshot_poll = now + SNAPSHOT_POLL;
            for surface in &mut host.surfaces {
                surface.dirty = true;
            }
        }
        if host.dirty {
            host.dirty = false;
            for surface in &mut host.surfaces {
                surface.dirty = true;
            }
        }
        for surface in &mut host.surfaces {
            if surface
                .anim_deadline
                .is_some_and(|deadline| deadline <= now)
            {
                surface.anim_deadline = None;
                surface.dirty = true;
            }
        }
        for i in 0..host.surfaces.len() {
            if host.surfaces[i].dirty {
                host.draw(i);
            }
        }
    }

    Ok(())
}

struct GpuShared {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

struct Swapchain {
    config: wgpu::SurfaceConfiguration,
    msaa: Option<MsaaTarget>,
    runner: Runner,
}

struct PanelSurface {
    // Drop order: the wgpu surface borrows the wl_surface kept alive by
    // `layer`, so it must drop first.
    wgpu_surface: wgpu::Surface<'static>,
    swapchain: Option<Swapchain>,
    layer: LayerSurface,
    output: wl_output::WlOutput,
    output_name: String,
    panels: Vec<PanelSpec>,
    app: WidgetsBandApp,
    width: u32,
    height: u32,
    scale: i32,
    dirty: bool,
    anim_deadline: Option<Instant>,
}

struct LayerHost {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    conn: Connection,
    config: HostConfig,
    source: Box<dyn PanelDataSource>,
    instance: wgpu::Instance,
    gpu: Option<GpuShared>,
    surfaces: Vec<PanelSurface>,
    dirty: bool,
    next_snapshot_poll: Instant,
    exit: bool,
}

impl LayerHost {
    fn create_panel_group(
        &mut self,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
        output_name: String,
        panels: Vec<PanelSpec>,
    ) {
        let Some(first_panel) = panels.first() else {
            return;
        };
        let panel_label = panel_label(&panels);
        tracing::info!(panels = %panel_label, output = %output_name, "creating panel surface");
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            layer_of(first_panel.geometry.layer),
            Some("prism-widgets"),
            Some(&output),
        );
        apply_layer_group_geometry(&panels, &layer);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();

        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).expect("display ptr"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).expect("surface ptr"),
        ));
        let wgpu_surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
        }
        .expect("create wgpu surface on layer surface");

        if self.gpu.is_none() {
            let adapter =
                pollster::block_on(self.instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::default(),
                    compatible_surface: Some(&wgpu_surface),
                    force_fallback_adapter: false,
                }))
                .expect("no compatible adapter");
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("prism-widgets::device"),
                    ..Default::default()
                }))
                .expect("request device");
            tracing::info!(backend = ?adapter.get_info().backend, "gpu initialized");
            self.gpu = Some(GpuShared {
                adapter,
                device,
                queue,
            });
        }

        let layout = first_panel.layout;
        let app = WidgetsBandApp::new(layout, self.panel_views(&panels));
        let height = panels
            .iter()
            .map(|panel| panel.geometry.height)
            .max()
            .unwrap_or(1);
        self.surfaces.push(PanelSurface {
            wgpu_surface,
            swapchain: None,
            layer,
            output,
            output_name,
            panels,
            app,
            width: 1,
            height,
            scale: 1,
            dirty: false,
            anim_deadline: None,
        });
    }

    fn configure_swapchain(&mut self, i: usize) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let surface = &mut self.surfaces[i];
        let scale = surface.scale as u32;
        let width = (surface.width * scale).max(1);
        let height = (surface.height * scale).max(1);
        setup_swapchain(
            gpu,
            &surface.wgpu_surface,
            &mut surface.swapchain,
            (width, height),
            surface.app.theme(),
            &format!("{}:{}", surface.output_name, panel_label(&surface.panels)),
        );
    }

    fn draw(&mut self, i: usize) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let surface = &mut self.surfaces[i];
        surface.dirty = false;
        let Some(sc) = surface.swapchain.as_mut() else {
            return;
        };

        surface.app.set_views(panel_views_from_source(
            self.source.as_ref(),
            &surface.panels,
        ));
        let outcome = render_frame(
            gpu,
            &surface.wgpu_surface,
            sc,
            &mut surface.app,
            (surface.width, surface.height),
            surface.scale,
            &format!("{}:{}", surface.output_name, panel_label(&surface.panels)),
        );
        surface.dirty = outcome.retry;
        surface.anim_deadline = outcome.anim_deadline;
    }

    fn surface_index_for(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.layer.wl_surface() == surface)
    }

    fn create_wanted_panels_for_output(
        &mut self,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
        output_name: String,
    ) {
        let panels: Vec<_> = self
            .config
            .panels
            .iter()
            .filter(|panel| wants_output(panel, &output_name))
            .filter(|panel| !self.has_surface(&panel.id, &output))
            .cloned()
            .collect();
        let mut reserved_by_group: HashMap<PanelGroupKey, Vec<PanelSpec>> = HashMap::new();
        let mut overlays = Vec::new();
        for panel in panels {
            if panel.geometry.exclusive_zone > 0 {
                let key = PanelGroupKey {
                    edge: exclusive_edge(panel.geometry.anchor),
                    layout: panel.layout,
                };
                reserved_by_group.entry(key).or_default().push(panel);
            } else {
                overlays.push(panel);
            }
        }

        let mut groups = reserved_by_group.into_iter().collect::<Vec<_>>();
        groups.sort_by_key(|(key, _)| (key.edge, key.layout));
        for (_, panels) in groups {
            self.create_panel_group(qh, output.clone(), output_name.clone(), panels);
        }
        for panel in overlays {
            self.create_panel_group(qh, output.clone(), output_name.clone(), vec![panel]);
        }
    }

    fn has_surface(&self, panel_id: &PanelId, output: &wl_output::WlOutput) -> bool {
        self.surfaces.iter().any(|surface| {
            surface.output == *output && surface.panels.iter().any(|panel| panel.id == *panel_id)
        })
    }

    fn panel_views(&self, panels: &[PanelSpec]) -> Vec<PanelView> {
        panel_views_from_source(self.source.as_ref(), panels)
    }
}

impl LayerShellHandler for LayerHost {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces
            .retain(|surface| surface.layer.wl_surface() != layer.wl_surface());
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(i) = self.surface_index_for(layer.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        {
            let surface = &mut self.surfaces[i];
            if width > 0 {
                surface.width = width;
            }
            if height > 0 {
                surface.height = height;
            }
        }
        self.configure_swapchain(i);
        self.surfaces[i].dirty = true;
    }
}

impl CompositorHandler for LayerHost {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(i) = self.surface_index_for(surface) else {
            return;
        };
        if self.surfaces[i].scale != new_factor {
            self.surfaces[i].scale = new_factor;
            surface.set_buffer_scale(new_factor);
            if self.surfaces[i].swapchain.is_some() {
                self.configure_swapchain(i);
            }
            self.surfaces[i].dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for LayerHost {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let Some(name) = self.output_state.info(&output).and_then(|info| info.name) else {
            tracing::warn!("output without a name; skipping");
            return;
        };
        self.create_wanted_panels_for_output(qh, output, name);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|surface| surface.output != output);
    }
}

impl ProvidesRegistryState for LayerHost {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(LayerHost);
delegate_output!(LayerHost);
delegate_layer!(LayerHost);
delegate_registry!(LayerHost);

fn wants_output(panel: &PanelSpec, output_name: &str) -> bool {
    panel
        .output
        .as_deref()
        .is_none_or(|name| name == output_name)
}

fn panel_label(panels: &[PanelSpec]) -> String {
    panels
        .iter()
        .map(|panel| panel.id.0.as_str())
        .collect::<Vec<_>>()
        .join("+")
}

fn panel_views_from_source(source: &dyn PanelDataSource, panels: &[PanelSpec]) -> Vec<PanelView> {
    panels
        .iter()
        .map(|panel| {
            PanelView::new(
                panel.appearance.clone(),
                panel.geometry.anchor,
                panel.layout,
                panel.geometry.width,
                source.snapshot_for(&panel.id),
            )
        })
        .collect()
}

fn layer_of(layer: PanelLayer) -> Layer {
    match layer {
        PanelLayer::Background => Layer::Background,
        PanelLayer::Bottom => Layer::Bottom,
        PanelLayer::Top => Layer::Top,
        PanelLayer::Overlay => Layer::Overlay,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ExclusiveEdge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PanelGroupKey {
    edge: ExclusiveEdge,
    layout: PanelLayout,
}

fn exclusive_edge(anchor: PanelAnchor) -> ExclusiveEdge {
    match anchor {
        PanelAnchor::TopLeft | PanelAnchor::Top | PanelAnchor::TopRight => ExclusiveEdge::Top,
        PanelAnchor::BottomLeft | PanelAnchor::Bottom | PanelAnchor::BottomRight => {
            ExclusiveEdge::Bottom
        }
        PanelAnchor::Left => ExclusiveEdge::Left,
        PanelAnchor::Right => ExclusiveEdge::Right,
    }
}

fn anchors(anchor: PanelAnchor) -> Anchor {
    match anchor {
        PanelAnchor::TopLeft => Anchor::TOP | Anchor::LEFT,
        PanelAnchor::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        PanelAnchor::TopRight => Anchor::TOP | Anchor::RIGHT,
        PanelAnchor::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        PanelAnchor::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        PanelAnchor::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
        PanelAnchor::Left => Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT,
        PanelAnchor::Right => Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT,
    }
}

fn layer_anchors(panel: &PanelSpec) -> Anchor {
    if panel.geometry.exclusive_zone <= 0 {
        return anchors(panel.geometry.anchor);
    }

    // Smithay only applies an exclusive zone when it can infer exactly
    // one edge from the anchor. Corner anchors such as top-left are
    // ambiguous, so reserving panels span the relevant edge and the UI
    // aligns the visible card inside the transparent full-edge surface.
    match panel.geometry.anchor {
        PanelAnchor::TopLeft | PanelAnchor::TopRight => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        PanelAnchor::BottomLeft | PanelAnchor::BottomRight => {
            Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT
        }
        PanelAnchor::Left => Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT,
        PanelAnchor::Right => Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT,
        PanelAnchor::Top | PanelAnchor::Bottom => anchors(panel.geometry.anchor),
    }
}

fn apply_layer_group_geometry(panels: &[PanelSpec], layer: &LayerSurface) {
    if panels.len() == 1 {
        apply_layer_geometry(&panels[0], layer);
        return;
    }

    let edge = exclusive_edge(panels[0].geometry.anchor);
    let margin = panels
        .iter()
        .map(|panel| panel.geometry.margin)
        .max()
        .unwrap_or(0);
    let height = panels
        .iter()
        .map(|panel| panel.geometry.height)
        .max()
        .unwrap_or(1);
    let width = panels
        .iter()
        .filter_map(|panel| panel.geometry.width)
        .max()
        .unwrap_or(height);
    let exclusive_zone = panels
        .iter()
        .map(|panel| panel.geometry.exclusive_zone)
        .max()
        .unwrap_or(-1);

    match edge {
        ExclusiveEdge::Top => {
            layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
            layer.set_margin(margin, margin, 0, margin);
            layer.set_size(0, height);
        }
        ExclusiveEdge::Bottom => {
            layer.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_margin(0, margin, margin, margin);
            layer.set_size(0, height);
        }
        ExclusiveEdge::Left => {
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT);
            layer.set_margin(margin, 0, margin, margin);
            layer.set_size(width.max(height), 0);
        }
        ExclusiveEdge::Right => {
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT);
            layer.set_margin(margin, margin, margin, 0);
            layer.set_size(width.max(height), 0);
        }
    }
    layer.set_exclusive_zone(exclusive_zone);
}

fn apply_layer_geometry(panel: &PanelSpec, layer: &LayerSurface) {
    let margin = panel.geometry.margin;
    let layer_anchor = layer_anchors(panel);
    layer.set_anchor(layer_anchor);
    match panel.geometry.anchor {
        PanelAnchor::TopLeft if layer_anchor.contains(Anchor::RIGHT) => {
            layer.set_margin(margin, margin, 0, margin)
        }
        PanelAnchor::TopLeft => layer.set_margin(margin, 0, 0, margin),
        PanelAnchor::Top => layer.set_margin(margin, margin, 0, margin),
        PanelAnchor::TopRight if layer_anchor.contains(Anchor::LEFT) => {
            layer.set_margin(margin, margin, 0, margin)
        }
        PanelAnchor::TopRight => layer.set_margin(margin, margin, 0, 0),
        PanelAnchor::BottomLeft if layer_anchor.contains(Anchor::RIGHT) => {
            layer.set_margin(0, margin, margin, margin)
        }
        PanelAnchor::BottomLeft => layer.set_margin(0, 0, margin, margin),
        PanelAnchor::Bottom => layer.set_margin(0, margin, margin, margin),
        PanelAnchor::BottomRight if layer_anchor.contains(Anchor::LEFT) => {
            layer.set_margin(0, margin, margin, margin)
        }
        PanelAnchor::BottomRight => layer.set_margin(0, margin, margin, 0),
        PanelAnchor::Left => layer.set_margin(margin, 0, margin, margin),
        PanelAnchor::Right => layer.set_margin(margin, margin, margin, 0),
    }

    let width = panel.geometry.width.unwrap_or(0);
    let height = panel.geometry.height;
    match layer_anchor {
        anchor if anchor.anchored_horizontally() => layer.set_size(0, height),
        anchor if anchor.anchored_vertically() => layer.set_size(width.max(height), 0),
        _ => layer.set_size(width, height),
    }
    layer.set_exclusive_zone(panel.geometry.exclusive_zone);
}

trait AnchorExt {
    fn anchored_horizontally(self) -> bool;
    fn anchored_vertically(self) -> bool;
}

impl AnchorExt for Anchor {
    fn anchored_horizontally(self) -> bool {
        self.contains(Self::LEFT) && self.contains(Self::RIGHT)
    }

    fn anchored_vertically(self) -> bool {
        self.contains(Self::TOP) && self.contains(Self::BOTTOM)
    }
}

fn setup_swapchain(
    gpu: &GpuShared,
    wgpu_surface: &wgpu::Surface<'_>,
    swapchain: &mut Option<Swapchain>,
    (width, height): (u32, u32),
    theme: Theme,
    label: &str,
) {
    match swapchain {
        Some(sc) => {
            if sc.config.width == width && sc.config.height == height {
                return;
            }
            sc.config.width = width;
            sc.config.height = height;
            wgpu_surface.configure(&gpu.device, &sc.config);
            sc.runner.set_surface_size(width, height);
            let extent = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            if let Some(msaa) = sc.msaa.as_mut() {
                if !msaa.matches(extent) {
                    *msaa =
                        MsaaTarget::new(&gpu.device, sc.config.format, extent, msaa.sample_count);
                }
            }
        }
        None => {
            let caps = wgpu_surface.get_capabilities(&gpu.adapter);
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|format| format.is_srgb())
                .unwrap_or(caps.formats[0]);
            let alpha_mode = if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
            {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                tracing::warn!(
                    surface = %label,
                    modes = ?caps.alpha_modes,
                    "no premultiplied alpha; surface will be opaque"
                );
                caps.alpha_modes[0]
            };
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 1,
            };
            wgpu_surface.configure(&gpu.device, &config);

            let mut runner = Runner::with_caps(
                &gpu.device,
                &gpu.queue,
                format,
                MSAA_SAMPLES,
                RunnerCaps::from_adapter(&gpu.adapter),
            );
            runner.set_theme(theme);
            runner.set_surface_size(width, height);
            runner.warm_default_glyphs();

            let msaa = (MSAA_SAMPLES > 1).then(|| {
                MsaaTarget::new(
                    &gpu.device,
                    format,
                    wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    MSAA_SAMPLES,
                )
            });
            tracing::info!(surface = %label, ?format, "swapchain configured");
            *swapchain = Some(Swapchain {
                config,
                msaa,
                runner,
            });
        }
    }
}

struct FrameOutcome {
    retry: bool,
    anim_deadline: Option<Instant>,
}

fn render_frame<A: App>(
    gpu: &GpuShared,
    wgpu_surface: &wgpu::Surface<'_>,
    sc: &mut Swapchain,
    app: &mut A,
    (width, height): (u32, u32),
    scale: i32,
    label: &str,
) -> FrameOutcome {
    let viewport = Rect::new(0.0, 0.0, width as f32, height as f32);

    app.before_build();
    let theme = app.theme();
    let mut tree = {
        let cx = BuildCx::new(&theme)
            .with_ui_state(sc.runner.ui_state())
            .with_viewport(viewport.w, viewport.h);
        app.build(&cx)
    };
    sc.runner.set_theme(theme);
    sc.runner.set_hotkeys(app.hotkeys());

    let prepare = sc
        .runner
        .prepare(&gpu.device, &gpu.queue, &mut tree, viewport, scale as f32);

    let frame = match wgpu_surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(texture)
        | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
        wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
            wgpu_surface.configure(&gpu.device, &sc.config);
            return FrameOutcome {
                retry: true,
                anim_deadline: None,
            };
        }
        other => {
            tracing::error!(surface = %label, "surface unavailable: {other:?}");
            return FrameOutcome {
                retry: false,
                anim_deadline: None,
            };
        }
    };
    let view = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("prism-widgets::encoder"),
        });
    sc.runner.render(
        &gpu.device,
        &mut encoder,
        &frame.texture,
        &view,
        sc.msaa.as_ref().map(|msaa| &msaa.view),
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
    );
    gpu.queue.submit(Some(encoder.finish()));
    frame.present();

    let mut anim_deadline = prepare.next_redraw_in.map(|delay| Instant::now() + delay);
    if prepare.needs_redraw && anim_deadline.is_none() {
        anim_deadline = Some(Instant::now());
    }
    FrameOutcome {
        retry: false,
        anim_deadline,
    }
}
