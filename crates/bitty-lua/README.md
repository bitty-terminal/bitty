# `bitty-lua`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-lua` wraps the pure-Rust `phodopus` VM (sole third-party dependency
per `Cargo.toml`, exact git revision `f6aa59a16c0dd8d3f65f54043f8c3c2fe02b5344`)
to give Bitty deterministic, bounded Lua execution for per-plugin isolation
and, per DEC-0011, user configuration evaluation (`src/lib.rs`,
`src/config.rs`). Each VM instance gets isolated globals with a restricted
standard library (`src/stdlib.rs`), host work happens only through
capability-checked callbacks (`src/host.rs`), and instruction, wall-clock,
and heap budgets fail closed into suspension (`src/lib.rs`: `drive_chunk`
and `drive_stashed`, sliced at `SLICE_FUEL = 1024`). Budget mechanics and
the determinism contract are documented in `src/lib.rs`.

## Boundaries

- Sole third-party dependency, per `Cargo.toml`: `phodopus` pinned at exact
  git revision `f6aa59a16c0dd8d3f65f54043f8c3c2fe02b5344`; no
  network-facing dependency is declared.
- Standard library baseline at `phodopus@f6aa59a`: in-core `utf8` and
  `string.format` with proportional fuel charging, plus retained bounded
  `string.byte`/`char`, `table.concat`/`sort`, and restricted
  `os.time`/`clock`/`date` (`src/stdlib.rs`); no ambient `io` authority,
  traceback-only `debug`, text-only `load`, and empty `package.path` with no
  native loader (pinned by `tests/readiness_mirror.rs`:
  `stdlib_allowlist_denies_ambient_authority`).
- Plugin VMs build only through the fail-closed gate (`src/gate.rs`:
  `PluginVmBuilder`/`build_plugin_vm` require explicit RC-1/RC-2 budgets);
  the raw constructors are deprecated and kept for the single internal
  gate path (pinned by `tests/load_gate.rs`).
- Configuration chunks run under the same budgets as plugins and must return
  plain-data tables; the typed schema in `bitty-config` stays the validation
  authority (see `src/config.rs`).
- There is no `mlua` dependency anywhere, so no VM type conflict can arise
  (`src/lib.rs` documents the `phodopus::Lua` boundary).

## Lua surface: presentation overlay (issue #2849)

```lua
local handle, err = bitty.ui.mount("overlay", scene)   -- needs ui.rich + ui.overlay
local shown, err = bitty.ui.present.show(handle)        -- needs ui.present
local hidden, err = bitty.ui.present.hide(handle)       -- idempotent, ungated
```

The surface is non-modal and paint-only: it draws above the focused view,
never receives key or pointer events, never takes focus, and never changes the
focus ring. Core owns geometry, clipping, stacking, the frame budget, and
teardown; the plugin owns only the scene.

