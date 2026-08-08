//! Clipboard history via the compositor's ext-data-control global.
//!
//! Unlike the polled providers, this worker is event-driven: it opens its own
//! Wayland connection (separate from the host's, keeping the provider seam
//! one-way), registers as a clipboard manager, and pushes a snapshot whenever
//! the selection changes. Only text selections are recorded, and selections a
//! password manager marks with the `x-kde-passwordManagerHint` mime type are
//! never read at all.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use prism_widgets_core::{
    ClipboardSpec, ListEntry, ListGroup, ModuleSnapshot, ModuleStatus, ModuleUpdate, ModuleValue,
    PanelId,
};
use prism_widgets_host::SnapshotSender;
use rustix::event::{PollFd, PollFlags, Timespec};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};

/// How often the watch loop wakes to check the shutdown flag while idle.
const SHUTDOWN_POLL: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 500_000_000,
};
/// Give a selection owner this long to write its payload before dropping it.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-entry storage cap; longer payloads are truncated at this size.
const MAX_ENTRY_BYTES: usize = 128 * 1024;
/// Single-line entries above this size get a size annotation.
const LARGE_ENTRY_BYTES: usize = 2048;

/// Password managers offer this alongside sensitive selections; its presence
/// means the selection must not be recorded (we don't read its value — any
/// owner offering the hint is treated as sensitive).
const SENSITIVE_MIME: &str = "x-kde-passwordManagerHint";

/// Text mimes we can record, in preference order (compared case-insensitively).
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "utf8_string",
    "text/plain",
    "string",
    "text",
];

/// Worker entry point: watch until shutdown, pushing a snapshot on every
/// history change. Setup or protocol failures surface as a warning snapshot
/// so the panel shows why there's no history rather than a stuck spinner.
pub(crate) fn watch_clipboard(
    spec: &ClipboardSpec,
    panel: PanelId,
    module: String,
    epoch: u64,
    sender: &SnapshotSender,
    shutdown: &AtomicBool,
) {
    if let Err(err) = run_watch(spec, &panel, &module, epoch, sender, shutdown) {
        tracing::warn!("clipboard watcher stopped: {err:#}");
        let snapshot = ModuleSnapshot {
            id: spec.id.clone(),
            title: "clipboard".into(),
            value: ModuleValue::State {
                label: "unavailable".into(),
                detail: Some(format!("{err:#}")),
            },
            status: ModuleStatus::Warning,
            updated_at: Some(SystemTime::now()),
            stale_after: None,
        };
        let _ = sender.send(ModuleUpdate {
            epoch,
            panel,
            module,
            snapshot,
        });
    }
}

fn run_watch(
    spec: &ClipboardSpec,
    panel: &PanelId,
    module: &str,
    epoch: u64,
    sender: &SnapshotSender,
    shutdown: &AtomicBool,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let (globals, mut queue) =
        registry_queue_init::<WatchState>(&conn).context("initializing registry")?;
    let qh = queue.handle();

    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("binding ext_data_control_manager_v1 (compositor lacks ext-data-control-v1)")?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("binding wl_seat")?;
    let _device = manager.get_data_device(&seat, &qh, ());

    let mut state = WatchState::default();
    let mut history = ClipboardHistory::new(spec.max_entries);

    // The device receives the current selection immediately on creation, so
    // after one roundtrip the history reflects the live clipboard — push that
    // even when it's empty, replacing the loading placeholder.
    queue.roundtrip(&mut state).context("initial roundtrip")?;
    absorb_selection(&conn, &mut state, &mut history);
    if !send_history(spec, panel, module, epoch, sender, &history) {
        return Ok(());
    }

    loop {
        queue
            .dispatch_pending(&mut state)
            .context("dispatching events")?;
        if state.selection_dirty
            && absorb_selection(&conn, &mut state, &mut history)
            && !send_history(spec, panel, module, epoch, sender, &history)
        {
            return Ok(());
        }
        if state.finished {
            anyhow::bail!("compositor revoked the data-control device");
        }
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }

        match queue.flush() {
            Ok(()) => {}
            // Send buffer full; the pending requests go out on a later tick.
            Err(WaylandError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err).context("flushing connection"),
        }
        let Some(guard) = queue.prepare_read() else {
            continue; // events already queued; dispatch them first
        };
        let fd = guard.connection_fd();
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&SHUTDOWN_POLL)) {
            Ok(0) => drop(guard), // idle tick: loop around for the shutdown check
            Ok(_) => match guard.read() {
                Ok(_) => {}
                // Spurious wakeup: with the rs backend an empty socket reads
                // as WouldBlock rather than 0 events; not fatal.
                Err(WaylandError::Io(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err).context("reading connection"),
            },
            Err(rustix::io::Errno::INTR) => drop(guard),
            Err(err) => return Err(err).context("polling connection"),
        }
    }
}

