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
- Wire the unused `WidgetsApp` out or into use (only `WidgetsBandApp` is built).
- Revisit the usage providers' string round-trip: structured values are
  flattened to `State` strings and re-parsed in the UI.

## Prism IPC

Prism already has one-shot workspace/window/output IPC, but the long-lived
`EventStream` form that status surfaces want is not implemented yet. Use Wayland
protocols and local/API providers first; add Prism IPC as a provider when the
stream exists.
