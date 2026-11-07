//! Live configuration reload (CTX-0814, issue #1397).
//!
//! `bitty-config` owns the *policy* of a reload: `ReloadClass` and
//! [`bitty_config::diff`] classify a field diff as live / restart-required /
//! rejected. This module is the *activation path* that policy feeds — it holds
//! the last-applied [`EffectiveConfig`], re-reads the sources, and pushes the
//! live presentation subset into the running [`bitty_runtime::Runtime`] through
//! the runtime's live-adopt setters.
//!
//! Two entry points share one engine:
//!
//! * [`reload_requested_with`] serves the `bitty ctl` verb
//!   `bitty.debug/reloadConfig` (an explicit client request, optionally a
//!   dry run for `bitty reload --dry-run`), and
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
//!   reflow), `window.opacity` (`set_window_opacity`: GPU surface alpha),
//!   `decoration.background_*` plus `views` (`set_background_appearance`: the
//!   full fail-closed load pipeline before any swap), and the eight global
//!   chrome-style leaves (CTX-1162 S3: `appearance.style` glow/shadow/fade
//!   plus the `decoration` tint pair, adopted as one `ChromeStyle` via
//!   `set_chrome_style` right after `set_background_appearance`);
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
use std::path::{Path, PathBuf};
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
    /// Machine-readable kind: `unchanged`, `applied`, `would-apply`,
    /// `restart-required`, `rejected`, `load-error`, or `apply-error`.
    /// `would-apply` appears only for a dry run.
    pub(crate) kind: &'static str,
    /// Whether the running runtime adopted the change (always `false` for a
    /// dry run).
    pub(crate) applied: bool,
    /// Whether this attempt only reported the change list (`bitty reload
    /// --dry-run`).
    pub(crate) dry_run: bool,
    /// Dotted paths that changed (the report's diff fields). For a dry run,
    /// the paths that would change.
    pub(crate) changed: Vec<String>,
    /// Dotted paths the plan validation refused. A non-empty list always
    /// means the running plan was left untouched.
    pub(crate) rejected: Vec<String>,
    /// Changed live-class fields the running runtime has no adopter for yet;
    /// they are recorded and take effect on the next start.
    pub(crate) restart_required: Vec<String>,
    /// Plugin ids whose enabled state in the installed plugin store differs
    /// from the set the running instance launched with (bitty#2969). The
    /// plugin set is not part of the config plan and has no live apply path,
    /// so these stay at their launch generations until a restart.
    pub(crate) plugins_restart_required: Vec<String>,
    /// Detail for `rejected` / `load-error` / `apply-error`; `None`
    /// otherwise.
    pub(crate) message: Option<String>,
    /// The typed `E_CONFIG_MODULE_*` record behind a `load-error`, when the
    /// failure was a configuration module refusal. The running plan is never
    /// partially adopted; this is the contract's typed outcome for the reload
    /// path.
    pub(crate) module: Option<Box<bitty_config::ConfigModuleDiagnostic>>,
    /// Monotonic version of the running plan (bitty#2927). Starts at 1 and
    /// increments each time a reload adopts a changed plan; `unchanged`,
    /// restart-required, rejected, dry-run, and error attempts keep it.
    pub(crate) plan_version: u64,
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
            | "window.background_image"
            | "window.background_fit"
            | "window.background_opacity"
            | "window.background_position"
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
            | "decoration.background_tint"
            | "decoration.background_tint_strength"
            | "appearance.style.glow_color"
            | "appearance.style.glow_width"
            | "appearance.style.shadow_color"
            | "appearance.style.shadow_offset_x"
            | "appearance.style.shadow_offset_y"
            | "appearance.style.fade_unfocused"
            | "appearance.theme"
            | "appearance.colors"
            | "keymaps"
            | "panel_rules"
            | "mod_key"
            | "leader_key"
            | "leader_timeout_ms"
            | "hints_enabled"
            | "layout.resize_step"
            | "workspace.show_bar"
            | "workspace.bar.edge"
            | "terminal.bidi"
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
    /// Resolved Leader binding (`leader_key` / `leader_timeout_ms`, or the
    /// canonical `input.leader` / `input.timeout_len`, CTX-1002).
    pub(crate) leader: bitty_config::ResolvedLeader,
    /// Resolved prefix-sequence bindings (`"leader <second>"` entries,
    /// CTX-1002 / issue #1650).
    pub(crate) prefix_bindings: Vec<bitty_config::PrefixBinding>,
    /// Resolved hint kill switch (`hints_enabled`).
    pub(crate) hints_enabled: bool,
    /// Tiled resize step (`layout.resize_step`, CTX-0963): read at keypress
    /// time, adopted live like the keymaps.
    pub(crate) resize_step: f32,
    /// Effective `mod_key` slot (issue #2407): plugin-suggested `alt`-bearing
    /// chords are re-spelled through it at match time.
    pub(crate) mod_key: bitty_config::ModKey,
    /// Effective `window.opacity` for the platform transparency hint.
    pub(crate) window_opacity: f32,
    /// Resolved theme identity for the window title (CTX-0898): a built-in
    /// preset name or a normalized `local:<name>` reference (CTX-2464). The
    /// title's source-layer suffix is the one from launch: the reload engine
    /// sees only the merged effective config, not layer attribution.
    pub(crate) theme_name: String,
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
    let prefix_bindings = bitty_config::resolve_prefix_bindings(&effective.keymaps)
        .map_err(|err| format!("keymaps: {err}"))?;
    let prefer_light = reload_prefer_light();
    // CTX-2464: a local selection has no static registry name. The title must
    // show the same explicit `local:<name>` reference the config carries, not
    // the silent built-in base used for color resolution.
    let local_reference = effective
        .appearance
        .theme
        .as_deref()
        .and_then(|raw| bitty_config::theme::parse_theme_selection(raw).ok())
        .and_then(|selection| match selection {
            bitty_config::theme::ThemeSelection::Single(name)
                if bitty_config::theme::is_local_theme_reference(&name) =>
            {
                Some(name)
            }
            _ => None,
        });
    let theme_name = local_reference.unwrap_or_else(|| {
        bitty_config::theme::resolve_selection(effective.appearance.theme.as_deref(), prefer_light)
            .name
            .to_string()
    });
    Ok(AppAdoption {
        keymaps,
        leader,
        prefix_bindings,
        hints_enabled: bitty_config::resolve_hint_config(effective).enabled,
        resize_step: effective.layout.resize_step,
        mod_key: effective.mod_key,
        window_opacity: effective.window.opacity,
        theme_name,
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
    // CTX-1612: paint-time BiDi policy (Live; one full redraw).
    runtime.set_bidi(resolved.bidi);
    runtime
        .set_decoration(resolved.decoration)
        .map_err(|err| format!("decoration: {err}"))?;
    runtime.set_outline(resolved.outline_focused, resolved.outline_idle);
    runtime
        .set_outline_widths(resolved.outline_width_focused, resolved.outline_width_idle)
        .map_err(|err| format!("decoration.border_width: {err}"))?;
    // Captured before the background setter moves the appearance fields out
    // of `resolved` below (`ChromeStyle` is `Copy`: eight scalar leaves).
    let chrome_style = resolved.chrome_style();
    runtime
        .set_background_appearance(
            resolved.view_appearance,
            resolved.background_image,
            resolved.background_fit,
            resolved.background_image_roots,
            resolved.window_background_image,
            resolved.window_background_fit,
            resolved.window_background_opacity,
            resolved.window_background_position,
        )
        .map_err(|err| format!("background: {err}"))?;
    // CTX-1162 S3: the eight global chrome-style leaves adopt as one value
    // right after the backgrounds/views set (which owns the per-`View`
    // glow/tint quartet), so a style-only reload repaints on the next tick.
    runtime
        .set_chrome_style(chrome_style)
        .map_err(|err| format!("chrome style: {err}"))?;
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
    // CTX-1080: declarative panel spawn rules adopt live; future spawns
    // evaluate the new set, existing panes keep their stamped state.
    runtime.set_panel_spawn_rules(effective.panel_rules.clone());
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

/// One bounded stat row for a `.toml` candidate in the themes directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeStamp {
    name: String,
    mtime: Option<SystemTime>,
    len: u64,
    /// Symlink target (never followed by the loader; included so retargeting
    /// the link is observed as a change).
    target: Option<PathBuf>,
}

/// Dependency-free poll watcher over the config file and local themes directory.
///
/// Stat-ing paths (rather than holding an open handle) means an atomic-rename
/// rewrite is observed as a change, and the watcher needs no event
/// subscription. The cost is one config `stat` plus one bounded themes scan
/// (at most [`bitty_config::MAX_LOCAL_THEME_DIR_ENTRIES`] entries, with stats
/// for at most [`bitty_config::MAX_LOCAL_THEME_FILES`] `.toml` candidates) per
/// [`CONFIG_POLL_INTERVAL`]. An edit that keeps both the length and the
/// filesystem's mtime granularity unchanged is not observed until the next
/// differing write; `bitty ctl config reload` is the explicit path.
///
/// CTX-2464: the themes directory is watched even when a built-in preset is
/// active, so creating the first local file is observable; edits inside an
/// existing file are observed through its `(mtime, len)` row. The reload
/// re-resolves the whole effective config, so the local palette is adopted
/// atomically by the existing engine and a rejected edit keeps the last good
/// palette.
#[derive(Debug)]
pub(crate) struct ConfigFileWatcher {
    path: Option<PathBuf>,
    theme_dir: Option<PathBuf>,
    last: Option<Stamp>,
    last_themes: Option<Vec<ThemeStamp>>,
    next_check: Instant,
}

impl ConfigFileWatcher {
    /// Start watching one config path (no themes directory).
    #[cfg(test)]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self::new_with_theme_dir(Some(path), None)
    }

    /// Start watching a config path and/or the local themes directory.
    pub(crate) fn new_with_theme_dir(path: Option<PathBuf>, theme_dir: Option<PathBuf>) -> Self {
        let last = path.as_deref().and_then(Self::stamp);
        let last_themes = theme_dir.as_deref().and_then(Self::theme_stamp);
        Self {
            path,
            theme_dir,
            last,
            last_themes,
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

    /// Bounded `.toml` candidate signature, sorted by file name.
    fn theme_stamp(dir: &std::path::Path) -> Option<Vec<ThemeStamp>> {
        // Never traverse a symlinked themes directory (the loader rejects it
        // too); an empty signature still lets creation/replacement be seen.
        let metadata = std::fs::symlink_metadata(dir).ok()?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Some(Vec::new());
        }
        let read_dir = std::fs::read_dir(dir).ok()?;
        let mut rows = Vec::new();
        let mut candidate_count = 0usize;
        // Bound the directory walk itself, then count every `.toml`
        // candidate so an add/remove across the 64-file cap still changes
        // the signature even when only the first 64 rows are stat-ed.
        for entry in read_dir
            .flatten()
            .take(bitty_config::MAX_LOCAL_THEME_DIR_ENTRIES)
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().ends_with(".toml") {
                continue;
            }
            candidate_count += 1;
            if candidate_count > bitty_config::MAX_LOCAL_THEME_FILES {
                continue;
            }
            // `symlink_metadata` never follows a link; the loader rejects
            // symlinked candidates, so the watcher must not traverse them.
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                // An entry that disappears between `read_dir` and `metadata`
                // is a change; keep a deterministic absent row.
                rows.push(ThemeStamp {
                    name,
                    mtime: None,
                    len: u64::MAX,
                    target: None,
                });
                continue;
            };
            rows.push(ThemeStamp {
                name,
                mtime: meta.modified().ok(),
                len: meta.len(),
                target: std::fs::read_link(entry.path()).ok(),
            });
        }
        if candidate_count > bitty_config::MAX_LOCAL_THEME_FILES {
            rows.push(ThemeStamp {
                name: String::from("\u{0}overflow"),
                mtime: None,
                len: candidate_count as u64,
                target: None,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Some(rows)
    }

    /// True when the config file or the local themes directory changed since
    /// the previous check. Creating, removing, or rewriting either kind of
    /// file all count; the baseline seeded by
    /// [`Self::new_with_theme_dir`] means an unchanged state reports `false`
    /// on the first check. Checks are throttled to [`CONFIG_POLL_INTERVAL`]: a
    /// call before the next check time reports `false` without touching the
    /// filesystem.
    pub(crate) fn poll_at(&mut self, at: Instant) -> bool {
        if at < self.next_check {
            return false;
        }
        self.next_check = at + CONFIG_POLL_INTERVAL;
        let now = self.path.as_deref().and_then(Self::stamp);
        let themes_now = self.theme_dir.as_deref().and_then(Self::theme_stamp);
        let changed = now != self.last || themes_now != self.last_themes;
        self.last = now;
        self.last_themes = themes_now;
        changed
    }
}

/// One re-resolved configuration plus the profile identity it was merged from.
///
/// The reload engine adopts the effective config; the identity lets the app
/// keep reads on the profile the running table actually reflects (bitty#2814).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedConfig {
    /// Merged effective config a reload would adopt.
    pub(crate) effective: EffectiveConfig,
    /// Profile the merge used (`--profile`/`BITTY_PROFILE`, else the user
    /// layer's selector); `None` when no profile participated.
    pub(crate) profile: Option<String>,
}

impl From<EffectiveConfig> for ResolvedConfig {
    fn from(effective: EffectiveConfig) -> Self {
        Self {
            effective,
            profile: None,
        }
    }
}

/// The enabled installed-plugin set the instance launched with (bitty#2969).
///
/// `root` is the resolved plugin store the instance discovered at startup;
/// `enabled` is the sorted list of store records that were enabled then. The
/// set is a launch-time baseline only: a later `bitty plugin enable|disable`
/// rewrites the store index, and a reload reports the difference.
#[derive(Debug, Clone)]
struct PluginSetSnapshot {
    /// Resolved plugin store root at launch (`None` when the environment had
    /// no data root, so no store plugin could have been discovered).
    root: Option<PathBuf>,
    /// Sorted, de-duplicated plugin ids enabled in that store at launch.
    enabled: Vec<String>,
}

/// Read the enabled plugin ids from an installed plugin store index.
///
/// An absent index is an empty store (no installed packages), so it reports an
/// empty list. A malformed or unreadable index reports `None`: the caller must
/// not turn an integrity failure into a fabricated plugin-set change.
fn enabled_store_plugins(root: Option<&Path>) -> Option<Vec<String>> {
    let root = root?;
    match bitty_runtime::plugin_runtime::resolution::load_index(root) {
        Ok(records) => {
            let mut enabled: Vec<String> = records
                .into_iter()
                .filter(|record| record.enabled)
                .map(|record| record.plugin_id)
                .collect();
            enabled.sort();
            enabled.dedup();
            Some(enabled)
        }
        Err(error) => {
            crate::logging::warn(|| {
                format!("bitty: plugin store index unreadable; plugin set not compared: {error}")
            });
            None
        }
    }
}

/// Record the enabled plugin set the running instance launched with.
///
/// Called once by `main` after startup plugin discovery, so a later
/// `bitty reload` can compare the installed store against what the instance
/// actually launched with (bitty#2969). A missing reload context (`--safe`,
/// headless paths, tests) or an unreadable index leaves plugin-set tracking
/// off: the reload never invents a change it cannot prove.
pub(crate) fn record_launch_plugin_set() {
    let root = crate::plugin_runtime::store_root();
    let Some(enabled) = enabled_store_plugins(root.as_deref()) else {
        return;
    };
    CONTEXT.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().as_mut() {
            ctx.plugins_launched = Some(PluginSetSnapshot { root, enabled });
        }
    });
}