/// Consume a pending selection change: read its text if it has a usable,
/// non-sensitive mime, fold it into the history, and destroy the offer.
/// Returns whether the history changed.
fn absorb_selection(
    conn: &Connection,
    state: &mut WatchState,
    history: &mut ClipboardHistory,
) -> bool {
    state.selection_dirty = false;
    let Some(offer) = state.clipboard_offer.take() else {
        return false; // selection cleared; history keeps its entries
    };
    let mimes = state.offers.remove(&offer.id()).unwrap_or_default();
    let text = read_offer_text(conn, &offer, &mimes);
    offer.destroy();
    match text {
        Some(text) => history.push(text),
        None => false,
    }
}

/// Read an offer's payload as text, or `None` when the offer is sensitive,
/// non-text, empty, or its owner won't deliver within the timeout.
fn read_offer_text(
    conn: &Connection,
    offer: &ExtDataControlOfferV1,
    mimes: &[String],
) -> Option<String> {
    if mimes.iter().any(|m| m.eq_ignore_ascii_case(SENSITIVE_MIME)) {
        tracing::debug!("skipping sensitive selection");
        return None;
    }
    let mime = choose_text_mime(mimes)?;

    let (read_fd, write_fd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|err| tracing::warn!("clipboard pipe: {err}"))
        .ok()?;
    offer.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd);
    if let Err(err) = conn.flush() {
        tracing::warn!("clipboard receive flush: {err}");
        return None;
    }

    let bytes = read_bounded(read_fd)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

fn choose_text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES.iter().find_map(|want| {
        mimes
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(want))
            .map(String::as_str)
    })
}

/// Drain a pipe until EOF, the size cap, or the receive deadline. Truncation
/// closes the pipe early; the stalled or oversized remainder is the owner's
/// problem (it sees EPIPE), not ours.
fn read_bounded(fd: rustix::fd::OwnedFd) -> Option<Vec<u8>> {
    let mut file = std::fs::File::from(fd);
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        let mut fds = [PollFd::new(&file, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return None, // owner never delivered; drop the entry
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                tracing::warn!("clipboard pipe poll: {err}");
                return None;
            }
        }
        match file.read(&mut chunk) {
            Ok(0) => return Some(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= MAX_ENTRY_BYTES {
                    buf.truncate(MAX_ENTRY_BYTES);
                    return Some(buf);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::warn!("clipboard pipe read: {err}");
                return None;
            }
        }
    }
}

fn send_history(
    spec: &ClipboardSpec,
    panel: &PanelId,
    module: &str,
    epoch: u64,
    sender: &SnapshotSender,
    history: &ClipboardHistory,
) -> bool {
    let snapshot = ModuleSnapshot {
        id: spec.id.clone(),
        title: "clipboard".into(),
        value: ModuleValue::List(history.list()),
        status: ModuleStatus::Ok,
        updated_at: Some(SystemTime::now()),
        stale_after: None,
    };
    sender
        .send(ModuleUpdate {
            epoch,
            panel: panel.clone(),
            module: module.to_string(),
            snapshot,
        })
        .is_ok()
}

/// Rolling most-recent-first history with de-duplication: re-copying an old
/// entry moves it back to the front instead of duplicating it.
pub(crate) struct ClipboardHistory {
    cap: usize,
    entries: VecDeque<String>,
}

impl ClipboardHistory {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: VecDeque::new(),
        }
    }

    /// Fold a new selection in, returning whether the visible history changed.
    pub(crate) fn push(&mut self, text: String) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        if self.entries.front() == Some(&text) {
            return false;
        }
        self.entries.retain(|entry| entry != &text);
        self.entries.push_front(text);
        self.entries.truncate(self.cap);
        tracing::debug!(entries = self.entries.len(), "clipboard history updated");
        true
    }

    pub(crate) fn list(&self) -> ListGroup {
        ListGroup {
            entries: self.entries.iter().map(|text| list_entry(text)).collect(),
        }
    }
}

