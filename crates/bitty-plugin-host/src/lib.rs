//! `bitty-plugin-host`: accepted plugin-platform host for Bitty.
//!
//! # Status — accepted contracts, implementation not yet verified
//!
//! This crate implements the accepted contracts from
//! `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/specifications/plugin-platform-rfc.md`.
//! That RFC is `accepted` (frontmatter `status: accepted` 2026-08-27) and closed
//! `OQ-011`, `OQ-012`, and `OQ-013` (bitty-docs open-questions register) per independent
//! review by the category owner, a docs curator, and a security reviewer.
//! Nothing here claims normative behavior beyond the accepted contract, stable file formats, frozen
//! capability identifiers, or a settled event-pipeline policy beyond what the RFC accepts. The implementation
//! is `Implemented`, not yet `Verified`, and carries no compatibility promise beyond the accepted contract.
//! Do not describe its behavior as shipped until a release ships it.
//!
//! The RFC's Lua runtime dependency (`lua-runtime-rfc`, `OQ-009`) is also still
//! proposed, so this crate is **pure data + validation** on the host side: it
//! owns the plugin registry, manifest validation, capability grammar, grant
//! lifecycle stubs, and the bounded event pipeline plus the bounded side queue
//! per `ADR-0003` rule 4. There is no Lua VM coupling, no file I/O, no platform
//! window/GPU coupling, and no `unsafe` — the crate is headlessly testable on
//! both Linux CI and the `windows-latest` job.
//!
//! The `install` module ([`install::verify_install`]) wires the **proposed**
//! `package-lifecycle` RFC (draft) into the install path: it calls
//! `bitty_package::verify_pipeline` (7 stages) before any staging, checks
//! capability-diff `P0-AC-030`, trust `V-A`/`V-B`/`V-C`, and generation
//! integrity, fail-closed with owned errors for `bitty plugin doctor`. The
//! package RFC itself is still `Proposed`; this wiring is a draft seam and
//! may change without a semver major bump until acceptance.
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
//!   treats overflow via the single shared policy (`DropOldest` accepted v1
//!   default, OQ-013 closed).
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
//! | Grant lifecycle | `grant` | [`grant::GrantRecord`] hash binding, [`grant::GrantStore`] revoke/re-grant/deny-loop prevention (per-capability denials, CTX-0465), update block ([`grant::GrantStore::apply_update`] fails closed on additions without approval, R-016/P0-AC-030), workspace narrowing (`apply_workspace_narrowing` rejects additions) |
//! | Plugin API v1 surface (OQ-011) — commands, services, settings | `registry` | [`registry::Registry`] qualified names (`plugin-id:resource`), duplicate rejection at graph construction, service interface syntax, lazy triggers |
//! | Lifecycle and generations | `registry`, `host` | [`registry::PluginState`] `Declared->Resolved->Registered->Activated->(Suspended)->Disposed`, [`registry::Generation`] monotonic, generation disposal completeness, safe-mode skip |
//! | Event pipeline — classes and phases (OQ-013) | `event` | [`event::EventClass`] Lifecycle/Observation/Interception, [`event::EventKind`] v1 closed set (4 interception points exactly) |
//! | Delivery, ordering, batching, and coalescing | `event` | [`event::EventQueue`] per-subscriber bounded FIFO, coalescing for title/cwd/focus/selection, [`event::DEFAULT_BATCH_EVENTS`]/[`event::DEFAULT_BATCH_BYTES`] (`<=32` / `8 KiB` accepted v1 baseline), [`event::DropPolicy`] `DropOldest` accepted default |
//! | Timeouts and failure policy | `event` | [`event::InterceptionDecision`] veto-wins, fail-closed timeouts (CTX-0465), [`event::should_proceed`], reentrancy rejected, interception not queued (cold-path synchronous) |
//! | Plugin host (ADRs) | `host` | [`host::PluginHost`] owns registry + grant store + event pipeline + [`host::SideQueue`] bounded side queue; no window/GPU/PTY coupling; headless testable |
//! | Package install verification (proposed, draft) | `install` | [`install::verify_install`] calls `bitty_package::verify_pipeline` (7 stages) before staging; `V-A`/`V-B`/`V-C` trust, capability-diff `P0-AC-030`, generation integrity `verify_all`; fail-closed owned errors + [`install::DoctorIssue`] for `bitty plugin doctor`; headless tamper/capability tests |
//! | Security alignment | all | No bypass, no ambient authority, presentation never rewrites terminal truth, high-risk identifiers distinct, `bitty --safe` skips third-party plugins |
//! | Unknown-origin restrictive policy (R-020, P0-AC-032) | `origin` | [`origin::DetectedOrigin`] advisory classification (fail-closed to `Unknown` on absent/conflicting signals), [`origin::OriginPolicy`] `Unknown`/`Remote` restrictive, relaxation only via explicit [`origin::OriginOverride::RelaxToStandard`] |
//! | Verification remaining under closed OQ-011..OQ-014 | docs + `event::DropPolicy` | `DropOldest` accepted v1 default; exact queue depths/timeouts per accepted `OQ-014` budgets; remaining work is implementation verification, not open RFC points |
//!
//! # Adopted security directions with live-boundary seams (OQ-054/055/057/084/085)
//!
//! The following modules record the adopted owner directions for the
//! security open questions. Each is pure, bounded, fail-closed data with unit
//! tests; none grants authority by itself. Live-boundary seams: per-key
//! `env.read:<KEY>` grants gate `bitty.env` reads (fail-closed, typed
//! `E_NOT_IMPLEMENTED` until granted), and the secret-tier consent gate
//! ([`secret_tiers::check_tier_access`]) runs at the secret resolve boundary
//! ([`host::PluginHost::resolve_secret_with_tier`]) with names-only audit;
//! trust-level admission gates effective authorization
//! ([`effective::authorize_with_trust`]), the role contract gates it too
//! ([`effective::authorize_with_role`]), and the credential exclusive-or
//! order is enforced by [`credential_ref::resolve_choice`] at the call
//! boundary where two references meet, with actual resolution on the
//! provider-schema surface ([`credential_ref::ProviderCredentialConfig`]).
//!
//! | Open question | Module | Adopted shape |
//! |---------------|--------|-----------------|
//! | OQ-085 trust levels + capability domains | `trust_levels` | [`trust_levels::TrustLevel`] levels 0–4, [`trust_levels::CapabilityDomain`] admission sets narrowing with level, mapping onto accepted [`capability::CapabilityFamily`] only; [`trust_levels::TrustLevel::check_family`] enforced by [`effective::authorize_with_trust`] |
//! | OQ-084 ontology/identity | `identity` | [`identity::EntityKind`] ten first-class kinds, [`identity::OntologyId`] `kind:value` identifiers, [`identity::Ownership`] links plus [`identity::Lifetime`] |
//! | OQ-055 secret-storage tiers | `secret_tiers` | [`secret_tiers::SecretTier`] four tiers with per-tier [`secret_tiers::TierPolicy`] (consent/audit/redaction); [`secret_tiers::CommandRef`] resolved by [`secret_tiers::execute_command_ref`] (shell-free, bounded); [`secret_tiers::KeyringBackend`] selection plus [`secret_tiers::RotationPolicy`]; [`secret_tiers::check_tier_access`] enforced at the resolve boundary |
//! | OQ-054 `api_key_env` vs `api_key_cmd` | `credential_ref` | [`credential_ref::CredentialRef`] env/cmd references (names only), [`credential_ref::resolve_precedence`] exclusive-or order plus [`credential_ref::resolve_choice`] call-boundary enforcement, [`credential_ref::check_project_override`] narrow-only boundary, [`credential_ref::ProviderCredentialConfig`] schema surface with [`credential_ref::resolve_provider_credential`] |
//! | OQ-057 role contract | `roles` | [`roles::AgentRole`] Commander/Implementer/Tester/Reviewer, [`roles::EnforcementPoint`] checks (incl. [`roles::EnforcementPoint::for_request_kind`]), per-role dispatch fan-out/depth ceilings ([`roles::AgentRole::check_dispatch`]) enforced by [`effective::delegate_with_role`], prompt-text denial ([`roles::deny_prompt_authority`]), [`roles::SandboxDecl`] posture checked against [`roles::SandboxRestrictions`] ([`roles::AgentRole::check_sandbox_exec`]), capability ceilings intersected with grants elsewhere; [`roles::AgentRole::check_request`] enforced by [`effective::authorize_with_role`] |
//!
//! # Drop policy — DropOldest accepted default for v1 (OQ-013 closed decision point)
//!
//! Queue overflow when a queue is full was a single shared decision
//! point owned by `OQ-013` and the RFC section “Delivery, ordering, batching,
//! and coalescing” (point 3). That point is **closed for v1: `DropOldest` is
//! the accepted default** per the accepted Plugin Platform RFC (2026-08-27,
//! frontmatter `status: accepted`; bitty-docs open-questions register).
//! `DropNewest` remains available via explicit construction
//! ([`event::DropPolicy::DropNewest`]) but is not the v1 default:
//!
//! - `DropOldest` (accepted v1 default): newest signals survive, consumers converge on current state,
//!   but early burst history is lost.
//! - `DropNewest` (explicit opt-in): already-queued events keep FIFO delivery, but newest signals
//!   starve under sustained flood.
//!
//! This crate exposes both via [`event::DropPolicy`]; `DropOldest` is the
//! accepted v1 default used by [`event::DEFAULT_QUEUE_CAPACITY`] /
//! `DEFAULT_PLUGIN_DROP_POLICY` and `bitty-runtime::Runtime::new` per the accepted
//! Plugin Platform RFC (2026-08-27; bitty-docs open-questions register). Numeric queue
//! depths and timeout milliseconds follow `OQ-014` closed by the accepted Isolation Resource RFC
//! on 2026-08-28 (frontmatter `status: accepted`); this crate uses `64`
//! per-queue, `32`/`8 KiB` per batch as the accepted v1 baseline. See
//! the accepted `plugin-platform-rfc.md` § “Delivery, ordering, batching, and coalescing”
//! for the authoritative trade-off statement.
//!
//! # Ownership rules (ADR-0003 / ADR-0004)
//!
//! - **Depends on:** `bitty-term-state`, `bitty-config`, and `bitty-package`
//!   (draft package lifecycle) only (path deps per the ADR crate graph). No other
//!   workspace crate is depended upon.
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

