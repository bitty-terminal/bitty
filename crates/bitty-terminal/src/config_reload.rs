//! Live configuration reload (CTX-0814, issue #1397).
//!
//! `bitty-config` owns the *policy* of a reload: [`ReloadClass`] and
//! [`bitty_config::diff`] classify a field diff as live / restart-required /
//! rejected. This module is the *activation path* that policy feeds — it holds
//! the last-applied [`EffectiveConfig`], re-reads the sources, and pushes the
//! live presentation subset into the running [`bitty_runtime::Runtime`] through
//! the runtime's live-adopt setters.
//!
//! Two entry points share one engine:
//!
//! * [`reload_requested`] serves the `bitty ctl` verb `bitty.debug/reloadConfig`
//!   (an explicit client request), and
//! * [`poll_file`] is a dependency-free poll watcher on the resolved config
//!   file, evaluated once per tick from the app's drive loop.
//!
//! The watcher polls `(mtime, len)` rather than subscribing to file events: a
//! file-event crate is a new dependency, which the current-phase rule gates
//! behind an ADR, and a poll also survives the atomic-rename writes editors use
//! (an event watcher bound to the old inode would miss them).
//!
//! ## Applied subset
//!
//! Every `Live`-class field has a runtime adopter:
//!
//! * runtime presentation (CTX-0814): decoration geometry, outline colors and
//!   widths, animation policy, window padding/radius, font size, and the
//!   workspace bar;
//! * runtime (CTX-0898, #1522): `appearance.theme` / `appearance.colors`
//!   (`set_theme_palette`), `font.family` / `font.line_height` /
//!   `font.letter_spacing` (`set_font_face`: face reload + atlas rebuild +
//!   reflow), `window.opacity` (`set_window_opacity`: GPU surface alpha), and
//!   `decoration.background_*` plus `views` (`set_background_appearance`: the
//!   full fail-closed load pipeline before any swap);
//! * app chrome (CTX-0898): `keymaps`, `mod_key`, `leader_key`,
//!   `leader_timeout_ms`, `hints_enabled`, `layout.resize_step` (CTX-0963),
//!   and the platform window transparency hint for `window.opacity`. The
//!   reload resolves them fail-closed together with the runtime setters and
//!   stashes the result; the app takes it on the same tick
//!   ([`take_app_adoption`]), because the chrome state lives on
//!   `TerminalApp`, not on `Runtime`.
//!
//! [`runtime_adopts`] is the single list of adopted fields. A future `Live`
//! field without an adopter stays out of it and is reported under
//! `restart_required` until it gains one.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use bitty_config::{EffectiveConfig, ReloadReport};

/// What a reload attempt did, as the caller-visible result.
#[derive(Debug)]
pub(crate) enum ReloadOutcome {
    /// The re-resolved config equals the running one; nothing to apply.
    Unchanged(ReloadReport),
    /// Live fields changed and were adopted by the runtime.
    Applied(ReloadReport),
    /// A restart-required field changed; the running config was kept.
    RestartRequired(ReloadReport),
    /// The new config was rejected (invalid field or unknown key); the running
    /// config was kept.
    Rejected(ReloadReport),
}

impl ReloadOutcome {
    /// The classification report for this outcome.
    #[must_use]
    pub(crate) fn report(&self) -> &ReloadReport {
        match self {
            Self::Unchanged(report)
            | Self::Applied(report)
            | Self::RestartRequired(report)
            | Self::Rejected(report) => report,
        }
    }

    /// Whether the change was adopted into the running runtime.
    #[must_use]
    fn applied(&self) -> bool {
        matches!(self, Self::Unchanged(_) | Self::Applied(_))
    }

    /// A stable machine-readable kind for the ctl response.
    #[must_use]
    fn kind(&self) -> &'static str {
        match self {
            Self::Unchanged(_) => "unchanged",
            Self::Applied(_) => "applied",
            Self::RestartRequired(_) => "restart-required",
            Self::Rejected(_) => "rejected",
        }
    }
}

/// Caller-visible result of a reload attempt, ready for JSON encoding by the
/// ctl layer (which owns `json_escape`).
#[derive(Debug)]
pub(crate) struct ReloadOutcomeInfo {
    /// Resolved config file path, or `(defaults; no file)`.
    pub(crate) path: String,
    /// Machine-readable kind: `unchanged`, `applied`, `restart-required`,
    /// `rejected`, `load-error`, or `apply-error`.
    pub(crate) kind: &'static str,
    /// Whether the running runtime adopted the change.
    pub(crate) applied: bool,
    /// Dotted paths that changed (the report's diff fields).
    pub(crate) changed: Vec<String>,
    /// Changed live-class fields the running runtime has no adopter for yet;
    /// they are recorded and take effect on the next start.
    pub(crate) restart_required: Vec<String>,
    /// Detail for `load-error` / `apply-error`; `None` otherwise.
    pub(crate) message: Option<String>,
}

/// Whether the running app adopts `field` live through [`apply_live`].
/// Every other changed field, even when `bitty-config` classifies it
/// `Live`, is reported under `restart_required` so a reply never claims an
/// adoption that did not happen.
///
/// Platform caveat for `window.opacity`: the renderer half (surface alpha)
/// always adopts, but winit can only set the window transparency hint at
/// creation on X11, so raising transparency on an X11 window that started
/// opaque may stay visually opaque until restart. That is a platform limit
/// of the hint, not a missing adopter, so the field stays adopted.
fn runtime_adopts(field: &str) -> bool {
    matches!(
        field,
        "font.family"
            | "font.size"
            | "font.line_height"
            | "font.letter_spacing"
            | "window.opacity"
            | "window.padding"
            | "window.radius_px"
            | "decoration.gaps_in"
            | "decoration.gaps_out"
            | "decoration.border"
            | "decoration.radius"
            | "decoration.content_inset"
            | "decoration.border_color"
            | "decoration.border_color_focused"
            | "decoration.border_color_idle"
            | "decoration.border_width"
            | "decoration.border_width_focused"
            | "decoration.border_width_idle"
            | "decoration.background_image"
            | "decoration.background_fit"
            | "decoration.background_image_roots"
            | "appearance.theme"
            | "appearance.colors"
            | "keymaps"
            | "mod_key"
            | "leader_key"
            | "leader_timeout_ms"
            | "hints_enabled"
            | "layout.resize_step"
            | "workspace.show_bar"
            | "workspace.bar.edge"
    ) || field.starts_with("appearance.animations")
        || field == "views"
        || field.starts_with(bitty_config::VIEWS_FIELD_PREFIX)
}

/// The reload engine: pure policy driver over the last-applied config.
///
/// `reload` never touches the running `Runtime`; it only classifies and (for a
/// clean live diff) commits the new config into `self`. The caller then adopts
/// the committed config. On any rejection or restart requirement the committed
/// config is left untouched, so `current` is always the truth the runtime
/// reflects.
#[derive(Debug)]
pub(crate) struct ReloadEngine {
    current: EffectiveConfig,
}

impl ReloadEngine {
    /// Build an engine seeded with the config the runtime is running.
    #[must_use]
    pub(crate) fn new(current: EffectiveConfig) -> Self {
        Self { current }
    }