/// The installed reload context: the engine plus how to re-resolve the config.
struct ReloadContext {
    engine: ReloadEngine,
    /// The config the process started with. Fields without a runtime adopter
    /// still run with these values until restart, so pending restart fields
    /// are always computed against it rather than against the last reload.
    launched: EffectiveConfig,
    /// The enabled plugin set the instance launched with (bitty#2969).
    /// `None` means plugin-set tracking is not installed for this context.
    plugins_launched: Option<PluginSetSnapshot>,
    /// Profile identity the running table reflects (bitty#2814): the startup
    /// chain until a reload is adopted, then the identity that reload was
    /// merged from. A persisted selector switch therefore moves it, while an
    /// explicit `--profile`/`BITTY_PROFILE` keeps it because the resolver keeps
    /// honouring the explicit request.
    profile: Option<String>,
    /// Re-resolve the effective config from the original sources.
    resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>>,
    watcher: Option<ConfigFileWatcher>,
    path_label: String,
    /// Monotonic version of the running plan (bitty#2927): seeded at 1 and
    /// incremented each time a reload adopts a changed plan.
    plan_version: u64,
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
/// user config file (as reported at startup) and becomes the watched path;
/// `profile` is the profile that startup chain loaded, so the running identity
/// is known before the first reload.
pub(crate) fn install(
    args: crate::cli::Args,
    effective: EffectiveConfig,
    file_path: Option<PathBuf>,
    profile: Option<String>,
) {
    // CTX-2464: the local themes directory is watched even without a config
    // file, because `--theme local:<name>` can select a file directly and a
    // directory that does not exist yet becomes observable on creation.
    let theme_dir = bitty_config::local_themes_dir();
    let resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>> =
        Box::new(move || {
            crate::config_cli::load_app_config_typed(&args).map(|app| ResolvedConfig {
                effective: app.effective,
                profile: app.profile_name,
            })
        });
    install_context(effective, profile, resolve, file_path, theme_dir);
    // bitty#2969: the context is installed after startup plugin discovery, so
    // this is the moment the enabled store set the instance runs with is
    // known. A later `bitty plugin enable|disable` rewrites the store index
    // and the reload reports the difference as restart-required.
    record_launch_plugin_set();
}

/// Install the process-wide context (shared by production and tests).
fn install_context(
    effective: EffectiveConfig,
    profile: Option<String>,
    resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>>,
    watch: Option<PathBuf>,
    theme_dir: Option<PathBuf>,
) {
    let path_label = watch.as_ref().map_or_else(
        || String::from("(defaults; no file)"),
        |path| path.display().to_string(),
    );
    let watcher = if watch.is_some() || theme_dir.is_some() {
        Some(ConfigFileWatcher::new_with_theme_dir(watch, theme_dir))
    } else {
        None
    };
    CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(ReloadContext {
            launched: effective.clone(),
            plugins_launched: None,
            engine: ReloadEngine::new(effective),
            profile,
            resolve,
            watcher,
            path_label,
            plan_version: 1,
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
    resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>>,
    watch: Option<PathBuf>,
) {
    install_context(effective, None, resolve, watch, None);
}

/// Install a context with an injected resolver and the profile identity the
/// seeded config was merged from (tests only).
#[cfg(test)]
pub(crate) fn install_with_profile(
    effective: EffectiveConfig,
    profile: Option<String>,
    resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>>,
    watch: Option<PathBuf>,
) {
    install_context(effective, profile, resolve, watch, None);
}

/// Install a context with an injected resolver and an explicit themes
/// directory (tests only).
#[cfg(test)]
pub(crate) fn install_with_theme_dir(
    effective: EffectiveConfig,
    resolve: Box<dyn Fn() -> Result<ResolvedConfig, crate::config_cli::LoadFailure>>,
    watch: Option<PathBuf>,
    theme_dir: Option<PathBuf>,
) {
    install_context(effective, None, resolve, watch, theme_dir);
}

/// Seed the launch-time plugin set for an installed context (tests only).
///
/// Production records it from the environment through
/// [`record_launch_plugin_set`]; tests pass an explicit store root so no
/// developer store is ever read.
#[cfg(test)]
pub(crate) fn set_launch_plugin_set(root: Option<PathBuf>, enabled: Vec<String>) {
    CONTEXT.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().as_mut() {
            ctx.plugins_launched = Some(PluginSetSnapshot { root, enabled });
        }
    });
}

/// Test-only convenience: a real (non-dry-run) reload against the installed
/// context.
///
/// Returns `None` when no context is installed, so the caller keeps its
/// probe-only response (headless/test paths never install a context).
#[cfg(test)]
pub(crate) fn reload_requested(runtime: &mut bitty_runtime::Runtime) -> Option<ReloadOutcomeInfo> {
    reload_requested_with(runtime, false)
}

/// The reload path with the `bitty reload --dry-run` flag (bitty#2927).
///
/// A dry run re-resolves and classifies the config exactly like a real
/// reload, but never commits the plan and never touches the runtime, so the
/// reported change list is what the next real reload would adopt.
pub(crate) fn reload_requested_with(
    runtime: &mut bitty_runtime::Runtime,
    dry_run: bool,
) -> Option<ReloadOutcomeInfo> {
    CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let ctx = slot.as_mut()?;
        Some(ctx.reload_into(runtime, dry_run))
    })
}

