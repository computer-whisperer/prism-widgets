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
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use prism_widgets_core::{
    ClipboardSpec, ListEntry, ListGroup, ModuleActionKind, ModuleSnapshot, ModuleStatus,
    ModuleUpdate, ModuleValue, PanelId, Thumbnail,
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
/// Per-entry storage cap for text; longer payloads are truncated at this
/// size (a truncated text is still useful, unlike a truncated image).
const MAX_ENTRY_BYTES: usize = 128 * 1024;
/// Single-line entries above this size get a size annotation.
const LARGE_ENTRY_BYTES: usize = 2048;
/// Per-entry cap for image payloads; an image that exceeds it is dropped
/// entirely — truncated image bytes are garbage.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Total encoded-image budget across the history; oldest image entries are
/// evicted (entirely) to stay under it, so a run of screenshots can't pin
/// hundreds of megabytes.
const IMAGE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
// The eviction loop's newest-entry exemption is positional (index 0). It is
// only guaranteed to shield the just-pushed *image* because a single image
// can never exceed the budget by itself — which this pins down:
const _: () = assert!(MAX_IMAGE_BYTES <= IMAGE_BUDGET_BYTES);
/// Thumbnail bounding box, logical pixels (aspect preserved within it).
const THUMB_MAX_W: u32 = 220;
const THUMB_MAX_H: u32 = 64;

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

/// Image mimes we can record, in preference order — mirrors the codecs the
/// `image` crate is built with here.
const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/bmp"];

/// File copies from file managers; preferred over plain text when both are
/// offered so a restore can paste files, not path strings.
const URI_LIST_MIME: &str = "text/uri-list";

/// Mimes offered when this provider re-owns the selection for a restored
/// text entry.
const OFFER_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// What one history entry holds — enough fidelity to display a row and to
/// serve the payload back on restore.
///
/// Debug is manual: image byte buffers print as their length, not their
/// contents.
#[derive(Clone)]
pub(crate) enum Payload {
    Text(String),
    /// Verbatim `text/uri-list` body (file copies).
    Uris(String),
    Image {
        /// The mime the bytes were received as; restores offer exactly this
        /// (no transcoding).
        mime: String,
        bytes: Arc<Vec<u8>>,
        /// Decoded dimensions; `None` when the decoder failed (the entry is
        /// still shown and restorable — we just can't preview it).
        dims: Option<(u32, u32)>,
        thumb: Option<Thumbnail>,
    },
}

impl Payload {
    fn bytes(&self) -> &[u8] {
        match self {
            Payload::Text(text) | Payload::Uris(text) => text.as_bytes(),
            Payload::Image { bytes, .. } => bytes,
        }
    }

    /// Bytes to serve for a specific requested mime. Uri-list bodies come
    /// off the wire with RFC 2483 CRLF line endings and percent-encoding;
    /// serving that verbatim as text/plain pastes literal `\r` into
    /// editors, so the plain-text offers get a newline-joined rendering
    /// while `text/uri-list` receivers get the verbatim body.
    fn bytes_for(&self, mime: &str) -> std::borrow::Cow<'_, [u8]> {
        match self {
            Payload::Uris(body) if !mime.eq_ignore_ascii_case(URI_LIST_MIME) => {
                std::borrow::Cow::Owned(parse_uris(body).join("\n").into_bytes())
            }
            _ => std::borrow::Cow::Borrowed(self.bytes()),
        }
    }

    /// Mimes to offer when serving this payload back.
    fn offer_mimes(&self) -> Vec<String> {
        match self {
            Payload::Text(_) => OFFER_MIMES.iter().map(|m| (*m).to_string()).collect(),
            Payload::Uris(_) => vec![
                URI_LIST_MIME.to_string(),
                "text/plain;charset=utf-8".to_string(),
                "text/plain".to_string(),
            ],
            Payload::Image { mime, .. } => vec![mime.clone()],
        }
    }

    fn image_size(&self) -> usize {
        match self {
            Payload::Image { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }

    /// Nothing worth recording: whitespace-only text or an empty payload.
    fn is_blank(&self) -> bool {
        match self {
            Payload::Text(text) | Payload::Uris(text) => text.trim().is_empty(),
            Payload::Image { bytes, .. } => bytes.is_empty(),
        }
    }
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Payload::Text(text) => f.debug_tuple("Text").field(text).finish(),
            Payload::Uris(body) => f.debug_tuple("Uris").field(body).finish(),
            Payload::Image { mime, bytes, dims, .. } => f
                .debug_struct("Image")
                .field("mime", mime)
                .field("len", &bytes.len())
                .field("dims", dims)
                .finish(),
        }
    }
}