    /// The config the runtime currently reflects.
    #[must_use]
    pub(crate) fn current(&self) -> &EffectiveConfig {
        &self.current
    }

    /// Restore `current` after a failed adopt, keeping engine and runtime in
    /// sync.
    fn replace_current(&mut self, config: EffectiveConfig) {
        self.current = config;
    }

    /// Classify `incoming` against the running config and, when it is a clean
    /// live change, commit it.
    pub(crate) fn reload(&mut self, incoming: EffectiveConfig) -> ReloadOutcome {
        let report = bitty_config::diff(&self.current, &incoming);
        if report.has_rejected {
            return ReloadOutcome::Rejected(report);
        }
        if report.needs_restart {
            return ReloadOutcome::RestartRequired(report);
        }
        if report.diffs.is_empty() {
            return ReloadOutcome::Unchanged(report);
        }
        match bitty_config::reconcile_live(&mut self.current, &incoming) {
            Ok(applied) => ReloadOutcome::Applied(applied),
            // `reconcile_live` only rejects the rejected / restart-required
            // cases already handled above; if that ever changes, keep the
            // running config and report the classify-level diff.
            Err(_) => ReloadOutcome::Rejected(report),
        }
    }
}

/// App-owned half of a live reload (CTX-0898, #1522): chrome key state and
/// the platform window opacity hint, resolved fail-closed by [`apply_live`]
/// and taken by the app on the same tick via [`take_app_adoption`].
#[derive(Debug, Clone)]
pub(crate) struct AppAdoption {
    /// Resolved keymap table (shipped defaults under `mod_key` + overrides).
    pub(crate) keymaps: Vec<bitty_config::ResolvedKeymap>,
    /// Resolved Leader binding (`leader_key` / `leader_timeout_ms`).
    pub(crate) leader: bitty_config::ResolvedLeader,
    /// Resolved hint kill switch (`hints_enabled`).
    pub(crate) hints_enabled: bool,
    /// Tiled resize step (`layout.resize_step`, CTX-0963): read at keypress
    /// time, adopted live like the keymaps.
    pub(crate) resize_step: f32,
    /// Effective `window.opacity` for the platform transparency hint.
    pub(crate) window_opacity: f32,
    /// Resolved theme preset name for the window title (CTX-0898). The
    /// title's source-layer suffix is the one from launch: the reload engine
    /// sees only the merged effective config, not layer attribution.
    pub(crate) theme_name: &'static str,
}

thread_local! {
    /// The latest successfully applied [`AppAdoption`], awaiting the app.
    /// One slot, overwritten by each successful reload, so it is bounded and
    /// the app always adopts the newest accepted config. Main-thread only,
    /// like [`CONTEXT`].
    static PENDING_APP: RefCell<Option<AppAdoption>> = const { RefCell::new(None) };

    /// Last live OS appearance event (CTX-0951 CodeRabbit on #1687).
    ///
    /// The cold-path [`bitty_platform::query_system_appearance`] degrades to
    /// `Unknown` on every platform today, so a reload that re-queries would
    /// reinstall the dark half even after a live light event selected the
    /// light half. The `SystemAppearanceChanged` handler records each known
    /// event here; reload resolutions prefer it and keep the dark-first
    /// fallback before the first event. Main-thread only, like [`CONTEXT`].
    static LAST_APPEARANCE: RefCell<Option<bitty_platform::SystemAppearance>> =
        const { RefCell::new(None) };
}

/// Record a live OS appearance event for later reload resolutions.
///
/// Only known signals (`Light`/`Dark`) are retained; `Unknown` never
/// overwrites a real event. Cold-path only (one OS toggle per event).
pub(crate) fn record_system_appearance(appearance: bitty_platform::SystemAppearance) {
    if appearance.is_known() {
        LAST_APPEARANCE.with(|slot| *slot.borrow_mut() = Some(appearance));
    }
}

/// Whether a reload resolves a dual theme to its light half (CTX-0951).
///
/// Prefers the last live appearance event when one was recorded; otherwise
/// falls back to the synchronous OS query (dark-first while it degrades to
/// `Unknown`).
fn reload_prefer_light() -> bool {
    if let Some(appearance) = LAST_APPEARANCE.with(|slot| *slot.borrow()) {
        return matches!(appearance, bitty_platform::SystemAppearance::Light);
    }
    matches!(
        bitty_platform::query_system_appearance(),
        bitty_platform::SystemAppearance::Light
    )
}

/// Take the pending app-side adoption, if a reload produced one since the
/// last call.
pub(crate) fn take_app_adoption() -> Option<AppAdoption> {
    PENDING_APP.with(|slot| slot.borrow_mut().take())
}

/// Resolve the app-owned half of `effective` without side effects.
///
/// Uses the startup resolvers (`resolve_keymaps`, `resolve_leader_for`,
/// `resolve_hint_config`) so a reload binds exactly what a fresh launch
/// would; any error fails the whole reload before the runtime is touched.
///
/// CTX-0951: the title theme resolves to the startup-active half of a dual
/// selection (dark-first while the OS query degrades to `Unknown`), matching
/// `load_app_config`. Live OS toggles refresh the title separately via the
/// `SystemAppearanceChanged` handler, which also records the event so later
/// reloads keep the live half (CodeRabbit on #1687).
fn resolve_app_adoption(effective: &EffectiveConfig) -> Result<AppAdoption, String> {
    let keymaps =
        bitty_config::resolve_keymaps(effective).map_err(|err| format!("keymaps: {err}"))?;
    let leader = bitty_config::resolve_leader_for(effective, bitty_config::LeaderPlatform::host())
        .map_err(|err| format!("leader_key: {err}"))?;
    let prefer_light = reload_prefer_light();
    Ok(AppAdoption {
        keymaps,
        leader,
        hints_enabled: bitty_config::resolve_hint_config(effective).enabled,
        resize_step: effective.layout.resize_step,
        window_opacity: effective.window.opacity,
        theme_name: bitty_config::theme::resolve_selection(
            effective.appearance.theme.as_deref(),
            prefer_light,
        )
        .name,
    })
}

/// Adopt every live field of `effective`: the runtime half directly, the app
/// half by stashing it for [`take_app_adoption`].
///
/// Resolution that can fail without side effects (the startup runtime-config
/// mapping, keymaps, leader) runs first. The runtime setters then run with
/// the theme before backgrounds/views (their AC-1/AC-2 check reads the
/// installed theme background) and the font face before the font size (the
/// size reload reads the installed family). The app half is stashed only
/// when every runtime setter succeeded. Returns the first error, formatted
/// for the ctl reply.
pub(crate) fn apply_live(
    runtime: &mut bitty_runtime::Runtime,
    effective: &EffectiveConfig,
) -> Result<(), String> {
    let app = resolve_app_adoption(effective)?;
    apply_live_presentation(runtime, effective)?;
    PENDING_APP.with(|slot| *slot.borrow_mut() = Some(app));
    Ok(())
}

