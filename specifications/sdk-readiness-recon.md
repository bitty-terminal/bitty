---
title: SDK Readiness Recon for bitty-plugin-sdk, bitty-plugin-template, and Lua Core Parts
description: Recon-only readiness assessment recording which SDK surface can be frozen now, what is still churning, and the recommended build sequencing (CTX-0705)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# SDK Readiness Recon (CTX-0705)

> Status: **draft recon record** (survey only, not accepted). This document
> records evidence revisions, separates freezable surface from churning
> surface, and proposes candidate SDK/template layouts plus build sequencing.
> It authorizes no implementation, freezes no API, adds no Core API, and
> weakens no security control. Lifecycle: this record is bitty-side candidate
> text; canonicalization into `bitty-plugins-docs` and any open-question
> register update are follow-up tasks in their owning repositories and are
> explicitly out of scope here.
>
> Task: `CTX-0705` | Primary RFCs: `OQ-011`, `OQ-056`, `OQ-058`, `RFC-OQ-3`

## Claim-status legend

Every statement below carries one label. Unlabeled statements are candidate
proposals from this record.

- `[accepted]` — ratified by a merged RFC/ADR cited inline.
- `[shipped]` — merged implementation evidence cited inline.
- `[surveyed]` — read-only inspection during this recon (paths + revisions
  cited, no behavior executed).
- `[candidate]` — this record's proposal, not ratified anywhere.

## Evidence revisions (all `[surveyed]`)

| Artifact | Revision inspected |
| -------- | ------------------ |
| `bitty` worktree `ctx-0705/sdk-recon` | `50ae226` (= `origin/main` at recon time) |
| `bitty-plugin-host` sources | `crates/bitty-plugin-host/src/{lib,grant,manifest,registry,host,event,capability}.rs` at `50ae226` |
| `bitty-lua` bridge | `crates/bitty-lua/src/host.rs` at `50ae226` (imports `phodopus::`, not `mlua`/`piccolo`) |
| `bitty-runtime` provider contract | `crates/bitty-runtime/src/registry/provider.rs` (CW-23, `OQ-058`) at `50ae226` |
| `bitty-ui` placement / provider / status | `crates/bitty-ui/src/{placement,provider,status_registry}.rs` at `50ae226` |
| `bitty-plugins` checkout | `bitty-plugins/` with `sdk/bitty-plugin-sdk` and `template/bitty-plugin-template` nested checkouts; sample plugins `activity`, `palette`, `statusline`, `file-manager`, `git-panel` |
| `bitty-plugin-sdk` | `lua/bitty.d.lua` (R-SDK-1), `surface/bitty-plugin-api-v1.json`, `src/{mock-host,manifest,conformance}.ts` |
| `bitty-plugin-template` | `template/{bitty-plugin.toml,lua/@@PLUGIN_MODULE@@/init.lua,package.json,justfile}` |
| Open-question register | `bitty-docs` `docs/decisions/open-questions.md` (OQ statuses quoted below) |
| Lua surface contract | `bitty-plugins-docs` `sdk/plugin-api-v1-lua-surface-rfc.md` (`accepted` 2026-09-11, ADR 0009) |

## 1. What can be frozen NOW vs what is still churning

### 1.1 Freezable NOW (accepted contracts with shipped or Implemented evidence)

These surfaces are `[accepted]` and safe for the SDK to generate from. Per
LUA-OQ-1 authority placement, the SDK consumes the accepted text and invents
no identifiers.

