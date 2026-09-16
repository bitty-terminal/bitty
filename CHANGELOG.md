# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Host `bitty.process.spawn` surface for Layer-2 system-CLI execution
  (CTX-0445, OQ-013/OQ-053):** a bounded, consent-gated spawn surface that
  executes only allowlisted `[tools.*]` binaries/verbs. Every spawn checks
  request shape, allowlist routing (seam owned by CTX-0444; deny-all until it
  lands), `process.spawn` scope, explicit effect opt-in, and per-spawn
  `ConsentLedger` consent, then runs argv-only (never a shell) with a closed
  environment, concurrent bounded drain (no deadlocks), timeout kill+reap
  (timeout reports `Unknown`, never blind failure), and
  `is_untrusted_surface` labeling. The Lua bridge exposes
  `bitty.process.spawn({ ... })` returning
  `{ output, stderr, truncated, exit_code, untrusted }` with typed
  `E_SPAWN_UNAVAILABLE` / `E_CAPABILITY_DENIED` / `E_SPAWN_DENIED` /
  `E_SPAWN_TIMEOUT` / `E_SPAWN_FAILED` errors; per-call output on the panel
  path is capped at `8 KiB` to fit panel bus admission.
- **Configurable per-panel background image (CTX-0347, RFC-0001/OQ-042):**
  `decoration.background_image` / `background_fit` set a global background
  image and `views.<selector>.background_image` / `background_fit` override it
  per panel (per field, tier order). `decoration.background_image_roots` is a
  deny-by-default, global-only approved-root list that no `views.*` entry can
  widen. Accepted formats are PNG, JPEG (baseline/progressive), and static
  WebP; animated and malformed containers are rejected by header sniff before
  decode. Accepted bounds: BG-1 `4 MiB` encoded, BG-2 `4096x4096`, BG-3
  `64 MiB` decoded per image, BG-4 `256 MiB` aggregate, BG-5 `256` resident
  images, BG-6 one image per `View`, BG-7 `32` blits / `64 MiB` per frame.
  Fit modes are `fill`/`fit`/`center`/`tile`/`stretch`; the image paints inside
  the `View` content rect (DPI-correct, behind content) and never changes cell
  geometry or Terminal Truth. Decode and cache are bounded and off the hot
  path, keyed by canonical path plus content identity. `bitty config check`
  and startup run the same fail-closed load pipeline, so a missing file, an
  unapproved root, an unsupported/animated format, or an over-limit image
  rejects the whole config with a source-attributed key. `bitty --safe` ignores
  every background key and opens no image file.
- **Per-View appearance overrides (CTX-0343, RFC-0001/OQ-041):** a `views`
  table overrides appearance per panel with a closed selector grammar —
  `"*"` (every View) < a content type (`empty`/`terminal`/`rich`/`browser`)
  < `"ws:<1..=16>"` (one Workspace) < `"view:<ViewId>"` (one View). Accepted
  fields: `border_color`/`_focused`/`_idle`, `border_width`/`_focused`/`_idle`,
  `background_image`, and `background_fit`. Resolution is per field per View in
  tier order, independent of `init.lua` declaration order, so an unset field
  inherits the next-less-specific value and is never silently shadowed.
  `opacity`, `blur` (OQ-038), and `animations` (OQ-043) are reserved and
  rejected; `decoration.background_image_roots` stays global-only. Unknown
  selectors/fields/types, malformed labels, out-of-range widths, invalid
  colors, and bad fit values reject the whole reload fail-closed with a
  source-attributed diagnostic. AC-1/AC-2 are enforced fail-closed at two
  production points: during merge/reconcile every resolvable target (`*` and
  each content type) is checked and a violation rejects the whole reload with
  a `views[<selector>].<field>` diagnostic; a `ws:`/`view:` entry that is
  inert until it first matches is checked before the View creation, bind, or
  workspace move commits it (and fails that operation closed). The OQ-045
  width cue satisfies AC-2, AC-3 stays advisory. The `views` table adds no
  whole-table entry cap (RFC-0001: no new numeric ceiling; the closed
  selector set and Config VM parse budgets bound it). The layer is Live;
  `bitty --safe` ignores every `views.*` entry. `background_image`/
  `background_fit` land as resolution + validation only — image decode/render
  and root trust stay with CTX-0347.
- **Guided `bitty init` configuration wizard (CTX-0345):** the opt-in setup
  wizard now walks through the most useful shipped appearance and behavior
  keys in addition to shell/theme/font-size/keymaps: font family,
  decoration basics (`decoration.gaps_in`, `decoration.gaps_out`,
  `decoration.border`, `decoration.radius`), `terminal.scrollback`, and
  top-level `close_confirm`. The rendered `init.lua` always carries those
  keys and is validated through the effective config path before any write
  (shipped keys only; no secrets, no network). Non-interactive runs use
  `--yes` plus explicit value flags (`--theme`, `--font-family`,
  `--font-size`, `--scrollback`, `--close-confirm`, `--gaps-in`,
  `--gaps-out`, `--border`, `--radius`); each flag answers its step, skips
  that prompt, and is validated fail-closed. Without a TTY on stdin,
  `bitty init` exits 2 with usage instead of blocking, and an existing file
  is still only replaced with `--force` (keeping a `.bak` backup).
- **Close confirmation for running jobs (CTX-0370):** closing a pane or the
  window while a PTY still has a foreground job now prompts before discarding
  the work. Busy detection reads the kernel foreground process group
  (`tcgetpgrp`) and treats "foreground process group == the spawned shell" as
  idle; undetectable states (Windows ConPTY, dead PTYs) count as not busy
  rather than prompting blind. The gate reuses the accepted modal contract —
  the first close gesture arms a bounded overlay pill
  (`Running job: <name> — close pane/window anyway?`), repeating the close
  gesture confirms, `Esc` cancels — and never introduces a new keybinding.
  New top-level config key **`close_confirm = "always" | "when_busy" |
"never"`** (default `when_busy`; `always` prompts even for an idle shell,
  `never` disables). Unknown values fail closed, project config cannot
  declare it (a repo must not disable a data-loss guard), and the key is
  restart-required on reload. The workspace kill-confirm gate (CTX-0257) is
  unchanged.
- **Panel animations (CTX-0341, RFC-0002):** `appearance.animations` adds
  renderer-side, compositor-gated transitions for panel open and close, focus
  change, and workspace switch. Accepted defaults: open `150` ms /
  `ease_out`, close `120` ms / `ease_in`, focus `100` ms / `ease_in_out`,
  workspace `200` ms / `ease_in_out`; every duration is an integer in
  `0..=500` ms and the easing enum is closed
  (`linear | ease_in | ease_out | ease_in_out | spring`). `spring` is a
  reserved leaf whose parameters are deferred, so it resolves to
  `ease_in_out`. `enabled = true` and `reduced_motion = "auto"` are the
  defaults; `reduced_motion = "always"`, `enabled = false`, and `bitty --safe`
  are all equivalent to `0` ms instant final-state application. Out-of-range
  durations and unknown easings reject the reload with a source-attributed
  diagnostic (never clamped); `appearance.animations` reconciles live.
  Animations interpolate Core-owned chrome only (outline color/alpha) and are
  frame-on-demand: a completed transition schedules no periodic wakeups
  (PB-7), and the terminal grid, cursor, and scrollback are never
  interpolated.