/// Adopt the runtime-owned live subset of `effective` into `runtime`.
///
/// Reuses the startup mapping
/// ([`crate::config_cli::runtime_config_from_effective_for`] with the current
/// OS query) so a reload resolves the same theme/decoration/outline/
/// animation/font/background values as a fresh launch, then drives the
/// runtime's live-adopt setters. Returns the first setter error, formatted
/// for the ctl reply.
///
/// CTX-0951: dual themes adopt the half matching the last live OS appearance
/// event when one was recorded, else the half matching the OS query at reload
/// time (dark-first while the query degrades to `Unknown`).
pub(crate) fn apply_live_presentation(
    runtime: &mut bitty_runtime::Runtime,
    effective: &EffectiveConfig,
) -> Result<(), String> {
    let prefer_light = reload_prefer_light();
    let resolved = crate::config_cli::runtime_config_from_effective_for(effective, prefer_light)?;
    // CTX-0898 ordering: every input of the per-`View` RFC-0001 AC-1/AC-2
    // check — the theme background, the global focused/idle outline colors,
    // and the outline ring widths (the AC-2 width cue) — is installed before
    // `set_background_appearance`, so the check runs against exactly the
    // config this reload adopts, never a stale mix of old and new.
    runtime.set_theme_palette(resolved.theme);
    runtime
        .set_decoration(resolved.decoration)
        .map_err(|err| format!("decoration: {err}"))?;
    runtime.set_outline(resolved.outline_focused, resolved.outline_idle);
    runtime
        .set_outline_widths(resolved.outline_width_focused, resolved.outline_width_idle)
        .map_err(|err| format!("decoration.border_width: {err}"))?;
    runtime
        .set_background_appearance(
            resolved.view_appearance,
            resolved.background_image,
            resolved.background_fit,
            resolved.background_image_roots,
        )
        .map_err(|err| format!("decoration.background: {err}"))?;
    runtime.set_animations(resolved.animations);
    runtime
        .set_window_padding(effective.window.padding)
        .map_err(|err| format!("window.padding: {err}"))?;
    runtime
        .set_window_radius_px(effective.window.radius_px)
        .map_err(|err| format!("window.radius_px: {err}"))?;
    // CTX-0898: `font.family`, `font.size`, and the derived base cell
    // (`font.line_height` / `font.letter_spacing`) adopt together, so a
    // reload changing several of them rebuilds the atlas once.
    runtime
        .set_font_face(
            &resolved.font_family,
            resolved.font_size,
            resolved.cell_width,
            resolved.cell_height,
        )
        .map_err(|err| format!("font: {err}"))?;
    let blendable = runtime
        .set_window_opacity(effective.window.opacity)
        .map_err(|err| format!("window.opacity: {err}"))?;
    if !blendable {
        crate::logging::warn(|| {
            format!(
                "bitty: window.opacity={:.3} unsupported on this GPU surface (no premultiplied alpha mode) — staying opaque",
                bitty_platform::sanitize_opacity(effective.window.opacity)
            )
        });
    }
    // CTX-0979: Core draws no workspace display. `workspace.show_bar` and
    // `workspace.bar.edge` are accepted by `bitty-config` for the bar
    // plugin; Core carries no visibility or edge state to adopt here.
    Ok(())
}

/// A `(mtime, len)` fingerprint of the watched file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
    /// Symlink target when the watched path is a symlink, so retargeting it to
    /// a file with the same mtime and length is still a change.
    target: Option<PathBuf>,
}

/// Minimum time between two `stat` calls of the watched file.
///
/// The poll runs from the per-frame drive loop; stat-ing every frame would put
/// filesystem latency (for example a network home directory) on the render
/// path. Half a second keeps an edit visible almost immediately.
pub(crate) const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Dependency-free poll watcher over one config path.
///
/// Stat-ing the path (rather than holding an open handle) means an
/// atomic-rename or symlink-retarget rewrite is observed as a change, and the
/// watcher needs no event subscription. The cost is one `stat` per
/// [`CONFIG_POLL_INTERVAL`]. An edit that keeps both the length and the
/// filesystem's mtime granularity unchanged is not observed until the next
/// differing write; `bitty ctl config reload` is the explicit path.
#[derive(Debug)]
pub(crate) struct ConfigFileWatcher {
    path: PathBuf,
    last: Option<Stamp>,
    next_check: Instant,
}

impl ConfigFileWatcher {
    /// Start watching `path`, seeding the baseline from its current state.
    pub(crate) fn new(path: PathBuf) -> Self {
        let last = Self::stamp(&path);
        Self {
            path,
            last,
            next_check: Instant::now(),
        }
    }

    fn stamp(path: &std::path::Path) -> Option<Stamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Stamp {
            mtime: meta.modified().ok(),
            len: meta.len(),
            target: std::fs::read_link(path).ok(),
        })
    }

    /// True when the file changed since the previous check. Creating,
    /// removing, or rewriting the file all count; the baseline seeded by
    /// [`Self::new`] means an unchanged file reports `false` on the first
    /// check. Checks are throttled to [`CONFIG_POLL_INTERVAL`]: a call before
    /// the next check time reports `false` without touching the filesystem.
    pub(crate) fn poll_at(&mut self, at: Instant) -> bool {
        if at < self.next_check {
            return false;
        }
        self.next_check = at + CONFIG_POLL_INTERVAL;
        let now = Self::stamp(&self.path);
        let changed = now != self.last;
        self.last = now;
        changed
    }
}

/// The installed reload context: the engine plus how to re-resolve the config.
struct ReloadContext {
    engine: ReloadEngine,
    /// The config the process started with. Fields without a runtime adopter
    /// still run with these values until restart, so pending restart fields
    /// are always computed against it rather than against the last reload.
    launched: EffectiveConfig,
    /// Re-resolve the effective config from the original sources.
    resolve: Box<dyn Fn() -> Result<EffectiveConfig, String>>,
    watcher: Option<ConfigFileWatcher>,
    path_label: String,
}

thread_local! {
    /// Process-wide reload context, installed once by `main` before the event
    /// loop. The ctl drain and the tick poll both run on the main thread that
    /// owns `Runtime`, so a thread-local matches the existing control-queue
    /// pattern and keeps the `!Send` runtime single-threaded.
    static CONTEXT: RefCell<Option<ReloadContext>> = const { RefCell::new(None) };
}

/// Install the production reload context from the resolved startup config.
///
/// Called once by `main` before the event loop. `file_path` is the resolved
/// user config file (as reported at startup) and becomes the watched path.
pub(crate) fn install(
    args: crate::cli::Args,
    effective: EffectiveConfig,
    file_path: Option<PathBuf>,
) {
    let path_label = file_path.as_ref().map_or_else(
        || String::from("(defaults; no file)"),
        |path| path.display().to_string(),
    );
    let watcher = file_path.map(ConfigFileWatcher::new);
    let resolve: Box<dyn Fn() -> Result<EffectiveConfig, String>> =
        Box::new(move || crate::config_cli::load_app_config(&args).map(|app| app.effective));
    CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(ReloadContext {
            launched: effective.clone(),
            engine: ReloadEngine::new(effective),
            resolve,
            watcher,
            path_label,
        });
    });
}

