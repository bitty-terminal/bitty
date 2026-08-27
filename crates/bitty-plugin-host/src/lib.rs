//! `bitty-plugin-host`: draft plugin-platform host for Bitty.
//!
//! # Draft status — not normative
//!
//! This crate implements the **proposed** contracts from
//! `bitty-docs/docs/specifications/plugin-platform-rfc.md`.
//! That RFC is still `Proposed` (frontmatter `draft`) and closes
//! `OQ-011`, `OQ-012`, and `OQ-013` only if it is adopted after independent
//! review by the category owner, a docs curator, and a security reviewer.
//! Nothing here claims normative behavior, stable file formats, frozen
//! capability identifiers, or a settled event-pipeline policy. The crate is
//! intentionally `draft` / `proposed` and its contract **may change** without
//! a semver major bump until the RFC is accepted. Do not describe its
//! behavior as shipped until an ADR records acceptance and a release ships it.
//!
//! The RFC's Lua runtime dependency (`lua-runtime-rfc`, `OQ-009`) is also still
//! proposed, so this crate is **pure data + validation** on the host side: it
//! owns the plugin registry, manifest validation, capability grammar, grant
//! lifecycle stubs, and the bounded event pipeline plus the bounded side queue
//! per `ADR-0003` rule 4. There is no Lua VM coupling, no file I/O, no platform
//! window/GPU coupling, and no `unsafe` — the crate is headlessly testable on
//! both Linux CI and the `windows-latest` job.
//!
//! # Pipeline (candidate)
//!
//! ```text
//! bitty-plugin.toml --parse/validate--> PluginManifest --registry--> (generation)
//!                                    --capability--> GrantStore (manifest-hash binding)
//!                                    --event--> EventPipeline (per-subscriber bounded queues)
//!                                    --side queue--> HostObservation (cold, after state update)
//! ```
//!
//! - Manifest discovery and validation happen before any plugin code runs.
//!   Both the package manager and the host validate the same schema and version.
//! - The capability grammar is deny-by-default, closed, no wildcards, and
//!   unknown identifiers fail validation instead of being ignored.
//! - Grants persist as hash-bound records; added capabilities block automatic
//!   update pending diff approval, narrowed sets carry forward silently.
//! - The event pipeline keeps one bounded FIFO queue per `(plugin, event-type)`,
//!   supports coalescing where semantics allow, bounds batch size/time, and
//!   treats overflow via the single shared open decision point.
//! - The side queue that observes terminal events is strictly bounded and never
//!   blocks the producer (ADR-0003 rule 4, threat `T-01`).
//!
//! # RFC section mapping
//!
//! | RFC section | Module(s) | Key items |
//! |-------------|-----------|-----------|
//! | Manifest and identity (OQ-012, part 1) | `manifest` | [`manifest::PluginManifest`] + [`manifest::PluginId`] + [`manifest::QualifiedName`] + hard limits (256 KiB, 128 commands, 256 events, 32 patterns/kind, 16 services, 8 deps, 8 KiB pattern text) |
//! | Identity and compatibility | `manifest` | [`manifest::PluginId`] qualified `owner.name`, [`manifest::Compat`] semver ranges, duplicate detection |
//! | Identifier grammar and families (OQ-012, part 2) | `capability` | [`capability::CapabilityId`] closed grammar `family.resource[.scope][:PARAM]`, deny-by-default, no wildcards, high-risk flag, [`capability::effect_statement`] |
//! | Grant lifecycle | `grant` | [`grant::GrantRecord`] hash binding, [`grant::GrantStore`] revoke/re-grant/deny-loop prevention, workspace narrowing (`apply_workspace_narrowing` rejects additions) |
//! | Plugin API v1 surface (OQ-011) — commands, services, settings | `registry` | [`registry::Registry`] qualified names (`plugin-id:resource`), duplicate rejection at graph construction, service interface syntax, lazy triggers |
//! | Lifecycle and generations | `registry`, `host` | [`registry::PluginState`] `Declared->Resolved->Registered->Activated->(Suspended)->Disposed`, [`registry::Generation`] monotonic, generation disposal completeness, safe-mode skip |
//! | Event pipeline — classes and phases (OQ-013) | `event` | [`event::EventClass`] Lifecycle/Observation/Interception, [`event::EventKind`] v1 closed set (4 interception points exactly) |
//! | Delivery, ordering, batching, and coalescing | `event` | [`event::EventQueue`] per-subscriber bounded FIFO, coalescing for title/cwd/focus/selection, [`event::DEFAULT_BATCH_EVENTS`]/[`event::DEFAULT_BATCH_BYTES`] (`<=32` / `8 KiB` proposed), [`event::DropPolicy`] open decision point |
//! | Timeouts and failure policy | `event` | [`event::InterceptionDecision`] veto-wins, fail-open, [`event::should_proceed`], reentrancy rejected, interception not queued (cold-path synchronous) |
//! | Plugin host (ADRs) | `host` | [`host::PluginHost`] owns registry + grant store + event pipeline + [`host::SideQueue`] bounded side queue; no window/GPU/PTY coupling; headless testable |
//! | Security alignment | all | No bypass, no ambient authority, presentation never rewrites terminal truth, high-risk identifiers distinct, `bitty --safe` skips third-party plugins |
//! | Open points remaining under OQ-011..OQ-014 | docs + `event::DropPolicy` | `DropPolicy` open point documented; exact queue depths/timeouts remain `OQ-014` candidates |
//!
//! # Drop policy — open decision point
//!
//! Queue overflow when a queue is full is a single shared **open decision
//! point** owned by `OQ-013` and the RFC section “Delivery, ordering, batching,
//! and coalescing” (point 3). Two candidate policies remain proposed:
//!
//! - `DropOldest`: newest signals survive, consumers converge on current state,
//!   but early burst history is lost.
//! - `DropNewest`: already-queued events keep FIFO delivery, but newest signals
//!   starve under sustained flood.
//!
//! This crate exposes both via [`event::DropPolicy`] and requires callers to
//! choose explicitly (no implicit default). Numeric queue depths and timeout
//! milliseconds are `OQ-014` (proposed defaults in this crate are `64` per-queue,
//! `32`/`8 KiB` per batch as candidate values, not normative). See
//! `plugin-platform-rfc.md` § “Delivery, ordering, batching, and coalescing”
//! for the authoritative trade-off statement; other documents must reference
//! that point instead of asserting a settled policy.
//!
//! # Ownership rules (ADR-0003 / ADR-0004)
//!
//! - **Depends on:** `bitty-term-state` and `bitty-config` only (path deps per
//!   the ADR crate graph). No other workspace crate is depended upon.
//! - **No third-party dependencies** (pure `std`). `mlua` seam deferred; `toml`
//!   parsing stays outside this crate's pure-data core (caller supplies an
//!   already-parsed [`manifest::PluginManifest`] or raw bytes length).
//! - **Never holds** GPU objects, window handles, PTY file descriptors, or
//!   internal Rust hot-path objects. It observes terminal events only through
//!   the bounded side queue ([`host::SideQueue`]) and through the public
//!   `Snapshot` surface where needed (never grid internals).
//! - **`#![forbid(unsafe_code)]`** at crate and workspace level; `MSRV 1.85`,
//!   `edition = "2024"`.
//! - All structures are owned (`String`, `Vec`, `BTreeMap` …), never `&str` —
//!   so manifests, grant records, and events are cloneable, comparable, and
//!   sendable without lifetimes.
//! - `bitty-plugin-host` is `publish = false` at the workspace level today;
//!   publication will track RFC acceptance.