| Surface | Contract status | Code evidence (`[surveyed]` at `50ae226`) | SDK implication |
| ------- | --------------- | ------------------------------------------ | --------------- |
| Manifest schema + limits (`PluginId`, `Compat`, `MAX_COMMANDS = 128`, `MAX_EVENT_TYPES = 256`, tool allowlist `git`-only) | `[accepted]` Plugin Platform RFC (OQ-012) | `manifest.rs` total validation, headless | Freeze: manifest JSON schema + `bitty-plugin-lint` rules (R-SDK-2) |
| Capability grammar (closed, deny-by-default, no wildcards, `effect_statement`) | `[accepted]` Plugin Platform RFC (OQ-012) | `capability.rs` `CapabilityFamily`/`CapabilityId` | Freeze: capability catalog + `env:<KEY>`/`platform.notify` gating tables |
| Grant lifecycle (hash binding, `GrantConsent` explicit-consent type, `apply_update` fail-closed on additions, narrowing) | `[accepted]` Plugin Platform RFC (OQ-012) | `grant.rs` `GrantRecord`/`GrantStore` in-memory stubs (persistence deferred) | Freeze: grant-diff approval UX contract; do NOT freeze storage paths (still stubs) |
| Registry + generations (`Declared→…→Disposed`, duplicate qualified-name rejection, monotonic `Generation`) | `[accepted]` Plugin Platform RFC (OQ-011) | `registry.rs` | Freeze: qualified-name rule `<plugin-id>:<resource>`, lifecycle names |
| Event pipeline (3 classes, closed 17-kind v1 set, per-subscriber FIFO 64, `DropOldest` default, 4 interception points veto-only) | `[accepted]` Plugin Platform RFC (OQ-013) | `event.rs` `EventKind::parse`/`as_str` round-trip | Freeze: event-name strings + payload shapes (see §2.3) |
| Lua v1 function spellings (`bitty.commands.register`, `bitty.events.subscribe`, `bitty.ui.mount/update`, `bitty.terminal.snapshot`, `bitty.store.*`, `bitty.settings.*`, `bitty.notify.show`, `bitty.services.get/provide`, `bitty.keymaps.suggest`, `bitty.tasks.*`, `bitty.timers.*`, `bitty.env.get/has`) | `[accepted]` Lua Surface RFC + ADR 0009 (OQ-011) | `bitty.d.lua` + `bitty-plugin-api-v1.json` generated from ADR 0009 text | Freeze: `bitty.d.lua` typings + surface JSON (already generated; keep generation one-way from accepted text) |
| VM budgets (RC-1 `10^7` instr / 50 ms / 8 ms warn; RC-2 32 MiB per plugin; three-level queue 64/1024+256 KiB/8192+2 MiB) | `[accepted]` Isolation Resource RFC (OQ-014) | `bitty-lua` `RC1_*`/`RC2_*` consts; measurement tests cited in register | Freeze: budget numbers in SDK conformance tests (timeout/quota expectations) |
| Bridge mechanics (read-only `bitty` root, source-rooted `require`, `RegistrationCapture` admission bounds 128/256/64, sync non-blocking `HostServices`) | `[accepted]` Host Runtime RFC + ADR 0010 (OQ-033, OQ-035); first slice `[shipped]` in `bitty` PR #554 | `host.rs` 9 namespaces wired (see §1.3 gap) | Freeze: `init.lua` activation shape (registrations valid during activation only), `require` rules, error classes |
| Plugin authoring shape | `[accepted]` by example + template | `activity/lua/activity/init.lua` follows the RFC (bounded timers, `bitty.store` aggregates, no ambient authority); template `@@PLUGIN_MODULE@@` layout | Freeze: template file layout (see §2.2) |

### 1.2 Still churning — DO NOT freeze (all Open, no implementation claim)

| Item | Status | Why it blocks SDK surface |
| ---- | ------ | ------------------------- |
| Provider contract (OQ-058) | Open: role panels, IPC event kinds/routing, panel/session lifecycle coupling, envelope semantics (identity, attribution, deadline, priority, dedup, cancellation, expiry) | No `register_panel`/`PanelId`/panel-lifecycle Lua spelling exists; the RFC exclusion list (§"Not in v1" item 6) defers it. In-tree `PanelProvider` (`bitty-runtime/registry/provider.rs`, CW-23) is `[candidate]` scaffolding, capability-gated but not host-wired |
| Placement (RFC-OQ-3) | Candidate direction recorded (refined Option C, `PanelId` visible identity / `View` hidden attachment, transitional `ViewContent::Panel` encoding); awaits RFC amendment | SDK must not expose `PanelId`/`ViewId` mapping or focus-target helpers until accepted; `bitty-ui` `placement.rs` types are Core-owned presentation state, not plugin API |
| Event bus beyond v1 (OQ-056) | Open: semantic UI slots, presentation projection, workspace policies, automation action classes, service multiplicity, API version assignment | `bitty.ui.mount` stays declarative slot content only; `status.component` composition (`status_registry.rs`, CW-24) and `LayoutProvider` (`provider.rs`, CW-07) are Core-side registries with grant-flag carriage, not Lua-callable v1 surface |
| Reload/update triggers + queue drain (OQ-072) | Open: trigger surface (`bitty plugin reload`, IPC, watcher contract), generation-N queue drain at disposal | Template must document manual-reload-only workflow; SDK conformance must not assert watcher/debounce behavior |
| Panel lease/handoff (OQ-083, refines OQ-058) | Open: no lease/description/roaming/handoff mechanism | Out of SDK scope entirely until OQ-058 lands |
| Streaming components (OQ-082) | Open: no damage-budget/lifecycle contract at 30–60 Hz | SDK must keep the "no hot-path events" exclusion frozen; no streaming helper surface |
| Grant persistence path | Deferred behind in-memory stubs (`grant.rs`: no file I/O yet; state-directory + CLI revocation surface outstanding) | SDK mock host may simulate grants in memory; must not promise on-disk paths or cross-version grant migration |