/// De-duplication equality: same copied content, ignoring derived fields
/// (thumbnails, dims).
impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Payload::Text(a), Payload::Text(b)) => a == b,
            (Payload::Uris(a), Payload::Uris(b)) => a == b,
            (
                Payload::Image {
                    mime: am, bytes: ab, ..
                },
                Payload::Image {
                    mime: bm, bytes: bb, ..
                },
            ) => am == bm && ab == bb,
            _ => false,
        }
    }
}

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
            let Some(payload) = history.payload_of(key) else {
                return; // entry evicted between render and click
            };
            let source = manager.create_data_source(qh, ());
            for mime in payload.offer_mimes() {
                source.offer(mime);
            }
            device.set_selection(Some(&source));
            // A previously owned source is now stale; the compositor sends
            // it `cancelled`, and that handler destroys it.
            state.own = Some(OwnSelection {
                source,
                payload,
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
    let payload = read_offer_payload(conn, &offer, &mimes);
    offer.destroy();
    match payload {
        Some(payload) => history.push(payload),
        None => false,
    }
}

/// What we decided to read out of an offer, and as which mime.
enum MimeChoice<'a> {
    Uris(&'a str),
    Text(&'a str),
    Image(&'a str),
}

/// Pick the best recordable mime: file lists beat plain text (a restore
/// can then paste files), text beats images (apps offering both usually
/// mean the text rendering), images beat nothing.
fn choose_mime(mimes: &[String]) -> Option<MimeChoice<'_>> {
    let find = |want: &str| {
        mimes
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(want))
            .map(String::as_str)
    };
    if let Some(mime) = find(URI_LIST_MIME) {
        return Some(MimeChoice::Uris(mime));
    }
    if let Some(mime) = TEXT_MIMES.iter().find_map(|want| find(want)) {
        return Some(MimeChoice::Text(mime));
    }
    IMAGE_MIMES
        .iter()
        .find_map(|want| find(want))
        .map(MimeChoice::Image)
}

/// Read an offer's payload, or `None` when the offer is sensitive, has no
/// recordable mime, is empty, or its owner won't deliver within the timeout.
fn read_offer_payload(
    conn: &Connection,
    offer: &ExtDataControlOfferV1,
    mimes: &[String],
) -> Option<Payload> {
    if mimes.iter().any(|m| m.eq_ignore_ascii_case(SENSITIVE_MIME)) {
        tracing::debug!("skipping sensitive selection");
        return None;
    }
    let choice = choose_mime(mimes)?;
    let (mime, cap) = match &choice {
        MimeChoice::Uris(mime) | MimeChoice::Text(mime) => (*mime, MAX_ENTRY_BYTES),
        MimeChoice::Image(mime) => (*mime, MAX_IMAGE_BYTES),
    };

    let (read_fd, write_fd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|err| tracing::warn!("clipboard pipe: {err}"))
        .ok()?;
    offer.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd);
    if let Err(err) = conn.flush() {
        tracing::warn!("clipboard receive flush: {err}");
        return None;
    }

    let (bytes, truncated) = read_bounded(read_fd, cap)?;
    match choice {
        MimeChoice::Text(_) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (!text.trim().is_empty()).then_some(Payload::Text(text))
        }
        MimeChoice::Uris(_) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (!parse_uris(&text).is_empty()).then_some(Payload::Uris(text))
        }
        MimeChoice::Image(mime) => {
            if truncated {
                tracing::debug!(mime, "dropping oversized image selection");
                return None;
            }
            if bytes.is_empty() {
                return None;
            }
            let decoded = decode_image(&bytes)
                .map_err(|err| tracing::debug!(mime, "image decode failed: {err}"))
                .ok();
            let dims = decoded.as_ref().map(|img| (img.width(), img.height()));
            let thumb = decoded.map(|img| {
                // thumbnail() would upscale sources smaller than the box;
                // small images pass through at native size instead.
                let small = if img.width() <= THUMB_MAX_W && img.height() <= THUMB_MAX_H {
                    img.to_rgba8()
                } else {
                    img.thumbnail(THUMB_MAX_W, THUMB_MAX_H).to_rgba8()
                };
                let (width, height) = (small.width(), small.height());
                Thumbnail {
                    width,
                    height,
                    rgba: Arc::from(small.into_raw().into_boxed_slice()),
                }
            });
            Some(Payload::Image {
                mime: mime.to_string(),
                bytes: Arc::new(bytes),
                dims,
                thumb,
            })
        }
    }
}

