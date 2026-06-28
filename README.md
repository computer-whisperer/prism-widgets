# prism-widgets

Configurable Damascene `wlr-layer-shell` information panels for status surfaces
that do not belong in the primary `prism-bar`.

The project starts from the same architectural spine as `prism-bar`: Damascene
for GPU UI, `wlr-layer-shell` surfaces, and KDL config. The host opens real
layer-shell panels, polls providers on background worker threads, repaints only
when a snapshot actually changes, and reloads the config live.

The important dependency boundary is provider isolation. A future common runner
shared with `prism-bar` must not depend on GitHub, subscription-usage APIs, or
other application-specific integrations. Those belong in provider crates above
the host.

## Workspace Shape

- `prism-widgets-core` — shared panel/module data types, no GPU/Wayland/API deps.
- `prism-widgets-host` — provider-free runner boundary, intended extraction point
  for code shared with `prism-bar`.
- `prism-widgets-ui` — Damascene projection from snapshots to UI trees.
- `prism-widgets-providers` — application-specific data providers.
- `prism-widgets` — binary, config parsing, provider assembly.

## Configuration

Config lives at `$PRISM_WIDGETS_CONFIG`, else
`$XDG_CONFIG_HOME/prism-widgets/config.kdl`, else
`~/.config/prism-widgets/config.kdl`. A missing file uses a one-panel clock
default; parse errors are startup errors.

See [`resources/default-config.kdl`](resources/default-config.kdl).

Useful startup commands:

```bash
prism-widgets --print-config-path
prism-widgets --init-config
prism-widgets --dump-default-config
prism-widgets --config /tmp/widgets.kdl --dry-run
```

`--init-config` creates the parent directory and writes the documented sample
config, but it will not overwrite an existing file.

Panels reserve compositor layout space by default with `reserve true`.
Reserving panels on the same output edge are coalesced into one layer-shell
surface, so a left and right top panel share one compositor reservation and
stack as a unit with other bars such as `prism-bar`. Use `reserve false` for
overlay-style widgets. `exclusive-zone N` is available for manual layer-shell
overrides; `-1` opts out and `0` is the protocol's neutral mode.

Panels can choose `layout "bar"` or `layout "sidebar"`. Omitted layout is
inferred from the anchor: top/bottom anchors use `bar`, and left/right anchors
use `sidebar`. Bar panels render compact horizontal status clusters; sidebar
panels render vertical Damascene cards with item rows for each module. Sidebar
panels default to a 320 px reservation when `width` is omitted.

The layer-shell host only reserves and positions the band. Inside that band,
the UI is a Damascene widget cluster built from stock `card`, `toolbar`,
`item`, and status widgets rather than a manually painted bar.

Sidebar panel header text is hidden by default; set `show-header true` on a
panel if you want the panel id displayed above its modules.

When running as a layer-shell panel, `prism-widgets` watches the config file's
parent directory and reloads the panel configuration after changes. Invalid
reloads are logged and the current running configuration is kept.

Omit a panel's `output` node to show it on every output. Set `output "DP-1"`
or the relevant connector name for panels that should only appear on tertiary
displays.

## Providers

`clock` renders locally from `chrono`. `command` executes its `exec` string
through `sh -lc` with a 10 second timeout. Plain stdout becomes text; JSON can
emit richer values:

```json
{"label":"ready","detail":"main","status":"ok"}
{"current":42,"total":100,"status":"info"}
{"percent":73,"title":"quota","status":"warning"}
```

`usage source="claude"` reads Claude Code OAuth credentials from
`$HOME/.claude/.credentials.json` by default and calls Anthropic's
`/api/oauth/usage` endpoint. Add `account` and `claude-dir` when you keep
multiple Claude accounts:

```kdl
usage source="claude" id="claude-personal" account="personal" claude-dir="~/.claude" interval=300
usage source="claude" id="claude-work" account="work" claude-dir="~/.claude-work" interval=300
```

`usage source="codex"` reads Codex CLI auth from `$HOME/.codex/auth.json` by
default. It refreshes the OAuth access token when needed, then calls
ChatGPT's `/backend-api/wham/usage` endpoint using the same subscription auth
path as Codex. Use `account` for display labels, `codex-home` for a separate
Codex profile root, or `auth-path` for an exact auth file:

```kdl
usage source="codex" account="default" codex-home="~/.codex" interval=300
usage source="codex" id="codex-work" account="work" codex-home="~/.codex-work" interval=300
usage source="codex" id="codex-alt" auth-path="/secure/path/codex-auth.json" interval=300
```

Any other `usage` source uses the `SOURCE-usage-json` helper convention. A
`source` containing whitespace is treated as the full command. `github` uses
`gh api` and maps the latest Actions run into a status.
`workflow=` accepts a workflow file/ID or filters recent runs by display name.

`cpu`, `memory`, and `gpu` read local system load directly from `/proc` and
`/sys` (no extra tooling) and render as stacked percentage meters with a
context detail line:

```kdl
cpu interval=3          // util% + load (1m / cores); temperature in the detail
memory interval=5       // ram% + swap%; used / total GiB in the detail
gpu card=0 interval=3   // amdgpu busy% + vram% (+ power% if exposed); temp · watts
```

`cpu` samples `/proc/stat` twice per refresh to derive utilization, and reports
the 1-minute load average as a percentage of core count (over 100% means the
machine is oversubscribed). `gpu` reads one `amdgpu` DRM card per module
(`card=` is the `/sys/class/drm/cardN` index); the power meter is omitted on
cards that do not expose a draw and cap, such as integrated parts.

## Running

```bash
cargo run -p prism-widgets -- --dry-run
PRISM_WIDGETS_CONFIG=resources/default-config.kdl cargo run -p prism-widgets -- --dry-run
cargo run -p prism-widgets -- --config resources/default-config.kdl --dry-run
cargo run -p prism-widgets
```

The non-dry-run path requires a Wayland compositor with `wlr-layer-shell`.

## Building

```
cargo build --release
```

A wgpu-capable GPU (Vulkan on Linux) and system libwayland are required for the
live path — wgpu's WSI needs raw `wl_display`/`wl_surface` pointers. An
AUR-oriented `PKGBUILD` is provided for tagged releases; it installs the
`prism-widgets` binary, this README, and the license files.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
