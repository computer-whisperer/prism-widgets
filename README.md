# prism-widgets

Configurable Damascene `wlr-layer-shell` information panels for status surfaces
that do not belong in the primary `prism-bar`.

The project starts from the same architectural spine as `prism-bar`: Damascene
for GPU UI, `wlr-layer-shell` surfaces, and KDL config. The current host opens
real layer-shell panels with placeholder provider snapshots; provider polling
and live reload are next.

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

## Running

```bash
cargo run -p prism-widgets -- --dry-run
PRISM_WIDGETS_CONFIG=resources/default-config.kdl cargo run -p prism-widgets -- --dry-run
cargo run -p prism-widgets
```

The non-dry-run path requires a Wayland compositor with `wlr-layer-shell`.