/// Decode clipboard image bytes with explicit limits: the bytes come from
/// arbitrary applications, and the `image` crate's defaults (512 MB alloc,
/// unbounded dimensions) are too generous for a decompression bomb landing
/// in a widget.
fn decode_image(bytes: &[u8]) -> image::ImageResult<image::DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits);
    reader.decode()
}

/// Non-empty, non-comment lines of a `text/uri-list` body (RFC 2483:
/// `#` lines are comments).
fn parse_uris(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Drain a pipe until EOF, the size cap, or the receive deadline. Returns
/// the bytes and whether the cap truncated them. Truncation closes the pipe
/// early; the stalled or oversized remainder is the owner's problem (it
/// sees EPIPE), not ours.
fn read_bounded(fd: rustix::fd::OwnedFd, cap: usize) -> Option<(Vec<u8>, bool)> {
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
            Ok(0) => return Some((buf, false)),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Strictly-greater: a payload of exactly `cap` bytes is
                // complete, not truncated — keep reading until we see either
                // EOF or a byte beyond the cap.
                if buf.len() > cap {
                    buf.truncate(cap);
                    return Some((buf, true));
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
    payload: Payload,
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
    pub(crate) fn push(&mut self, payload: Payload) -> bool {
        if payload.is_blank() {
            return false;
        }
        if self
            .entries
            .front()
            .is_some_and(|front| front.payload == payload)
        {
            return false;
        }
        // A re-copy of an existing entry keeps its key (promotion, not a
        // new entry); genuinely new content gets a fresh one.
        let key = match self
            .entries
            .iter()
            .position(|entry| entry.payload == payload)
        {
            Some(i) => self.entries.remove(i).expect("indexed entry").key,
            None => {
                let key = self.next_key;
                self.next_key += 1;
                key
            }
        };
        self.entries.push_front(HistoryEntry { key, payload });
        self.entries.truncate(self.cap);
        self.evict_over_image_budget();
        tracing::debug!(entries = self.entries.len(), "clipboard history updated");
        true
    }

    /// Drop oldest image entries (entirely) until encoded image bytes fit
    /// the budget. The newest entry is exempt — the copy that just
    /// happened must never be the one evicted.
    fn evict_over_image_budget(&mut self) {
        let mut total: usize = self
            .entries
            .iter()
            .map(|entry| entry.payload.image_size())
            .sum();
        while total > IMAGE_BUDGET_BYTES {
            let Some(i) = self
                .entries
                .iter()
                .enumerate()
                .skip(1)
                .rev()
                .find(|(_, entry)| entry.payload.image_size() > 0)
                .map(|(i, _)| i)
            else {
                return; // only the newest image remains; keep it
            };
            let evicted = self.entries.remove(i).expect("indexed entry");
            total -= evicted.payload.image_size();
            tracing::debug!("evicted image entry over budget");
        }
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

    pub(crate) fn payload_of(&self, key: u64) -> Option<Payload> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.payload.clone())
    }

    pub(crate) fn list(&self) -> ListGroup {
        ListGroup {
            entries: self
                .entries
                .iter()
                .map(|entry| list_entry(entry.key, &entry.payload))
                .collect(),
        }
    }
}

fn list_entry(key: u64, payload: &Payload) -> ListEntry {
    let (label, meta, thumbnail) = match payload {
        Payload::Text(text) => (entry_label(text), entry_meta(text), None),
        Payload::Uris(body) => {
            let uris = parse_uris(body);
            let label = uris.first().map(|uri| uri_file_name(uri)).unwrap_or("");
            let meta = (uris.len() > 1).then(|| format!("{} files", uris.len()));
            (label.to_string(), meta, None)
        }
        Payload::Image {
            bytes, dims, thumb, ..
        } => {
            let label = match dims {
                Some((w, h)) => format!("image {w}\u{d7}{h}"),
                None => "image".to_string(),
            };
            (label, Some(human_size(bytes.len())), thumb.clone())
        }
    };
    ListEntry {
        key: Some(key.to_string()),
        label,
        meta,
        thumbnail,
    }
}

/// Display name of one uri: the last path segment (files) or the uri
/// itself when there is no path structure.
fn uri_file_name(uri: &str) -> &str {
    uri.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(uri)
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
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
    payload: Payload,
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
            Event::Send { mime_type, fd } => {
                // Serve only for the live source; a Send racing its own
                // cancellation gets nothing (receiver sees EOF).
                if let Some(own) = state
                    .own
                    .as_ref()
                    .filter(|own| own.source == *source)
                {
                    write_bounded(fd, &own.payload.bytes_for(&mime_type));
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

    fn txt(s: &str) -> Payload {
        Payload::Text(s.into())
    }

    /// An image payload of `len` bytes, distinct per `seed`.
    fn img(seed: u8, len: usize) -> Payload {
        Payload::Image {
            mime: "image/png".into(),
            bytes: Arc::new(vec![seed; len]),
            dims: None,
            thumb: None,
        }
    }

    #[test]
    fn history_is_newest_first_deduped_and_capped() {
        let mut history = ClipboardHistory::new(3);
        assert!(history.push(txt("one")));
        assert!(history.push(txt("two")));
        assert!(history.push(txt("three")));
        assert_eq!(labels(&history), ["three", "two", "one"]);

        // Re-copying an old entry promotes it without duplicating.
        assert!(history.push(txt("one")));
        assert_eq!(labels(&history), ["one", "three", "two"]);

        // Copying the current front again changes nothing.
        assert!(!history.push(txt("one")));

        // The cap evicts the oldest.
        assert!(history.push(txt("four")));
        assert_eq!(labels(&history), ["four", "one", "three"]);
    }

    #[test]
    fn whitespace_only_selections_are_ignored() {
        let mut history = ClipboardHistory::new(4);
        assert!(!history.push(txt("   \n\t")));
        assert!(labels(&history).is_empty());
    }

    #[test]
    fn multiline_entries_show_head_line_and_count() {
        let entry = list_entry(0, &txt("\n  fn main() {\n    body\n}\n"));
        assert_eq!(entry.label, "fn main() {");
        assert_eq!(entry.meta.as_deref(), Some("4 lines"));

        let short = list_entry(1, &txt("hello"));
        assert_eq!(short.label, "hello");
        assert_eq!(short.meta, None);
        assert_eq!(short.key.as_deref(), Some("1"));
    }

    #[test]
    fn large_single_line_entries_carry_a_size_annotation() {
        let big = "x".repeat(10 * 1024);
        let entry = list_entry(0, &txt(&big));
        assert_eq!(entry.meta.as_deref(), Some("10 KB"));
    }

    #[test]
    fn uri_list_entries_show_file_name_and_count() {
        let body = "# comment\nfile:///home/u/shot.png\nfile:///home/u/notes.txt\n";
        let entry = list_entry(0, &Payload::Uris(body.into()));
        assert_eq!(entry.label, "shot.png");
        assert_eq!(entry.meta.as_deref(), Some("2 files"));

        let single = list_entry(1, &Payload::Uris("file:///tmp/a.tar.gz\n".into()));
        assert_eq!(single.label, "a.tar.gz");
        assert_eq!(single.meta, None);
    }

    #[test]
    fn image_entries_show_dims_size_and_thumbnail() {
        let thumb = Thumbnail {
            width: 2,
            height: 1,
            rgba: Arc::from(vec![0u8; 8].into_boxed_slice()),
        };
        let entry = list_entry(
            0,
            &Payload::Image {
                mime: "image/png".into(),
                bytes: Arc::new(vec![0; 3 * 1024 * 1024]),
                dims: Some((1920, 1080)),
                thumb: Some(thumb.clone()),
            },
        );
        assert_eq!(entry.label, "image 1920\u{d7}1080");
        assert_eq!(entry.meta.as_deref(), Some("3.0 MB"));
        assert_eq!(entry.thumbnail, Some(thumb));

        // Decode failure: still listed and restorable, just unlabeled dims.
        let opaque = list_entry(1, &img(1, 2048));
        assert_eq!(opaque.label, "image");
        assert_eq!(opaque.meta.as_deref(), Some("2 KB"));
        assert_eq!(opaque.thumbnail, None);
    }

    #[test]
    fn image_budget_evicts_oldest_images_but_never_the_newest() {
        let mut history = ClipboardHistory::new(10);
        history.push(txt("keep-me"));
        // Five 20 MB images: 100 MB total, budget is 64 MB — the two oldest
        // images must go (100→80→60), text is untouched.
        for seed in 0..5 {
            assert!(history.push(img(seed, 20 * 1024 * 1024)));
        }
        let labels = labels(&history);
        assert_eq!(labels.len(), 4, "3 images + the text entry: {labels:?}");
        assert!(labels.contains(&"keep-me".to_string()));

        // A single over-budget image is still kept (newest is exempt).
        let mut history = ClipboardHistory::new(4);
        assert!(history.push(img(9, IMAGE_BUDGET_BYTES + 1)));
        assert_eq!(history.list().entries.len(), 1);
    }

    #[test]
    fn entry_keys_are_stable_across_promotion_and_lookup() {
        let mut history = ClipboardHistory::new(3);
        history.push(txt("alpha"));
        history.push(txt("beta"));
        let alpha_key: u64 = history.list().entries[1]
            .key
            .as_deref()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(history.payload_of(alpha_key), Some(txt("alpha")));

        // Re-copying keeps the key; promoting moves without renaming.
        assert!(history.push(txt("alpha")));
        assert_eq!(
            history.list().entries[0].key.as_deref(),
            Some(alpha_key.to_string().as_str())
        );
        assert!(!history.promote(alpha_key), "already at front");
        history.push(txt("gamma"));
        assert!(history.promote(alpha_key));
        assert_eq!(history.list().entries[0].label, "alpha");

        // Unknown / evicted keys are inert.
        assert!(!history.promote(9999));
        assert_eq!(history.payload_of(9999), None);
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

        // Seed the selection with a payload, served by WatchState's own
        // source dispatch (the test connection acts as a normal copier).
        fn seed_selection(
            state: &mut WatchState,
            manager: &ExtDataControlManagerV1,
            device: &ExtDataControlDeviceV1,
            qh: &QueueHandle<WatchState>,
            payload: Payload,
        ) {
            let source = manager.create_data_source(qh, ());
            for mime in payload.offer_mimes() {
                source.offer(mime);
            }
            device.set_selection(Some(&source));
            state.own = Some(OwnSelection {
                source,
                payload,
                entry: 0,
            });
        }

        let text_payload = format!("clipboard-live-test-{}", std::process::id());
        seed_selection(
            &mut state,
            &manager,
            &_device,
            &qh,
            Payload::Text(text_payload.clone()),
        );
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

        // Wait until the watcher's snapshot contains a matching entry,
        // serving its receive (our Send events) via roundtrips meanwhile.
        fn wait_for_entry(
            state: &mut WatchState,
            queue: &mut wayland_client::EventQueue<WatchState>,
            snapshots: &prism_widgets_host::Channel<ModuleUpdate>,
            what: &str,
            matches: impl Fn(&ListEntry) -> bool,
        ) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(Instant::now() < deadline, "watcher never recorded {what}");
                queue.roundtrip(state).expect("roundtrip while waiting");
                while let Ok(update) = snapshots.try_recv() {
                    if let ModuleValue::List(group) = &update.snapshot.value {
                        if let Some(entry) = group.entries.iter().find(|entry| matches(entry)) {
                            return entry.key.clone().expect("entry key");
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        // Once our seed source is cancelled (the watcher re-owned the
        // selection), read the fresh offer back — served by the watcher.
        fn read_back(
            state: &mut WatchState,
            queue: &mut wayland_client::EventQueue<WatchState>,
            conn: &Connection,
        ) -> Payload {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "watcher never re-owned the selection"
                );
                queue.roundtrip(state).expect("roundtrip after restore");
                if state.own.is_none() {
                    if let Some(offer) = state.clipboard_offer.take() {
                        let mimes = state.offers.remove(&offer.id()).unwrap_or_default();
                        let payload = read_offer_payload(conn, &offer, &mimes);
                        offer.destroy();
                        if let Some(payload) = payload {
                            return payload;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        // Round 1: text.
        let entry_key = wait_for_entry(&mut state, &mut queue, &snapshots, "text", |entry| {
            entry.label == text_payload
        });
        handle.dispatch(ModuleAction {
            panel: panel.clone(),
            module: "clipboard".into(),
            kind: ModuleActionKind::ActivateEntry { entry: entry_key },
        });
        assert_eq!(
            read_back(&mut state, &mut queue, &conn),
            Payload::Text(text_payload)
        );

        // Round 2: image. A tiny PNG round-trips byte-exactly, and the
        // recorded entry must carry decoded dims and a thumbnail.
        let mut png = Vec::new();
        image::RgbaImage::from_fn(3, 2, |x, y| image::Rgba([x as u8 * 80, y as u8 * 100, 7, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut png),
                image::ImageFormat::Png,
            )
            .expect("encode test png");
        let image_payload = Payload::Image {
            mime: "image/png".into(),
            bytes: Arc::new(png),
            dims: None,
            thumb: None,
        };
        seed_selection(&mut state, &manager, &_device, &qh, image_payload.clone());
        queue.flush().expect("flush image seed");

        let entry_key = wait_for_entry(&mut state, &mut queue, &snapshots, "image", |entry| {
            entry.label == "image 3\u{d7}2" && entry.thumbnail.is_some()
        });
        handle.dispatch(ModuleAction {
            panel,
            module: "clipboard".into(),
            kind: ModuleActionKind::ActivateEntry { entry: entry_key },
        });
        // Payload equality is (mime, bytes) — thumbnails/dims are derived.
        assert_eq!(read_back(&mut state, &mut queue, &conn), image_payload);
        drop(handle);
    }

    #[test]
    fn exact_cap_payload_is_complete_not_truncated() {
        fn read_pipe(data: &[u8], cap: usize) -> Option<(Vec<u8>, bool)> {
            let (read_fd, write_fd) =
                rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
            let data = data.to_vec();
            let writer = std::thread::spawn(move || {
                let mut file = std::fs::File::from(write_fd);
                let _ = file.write_all(&data);
            });
            let result = read_bounded(read_fd, cap);
            writer.join().unwrap();
            result
        }

        let (bytes, truncated) = read_pipe(&[7u8; 1000], 1000).unwrap();
        assert_eq!((bytes.len(), truncated), (1000, false), "exact cap is complete");
        let (bytes, truncated) = read_pipe(&[7u8; 1001], 1000).unwrap();
        assert_eq!((bytes.len(), truncated), (1000, true), "over cap truncates");
        let (bytes, truncated) = read_pipe(&[7u8; 500], 1000).unwrap();
        assert_eq!((bytes.len(), truncated), (500, false));
    }

    #[test]
    fn uri_restores_serve_clean_text_for_plain_mimes() {
        let payload = Payload::Uris("file:///a/b.png\r\nfile:///c/d.txt\r\n".into());
        assert_eq!(
            payload.bytes_for("text/plain").as_ref(),
            b"file:///a/b.png\nfile:///c/d.txt"
        );
        assert_eq!(
            payload.bytes_for("text/uri-list").as_ref(),
            b"file:///a/b.png\r\nfile:///c/d.txt\r\n",
            "uri-list receivers get the verbatim body"
        );
        // Non-uri payloads serve identically for every mime.
        assert_eq!(txt("hi").bytes_for("text/plain").as_ref(), b"hi");
    }

    #[test]
    fn mime_preference_is_uris_then_text_then_image() {
        let text_and_image = vec![
            "image/png".to_string(),
            "TEXT/PLAIN".to_string(),
            "text/plain;charset=UTF-8".to_string(),
        ];
        assert!(matches!(
            choose_mime(&text_and_image),
            Some(MimeChoice::Text("text/plain;charset=UTF-8"))
        ));
        let with_uris = vec!["text/plain".to_string(), "text/uri-list".to_string()];
        assert!(matches!(
            choose_mime(&with_uris),
            Some(MimeChoice::Uris(_))
        ));
        let image_only = vec!["image/jpeg".to_string(), "image/png".to_string()];
        assert!(matches!(
            choose_mime(&image_only),
            Some(MimeChoice::Image("image/png"))
        ));
        assert!(choose_mime(&["application/x-tar".to_string()]).is_none());
    }
}
