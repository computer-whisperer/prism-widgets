//! Clipboard history via the compositor's ext-data-control global.
//!
//! Unlike the polled providers, this worker is event-driven: it opens its own
//! Wayland connection (separate from the host's, keeping the provider seam
//! one-way), registers as a clipboard manager, and pushes a snapshot whenever
//! the selection changes. Only text selections are recorded, and selections a
//! password manager marks with the `x-kde-passwordManagerHint` mime type are
//! never read at all.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use prism_widgets_core::{
    ClipboardSpec, ListEntry, ListGroup, ModuleActionKind, ModuleSnapshot, ModuleStatus,
    ModuleUpdate, ModuleValue, PanelId,
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
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
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

/// Mimes offered when this provider re-owns the selection for a restored
/// history entry.
const OFFER_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Worker entry point: watch until shutdown, pushing a snapshot on every
/// history change. Setup or protocol failures surface as a warning snapshot
/// so the panel shows why there's no history rather than a stuck spinner.
/// The receiving half of a module's action route: the channel the host's
/// `ProviderHandle::dispatch` feeds, plus the pipe end whose readability
/// interrupts the watcher's poll.
pub(crate) struct ActionInbox {
    pub(crate) actions: mpsc::Receiver<ModuleActionKind>,
    pub(crate) wake: rustix::fd::OwnedFd,
}

