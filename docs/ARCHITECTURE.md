# Architecture

`prism-widgets` is deliberately split so the runner can later be shared with
`prism-bar` without importing widget-specific integrations.

## Boundaries

`prism-widgets-core` owns stable data contracts:

- panel geometry and appearance
- module specs
- provider snapshots
- status/freshness metadata

`prism-widgets-host` should stay provider-free. Its eventual job is:

- Wayland registry/output/seat handling
- `wlr-layer-shell` surface lifecycle
- wgpu swapchain and Damascene runner ownership
- config reload application
- redraw scheduling from protocol events, provider snapshots, and animation
  deadlines

`prism-widgets-providers` owns application-specific dependencies and polling:

- GitHub CI/check/run status
- Codex/Claude/subscription usage probes
- command/file/local metric adapters
- clipboard history via ext-data-control (event-driven, see below)
- future Prism IPC event-stream provider

`prism-widgets-ui` turns snapshots into Damascene `El` trees. It should not
perform I/O.

## Provider Scheduling

Providers never run on the render thread. `prism-widgets-providers` spawns one
worker thread per polled module (`start_scheduler`); each loops fetch → push →
sleep, sending a `ModuleUpdate` into the host event loop through a calloop
channel. The host holds a lock-free `SnapshotCache` it reads at draw time and
mutates only from the channel callback. The clock is the exception: it is a
pure function of the current time, so the host renders it locally on a 1-second
tick rather than on a worker.

The clipboard module is the first event-driven provider: its worker opens its
own Wayland connection (never the host's — the provider seam stays one-way),
registers as an ext-data-control clipboard manager, and pushes a snapshot when
the selection changes instead of on a timer. It idles in a 500ms-timeout poll
on the connection fd, which is also how it notices shutdown. Selections
offering `x-kde-passwordManagerHint` are never read; only text mimes are
recorded, truncated at 128 KiB. `examples/clipboard-tool.rs` exercises the
same protocol from the other side (get/set/clear) for end-to-end testing
without wl-clipboard.

## Input and Actions

The host binds the first seat's pointer (`SeatState`; the
`delegate_dispatch2!` blanket covers the seat objects, so no extra delegates)
and feeds surface-local logical positions into the target surface's damascene
`Runner`, which synthesizes `UiEvent`s. The host dispatches those into
`WidgetsBandApp::on_event`; the app can't perform side effects, so clicks
accumulate in an outbox of `ModuleAction`s that the host drains after
dispatch and forwards through `ProviderHandle::dispatch` — the reverse
direction of the snapshot channel, equally opaque to the host.

Clipboard restore is the first consumer: clicking a history row routes an
`ActivateEntry` to the watcher (via an mpsc channel plus a wake pipe that
interrupts its poll), which re-owns the selection with that entry's text and
promotes the entry when the compositor echoes the selection back — the echo,
not the click, is the source of truth for history order. While serving, the
watcher skips reading its own offers (a self-read would deadlock against its
own `Send` handler). Known limitation: a config reload (or exit) while the
watcher owns a restored selection drops its connection and clears the
clipboard — selection ownership does not hand over across generations.

Not yet wired: keyboard (layer surfaces keep `KeyboardInteractivity::None`),
cursor shape (the compositor default shows over panels), touch, and Axis
(scroll) events — dropped until something scrolls.

Each provider generation carries an `epoch`. On config reload the host drops
the old `SchedulerHandle` (signalling its workers to stop), bumps the epoch,
and spawns a fresh generation; snapshots from workers still mid-fetch arrive
with the retired epoch and are ignored. The host repaints a surface only when
the display-relevant projection of its snapshots changes, so an unchanged
GitHub status or a clock whose minute has not advanced costs no GPU work.

`prism-widgets-host` stays provider-free: it knows only the `ProviderSpawner`
closure, the opaque `ProviderHandle` it drops to stop a generation, and the
`ModuleUpdate`s that arrive. The `--dry-run` path bypasses all of this and
fetches once synchronously via `SnapshotStore`.

## Remaining Work

- Per-module threads suit a status surface's handful of modules; swap
  `SchedulerHandle` for a bounded pool if module counts ever grow large.

## Prism IPC

Prism already has one-shot workspace/window/output IPC, but the long-lived
`EventStream` form that status surfaces want is not implemented yet. Use Wayland
protocols and local/API providers first; add Prism IPC as a provider when the
stream exists.
