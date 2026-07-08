//! Provider-free host abstractions.
//!
//! This crate is where the `prism-bar`/`prism-widgets` common runner can
//! eventually live. Keep application integrations out of this crate: no
//! GitHub, no subscription APIs, no command-specific parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::{
    channel::{channel, Event as ChannelEvent},
    generic::Generic,
    EventLoop, Interest, Mode, PostAction,
};
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

use prism_widgets_core::{
    clock_snapshot, ModuleSnapshot, ModuleSpec, ModuleUpdate, PanelAnchor, PanelId, PanelLayer,
    PanelLayout, PanelSnapshot, PanelSpec,
};
use prism_widgets_ui::{PanelView, WidgetsBandApp};

/// Cross-thread sender used by provider workers to push snapshots into the
/// host event loop. Re-exported so provider crates do not need a direct
/// calloop dependency.
pub use smithay_client_toolkit::reexports::calloop::channel::Sender;

const MSAA_SAMPLES: u32 = 4;
const CLOCK_TICK: Duration = Duration::from_secs(1);
const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// Channel sender handed to provider workers.
pub type SnapshotSender = Sender<ModuleUpdate>;

/// Spawns a provider generation against a set of panel specs, pushing
/// snapshots into the given sender tagged with the given epoch. The returned
/// handle owns the worker threads; dropping it shuts that generation down.
///
/// This is the only seam between the provider-free host and the provider
/// crate: the host calls it on startup and on every reload, and never learns
/// what the providers actually do.
pub type ProviderSpawner =
    Box<dyn Fn(&[PanelSpec], SnapshotSender, u64) -> Box<dyn ProviderHandle>>;

/// Opaque handle to a running provider generation. Dropping it must stop the
/// generation's workers (after any in-flight fetch completes).
pub trait ProviderHandle {}

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

/// Latest snapshots assembled by the host, populated from worker threads via
/// the snapshot channel. Reads are lock-free: this lives on the event-loop
/// thread and is only mutated by channel callbacks, never by providers.
///
/// Clock modules are rendered on read from the current time; all other
/// modules are served from the last value a worker pushed, falling back to a
/// loading placeholder until the first fetch lands.
struct SnapshotCache {
    panels: HashMap<String, PanelSpec>,
    modules: HashMap<(String, String), ModuleSnapshot>,
}

impl SnapshotCache {
    fn from_specs(specs: &[PanelSpec]) -> Self {
        let panels = specs
            .iter()
            .map(|panel| (panel.id.0.clone(), panel.clone()))
            .collect();
        Self {
            panels,
            modules: HashMap::new(),
        }
    }

    /// Apply a worker update, returning whether it changed what would be drawn.
    fn apply(&mut self, update: ModuleUpdate) -> bool {
        let key = (update.panel.0, update.module);
        let changed = self
            .modules
            .get(&key)
            .is_none_or(|old| !old.display_eq(&update.snapshot));
        self.modules.insert(key, update.snapshot);
        changed
    }
}

impl PanelDataSource for SnapshotCache {
    fn snapshot_for(&self, panel_id: &PanelId) -> PanelSnapshot {
        self.panels
            .get(&panel_id.0)
            .map(|panel| PanelSnapshot {
                panel_id: panel.id.clone(),
                modules: panel
                    .modules
                    .iter()
                    .map(|spec| match spec {
                        ModuleSpec::Clock(clock) => clock_snapshot(clock),
                        _ => self
                            .modules
                            .get(&(panel.id.0.clone(), spec.id().to_string()))
                            .cloned()
                            .unwrap_or_else(|| ModuleSnapshot::loading(spec.id(), spec.id())),
                    })
                    .collect(),
            })
            .unwrap_or_else(|| PanelSnapshot::empty(panel_id.clone()))
    }
}

fn config_has_clock(config: &HostConfig) -> bool {
    config
        .panels
        .iter()
        .flat_map(|panel| &panel.modules)
        .any(|module| matches!(module, ModuleSpec::Clock(_)))
}