| Item         | Contract                                                                                                                                                                                                                                                                                                 |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `show`       | Argument: an integer block handle this generation mounted into the `overlay` slot. Returns `true` on success and `false` only where documented by the host.                                                                                                                                              |
| `hide`       | Argument: the same handle. Idempotent: `false` means "nothing of mine was live", never an error. Cleanup stays ungated so a revoked grant can still remove the surface.                                                                                                                                  |
| Grants       | `ui.present` alone authorizes drawing. `ui.rich` + `ui.overlay` are needed to mount the block; `ui.overlay.focus` is required only for the focusable capture overlay and is neither implied by nor implied from `ui.present`.                                                                            |
| Admission    | One surface per window per generation; a second plugin's `show` fails closed with `E_UI_UNAVAILABLE`. Never sets the modal authority.                                                                                                                                                                    |
| Budget       | Bounded like a chrome band: at most 64 rows and 16 KiB of row text per frame, plus the v1 scene budgets enforced at mount. The trailing rows are dropped and the drop is reported, never silently painted.                                                                                               |
| Focus-follow | The surface tracks the focused view. A focus switch re-centers it; closing the view it was drawn over removes it.                                                                                                                                                                                        |
| Dismissal    | `Esc` dismisses it without consuming the press (a real confirmation gate keeps its consume). The owner observes one `overlay.dismissed` event with `{ owner, handle, reason }` where `reason` is `"escape"` or `"view-closed"`; delivery is queued to the cold tick, never a callback on the input path. |
| Errors       | `E_CAPABILITY_DENIED` (missing `ui.present`), `E_UI_UNAVAILABLE` (safe mode, no host surface, or a second plugin's admission), `E_UI_NOT_OWNER` (a handle this generation did not mount into the `overlay` slot), `E_UI_COMPONENT_INVALID` (a non-integer handle), `E_TIMEOUT` (call deadline).          |
| Lifecycle    | Suspend, unload, disable, crash, rollback, and generation change drop the surface and release its admitted slot; a stale generation's next present paints nothing.                                                                                                                                       |
| Safe mode    | No plugin VM exists, so no surface exists. A generation that reaches the host under safe policy is refused with `E_UI_UNAVAILABLE`.                                                                                                                                                                      |

`bitty.ui.overlay.show` and `bitty.ui.overlay.hide` remain as the CTX-1431
spelling of the same session; they are gated by `ui.present` as well, so
neither spelling can gain input authority. The focusable capture surface
(`bitty.ui.overlay.acquire`, `update`, `poll`, `release`) and its
`E_UI_ALREADY_CAPTURED` single-owner contract are unchanged.

## Lua surface: `bitty.ui.unmount` (issue #3106)

```lua
local handle, err = bitty.ui.mount("bottom", scene)  -- needs ui.rich
local released = bitty.ui.unmount(handle)            -- needs ui.rich
```

Plugin API v1 mounted a block with no way to release it: the block, its
generation-owned handle, and its retained-string bytes stayed allocated until
the generation was torn down (suspend, reload, dispose). A plugin that opens and
closes a view therefore had to reuse one fixed set of handles for its whole
generation, because a mount per open would exhaust the 64-block registry after
eight cycles. `bitty.ui.unmount` is the release primitive.

| Item     | Contract                                                                                                                                                                                                                                                             |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Argument | An integer handle this generation received from `bitty.ui.mount`: a v1 band/overlay block or a `terminal`-slot rich scene. A non-integer is `E_UI_COMPONENT_INVALID`.                                                                                                |
| Result   | `true` when a handle this generation held was released; `false` for a stale, foreign, or unknown handle — the `ui.update` contract, so a cleanup path never has to tell "already gone" apart from an error.                                                          |
| Grant    | `ui.rich`, the same grant `mount`/`update` need. Without it the call is refused with `E_CAPABILITY_DENIED` and the block stays mounted.                                                                                                                              |
| Release  | A band/overlay block leaves the registry and its retained-string bytes are refunded to the 2 MiB aggregate; a `terminal`-slot scene is detached and its Core-owned panel retired. The handle is never re-minted, so `update`/`unmount` on it keep reporting `false`. |
| Capture  | A live focusable-overlay capture handle reports `false` and changes nothing: that surface is owned by `bitty.ui.overlay.release`, which also ends the input-capture session, so releasing the block alone would orphan a capture.                                    |
| Teardown | Unchanged: suspend, unload, disable, crash, and reload still drop the whole registry. `unmount` is the per-block release, not a replacement for generation teardown.                                                                                                 |

The caps are unchanged (64 blocks and 2 MiB of retained strings per plugin
generation); `unmount` makes them reclaimable inside a generation instead of
only at teardown.

## Lua surface: `bitty.ui.state` (issue #3107)

```lua
local handle, err = bitty.ui.mount("bottom", scene)  -- needs ui.rich
local state = bitty.ui.state(handle)                 -- { presented = bool, focused = bool } or nil
```

A plugin that mounts a block and schedules periodic refreshes (`bitty.timers.create`)
needs a way to tell whether its block is currently presented and whether its
host surface holds focus, so it can pause or skip refresh work while hidden or
unfocused (for example on `focus.changed`). `bitty.ui.state(handle)` queries the
mounted block's live state under the same `ui.rich` grant as `mount`, `update`,
and `unmount`.

| Item     | Contract                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Argument | An integer handle this generation received from `bitty.ui.mount`. A non-integer fails closed with `E_UI_COMPONENT_INVALID`.                                                                                                                                                                                                                                                                                                                                                                                          |
| Result   | `{ presented = boolean, focused = boolean }` for a live handle owned by this generation; `nil` for a stale, unmounted, foreign, or unknown handle (matching the `ui.update`/`ui.unmount` stale-handle contract).                                                                                                                                                                                                                                                                                                     |
| Grant    | `ui.rich`, the same grant `mount`/`update`/`unmount` need. Without it the call fails closed with `E_CAPABILITY_DENIED`.                                                                                                                                                                                                                                                                                                                                                                                              |
| Slots    | `top`/`bottom`/`tabline`: `presented` is `true` when the band is non-empty (reserves its row and paints), and `focused` is `presented and window_focused`. `overlay`: `presented` is `true` while the block owns capture or the presentation overlay session, and `focused` is `true` only while it owns capture and the window is focused. `terminal`: `presented` is `true` while the rich scene is attached, and `focused` is `true` when the scene's panel view is the focused target and the window is focused. |

## Lua surface: `process.spawn` working directory (CTX-1715)

```lua
local result = bitty.process.spawn(
  { "status", "--porcelain" },
  { cwd = "pane" }  -- optional; "pane" is the only accepted value
)
```

`cwd` selects a **semantic**, never a path. `"pane"` asks the host to run the
child in the directory of the **globally focused view at call time** (there is
no per-plugin pane binding), resolved from that pane's **PTY child's kernel
working directory** — the same live model behind `bitty.terminal.snapshot`,
read as `/proc/<pid>/cwd` on Linux and pinned to a directory identity. The host
validates the resolved directory (absolute, at most 4096 bytes, no NUL or
control bytes, canonicalized, an existing directory) and records the resolved
canonical path in the plugin spawn audit entry.

A plugin cannot **supply** that path, and a pane program cannot **steer** it.
The pane's `OSC 7` report is untrusted terminal output — any program running in
a pane, and displayed content that emits `OSC 7`, can set it to any existing
local path — so the resolver never reads it (issue #2911). The child's
directory comes from the kernel instead: the source is re-validated against
the pinned inode at the spawn boundary, so a directory swapped or deleted
between observation and spawn is a refusal, not a redirect. Where the platform
cannot prove a child's cwd (macOS, the BSDs, Windows), the `pane` semantic is
refused rather than resolved from the report.

| Item                      | Contract                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cwd`                     | Optional string. `"pane"` is the only accepted value. An absolute or relative path, `~`, `..`, a symlink target, an empty string, and a wrong-case token are all `E_DEF_INVALID` before the host call.                                                                                                                                                                                         |
| Unknown options           | `E_DEF_INVALID`; no option is ever silently ignored. A table mixing `cwd = "pane"` with an unknown key is refused regardless of key order.                                                                                                                                                                                                                                                     |
| `E_SPAWN_CWD_UNAVAILABLE` | The host has no pane binding, the focused view has no terminal session, the focused pane has no provable PTY-child working directory (no child, a reaped child, an unreadable or deleted kernel cwd, or a platform without a kernel cwd query), or the directory changed identity before the spawn. The spawn never falls back to the host process cwd and never to the pane's `OSC 7` report. |
| Unchanged refusals        | The `-C`, `--git-dir`, and `--work-tree` argv denials and the `[tools.git]` executable allowlist are unchanged; they close argv-selected directories, not repository metadata (see below).                                                                                                                                                                                                     |
| Not a filesystem grant    | The pane directory is a **starting directory**, not an `fs` grant. `process.spawn:git` is itself a repository-read capability: the child reads the pane tree with the user's OS authority, not through the plugin's `fs` grants. The plugin API's reads still follow `fs.read`/`fs.write`, and the inherited exec confinement is unchanged.                                                    |
| Reload-free               | The source is read per call, so changing the pane directory changes the result without reloading the plugin.                                                                                                                                                                                                                                                                                   |
| Unchanged calls           | Omitting the option keeps the host default (no child cwd), so existing calls stay valid.                                                                                                                                                                                                                                                                                                       |

**Repository metadata is a separate channel.** The `-C`, `--git-dir`, and
`--work-tree` denials close argv-selected directories, not repository
selection: a resolved pane tree can carry `.git`/`core.worktree` metadata
that points `git` at another repository, and a plugin able to write the pane
tree could plant it (latent today, because `fs.write` is not implemented).
`.git` policy and an inode-bound cwd are tracked follow-ups.

## Lua surface: profile lifecycle (CTX-1749)

```lua
local p = bitty.profiles.preview_create("work", { from = "default" })
local created, err = bitty.profiles.create("work", { from = "default", digest = p.digest })
```

`bitty.profiles` is also mounted as `bitty.config.profiles`; both names are the
same read-only table. It lists profiles and drives four lifecycle pairs:
create, clone, rename, and delete. Every mutation is two calls:
`preview_<verb>` renders the exact bounded diff and issues a one-shot approval
digest, and `<verb>` applies that digest. The digest binds the plugin ID, the
verb, the target name(s), the options, and the full candidate
preimage/postimage, so a preview cannot authorize a different profile, a
different option set, or different bytes. A digest is accepted either as the
trailing argument or as `opts.digest` (both must match when both are given);
without one, the apply call fails closed with `E_CONFIG_APPROVAL_REQUIRED`, and
a consumed, expired, or wrong-target token fails with the same code (a digest
that is not lowercase SHA-256 hex is `E_CONFIG_APPROVAL_MISMATCH`). All calls
require the recorded `config.manage` grant.

| Verb             | Arguments                    | Result                                                                                                                                                         |
| ---------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list`           | none                         | `rows` and the identical `profiles` array (`{ name, active }`), `active`, `count`, `truncated`.                                                                |
| `preview_create` | `name[, opts]`               | `digest`, `rendered`, `changed`, `changed_keys`, `replaces_existing`, `selector_updated = false`, `active`.                                                    |
| `create`         | `name[, opts][, digest]`     | `name`, `created`, `changed`, `replaced`, `backed_up`, `extends`, `preset`, `reload = "none"`.                                                                 |
| `preview_clone`  | `src, dst[, opts]`           | Preview shape above (`selector_updated = false`).                                                                                                              |
| `clone`          | `src, dst[, opts][, digest]` | `source`, `name`, `target`, `cloned`, `changed`, `replaced`, `backed_up`, `extends`, `reload = "none"`.                                                        |
| `preview_rename` | `old, new[, opts]`           | Preview shape above; `selector_updated` says whether the persisted selector will be rewritten.                                                                 |
| `rename`         | `old, new[, opts][, digest]` | `old_name`, `new_name`, `name`, `renamed`, `changed`, `replaced`, `backed_up`, `selector_updated`, `reload` (`"auto"` when the selector moved, else `"none"`). |
| `preview_delete` | `name[, opts]`               | Preview shape above; `active` flags the currently persisted profile.                                                                                           |
| `delete`         | `name[, opts][, digest]`     | `name`, `deleted`, `changed`, `backed_up`, `was_active`, `reload = "none"`.                                                                                    |

Options are a closed table: an unknown key, a non-boolean `force`, or a
non-table argument is `E_PROFILE_USAGE`, never ignored. `preview_create`
accepts `{ force?, from?, preset? }` and `create` accepts the same table plus
`digest?`. `from` names an existing profile whose bytes are copied verbatim
(the new body therefore keeps the source's own `extends`, and the outcome
reports that recorded base); `preset` is one of `minimal`, `dark`, or `light`;
and `from` and `preset` are mutually exclusive. `preview_clone`,
`preview_rename`, and `preview_delete` accept `{ force? }`; their apply verbs
accept `{ force?, digest? }`.

The pre-existing `list`, `preview_use`, and `use` verbs keep their contracts:
`preview_use` returns `digest`, `rendered`, `keys`, and `changed`, and `use`
returns `active`, `changed`, `reload = "auto"`, and `pending_restart`.

`force` semantics:

| Verb     | Without `force`                                                         | With `force`                                                                                                                                                         |
| -------- | ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create` | an existing destination is `E_PROFILE_CONFLICT`.                        | the destination is backed up as `<name>.lua.bak-<epoch>` and replaced.                                                                                               |
| `clone`  | an existing destination is `E_PROFILE_CONFLICT`.                        | same backup-then-replace behaviour.                                                                                                                                  |
| `rename` | an existing destination is `E_PROFILE_CONFLICT`.                        | the destination is backed up and replaced; the source is then renamed and the persisted selector rewritten when it named the source (the selector is backed up too). |
| `delete` | the active profile and the reserved `default` are `E_PROFILE_CONFLICT`. | the active profile and/or `default.lua` may be removed; a timestamped backup is always kept.                                                                         |

Independently of `force`, a profile that another profile extends cannot be
deleted or renamed (`E_PROFILE_CONFLICT` naming the dependents), and `default`
cannot be renamed. Names are `[a-z0-9][a-z0-9_-]*`, 1..=64 bytes, with no path
separators; refusals are path-free (no host path is ever echoed).

**Deleting the active profile.** `delete(name, { force = true })` on the active
profile is allowed and intentionally leaves the persisted selector dangling:
the next launch fails closed with "profile not found". This mirrors
`bitty config profile delete --force`. The preview always reports
`active = true` for an active target so a confirmation surface can require an
explicit acknowledgement; a plugin that wants to avoid the dangling state must
`preview_use`/`use` another profile first. The same rule applies to
`default.lua`, which is only the implicit base profile when no profile is
selected.

| Code                         | Meaning                                                                                                                                         |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `E_CAPABILITY_DENIED`        | the generation holds no recorded `config.manage` grant.                                                                                         |
| `E_CONFIG_UNAVAILABLE`       | no writable profile backend is installed.                                                                                                       |
| `E_CONFIG_APPROVAL_REQUIRED` | missing, replayed, expired, or wrong-target digest (apply calls).                                                                               |
| `E_CONFIG_APPROVAL_MISMATCH` | a digest is not a lowercase SHA-256 hex string, or the trailing and embedded digests disagree.                                                  |
| `E_CONFIG_STALE`             | the candidate bytes moved between preview and apply.                                                                                            |
| `E_PROFILE_USAGE`            | malformed arguments: non-table options, unknown option key, wrong value type, or `from` combined with `preset`.                                 |
| `E_PROFILE_INVALID`          | invalid profile name, unknown preset, profile body that fails validation, unrewritable selector, or a selector update that cannot be validated. |
| `E_PROFILE_CONFLICT`         | reserved `default` where it is refused, an existing target without `force`, active-profile delete without `force`, or an `extends` dependent.   |
| `E_PROFILE_NOT_FOUND`        | the source or target profile does not exist.                                                                                                    |
| `E_PROFILE_IO`               | a filesystem operation failed; the previous bytes are preserved where a rollback was possible.                                                  |
| `E_PROFILE_INTERNAL`         | a rollback or restore invariant failed; the payload names the operation, never a path.                                                          |
| `E_DEF_INVALID`              | wrong arity or a non-string name argument.                                                                                                      |
| `E_DEF_LIMIT` / `E_TIMEOUT`  | the `config.manage` call/byte budget or the authorization deadline was exceeded.                                                                |

## Lua surface: Wheel management (`bitty.wheel` / `bitty.config.wheel`, CTX-1817)

`bitty.wheel` (also mounted at `bitty.config.wheel`) exposes bounded read/write management of `$XDG_DATA_HOME/wheel/{settings.json,credentials.json,approvals.json}` to plugins holding the `config.manage` grant:

- `get()` returns `{ default_provider, default_model, thinking_enabled, thinking_gear, approval_mode, providers, permissions, approvals }` with boolean/source-only credential attribution (`env` > `global_config` > `data_credentials` > `none`).
- `preview(key, value)` / `set(key, value, digest)` mutate `settings.json` (`default_provider`, `default_model`, `thinking_enabled`, `thinking_gear`, `approval_mode`, `permissions.<tool>`, `providers.<name>.model`).
- `preview_auth_set(provider, api_key)` / `auth_set(provider, api_key, digest)` and `preview_auth_remove(provider)` / `auth_remove(provider, digest)` mutate `credentials.json` (`0700`/`0600`) with `"***REDACTED***"` previews and SHA-256 secret-digest binding.
- `preview_resolve_approval(id, decision)` / `resolve_approval(id, decision, digest)` resolve pending entries in `approvals.json` (`allow_once`, `allow_session`, `deny`).

## Evidence

- `tests/readiness_mirror.rs` (RUN-29) mirrors the Phodopus executable gate
  (`phodopus@f6aa59a`, `crates/phodopus/tests/readiness_gate.rs`) at the
  bitty seam: RC-1 instruction/wall budgets, RC-2 quota refusal, FS-1
  denial atomicity, FS-3 fault containment, FS-5 recovery, FS-9 no-bypass,
  the stdlib allowlist, module isolation, and host-op cancellation as typed
  `E_TIMEOUT`; every test fails if the mapped behavior regresses and no
  test asserts on wall-clock durations.
- `tests/measurement_lua.rs` proves the RC-1/RC-2 enforcement curves and
  the `VmBudgetSnapshot` counter methodology; `tests/host_bridge.rs` proves
  the bridge deadline/timeout paths; `tests/load_gate.rs` proves the
  fail-closed build gate and stable `E_*` budget codes; `tests/safe_mode.rs`
  proves `--safe` admits zero third-party surface.

## Layout

- `Cargo.toml` — package metadata and the `phodopus` dependency.
- `src/lib.rs` — crate docs with the role, budgets, and determinism sections.
- `src/host.rs` — VM lifecycle, budget enforcement, and host calls.
- `src/ui.rs` — Plugin API v1 declarative UI scenes (`bitty.ui.mount` /
  `bitty.ui.update` / `bitty.ui.unmount` / `bitty.ui.state`), the closed v1
  `UiSlot` enum, and the bounded `UiNode` model.
- `src/keymaps.rs` — bounded keymap/action registry page for
  `bitty.keymaps.list()` (issue #2405): canonical rows, 256-row pages, the
  1024-row addressable cap, and 64 KiB whole-row tail truncation.
- `src/config.rs` — configuration-chunk evaluation and table extraction.
- `src/stdlib.rs` — restricted standard-library construction.
- `src/gate.rs` — fail-closed plugin-VM build gate and safe-mode policy.
- `src/store.rs` — host-owned RC-11 persistent-store quota backend
  (`bitty.store`; pinned by `tests/store_quota.rs`).
- `src/error.rs` — stable `E_*` bridge codes for VM budget failures.
- `tests/` — headless budget and isolation tests, including the
  `readiness_mirror.rs` gate mirror.