fn list_entry(text: &str) -> ListEntry {
    ListEntry {
        label: entry_label(text),
        meta: entry_meta(text),
    }
}

/// First non-empty line, trimmed — a multi-line payload is represented by its
/// head, with the line count carried in the meta annotation.
fn entry_label(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn entry_meta(text: &str) -> Option<String> {
    let lines = text.trim_end().lines().count();
    if lines > 1 {
        return Some(format!("{lines} lines"));
    }
    (text.len() > LARGE_ENTRY_BYTES).then(|| format!("{} KB", text.len() / 1024))
}

#[derive(Default)]
struct WatchState {
    /// Mimes announced per live offer, keyed by offer object id.
    offers: HashMap<ObjectId, Vec<String>>,
    /// The offer for the most recent clipboard selection, pending a read.
    clipboard_offer: Option<ExtDataControlOfferV1>,
    /// Set when a selection event arrived; the watch loop consumes it outside
    /// event dispatch so the (blocking, bounded) pipe read never runs inside
    /// a handler.
    selection_dirty: bool,
    /// The compositor invalidated the device (seat gone).
    finished: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WatchState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Global add/remove after init (e.g. hotplugged seats) is ignored;
        // the watcher binds once at startup.
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // ext_data_control_manager_v1 has no events.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Capabilities and name are irrelevant; the seat only parameterizes
        // get_data_device.
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => {
                // The protocol requires destroying the replaced offer.
                if let Some(prev) = state.clipboard_offer.take() {
                    state.offers.remove(&prev.id());
                    prev.destroy();
                }
                state.clipboard_offer = id;
                state.selection_dirty = true;
            }
            Event::PrimarySelection { id: Some(offer) } => {
                // Primary selection (mouse highlight) is deliberately not
                // recorded — it changes on every drag and is rarely an
                // intentional "copy". Destroy the offer immediately.
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(WatchState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(history: &ClipboardHistory) -> Vec<String> {
        history
            .list()
            .entries
            .iter()
            .map(|entry| entry.label.clone())
            .collect()
    }

    #[test]
    fn history_is_newest_first_deduped_and_capped() {
        let mut history = ClipboardHistory::new(3);
        assert!(history.push("one".into()));
        assert!(history.push("two".into()));
        assert!(history.push("three".into()));
        assert_eq!(labels(&history), ["three", "two", "one"]);

        // Re-copying an old entry promotes it without duplicating.
        assert!(history.push("one".into()));
        assert_eq!(labels(&history), ["one", "three", "two"]);

        // Copying the current front again changes nothing.
        assert!(!history.push("one".into()));

        // The cap evicts the oldest.
        assert!(history.push("four".into()));
        assert_eq!(labels(&history), ["four", "one", "three"]);
    }

    #[test]
    fn whitespace_only_selections_are_ignored() {
        let mut history = ClipboardHistory::new(4);
        assert!(!history.push("   \n\t".into()));
        assert!(labels(&history).is_empty());
    }

    #[test]
    fn multiline_entries_show_head_line_and_count() {
        let entry = list_entry("\n  fn main() {\n    body\n}\n");
        assert_eq!(entry.label, "fn main() {");
        assert_eq!(entry.meta.as_deref(), Some("4 lines"));

        let short = list_entry("hello");
        assert_eq!(short.label, "hello");
        assert_eq!(short.meta, None);
    }

    #[test]
    fn large_single_line_entries_carry_a_size_annotation() {
        let big = "x".repeat(10 * 1024);
        let entry = list_entry(&big);
        assert_eq!(entry.meta.as_deref(), Some("10 KB"));
    }

    #[test]
    fn text_mime_preference_is_case_insensitive_and_ordered() {
        let mimes = vec![
            "image/png".to_string(),
            "TEXT/PLAIN".to_string(),
            "text/plain;charset=UTF-8".to_string(),
        ];
        assert_eq!(choose_text_mime(&mimes), Some("text/plain;charset=UTF-8"));
        assert_eq!(choose_text_mime(&["image/png".to_string()]), None);
    }
}