- **Built-in theme preset catalog (CTX-0350):** the preset registry grows from
  one to 30 curated themes — `bitty-dark` (default, alias `dark`) plus
  tokyo-night (storm/day), catppuccin (mocha/macchiato/frappe/latte), github
  (dark/light), gruvbox (dark/light), solarized (dark/light), one
  (dark/light), ayu (dark/mirage/light), kanagawa (wave/lotus), rose-pine
  (main/moon/dawn), everforest (dark/light), dracula, nord, monokai, and
  night-owl. Each preset carries a `Dark`/`Light` `ThemeCategory`, aliases
  (e.g. `tokyonight`, `catppuccin`, `github`), and provenance (upstream
  source URL + license). `bitty list themes` enumerates the catalog with
  category and license, and `bitty init` accepts any catalog name or alias.
  Outline tokens are derived per palette so both CTX-0340 rules hold — AC-1
  (focused >= 3:1 vs background) and AC-2 (focused >= 3:1 vs idle) — with no
  preset exemption. Fail-closed tests pin unique names/aliases, 16 ANSI
  entries, foreground contrast >= 4.5:1 (`solarized-light` documented
  exemption), focused outline >= 3:1, focused/idle outline >= 3:1, and an
  `EffectiveConfig::validate()` sweep over every catalog preset.
- **Configurable focused/idle outline colors (CTX-0340):**
  `decoration.border_color` (base, unset), `decoration.border_color_focused`
  (default `#33CCFF`), and `decoration.border_color_idle` (default
  `#595959AA`) color each view outline by focus state, Hyprland-style.
  Values are canonical `#RRGGBB` / `#RRGGBBAA` (alpha defaults `FF`),
  reload live, and resolve theme token -> base -> explicit pair; an unset
  pair member inherits the resolved base. `--safe` forces opaque `#FFFFFF`
  focused / `#808080` idle. Validation enforces AC-1 (focused >= 3:1 vs
  the workspace background) and AC-2 (focused >= 3:1 vs idle when the two
  differ) fail-closed; AC-3 (idle >= 1.5:1) is a `bitty config check`
  advisory.
- **Configurable panel content inset (CTX-0333):** `decoration.content_inset`
  (logical px, default `6`, range `0..=32`, safe mode `0`) pads the painted
  content inside each view frame, so text no longer sits flush against the
  panel margin line. Exposed via `init.lua`, validated fail-closed, and
  mirrored in `bitty check`/`bitty inspect`.
