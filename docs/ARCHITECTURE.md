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

## Near-Term Milestones

1. Port the `prism-bar` layer-shell host loop into `prism-widgets-host` against
   `PanelSpec` and `PanelSnapshot`.
2. Add provider scheduling on worker threads, feeding full snapshots into the
   host event loop.
3. Implement a generic `command` provider before bespoke API clients.
4. Add GitHub status provider with env-based token lookup and rate-limit state.
5. Add usage providers once their local data sources are clear.

## Prism IPC

Prism already has one-shot workspace/window/output IPC, but the long-lived
`EventStream` form that status surfaces want is not implemented yet. Use Wayland
protocols and local/API providers first; add Prism IPC as a provider when the
stream exists.