/// Remove any installed context (test teardown). Production installs once and
/// exits with the process, so it needs no clear.
#[cfg(test)]
pub(crate) fn clear() {
    CONTEXT.with(|slot| *slot.borrow_mut() = None);
    LAST_APPEARANCE.with(|slot| *slot.borrow_mut() = None);
}

/// Install a context with an injected resolver (tests only).
#[cfg(test)]
pub(crate) fn install_with(
    effective: EffectiveConfig,
    resolve: Box<dyn Fn() -> Result<EffectiveConfig, String>>,
    watch: Option<PathBuf>,
) {
    let path_label = watch.as_ref().map_or_else(
        || String::from("(defaults; no file)"),
        |path| path.display().to_string(),
    );
    let watcher = watch.map(ConfigFileWatcher::new);
    CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(ReloadContext {
            launched: effective.clone(),
            engine: ReloadEngine::new(effective),
            resolve,
            watcher,
            path_label,
        });
    });
}

/// Serve an explicit `bitty ctl` reload against the installed context.
///
/// Returns `None` when no context is installed, so the caller keeps its
/// probe-only response (headless/test paths never install a context).
pub(crate) fn reload_requested(runtime: &mut bitty_runtime::Runtime) -> Option<ReloadOutcomeInfo> {
    CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let ctx = slot.as_mut()?;
        Some(ctx.reload_into(runtime))
    })
}

/// The live effective config the runtime currently reflects (CTX-0951).
///
/// Clones the reload engine's committed `current` when a context is installed
/// (production installs it in `main` before the event loop; tests use
/// `install_with`). Returns `None` without a context, so the
/// `SystemAppearanceChanged` handler stays a no-op there instead of guessing.
/// The clone is cold-path only (one OS toggle per event), never per frame.
pub(crate) fn current_effective() -> Option<EffectiveConfig> {
    CONTEXT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|ctx| ctx.engine.current().clone())
    })
}

/// Poll the watched config file; when it changed, reload and adopt. Returns
/// the outcome (for logging) only when a change was seen.
pub(crate) fn poll_file(runtime: &mut bitty_runtime::Runtime) -> Option<ReloadOutcomeInfo> {
    poll_file_at(runtime, Instant::now())
}

/// [`poll_file`] against an explicit clock (the watcher is throttled to
/// [`CONFIG_POLL_INTERVAL`]).
fn poll_file_at(runtime: &mut bitty_runtime::Runtime, at: Instant) -> Option<ReloadOutcomeInfo> {
    CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let ctx = slot.as_mut()?;
        if !ctx.watcher.as_mut()?.poll_at(at) {
            return None;
        }
        let info = ctx.reload_into(runtime);
        crate::logging::info(|| {
            format!(
                "bitty: config file changed -> reload {} (applied={})",
                info.kind, info.applied
            )
        });
        Some(info)
    })
}

/// Live-class fields that differ between `launched` and `current` and that
/// `adopts` rejects: the restart-pending set (split out so the filter is
/// testable while every current `Live` field has an adopter).
fn pending_fields(
    launched: &EffectiveConfig,
    current: &EffectiveConfig,
    adopts: fn(&str) -> bool,
) -> Vec<String> {
    bitty_config::diff(launched, current)
        .diffs
        .into_iter()
        .filter(|diff| diff.class == bitty_config::ReloadClass::Live && !adopts(&diff.field))
        .map(|diff| diff.field)
        .collect()
}

impl ReloadContext {
    /// Live-class fields whose accepted value differs from the launch value
    /// but that the runtime cannot adopt live: they stay pending until restart
    /// on every later reply, including an `unchanged` one (CodeRabbit on
    /// #1516), and drop out once the file returns to the launch value.
    fn pending_restart_fields(&self) -> Vec<String> {
        pending_fields(&self.launched, self.engine.current(), runtime_adopts)
    }