pub struct ConfigReloader {
    path: PathBuf,
    reload: Box<dyn FnMut() -> Result<HostConfig>>,
}

impl ConfigReloader {
    pub fn new(
        path: impl Into<PathBuf>,
        reload: impl FnMut() -> Result<HostConfig> + 'static,
    ) -> Self {
        Self {
            path: path.into(),
            reload: Box::new(reload),
        }
    }
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
pub fn run_layer_shell(config: HostConfig, spawner: ProviderSpawner) -> Result<()> {
    run_layer_shell_with_reload(config, spawner, None)
}

pub fn run_layer_shell_with_reload(
    config: HostConfig,
    spawner: ProviderSpawner,
    reloader: Option<ConfigReloader>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) =
        registry_queue_init::<LayerHost>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let (sender, snapshots) = channel::<ModuleUpdate>();
    let cache = SnapshotCache::from_specs(&config.panels);
    let next_clock_tick = config_has_clock(&config).then(|| Instant::now() + CLOCK_TICK);

    let mut host = LayerHost {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        conn: conn.clone(),
        config,
        cache,
        spawner,
        sender,
        provider_handle: None,
        current_epoch: 0,
        reloader,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
        gpu: None,
        surfaces: Vec::new(),
        reload_at: None,
        next_clock_tick,
        exit: false,
    };