pub mod bundled;
pub mod capability;
pub mod credential_ref;
pub mod effective;
pub mod error;
pub mod event;
pub mod fs_authz;
pub mod grant;
pub mod host;
pub mod identity;
pub mod install;
pub mod lifecycle;
pub mod manifest;
pub mod origin;
pub mod registry;
pub mod roles;
pub mod secret_tiers;
pub mod secrets;
pub mod tools;
pub mod trust_levels;

pub use capability::{
    CapabilityFamily, CapabilityId, effect_statement, validate_closed_capability,
};
pub use credential_ref::{
    CredentialPrecedence, CredentialRef, CredentialSource, MAX_CREDENTIAL_CMD_ARGS,
    MAX_CREDENTIAL_CMD_OUTPUT_BYTES, MAX_CREDENTIAL_CMD_PART_BYTES, MAX_CREDENTIAL_ENV_NAME_BYTES,
    ProviderCredentialConfig, check_project_override, check_provider_override,
    execute_credential_cmd, resolve_choice, resolve_precedence, resolve_provider_credential,
    resolve_provider_credential_live,
};
pub use effective::{
    AgentRequest, AuditDecision, AuditEntry, AuditLedger, CapabilityScope, DenialKind, DenialStep,
    EFFECTIVE_AUDIT_BOUND_NOTE, ENFORCEMENT_MAP, EffectiveCapability, EffectiveDenial,
    EffectiveLayer, EffectiveStack, EnforcementClass, EnforcementEntry, HOST_DEFAULT_MAX_AGENTS,
    MAX_AUDIT_ENTRIES, MAX_DENIAL_ITEMS, MAX_POLICY_FILE_BYTES, MAX_POLICY_FILE_LINES,
    MAX_POLICY_LINE_BYTES, MAX_RAW_DECLARATION_BYTES, MAX_RAW_DECLARATIONS, MAX_SCOPE_CAPS,
    PROJECT_POLICY_DIR_NAME, PROJECT_POLICY_FILE_NAME, PolicyProvenance, RequestKind,
    USER_POLICY_FILE_NAME, authorize, authorize_with_role, authorize_with_trust,
    authorize_with_trust_and_role, delegate, enforcement_class_for, parse_policy,
    project_policy_path, user_policy_path_with_env,
};
pub use error::{ErrorClass, PluginError};
pub use event::{
    BATCH_MAX_BYTES, BATCH_MAX_EVENTS, BoundedText, BudgetSnapshot, DEFAULT_BATCH_BYTES,
    DEFAULT_BATCH_EVENTS, DEFAULT_QUEUE_CAPACITY, DropPolicy, EVENT_MAX_BYTES, Event, EventClass,
    EventKind, EventPayload, EventPipeline, EventQueue, GLOBAL_QUEUED_BYTES_LIMIT,
    GLOBAL_QUEUED_EVENT_LIMIT, InterceptionDecision, PER_PLUGIN_QUEUED_BYTES_LIMIT,
    PER_PLUGIN_QUEUED_EVENT_LIMIT, PER_SUBSCRIPTION_QUEUE_LIMIT, RC1_INSTRUCTION_BUDGET,
    RC1_WALL_CLOCK_BUDGET_MS, RC1_WARNING_MS, RC2_MEMORY_AGGREGATE_BYTES,
    RC2_MEMORY_PER_PLUGIN_BYTES, RC6_FD_PER_PLUGIN, accumulate_interceptions, should_proceed,
};
pub use fs_authz::{
    FilesystemScope, FsAuditDecision, FsAuditEntry, FsAuditLedger, FsAuthorized, FsConsent,
    FsDecision, FsDenialKind, FsError, MAX_FS_AUDIT_ENTRIES, MAX_FS_AUDIT_ITEMS, MAX_FS_CONSENTS,
    MAX_FS_CONTENT_LINE_BYTES, MAX_FS_CONTENT_SCAN_BYTES, MAX_FS_CONTENT_SCAN_LINES,
    MAX_FS_PATH_BYTES, SensitivePathPolicy, authorize_fs, content_looks_secret,
    is_literal_scope_pattern,
};
pub use grant::{
    GRANTS_FILE_NAME, GRANTS_STATE_VERSION, GrantConsent, GrantOrigin, GrantRecord, GrantStore,
    MAX_GRANT_HASH_CHARS, MAX_GRANTS_CAPS, MAX_GRANTS_FILE_BYTES, MAX_GRANTS_LINES,
    MAX_GRANTS_PLUGINS, RevokeReport, grants_path_for,
};
pub use host::{HostObservation, PluginHost, SideQueue};
pub use identity::{EntityKind, Lifetime, MAX_ONTOLOGY_ID_BYTES, OntologyId, Ownership};
pub use install::{
    DoctorIssue, InstallInputs, NATIVE_ARTIFACT_EXTENSIONS, is_native_artifact_file_name,
    is_staging_allowed, reject_native_artifact_files, verify_install, verify_install_with_files,
};
pub use lifecycle::{
    BudgetDimension, Clock, ESCALATION_WINDOW_SECS, ESCALATIONS_TO_SUSPEND, EnforcementAction,
    EnforcementRecord, LifecycleEnforcer, MAX_ENFORCEMENT_RECORDS, MAX_TRACKED_OWNERS, ManualClock,
    PluginLifecycleStatus, ReloadOutcome, ReloadReport, ReloadResources, SystemClock,
    reload_generation,
};
pub use manifest::{
    CMD_SCHEMA_MAX_BYTES, CMD_SCHEMA_MAX_DEPTH, CapabilityRequests, Compat, FilesystemRequest,
    FsAccess, LazyCommand, LazyTriggers, MANIFEST_MAX_BYTES, MAX_COMMANDS, MAX_DEPENDENCIES,
    MAX_EVENT_TYPES, MAX_FS_PATTERNS_PER_KIND, MAX_PATTERN_TEXT_BYTES, MAX_PROVIDED_SERVICES,
    MAX_TOOLS, PluginDependency, PluginId, PluginIdentity, PluginManifest, ProvidedService,
    QualifiedName, SchemaSummary, ToolDeclaration, is_hostile_fs_pattern,
    summarize_interface_schema, validate_interface_schema,
};
pub use origin::{
    DetectedOrigin, OriginOverride, OriginPolicy, OriginSignals, classify_origin,
    resolve_origin_policy,
};
pub use registry::{Generation, PluginState, Registry, RegistryEntry, check_command_equivalence};
pub use roles::{
    AgentRole, EnforcementPoint, MAX_DELEGATION_DEPTH, MAX_DISPATCH_FANOUT, MAX_ROLE_LABEL_BYTES,
    SandboxDecl, SandboxRestrictions, deny_prompt_authority,
};
pub use secret_tiers::{
    CommandRef as SecretCommandRef, ConsentRule, DEFAULT_ROTATION_MAX_AGE_SECS, KeyringBackend,
    KeyringRef, MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_REF_ARGS, MAX_COMMAND_REF_PART_BYTES,
    MAX_KEYRING_ACCOUNT_BYTES, MAX_KEYRING_BACKEND_LABEL_BYTES, MAX_KEYRING_SERVICE_BYTES,
    MAX_ROTATION_MAX_AGE_SECS, MAX_TIER_LABEL_BYTES, RotationPolicy, SecretTier, TierAccess,
    TierPolicy, check_tier_access, execute_command_ref, read_keyring_ref, rotate_secret_value,
    select_keyring_backend, select_keyring_backend_with,
};
pub use secrets::{
    FileSecretStore, MAX_HANDLE_NAME_BYTES, MAX_RESOLVED_ENV_VARS, MAX_SECRET_AUDIT_ENTRIES,
    MAX_SECRET_AUDIT_ITEMS, MAX_SECRET_FILE_BYTES, MAX_SECRET_FILE_LINE_BYTES,
    MAX_SECRET_FILE_LINES, MAX_SECRET_VALUE_BYTES, MAX_SECRETS, SECRET_REDACTED_MARKER,
    SECRET_SCHEME_PREFIX, SECRET_STORE_DIR_NAME, SECRET_STORE_FILE_NAME, SanitizedEnvView,
    SecretAuditDecision, SecretAuditEntry, SecretAuditLedger, SecretConsent, SecretDenialKind,
    SecretDescriptor, SecretError, SecretHandle, SecretStore, data_home_with_env,
    is_sensitive_env_name, looks_like_literal_secret, looks_like_secret_token,
    reject_literal_secret, reject_literal_secrets_in_env, scrub_against_store,
    scrub_text_with_secrets, secret_store_path, secret_store_path_with_env,
};
pub use tools::{
    ACCEPTED_TOOL_GIT, DENIED_SPAWN_ENV_PREFIXES, DENIED_SPAWN_ENV_VARS, GIT_ALLOWED_SUBCOMMANDS,
    MAX_GIT_ARG_BYTES, MAX_GIT_ARGS, MAX_GIT_TOTAL_BYTES, PAYLOAD_MAX_BYTES, is_accepted_tool,
    is_allowed_git_args, is_safe_spawn_env, is_tool_spawn_allowed, is_valid_tool_name,
};
pub use trust_levels::{CapabilityDomain, MAX_TRUST_LABEL_BYTES, TrustLevel};