- **`--fail-loud` startup mode (CTX-0481, issue #762):** `bitty --fail-loud`
  (or `BITTY_FAIL_LOUD=1`) aborts startup with exit 1 when the primary shell,
  a startup pane shell, or the IPC servo fails or is rejected, instead of
  continuing in a degraded session; the default stays fail-soft and a
  platform-unsupported IPC servo is never fatal.
- **IPC wire version negotiation (CTX-0484, issue #765):** `bitty-ipc`
  advertises `SUPPORTED_WIRE_VERSIONS` and offers
  `negotiate_wire_version(&[u16])`, which selects the highest mutual version
  and fails closed with `VersionMismatch` when there is no overlap (IPC and
  Agent RFC, Versioning section).

### Changed

- **Optional panel experiences extracted from `bitty-runtime` (CTX-0438,
  024 §17.3):** the first-party `bitty-terminal.ai-panel` and
  `bitty-terminal.mail-panel` implementations and their state moved out of the
  microkernel runtime crate into the new in-tree staging crate `bitty-panels`,
  which consumes only the public Panel Runtime path
  (`bitty_runtime::registry` + `bitty_ui`) through a private panel-session
  scaffold. `bitty-runtime` keeps mechanism and no longer carries
  `ai_panel`/`mail_panel`; the workspace grows to 20 crates. Bundled catalog
  entries, manifests (`bitty-plugin-host::bundled`), capability strings, wire
  shapes, and public panel behavior are unchanged; the full plugin splits stay
  gated on the panel-provider contract (OQ-058) under `CTX-0402`/`CTX-0403`.
  Panels recorded as Core or already split (`browser-panel`, the
  `palette`/`statusline` Core helpers, `project`, `shell-integration`,
  workspace) deliberately stay in `bitty-runtime` in this phase.
- **Packaging artifacts consolidated under `packaging/` (CTX-0432):** the
  Homebrew formula moved from the duplicated `Formula/` and
  `homebrew/Formula/` copies to `packaging/homebrew/bitty.rb`, the Scoop
  manifest now lives only at `packaging/scoop-bitty.json`, and the root
  `PKGBUILD` mirror was retired in favor of `packaging/PKGBUILD` (the AUR
  publish jobs already copied the packaging recipe). The packaging guard
  scripts and release validation no longer check the retired mirror paths.
- **Palette plugin extracted from the bundled catalog (CTX-0397, OQ-053):**
  `bitty-terminal.palette` moved to the independent
  `bitty-terminal/palette` package and is no longer part of the
  bundled-disabled catalog, so the id can be installed through the external
  package path. The catalog now stages seven plugins. The Core Panel Runtime
  overlay helpers and theme bridge in `bitty-runtime::palette` are unchanged;
  the plugin manifest, commands, and events are identical, and the independent
  Lua package requests `ui.rich` in addition to `ui.overlay` because the
  accepted Plugin API v1 overlay path requires it.
- **Statusline plugin extracted from the bundled catalog (CTX-0398, OQ-053):**
  the `bitty-terminal.statusline` presentation moved to the independent
  `bitty-terminal/statusline` package and is no longer part of the
  bundled-disabled catalog, so the id can be installed through the external
  package path. The catalog now stages seven plugins. The workspaceline claim
  and workspace lifecycle stay bundled in `bitty-terminal.workspace`, and
  shell integration stays the OSC 7/133 semantic-zone provider; the plugin id,
  capabilities (`terminal.semantic-read`, `ui.rich`), and lazy events are
  unchanged from the bundled manifest. The Core Panel Runtime helpers in
  `bitty-runtime::statusline` are unchanged.
- **Git-panel plugin extracted from the bundled catalog (CTX-0400, OQ-053):**
  `bitty-terminal.git-panel` moved to the independent
  `bitty-terminal/git-panel` package and is no longer part of the
  bundled-disabled catalog, so the id can be installed through the external
  package path (CTX-0406 reserves bundled ids). The catalog now stages seven
  plugins. The plugin id, capabilities (`panel.provider`, `panel.create`,
  `process.spawn:git`, `terminal.semantic-read`, `fs.read:~/projects/**`),
  commands (`open`, `status`, `diff`, `log`, `branch`), and observation
  events are unchanged from the bundled manifest, and the allowlisted `git`
  verbs (`status`, `diff`, `log`, `branch`, `show`, `rev-parse`, `ls-files`)
  with bounded output follow the accepted Layer 2 `[tools.git]` slice
  (CTX-0425). The bundled review implementation in
  `bitty-runtime::git_panel` is removed; the independent Lua package carries
  the listing and allowlist helpers. No behavior change for existing users:
  the bundled set stays disabled by default and `bitty --safe` still rejects
  `bitty-terminal.*` exactly like third-party ids.
- **File-manager plugin extracted from the bundled catalog (CTX-0399, OQ-053):**
  `bitty-terminal.file-manager` moved to the independent
  `bitty-terminal/file-manager` package and is no longer part of the
  bundled-disabled catalog, so the id can be installed through the external
  package path (CTX-0406 reserves bundled ids). The catalog now stages six
  plugins. The plugin id, capabilities (`panel.provider`, `panel.create`,
  `terminal.semantic-read`, `fs.read:~/projects/**`, optional
  `fs.write:~/projects/**`), commands (`open`, `preview`, `rename`), and
  observation events are unchanged from the bundled manifest. The bundled
  review implementation in `bitty-runtime::file_manager` is removed; the
  independent Lua package carries the pure listing/navigation/preview policy
  (no spawn; fs access host-mediated) with a headless Lua suite. No behavior
  change for existing users: the bundled set stays disabled by default and
  `bitty --safe` still rejects `bitty-terminal.*` exactly like third-party
  ids.

- **Selection no longer auto-copies by default (CTX-0371):** `selection.auto_copy`
  now defaults to `false` (was `true`), so a committed mouse selection keeps its
  highlight and no longer overwrites the system clipboard (or the Linux primary
  selection) on release. This matches kitty (`copy_on_select no`) and ghostty
  (`copy-on-select none`). Press `Ctrl+Shift+C` to copy explicitly, or opt back
  in with `selection = { auto_copy = true }` in `init.lua`. Selection clearing
  (click / `Esc` / typing / IME commit) is unchanged.

- **Unified panel gaps (CTX-0333):** `decoration.gaps_in` now defaults to `6`
  (was `4`), matching `decoration.gaps_out`, so the default sibling
  (panel-to-panel / panel-to-terminal) and container gaps read as one spacing.
  The coherent model is `effective gap = decoration.gap * DPI_scale +
layout.gap_cells * cell_axis`; with the default `layout` cell gaps of `0`
  both effective gaps are `6` logical px. Views now paint inside the
  `border + content_inset` padding, which changes default tiled content grids.

- **Scrollbar overlays by default (CTX-0362):** `scrollbar.mode` now defaults
  to `auto` (was `hidden`): the right-edge scrollback thumb is transparent at
  rest and reveals on mouse proximity/hover, then hides again on leave. It
  stays geometry-neutral (present-layer overlay, zero fills and zero layout
  delta until engaged), and `hidden`/`always` remain selectable. Wheel
  scroll-speed keys `terminal.scroll_lines_per_notch` (default `3`, range
  `1..=32`) and `terminal.scroll_pixels_per_notch` (default `16`, range
  `1..=256`) are unchanged and continue to scale the scroll amount.
- **`Esc` is consumed only by confirmation gates (CTX-0475, issue #756):** an
  `Esc` press is consumed only when it cancels a real confirmation gate
  (suspicious-paste, workspace kill-confirm, close-confirm); dismissing the
  informational help popup now passes `Esc` through and delivers `\x1b` to the
  focused PTY, so fullscreen apps (vim/less) no longer desync their mode
  state. Gate semantics are unchanged: a cancelled paste or close never leaks
  bytes to the PTY, and when a gate and the help popup coincide both close and
  the gate keeps the consume.
- **Invalid CLI layout/focus/log values fail closed (CTX-0480, issue #761):**
  invalid `--split`, `--split-ratio`, `--log-level`, `--layout`, and `--focus`
  values now print usage and exit 2 instead of warning and running defaults
  (including `--split=h:junk`, `h:`, `:0.5` without an axis, and malformed
  `--layout` specs such as partial overlay geometry or non-finite ratios);
  finite out-of-range ratios/stack counts still clamp loudly, negative
  space-form `--split-ratio -5` is parsed as an out-of-range value and clamps
  like `--split-ratio=-5`, and an unresolvable-but-valid `--focus <id>` still
  warns and continues.
- **Clipboard payload limits are explicit (CTX-0478, issue #759):**
  `set_text`/`set_primary`/`get_text`/`get_primary` reject over-limit payloads
  with the new typed `ClipboardPayloadTooLarge { len, max }` instead of
  truncating silently (a rejected write leaves both selections unchanged); the
  bounded accessors used by chord/right-click paste and the OSC 52 read reply
  clip an oversized value at a UTF-8 char boundary (8 KiB default), so an
  oversized system clipboard pastes its bounded prefix instead of becoming a
  silent no-op; lossy reads return an empty string on a system read failure
  instead of replaying a stale in-memory value.
- **Agent/perf/compat surfaces are honest (CTX-0484, issue #765):**
  `AgentMessage` carries an explicit `ContentTrust` label (default
  `Untrusted`, `Trusted` only via the new `AgentMessage::new_trusted`, and
  `Role::Tool` messages can never be trusted); `IdleReport` gains a tri-state
  `cpu_budget_verdict()` (`Met`/`Exceeded`/`Unmeasured`, with
  `meets_cpu_budget()` false when unmeasured); `LatencyReport` discloses its
  measurement `mode` (injected echo vs real PTY echo) and adds work
  p50/p99/min; and the compat-lab matrix derives reference outcomes from the
  on-disk dumps instead of hardcoding `SKIP`.

### Fixed

- **Bounded damage history with full-grid fallback (CTX-0468, issue #749):**
  `State::damage_since` returns a single full-grid region when the requested
  generation predates the oldest retained damage batch, when a stale caller
  has no retained history, or before the union can exceed the 256-region cap,
  instead of returning only surviving partial batches and under-painting.
  Up-to-date callers keep the O(1)/O(window) path, and every fallback
  over-damages to the full grid, never under-damages.
- **Terminal state erase/resize/reflow/hyperlink correctness (CTX-0469, issue
  #750):** erase ranges expand both wide-pair edges independently; a
  height-only resize preserves wrapped-line continuation flags (width changes
  still break them); search reads the live grid row instead of cloning up to
  1M cells; width-resize reflow collects logical lines once and reuses its
  per-line row counts; the 256-region damage cap is evaluated before quadratic
  merge work; origin-mode CPR uses saturating subtraction (a cursor above the
  region reports row 1); and the hyperlink table evicts oldest-first with
  monotonic ids, so an evicted id fails closed to no link and is never reused.
- **Runtime teardown joins forwarders with bounds (CTX-0472, issue #753):**
  `Runtime` gains `Drop`/`shutdown`/`shutdown_with_timeout`, which clear the
  waker, drop receivers and owned PTYs, and join primary and pane forwarders
  with a 500 ms bound; respawn and pane close join the old forwarders instead
  of detaching them; the panel worker probe is cancellable and its shutdown is
  bounded at 2 s, so teardown can no longer hang on a wedged worker.
- **Runtime hot-path hygiene (CTX-0473, issue #754):** forwarder-thread spawn
  refusal no longer panics or drops the `PtyReader` (the reader is handed back
  so the child's output keeps flowing; the refusal is counted and warned);
  best-effort PTY writes and flushes now account written/dropped bytes instead
  of silently discarding failures; and OSC 52/kitty reject and spawn-failure
  logging is throttled (4 messages per 1 s per site, with a suppressed-count
  suffix) so a hostile flood cannot spool unbounded stderr.
- **Spawn timeouts preserve output; PTY polling is budgeted (CTX-0476, issue
  #757):** a timed-out `bitty.process.spawn` now returns the stdout/stderr
  bytes collected before the kill (with byte-count evidence) instead of empty
  strings, and drain joins are bounded at 1 s (a stalled grandchild-held pipe
  is detached and reported as an evidence ref); `poll_pty` and
  `pump_pane_sessions` share one global budget (32 chunks / 256 KiB / 10 ms
  per poll) and the forwarder coalesces immediately-available chunks behind a
  single wakeup, so a hostile flood can no longer stall the render thread or
  storm the event loop.
- **PTY and diagnostics platform gaps (CTX-0478, issue #759):** a foreground
  job name is re-checked against the current foreground group after the read
  (a reused pid no longer reports an unrelated process; the pid is still
  reported); `terminal.shell` or spawn argv containing NUL reports a precise
  "must not contain NUL bytes" diagnostic via `PtyError::NulInProgram`;
  `Clipboard::new` records and exposes why it degraded to the headless buffer
  via `headless_reason()`; and ConPTY's missing foreground surface is
  documented and pinned as "cannot determine" (never busy) on Windows.
- **Config validation coverage, trust comparison, platform roots (CTX-0479,
  issue #760):** diagnostics now cover `selection`, `close_confirm`, `mod_key`,
  and `views`; the config trust comparison is normalized and round-trips
  through its durable store form; and Windows `%APPDATA%`/`%LOCALAPPDATA%`
  participate in config-root resolution.
- **Live plugin snapshots and zoom ownership (CTX-0481, issue #762):** the
  plugin `LiveSnapshot` commits the runtime's committed generation from
  `drive_tick` with live geometry, cursor, alt-screen mode, and title, and
  refuses a generation regression, so the frozen generation-1 view cannot
  recur; `ZoomState` restores the real tree only while the runtime still holds
  the installed proxy (structural identity, reflow-stable), and a ctl
  pre-mutation hook routes layout-mutating verbs so a ctl split can no longer
  land on the zoom proxy and be dropped by the later zoom restore.
- **Caret/tilde upper-bound overflow errors cleanly; shorthand stays accepted
  (CTX-0493, issue #793):** `^4294967295`, `^0.4294967295`, and
  `~1.4294967295` no longer panic under overflow checks or silently wrap to a
  never-matching range; the caret/tilde upper-bound increments use
  `checked_add` and fail with a clean `PackageError` ("has no representable
  successor"), while the maximal representable bounds (`^4294967294`,
  `~4294967295.4294967294`) still expand. Partial comparator shorthand
  (`>=0.5,<1.0`, `>=2.30`) remains accepted (zero-padded to `>=0.5.0,<1.0.0`
  and `>=2.30.0` by CTX-0466) and is now regression-pinned across
  `bitty-package`, `bitty-runtime`, and `bitty-plugin-host`.

### Security

- **Package trust verification is fail-closed (CTX-0462, issue #743):** the
  forgeable V-C signature stub (a `verify_signature` that recomputed
  `SHA-256(key_id || manifest || artifact)`, mintable by anyone holding the
  public `key_id`) is removed. Signature records are now rejected as
  unavailable, so `TrustMode::Signed` installs fail closed instead of
  accepting forgeries; the reserved Ed25519-shaped wire fields stay for the
  OQ-029 follow-up (bitty#767, DEC-0059). V-A/V-B verification and
  signature-first pin ordering are unchanged.
- **Devtools IPC transport verifies its endpoint (CTX-0463, issue #744):**
  `transport_attested_peer` re-verifies the bound endpoint per connection
  (0700 directory and 0600 socket, both owned by the runtime uid, symlinks
  rejected), the client checks the same ownership before connecting, and a
  `BITTY_SOCKET` override is verified rather than trusted verbatim; child
  token error paths return static, token-free reasons. True per-connection
  `SO_PEERCRED` fd checks stay deferred (tracked for CTX-0159).
- **Lua sandbox budgets cover compile and host calls (CTX-0464, issue #745):**
  `drive_chunk` refuses chunks over a 1 MiB cap before parsing; the wall-clock
  budget now includes `Closure::load`, so an over-budget compile suspends with
  `WallClockExceeded` without executing; mutating host services (`store.*`,
  `notify.show`) re-check the call deadline so post-deadline effects never
  commit; and `bitty.process.spawn` runs under its own deadline (5 s default,
  30 s max). Tighter call-site caps (64 KiB config, 8 KiB events) still apply.
- **Plugin host predicates are hardened (CTX-0465, issue #746):** the
  allowlisted `git` verbs deny glued `-c*`/`-C*`, every `--config*` form, and
  `--paginate`/`--pager` (`--no-pager` stays allowed), and spawn environments
  reject `PAGER`/`GIT_PAGER`; manifest `fs.read`/`fs.write` patterns reject
  absolute paths, `~user/`, `..` on either separator, and credential
  locations (`~/.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`, `.docker`,
  `~/.config/gh|gcloud`) while patterns like `~/projects/**` remain valid; an
  intercept timeout now denies for every decision (was proceed); a
  per-capability revoke persists its denial until an explicit re-grant
  (`clear_cap_denial`), and revoking the last capability still escalates to a
  full revoke.
- **Package compat ranges and source URLs are validated (CTX-0466, issue
  #747):** installs evaluate the real `VersionReq` against the host version
  fail-closed (mismatched, unparseable, or missing host rejected), source URLs
  are scheme-allowlisted (registry `https` only; git `https` and `git+ssh`),
  host/userinfo/port shapes are checked, and git revisions get charset and
  structure checks while mutable names (`HEAD`, branches) remain allowed with
  SHA pinning recommended, not enforced. Shorthand comparators (`1.2`)
  normalize to `1.2.0`; IPv6 hosts, scp-like `git@host:` URLs, and plain
  `ssh://` stay rejected in v1.
- **Rich background identity and kitty chunked caps (CTX-0467, part of issue
  #748):** background images are acquired through a single fd-pinned open with
  pre/post fstat re-verification and cached under a key derived from the
  decoded bytes (length, post-read mtime, FNV-1a/64 content hash), so a
  same-length rewrite with a restored mtime re-decodes instead of poisoning
  the cache; chunked kitty transmissions are bounded by the unified 4 KiB
  per-transmission cap, over-cap transfers fail `Oversize` before buffering,
  and chunked admission never evicts resident images (fits-cap-but-no-room and
  count-full completion fail `LedgerFull`) while only single-shot ingest
  evicts.
- **IPC channel/bridge hostile-peer robustness (CTX-0483, issue #764):**
  `RateLimiter` now honors `limit_per_sec` with a token bucket (sustained load
  capped at the configured rate, bursts up to `burst` still allowed, backwards
  clock steps accrue nothing); expired requests are removed from the pending
  table and the not-yet-delivered request queue, so a timed-out request can
  never execute after its deadline; and answers for unknown/expired/completed
  ids are dropped before enqueue instead of filling the 64-deep inbound queue.
- **Composer external editor is allowlisted with owner-only temp files
  (CTX-0485, issue #799):** `$VISUAL`/`$EDITOR` are no longer trusted as an
  executable name. The composer resolves only the exact bare names
  `nvim`/`vim`/`vi` (`EDITOR_ALLOWLIST`); the first non-empty variable wins and
  a hostile value fails closed with `EditorError::NotAllowed` before any temp
  file is written or child spawned (no fallback to the other variable), so
  `EDITOR=sh`, paths, flags, spaces, and shell metacharacters are denied. The
  composer temp file drops its `.sh` suffix so no editor plugin, file manager,
  or OS handler treats terminal content as executable, and stays owner-only:
  on Unix the file is created with mode `0o600`, the mode is re-asserted after
  the write, and a permission failure deletes the file and fails closed.
  Non-Unix inherits the per-user temp-directory ACL (documented residual).
- **OSC 8 `file:` links are never presented; clipboard writes default to Gated
  (CTX-0486, issue #802):** OSC 8 hyperlink presentation now accepts only
  `http`/`https`/`mailto` URIs, so untrusted output can no longer surface a
  clickable local-file span (`file:///etc/passwd`, case variants, and encoded
  spellings are rejected outright); opening a local file remains the runtime's
  explicit `FileUrlActivation` gesture path. The OSC 52 write-capture state
  carries a `ClipboardPolicy` defaulting to `Gated`: `Gated`/`Denied` reject
  every write with the new `ClipboardOutcome::WriteDenied`, store nothing, and
  count `denied_writes`, and only an explicit `Allow` captures, so a granted
  read can never expose a payload that was not explicitly allowed.
- **Spawn environments deny git config and external-process vectors
  (CTX-0488, issue #804):** explicit spawn env entries are validated before
  routing, scope, or consent against a denylist that closes the env-encoded
  forms of the argv gate: `GIT_CONFIG`, `GIT_CONFIG_COUNT`,
  `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_NOSYSTEM`/`SYSTEM`/`GLOBAL` and the
  numbered `GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*` families;
  `GIT_EXTERNAL_DIFF`, `GIT_DIFF_OPTS`, `GIT_EDITOR`, `GIT_SEQUENCE_EDITOR`,
  `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_PROXY_COMMAND`, `GIT_ASKPASS`,
  `SSH_ASKPASS`, and `GIT_EXEC_PATH`; and the repo-identity escapes
  `GIT_DIR`/`GIT_WORK_TREE`.
  Matching is ASCII case-insensitive and prefix-aware for the numbered
  families, so a case-folded Windows lookup cannot bypass; near-miss and benign
  settings (`MY_PAGER`, `GIT_TERMINAL_PROMPT`, `LANG`) stay allowed.
- **Filesystem patterns reject overbroad roots and sensitive locations
  (CTX-0489, issue #796):** manifest `fs.read`/`fs.write` validation no longer
  accepts a `~`-rooted pattern that matches unknown home children — bare `~`,
  `~/`, `~/**`, `~/*`, `~/.*`, and foreign `~user` homes fail closed because a
  literal first child must pin the grant. Sensitive credential segments
  (`.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`, `.docker`, and the
  `~/.config/gh`/`gcloud` prefixes) match ASCII case-insensitively on either
  `/` or `\`, and empty/`.` segments are normalized away first so
  `~/.config/./gh/...` and `~//.config/gh/...` cannot bypass; valid narrowing
  patterns such as `~/projects/**` and `~/.config/ghost/**` stay allowed.

### Per-pane damage tracking: splits stop forcing full repaint (CTX-0386, issue #642)

- Each split pane now derives its damage from its own grid generation ring
  (`State::damage_since` against a per-`PaneSession` `last_presented_generation`)
  instead of any live pane session forcing a full per-leaf redraw. A pane that
  produced no output contributes its retained complete draw list, so a frame
  driven by one pane re-examines only that pane while the composited frame
  still carries every pane. Headless two-pane measurement: a one-pane update
  frame drops from 1629 to 815 cells examined and 85 to 5 glyphs emitted
  (`PresentStats::cells_examined`/`glyphs_emitted`, new per-frame work
  counters). Layout/focus edits, window resize, DPI/font, appearance
  transitions, and the first frame still force a full invalidation; the cursor
  overlay is recomputed per frame so a reused leaf never carries a stale
  cursor.

### Idle-outline visibility across preset themes (CTX-0354, issue #630)

- Cleared the advisory CTX-0340 AC-3 idle-outline warning (`idle >= 1.5:1`
  against the background) for the five presets the CTX-0350 AC-2 fix left
  below the floor: solarized-dark (`#073642` -> `#274F58`), one-dark
  (`#323844` -> `#3B4E64`), rose-pine-dawn (`#DFDAD9` -> `#B7C1C6`),
  everforest-dark (`#543A48` -> `#5B4E53`), and everforest-light (`#EAEDC8`
  -> `#BCC2AC`; it was 1.00:1 and effectively invisible). Each idle is a
  Bitty-owned blend of the preset's selection color toward its focused
  outline, placed at the midpoint of the window where AC-2 and AC-3 both hold;
  no AC-1/AC-2 value changes.
- `tokyo-night-day` remains the one documented advisory exemption: no palette
  surface or blend clears AC-3 while holding AC-2 against its focused
  `#007197`, so it keeps the visible `#B7C1E3` selection idle (1.38:1) and
  `bitty config check` still reports the advisory. The catalog test now
  asserts AC-3 for every non-exempt preset, rejects a stale exemption, and
  measures with the runtime `OutlineColor` compositing path.

### Test determinism: cwd_inherit one-shot pwd race on macOS (CTX-0376, issue #623)

- `new_pane_inherits_focused_pane_osc7_cwd` and `focused_pane_selects_the_inherited_cwd`
  no longer spawn a one-shot `/bin/pwd -P` as the pane shell: on macOS the
  kernel discards unread PTY slave output when the child exits before the
  reader drains it (XNU `S_CTTYREF`; Apple Developer Forums thread 663632,
  pexpect#662, Ruby bug #20682), so the grid could stay blank forever and no
  wait window could recover it. The pane shell now runs
  `pwd -P; exec sleep 30` under `/bin/sh`, printing the same physical cwd while
  keeping the slave open; the assertion still proves the inherited cwd from the
  spawned child's own output.

### Help popup occludes grid text (CTX-0336, issue #559)

- Fixed the `Mod+backtick` which-key help popup (`Mod+?`) painting the
  underlying terminal text through its opaque background and border: `DrawList`
  composites every glyph after every fill, so base-grid glyphs repainted over
  the panel. The panel now drops pre-existing glyphs whose pixel box intersects
  its frame, keeping the popup fully opaque regardless of `window.opacity`.
- Added a headless regression (`help_panel_occludes_grid_inside_its_frame`)
  that brackets the panel from its border pixels and proves the underlay no
  longer bleeds through.

### Plugin store: XDG source resolution, staging, and integrity (CTX-0329)

- Ratified `plugin-host-runtime-rfc` Gap B implemented in `bitty-runtime`'s
  `plugin_runtime`: the plugin store lives at
  `$XDG_DATA_HOME/bitty/plugins/` with stored manifest bodies and module trees
  under `packages/<id>/<version>/` and an atomic `current.json` active pointer
  written by write-temp-then-rename.
- Loading reads the pointer, loads the recorded manifest body, and re-verifies
  `manifest_hash` and the module-tree `content_digest` fail-closed before any
  VM is created. A missing body, hash/digest mismatch, identity/version
  mismatch, or store-root escape is a typed `NotFound`/`Integrity` failure with
  no silent fallback to another revision or to bundled content. Native
  artifacts (`.so`, `.dll`, `.dylib`, `.node`) and the 4096-file / 16 MiB /
  1024-byte tree bounds are re-checked during the same scan.
- The runtime source record (`PluginRecord`) extends the shipped
  `{source, manifest_hash, enabled, granted}` shape with `source_class`,
  `plugin_id`, `version`, `root`, and `content_digest` (RFC B.4). The closed
  source-class set is `bundled | registry | git | local-path`; only `bundled`
  is first-party, and `--safe` creates no VM for any other class and never
  reads the installed store tree (RFC A.4 rule 6). A `current.json` record may
  not self-declare `bundled`: bundled provenance comes only from a configured
  trusted root, and a `bundled` record in the store fails closed.
- `local-path` development sources resolve from their canonical absolute path
  read-only, are always re-digested, and are visibly unverified; drift keeps
  the package unverified rather than hidden, and a changed/non-canonical root
  is rejected. Startup discovery orders bundled, then installed XDG records,
  then development roots, so provenance is deterministic and a bundled package
  wins an id collision.

### Missing-glyph fallback: coverage chain + tofu box (CTX-0368, issue #610)

- The per-glyph fallback chain is now coverage-driven and platform-pinned:
  Linux appends `Noto Color Emoji` after the existing mono/`DejaVu Sans Mono`/
  `Noto Sans Symbols 2` tails; macOS uses `Menlo`/`Monaco`/`Apple Braille`/
  `Apple Symbols`/`Apple Color Emoji`/`Arial Unicode MS`; Windows uses
  `Consolas`/`Cascadia Mono`/`Segoe UI Symbol`/`Segoe UI Emoji`/`Arial Unicode
MS`. Every chain stays deterministic and bounded (`<= 8` tails, `<= 12`
  faces) and is loaded once at startup. Color emoji render monochrome via the
  atlas coverage flatten; color-glyph rendering remains a follow-up open
  question.
- `FallbackRasterizer::resolve` reports the outcome explicitly (`covered`,
  winning face, bitmap), so a scalar the primary lacks resolves through the
  chain and is reported covered instead of silently blank.
- When no loaded face covers a scalar, the grid now paints the text-rendering
  RFC tofu box (1 px outline at the cluster's cell extent, spanning both
  columns for wide scalars) and increments the new
  `RenderCounters::missing_glyphs`, instead of leaving the cell blank. Grid
  width semantics (`char_cell_width`) are unchanged.
- Tests: headless coverage/tofu/determinism/bounded-cache units in
  `bitty-render` plus a skip-graceful live host-font test; live screenshot
  comparison against Ghostty in the task evidence.

### Nested tmux blank window: primary grid follows the decorated content frame (CTX-0375, issue #615)

- Fixed a nested `tmux` (and any full-screen TUI drawing on the last row)
  rendering a blank/bottom-cropped window: `reflow_to_grid` sized the primary
  terminal grid and PTY to the window grid while `present_frames` derives each
  leaf's decorated content grid (`gaps_out + border + content_inset`), so the
  present viewport cropped the right/bottom rows and never painted tmux's
  status bar. The primary grid now follows the primary owner leaf's decorated
  content frame on window resize and DPI adoption, mirroring the existing
  split path (CTX-0359).
- Added a headless regression (`nested_tmux_present`) that pins the
  grid-equals-content-frame invariant and proves a tmux-style status bar on
  the last grid row paints.

## [0.0.20] - 2026-09-11

### Release highlights

- **Plugin runtime Gap A (CTX-0328):** per-plugin `!Send` VM lifecycle and
  atomic activation landed across `bitty-plugin-host`, `bitty-lua`,
  `bitty-runtime`, and `bitty-app`; bundled plugins (including the activity
  plugin) now load and activate, and `--safe` starts with no third-party VM.
- **`ctl` D1/D2/D3 correctness (CTX-0321/0322/0323):** terminal text commands
  render grid text instead of the Rust `Debug` snapshot; workspace ids are
  stable `ws{seq}` values across `new`/`list`/`focus`/`close`/`move`;
  `terminal spawn` creates an observable pane session.
- **Kitty graphics:** APC `G` parser wiring with base64 unwrap (CTX-0255/0256),
  bounded PNG/RGB decode (CTX-0247), placement/rasterize/composite into the
  present path (CTX-0248), per-pane origin binding with a per-frame blit
  budget and raster cache (CTX-0252/0254), GPU display gate and saturating
  origin math (CTX-0253), and real-GPU texture upload + blit (CTX-0291).
- **Workspace decoration and presentation (CTX-0292):** Core-owned gaps px,
  border, and radius, plus SDF rounded decoration fills and inner-arc glyph
  clipping (CTX-0311) and scrollbar overlay tracking the decorated content
  frame (CTX-0313); `window.padding`/`opacity` wiring, premultiplied alpha,
  theme cursor hue, tall-glyph clipping, and session-less split present fixes.
- **Configuration matrix:** arrow-key default variants for HJKL actions
  (CTX-0262), Mod+M/HJKL pinned to both mods with a Mod-aware resize variant
  (CTX-0258), configurable leader/mod key, F-keys plus INS/DEL/HM/END/PU/PD as
  bindable chrome chords, per-window font zoom, which-key help popup,
  focus-follows-mouse, and knob-effect config tests (CTX-0295).
- **Plugin CLI (CTX-0150):** `bitty plugin` subcommands with capability gates;
  `required_services` manifest resolution (CTX-0277) and 0600 managed-manifest
  permissions (CTX-0293).
- **CarryCtx snapshot publication:** moved in-repo with the external mirror
  retired (CTX-0319); publish pipeline unified with provenance and `ctxpack`
  `format_version` 2 validation (CTX-0314/0317), export-time secret redaction,
  scratch/host-path lint gates, and `just workflow-import` mirror restore
  (CTX-0316).
- **In-repo module splits:** `bitty-app` `main.rs`/`ctl.rs`, IPC
  `devtools.rs`, runtime `registry.rs`, VT `parser.rs`, and term-state
  `state.rs` decomposed into focused modules (CTX-0305/0306/0307/0308/0309/
  0310); remaining hardcode-audit literals named and host identifiers
  sanitized (CTX-0299/0300/0301).
- **Platform and terminal correctness:** Windows ConPTY Tier-1 slice
  (CTX-0268), `terminal.scrollback` capacity and `terminal.shell` honored
  (CTX-0297/0298), dropped mouse validation restored (CTX-0303), narrow reflow
  tail rewrap (CTX-0266), and live split resize updating primary grid plus PTY
  winsize (CTX-0269).

### Release engineering

- Workspace version bumped `0.0.1 -> 0.0.20` (the first Cargo bump since the
  `0.0.1` leaf release; tags `v0.0.2`-`v0.0.19` never moved the in-repo
  version), applied to `Cargo.toml`, `Cargo.lock`, internal `path` dependency
  pins, root and `packaging/PKGBUILD*` `pkgver`, `nfpm.yaml`, and the
  `flake.nix` fallback. `scripts/check-release-version.sh` now keeps
  `bitty --version`, packaging metadata, and the release tag in agreement.

### Plugin host runtime: per-plugin VM lifecycle and activation (CTX-0328)

- Ratified `plugin-host-runtime-rfc` Gap A implemented across the policy,
  mechanism, orchestration, and application layers. `bitty-plugin-host` stays
  VM-free; `bitty-runtime` gains a `plugin_runtime` module owning one `!Send`
  `piccolo` VM per `(PluginId, generation)` on a single executor thread with
  the `Unloaded -> Loading -> Activating -> Active -> Suspended -> Disposing
-> Disposed` lifecycle.
- `bitty-lua` gains the VM seam: read-only `bitty` host-module injection,
  rooted source-only `require`, bounded depth/node/byte marshalling,
  generation-scoped registration capture, and budget-enforced callback
  invocation. The retained stdlib piccolo omits is installed (`utf8`,
  restricted `os.time/clock/date`, `string.byte/char/format`, `table.concat`,
  `table.sort`).
- Activation runs the fixed `init.lua`, validates the captured registrations
  against the manifest, and commits atomically; any failure purges the policy
  host generation (identity, command ownership, subscriptions) and records a
  terminal failure, so no partial activation survives and a retry starts clean.
  Minimal host services (`terminal.snapshot` sync bounded, `notify.show` async
  hand-off, `store.*` sync atomic quota-bounded, `settings.*` read-only) and
  typed `E_TIMEOUT`/`E_*` errors.
- `bitty-app` discovers plugin packages at startup and activates them; a new
  `--safe` flag creates no third-party VM. Safe-mode eligibility comes from the
  discovery root's provenance (`SourceClass`), never a self-declared manifest
  id, so a package cannot claim bundled trust by naming itself `bitty.*`.
  Bounds: 256 KiB manifest, 4096 module files, 16 MiB tree, 1024-byte path.
- Follow-ups: Gap B source staging/integrity (CTX-0329) and Gap C hardening
  (CTX-0330).

### `terminal.shell` honored at startup spawn (CTX-0298, issue #495)

- The effective `terminal.shell` value is now used when no explicit CLI
  program is given. Precedence: CLI program > configured `terminal.shell` >
  `$SHELL` > `/bin/sh`. The frozen startup spawn recipe (split panes,
  headless fallback) replays the same resolution.
- Direct `argv[0]` throughout: no shell interpolation, no word splitting.
  The project-layer trust gate that rejects `terminal.shell` is unchanged.
- Fail-closed: config validation rejects control characters in
  `terminal.shell`; the spawn resolver trims and skips blank, oversized, or
  control-laden values, and a configured shell that fails to spawn retries
  `/bin/sh` exactly like the existing `$SHELL` failure path.
- Tests: pure precedence/fail-closed unit coverage plus bounded live-spawn
  effect tests proving the configured shell executes as `argv[0]`.

### `bitty plugin` CLI-first management (CTX-0150, issue #244)

- New `bitty plugin list|install|remove|enable|disable|info` subcommands over
  the existing draft `bitty-plugin-host` machinery (static bundled manifests,
  closed capability grammar, hash binding). Local class: no instance, no IPC,
  no plugin VM, and no plugin code is executed.
- One machine-managed manifest at
  `$XDG_CONFIG_HOME/bitty/bitty-plugins.toml` (or beside an explicit
  `--config` path): a strict bounded format whose unknown keys, malformed
  values, and over-limit files fail closed. `remove` requires `--force` and
  keeps a `.bak` of the previous manifest; installs pin
  `PluginManifest::manifest_hash()` and `enable` re-checks that pin.
- Capability consent: the prompt lists every requested capability with its
  plain-language effect and high-risk marker; `--yes` approves
  non-interactively, the interactive `[y/N]` path fails closed on EOF or a
  decline, and an update that adds capabilities blocks until re-approved
  while narrowed/unchanged sets carry forward (P0-AC-030 pattern).
- `bitty-plugin-host`: new `CapabilityRequests::all_ids()` exposes the exact
  requested-capability expansion (flat ids plus `fs.read`/`fs.write`
  patterns) shared by host activation and the CLI consent surface.
- v1 installs bundled ids only (`bitty-terminal.*`); registry/Git/local-path
  sources remain deferred with the package manager and fail closed with a
  clear error. Tests: 16 unit (`plugin`) + 1 dispatch-parse unit + 6
  end-to-end binary cases, all headless.

### Kitty placement + rasterize + composite into present path (CTX-0248, issue #426)

- New `bitty-rich` `kitty_place` layer: `a=t`/`T`/unsupported action mapping, cursor-anchored cell rects, scroll-with-content, alt-screen clear, CTX-0247 decode caps reused, no allocation before validation.
- New `DrawList.images` present layer in `bitty-render`: topmost blits above fills and glyphs, never grid truth; software and headless CPU compositors blend them. Runtime tick overlay feeds placed images into the present path.
- Known limitation: the real-GPU (wgpu) path ignores `DrawList.images` until a texture-upload path lands (documented in `crates/bitty-render/src/gpu.rs`); software/headless paths composite.
- Tests: 21 rich + 5 render + 8 runtime present tests, all headless.

### `frameHash` lossless frame digest (CTX-0244, issue #420)

- New `bitty.debug/frameHash` IPC method: SHA-256 (std-only, no new
  dependency) over canonical `BFH1 || width_be32 || height_be32 ||
frameSeq_be64 || headless_rgba`, answering the CTX-0242 V1-V3 equality
  question in 32 bytes with zero pixel bytes on the wire.
- Same CTX-0188 consent minter, no new scope (`debug.trace` +
  `terminal.inspect`), new `AutomationFamily::FrameDigest` (never widens
  `Capture`), TTL cap 120 s (default minter refuses digest grants),
  2 digests/s rate ceiling, local-attested transport only (fail-closed
  `ScopeDenied` otherwise), `Unavailable` on empty surfaces (never a hash
  of nothing), granted AND denied calls audited in the existing bounded
  (64, drop-oldest) log with the served digest hex.
- Runtime publishes the presented headless frame only while a digest grant
  is live (zero clone cost otherwise) and only for headless presents.
- Digest tests run headless/in-process in CI; the live-grant ceremony test
  is `#[ignore]`-gated local-manual only. Raw pixel channel stays
  deferred indefinitely (no pixel bytes cross IPC under any grant).

### Panel live V1-V3 gates on `frameHash` equality (CTX-0242)

- New `crates/bitty-runtime/tests/panel_live_framehash.rs`: headless,
  deterministic, synthetic-only equality gates wiring V1-V3 to the CTX-0244
  digest (`frame_digest_hex`, the same function the server hashes with).
  V1 pins the gap-band paint contract (gapped digest differs from the
  no-gap baseline for identical content, fresh re-run reproduces it
  exactly, plus a direct gap-pixel == theme bg assertion); V2 pins
  focus-switch diff plus switch-back restore (primary follows focus,
  CTX-0234); V3 pins workspace-alias vs tabs-shim digest equality for
  identical content (layout parity consequence of the `tabs_compat` guard).
- New socket-level lossless proof in the same file (CTX-0188 harness
  pattern: real Unix socket, file-local serial guard, reply correlation):
  a real `Runtime` headless frame published via `publish_frame_rgba`
  digests identically through `bitty.debug/frameHash`, with zero pixel
  bytes on the wire. The live-grant ws4 ceremony (V1 gaps, V2 tab focus,
  V3 alias parity per focus step, expiry `ScopeDenied`, digest audit) is
  `#[ignore]`-gated local-manual only and never runs in CI.

### AUR bitty-bin prebuilt package (CTX-0138, issue #227)

- New `packaging/PKGBUILD.bin` template for AUR `bitty-bin`: installs the prebuilt `bitty-x86_64-unknown-linux-gnu` release binary to `/usr/bin/bitty` (no user-side compile), real sha256 filled by the release workflow (never `SKIP` for the binary), `arch=('x86_64')` only, `provides=('bitty')`, `conflicts=('bitty' 'bitty-nightly' 'bitty-git')`, plus `.desktop`/icons from the release source tarball.
- Release workflow gains an `aur-bin` publish job (same SSH host-key pinning and isolated `BUILD_DIR` pattern as the `aur` job; first push registers `bitty-bin` on AUR; gated on `vars.AUR_BIN_PUBLISH`).
- New `scripts/check-release-version.sh` keeps the Cargo workspace version aligned with release tags (plus `PKGBUILD*`/`nfpm.yaml` consistency), enforced in the release `validate` job; `scripts/check-pkgbuild-bin.sh` render-tests the template. No version bump in this change; the bump ships with the release that enables the publish job. Asset-name dependency on CTX-0164 (#264) noted in the template: the `bitty-<target>` dist name must stay stable across the `bitty-app` -> `bitty` inner rename.

### Rename binary artifact `bitty-app` -> `bitty` (CTX-0164, #264)

- Crate `bitty-app` keeps its name; only the binary artifact renames via
  `[[bin]] name = "bitty"` in `crates/bitty-app/Cargo.toml`, so
  `target/debug/bitty`, `target/release/bitty`, `ps`, and fastfetch show
  `bitty`.
- Packaging follows the artifact: root `PKGBUILD` + `packaging/PKGBUILD`
  install `target/release/bitty` to `/usr/bin/bitty`; `Formula/bitty.rb` +
  `homebrew/Formula/bitty.rb` install `target/release/bitty`; `nfpm.yaml`
  already used `src: ./target/release/bitty` (no change);
  `.github/workflows/release.yml` builds `bitty` directly and keeps
  conventional assets `dist/bitty-<target>` (`-x86_64-unknown-linux-gnu`,
  `-aarch64-apple-darwin`, `-x86_64-pc-windows-msvc.exe`, etc.) plus
  `.sha256`, `SHA256SUMS`, `provenance.json`, and `dist/bitty-<target>.deb`
  / `.rpm` / `.apk` / `.pkg.tar.zst`; Homebrew/Scoop jobs already fetch
  `bitty-<target>` assets and install as `bitty` (no URL change).
- AUR package name `bitty` unchanged. `packaging/bitty.desktop` `Exec=bitty`
  verified unchanged. No shell completions exist in-repo (no change).
- Docs updated where `bitty-app` was used as the command (`soak-0.0.1.md`
  headless/window paths, `perf-baseline.md` next step, `tools/perf/*`,
  `scripts/visual-smoke.sh`, `scripts/dogfood.sh` messages); `cargo -p
 bitty-app` crate selectors and historical `0.0.1` release records kept.

### Post-0.0.1 maintenance — triage 2026-09-01 (CTX-0117, docs-only)

- Triage 0.0.1: GitHub Issues 0 open (verified `gh issue list` 2026-09-01, no new bugs from 0.0.1); crates.io 9/9 at 0.0.1 verified (`cargo info bitty-*` all show 0.0.1, docs.rs 302 to `bitty_*`), GitHub Release `v0.0.1` prerelease 3 assets (`bitty-v0.0.1-linux-x64.tar.gz` 3.3 MiB `18f9ceeef4930f08cc825541a63f4e7024bf19ec3ea69ca9621be95407358838`, `SHA256SUMS`, `provenance.json`) downloadCount 0 each (expected immediately post-release, no user feedback yet), `recordings/compat-matrix-2026-09-01.json` 14 surfaces all self PASS.
- Fix: docs-only regression — README pre-implementation disclaimer clarified from “no published artifacts beyond dry-run” to note 9 crates at 0.0.1 on crates.io plus Linux x64 binary preview on GitHub Releases (binary `publish = false`, never on crates.io); no code, no version bump, no publish.
- Gates: `just check` PASS (fmt-check + clippy -D warnings + test + actionlint + markdownlint, 1394+ tests, 61 files), `cargo check --target x86_64-pc-windows-gnu` PASS, `cargo audit` PASS (1235 advisories, 2 allowed paste/ttf-parser `RUSTSEC-2024-0436`/`RUSTSEC-2026-0192`), `cargo deny check` PASS, no `cargo publish` (docs-only), no pin drift (toolchain `1.97.1`, MSRV `1.85`, edition `2024`, resolver `3`).
- Crates.io/docs.rs monitoring 2026-09-01: 9 published crates remain indexed, docs.rs redirects 302 for `vt`/`term-state`/`render` verify built docs; release feedback deferred to weekly patrol (next check `cargo audit`/`cargo info`).

- Post-0.0.1 development continues on `main` (tail crates `plugin-host`, `rich`, `ipc`, `agent`, `runtime`, `app`, `core` deferred to `0.1.0`).

## [0.0.1] - 2026-09-01

### Formal leaf release (Groups 1-3, 9 crates at 0.0.1)

Published in DAG order with index propagation waits and crates.io rate-limit handling (5 new crates per ~10 min window; G1 5 + lua hit 429 at 14:46:23 retried 14:46:39, G3 hit 429 until 15:06:23).

**Group 1 — Leaves (no workspace deps):** `bitty-vt` 0.0.1 (vt 0.15, 24 files 118.4 KiB 27.7 compressed), `bitty-pty` 0.0.1 (portable-pty 0.9, 14 files 75.1 KiB), `bitty-platform` 0.0.1 (winit 0.30 + raw-window-handle 0.6.2, 17 files 187.6 KiB), `bitty-config` 0.0.1 (13 files 120.7 KiB), `bitty-package` 0.0.1 (18 files 269.7 KiB), `bitty-lua` 0.0.1 (piccolo 0.3.3, 6 files 60.8 KiB).

**Group 2 — Terminal Truth:** `bitty-term-state` 0.0.1 (depends on `bitty-vt = "0.0.1"`, 25 files 225.8 KiB).

**Group 3 — Presentation branch (parallel after Group 2):** `bitty-ui` 0.0.1 (depends on `term-state`, 12 files 158.9 KiB), `bitty-render` 0.0.1 (depends on `term-state` + `platform`, 19 files 329.9 KiB, wgpu 26.0 + crossfont 0.9).

Seven tail crates remain `publish = false` at `0.0.1` (deferring `0.1.0`): `plugin-host`, `rich`, `ipc`, `agent`, `runtime`, `app`, `core`.

Toolchain pinned `1.97.1`, MSRV `1.85`, edition `2024`, resolver `3`, `just check` PASS (fmt-clippy-test-actionlint-markdownlint), `cargo check --target x86_64-pc-windows-gnu` PASS, `act -n` DRYRUN PASS, cargo audit/deny checked via CI.

Fix: `bitty-term-state` dev-dep `bitty-ui` version pin removed (path-only) to break cycle `term-state dev-> ui -> term-state` that blocked Group 2 publish after vt indexed; tests remain headless bounded (61+4+3+4+8).

### Binary preview — `bitty-app` 0.0.1 (publish = false, GitHub Releases only)

`bitty-app` never on crates.io. This tag publishes a Linux x86_64 preview: `bitty-v0.0.1-linux-x64.tar.gz` (bitty-app 11 MiB + LICENSE + README + CHANGELOG, 3.3 MiB compressed) + `SHA256SUMS` + `provenance.json` (commit, toolchain 1.97.1, Cargo.lock hash, target `x86_64-unknown-linux-gnu`). Build `cargo build -p bitty-app --release --locked`, verified `--headless` smoke (fills 1921 glyphs 21, layout-proof split/stack/overlay distinct deterministic rgba).

Future nightly `nightly-YYYYMMDD+sha` will reuse this shape with matrix linux-x64 / macos-arm64 / windows-x64, retention 14, gated on `just check` + supply-chain + Windows.