/// Maximum bytes accepted for `core.config.reload` params.
pub(crate) const MAX_RELOAD_PARAMS_BYTES: usize = 256;

/// Parse `core.config.reload` params: absent/`{}`/`null` mean a real reload;
/// `{"dry_run":true}` reports without applying.
///
/// Bounded and strict: the params object is flat, carries only `dry_run`, and
/// any other shape is an `InvalidParams` usage error (never silently ignored).
pub(crate) fn parse_reload_params(raw: Option<&str>) -> Result<bool, String> {
    let Some(raw) = raw else {
        return Ok(false);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed == "null" {
        return Ok(false);
    }
    if trimmed.len() > MAX_RELOAD_PARAMS_BYTES {
        return Err(format!(
            "reload params too large ({} > {MAX_RELOAD_PARAMS_BYTES} bytes)",
            trimmed.len()
        ));
    }
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .map(str::trim)
        .ok_or_else(|| String::from("reload params must be a JSON object"))?;
    let (key, value) = inner
        .split_once(':')
        .ok_or_else(|| String::from("reload params must carry a `dry_run` boolean"))?;
    if key.trim().trim_matches('"') != "dry_run" {
        return Err(String::from(
            "reload params accept only the `dry_run` boolean",
        ));
    }
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "reload params.dry_run must be true or false (got {other:?})"
        )),
    }
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

/// The profile the running table was merged from (bitty#2814).
///
/// The outer option reports whether a reload context is installed: `None`
/// means there is none (unit tests, headless paths, `--safe`), so the caller
/// keeps its startup identity. `Some(None)` is a real state — the running
/// config was merged without any profile. A context borrowed by an in-flight
/// reload or classification also reports `None`, so a caller falls back to its
/// startup identity instead of panicking.
pub(crate) fn running_profile() -> Option<Option<String>> {
    CONTEXT.with(|slot| {
        slot.try_borrow()
            .ok()
            .and_then(|slot| slot.as_ref().map(|ctx| ctx.profile.clone()))
    })
}