pub(crate) fn watch_clipboard(
    spec: &ClipboardSpec,
    panel: PanelId,
    module: String,
    epoch: u64,
    sender: &SnapshotSender,
    shutdown: &AtomicBool,
    inbox: ActionInbox,
) {
    if let Err(err) = run_watch(spec, &panel, &module, epoch, sender, shutdown, inbox) {
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
    inbox: ActionInbox,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let (globals, mut queue) =
        registry_queue_init::<WatchState>(&conn).context("initializing registry")?;
    let qh = queue.handle();

    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("binding ext_data_control_manager_v1 (compositor lacks ext-data-control-v1)")?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("binding wl_seat")?;
    let device = manager.get_data_device(&seat, &qh, ());

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

    let actions = inbox.actions;
    let wake = std::fs::File::from(inbox.wake);
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
        // Drain UI actions before flushing so their requests (set_selection)
        // ride this iteration's flush.
        while let Ok(kind) = actions.try_recv() {
            handle_action(&mut state, &qh, &manager, &device, &history, kind);
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
        let conn_fd = guard.connection_fd();
        let mut fds = [
            PollFd::from_borrowed_fd(conn_fd, PollFlags::IN),
            PollFd::new(&wake, PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, Some(&SHUTDOWN_POLL)) {
            Ok(0) => drop(guard), // idle tick: loop around for the shutdown check
            Ok(_) => {
                let conn_ready = !fds[0].revents().is_empty();
                let wake_ready = !fds[1].revents().is_empty();
                if conn_ready {
                    match guard.read() {
                        Ok(_) => {}
                        // Spurious wakeup: with the rs backend an empty socket
                        // reads as WouldBlock rather than 0 events; not fatal.
                        Err(WaylandError::Io(err))
                            if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(err) => return Err(err).context("reading connection"),
                    }
                } else {
                    drop(guard);
                }
                if wake_ready {
                    drain_wake(&wake);
                    // The queued actions themselves are drained at loop top.
                }
            }
            Err(rustix::io::Errno::INTR) => drop(guard),
            Err(err) => return Err(err).context("polling connection"),
        }
    }
}

/// Perform a UI-originated action. Restores re-own the selection with the
/// entry's text via a fresh data-control source; the compositor then echoes
/// a `selection` event back to our device, which is where the history
/// promotion and snapshot push happen (single source of truth).
fn handle_action(
    state: &mut WatchState,
    qh: &QueueHandle<WatchState>,
    manager: &ExtDataControlManagerV1,
    device: &ExtDataControlDeviceV1,
    history: &ClipboardHistory,
    kind: ModuleActionKind,
) {
    match kind {
        ModuleActionKind::ActivateEntry { entry } => {
            let Ok(key) = entry.parse::<u64>() else {
                tracing::warn!("malformed clipboard entry key {entry:?}");
                return;
            };
            let Some(text) = history.text_of(key) else {
                return; // entry evicted between render and click
            };
            let source = manager.create_data_source(qh, ());
            for mime in OFFER_MIMES {
                source.offer((*mime).to_string());
            }
            device.set_selection(Some(&source));
            // A previously owned source is now stale; the compositor sends
            // it `cancelled`, and that handler destroys it.
            state.own = Some(OwnSelection {
                source,
                text,
                entry: key,
            });
        }
    }
}

/// Empty the wake pipe after its poll fired; the actual actions travel on
/// the mpsc channel, the pipe only interrupts the poll.
fn drain_wake(wake: &std::fs::File) {
    let mut buf = [0u8; 64];
    loop {
        match (&*wake).read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
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
    if let Some(own) = &state.own {
        // The selection event echoes our own set_selection (a restore).
        // Receiving from ourselves would deadlock — the pipe only fills
        // when we dispatch our own Send event — and we already know the
        // text; promote the restored entry instead. A foreign copy racing
        // this is resolved by ordering: Smithay cancels our source before
        // broadcasting the new selection (the protocol itself mandates no
        // order), so `Cancelled` clears `own` earlier in the same dispatch
        // batch. A compositor that delayed `cancelled` to a later batch
        // would mis-record one foreign copy as a promote.
        offer.destroy();
        return history.promote(own.entry);
    }
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
/// entry moves it back to the front instead of duplicating it. Each entry
/// carries a stable numeric key — kept across promotions — that the UI
/// echoes back in restore actions.
pub(crate) struct ClipboardHistory {
    cap: usize,
    next_key: u64,
    entries: VecDeque<HistoryEntry>,
}

struct HistoryEntry {
    key: u64,
    text: String,
}

impl ClipboardHistory {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            next_key: 0,
            entries: VecDeque::new(),
        }
    }

    /// Fold a new selection in, returning whether the visible history changed.
    pub(crate) fn push(&mut self, text: String) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        if self.entries.front().is_some_and(|front| front.text == text) {
            return false;
        }
        // A re-copy of an existing entry keeps its key (promotion, not a
        // new entry); genuinely new text gets a fresh one.
        let key = match self.entries.iter().position(|entry| entry.text == text) {
            Some(i) => self.entries.remove(i).expect("indexed entry").key,
            None => {
                let key = self.next_key;
                self.next_key += 1;
                key
            }
        };
        self.entries.push_front(HistoryEntry { key, text });
        self.entries.truncate(self.cap);
        tracing::debug!(entries = self.entries.len(), "clipboard history updated");
        true
    }

    /// Move the entry with the given key to the front (a restore landed).
    /// Returns whether the visible history changed.
    pub(crate) fn promote(&mut self, key: u64) -> bool {
        match self.entries.iter().position(|entry| entry.key == key) {
            Some(0) => false,
            Some(i) => {
                let entry = self.entries.remove(i).expect("indexed entry");
                self.entries.push_front(entry);
                true
            }
            None => false, // evicted since the click
        }
    }

    pub(crate) fn text_of(&self, key: u64) -> Option<String> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.text.clone())
    }

    pub(crate) fn list(&self) -> ListGroup {
        ListGroup {
            entries: self
                .entries
                .iter()
                .map(|entry| list_entry(entry.key, &entry.text))
                .collect(),
        }
    }
}