    /// Re-resolve, classify, adopt if live, and build the caller-visible info.
    fn reload_into(&mut self, runtime: &mut bitty_runtime::Runtime) -> ReloadOutcomeInfo {
        let incoming = match (self.resolve)() {
            Ok(config) => config,
            Err(message) => {
                return ReloadOutcomeInfo {
                    path: self.path_label.clone(),
                    kind: "load-error",
                    applied: false,
                    changed: Vec::new(),
                    restart_required: Vec::new(),
                    message: Some(message),
                };
            }
        };
        let previous = self.engine.current().clone();
        let outcome = self.engine.reload(incoming);
        let applied = outcome.applied();
        if matches!(outcome, ReloadOutcome::Applied(_)) {
            if let Err(message) = apply_live(runtime, self.engine.current()) {
                // Keep engine and runtime consistent: a failed adopt must not
                // leave the engine claiming the new config is live. Earlier
                // setters may already have changed the runtime, so reapply the
                // previous presentation first (CodeRabbit on #1516). The
                // background setter swaps its retained generation back, so
                // the rollback never re-reads an image from disk (CTX-0898).
                if let Err(restore) = apply_live_presentation(runtime, &previous) {
                    crate::logging::warn(|| {
                        format!("bitty: config reload rollback failed: {restore}")
                    });
                }
                runtime.release_retained_backgrounds();
                self.engine.replace_current(previous);
                return ReloadOutcomeInfo {
                    path: self.path_label.clone(),
                    kind: "apply-error",
                    applied: false,
                    changed: Vec::new(),
                    restart_required: Vec::new(),
                    message: Some(message),
                };
            }
        }
        // Committed (or nothing to adopt): drop the replaced background
        // generation so at most one decoded generation stays resident.
        runtime.release_retained_backgrounds();
        let changed: Vec<String> = outcome
            .report()
            .diffs
            .iter()
            .map(|diff| diff.field.clone())
            .collect();
        let restart_required = self.pending_restart_fields();
        ReloadOutcomeInfo {
            path: self.path_label.clone(),
            kind: outcome.kind(),
            applied,
            changed,
            restart_required,
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::ReloadClass;
    use std::time::SystemTime;

    fn changed_fields(info: &ReloadOutcomeInfo) -> Vec<&str> {
        info.changed.iter().map(String::as_str).collect()
    }

    /// A tiny valid config file the resolver reads back, so a change to the
    /// running engine is expressed as a file edit rather than a fabricated
    /// diff.
    fn write_config(path: &std::path::Path, font_size: f32) {
        std::fs::write(path, format!("[font]\nsize = {font_size}\n")).expect("write config");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        dir.push(format!(
            "bitty-reload-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn watcher_polls_at_most_once_per_interval() {
        let dir = temp_dir("interval");
        let path = dir.join("bitty.toml");
        write_config(&path, 12.0);
        let mut watcher = ConfigFileWatcher::new(path.clone());
        let start = Instant::now();
        assert!(!watcher.poll_at(start), "unchanged file");
        write_config(&path, 13.25);
        assert!(
            !watcher.poll_at(start + CONFIG_POLL_INTERVAL / 2),
            "no stat before the interval elapses"
        );
        assert!(
            watcher.poll_at(start + CONFIG_POLL_INTERVAL),
            "change seen at the next check"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unadopted_live_field_is_reported_pending_until_reverted() {
        // Every current `Live` field has an adopter (CTX-0898), so the
        // pending filter is exercised with an adopter that refuses one field:
        // the filter must report it while it differs from the launch value
        // and drop it once the value returns.
        fn all_but_family(field: &str) -> bool {
            field != "font.family"
        }
        let baseline = bitty_config::fallback_builtin();
        let mut edited = baseline.clone();
        edited.font.family = format!("{} Alt", baseline.font.family);
        edited.font.size = 15.0;
        assert_eq!(
            pending_fields(&baseline, &edited, all_but_family),
            vec![String::from("font.family")]
        );
        assert!(pending_fields(&baseline, &edited, runtime_adopts).is_empty());
        assert!(pending_fields(&baseline, &baseline, all_but_family).is_empty());
    }

    #[test]
    fn every_live_inventory_field_has_an_adopter() {
        // CTX-0898 acceptance: iterate the exported single-source Live list
        // that drives `classify_field`, so a new Live leaf without an adopter
        // fails here instead of silently landing in `restart_required`.
        for field in bitty_config::LIVE_FIELDS {
            assert_eq!(
                bitty_config::classify_field(field),
                ReloadClass::Live,
                "{field} is listed Live but classifies otherwise"
            );
            assert!(runtime_adopts(field), "{field} has no live adopter");
        }
        let per_view = format!("{}*]", bitty_config::VIEWS_FIELD_PREFIX);
        assert_eq!(bitty_config::classify_field(&per_view), ReloadClass::Live);
        assert!(runtime_adopts(&per_view), "{per_view} has no live adopter");
        for field in bitty_config::RESTART_REQUIRED_FIELDS {
            assert!(!runtime_adopts(field), "{field} must stay restart-required");
        }
    }

    #[test]
    fn runtime_font_family_bound_matches_config() {
        // Review PX-4285 finding 6: the runtime mirrors the config bound
        // (no normal bitty-config dependency there); pin equality.
        assert_eq!(
            bitty_runtime::runtime::live_config::MAX_LIVE_FONT_FAMILY_BYTES,
            bitty_config::types::MAX_FONT_FAMILY_LEN
        );
    }

    #[test]
    fn every_diffed_live_leaf_is_in_the_exported_list() {
        // The diff emits only listed Live leaves: change every Live leaf at
        // once and check each emitted path is in `LIVE_FIELDS` (or a views
        // selector path), so the list cannot fall behind the diff.
        let base = bitty_config::fallback_builtin();
        let mut all = base.clone();
        all.font.family = format!("{} Alt", base.font.family);
        all.font.size = base.font.size + 1.0;
        all.font.line_height = 1.5;
        all.font.letter_spacing = 1.0;
        all.window.opacity = 0.5;
        all.window.padding = base.window.padding + 1;
        all.window.radius_px = base.window.radius_px + 1;
        all.appearance.theme = Some(String::from("dracula"));
        all.mod_key = bitty_config::ModKey::Super;
        all.leader_timeout_ms = Some(2_000);
        all.hints_enabled = Some(false);
        all.decoration.background_fit = Some(bitty_config::types::BackgroundFit::Tile);
        all.decoration.background_image_roots = Some(vec![
            std::env::temp_dir()
                .join("bitty-walls")
                .display()
                .to_string(),
        ]);
        all.views = vec![bitty_config::types::ViewOverride {
            selector: bitty_config::types::ViewSelector::Wildcard,
            overrides: bitty_config::types::ViewAppearanceOverride {
                border_width: Some(2),
                ..Default::default()
            },
        }];
        let report = bitty_config::diff(&base, &all);
        assert!(!report.has_rejected, "{report:?}");
        assert!(report.diffs.len() >= 14, "{report:?}");
        for diff in &report.diffs {
            assert_eq!(diff.class, ReloadClass::Live, "{diff:?}");
            assert!(
                bitty_config::LIVE_FIELDS.contains(&diff.field.as_str())
                    || diff.field.starts_with(bitty_config::VIEWS_FIELD_PREFIX),
                "{} is diffed but not in LIVE_FIELDS",
                diff.field
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn watcher_detects_a_symlink_retarget_with_identical_metadata() {
        let dir = temp_dir("retarget");
        let first = dir.join("a.toml");
        let second = dir.join("b.toml");
        write_config(&first, 12.0);
        write_config(&second, 12.0);
        // Same length and same mtime: only the link target differs.
        let stamp = std::fs::metadata(&first)
            .and_then(|meta| meta.modified())
            .expect("mtime");
        std::fs::File::options()
            .write(true)
            .open(&second)
            .and_then(|file| file.set_modified(stamp))
            .expect("align mtime");
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&first, &link).expect("symlink");
        let mut watcher = ConfigFileWatcher::new(link.clone());
        let start = Instant::now();
        assert!(!watcher.poll_at(start), "unchanged link");
        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&second, &link).expect("retarget");
        assert!(
            watcher.poll_at(start + CONFIG_POLL_INTERVAL),
            "a retarget is a change even with identical target metadata"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_runtime_adopted_fields_are_reported_as_live() {
        assert!(runtime_adopts("font.size"));
        assert!(runtime_adopts("decoration.gaps_in"));
        assert!(runtime_adopts("appearance.animations.duration_ms.open"));
        assert!(runtime_adopts("workspace.show_bar"));
        assert!(runtime_adopts("workspace.bar.edge"));
        for restart in [
            "terminal.scrollback",
            "terminal.shell",
            "layout.gaps_in",
            "plugins",
            "close_confirm",
        ] {
            assert!(!runtime_adopts(restart), "{restart} is restart-required");
        }
    }

    fn deterministic_runtime(effective: &EffectiveConfig) -> bitty_runtime::Runtime {
        let cfg = crate::config_cli::runtime_config_from_effective(effective).expect("cfg");
        bitty_runtime::Runtime::with_deterministic_rasterizer(cfg).expect("runtime")
    }

    #[test]
    fn apply_live_adopts_theme_and_custom_colors() {
        let baseline = bitty_config::fallback_builtin();
        let mut runtime = deterministic_runtime(&baseline);
        let mut light = baseline.clone();
        light.appearance.theme = Some(String::from("dracula"));
        apply_live(&mut runtime, &light).expect("adopt theme");
        let expected = bitty_runtime::ThemePalette::from_theme(bitty_config::theme::resolve_theme(
            Some("dracula"),
        ));
        assert_eq!(runtime.config().theme.background, expected.background);
        assert_eq!(runtime.active_background(), expected.background);

        let mut custom = light.clone();
        let preset = bitty_config::theme::resolve_theme(None);
        let palette = bitty_config::theme::CustomPalette {
            background: [0x10, 0x20, 0x30],
            foreground: preset.foreground,
            cursor: preset.cursor,
            selection: preset.selection,
            ansi: preset.ansi,
        };
        custom.appearance.colors = Some(palette);
        apply_live(&mut runtime, &custom).expect("adopt colors");
        assert_eq!(
            runtime.active_background(),
            [0x10, 0x20, 0x30, 0xFF],
            "appearance.colors replaces the preset palette"
        );
        let _ = take_app_adoption();
    }

    #[test]
    fn apply_live_adopts_font_family_and_spacing() {
        let baseline = bitty_config::fallback_builtin();
        let mut runtime = deterministic_runtime(&baseline);
        let before = runtime.live_cell_size();
        let mut edited = baseline.clone();
        edited.font.family = String::from("Alt Mono");
        edited.font.line_height = 1.5;
        edited.font.letter_spacing = 2.0;
        apply_live(&mut runtime, &edited).expect("adopt font");
        let (width, height) = edited.font.default_effective_cell();
        assert_eq!(runtime.config().font_family, "Alt Mono");
        assert_eq!(
            (runtime.config().cell_width, runtime.config().cell_height),
            (width, height)
        );
        assert_ne!(runtime.live_cell_size(), before, "atlas cell rebuilt");
        let _ = take_app_adoption();
    }

    #[test]
    fn apply_live_adopts_window_opacity_and_stashes_it_for_the_app() {
        let baseline = bitty_config::fallback_builtin();
        let mut runtime = deterministic_runtime(&baseline);
        let _ = take_app_adoption();
        let mut edited = baseline.clone();
        edited.window.opacity = 0.75;
        apply_live(&mut runtime, &edited).expect("adopt opacity");
        assert!((runtime.window_opacity() - 0.75).abs() < f32::EPSILON);
        let app = take_app_adoption().expect("app half stashed");
        assert!((app.window_opacity - 0.75).abs() < f32::EPSILON);
        assert!(take_app_adoption().is_none(), "single-shot slot");
    }

    #[test]
    fn apply_live_resolves_keymaps_leader_and_hints_for_the_app() {
        let baseline = bitty_config::fallback_builtin();
        let mut runtime = deterministic_runtime(&baseline);
        let _ = take_app_adoption();
        let mut edited = baseline.clone();
        edited.mod_key = bitty_config::ModKey::Super;
        edited.leader_timeout_ms = Some(2_500);
        edited.hints_enabled = Some(false);
        edited.layout.resize_step = 0.02;
        apply_live(&mut runtime, &edited).expect("adopt chrome");
        let app = take_app_adoption().expect("app half stashed");
        let expected = bitty_config::resolve_keymaps(&edited).expect("keymaps");
        assert_eq!(app.keymaps, expected, "mod_key rebinds the shipped map");
        assert_ne!(
            app.keymaps,
            bitty_config::resolve_keymaps(&baseline).expect("baseline"),
        );
        assert_eq!(app.leader.timeout_ms, 2_500);
        assert!(!app.hints_enabled);
        assert!(
            (app.resize_step - 0.02).abs() < f32::EPSILON,
            "CTX-0963: the resize step rides the app half live"
        );
    }

    /// A `views[view:1]` rule overriding the idle outline color only.
    fn view1_idle(color: [u8; 4]) -> bitty_config::types::ViewOverride {
        bitty_config::types::ViewOverride {
            selector: bitty_config::types::ViewSelector::View(1),
            overrides: bitty_config::types::ViewAppearanceOverride {
                border_color_idle: Some(bitty_config::types::OutlineColor(color)),
                ..Default::default()
            },
        }
    }

    const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
    const YELLOW: [u8; 4] = [0xFF, 0xFF, 0x00, 0xFF];

    #[test]
    fn views_check_uses_the_outline_adopted_in_the_same_reload() {
        // Review PX-4285 finding 1: the per-View AC-1/AC-2 check must run
        // against the global outline this reload installs. The safe baseline
        // focuses with #FFFFFF at equal 1/1 widths (no width cue), so AC-2
        // decides on color alone. `view:1` is inert at merge time, so the
        // runtime check is the only gate for it.
        let baseline = bitty_config::fallback_builtin();

        // (a) Valid only under the OLD outline: idle #FFFFFF equals the old
        // focused color (AC-2 does not apply) but sits at 1.07:1 against the
        // new focused #FFFF00. Must be rejected.
        let mut runtime = deterministic_runtime(&baseline);
        let mut stale_ok = baseline.clone();
        stale_ok.decoration.border_color_focused = Some(bitty_config::types::OutlineColor(YELLOW));
        stale_ok.views = vec![view1_idle(WHITE)];
        stale_ok.validate().expect("config-level contract passes");
        let err = apply_live(&mut runtime, &stale_ok).expect_err("new outline violates AC-2");
        assert!(err.contains("AC-2"), "{err}");
        assert!(
            runtime.config().view_appearance.is_empty(),
            "rule not adopted"
        );

        // (b) Valid only under the NEW outline: idle #FFFF00 is 1.07:1
        // against the old focused #FFFFFF but equals the new focused color.
        // Must be accepted.
        let mut runtime = deterministic_runtime(&baseline);
        let mut new_ok = baseline.clone();
        new_ok.decoration.border_color_focused = Some(bitty_config::types::OutlineColor(YELLOW));
        new_ok.views = vec![view1_idle(YELLOW)];
        new_ok.validate().expect("config-level contract passes");
        apply_live(&mut runtime, &new_ok).expect("valid under the new outline");
        assert_eq!(runtime.config().outline_focused, YELLOW);
        assert_eq!(runtime.config().view_appearance.len(), 1);
        let _ = take_app_adoption();
    }

    /// 4x4 opaque-red PNG (same hermetic fixture as the runtime tests).
    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x06, 0x00, 0x00, 0x00, 0xA9,
        0xF1, 0x9E, 0x7E, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xFC,
        0xCF, 0xC0, 0xF0, 0x9F, 0x01, 0x09, 0x30, 0x21, 0x73, 0x88, 0x13, 0x00, 0x00, 0x83, 0xD1,
        0x02, 0x06, 0x04, 0xBC, 0x24, 0x47, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
        0x42, 0x60, 0x82,
    ];

    /// Baseline with one approved, decoded global background image.
    fn baseline_with_image(dir: &std::path::Path) -> EffectiveConfig {
        let image = dir.join("red.png");
        std::fs::write(&image, RED_PNG).expect("write fixture");
        let mut baseline = bitty_config::fallback_builtin();
        baseline.decoration.background_image = Some(image.display().to_string());
        baseline.decoration.background_image_roots = Some(vec![dir.display().to_string()]);
        baseline
    }

    #[test]
    fn reload_with_theme_and_bad_background_rolls_back_fully() {
        // Review PX-4285 finding 2: theme change + bad background through
        // the real reload path leaves the runtime on the previous config,
        // with the previous image still resident and never re-read.
        let dir = temp_dir("rollback-bad-bg");
        let baseline = baseline_with_image(&dir);
        let mut runtime = deterministic_runtime(&baseline);
        assert_eq!(runtime.background_image_count(), 1);
        let loads = runtime.background_loads();
        let old_theme = runtime.config().theme;

        let mut incoming = baseline.clone();
        incoming.appearance.theme = Some(String::from("dracula"));
        incoming.decoration.background_image = Some(dir.join("absent.png").display().to_string());
        let resolved = incoming.clone();
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(resolved.clone())),
            None,
        );
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "apply-error", "{info:?}");
        assert!(!info.applied);
        assert_eq!(runtime.config().theme, old_theme, "theme restored");
        assert_eq!(
            runtime.config().background_image,
            baseline.decoration.background_image
        );
        assert_eq!(runtime.background_image_count(), 1, "old image resident");
        assert_eq!(runtime.background_loads(), loads, "no disk re-read");
        assert!(!runtime.has_retained_backgrounds(), "no generation leaks");
        assert!(take_app_adoption().is_none());
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_after_a_background_swap_restores_the_retained_store() {
        // Review PX-4285 finding 2: a setter failing AFTER the background
        // swap rolls back by swapping the retained decoded store back, so the
        // old image is not decoded again (and survives deletion from disk).
        let dir = temp_dir("rollback-retained");
        let baseline = baseline_with_image(&dir);
        let mut runtime = deterministic_runtime(&baseline);
        let other = dir.join("other.png");
        std::fs::write(&other, RED_PNG).expect("write second fixture");

        let mut incoming = baseline.clone();
        incoming.appearance.theme = Some(String::from("dracula"));
        incoming.decoration.background_image = Some(other.display().to_string());
        // A size valid for `bitty-config` (`(0, 128]`) but outside the live
        // zoom range: it passes classification and the runtime-config
        // mapping, then fails in `set_font_face`, which runs after the
        // background swap.
        incoming.font.size = bitty_runtime::config::FONT_ZOOM_MAX_PT * 2.0;
        let old_theme = runtime.config().theme;
        let resolved = incoming.clone();
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(resolved.clone())),
            None,
        );
        // The original image vanishes: a re-read would now fail.
        std::fs::remove_file(dir.join("red.png")).expect("remove original");
        let loads_before = runtime.background_loads();
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "apply-error", "{info:?}");
        assert!(
            info.message.as_deref().unwrap_or_default().contains("font"),
            "{info:?}"
        );
        assert_eq!(runtime.config().theme, old_theme, "theme restored");
        assert_eq!(
            runtime.config().background_image,
            baseline.decoration.background_image,
            "previous background restored"
        );
        assert_eq!(runtime.background_image_count(), 1);
        // `background_loads` counts decodes per store: the swapped-back store
        // reports exactly its pre-reload count, and its source file no longer
        // exists, so the resident image came from retention, not a re-read.
        assert_eq!(
            runtime.background_loads(),
            loads_before,
            "the retained store came back unchanged"
        );
        assert!(!runtime.has_retained_backgrounds(), "retention released");
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_live_fails_closed_on_bad_background_and_stashes_nothing() {
        let baseline = bitty_config::fallback_builtin();
        let mut runtime = deterministic_runtime(&baseline);
        let _ = take_app_adoption();
        let dir = temp_dir("bg");
        let mut edited = baseline.clone();
        edited.decoration.background_image = Some(dir.join("absent.png").display().to_string());
        edited.decoration.background_image_roots = Some(vec![dir.display().to_string()]);
        let err = apply_live(&mut runtime, &edited).expect_err("missing image");
        assert!(err.contains("decoration.background"), "{err}");
        assert!(runtime.config().background_image.is_none());
        assert!(take_app_adoption().is_none(), "no app half on failure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_reports_unchanged_for_identical_config() {
        let base = bitty_config::fallback_builtin();
        let mut engine = ReloadEngine::new(base.clone());
        let outcome = engine.reload(base);
        assert!(matches!(outcome, ReloadOutcome::Unchanged(_)));
        assert_eq!(outcome.kind(), "unchanged");
        assert!(outcome.applied());
    }

    #[test]
    fn engine_applies_a_live_field() {
        let mut engine = ReloadEngine::new(bitty_config::fallback_builtin());
        let mut incoming = bitty_config::fallback_builtin();
        incoming.font.size = 15.0;
        let outcome = engine.reload(incoming);
        assert!(matches!(outcome, ReloadOutcome::Applied(_)), "{outcome:?}");
        assert_eq!(engine.current().font.size, 15.0, "live change is committed");
    }

    #[test]
    fn engine_keeps_running_config_on_restart_required() {
        let mut engine = ReloadEngine::new(bitty_config::fallback_builtin());
        let baseline = engine.current().terminal.scrollback;
        let mut incoming = bitty_config::fallback_builtin();
        incoming.terminal.scrollback = baseline.saturating_add(1);
        let outcome = engine.reload(incoming);
        assert!(
            matches!(outcome, ReloadOutcome::RestartRequired(_)),
            "{outcome:?}"
        );
        assert_eq!(
            engine.current().terminal.scrollback,
            baseline,
            "restart-required change must not be committed"
        );
    }

    #[test]
    fn watcher_detects_create_modify_remove() {
        let dir = temp_dir("watch");
        let path = dir.join("config.toml");
        // Start with no file: the baseline is "absent". Each check advances
        // the clock one poll interval so the throttle never masks a change.
        let mut watcher = ConfigFileWatcher::new(path.clone());
        let mut at = Instant::now();
        let mut poll = |watcher: &mut ConfigFileWatcher| {
            let seen = watcher.poll_at(at);
            at += CONFIG_POLL_INTERVAL;
            seen
        };
        assert!(!poll(&mut watcher), "no change right after construction");
        write_config(&path, 12.0);
        assert!(poll(&mut watcher), "file creation is a change");
        assert!(!poll(&mut watcher), "no change when the file is untouched");
        // A rewrite with a different length changes the stamp even when the
        // mtime granularity is coarse.
        write_config(&path, 15.5);
        assert!(poll(&mut watcher), "rewrite is a change");
        std::fs::remove_file(&path).expect("remove");
        assert!(poll(&mut watcher), "removal is a change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_live_presentation_adopts_font_and_padding() {
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let mut effective = bitty_config::fallback_builtin();
        effective.font.size = 17.0;
        effective.window.padding = 9;
        apply_live_presentation(&mut runtime, &effective).expect("adopt");
        assert!((runtime.config().font_size - 17.0).abs() < f32::EPSILON);
        assert_eq!(runtime.config().window_padding, 9);
    }

    #[test]
    fn reload_keeps_recorded_appearance_for_dual_theme() {
        // CodeRabbit on #1687: the cold-path OS query degrades to Unknown,
        // so a reload must reuse the last live appearance event instead of
        // re-querying, else a light session snaps back to the dark half.
        clear();
        let baseline = bitty_config::fallback_builtin();
        let mut dual = baseline.clone();
        dual.appearance.theme = Some(String::from("light:github-light,dark:dracula"));
        let mut runtime = deterministic_runtime(&baseline);
        let dracula =
            bitty_runtime::ThemePalette::from_theme(&bitty_config::theme::DRACULA).background;
        let light_expected = bitty_runtime::ThemePalette::from_theme(
            bitty_config::theme::resolve_theme(Some("github-light")),
        )
        .background;

        // No event yet: dark-first fallback (query is Unknown everywhere).
        apply_live_presentation(&mut runtime, &dual).expect("adopt dark-first");
        assert_eq!(runtime.config().theme.background, dracula);

        // A live light event sticks across reloads.
        record_system_appearance(bitty_platform::SystemAppearance::Light);
        apply_live_presentation(&mut runtime, &dual).expect("adopt light");
        assert_eq!(runtime.config().theme.background, light_expected);
        let app = resolve_app_adoption(&dual).expect("app adoption");
        assert_eq!(
            app.theme_name,
            bitty_config::theme::resolve_theme(Some("github-light")).name
        );

        // Unknown never overwrites a real event.
        record_system_appearance(bitty_platform::SystemAppearance::Unknown);
        apply_live_presentation(&mut runtime, &dual).expect("adopt still light");
        assert_eq!(runtime.config().theme.background, light_expected);

        // A dark event swaps back.
        record_system_appearance(bitty_platform::SystemAppearance::Dark);
        apply_live_presentation(&mut runtime, &dual).expect("adopt dark");
        assert_eq!(runtime.config().theme.background, dracula);
        clear();
    }

    #[test]
    fn apply_live_presentation_reserves_no_bar_band() {
        // CTX-0979: Core draws no workspace display. A reload flipping
        // `workspace.bar.edge` and `workspace.show_bar` reserves nothing:
        // the container stays the full window and the grid keeps every row.
        let baseline = bitty_config::fallback_builtin();
        let cfg = crate::config_cli::runtime_config_from_effective(&baseline).expect("cfg");
        let mut runtime = bitty_runtime::Runtime::new(cfg).expect("runtime");
        runtime.workspace_new().expect("ws2");
        assert!(runtime.workspace_switch(0));
        let mut engine = ReloadEngine::new(baseline);
        let window = runtime.window_cells();
        assert_eq!(runtime.container(), window, "no Core bar reserved");

        let mut top = bitty_config::fallback_builtin();
        top.workspace.bar_edge = Some(bitty_config::types::WorkspaceBarEdge::Top);
        let outcome = engine.reload(top);
        assert!(matches!(outcome, ReloadOutcome::Applied(_)), "{outcome:?}");
        apply_live_presentation(&mut runtime, engine.current()).expect("adopt edge");
        assert_eq!(runtime.container(), window, "edge change reserves nothing");

        let mut hidden = engine.current().clone();
        hidden.workspace.show_bar = Some(false);
        let outcome = engine.reload(hidden);
        assert!(matches!(outcome, ReloadOutcome::Applied(_)), "{outcome:?}");
        apply_live_presentation(&mut runtime, engine.current()).expect("adopt hide");
        assert_eq!(runtime.container(), window, "hide reserves nothing");
    }

    #[test]
    fn reload_requested_without_context_is_none() {
        clear();
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        assert!(reload_requested(&mut runtime).is_none());
    }

    #[test]
    fn reload_requested_applies_a_live_file_change() {
        let dir = temp_dir("ctl");
        let path = dir.join("config.toml");
        let baseline = bitty_config::fallback_builtin();
        let mut live = baseline.clone();
        live.font.size = 16.0;
        write_config(&path, live.font.size);

        let resolver_path = path.clone();
        let resolver = Box::new(move || {
            let text = std::fs::read_to_string(&resolver_path).map_err(|err| err.to_string())?;
            let size: f32 = text
                .lines()
                .find_map(|line| line.strip_prefix("size = "))
                .and_then(|raw| raw.trim().parse().ok())
                .ok_or_else(|| String::from("size missing"))?;
            let mut config = bitty_config::fallback_builtin();
            config.font.size = size;
            Ok(config)
        });
        clear();
        install_with(baseline, resolver, Some(path.clone()));

        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "applied");
        assert!(info.applied);
        assert!(changed_fields(&info).contains(&"font.size"), "{info:?}");
        assert!((runtime.config().font_size - 16.0).abs() < f32::EPSILON);
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_file_only_fires_on_a_change() {
        let dir = temp_dir("poll");
        let path = dir.join("config.toml");
        write_config(&path, 12.0);
        let resolver_path = path.clone();
        let resolver = Box::new(move || {
            let text = std::fs::read_to_string(&resolver_path).map_err(|err| err.to_string())?;
            let size: f32 = text
                .lines()
                .find_map(|line| line.strip_prefix("size = "))
                .and_then(|raw| raw.trim().parse().ok())
                .ok_or_else(|| String::from("size missing"))?;
            let mut config = bitty_config::fallback_builtin();
            config.font.size = size;
            Ok(config)
        });
        let mut baseline = bitty_config::fallback_builtin();
        baseline.font.size = 12.0;
        clear();
        install_with(baseline, resolver, Some(path.clone()));

        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let start = Instant::now();
        assert!(
            poll_file_at(&mut runtime, start).is_none(),
            "no edit -> no reload"
        );
        // Rewrite with a new size; the next check must observe it and adopt.
        write_config(&path, 20.0);
        let info =
            poll_file_at(&mut runtime, start + CONFIG_POLL_INTERVAL).expect("change observed");
        assert_eq!(info.kind, "applied");
        assert!((runtime.config().font_size - 20.0).abs() < f32::EPSILON);
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_required_change_is_retained_and_reported() {
        let baseline = bitty_config::fallback_builtin();
        let mut live = baseline.clone();
        live.terminal.scrollback = baseline.terminal.scrollback.saturating_add(1);
        let resolve_live = live.clone();
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(resolve_live.clone())),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "restart-required");
        assert!(!info.applied);
        clear();
    }

    #[test]
    fn load_error_is_reported_without_touching_runtime() {
        let baseline = bitty_config::fallback_builtin();
        clear();
        install_with(baseline, Box::new(|| Err(String::from("boom"))), None);
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error");
        assert!(!info.applied);
        assert_eq!(info.message.as_deref(), Some("boom"));
        clear();
    }

    #[test]
    fn classification_marks_font_size_live() {
        // Guard the assumption the engine relies on: font.size is a live field
        // (so `engine_applies_a_live_field` exercises the apply path).
        assert_eq!(bitty_config::classify_field("font.size"), ReloadClass::Live);
        assert_ne!(
            bitty_config::classify_field("terminal.scrollback"),
            ReloadClass::Live
        );
    }

    #[test]
    fn current_effective_tracks_the_committed_config() {
        // CTX-0951: the `SystemAppearanceChanged` handler resolves against
        // this accessor, so it must mirror the engine's committed `current`
        // and stay `None` without a context (handler no-ops there).
        clear();
        assert!(current_effective().is_none());
        let mut baseline = bitty_config::fallback_builtin();
        baseline.font.size = 12.0;
        let mut live = baseline.clone();
        live.font.size = 16.0;
        let resolve_live = live.clone();
        install_with(baseline, Box::new(move || Ok(resolve_live.clone())), None);
        let before = current_effective().expect("context installed");
        assert!((before.font.size - 12.0).abs() < f32::EPSILON);
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "applied");
        let after = current_effective().expect("context installed");
        assert!((after.font.size - 16.0).abs() < f32::EPSILON);
        clear();
        assert!(current_effective().is_none());
    }

    #[test]
    fn dual_theme_title_resolves_to_startup_active_half() {
        // CTX-0951 NEEDS-FIX: the reload title must not fall back to Bitty
        // Dark for a dual selection. While the OS query degrades to Unknown
        // this is the dark half (deterministic in CI).
        let mut effective = bitty_config::fallback_builtin();
        effective.appearance.theme = Some(String::from("light:github-light,dark:dracula"));
        let adoption = resolve_app_adoption(&effective).expect("adoption");
        assert_eq!(adoption.theme_name, "dracula");
    }
}