/// Classify a profile switch the app has just persisted (bitty#2814).
///
/// `AppConfigManage` writes the selector and the file watcher resolves the same
/// sources on its next tick. Running that resolution here — the installed
/// resolver, so exactly the watcher's code — and diffing it against the
/// committed running config answers whether the switch is live-only.
///
/// This is classification only. The running profile identity moves in
/// [`ReloadContext::reload_into`] when the watcher actually adopts the
/// resolution (and a rolled-back adopt restores it), so a switch that is
/// refused or fails to apply can never leave `profiles.list.active`,
/// `config.get`, or `config.sources` naming a profile the runtime does not
/// reflect (bitty#2848 review).
///
/// Returns `pending_restart`:
///
/// * `false` when every differing field is live-class and adopted *and* the
///   resolution selected `requested`, i.e. the next tick will adopt exactly
///   this switch.
/// * `true` when a restart-class (or otherwise unadoptable) field differs, the
///   resolver fails, no context is installed, or the resolution selected a
///   different profile. That last arm covers a one-launch
///   `--profile`/`BITTY_PROFILE` override: the resolver keeps honoring it, so
///   the selector write never merges while the override holds, and the reply
///   must not claim a switch the running table cannot make (bitty#2848
///   review).
///
/// The reload context and the plugin runtime both live on the main thread (the
/// app installs `AppConfigManage` as an `Rc` in that runtime), so reaching this
/// thread-local from the backend is sound. A resolver that re-entered this
/// module would find the context borrowed; `try_borrow_mut` turns that into the
/// conservative `true` instead of a panic.
///
/// Cost: this resolves the whole config (user layer plus profile chain) on the
/// plugin call path. The 10 ms budget charged before the backend covers
/// authorization only, not this classification; the write head stays
/// approval-gated and rate-limited to 16 calls/s.
pub(crate) fn classify_profile_switch(requested: &str) -> bool {
    CONTEXT.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return true;
        };
        let Some(ctx) = slot.as_mut() else {
            return true;
        };
        let Ok(resolved) = (ctx.resolve)() else {
            return true;
        };
        // The override (or a selector that lost the race) means the persisted
        // switch is not what the running table is about to become.
        if resolved.profile.as_deref() != Some(requested) {
            return true;
        }
        !pending_fields(ctx.engine.current(), &resolved.effective, runtime_adopts).is_empty()
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
        let info = ctx.reload_into(runtime, false);
        // A load/admin error is user-actionable: the rejected edit (including
        // a rejected local theme) leaves the last good palette live, so the
        // diagnostic must not be hidden behind the info gate.
        if let Some(message) = &info.message {
            let message = message.clone();
            crate::logging::warn(move || format!("bitty: config/theme reload failed: {message}"));
        }
        crate::logging::info(|| {
            format!(
                "bitty: config/theme source changed -> reload {} (applied={})",
                info.kind, info.applied
            )
        });
        Some(info)
    })
}

/// Whether one classified field difference is adopted live by the runtime.
///
/// The single inversion point shared by both restart-pending classifications,
/// so a new [`bitty_config::ReloadClass`] cannot drift between them.
fn adopted_live(class: bitty_config::ReloadClass, field: &str, adopts: fn(&str) -> bool) -> bool {
    class == bitty_config::ReloadClass::Live && adopts(field)
}

