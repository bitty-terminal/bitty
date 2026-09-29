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
//! The runtime exposes live-adopt setters for the presentation layer:
//! decoration (gaps/border/radius/content_inset), outline colors, animation
//! policy, window padding, window radius, and font size. Those are the fields
//! this path adopts. The remaining `Live` inventory entries (font family /
//! line height / letter spacing, appearance theme+colors, keymaps / leader /
//! mod key, window opacity) have no runtime adopter yet — adopting them is a
//! follow-up, and a reload that touches *only* those still reports success for
//! the diff while listing them under `changed` for the caller to see.

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

/// Whether the running runtime adopts `field` live through
/// [`apply_live_presentation`]. Every other changed field, even when
/// `bitty-config` classifies it `Live`, is reported under `restart_required`
/// so a reply never claims an adoption that did not happen.
fn runtime_adopts(field: &str) -> bool {
    matches!(
        field,
        "font.size"
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
    ) || field.starts_with("appearance.animations")
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

/// Adopt the live presentation subset of `effective` into `runtime`.
///
/// Reuses the startup mapping ([`crate::config_cli::runtime_config_from_effective`])
/// so a reload resolves the same decoration/outline/animation/font values as a
/// fresh launch, then drives the runtime's live-adopt setters. Returns the
/// first setter error, formatted for the ctl reply.
pub(crate) fn apply_live_presentation(
    runtime: &mut bitty_runtime::Runtime,
    effective: &EffectiveConfig,
) -> Result<(), String> {
    let resolved = crate::config_cli::runtime_config_from_effective(effective)?;
    runtime
        .set_decoration(resolved.decoration)
        .map_err(|err| format!("decoration: {err}"))?;
    runtime.set_outline(resolved.outline_focused, resolved.outline_idle);
    runtime.set_animations(resolved.animations);
    runtime
        .set_window_padding(effective.window.padding)
        .map_err(|err| format!("window.padding: {err}"))?;
    runtime
        .set_window_radius_px(effective.window.radius_px)
        .map_err(|err| format!("window.radius_px: {err}"))?;
    runtime
        .set_font_size(resolved.font_size)
        .map_err(|err| format!("font.size: {err}"))?;
    Ok(())
}

/// A `(mtime, len)` fingerprint of the watched file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
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

impl ReloadContext {
    /// Live-class fields whose accepted value differs from the launch value
    /// but that the runtime cannot adopt live: they stay pending until restart
    /// on every later reply, including an `unchanged` one (CodeRabbit on
    /// #1516), and drop out once the file returns to the launch value.
    fn pending_restart_fields(&self) -> Vec<String> {
        bitty_config::diff(&self.launched, self.engine.current())
            .diffs
            .into_iter()
            .filter(|diff| {
                diff.class == bitty_config::ReloadClass::Live && !runtime_adopts(&diff.field)
            })
            .map(|diff| diff.field)
            .collect()
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
            if let Err(message) = apply_live_presentation(runtime, self.engine.current()) {
                // Keep engine and runtime consistent: a failed adopt must not
                // leave the engine claiming the new config is live. Earlier
                // setters may already have changed the runtime, so reapply the
                // previous presentation first (CodeRabbit on #1516).
                if let Err(restore) = apply_live_presentation(runtime, &previous) {
                    crate::logging::warn(|| {
                        format!("bitty: config reload rollback failed: {restore}")
                    });
                }
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
    fn unadopted_live_field_stays_pending_across_reloads() {
        let baseline = bitty_config::fallback_builtin();
        let mut edited = baseline.clone();
        edited.font.family = format!("{} Alt", baseline.font.family);
        let desired = std::rc::Rc::new(std::cell::RefCell::new(edited));
        let source = std::rc::Rc::clone(&desired);
        clear();
        install_with(
            baseline.clone(),
            Box::new(move || Ok(source.borrow().clone())),
            None,
        );
        let mut runtime = bitty_runtime::Runtime::with_defaults().expect("runtime");
        let first = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(first.kind, "applied");
        assert_eq!(first.restart_required, vec![String::from("font.family")]);
        let second = reload_requested(&mut runtime).expect("context installed");
        assert_eq!(second.kind, "unchanged");
        assert_eq!(
            second.restart_required,
            vec![String::from("font.family")],
            "a pending restart is reported until it takes effect"
        );
        // Reverting the file to the launch value clears the pending field.
        *desired.borrow_mut() = baseline;
        let reverted = reload_requested(&mut runtime).expect("context installed");
        assert!(reverted.restart_required.is_empty(), "{reverted:?}");
        clear();
    }

    #[test]
    fn only_runtime_adopted_fields_are_reported_as_live() {
        assert!(runtime_adopts("font.size"));
        assert!(runtime_adopts("decoration.gaps_in"));
        assert!(runtime_adopts("appearance.animations.duration_ms.open"));
        for pending in [
            "font.family",
            "window.opacity",
            "appearance.theme",
            "decoration.background_image",
            "keymaps",
        ] {
            assert!(!runtime_adopts(pending), "{pending} has no live adopter");
        }
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
}