#![forbid(unsafe_code)]

pub mod capability;
pub mod error;
pub mod event;
pub mod grant;
pub mod host;
pub mod manifest;
pub mod registry;

pub use capability::{CapabilityFamily, CapabilityId, effect_statement};
pub use error::{ErrorClass, PluginError};
pub use event::{
    DEFAULT_BATCH_BYTES, DEFAULT_BATCH_EVENTS, DEFAULT_QUEUE_CAPACITY, DropPolicy, Event,
    EventClass, EventKind, EventPayload, EventPipeline, EventQueue, InterceptionDecision,
    accumulate_interceptions, should_proceed,
};
pub use grant::{GrantOrigin, GrantRecord, GrantStore, RevokeReport};
pub use host::{HostObservation, PluginHost, SideQueue};
pub use manifest::{
    CapabilityRequests, Compat, FilesystemRequest, FsAccess, LazyTriggers, MANIFEST_MAX_BYTES,
    MAX_COMMANDS, MAX_DEPENDENCIES, MAX_EVENT_TYPES, MAX_FS_PATTERNS_PER_KIND,
    MAX_PATTERN_TEXT_BYTES, MAX_PROVIDED_SERVICES, PluginId, PluginIdentity, PluginManifest,
    QualifiedName,
};
pub use registry::{Generation, PluginState, Registry, RegistryEntry};