/// Fields that differ and that the running runtime would not adopt live: the
/// restart-pending set.
///
/// `launched` against the committed `current` is the ctl `restart_required`
/// case (fields the runtime could not adopt stay at their launch values);
/// `running` against a prospective config is the profile-switch classification
/// (bitty#2814). `adopts` is injected so the filter is testable; production
/// passes [`runtime_adopts`].
fn pending_fields(
    launched: &EffectiveConfig,
    current: &EffectiveConfig,
    adopts: fn(&str) -> bool,
) -> Vec<String> {
    bitty_config::diff(launched, current)
        .diffs
        .into_iter()
        .filter(|diff| !adopted_live(diff.class, &diff.field, adopts))
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

    /// Plugin ids whose enabled state in the installed store differs from the
    /// set the instance launched with (bitty#2969).
    ///
    /// Empty when plugin-set tracking is not installed (tests, `--safe`) or
    /// when the store index cannot be read: an integrity failure must never
    /// become a fabricated plugin-set change.
    fn pending_plugin_set(&self) -> Vec<String> {
        let Some(launched) = self.plugins_launched.as_ref() else {
            return Vec::new();
        };
        let Some(current) = enabled_store_plugins(launched.root.as_deref()) else {
            return Vec::new();
        };
        let launched: std::collections::BTreeSet<&str> =
            launched.enabled.iter().map(String::as_str).collect();
        let current: std::collections::BTreeSet<&str> =
            current.iter().map(String::as_str).collect();
        launched
            .symmetric_difference(&current)
            .map(|id| (*id).to_string())
            .collect()
    }

    /// Re-resolve, classify, adopt if live, and build the caller-visible info.
    ///
    /// `dry_run` re-resolves and classifies but never commits the plan or
    /// touches the runtime (bitty#2927), so `bitty reload --dry-run` reports
    /// exactly what the next real reload would adopt.
    fn reload_into(
        &mut self,
        runtime: &mut bitty_runtime::Runtime,
        dry_run: bool,
    ) -> ReloadOutcomeInfo {
        let resolved = match (self.resolve)() {
            Ok(resolved) => resolved,
            Err(failure) => {
                let rejected = failure.field().map(str::to_string).into_iter().collect();
                let (message, module) = failure.into_parts();
                return ReloadOutcomeInfo {
                    path: self.path_label.clone(),
                    kind: "load-error",
                    applied: false,
                    dry_run,
                    changed: Vec::new(),
                    rejected,
                    restart_required: Vec::new(),
                    plugins_restart_required: Vec::new(),
                    message: Some(message),
                    module,
                    plan_version: self.plan_version,
                };
            }
        };
        if dry_run {
            return self.dry_run_info(resolved.effective);
        }
        let previous = self.engine.current().clone();
        let previous_profile = self.profile.clone();
        let outcome = self.engine.reload(resolved.effective);
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
                // The rollback restores the running table, so the profile
                // identity must not claim the resolution either (bitty#2814).
                self.profile = previous_profile;
                return ReloadOutcomeInfo {
                    path: self.path_label.clone(),
                    kind: "apply-error",
                    applied: false,
                    dry_run: false,
                    changed: Vec::new(),
                    rejected: Vec::new(),
                    restart_required: Vec::new(),
                    plugins_restart_required: Vec::new(),
                    message: Some(message),
                    module: None,
                    plan_version: self.plan_version,
                };
            }
            // The running plan adopted a changed config: the plan version
            // advances. Unchanged, restart-required, rejected, and failed
            // attempts keep it.
            self.plan_version = self.plan_version.saturating_add(1);
        }
        // Committed (or nothing to adopt): drop the replaced background
        // generation so at most one decoded generation stays resident.
        runtime.release_retained_backgrounds();
        // An adopted resolution moved the running table (even when nothing
        // differed and only the selector identity changed), so reads follow
        // it; a rejected or restart-required resolution kept the old table and
        // therefore the old identity (bitty#2814).
        if applied {
            self.profile = resolved.profile;
        }
        let report = outcome.report();
        // A refused plan changed nothing: rejected paths travel in `rejected`,
        // never in `changed`.
        let changed: Vec<String> = report
            .diffs
            .iter()
            .filter(|diff| diff.class != bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.field.clone())
            .collect();
        let rejected: Vec<String> = report
            .diffs
            .iter()
            .filter(|diff| diff.class == bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.field.clone())
            .collect();
        // A rejected plan names its reason in the diff's `after` column.
        let message = report
            .diffs
            .iter()
            .find(|diff| diff.class == bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.after.clone());
        // Restart-pending set: fields the running plan cannot adopt live. For
        // an accepted restart-required diff the engine keeps the previous
        // plan, so the launch-vs-current list alone would be empty; add the
        // refused-to-adopt fields from this report so the reply names them
        // (bitty#2927: never silently ignored).
        let mut restart_required = self.pending_restart_fields();
        if matches!(outcome, ReloadOutcome::RestartRequired(_)) {
            for diff in &report.diffs {
                if !adopted_live(diff.class, &diff.field, runtime_adopts)
                    && !restart_required.contains(&diff.field)
                {
                    restart_required.push(diff.field.clone());
                }
            }
        }
        // bitty#2969: the installed plugin set is a separate resource from the
        // config plan and has no live apply path, so a store change the
        // instance launched without is restart-required, never a silent
        // no-op. A config `unchanged` attempt that only carries a pending
        // plugin set reports `restart-required` (and never claims an apply).
        let plugins_restart_required = self.pending_plugin_set();
        let (kind, applied) = if !plugins_restart_required.is_empty()
            && matches!(outcome, ReloadOutcome::Unchanged(_))
        {
            ("restart-required", false)
        } else {
            (outcome.kind(), applied)
        };
        ReloadOutcomeInfo {
            path: self.path_label.clone(),
            kind,
            applied,
            dry_run: false,
            changed,
            rejected,
            restart_required,
            plugins_restart_required,
            message,
            module: None,
            plan_version: self.plan_version,
        }
    }

    /// Classify a prospective resolution without committing it (dry run).
    ///
    /// Uses the same [`bitty_config::diff`] and [`pending_fields`] policy as
    /// the real path, so the reported change list matches the next apply.
    fn dry_run_info(&self, incoming: EffectiveConfig) -> ReloadOutcomeInfo {
        let report = bitty_config::diff(self.engine.current(), &incoming);
        // A refused plan would change nothing: rejected paths travel in
        // `rejected`, never in `changed`.
        let changed: Vec<String> = report
            .diffs
            .iter()
            .filter(|diff| diff.class != bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.field.clone())
            .collect();
        let rejected: Vec<String> = report
            .diffs
            .iter()
            .filter(|diff| diff.class == bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.field.clone())
            .collect();
        let message = report
            .diffs
            .iter()
            .find(|diff| diff.class == bitty_config::ReloadClass::Rejected)
            .map(|diff| diff.after.clone());
        // A rejected plan never reaches the restart check: the running plan
        // is untouched, so nothing is pending.
        let restart_required = if rejected.is_empty() {
            pending_fields(&self.launched, &incoming, runtime_adopts)
        } else {
            Vec::new()
        };
        // bitty#2969: report a pending plugin-set change without applying
        // anything, exactly like the real path will.
        let plugins_restart_required = if rejected.is_empty() {
            self.pending_plugin_set()
        } else {
            Vec::new()
        };
        let kind = if !rejected.is_empty() {
            "rejected"
        } else if !restart_required.is_empty() || !plugins_restart_required.is_empty() {
            "restart-required"
        } else if changed.is_empty() {
            "unchanged"
        } else {
            "would-apply"
        };
        ReloadOutcomeInfo {
            path: self.path_label.clone(),
            kind,
            applied: false,
            dry_run: true,
            changed,
            rejected,
            restart_required,
            plugins_restart_required,
            message,
            module: None,
            plan_version: self.plan_version,
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
        //
        // CTX-1162 S3 closes the CTX-1159 S0 carve-out (DEC-0113 Fix 3): the
        // eight style leaves are adopted live by `set_chrome_style` (driven
        // from `apply_live_presentation`), so every Live leaf now names its
        // adopter and a style-only reload reports nothing pending.
        for field in bitty_config::LIVE_FIELDS {
            assert_eq!(
                bitty_config::classify_field(field),
                ReloadClass::Live,
                "{field} is listed Live but classifies otherwise"
            );
            assert!(runtime_adopts(field), "{field} has no live adopter");
        }
        // A style-only edit is fully adopted: nothing stays pending.
        let baseline = bitty_config::fallback_builtin();
        let mut styled = baseline.clone();
        styled.appearance.style = Some(bitty_config::types::StyleConfig {
            glow_width: Some(8),
            ..Default::default()
        });
        styled.decoration.background_tint_strength = Some(0.5);
        assert!(
            pending_fields(&baseline, &styled, runtime_adopts).is_empty(),
            "style leaves must adopt live, got {:?}",
            pending_fields(&baseline, &styled, runtime_adopts)
        );
        let per_view = format!("{}*]", bitty_config::VIEWS_FIELD_PREFIX);
        assert_eq!(bitty_config::classify_field(&per_view), ReloadClass::Live);
        assert!(runtime_adopts(&per_view), "{per_view} has no live adopter");
        for field in bitty_config::RESTART_REQUIRED_FIELDS {
            assert!(!runtime_adopts(field), "{field} must stay restart-required");
        }
    }

    #[test]
    fn apply_live_presentation_adopts_chrome_style() {
        // CTX-1162 S3 apply path: a style-carrying effective config lands on
        // the runtime through `set_chrome_style` (globals) while the
        // backgrounds/views setter keeps owning the per-`View` quartet.
        let mut runtime = bitty_runtime::Runtime::new(bitty_runtime::RuntimeConfig::default())
            .expect("default runtime builds");
        assert!(runtime.config().chrome_style().is_inert());
        let mut effective = bitty_config::fallback_builtin();
        effective.appearance.style = Some(bitty_config::types::StyleConfig {
            glow_color: bitty_config::types::OutlineColor::parse("#FF0000"),
            glow_width: Some(6),
            shadow_color: bitty_config::types::OutlineColor::parse("#00000080"),
            shadow_offset_x: Some(4),
            shadow_offset_y: Some(-4),
            fade_unfocused: Some(0.5),
        });
        effective.decoration.background_tint = bitty_config::types::OutlineColor::parse("#112233");
        effective.decoration.background_tint_strength = Some(0.25);
        apply_live_presentation(&mut runtime, &effective).expect("style adopts live");
        let adopted = runtime.config().chrome_style();
        assert_eq!(adopted.glow_color, Some([0xFF, 0x00, 0x00, 0xFF]));
        assert_eq!(adopted.glow_width, 6);
        assert_eq!(adopted.shadow_color, Some([0x00, 0x00, 0x00, 0x80]));
        assert_eq!(adopted.shadow_offset_x, 4);
        assert_eq!(adopted.shadow_offset_y, -4);
        assert!((adopted.fade_unfocused - 0.5).abs() < f32::EPSILON);
        assert_eq!(adopted.background_tint, Some([0x11, 0x22, 0x33, 0xFF]));
        assert!((adopted.background_tint_strength - 0.25).abs() < f32::EPSILON);
        assert!(!adopted.is_inert());
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
        all.window.background_image = Some(
            std::env::temp_dir()
                .join("bitty-wall.png")
                .display()
                .to_string(),
        );
        all.window.background_fit = Some(bitty_config::types::BackgroundFit::Tile);
        all.window.background_opacity = Some(0.5);
        all.window.background_position = Some(bitty_config::types::BackgroundPosition::TopLeft);
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
            "terminal.page_scroll_percent",
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
        let outline_before_focused = runtime.config().outline_focused;
        let outline_before_idle = runtime.config().outline_idle;
        let theme_outline_before_focused = runtime.config().theme.border_focused;
        let theme_outline_before_idle = runtime.config().theme.border_idle;

        let mut custom = light.clone();
        let preset = bitty_config::theme::resolve_theme(None);
        let mut ansi = preset.ansi;
        for (index, entry) in ansi.iter_mut().enumerate() {
            *entry = [0x10 + index as u8, 0x40, 0x80];
        }
        let palette = bitty_config::theme::CustomPalette {
            background: [0x10, 0x20, 0x30],
            foreground: [0xE0, 0xE0, 0xE0],
            cursor: [0xFF, 0x2E, 0x88],
            selection: [0x24, 0x30, 0x4A],
            ansi,
            border_focused: None,
            border_idle: None,
            category: None,
        };
        custom.appearance.colors = Some(palette);
        apply_live(&mut runtime, &custom).expect("adopt colors");
        let expected = bitty_runtime::ThemePalette::from_custom(&palette);
        let live_theme = runtime.config().theme;
        assert_eq!(
            live_theme.background, expected.background,
            "custom background reaches the live renderer palette"
        );
        assert_eq!(live_theme.foreground, expected.foreground);
        assert_eq!(live_theme.cursor, expected.cursor);
        assert_eq!(live_theme.selection, expected.selection);
        assert_eq!(live_theme.ansi, expected.ansi);
        assert_eq!(
            live_theme.border_focused, theme_outline_before_focused,
            "custom colors must not rewrite the preset-owned focused outline token"
        );
        assert_eq!(
            live_theme.border_idle, theme_outline_before_idle,
            "custom colors must not rewrite the preset-owned idle outline token"
        );
        assert_eq!(
            runtime.active_background(),
            [0x10, 0x20, 0x30, 0xFF],
            "appearance.colors replaces the preset palette"
        );
        // Custom palettes own only the terminal roles; the outline pair keeps
        // whatever the selected configuration resolved (documented
        // derivation), so a custom table cannot silently rewrite chrome
        // tokens it does not declare.
        assert_eq!(
            runtime.config().outline_focused,
            outline_before_focused,
            "custom colors must not change the focused outline"
        );
        assert_eq!(
            runtime.config().outline_idle,
            outline_before_idle,
            "custom colors must not change the idle outline"
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
            Box::new(move || Ok(resolved.clone().into())),
            None,
        );
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "apply-error", "{info:?}");
        assert!(!info.applied);
        assert_eq!(
            running_profile(),
            Some(None),
            "a rolled-back adopt restores the running identity (bitty#2814)"
        );
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
            Box::new(move || Ok(resolved.clone().into())),
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
            Ok(config.into())
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
            Ok(config.into())
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
            Box::new(move || Ok(resolve_live.clone().into())),
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
        install_with(
            baseline,
            Box::new(|| {
                Err(crate::config_cli::LoadFailure::recoverable(String::from(
                    "boom",
                )))
            }),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error");
        assert!(!info.applied);
        assert_eq!(info.message.as_deref(), Some("boom"));
        clear();
    }

    #[test]
    fn broken_reload_retains_the_last_good_plan() {
        // CTX-1339 (#2225): startup fell back to built-ins; a later broken
        // reload must keep that last-good plan instead of replacing it.
        let mut baseline = bitty_config::EffectiveConfig::default();
        baseline.font.size = 13.5;
        clear();
        install_with(
            baseline.clone(),
            Box::new(|| {
                Err(crate::config_cli::LoadFailure::recoverable(String::from(
                    "bitty: invalid config file '/xdg/bitty/init.lua': syntax error",
                )))
            }),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error");
        assert!(!info.applied);
        assert_eq!(
            current_effective(),
            Some(baseline),
            "a broken reload must retain the last good plan"
        );
        clear();
    }

    #[test]
    fn module_failure_on_reload_is_refused_and_keeps_the_last_good_plan() {
        // bitty#2869: a loader refusal is a strict failure on the reload path.
        // The generation is re-evaluated from disk, so a module edit is seen,
        // but a refusal at any point leaves the running plan untouched.
        let dir = temp_dir("reload-module");
        std::fs::create_dir_all(dir.join("lua")).expect("module root");
        std::fs::write(
            dir.join("lua").join("theme.lua"),
            "return { theme = 'tokyo-night' }\n",
        )
        .expect("write module");
        let path = dir.join("init.lua");
        std::fs::write(
            &path,
            "return { theme = require('theme').theme, font = { family = \"Mono\", size = 13.0 } }\n",
        )
        .expect("write valid config");
        let mut args = crate::cli::Args::new();
        args.config_path = Some(path.to_string_lossy().into_owned());
        let baseline = crate::config_cli::load_app_config(&args)
            .expect("a module graph loads")
            .effective;
        let resolver_args = args.clone();
        clear();
        install_with(
            baseline,
            Box::new(move || {
                crate::config_cli::load_app_config_typed(&resolver_args).map(|app| {
                    crate::config_reload::ResolvedConfig {
                        effective: app.effective,
                        profile: app.profile_name,
                    }
                })
            }),
            Some(path.clone()),
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let launched = runtime.config().clone();

        std::fs::write(&path, "return { theme = require('gone').theme }\n")
            .expect("write refusing config");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error", "{info:?}");
        assert!(!info.applied);
        let message = info.message.as_deref().unwrap_or_default();
        assert!(message.contains("E_CONFIG_MODULE_NOT_FOUND"), "{message}");
        assert!(message.contains("module_spec=\"gone\""), "{message}");
        // Typed-outcome parity: the reload result carries the same record the
        // other three paths report, not only the flattened message.
        let diagnostic = info.module.as_deref().expect("typed module record");
        assert_eq!(diagnostic.code().as_str(), "E_CONFIG_MODULE_NOT_FOUND");
        assert_eq!(diagnostic.phase.as_str(), "resolve");
        assert_eq!(diagnostic.module_spec.as_deref(), Some(b"gone".as_slice()));
        assert!(!message.contains("lua/gone.lua"), "{message}");
        assert!(!message.contains("dev="), "{message}");
        // The previous plan is still the running plan.
        assert_eq!(runtime.config().font_size, launched.font_size);

        // A module that raises keeps its message in `detail` but never echoes
        // the module body: a secret-shaped constant on another line must not
        // reach the reload result, and neither must the module file path.
        std::fs::write(
            dir.join("lua/boom.lua"),
            "local token = 'sk-live-DO-NOT-ECHO'\nerror('module exploded')\n",
        )
        .expect("write raising module");
        std::fs::write(&path, "return { theme = require('boom').theme }\n")
            .expect("write raising config");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error", "{info:?}");
        let message = info.message.as_deref().unwrap_or_default();
        let diagnostic = info.module.as_deref().expect("typed module record");
        assert_eq!(diagnostic.code().as_str(), "E_CONFIG_MODULE_EVAL");
        assert!(message.contains("module_spec=\"boom\""), "{message}");
        assert!(message.contains("module exploded"), "{message}");
        assert!(!message.contains("sk-live-DO-NOT-ECHO"), "{message}");
        assert!(!message.contains("lua/boom.lua"), "{message}");
        assert_eq!(runtime.config().font_size, launched.font_size);
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_config_edit_keeps_the_last_good_plan() {
        // CTX-1490 (issue #2580): live reload stays strict. An invalid edit
        // must report `load-error` and keep the last-good plan instead of
        // silently dropping to defaults through startup salvage.
        let dir = temp_dir("lastgood-lua");
        let path = dir.join("init.lua");
        std::fs::write(
            &path,
            "return { font = { family = \"Mono\", size = 13.0 } }\n",
        )
        .expect("write valid config");
        let mut args = crate::cli::Args::new();
        args.config_path = Some(path.to_string_lossy().into_owned());
        let baseline = crate::config_cli::load_app_config(&args)
            .expect("valid config loads")
            .effective;
        let resolver_args = args.clone();
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || {
                crate::config_cli::load_app_config_typed(&resolver_args).map(|app| {
                    crate::config_reload::ResolvedConfig {
                        effective: app.effective,
                        profile: app.profile_name,
                    }
                })
            }),
            Some(path.clone()),
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");

        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "unchanged");

        std::fs::write(&path, "return { window = { opacity = 2.0 } }\n")
            .expect("write invalid config");
        let info = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(info.kind, "load-error");
        assert!(!info.applied);
        assert!(
            info.message
                .as_deref()
                .unwrap_or_default()
                .contains("window.opacity"),
            "reload message must name the field: {:?}",
            info.message
        );
        assert_eq!(
            current_effective(),
            Some(baseline.clone()),
            "an invalid edit must retain the last good plan"
        );
        // bitty#2927: the rejected path is structural and the running plan
        // version never advances for a refused edit.
        assert_eq!(info.rejected, vec![String::from("window.opacity")]);
        assert_eq!(info.plan_version, 1);
        assert!(!info.dry_run);
        clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_reports_the_change_list_without_applying_or_bumping_the_version() {
        // bitty#2927: `bitty reload --dry-run` must classify exactly like the
        // real path but leave the runtime and the plan version untouched.
        let dir = temp_dir("dry-run");
        let path = dir.join("init.lua");
        std::fs::write(
            &path,
            "return { font = { family = \"Mono\", size = 13.0 } }\n",
        )
        .expect("write valid config");
        let mut args = crate::cli::Args::new();
        args.config_path = Some(path.to_string_lossy().into_owned());
        let baseline = crate::config_cli::load_app_config(&args)
            .expect("valid config loads")
            .effective;
        let resolver_args = args.clone();
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || {
                crate::config_cli::load_app_config_typed(&resolver_args).map(|app| {
                    crate::config_reload::ResolvedConfig {
                        effective: app.effective,
                        profile: app.profile_name,
                    }
                })
            }),
            Some(path.clone()),
        );
        let cfg =
            crate::config_cli::runtime_config_from_effective(&baseline).expect("runtime config");
        let mut runtime =
            bitty_runtime::Runtime::with_deterministic_rasterizer(cfg).expect("runtime");

        std::fs::write(
            &path,
            "return { font = { family = \"Mono\", size = 15.5 } }\n",
        )
        .expect("write changed config");
        let preview = reload_requested_with(&mut runtime, true).expect("context installed");
        assert_eq!(preview.kind, "would-apply", "{preview:?}");
        assert!(!preview.applied);
        assert!(preview.dry_run);
        assert_eq!(preview.changed, vec![String::from("font.size")]);
        assert!(preview.rejected.is_empty());
        assert_eq!(preview.plan_version, 1);
        assert!(
            (runtime.config().font_size - 13.0).abs() < f32::EPSILON,
            "a dry run must not touch the runtime"
        );
        assert_eq!(current_effective(), Some(baseline));

        // The same change through the real path applies and advances the
        // plan version the dry run already reported.
        let applied = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(applied.kind, "applied", "{applied:?}");
        assert!(applied.applied);
        assert!(!applied.dry_run);
        assert_eq!(applied.plan_version, 2);
        assert!((runtime.config().font_size - 15.5).abs() < f32::EPSILON);
        clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_params_are_bounded_and_strict() {
        assert!(!parse_reload_params(None).expect("absent params"));
        assert!(!parse_reload_params(Some("{}")).expect("empty object"));
        assert!(!parse_reload_params(Some(" null ")).expect("null params"));
        assert!(!parse_reload_params(Some("{\"dry_run\":false}")).expect("explicit false"));
        assert!(parse_reload_params(Some("{\"dry_run\":true}")).expect("dry run"));
        assert!(parse_reload_params(Some(" { \"dry_run\" : true } ")).expect("whitespace"));
        for bad in [
            "[]",
            "{\"dry_run\":1}",
            "{\"other\":true}",
            "{\"dry_run\":true,\"x\":1}",
        ] {
            assert!(parse_reload_params(Some(bad)).is_err(), "{bad:?} must fail");
        }
        let oversized = format!(
            "{{\"dry_run\":{}}}",
            "true ".repeat(MAX_RELOAD_PARAMS_BYTES)
        );
        assert!(parse_reload_params(Some(&oversized)).is_err());
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
        install_with(
            baseline,
            Box::new(move || Ok(resolve_live.clone().into())),
            None,
        );
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
    fn classify_profile_switch_is_conservative_without_a_context() {
        // bitty#2814: a headless/test path cannot classify the switch, so it
        // must report a restart instead of guessing.
        clear();
        assert!(classify_profile_switch("work"));
        assert!(running_profile().is_none(), "no context, no identity");
    }

    #[test]
    fn classify_profile_switch_reports_live_without_adopting_the_identity() {
        // bitty#2848 review: classification must not move the running identity;
        // only an adopted reload does (see `adopted_reload_moves_the_running_profile`).
        let baseline = bitty_config::fallback_builtin();
        let mut live = baseline.clone();
        live.font.size = baseline.font.size + 1.0;
        clear();
        install_with_profile(
            baseline,
            Some(String::from("default")),
            Box::new(move || {
                Ok(ResolvedConfig {
                    effective: live.clone(),
                    profile: Some(String::from("work")),
                })
            }),
            None,
        );
        assert!(!classify_profile_switch("work"), "font.size is live-class");
        assert_eq!(
            running_profile(),
            Some(Some(String::from("default"))),
            "nothing was adopted yet"
        );
        clear();
    }

    #[test]
    fn classify_profile_switch_reports_restart_for_a_restart_diff() {
        let baseline = bitty_config::fallback_builtin();
        let mut live = baseline.clone();
        live.terminal.scrollback = baseline.terminal.scrollback.saturating_add(1);
        clear();
        install_with_profile(
            baseline,
            Some(String::from("default")),
            Box::new(move || {
                Ok(ResolvedConfig {
                    effective: live.clone(),
                    profile: Some(String::from("work")),
                })
            }),
            None,
        );
        assert!(
            classify_profile_switch("work"),
            "terminal.scrollback is restart-required"
        );
        assert_eq!(
            running_profile(),
            Some(Some(String::from("default"))),
            "the running table did not move"
        );
        clear();
    }

    #[test]
    fn classify_profile_switch_reports_restart_for_another_profile() {
        // bitty#2848 review: the resolution selecting a different profile than
        // the requested one (a one-launch `--profile`/`BITTY_PROFILE` override)
        // means the switch cannot reach the running table, even when the merged
        // config is identical.
        let baseline = bitty_config::fallback_builtin();
        clear();
        install_with_profile(
            baseline.clone(),
            Some(String::from("work")),
            Box::new(move || {
                Ok(ResolvedConfig {
                    effective: baseline.clone(),
                    profile: Some(String::from("work")),
                })
            }),
            None,
        );
        assert!(
            classify_profile_switch("default"),
            "an explicit override keeps the running table"
        );
        clear();
    }

    #[test]
    fn classify_profile_switch_reports_restart_when_the_resolver_fails() {
        clear();
        install_with_profile(
            bitty_config::fallback_builtin(),
            Some(String::from("default")),
            Box::new(|| {
                Err(crate::config_cli::LoadFailure::recoverable(String::from(
                    "boom",
                )))
            }),
            None,
        );
        assert!(classify_profile_switch("work"));
        assert_eq!(running_profile(), Some(Some(String::from("default"))));
        clear();
    }

    #[test]
    fn adopted_reload_moves_the_running_profile() {
        // bitty#2814: the profile identity follows an adopted resolution (even
        // when only the selector changed) and stays put when the resolution is
        // not adopted.
        let baseline = bitty_config::fallback_builtin();
        let mut live = baseline.clone();
        live.font.size = baseline.font.size + 1.0;
        clear();
        install_with_profile(
            baseline.clone(),
            Some(String::from("default")),
            Box::new(move || {
                Ok(ResolvedConfig {
                    effective: live.clone(),
                    profile: Some(String::from("work")),
                })
            }),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        assert_eq!(
            reload_requested(&mut runtime).expect("context").kind,
            "applied"
        );
        assert_eq!(running_profile(), Some(Some(String::from("work"))));

        let mut restart = baseline.clone();
        restart.terminal.scrollback = baseline.terminal.scrollback.saturating_add(1);
        clear();
        install_with_profile(
            baseline,
            Some(String::from("default")),
            Box::new(move || {
                Ok(ResolvedConfig {
                    effective: restart.clone(),
                    profile: Some(String::from("work")),
                })
            }),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        assert_eq!(
            reload_requested(&mut runtime).expect("context").kind,
            "restart-required"
        );
        assert_eq!(running_profile(), Some(Some(String::from("default"))));
        clear();
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
    fn write_local_theme(path: &std::path::Path, background: &str, marker: &str) {
        let ansi = [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
        ]
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ");
        std::fs::write(
            path,
            format!(
                "name = \"sunset\"\ncategory = \"dark\"\n\
                 background = \"{background}\"\nforeground = \"#cdd6f4\"\n\
                 cursor = \"#f5e0dc\"\nselection = \"#313244\"\nansi = [{ansi}]\n{marker}"
            ),
        )
        .expect("write local theme");
    }

    #[test]
    fn local_theme_file_edit_reloads_and_invalid_edit_keeps_last_good_palette() {
        let dir = temp_dir("local-theme");
        let theme_dir = dir.join("themes");
        std::fs::create_dir_all(&theme_dir).expect("themes dir");
        let path = theme_dir.join("sunset.toml");
        write_local_theme(&path, "#1e1e2e", "");

        let mut baseline = bitty_config::fallback_builtin();
        baseline.appearance.theme = Some(String::from("local:sunset"));
        bitty_config::local_theme::apply_selection_from_dir(&mut baseline, &theme_dir)
            .expect("baseline local palette")
            .expect("local selection");

        let resolver_dir = theme_dir.clone();
        let resolver = Box::new(move || {
            let mut config = bitty_config::fallback_builtin();
            config.appearance.theme = Some(String::from("local:sunset"));
            bitty_config::local_theme::apply_selection_from_dir(&mut config, &resolver_dir)
                .map_err(crate::config_cli::LoadFailure::recoverable)?;
            Ok(config.into())
        });
        clear();
        install_with_theme_dir(baseline.clone(), resolver, None, Some(theme_dir.clone()));
        let mut runtime = deterministic_runtime(&baseline);
        let start = Instant::now();
        assert!(
            poll_file_at(&mut runtime, start).is_none(),
            "unchanged theme file -> no reload"
        );

        // A valid edit is reparsed and adopted through the existing theme
        // palette setter on the next config tick.
        write_local_theme(&path, "#101018", "\n# first edit\n");
        let info = poll_file_at(&mut runtime, start + CONFIG_POLL_INTERVAL)
            .expect("valid local edit observed");
        assert_eq!(info.kind, "applied");
        assert_eq!(runtime.config().theme.background, [0x10, 0x10, 0x18, 0xFF]);

        // An invalid edit is rejected; the last good palette stays live and
        // the reload reports a diagnostic instead of committing a partial
        // palette.
        std::fs::write(&path, "name = \"sunset\"\n").expect("break theme");
        let info = poll_file_at(&mut runtime, start + CONFIG_POLL_INTERVAL * 2)
            .expect("invalid local edit observed");
        assert_eq!(info.kind, "load-error");
        assert!(
            info.message
                .as_deref()
                .is_some_and(|message| message.contains("missing required field")),
            "{info:?}"
        );
        assert_eq!(
            runtime.config().theme.background,
            [0x10, 0x10, 0x18, 0xFF],
            "last good palette retained"
        );

        // Removal is a directory change too: the selected file is now
        // missing, so the reload fails closed and keeps the last good palette.
        std::fs::remove_file(&path).expect("remove theme");
        let info = poll_file_at(&mut runtime, start + CONFIG_POLL_INTERVAL * 3)
            .expect("theme removal observed");
        assert_eq!(info.kind, "load-error");
        assert_eq!(runtime.config().theme.background, [0x10, 0x10, 0x18, 0xFF]);

        // Re-creating the file with a valid palette is adopted on the next
        // tick, proving a directory gain also triggers a reload.
        write_local_theme(&path, "#0a0a12", "\n# recreated\n");
        let info = poll_file_at(&mut runtime, start + CONFIG_POLL_INTERVAL * 4)
            .expect("theme creation observed");
        assert_eq!(info.kind, "applied");
        assert_eq!(runtime.config().theme.background, [0x0A, 0x0A, 0x12, 0xFF]);
        clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a plugin-store index with one record for `id`.
    fn write_plugin_index(root: &Path, id: &str, enabled: bool) {
        std::fs::create_dir_all(root).expect("store root");
        let record = bitty_runtime::plugin_runtime::resolution::PluginRecord {
            source_class: bitty_runtime::plugin_runtime::SourceClass::Registry,
            plugin_id: id.to_string(),
            version: String::from("1.0.0"),
            root: format!("packages/{id}/1.0.0"),
            manifest_hash: "a".repeat(64),
            content_digest: "b".repeat(64),
            enabled,
            granted: Vec::new(),
        };
        bitty_runtime::plugin_runtime::resolution::write_index(root, &[record])
            .expect("write plugin index");
    }

    /// Install a reload context whose resolver returns `baseline` unchanged,
    /// with an explicit launch plugin set (never the developer's store).
    fn install_plugin_context(baseline: &EffectiveConfig, root: &Path, enabled: Vec<String>) {
        clear();
        let resolved = baseline.clone();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(resolved.clone().into())),
            None,
        );
        set_launch_plugin_set(Some(root.to_path_buf()), enabled);
    }

    #[test]
    fn reload_reports_a_newly_enabled_plugin_as_restart_required() {
        // bitty#2969: enabling a store plugin after launch is a plugin-set
        // change, not a config diff. The reload must name the plugin and
        // never report `unchanged`.
        let dir = temp_dir("plugin-enable");
        let store = dir.join("store");
        write_plugin_index(&store, "xuepoo.fixture", false);
        let baseline = bitty_config::fallback_builtin();
        install_plugin_context(&baseline, &store, Vec::new());
        let mut runtime = deterministic_runtime(&baseline);

        let unchanged = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(unchanged.kind, "unchanged", "{unchanged:?}");
        assert!(
            unchanged.plugins_restart_required.is_empty(),
            "a disabled record is not part of the running set"
        );
        assert_eq!(unchanged.plan_version, 1);

        write_plugin_index(&store, "xuepoo.fixture", true);
        let info = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(info.kind, "restart-required", "{info:?}");
        assert!(!info.applied, "a restart-required attempt never applies");
        assert_eq!(
            info.plugins_restart_required,
            vec![String::from("xuepoo.fixture")]
        );
        assert!(info.changed.is_empty(), "no config field changed");
        assert_eq!(
            info.plan_version, 1,
            "a restart-required attempt keeps the plan version"
        );

        // The dry run reports the same pending set and touches nothing.
        let preview = reload_requested_with(&mut runtime, true).expect("context installed");
        assert_eq!(preview.kind, "restart-required", "{preview:?}");
        assert!(preview.dry_run);
        assert_eq!(
            preview.plugins_restart_required,
            vec![String::from("xuepoo.fixture")]
        );

        clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_reports_a_disabled_plugin_as_restart_required() {
        // bitty#2969: the symmetric case — disabling a plugin the instance
        // launched with is pending until restart, and the id is named.
        let dir = temp_dir("plugin-disable");
        let store = dir.join("store");
        write_plugin_index(&store, "xuepoo.fixture", true);
        let baseline = bitty_config::fallback_builtin();
        install_plugin_context(&baseline, &store, vec![String::from("xuepoo.fixture")]);
        let mut runtime = deterministic_runtime(&baseline);

        let unchanged = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(unchanged.kind, "unchanged", "{unchanged:?}");
        assert!(unchanged.plugins_restart_required.is_empty());

        write_plugin_index(&store, "xuepoo.fixture", false);
        let info = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(info.kind, "restart-required", "{info:?}");
        assert_eq!(
            info.plugins_restart_required,
            vec![String::from("xuepoo.fixture")]
        );

        clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_ignores_an_unreadable_plugin_index() {
        // An integrity failure must never be turned into a fabricated
        // plugin-set change: the reload stays `unchanged`.
        let dir = temp_dir("plugin-corrupt");
        let store = dir.join("store");
        write_plugin_index(&store, "xuepoo.fixture", true);
        let baseline = bitty_config::fallback_builtin();
        install_plugin_context(&baseline, &store, vec![String::from("xuepoo.fixture")]);
        let mut runtime = deterministic_runtime(&baseline);

        std::fs::write(store.join("current.json"), b"{ not an index").expect("corrupt index");
        let info = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(info.kind, "unchanged", "{info:?}");
        assert!(info.plugins_restart_required.is_empty());

        clear();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_without_plugin_tracking_never_reports_a_plugin_set() {
        // Tests, headless paths, and `--safe` install no launch plugin set;
        // the reload must not read the developer's store to invent one.
        let dir = temp_dir("plugin-untracked");
        let store = dir.join("store");
        write_plugin_index(&store, "xuepoo.fixture", true);
        let baseline = bitty_config::fallback_builtin();
        clear();
        let resolved = baseline.clone();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(resolved.clone().into())),
            None,
        );
        let mut runtime = deterministic_runtime(&baseline);
        let info = reload_requested_with(&mut runtime, false).expect("context installed");
        assert_eq!(info.kind, "unchanged", "{info:?}");
        assert!(info.plugins_restart_required.is_empty());
        clear();
        let _ = std::fs::remove_dir_all(dir);
    }
}