fn list_entry(key: u64, text: &str) -> ListEntry {
    ListEntry {
        key: Some(key.to_string()),
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
    /// Live while this provider owns the selection (a restored entry being
    /// served to other clients). Cleared by the source's `cancelled` event
    /// when someone else copies.
    own: Option<OwnSelection>,
    /// The compositor invalidated the device (seat gone).
    finished: bool,
}

struct OwnSelection {
    source: ExtDataControlSourceV1,
    text: String,
    /// History key of the entry being served, promoted when the
    /// compositor echoes the selection back.
    entry: u64,
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

impl Dispatch<ExtDataControlSourceV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_source_v1::Event;
        match event {
            Event::Send { mime_type: _, fd } => {
                // Serve only for the live source; a Send racing its own
                // cancellation gets nothing (receiver sees EOF).
                if let Some(own) = state
                    .own
                    .as_ref()
                    .filter(|own| own.source == *source)
                {
                    write_bounded(fd, own.text.as_bytes());
                }
            }
            Event::Cancelled => {
                if state
                    .own
                    .as_ref()
                    .is_some_and(|own| own.source == *source)
                {
                    state.own = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

/// Write a restored payload to a receiver's pipe, bounded by the same
/// deadline as reads: a receiver that never drains its pipe costs us at
/// most `RECEIVE_TIMEOUT`, then sees a short payload.
fn write_bounded(fd: rustix::fd::OwnedFd, bytes: &[u8]) {
    // The receiver created this fd and blocking pipes are the norm; a
    // blocking write() transfers ALL bytes before returning, which would
    // ignore the deadline and wedge the watcher inside its own Send
    // handler on a stalled receiver. Force O_NONBLOCK so poll(OUT) +
    // WouldBlock does the pacing as written below.
    if let Err(err) = rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK) {
        tracing::warn!("clipboard serve fcntl: {err}");
        return;
    }
    let mut file = std::fs::File::from(fd);
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut offset = 0;
    while offset < bytes.len() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        let mut fds = [PollFd::new(&file, PollFlags::OUT)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return, // receiver stalled
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
        match file.write(&bytes[offset..]) {
            Ok(0) => return,
            Ok(n) => offset += n,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return, // receiver closed (EPIPE) or similar
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
        let entry = list_entry(0, "\n  fn main() {\n    body\n}\n");
        assert_eq!(entry.label, "fn main() {");
        assert_eq!(entry.meta.as_deref(), Some("4 lines"));

        let short = list_entry(1, "hello");
        assert_eq!(short.label, "hello");
        assert_eq!(short.meta, None);
        assert_eq!(short.key.as_deref(), Some("1"));
    }

    #[test]
    fn large_single_line_entries_carry_a_size_annotation() {
        let big = "x".repeat(10 * 1024);
        let entry = list_entry(0, &big);
        assert_eq!(entry.meta.as_deref(), Some("10 KB"));
    }

    #[test]
    fn entry_keys_are_stable_across_promotion_and_lookup() {
        let mut history = ClipboardHistory::new(3);
        history.push("alpha".into());
        history.push("beta".into());
        let alpha_key: u64 = history.list().entries[1]
            .key
            .as_deref()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(history.text_of(alpha_key).as_deref(), Some("alpha"));

        // Re-copying keeps the key; promoting moves without renaming.
        assert!(history.push("alpha".into()));
        assert_eq!(
            history.list().entries[0].key.as_deref(),
            Some(alpha_key.to_string().as_str())
        );
        assert!(!history.promote(alpha_key), "already at front");
        history.push("gamma".into());
        assert!(history.promote(alpha_key));
        assert_eq!(history.list().entries[0].label, "alpha");

        // Unknown / evicted keys are inert.
        assert!(!history.promote(9999));
        assert_eq!(history.text_of(9999), None);
    }

    /// Full action loop against the live compositor: a tool-side connection
    /// seeds the selection, the scheduler's watcher records it, a dispatched
    /// restore makes the watcher re-own the selection, and the tool side
    /// reads it back. Needs a Wayland session with ext-data-control; run
    /// explicitly with `cargo test -p prism-widgets-providers -- --ignored`.
    ///
    /// WARNING: running this REPLACES the session's clipboard selection with
    /// a test payload (and leaves it cleared when the test process exits).
    #[test]
    #[ignore = "needs a live Wayland session with ext-data-control"]
    fn restore_reowns_selection_live() {
        use prism_widgets_core::{
            ModuleAction, ModuleSpec, PanelAnchor, PanelAppearance, PanelGeometry, PanelLayer,
            PanelLayout, PanelSpec, ThemeName,
        };
        use prism_widgets_host::ProviderHandle as _;

        // Tool-side connection: seeds the clipboard, then reads it back.
        let conn = Connection::connect_to_env().expect("wayland session");
        let (globals, mut queue) = registry_queue_init::<WatchState>(&conn).expect("registry");
        let qh = queue.handle();
        let manager: ExtDataControlManagerV1 =
            globals.bind(&qh, 1..=1, ()).expect("ext-data-control");
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("wl_seat");
        let _device = manager.get_data_device(&seat, &qh, ());
        let mut state = WatchState::default();
        queue.roundtrip(&mut state).expect("initial roundtrip");

        // Seed a unique payload, served by WatchState's own source dispatch.
        let payload = format!("clipboard-live-test-{}", std::process::id());
        let source = manager.create_data_source(&qh, ());
        for mime in OFFER_MIMES {
            source.offer((*mime).to_string());
        }
        _device.set_selection(Some(&source));
        state.own = Some(OwnSelection {
            source,
            text: payload.clone(),
            entry: 0,
        });
        queue.flush().expect("flush seed");

        let (sender, snapshots) = prism_widgets_host::snapshot_channel();
        let panel = PanelId::new("live-test");
        let spec = PanelSpec {
            id: panel.clone(),
            output: None,
            layout: PanelLayout::Sidebar,
            geometry: PanelGeometry {
                width: Some(300),
                height: 100,
                margin: 0,
                exclusive_zone: -1,
                anchor: PanelAnchor::Right,
                layer: PanelLayer::Top,
            },
            appearance: PanelAppearance {
                opacity: 1.0,
                radius: 0.0,
                border: false,
                show_header: false,
                theme: ThemeName::Dark,
            },
            modules: vec![ModuleSpec::Clipboard(ClipboardSpec {
                id: "clipboard".into(),
                max_entries: 4,
            })],
        };
        let handle = crate::start_scheduler(&[spec], sender, 1);

        // Wait for the watcher to record our payload, serving its receive
        // (our Send event) via roundtrips meanwhile.
        let deadline = Instant::now() + Duration::from_secs(10);
        let entry_key = 'found: loop {
            assert!(
                Instant::now() < deadline,
                "watcher never recorded the seeded payload"
            );
            queue.roundtrip(&mut state).expect("roundtrip while waiting");
            while let Ok(update) = snapshots.try_recv() {
                if let ModuleValue::List(group) = &update.snapshot.value {
                    if let Some(entry) =
                        group.entries.iter().find(|entry| entry.label == payload)
                    {
                        break 'found entry.key.clone().expect("entry key");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // Restore through the host-facing seam.
        handle.dispatch(ModuleAction {
            panel,
            module: "clipboard".into(),
            kind: ModuleActionKind::ActivateEntry { entry: entry_key },
        });

        // The watcher re-owns the selection: our seed source gets cancelled
        // (clearing `state.own`), a fresh offer arrives, and reading it back
        // must yield the payload — served by the watcher thread.
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            assert!(
                Instant::now() < deadline,
                "watcher never re-owned the selection"
            );
            queue.roundtrip(&mut state).expect("roundtrip after restore");
            if state.own.is_none() {
                if let Some(offer) = state.clipboard_offer.take() {
                    let mimes = state.offers.remove(&offer.id()).unwrap_or_default();
                    let text = read_offer_text(&conn, &offer, &mimes);
                    offer.destroy();
                    if let Some(text) = text {
                        break text;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(text, payload);
        drop(handle);
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