    let mut event_loop: EventLoop<LayerHost> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;
    if let Some(reloader) = host.reloader.as_ref() {
        watch_config(&mut event_loop, &reloader.path)?;
    }
    event_loop
        .handle()
        .insert_source(snapshots, |event, _, host: &mut LayerHost| {
            if let ChannelEvent::Msg(update) = event {
                host.on_module_update(update);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert snapshot channel: {e}"))?;

    // Start the first provider generation now that the channel is live.
    host.spawn_providers();

    while !host.exit {
        let now = Instant::now();
        let mut timeout = host
            .next_clock_tick
            .map(|deadline| deadline.saturating_duration_since(now));
        if let Some(deadline) = host.reload_at {
            timeout = Some(min_timeout(
                timeout,
                deadline.saturating_duration_since(now),
            ));
        }
        for surface in &host.surfaces {
            if let Some(deadline) = surface.anim_deadline {
                timeout = Some(min_timeout(
                    timeout,
                    deadline.saturating_duration_since(now),
                ));
            }
        }

        event_loop
            .dispatch(timeout, &mut host)
            .context("event loop dispatch")?;

        let now = Instant::now();
        if host.next_clock_tick.is_some_and(|tick| tick <= now) {
            host.next_clock_tick = Some(now + CLOCK_TICK);
        }
        if host.reload_at.is_some_and(|deadline| deadline <= now) {
            host.reload_at = None;
            host.reload_config(&qh);
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
            host.refresh_and_draw(i, &qh);
        }
    }

    Ok(())
}

/// Smallest of an optional running timeout and a candidate deadline.
fn min_timeout(current: Option<Duration>, candidate: Duration) -> Duration {
    current.map_or(candidate, |current| current.min(candidate))
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
    // True once we have committed a frame and requested a `wl_surface.frame`
    // callback that has not yet fired. While set, we hold off drawing and
    // committing again — timer/provider triggers only accumulate `dirty`, and
    // the next actual draw waits for the callback. The compositor withholds
    // callbacks for powered-off outputs, so this stalls rendering for free
    // while screens are off instead of spinning at buffer-release rate.
    awaiting_frame: bool,
    // Last snapshots painted to this surface, for redraw suppression.
    last_snapshots: Vec<PanelSnapshot>,
}

struct LayerHost {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    conn: Connection,
    config: HostConfig,
    cache: SnapshotCache,
    spawner: ProviderSpawner,
    sender: SnapshotSender,
    provider_handle: Option<Box<dyn ProviderHandle>>,
    current_epoch: u64,
    reloader: Option<ConfigReloader>,
    instance: wgpu::Instance,
    gpu: Option<GpuShared>,
    surfaces: Vec<PanelSurface>,
    reload_at: Option<Instant>,
    next_clock_tick: Option<Instant>,
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
            awaiting_frame: false,
            last_snapshots: Vec::new(),
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

    /// Assemble this surface's snapshots, and redraw only when the result
    /// differs from what is currently painted (or a redraw is forced).
    fn refresh_and_draw(&mut self, i: usize, qh: &QueueHandle<Self>) {
        // Hold off while a requested frame callback is still outstanding: the
        // compositor paces us (and withholds callbacks entirely for
        // powered-off outputs). Timer/provider triggers keep marking `dirty`;
        // the accumulated work is flushed when the callback fires.
        if self.surfaces[i].awaiting_frame {
            return;
        }
        let snapshots: Vec<PanelSnapshot> = self.surfaces[i]
            .panels
            .iter()
            .map(|panel| self.cache.snapshot_for(&panel.id))
            .collect();
        let changed = !snapshots_display_eq(&snapshots, &self.surfaces[i].last_snapshots);
        if self.surfaces[i].dirty || changed {
            self.surfaces[i].last_snapshots = snapshots.clone();
            self.draw(i, snapshots, qh);
        }
    }

    fn draw(&mut self, i: usize, snapshots: Vec<PanelSnapshot>, qh: &QueueHandle<Self>) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let surface = &mut self.surfaces[i];
        surface.dirty = false;
        let Some(sc) = surface.swapchain.as_mut() else {
            return;
        };

        // Cloned so the `&mut surface.app` borrow below can coexist; the proxy
        // is a cheap handle to the same server-side surface wgpu commits.
        let wl_surface = surface.layer.wl_surface().clone();
        surface
            .app
            .set_views(panel_views_from_snapshots(&surface.panels, &snapshots));
        let outcome = render_frame(
            gpu,
            &wl_surface,
            qh,
            &surface.wgpu_surface,
            sc,
            &mut surface.app,
            (surface.width, surface.height),
            surface.scale,
            &format!("{}:{}", surface.output_name, panel_label(&surface.panels)),
        );
        surface.dirty = outcome.retry;
        surface.anim_deadline = outcome.anim_deadline;
        // Only wait on a callback if we actually committed a buffer; the
        // retry/error paths commit nothing, so no callback would ever arrive.
        surface.awaiting_frame = outcome.committed;
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
        let snapshots: Vec<PanelSnapshot> = panels
            .iter()
            .map(|panel| self.cache.snapshot_for(&panel.id))
            .collect();
        panel_views_from_snapshots(panels, &snapshots)
    }

    /// Drop the current provider generation (stopping its workers) and start a
    /// fresh one for the current config under the current epoch.
    fn spawn_providers(&mut self) {
        self.provider_handle = None;
        self.provider_handle = Some((self.spawner)(
            &self.config.panels,
            self.sender.clone(),
            self.current_epoch,
        ));
    }

    fn on_module_update(&mut self, update: ModuleUpdate) {
        // Drop results from a provider generation that a reload has retired;
        // a worker still mid-fetch when the config changed lands here.
        if update.epoch != self.current_epoch {
            return;
        }
        // The draw phase re-assembles and diffs every loop iteration, so the
        // updated cache repaints on this same wake if it changed anything.
        self.cache.apply(update);
    }

    fn reload_config(&mut self, qh: &QueueHandle<Self>) {
        let Some(reloader) = self.reloader.as_mut() else {
            return;
        };
        let config = match (reloader.reload)() {
            Ok(config) => config,
            Err(err) => {
                tracing::error!("config reload failed; keeping current config\n{err:#}");
                return;
            }
        };

        tracing::info!("config reloaded");
        self.current_epoch += 1;
        self.cache = SnapshotCache::from_specs(&config.panels);
        self.next_clock_tick = config_has_clock(&config).then(|| Instant::now() + CLOCK_TICK);
        self.config = config;
        self.spawn_providers();
        self.surfaces.clear();
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            let Some(name) = self.output_state.info(&output).and_then(|info| info.name) else {
                continue;
            };
            self.create_wanted_panels_for_output(qh, output, name);
        }
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
        // A configure asks us to commit a fresh buffer; don't let a stale gate
        // stall the response.
        self.surfaces[i].awaiting_frame = false;
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
            self.surfaces[i].awaiting_frame = false;
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
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // The compositor is ready for another frame on this surface. Release
        // the gate; any accumulated `dirty`/`anim_deadline` is drawn on the
        // next loop iteration.
        if let Some(i) = self.surface_index_for(surface) {
            self.surfaces[i].awaiting_frame = false;
        }
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

fn watch_config(event_loop: &mut EventLoop<LayerHost>, path: &Path) -> Result<()> {
    use rustix::fs::inotify;

    let (Some(dir), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Ok(());
    };
    if !dir.is_dir() {
        tracing::info!("{} absent; live config reload inactive", dir.display());
        return Ok(());
    }
    let file_name = file_name.to_owned();

    let fd = inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
        .context("inotify init")?;
    inotify::add_watch(
        &fd,
        dir,
        inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::CREATE
            | inotify::WatchFlags::DELETE,
    )
    .context("inotify add_watch")?;
    tracing::debug!("watching {} for config changes", dir.display());

    event_loop
        .handle()
        .insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, fd, host: &mut LayerHost| {
                let mut buf = [std::mem::MaybeUninit::uninit(); 1024];
                let mut reader = inotify::Reader::new(fd, &mut buf);
                while let Ok(event) = reader.next() {
                    let matches = event
                        .file_name()
                        .map(|name| name.to_bytes() == file_name.as_encoded_bytes())
                        .unwrap_or(false);
                    if matches {
                        host.reload_at = Some(Instant::now() + CONFIG_RELOAD_DEBOUNCE);
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert inotify source: {e}"))?;
    Ok(())
}

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

fn panel_views_from_snapshots(panels: &[PanelSpec], snapshots: &[PanelSnapshot]) -> Vec<PanelView> {
    panels
        .iter()
        .zip(snapshots)
        .map(|(panel, snapshot)| {
            PanelView::new(
                panel.appearance.clone(),
                panel.geometry.anchor,
                panel.layout,
                panel.geometry.width,
                snapshot.clone(),
            )
        })
        .collect()
}

/// Whether two sets of panel snapshots paint identically, ignoring freshness
/// timestamps (see [`ModuleSnapshot::display_eq`]).
fn snapshots_display_eq(a: &[PanelSnapshot], b: &[PanelSnapshot]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(left, right)| {
            left.panel_id == right.panel_id
                && left.modules.len() == right.modules.len()
                && left
                    .modules
                    .iter()
                    .zip(&right.modules)
                    .all(|(left, right)| left.display_eq(right))
        })
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
    // Whether a buffer was actually attached and committed this call. When
    // false (surface lost/unavailable), no frame callback was armed.
    committed: bool,
}

fn render_frame<A: App>(
    gpu: &GpuShared,
    wl_surface: &wl_surface::WlSurface,
    qh: &QueueHandle<LayerHost>,
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
                committed: false,
            };
        }
        other => {
            tracing::error!(surface = %label, "surface unavailable: {other:?}");
            return FrameOutcome {
                retry: false,
                anim_deadline: None,
                committed: false,
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
    // Request a frame callback *before* presenting. The request is
    // double-buffered surface state latched by the commit that wgpu issues
    // inside `present()`, so it rides out on this same frame. SCTK routes the
    // callback back to `CompositorHandler::frame` via the surface user-data.
    wl_surface.frame(qh, wl_surface.clone());
    frame.present();

    let mut anim_deadline = prepare.next_redraw_in.map(|delay| Instant::now() + delay);
    if prepare.needs_redraw && anim_deadline.is_none() {
        anim_deadline = Some(Instant::now());
    }
    FrameOutcome {
        retry: false,
        anim_deadline,
        committed: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use prism_widgets_core::{
        ClockSpec, CommandSpec, ModuleStatus, ModuleValue, PanelAppearance, PanelGeometry,
        ThemeName,
    };

    fn panel(id: &str, modules: Vec<ModuleSpec>) -> PanelSpec {
        PanelSpec {
            id: PanelId::new(id),
            output: None,
            layout: PanelLayout::Bar,
            geometry: PanelGeometry {
                width: None,
                height: 1,
                margin: 0,
                exclusive_zone: -1,
                anchor: PanelAnchor::TopRight,
                layer: PanelLayer::Top,
            },
            appearance: PanelAppearance {
                opacity: 1.0,
                radius: 0.0,
                border: false,
                show_header: false,
                theme: ThemeName::Dark,
            },
            modules,
        }
    }

    fn clock(id: &str) -> ModuleSpec {
        ModuleSpec::Clock(ClockSpec {
            id: id.into(),
            format: "%H:%M".into(),
        })
    }

    fn command(id: &str) -> ModuleSpec {
        ModuleSpec::Command(CommandSpec {
            id: id.into(),
            exec: "true".into(),
            interval: Duration::from_secs(60),
        })
    }

    fn cmd_state(label: &str) -> ModuleSnapshot {
        ModuleSnapshot {
            id: "cmd".into(),
            title: "cmd".into(),
            value: ModuleValue::State {
                label: label.into(),
                detail: None,
            },
            status: ModuleStatus::Ok,
            updated_at: Some(SystemTime::now()),
            stale_after: None,
        }
    }

    fn update(snapshot: ModuleSnapshot) -> ModuleUpdate {
        ModuleUpdate {
            epoch: 0,
            panel: PanelId::new("p"),
            module: "cmd".into(),
            snapshot,
        }
    }

    #[test]
    fn clock_renders_live_while_unfetched_modules_show_a_placeholder() {
        let spec = panel("p", vec![clock("clk"), command("cmd")]);
        let cache = SnapshotCache::from_specs(std::slice::from_ref(&spec));
        let snapshot = cache.snapshot_for(&PanelId::new("p"));

        assert_eq!(snapshot.modules.len(), 2);
        assert_eq!(snapshot.modules[0].id, "clk");
        assert!(matches!(snapshot.modules[0].value, ModuleValue::Text(_)));
        assert_eq!(snapshot.modules[1].status, ModuleStatus::Unknown);
    }

    #[test]
    fn apply_serves_latest_value_and_reports_display_changes() {
        let spec = panel("p", vec![command("cmd")]);
        let mut cache = SnapshotCache::from_specs(std::slice::from_ref(&spec));

        assert!(
            cache.apply(update(cmd_state("ok"))),
            "first value is a change"
        );
        assert_eq!(
            cache.snapshot_for(&PanelId::new("p")).modules[0].value,
            ModuleValue::State {
                label: "ok".into(),
                detail: None,
            }
        );
        // Same paint, newer timestamp → not a change.
        assert!(!cache.apply(update(cmd_state("ok"))));
        // Different label → a change.
        assert!(cache.apply(update(cmd_state("fail"))));
    }

    #[test]
    fn snapshots_display_eq_ignores_timestamps() {
        let snapshots = |label: &str| {
            vec![PanelSnapshot {
                panel_id: PanelId::new("p"),
                modules: vec![cmd_state(label)],
            }]
        };
        assert!(snapshots_display_eq(&snapshots("ok"), &snapshots("ok")));
        assert!(!snapshots_display_eq(
            &snapshots("ok"),
            &snapshots("changed")
        ));
    }
}