### 1.3 Parity gap found during recon (bitty-side, not SDK-side)

`[surveyed]` The `bitty-lua` bridge at `50ae226` wires 9 `bitty.*`
namespaces (`commands.register`, `events.subscribe`, `settings.get`,
`store.get/set`, `terminal.snapshot`, `notify.show`, `process.spawn`,
`timers.create/cancel`, `ui.mount/update`) but has **no `keymaps`,
`services`, `tasks`, or `env` tables**, although all four are `[accepted]`
v1 surface (Lua Surface RFC; `bitty.env.get/has` already fixed by ADR 0006).
The SDK `bitty.d.lua` declares all four. Direction of the gap: SDK typings
run ahead of the host bridge. Recommended follow-up (out of scope here):
bitty-side task to wire the four namespaces or record an explicit
deferral with per-namespace `E_NOT_IMPLEMENTED` diagnostics; SDK
conformance should mark those four namespaces `pending-host` until then so
`mock-host` parity tests do not assert behavior the host cannot perform.

## 2. Proposed layouts (all `[candidate]`)

### 2.1 SDK crate layout (`bitty-plugin-sdk`)

Keep the SDK TypeScript-first (it ships `bitty-plugin-lint`, mock host,
and codegen). No new top-level package; extend the existing tree:

```text
bitty-plugin-sdk/
  lua/
    bitty.d.lua            # R-SDK-1: GENERATED from accepted text (ADR 0009 +
                           #   surface JSON). Header records source revisions.
                           #   Never hand-edit spellings.
    examples/              # one minimal init.lua per L1/L2 area (commands,
                           #   events, store, ui.mount, snapshot)
  surface/
    bitty-plugin-api-v1.json   # machine source of truth for codegen; add
                           #   "hostParity": "wired|pending-host" per namespace
                           #   (§1.3) so conformance can skip pending-host.
  src/
    manifest.ts / schema.ts / json-schema.ts   # R-SDK-2 lint (frozen §1.1 row 1)
    capabilities.ts / path-pattern.ts          # closed-grammar tables
    mock-host.ts           # in-memory HostServices double; grants in memory
                           #   only (§1.2 last row); pending-host namespaces
                           #   throw typed E_NOT_IMPLEMENTED
    conformance.ts         # parity suite against surface JSON; per-namespace
                           #   skip on pending-host
    host-surface.ts / host-diagnostics.ts / diagnostics.ts / version-range.ts
  conformance/             # plugin-facing vectors (event payloads, budget
                           #   expectations, error classes)
```

Rules: one-way generation (accepted text → surface JSON → `bitty.d.lua` +
docs); the SDK never invents identifiers (LUA-OQ-1); `api_version`
`"1.0.0"` stays until a `bitty-docs` revision bumps it.

### 2.2 Template layout (`bitty-plugin-template`)

Freeze the existing `template/` shape; changes are additive only:

```text
template/
  bitty-plugin.toml        # id owner.name, compat bitty + plugin-api ranges,
                           #   deny-by-default [capabilities], [lazy] triggers
  lua/@@PLUGIN_MODULE@@/
    init.lua               # activation-only registrations; owns nothing past
                           #   its generation; manual-reload-only note (OQ-072)
    <module>.lua           # pure logic + bounded constants (retry caps, table
                           #   bounds) beside init.lua, as activity/ demonstrates
  package.json / bun.lock  # pin bitty-plugin-sdk commit for just manifest
  justfile                 # just manifest (R-SDK-2 gate), test, lint
  README.md                # capability justifications (why each grant is needed)
```

### 2.3 Lua core API surface (`[candidate]` consolidation — no new spelling)

The freezable core is exactly the accepted v1 set; this table consolidates
(does not extend) the Lua Surface RFC for SDK codegen scoping:

| Namespace | Functions | Capability gate |
| --------- | --------- | --------------- |
| `bitty.commands` | `register(def)` | none (core-registered) |
| `bitty.events` | `subscribe(name, handler)` — 17 closed names, envelope `{kind, sequence, payload}` | none; payloads bounded |
| `bitty.keymaps` | `suggest(def)` — suggestion only, `(when, chord)` identity, `when = "global"` in v1 | none |
| `bitty.settings` | `get(key)`, `set(key, value)` — plugin-owned namespace | none |
| `bitty.store` | `get(key)`, `set(key, value)` (`nil` deletes); 256 KiB total / 8 KiB value / depth 8 / 1024 nodes | none; quota errors typed |
| `bitty.notify` | `show(payload)` | `platform.notify` |
| `bitty.env` | `get(name)`, `has(name)` — `^[A-Z_][A-Z0-9_]*$`, desensitized | `env:<KEY>`; ungranted fails closed |
| `bitty.ui` | `mount(slot, component)`, `update(handle, component)` — nodes `Text/Row/Column/List` only, `block_id` versioning | `ui.rich`; `ui.overlay` for overlay slot |
| `bitty.terminal` | `snapshot(opts)` (`scope = "semantic"` default), read-only | `terminal.semantic-read` |
| `bitty.services` | `get(iface, opts)`, `provide(iface, impl)` — bounded interface schema in manifest | provider grants stay with callee |
| `bitty.tasks` | `spawn(fn)`, `cancel(task_id)` — 64 live tasks/plugin | RC-4 caps |
| `bitty.timers` | `create(delay_ms, cb)`, `cancel(timer_id)` | RC-4 caps |

Explicitly excluded (frozen exclusions, re-affirmed): Terminal Truth
writes, raw/input authority, hot-path events, L3 presentation, L4 protocol
registration, panel providers, browser/agent/MCP/AI surfaces, interception
rewriting, ambient filesystem/process/network/clipboard services, aliases,
Rust internals.

## 3. Recommended sequencing (all `[candidate]`)

Build only after the corresponding core surface converges; each step is a
separately scoped task in the owning repository.

1. **Close the §1.3 parity gap first (bitty-side).** Wire `keymaps`,
   `services`, `tasks`, `env` into the `bitty-lua` bridge (or record explicit
   deferrals). Nothing else in the SDK can claim v1-complete before this.
   Depends on: accepted text already present (no RFC wait).
2. **Freeze SDK generation pipeline.** One-way codegen (accepted text →
   `surface/bitty-plugin-api-v1.json` → `bitty.d.lua`), `hostParity` flags,
   `mock-host` pending-host diagnostics. Depends on: step 1.
3. **Freeze template + R-SDK-2 lint.** Template additive polish, `just
   manifest` gate pinned to an SDK commit, per-capability README
   justifications. Depends on: steps 1–2. OQ-072 stays manual-reload-only.
4. **Conformance vectors from shipped evidence.** Event-payload, budget, and
   error-class vectors checked against `bitty` parity tests (not invented).
   Depends on: steps 1–2 plus bitty CTX-0330 hardening landing.
5. **After OQ-058 + RFC-OQ-3 acceptance:** panel-provider Lua spelling,
   placement-safe helpers, `PanelId` identity rules — as API v1.x or v2
   addition via `bitty-docs` revision, never SDK-invented. After OQ-056:
   semantic slots / status-component composition / service multiplicity.
   After OQ-072: watcher-contract template workflow + queue-drain conformance.

## Open points

- Whether the four pending-host namespaces (§1.3) land together or
  incrementally determines whether SDK v1 conformance ships whole or gated
  per namespace.
- `bitty-plugin-sdk` pins `bitty-docs` ADR 0009 text at
  `e94d86ef6bcf4865a84119828756952e50f10266` (surface JSON `sources`);
  SDK tasks need a revision-refresh policy for accepted-text updates.
- Canonicalization of this record into `bitty-plugins-docs` (SDK-side
  proposal doc) is a follow-up in that repository.

## Acceptance criteria

- [ ] §1.1/§1.2 classification matches the open-question register (no Open
  item listed as freezable, no accepted contract listed as churning).
- [ ] §1.3 gap confirmed against `host.rs` at the cited revision (or
  corrected by the reviewer with a pointer).
- [ ] Layouts in §2 add no new Lua spelling beyond the accepted v1 set.
- [ ] `markdownlint-cli2` clean on this file.

## References

- Plugin Platform RFC (`accepted` 2026-08-27; OQ-011/012/013).
- Lua Surface RFC + ADR 0009 (`accepted` 2026-09-11; OQ-011 Lua spellings).
- Lua Runtime RFC + ADR 0005/0006/0007 (OQ-009/030/031/032); ADR 0012
  phodopus successor direction.
- Isolation Resource RFC (`accepted` 2026-08-28; OQ-014).
- Host Runtime RFC + ADR 0010 (`accepted` 2026-09-11; OQ-033/035; first
  slice `bitty` PR #554).
- Open-question register: OQ-056, OQ-058, OQ-072, OQ-082, OQ-083 Open.
- In-tree candidates: `bitty-runtime/src/registry/provider.rs` (CW-23),
  `bitty-ui/src/{placement (RFC-OQ-3), provider (CW-07), status_registry
  (CW-24)}.rs`, `specifications/ui-slot-inventory.md` (CTX-0429, OQ-056
  design record).
