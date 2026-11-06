//! `bitty doctor`: installation and compatibility diagnosis (CTX-0175).
//!
//! Local-class subcommand per the accepted CLI contract (`cli-contract-rfc.md`):
//! requires no running instance and must work in safe mode (no third-party
//! plugin VM is ever loaded here). It covers binary identity, configuration
//! validity (reusing the `config check` load path), keymap conflicts, font
//! availability over the `bitty-config` fallback chain, display presence
//! (Wayland/X11), GPU presence, clipboard backends, PTY availability,
//! terminfo, shell integration, image protocols, and plugin safe-mode state.
//!
//! # Repairs (`--fix`)
//!
//! `bitty doctor --fix [--dry-run]` applies only repairs that are provably
//! safe and reversible (CTX-1721, bitty#2924):
//!
//! - remove a stale instance socket that no server answers (owner/mode
//!   verified first; a socket that connects is never touched);
//! - install the bundled `bitty` terminfo entry into the user database
//!   (`$XDG_DATA_HOME/terminfo`, else `~/.terminfo`; never `/usr/share`);
//! - reset a persisted profile selector that points at a missing profile to
//!   `default`, keeping a `.bak` backup of the previous selector.
//!
//! Everything else is reported as a `hint` with the exact manual command and
//! is never applied. A key moved by a migration stays a hint for
//! `bitty config migrate` (bitty#2923) instead of a second migration path.
//! `--dry-run` emits the same receipts without writing. `--safe` refuses
//! `--fix` with [`DoctorFixError::SafeMode`] (exit `7`, the permission class).
//!
//! # Output and exit codes
//!
//! `--format table` (default) is human output and not a machine contract.
//! `--format json` / `--format jsonl` emit the versioned envelope (`v: 1`,
//! `command: "doctor"`, `ok`, `result` plus `error` on failure) on stdout;
//! diagnostics and logs go to stderr so JSON on stdout is never corrupted.
//!
//! Exit codes follow the stable taxonomy: `0` when every check passes (warns
//! are advisory and keep `0`), `1` when at least one check fails but the host
//! remains recoverable, otherwise the strongest failing category code
//! (`3` config, `4` plugin, `5` compatibility, `6` runtime unavailable,
//! `7` permission, `8` conflict).
//!
//! # Bounds and safety
//!
//! Every external probe runs under [`run_bounded`] (no shell, no pipes, kill
//! by PID on timeout). PATH searches cap directory and name lengths. JSON is
//! emitted through [`json_escape`] so control bytes can never break the
//! envelope. No `unsafe`, no network, no plugin code.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success: every diagnostic passed (warns allowed).
pub const EXIT_OK: i32 = 0;
/// Generic failure: at least one check failed, host still recoverable.
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error (unknown flag, bad `--format`, stray positional).
pub const EXIT_USAGE: i32 = 2;
/// Configuration error (`config check` reuse failed).
pub const EXIT_CONFIG: i32 = 3;
/// Plugin error (reserved: doctor never loads plugins; safe-mode note only).
pub const EXIT_PLUGIN: i32 = 4;
/// Compatibility error (terminfo / protocol mismatch).
pub const EXIT_COMPAT: i32 = 5;
/// IPC/runtime unavailable (reserved for future runtime probes).
pub const EXIT_RUNTIME: i32 = 6;
/// Permission denied (reserved for future permission probes).
pub const EXIT_PERM: i32 = 7;
/// Conflict (key alias / resource collision).
pub const EXIT_CONFLICT: i32 = 8;

// ---------------------------------------------------------------------------
// Status and check records
// ---------------------------------------------------------------------------

/// Per-check outcome. Warns are advisory: they keep exit `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Check passed.
    Pass,
    /// Recoverable concern with an actionable hint; exit stays `0`.
    Warn,
    /// Failed gate; drives the process exit code via [`DoctorCheck::code`].
    Fail,
}

impl DoctorStatus {
    /// Machine token used in table and JSON output.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    /// Parses a status token (used by tests and fixtures).
    /// Kept as stable fixture API even though the binary itself only emits
    /// tokens; allowed dead in non-test builds.
    #[allow(dead_code)]
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "pass" => Some(Self::Pass),
            "warn" => Some(Self::Warn),
            "fail" => Some(Self::Fail),
            _ => None,
        }
    }
}

/// One diagnostic row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Stable check id (`binary`, `config`, `fonts`, ...).
    pub id: &'static str,
    /// One-line human label.
    pub label: &'static str,
    /// Outcome.
    pub status: DoctorStatus,
    /// Exit code contributed when `status == Fail` (ignored otherwise).
    /// Must be one of 1, 3, 4, 5, 6, 7, 8.
    pub code: i32,
    /// Actionable remediation hint (empty when nothing to do).
    pub hint: String,
    /// Short evidence detail (paths, versions, counts).
    pub detail: String,
}

impl DoctorCheck {
    /// Builds a row, clamping `code` to the known set (unknown becomes 1).
    pub fn new(
        id: &'static str,
        label: &'static str,
        status: DoctorStatus,
        code: i32,
        hint: String,
        detail: String,
    ) -> Self {
        let code = match code {
            3..=8 => code,
            _ => EXIT_GENERIC,
        };
        Self {
            id,
            label,
            status,
            code,
            hint,
            detail,
        }
    }

    fn pass(id: &'static str, label: &'static str, detail: String) -> Self {
        Self::new(
            id,
            label,
            DoctorStatus::Pass,
            EXIT_OK,
            String::new(),
            detail,
        )
    }

    fn warn(id: &'static str, label: &'static str, hint: String, detail: String) -> Self {
        Self::new(id, label, DoctorStatus::Warn, EXIT_OK, hint, detail)
    }

    fn fail(
        id: &'static str,
        label: &'static str,
        code: i32,
        hint: String,
        detail: String,
    ) -> Self {
        Self::new(id, label, DoctorStatus::Fail, code, hint, detail)
    }
}

// ---------------------------------------------------------------------------
// Pure check constructors (fixture-injectable, unit-tested)
// ---------------------------------------------------------------------------

/// Configuration load outcome, reusing the `config check` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigInput {
    /// No config file found; defaults apply.
    Missing,
    /// Valid config; `source` names the winning layer for evidence.
    Ok {
        /// Human source summary (e.g. `"file: /home/u/.config/bitty/init.lua"`).
        source: String,
    },
    /// Invalid config; carries the user-facing error text.
    Invalid(String),
    /// A configuration module refusal (`E_CONFIG_MODULE_*`). The typed record
    /// is kept so `doctor` reports the same code, phase, spec, source,
    /// aggregate, and bounded detail the other three paths report, instead of
    /// flattening it into a generic invalid-config string.
    Module {
        /// The contract's typed loader refusal.
        diagnostic: Box<bitty_config::ConfigModuleDiagnostic>,
    },
}

/// Keymap resolution outcome for the effective config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapInput {
    /// Resolved table with `n` entries.
    Ok(usize),
    /// Resolution failed (unknown action/chord); carries the error text.
    Err(String),
    /// Skipped because configuration itself is invalid (fix config first).
    Skipped,
}

/// Binary identity: `version` non-empty and `exe_ok` (current exe resolved).
#[must_use]
pub fn check_binary(version: &str, exe_ok: bool, exe_detail: &str) -> DoctorCheck {
    if version.trim().is_empty() {
        return DoctorCheck::fail(
            "binary",
            "Binary identity",
            EXIT_GENERIC,
            "reinstall bitty from a pinned release; empty version means a broken build".to_string(),
            "version string is empty".to_string(),
        );
    }
    if !exe_ok {
        return DoctorCheck::fail(
            "binary",
            "Binary identity",
            EXIT_GENERIC,
            "reinstall bitty; the executable path could not be resolved".to_string(),
            format!("version={version} exe=<unresolvable>"),
        );
    }
    DoctorCheck::pass(
        "binary",
        "Binary identity",
        format!("version={version} exe={exe_detail}"),
    )
}

/// Configuration validity (`config check` reuse).
#[must_use]
pub fn check_config(input: &ConfigInput) -> DoctorCheck {
    match input {
        ConfigInput::Missing => DoctorCheck::pass(
            "config",
            "Configuration file",
            "no config file found; running on built-in defaults".to_string(),
        ),
        ConfigInput::Ok { source } => DoctorCheck::pass(
            "config",
            "Configuration file",
            format!("valid ({source})"),
        ),
        ConfigInput::Invalid(msg) => DoctorCheck::fail(
            "config",
            "Configuration file",
            EXIT_CONFIG,
            "run `bitty config check` for per-key sources, fix the file, then re-run `bitty doctor`"
                .to_string(),
            truncate(msg, 300),
        ),
        ConfigInput::Module { diagnostic } => DoctorCheck::fail(
            "config",
            "Configuration file",
            EXIT_CONFIG,
            format!(
                "{} in the configuration module graph; run `bitty config check`, fix the module, then re-run `bitty doctor`",
                diagnostic.code()
            ),
            // The typed record already bounds and escapes every field, so the
            // doctor detail is the same allowlisted render the other paths use.
            diagnostic.render(),
        ),
    }
}

/// Keymap conflict diagnosis. Messages mentioning collision/conflict/duplicate
/// map to exit 8 (conflict); other resolution errors map to exit 3 (config).
#[must_use]
pub fn check_keymaps(input: &KeymapInput) -> DoctorCheck {
    match input {
        KeymapInput::Ok(n) => DoctorCheck::pass(
            "keymaps",
            "Key bindings",
            format!("{n} entries resolved, no conflicts"),
        ),
        KeymapInput::Skipped => DoctorCheck::warn(
            "keymaps",
            "Key bindings",
            "fix the configuration file first, then re-run `bitty doctor`".to_string(),
            "skipped: configuration is invalid".to_string(),
        ),
        KeymapInput::Err(msg) => {
            let lowered = msg.to_lowercase();
            let conflict = ["conflict", "collision", "duplicate", "already"]
                .iter()
                .any(|w| lowered.contains(w));
            DoctorCheck::fail(
                "keymaps",
                "Key bindings",
                if conflict { EXIT_CONFLICT } else { EXIT_CONFIG },
                "run `bitty config check` to list bindings, disambiguate the chord, then re-run `bitty doctor`"
                    .to_string(),
                truncate(msg, 300),
            )
        }
    }
}

/// Font availability over the fallback chain.
///
/// `tool_available` tells whether `fc-match` exists at all; `families` is the
/// ordered `(family, present)` probe result. All present passes; some missing
/// warns (fallback still renders); none present fails generic (headless still
/// works, hence recoverable `1`, not a category gate).
#[must_use]
pub fn check_fonts(tool_available: bool, families: &[(&str, bool)]) -> DoctorCheck {
    if !tool_available {
        return DoctorCheck::warn(
            "fonts",
            "Font availability",
            "install fontconfig (fc-match) to let doctor verify the fallback chain".to_string(),
            "fc-match not found on PATH".to_string(),
        );
    }
    if families.is_empty() {
        return DoctorCheck::warn(
            "fonts",
            "Font availability",
            "no font families were probed; this is a doctor bug, please report it".to_string(),
            "empty probe set".to_string(),
        );
    }
    let total = families.len();
    let present: Vec<&str> = families
        .iter()
        .filter_map(|(name, ok)| if *ok { Some(*name) } else { None })
        .collect();
    if present.len() == total {
        DoctorCheck::pass(
            "fonts",
            "Font availability",
            format!(
                "{total}/{total} fallback families resolve ({})",
                present.join(", ")
            ),
        )
    } else if present.is_empty() {
        DoctorCheck::fail(
            "fonts",
            "Font availability",
            EXIT_GENERIC,
            "install a monospace font plus Noto Symbols 2 (package `noto-fonts` on Arch) so TUI graphs render"
                .to_string(),
            format!("0/{total} fallback families resolve"),
        )
    } else {
        let missing: Vec<&str> = families
            .iter()
            .filter_map(|(name, ok)| if *ok { None } else { Some(*name) })
            .collect();
        DoctorCheck::warn(
            "fonts",
            "Font availability",
            format!(
                "install the missing families (first: `{}`); on Arch, `noto-fonts` provides the braille symbols face",
                missing.first().unwrap_or(&"monospace")
            ),
            format!(
                "{}/{} resolve ({}); missing: {}",
                present.len(),
                total,
                present.join(", "),
                missing.join(", ")
            ),
        )
    }
}

/// Font-chain resolution and coverage probe (issue #2931).
///
/// `configured` is the effective `font.family`; `resolved` is the face the
/// renderer actually loaded (a platform-default substitution such as
/// Cascadia Mono for Consolas, or `None` when nothing loaded). `attempts`
/// lists every considered face in order with availability, and the remaining
/// fields describe the embedded symbols face and the bounded uncovered
/// sample.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontChainInput {
    /// Effective configured primary family.
    pub configured: String,
    /// Face the chain loaded, or `None` when no primary resolved.
    pub resolved: Option<String>,
    /// True when the configured primary itself is installed.
    pub primary_present: bool,
    /// True when `resolved` differs from `configured`.
    pub substituted: bool,
    /// Considered faces in order, with availability.
    pub attempts: Vec<(String, bool)>,
    /// Family name of the embedded symbols face.
    pub embedded_family: String,
    /// True when the embedded face is registered.
    pub embedded_present: bool,
    /// True when the embedded face maps the PUA icon range start.
    pub pua_covered: bool,
    /// Probed scalars with no covering face.
    pub uncovered_total: usize,
    /// Up to a few uncovered example scalars.
    pub uncovered_examples: Vec<char>,
    /// Scalars probed (the counter's denominator).
    pub probed: usize,
}

impl FontChainInput {
    /// Builds the doctor input from a renderer chain probe.
    #[must_use]
    pub fn from_probe(probe: &bitty_render::font_defaults::ChainProbe) -> Self {
        Self {
            configured: probe.configured.clone(),
            resolved: probe.resolved.clone(),
            primary_present: probe.primary_present,
            substituted: probe.substituted,
            attempts: probe
                .attempts
                .iter()
                .map(|attempt| (attempt.family.clone(), attempt.present))
                .collect(),
            embedded_family: probe.embedded_family.clone(),
            embedded_present: probe.embedded_present,
            pua_covered: probe.pua_covered,
            uncovered_total: probe.uncovered.total,
            uncovered_examples: probe.uncovered.examples.clone(),
            probed: probe.uncovered.probed,
        }
    }
}

/// Font chain the terminal will actually resolve (issue #2931).
///
/// Reports the configured primary, every attempted face, and the face
/// actually used; flags a missing primary; states whether the embedded
/// symbols face covers the PUA icon range; and surfaces the bounded count of
/// probed scalars that fell through the whole chain. Always advisory: the
/// fallback walk still renders, so this row warns and never fails the run.
#[must_use]
pub fn check_font_chain(input: &FontChainInput) -> DoctorCheck {
    if input.configured.trim().is_empty() {
        return DoctorCheck::warn(
            "font-chain",
            "Font chain",
            "run `bitty doctor` with a readable config so the font chain can be resolved"
                .to_string(),
            "font chain not probed".to_string(),
        );
    }
    let mut detail = format!("configured {:?} -> ", input.configured.trim());
    if input.attempts.is_empty() {
        detail.push_str("no faces attempted");
    } else {
        let listed: Vec<String> = input
            .attempts
            .iter()
            .take(MAX_CHAIN_ATTEMPTS)
            .map(|(family, present)| {
                format!(
                    "{family} ({})",
                    if *present { "present" } else { "missing" }
                )
            })
            .collect();
        detail.push_str(&listed.join(", "));
        if input.attempts.len() > MAX_CHAIN_ATTEMPTS {
            detail.push_str(", ...");
        }
    }
    match input.resolved.as_deref() {
        Some(face) if input.substituted => {
            detail.push_str(&format!(" -> using {face} (platform default)"));
        }
        Some(face) => detail.push_str(&format!(" -> using {face}")),
        None => detail.push_str(" -> no face loaded"),
    }
    if input.pua_covered {
        detail.push_str(&format!(
            "; PUA icons covered by embedded {:?} (U+E000, U+E0B0, U+F017)",
            input.embedded_family
        ));
    } else if input.embedded_present {
        detail.push_str("; embedded symbols face is missing PUA coverage");
    } else {
        detail.push_str("; embedded symbols face not registered");
    }
    if input.uncovered_total > 0 {
        let examples: Vec<String> = input
            .uncovered_examples
            .iter()
            .take(MAX_UNCOVERED_EXAMPLES)
            .map(|c| format!("U+{:04X}", u32::from(*c)))
            .collect();
        let noun = if input.uncovered_total == 1 {
            "glyph has"
        } else {
            "glyphs have"
        };
        detail.push_str(&format!(
            "; {} {noun} no covering face of {} probed",
            input.uncovered_total, input.probed
        ));
        if !examples.is_empty() {
            detail.push_str(&format!(" (e.g. {})", examples.join(", ")));
        }
    }
    if input.resolved.is_some()
        && input.primary_present
        && input.pua_covered
        && input.uncovered_total == 0
    {
        return DoctorCheck::pass("font-chain", "Font chain", detail);
    }
    let hint = if !input.primary_present {
        match input.resolved.as_deref() {
            Some(face) => format!(
                "install `{}` or keep `font.family` on the resolved `{face}`",
                input.configured.trim()
            ),
            None => format!(
                "install a monospace font; the configured `{}` did not load",
                input.configured.trim()
            ),
        }
    } else if !input.pua_covered {
        "the embedded symbols face did not cover U+E000; report this as a doctor bug".to_string()
    } else {
        "install a font covering the listed codepoints to close the remaining tofu".to_string()
    };
    DoctorCheck::warn("font-chain", "Font chain", hint, detail)
}

/// Maximum attempted faces named in the `font-chain` detail line.
const MAX_CHAIN_ATTEMPTS: usize = 6;

/// Maximum uncovered example codepoints named in the `font-chain` detail.
const MAX_UNCOVERED_EXAMPLES: usize = 3;

/// Text-stack preference and optional shaped-stack probe (CTX-1607).
///
/// `probe` is `None` when the shaped stack is intentionally not exercised
/// (`BITTY_TEXT_STACK=classic`) or the effective config could not be loaded;
/// otherwise it records whether `SwashSingle` could load the configured
/// family on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextStackInput {
    /// Effective `BITTY_TEXT_STACK` preference.
    pub preference: bitty_runtime::TextStackPreference,
    /// True when the environment value was present but invalid (ignored).
    pub invalid_env: bool,
    /// Shaped-stack probe outcome (`None` = not probed).
    pub probe: Option<Result<(), String>>,
}

impl Default for TextStackInput {
    fn default() -> Self {
        Self {
            preference: bitty_runtime::TextStackPreference::Auto,
            invalid_env: false,
            probe: None,
        }
    }
}

/// Text stack the terminal will actually use (CTX-1607).
///
/// The shaped `harfrust`/`swash` stack is the production default; the
/// crossfont fallback is expected and correct when forced
/// (`BITTY_TEXT_STACK=classic`) or when the shaped probe cannot load the
/// configured family. A failed shaped probe is a warn, never a failure:
/// the runtime falls back with a diagnostic, so the terminal still renders.
#[must_use]
pub fn check_text_stack(input: &TextStackInput) -> DoctorCheck {
    let env_note = if input.invalid_env {
        " (invalid BITTY_TEXT_STACK value ignored)"
    } else {
        ""
    };
    if input.preference == bitty_runtime::TextStackPreference::Classic {
        return DoctorCheck::pass(
            "text-stack",
            "Text stack",
            format!("crossfont classic (forced by BITTY_TEXT_STACK){env_note}"),
        );
    }
    match &input.probe {
        Some(Ok(())) => DoctorCheck::pass(
            "text-stack",
            "Text stack",
            format!("shaped harfrust/swash default (ligatures + font.features active){env_note}"),
        ),
        Some(Err(reason)) => DoctorCheck::warn(
            "text-stack",
            "Text stack",
            "install a monospace font so the shaped stack can load; crossfont fallback keeps rendering"
                .to_string(),
            format!("shaped unavailable: {reason}; crossfont fallback active{env_note}"),
        ),
        None => DoctorCheck::warn(
            "text-stack",
            "Text stack",
            "run `bitty doctor` with a readable config so the shaped probe can run".to_string(),
            format!("shaped stack not probed{env_note}"),
        ),
    }
}

/// Probes the shaped stack for `family` at `size` points (CTX-1607).
///
/// Doctor-only: this loads the process-wide font database (the same scan a
/// shaped runtime pays once at startup) and loads the primary face, so a
/// font-rich host pays a one-time discovery cost during diagnosis. Errors
/// are stringified for the doctor envelope and never panic.
fn probe_shaped_stack(family: &str, size: f32) -> Result<(), String> {
    use bitty_render::GlyphRasterizer as _;
    let mut shaped = bitty_render::SwashSingle::new().map_err(|err| err.to_string())?;
    let query = bitty_render::FontQuery {
        family: family.to_string(),
        style: bitty_render::FontStyle::Normal,
        point_size: size,
    };
    shaped
        .load_font(&query)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Display presence: Wayland (`WAYLAND_DISPLAY`) or X11 (`DISPLAY`).
/// Neither present is a warn (headless smoke is the supported fallback).
#[must_use]
pub fn check_display(
    wayland: Option<&str>,
    x11: Option<&str>,
    session_type: Option<&str>,
) -> DoctorCheck {
    let wayland = wayland.map(str::trim).filter(|s| !s.is_empty());
    let x11 = x11.map(str::trim).filter(|s| !s.is_empty());
    match (wayland, x11) {
        (Some(w), _) => DoctorCheck::pass(
            "display",
            "Display server",
            format!(
                "Wayland ({w}){}",
                session_type
                    .map(|s| format!(" session={s}"))
                    .unwrap_or_default()
            ),
        ),
        (None, Some(d)) => DoctorCheck::pass(
            "display",
            "Display server",
            format!(
                "X11 ({d}){}",
                session_type
                    .map(|s| format!(" session={s}"))
                    .unwrap_or_default()
            ),
        ),
        (None, None) => DoctorCheck::warn(
            "display",
            "Display server",
            "no WAYLAND_DISPLAY or DISPLAY: GUI needs a display server; headless (`--headless`) still works"
                .to_string(),
            "headless environment".to_string(),
        ),
    }
}

/// GPU presence via DRM render nodes. Absent is a warn: the headless software
/// seam renders without a GPU.
#[must_use]
pub fn check_gpu(dri_cards: usize) -> DoctorCheck {
    if dri_cards > 0 {
        DoctorCheck::pass(
            "gpu",
            "GPU / wgpu",
            format!("{dri_cards} DRM card node(s) in /dev/dri"),
        )
    } else {
        DoctorCheck::warn(
            "gpu",
            "GPU / wgpu",
            "no /dev/dri card nodes: GPU presentation stays on the headless software seam; install Mesa/vulkan drivers for hardware present"
                .to_string(),
            "no DRM card nodes".to_string(),
        )
    }
}

/// Clipboard backends found on PATH (e.g. `wl-copy`, `xclip`). Empty is a
/// warn: in-terminal copy still highlights; cross-app sync needs a backend.
#[must_use]
pub fn check_clipboard(found: &[&str]) -> DoctorCheck {
    if found.is_empty() {
        DoctorCheck::warn(
            "clipboard",
            "Clipboard backend",
            "install wl-clipboard (Wayland) or xclip/xsel (X11) for cross-application copy/paste"
                .to_string(),
            "no clipboard helper on PATH".to_string(),
        )
    } else {
        DoctorCheck::pass(
            "clipboard",
            "Clipboard backend",
            format!("available: {}", found.join(", ")),
        )
    }
}

/// PTY availability. Unix checks `/dev/ptmx`; Windows reports ConPTY.
#[must_use]
pub fn check_pty(available: bool, detail: &str) -> DoctorCheck {
    if available {
        DoctorCheck::pass("pty", "PTY support", detail.to_string())
    } else {
        DoctorCheck::fail(
            "pty",
            "PTY support",
            EXIT_GENERIC,
            "no PTY multiplexer found: check /dev/ptmx permissions or use ConPTY on Windows"
                .to_string(),
            detail.to_string(),
        )
    }
}

/// Terminfo entry for `$TERM`. Missing entry is a compatibility failure
/// (exit 5); a non-`bitty` TERM with a valid entry is a warn; an unprobable
/// setup (no TERM or no `infocmp`) is a warn with a hint.
#[must_use]
pub fn check_terminfo(term: Option<&str>, entry_found: Option<bool>) -> DoctorCheck {
    let term = term.map(str::trim).filter(|s| !s.is_empty());
    match (term, entry_found) {
        (None, _) => DoctorCheck::warn(
            "terminfo",
            "Terminfo entry",
            "TERM is unset: export TERM=bitty (or xterm-256color as fallback) before starting shells"
                .to_string(),
            "TERM unset".to_string(),
        ),
        (Some(t), None) => DoctorCheck::warn(
            "terminfo",
            "Terminfo entry",
            format!(
                "could not probe the `{t}` entry (infocmp missing or timed out): install ncurses and the bitty terminfo file"
            ),
            format!("TERM={t}, probe inconclusive"),
        ),
        (Some(t), Some(true)) if t == "bitty" => {
            DoctorCheck::pass("terminfo", "Terminfo entry", format!("TERM={t}, entry found"))
        }
        (Some(t), Some(true)) => DoctorCheck::warn(
            "terminfo",
            "Terminfo entry",
            "export TERM=bitty inside bitty for full capability coverage; other TERM values are best-effort"
                .to_string(),
            format!("TERM={t}, entry found (expected TERM=bitty)"),
        ),
        (Some(t), Some(false)) => DoctorCheck::fail(
            "terminfo",
            "Terminfo entry",
            EXIT_COMPAT,
            format!(
                "install the `{t}` terminfo entry (tic -x terminfo/bitty.ti from the bitty repo) or export a TERM that exists"
            ),
            format!("TERM={t}, no terminfo entry"),
        ),
    }
}

/// Shell integration: `$SHELL` (or the fallback) exists and is executable.
/// Missing shells are warns: startup falls back to `/bin/sh`.
#[must_use]
pub fn check_shell(shell: Option<&str>, exists: bool, executable: bool) -> DoctorCheck {
    let shell = shell.map(str::trim).filter(|s| !s.is_empty());
    match shell {
        None => DoctorCheck::warn(
            "shell",
            "Shell integration",
            "SHELL is unset: startup falls back to /bin/sh; export SHELL to silence this"
                .to_string(),
            "SHELL unset, fallback /bin/sh".to_string(),
        ),
        Some(s) if exists && executable => DoctorCheck::pass(
            "shell",
            "Shell integration",
            format!("SHELL={s} executable"),
        ),
        Some(s) if exists => DoctorCheck::warn(
            "shell",
            "Shell integration",
            format!("chmod +x {s} or pick an executable shell; startup falls back to /bin/sh"),
            format!("SHELL={s} not executable"),
        ),
        Some(s) => DoctorCheck::warn(
            "shell",
            "Shell integration",
            format!("install {s} or export a valid SHELL; startup falls back to /bin/sh"),
            format!("SHELL={s} not found"),
        ),
    }
}

/// Default child shell: the program a bare `bitty` launch spawns
/// (bitty#2928).
///
/// Reports the same chain the spawn layer resolves — configured
/// `terminal.shell` > `$SHELL` > platform default — so a Windows host without
/// `$SHELL` no longer reads as "falls back to /bin/sh". An unresolved chain is
/// a failure: startup refuses to continue without a child.
#[must_use]
pub fn check_default_shell(input: &DefaultShellInput) -> DoctorCheck {
    match input {
        DefaultShellInput::Resolved { program, source } => DoctorCheck::pass(
            "shell-default",
            "Default child shell",
            format!("spawns {program:?} (source: {source})"),
        ),
        DefaultShellInput::Unresolved { tried } => DoctorCheck::fail(
            "shell-default",
            "Default child shell",
            EXIT_GENERIC,
            format!(
                "install one of {} or set terminal.shell; startup refuses to continue without a child shell",
                tried
                    .iter()
                    .map(|candidate| format!("{candidate:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!("none of {} resolved on PATH", tried.join(", ")),
        ),
    }
}

/// Image protocols, reported honestly: kitty graphics and iTerm2 inline
/// images are supported as bounded subsets (paint-capable images paint;
/// kitty transmit-only and unknown actions store without painting) and sixel
/// is unsupported, so this check stays an informational capability report
/// (warn) rather than an installation gate.
#[must_use]
pub fn check_image_protocols() -> DoctorCheck {
    DoctorCheck::warn(
        "images",
        "Image protocols",
        "no action needed: text, braille/block fallbacks, hyperlinks, and bounded kitty plus iTerm2 inline images work; sixel is unsupported"
            .to_string(),
        "kitty-graphics: supported (bounded); iterm2-inline-images: supported (bounded, PNG); sixel: unsupported".to_string(),
    )
}

/// Plugin state: doctor never loads third-party plugin VMs, so this always
/// reports the safe-mode posture.
#[must_use]
pub fn check_plugins_safe_mode() -> DoctorCheck {
    DoctorCheck::pass(
        "plugins",
        "Plugins (safe mode)",
        "doctor runs with no third-party plugins loaded".to_string(),
    )
}

// ---------------------------------------------------------------------------
// Report assembly
// ---------------------------------------------------------------------------

/// All live inputs for one doctor run. Tests build fixtures directly.
#[derive(Debug, Clone)]
pub struct DoctorInputs {
    /// `bitty` version string.
    pub version: String,
    /// Whether the current executable path resolved.
    pub exe_ok: bool,
    /// Resolved executable path for evidence.
    pub exe_detail: String,
    /// Configuration load outcome.
    pub config: ConfigInput,
    /// Keymap resolution outcome.
    pub keymaps: KeymapInput,
    /// Whether `fc-match` exists on PATH.
    pub font_tool_available: bool,
    /// Ordered `(family, present)` probe results.
    pub families: Vec<(String, bool)>,
    /// Resolved font chain, embedded-face coverage, and tofu counter
    /// (issue #2931).
    pub font_chain: FontChainInput,
    /// Text-stack preference plus the shaped-stack probe (CTX-1607).
    pub text_stack: TextStackInput,
    /// `WAYLAND_DISPLAY` value.
    pub wayland: Option<String>,
    /// `DISPLAY` value.
    pub x11: Option<String>,
    /// `XDG_SESSION_TYPE` value.
    pub session_type: Option<String>,
    /// Count of `/dev/dri/card*` nodes.
    pub dri_cards: usize,
    /// Clipboard helpers found on PATH.
    pub clipboard_backends: Vec<String>,
    /// PTY multiplexer available.
    pub pty_available: bool,
    /// PTY evidence detail.
    pub pty_detail: String,
    /// `TERM` value.
    pub term: Option<String>,
    /// Whether `infocmp $TERM` succeeded (`None` = could not probe).
    pub terminfo_found: Option<bool>,
    /// `SHELL` value.
    pub shell: Option<String>,
    /// Whether the shell path exists.
    pub shell_exists: bool,
    /// Whether the shell path is executable.
    pub shell_executable: bool,
    /// Program startup would spawn when no CLI `PROGRAM` is given
    /// (bitty#2928): configured `terminal.shell` > `$SHELL` > platform
    /// default, resolved through the spawn layer's own chain.
    pub default_shell: DefaultShellInput,
}

/// Outcome of resolving the default child shell for `bitty doctor`.
///
/// Mirrors [`crate::spawn::resolve_default_shell`] as an owned, printable
/// value so the report never re-runs the `PATH` lookup while rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultShellInput {
    /// The chain resolved to a program.
    Resolved {
        /// Direct `argv[0]` startup would spawn.
        program: String,
        /// Precedence layer that chose it (`terminal.shell`, `$SHELL`,
        /// platform default).
        source: &'static str,
    },
    /// No candidate resolved; startup fails with this list.
    Unresolved {
        /// Candidate names that were tried, in order.
        tried: Vec<&'static str>,
    },
}

/// Outcome counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorSummary {
    /// Passing checks.
    pub pass: usize,
    /// Advisory warns.
    pub warn: usize,
    /// Failing checks.
    pub fail: usize,
}

/// One assembled doctor run.
#[derive(Debug, Clone)]
pub struct DoctorReport {
    /// `bitty` version string.
    pub version: String,
    /// Ordered check rows.
    pub checks: Vec<DoctorCheck>,
    /// Repair receipts from `--fix` (empty outside fix mode).
    pub fixes: Vec<FixReceipt>,
    /// Typed `--fix` refusal (`--safe`), which owns the exit code.
    pub fix_error: Option<DoctorFixError>,
    /// True when `--fix` was requested (the table always prints the repair
    /// section, including "nothing to do").
    pub fix_requested: bool,
}

impl DoctorReport {
    /// Counts per status.
    #[must_use]
    pub fn summary(&self) -> DoctorSummary {
        let mut out = DoctorSummary {
            pass: 0,
            warn: 0,
            fail: 0,
        };
        for check in &self.checks {
            match check.status {
                DoctorStatus::Pass => out.pass += 1,
                DoctorStatus::Warn => out.warn += 1,
                DoctorStatus::Fail => out.fail += 1,
            }
        }
        out
    }

    /// Process exit code: a `--fix` refusal owns the code, else `0` when
    /// nothing fails, else the strongest failing category code (`1` for
    /// generic failures, including a failed repair).
    ///
    /// A repair that succeeded does not rewrite the diagnostic row it
    /// addresses: this run still reports what it found, and the next run is
    /// clean.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if let Some(error) = self.fix_error {
            return error.code();
        }
        let mut worst = EXIT_OK;
        for check in &self.checks {
            if check.status == DoctorStatus::Fail && check.code > worst {
                worst = check.code;
            }
        }
        for receipt in &self.fixes {
            let code = receipt.exit_code();
            if code > worst {
                worst = code;
            }
        }
        worst
    }

    /// Machine `ok` flag: true only when nothing fails and `--fix` was not
    /// refused.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.fix_error.is_none() && self.exit_code() == EXIT_OK
    }

    /// Worst failing check for the JSON error object (`None` when ok).
    #[must_use]
    pub fn worst_failure(&self) -> Option<&DoctorCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == DoctorStatus::Fail)
            .max_by_key(|c| c.code)
    }
}

/// Assembles the ordered report from live or fixture inputs.
#[must_use]
pub fn assemble_report(inputs: &DoctorInputs) -> DoctorReport {
    let family_refs: Vec<(&str, bool)> = inputs
        .families
        .iter()
        .map(|(name, ok)| (name.as_str(), *ok))
        .collect();
    let clipboard_refs: Vec<&str> = inputs
        .clipboard_backends
        .iter()
        .map(String::as_str)
        .collect();
    DoctorReport {
        version: inputs.version.clone(),
        checks: vec![
            check_binary(&inputs.version, inputs.exe_ok, &inputs.exe_detail),
            check_config(&inputs.config),
            check_keymaps(&inputs.keymaps),
            check_fonts(inputs.font_tool_available, &family_refs),
            check_font_chain(&inputs.font_chain),
            check_text_stack(&inputs.text_stack),
            check_display(
                inputs.wayland.as_deref(),
                inputs.x11.as_deref(),
                inputs.session_type.as_deref(),
            ),
            check_gpu(inputs.dri_cards),
            check_clipboard(&clipboard_refs),
            check_pty(inputs.pty_available, &inputs.pty_detail),
            check_terminfo(inputs.term.as_deref(), inputs.terminfo_found),
            check_shell(
                inputs.shell.as_deref(),
                inputs.shell_exists,
                inputs.shell_executable,
            ),
            check_default_shell(&inputs.default_shell),
            check_image_protocols(),
            check_plugins_safe_mode(),
        ],
        fixes: Vec::new(),
        fix_error: None,
        fix_requested: false,
    }
}

// ---------------------------------------------------------------------------
// Repairs (`--fix`, CTX-1721 / bitty#2924)
// ---------------------------------------------------------------------------

/// Bundled terminfo source compiled by the terminfo repair.
///
/// Compile-time inclusion keeps the repair available in packaged builds that
/// ship only the compiled entry, and guarantees the bytes match this revision
/// (the same file `scripts/check-terminfo.sh` audits).
pub const BUNDLED_TERMINFO_SOURCE: &str = include_str!("../../../terminfo/bitty.ti");

/// Repair outcome, carried by [`FixReceipt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixOutcome {
    /// The repair wrote its change.
    Applied,
    /// `--dry-run`: the receipt describes the change without writing.
    WouldApply,
    /// Not provably safe: `hint` carries the exact manual command.
    Hint,
    /// The repair was attempted and failed; `code` carries the exit class.
    Failed,
}

impl FixOutcome {
    /// Machine token used in table and JSON output.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::WouldApply => "would-apply",
            Self::Hint => "hint",
            Self::Failed => "failed",
        }
    }
}

/// One typed repair receipt: what changed, from/to, and the undo.
///
/// `changed` names the target (socket/config path); `from`/`to` describe the
/// value transition; `undo` is the exact reversal. Hints carry the manual
/// command in `hint` and leave `changed`/`from`/`to`/`undo` empty because
/// nothing was touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixReceipt {
    /// Stable repair id (`instance-socket`, `terminfo`, `profile-selector`,
    /// `config-key`).
    pub id: &'static str,
    /// Applied / would-apply / hint / failed.
    pub outcome: FixOutcome,
    /// Target that changed (path), empty for hints.
    pub changed: String,
    /// Previous value/state.
    pub from: String,
    /// New value/state.
    pub to: String,
    /// Exact reversal of the change.
    pub undo: String,
    /// Manual command when the repair is a hint.
    pub hint: String,
    /// Evidence detail (verified owner/mode, compile output, error text).
    pub detail: String,
    /// Exit class for a failed repair (`0` otherwise).
    pub code: i32,
}

impl FixReceipt {
    fn applied(
        id: &'static str,
        changed: String,
        from: String,
        to: String,
        undo: String,
        detail: String,
    ) -> Self {
        Self {
            id,
            outcome: FixOutcome::Applied,
            changed,
            from,
            to,
            undo,
            hint: String::new(),
            detail,
            code: 0,
        }
    }

    fn would_apply(
        id: &'static str,
        changed: String,
        from: String,
        to: String,
        undo: String,
        detail: String,
    ) -> Self {
        Self {
            outcome: FixOutcome::WouldApply,
            ..Self::applied(id, changed, from, to, undo, detail)
        }
    }

    fn hint(id: &'static str, hint: String, detail: String) -> Self {
        Self {
            id,
            outcome: FixOutcome::Hint,
            changed: String::new(),
            from: String::new(),
            to: String::new(),
            undo: String::new(),
            hint,
            detail,
            code: 0,
        }
    }

    fn failed(id: &'static str, code: i32, changed: String, detail: String) -> Self {
        Self {
            id,
            outcome: FixOutcome::Failed,
            changed,
            from: String::new(),
            to: String::new(),
            undo: String::new(),
            hint: String::new(),
            detail,
            code,
        }
    }

    /// Exit code contributed by this receipt (`0` unless failed).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.outcome == FixOutcome::Failed {
            self.code
        } else {
            EXIT_OK
        }
    }
}

/// Typed `--fix` refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorFixError {
    /// `--fix` under `--safe`: safe mode never writes.
    SafeMode,
}

impl DoctorFixError {
    /// Exit code: the permission class (`7`), because `--safe` denies writes.
    #[must_use]
    pub fn code(self) -> i32 {
        EXIT_PERM
    }

    /// Envelope error class for the machine output.
    #[must_use]
    pub fn class(self) -> &'static str {
        "Denied"
    }

    /// Stable machine error code.
    #[must_use]
    pub fn token(self) -> &'static str {
        "safe-mode-refused"
    }

    /// User-facing message.
    #[must_use]
    pub fn message(self) -> &'static str {
        "bitty doctor: --fix is refused in --safe mode (safe mode never writes)"
    }
}

/// Filesystem roots the repairs are allowed to touch.
///
/// Injected so tests never read the real environment. Production builds use
/// [`RepairEnv::from_process`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairEnv {
    /// `XDG_RUNTIME_DIR` (instance sockets live under `<base>/bitty`).
    pub runtime_dir: Option<PathBuf>,
    /// Explicit `XDG_DATA_HOME` (terminfo fallback: `<base>/terminfo`).
    pub data_home: Option<PathBuf>,
    /// `HOME` (terminfo fallback: `<home>/.terminfo`).
    pub home: Option<PathBuf>,
    /// Effective uid used for endpoint verification.
    pub uid: u32,
}

impl RepairEnv {
    /// Live environment snapshot (never fails; missing values stay `None`).
    #[must_use]
    pub fn from_process() -> Self {
        let non_empty = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            runtime_dir: non_empty("XDG_RUNTIME_DIR"),
            data_home: non_empty("XDG_DATA_HOME"),
            home: non_empty("HOME"),
            uid: bitty_ipc::current_unix_uid().unwrap_or(0),
        }
    }

    /// Instance socket directory: `<XDG_RUNTIME_DIR or /run/user/<uid>>/bitty`.
    #[cfg(unix)]
    fn socket_leaf_dir(&self) -> Option<PathBuf> {
        let base = self
            .runtime_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", self.uid)));
        Some(base.join(bitty_ipc::devtools::SOCKET_LEAF_DIR))
    }

    /// No Unix sockets (or Windows pipes) are repaired here.
    #[cfg(not(unix))]
    fn socket_leaf_dir(&self) -> Option<PathBuf> {
        None
    }

    /// User terminfo database: explicit `XDG_DATA_HOME` wins (the mandated
    /// XDG location), else the ncurses default `~/.terminfo`. Never
    /// `/usr/share/terminfo`.
    #[must_use]
    pub fn terminfo_dir(&self) -> Option<PathBuf> {
        match (&self.data_home, &self.home) {
            (Some(data_home), _) => Some(data_home.join("terminfo")),
            (None, Some(home)) => Some(home.join(".terminfo")),
            (None, None) => None,
        }
    }
}

/// Quarantine-then-verify removal of a dead socket (CTX-0792 discipline).
///
/// The path is renamed to a hidden sibling first, so the object that is
/// inspected and removed is exactly the object that was probed: a server that
/// binds a replacement at the original name after the probe is never unlinked.
/// The quarantined object must still match the captured endpoint identity
/// (device, inode, owner, `0600`, no symlink); any change restores it (only
/// when the original name is free) and refuses the repair.
#[cfg(unix)]
fn remove_dead_socket(
    path: &Path,
    identity: bitty_ipc::devtools::SocketEndpointIdentity,
    uid: u32,
) -> Result<(), (i32, String)> {
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err((
            EXIT_GENERIC,
            "socket path has no usable file name".to_string(),
        ));
    };
    let mut quarantine = path.to_path_buf();
    let mut placed = false;
    for _ in 0..8 {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        quarantine = path.with_file_name(format!(
            ".{name}.doctor-quarantine-{}-{seq}",
            std::process::id()
        ));
        if quarantine.symlink_metadata().is_err() {
            placed = true;
            break;
        }
    }
    if !placed {
        return Err((
            EXIT_GENERIC,
            "cannot allocate a quarantine name for the stale socket".to_string(),
        ));
    }
    std::fs::rename(path, &quarantine).map_err(|err| {
        (
            io_exit_code(&err, EXIT_GENERIC),
            format!("cannot quarantine stale socket: {err}"),
        )
    })?;
    let matches = quarantine.symlink_metadata().is_ok_and(|meta| {
        !meta.file_type().is_symlink()
            && meta.dev() == identity.dev
            && meta.ino() == identity.ino
            && meta.uid() == uid
            && meta.mode() & 0o777 == 0o600
    });
    if !matches {
        // Never clobber a replacement bound at the original name.
        if !path.exists() {
            let _ = std::fs::rename(&quarantine, path);
        }
        return Err((
            EXIT_GENERIC,
            format!(
                "quarantined socket identity changed; left at {} for manual inspection",
                quarantine.display()
            ),
        ));
    }
    std::fs::remove_file(&quarantine).map_err(|err| {
        if !path.exists() {
            let _ = std::fs::rename(&quarantine, path);
        }
        (
            io_exit_code(&err, EXIT_GENERIC),
            format!("cannot remove quarantined stale socket: {err}"),
        )
    })
}

/// Removes every stale instance socket under `dir`.
///
/// A candidate is removed only when all of these hold:
///
/// - it is a `.sock` entry whose endpoint verifies (parent `0700`, socket
///   `0600`, both owned by `uid`, no symlink) — a foreign endpoint is a hint,
///   never touched;
/// - `connect` fails with `ConnectionRefused`, proving no server accepts on
///   that inode (there is no pid file to consult);
/// - the endpoint identity (device + inode) is unchanged immediately before
///   `remove_file`, so a rebind between check and removal aborts the repair.
///
/// A socket that connects is live and is never touched.
#[cfg(unix)]
#[must_use]
pub fn fix_stale_instance_sockets(dir: &Path, uid: u32, dry_run: bool) -> Vec<FixReceipt> {
    use std::os::unix::net::UnixStream;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut receipts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
            continue;
        }
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let Some(instance) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let identity = match bitty_ipc::devtools::verify_socket_endpoint_for_connect(path_str, uid)
        {
            Ok(identity) => identity,
            Err(err) => {
                let hint = format!(
                    "inspect {path_str} manually: run `bitty ctl instance list` and remove it only if no instance owns it"
                );
                receipts.push(FixReceipt::hint(
                    "instance-socket",
                    hint,
                    format!("endpoint not provably safe ({err})"),
                ));
                continue;
            }
        };
        match UnixStream::connect(&path) {
            // A socket that answers is live: never touch it.
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Err(err) => {
                receipts.push(FixReceipt::hint(
                    "instance-socket",
                    format!(
                        "inspect {path_str} manually: connect failed with {err} (not provably stale)"
                    ),
                    format!("cannot prove no server answers ({err})"),
                ));
                continue;
            }
        }
        let unchanged = bitty_ipc::devtools::verify_socket_endpoint_for_connect(path_str, uid)
            .is_ok_and(|current| current == identity);
        if !unchanged {
            receipts.push(FixReceipt::hint(
                "instance-socket",
                format!("re-run `bitty doctor --fix`: {path_str} changed while it was checked"),
                "socket endpoint identity changed between check and removal".to_string(),
            ));
            continue;
        }
        let changed = path.display().to_string();
        let from = "stale socket (connect refused; no server answering)".to_string();
        let to = "removed".to_string();
        let undo = format!("start bitty with --instance {instance} to recreate the socket");
        let detail = format!("endpoint verified (owner uid {uid}, mode 0600); no live server");
        if dry_run {
            receipts.push(FixReceipt::would_apply(
                "instance-socket",
                changed,
                from,
                to,
                undo,
                detail,
            ));
            continue;
        }
        match remove_dead_socket(&path, identity, uid) {
            Ok(()) => receipts.push(FixReceipt::applied(
                "instance-socket",
                changed,
                from,
                to,
                undo,
                detail,
            )),
            Err((code, message)) => receipts.push(FixReceipt::failed(
                "instance-socket",
                code,
                changed,
                message,
            )),
        }
    }
    receipts
}

/// Windows/non-Unix: no filesystem sockets are repaired.
#[cfg(not(unix))]
#[must_use]
pub fn fix_stale_instance_sockets(_dir: &Path, _uid: u32, _dry_run: bool) -> Vec<FixReceipt> {
    Vec::new()
}

/// Candidate paths `tic` may use for one entry (flat, first-letter, hex).
fn terminfo_entry_candidates(dir: &Path, term: &str) -> Vec<PathBuf> {
    let mut out = vec![dir.join(term)];
    if let Some(first) = term.chars().next() {
        out.push(dir.join(first.to_string()).join(term));
        out.push(dir.join(format!("{:x}", u32::from(first))).join(term));
    }
    out
}

/// Whether `term` is already installed in the user terminfo database `dir`.
#[must_use]
pub fn terminfo_entry_installed(dir: &Path, term: &str) -> bool {
    terminfo_entry_candidates(dir, term)
        .iter()
        .any(|path| path.is_file())
}

/// First installed path for `term` in `dir` (evidence for receipts).
fn installed_terminfo_path(dir: &Path, term: &str) -> Option<PathBuf> {
    terminfo_entry_candidates(dir, term)
        .into_iter()
        .find(|path| path.is_file())
}

/// Maps a filesystem failure to its exit class: permission is `7`, anything
/// else uses the caller's repair class.
fn io_exit_code(err: &std::io::Error, fallback: i32) -> i32 {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        EXIT_PERM
    } else {
        fallback
    }
}

/// Compiles [`BUNDLED_TERMINFO_SOURCE`] into `dir` with `tic`.
///
/// The source is staged as a hidden temp file inside `dir` (the only place
/// this repair may write) and removed afterwards. `tic` runs through
/// [`run_bounded`] (no shell, no pipes, kill by PID on timeout). Failures
/// carry their exit class (`7` permission, `5` compatibility).
fn install_bundled_terminfo(dir: &Path) -> Result<PathBuf, (i32, String)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(dir).map_err(|err| {
        (
            io_exit_code(&err, EXIT_COMPAT),
            format!("cannot create terminfo directory {}: {err}", dir.display()),
        )
    })?;
    let staged = dir.join(format!(
        ".bitty-doctor-terminfo-{}-{}.ti",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        std::fs::write(&staged, BUNDLED_TERMINFO_SOURCE).map_err(|err| {
            (
                io_exit_code(&err, EXIT_COMPAT),
                format!("cannot stage terminfo source: {err}"),
            )
        })?;
        let dir_str = dir.to_str().ok_or_else(|| {
            (
                EXIT_COMPAT,
                "terminfo directory is not valid UTF-8".to_string(),
            )
        })?;
        let staged_str = staged.to_str().ok_or_else(|| {
            (
                EXIT_COMPAT,
                "staged terminfo source is not valid UTF-8".to_string(),
            )
        })?;
        let Some(output) = run_bounded("tic", &["-x", "-o", dir_str, staged_str], 10) else {
            return Err((
                EXIT_COMPAT,
                "`tic` is unavailable or timed out; install ncurses".to_string(),
            ));
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err((
                EXIT_COMPAT,
                format!("`tic` failed: {}", truncate(stderr.trim(), 200)),
            ));
        }
        installed_terminfo_path(dir, "bitty").ok_or_else(|| {
            (
                EXIT_COMPAT,
                "`tic` reported success but wrote no entry".to_string(),
            )
        })
    })();
    let _ = std::fs::remove_file(&staged);
    result
}

/// Installs the bundled `bitty` terminfo entry when `TERM` has no entry.
///
/// Returns `None` when the entry is already installed (idempotent) or when
/// nothing can be repaired without guessing. A non-`bitty` `TERM` is a hint:
/// the bundled source only defines `bitty`, so installing it would not make
/// that `TERM` resolve.
#[must_use]
pub fn fix_missing_terminfo(
    term: Option<&str>,
    data_home: Option<&Path>,
    home: Option<&Path>,
    dry_run: bool,
) -> Option<FixReceipt> {
    let term = term.map(str::trim).filter(|value| !value.is_empty())?;
    let dir = match (data_home, home) {
        (Some(data_home), _) => data_home.join("terminfo"),
        (None, Some(home)) => home.join(".terminfo"),
        (None, None) => {
            return Some(FixReceipt::hint(
                "terminfo",
                "set XDG_DATA_HOME or HOME, then re-run `bitty doctor --fix`".to_string(),
                "no user terminfo root is available".to_string(),
            ));
        }
    };
    if terminfo_entry_installed(&dir, term) {
        return None;
    }
    if term != "bitty" {
        return Some(FixReceipt::hint(
            "terminfo",
            format!(
                "install the `{term}` entry for your terminal, or export TERM=bitty inside bitty"
            ),
            format!(
                "the bundled source only defines `bitty`; installing it would not make TERM={term} resolve"
            ),
        ));
    }
    let target = dir.join("b").join("bitty");
    if find_on_path("tic").is_none() {
        return Some(FixReceipt::hint(
            "terminfo",
            format!(
                "install ncurses (tic), then run `tic -x -o {} terminfo/bitty.ti`",
                dir.display()
            ),
            "`tic` not found on PATH".to_string(),
        ));
    }
    if dry_run {
        return Some(FixReceipt::would_apply(
            "terminfo",
            target.display().to_string(),
            "no `bitty` terminfo entry".to_string(),
            format!("compiled into {}", dir.display()),
            format!("rm -f '{}'", target.display()),
            "bundled terminfo/bitty.ti compiles with `tic -x`".to_string(),
        ));
    }
    match install_bundled_terminfo(&dir) {
        Ok(installed) => Some(FixReceipt::applied(
            "terminfo",
            installed.display().to_string(),
            "no `bitty` terminfo entry".to_string(),
            format!("compiled into {}", dir.display()),
            format!("rm -f '{}'", installed.display()),
            terminfo_install_detail(&dir),
        )),
        Err((code, detail)) => Some(FixReceipt::failed(
            "terminfo",
            code,
            target.display().to_string(),
            detail,
        )),
    }
}

/// Explains where the entry landed and how to make ncurses search it.
fn terminfo_install_detail(dir: &Path) -> String {
    let default_search = dir.file_name().is_some_and(|name| name == ".terminfo");
    if default_search {
        format!(
            "installed into {} (searched by ncurses by default)",
            dir.display()
        )
    } else {
        format!(
            "installed into {}; add TERMINFO_DIRS={} to search the XDG location",
            dir.display(),
            dir.display()
        )
    }
}

/// Resets a persisted profile selector that points at a missing profile.
///
/// The rewrite goes through [`crate::init::write_init_config`], which backs
/// the previous selector up to a collision-free `<file>.bak[.N]` before
/// replacing it, so the receipt's undo is a byte-exact restore. `default` is
/// only written when `profiles/default.lua` exists; otherwise the repair
/// cannot produce a loadable selector and stays a hint.
#[must_use]
pub fn fix_profile_selector(
    selector: &Path,
    profiles_dir: &Path,
    dry_run: bool,
) -> Option<FixReceipt> {
    let meta = std::fs::symlink_metadata(selector).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
        return Some(FixReceipt::hint(
            "profile-selector",
            format!(
                "inspect {} manually: it exceeds the {} byte config bound",
                selector.display(),
                bitty_config::file::MAX_CONFIG_FILE_BYTES
            ),
            "selector is too large to parse safely".to_string(),
        ));
    }
    let content = std::fs::read_to_string(selector).ok()?;
    let source = bitty_config::plan::ConfigSource::new(
        bitty_config::plan::LayerKind::User,
        Some(selector.display().to_string()),
    );
    // A legacy file (no selector) is not this repair's target; the config
    // check owns that message. A parse error likewise stays a diagnostic.
    let plan = bitty_config::file::parse_lua_config(&content, &source).ok()?;
    let selected = plan.selector?.profile;
    let selected_path = profiles_dir.join(format!("{selected}.lua"));
    if selected_path.is_file() {
        return None;
    }
    if selected == "default" {
        return Some(FixReceipt::hint(
            "profile-selector",
            format!(
                "run `bitty init` to create {}, or `bitty config profile use NAME` to select an existing profile",
                selected_path.display()
            ),
            "the selector already names `default`, but that profile is missing".to_string(),
        ));
    }
    let default_path = profiles_dir.join("default.lua");
    if !default_path.is_file() {
        return Some(FixReceipt::hint(
            "profile-selector",
            format!(
                "run `bitty init` to create {}, then `bitty config profile use default`",
                default_path.display()
            ),
            format!("selected profile `{selected}` is missing and no default profile exists"),
        ));
    }
    let Some(rewritten) = crate::profile::rewrite_selector_profile(&content, "default") else {
        return Some(FixReceipt::hint(
            "profile-selector",
            format!(
                "edit {} and set exactly one top-level `profile = \"default\"`",
                selector.display()
            ),
            "no single top-level `profile = \"...\"` assignment to rewrite".to_string(),
        ));
    };
    let changed = selector.display().to_string();
    let from = format!(
        "profile = \"{selected}\" (missing: {})",
        selected_path.display()
    );
    let to = "profile = \"default\"".to_string();
    if dry_run {
        return Some(FixReceipt::would_apply(
            "profile-selector",
            changed,
            from,
            to,
            format!("keep the current bytes of {}", selector.display()),
            "profiles/default.lua exists; selector parses and the rewrite validates".to_string(),
        ));
    }
    // Preflight write access so a permission failure maps to the permission
    // class without mutating content (append open writes nothing).
    if let Err(err) = std::fs::OpenOptions::new().append(true).open(selector) {
        let code = if err.kind() == std::io::ErrorKind::PermissionDenied {
            EXIT_PERM
        } else {
            EXIT_CONFIG
        };
        return Some(FixReceipt::failed(
            "profile-selector",
            code,
            changed,
            format!("cannot write selector: {err}"),
        ));
    }
    match crate::init::write_init_config(selector, &rewritten, true) {
        Ok(outcome) => {
            let undo = match outcome.backup {
                Some(backup) => format!(
                    "restore the previous selector: cp '{}' '{}'",
                    backup.display(),
                    selector.display()
                ),
                None => format!(
                    "restore the previous selector bytes of {}",
                    selector.display()
                ),
            };
            Some(FixReceipt::applied(
                "profile-selector",
                changed,
                from,
                to,
                undo,
                "selector rewritten; comments and overrides preserved".to_string(),
            ))
        }
        Err(crate::init::InitWriteError::Refused(message)) => Some(FixReceipt::failed(
            "profile-selector",
            EXIT_CONFIG,
            changed,
            format!("selector rewrite refused: {message}"),
        )),
        Err(crate::init::InitWriteError::Io(message)) => Some(FixReceipt::failed(
            "profile-selector",
            EXIT_CONFIG,
            changed,
            format!("selector rewrite failed: {message}"),
        )),
    }
}

/// A key moved by a migration stays a hint until `bitty config migrate`
/// (bitty#2923) exists; doctor never grows a second migration path.
#[must_use]
pub fn hint_moved_config_key(config_error: &str) -> Option<FixReceipt> {
    let lowered = config_error.to_ascii_lowercase();
    if !lowered.contains("undeclared field") && !lowered.contains("migrat") {
        return None;
    }
    Some(FixReceipt::hint(
        "config-key",
        "run `bitty config migrate` to move legacy keys to their current names".to_string(),
        truncate(config_error, 300),
    ))
}

/// Computes the repair receipts for one `--fix` run.
///
/// Pure over injected roots and observed diagnostics: every repair is a
/// named function with its own receipt, and anything not provably safe
/// becomes a hint. `dry_run` never writes.
#[must_use]
pub fn plan_repairs(
    config_path: Option<&Path>,
    config_error: Option<&str>,
    term: Option<&str>,
    terminfo_missing: bool,
    env: &RepairEnv,
    dry_run: bool,
) -> Vec<FixReceipt> {
    let mut receipts = Vec::new();
    if let Some(dir) = env.socket_leaf_dir() {
        receipts.extend(fix_stale_instance_sockets(&dir, env.uid, dry_run));
    }
    if terminfo_missing {
        if let Some(receipt) =
            fix_missing_terminfo(term, env.data_home.as_deref(), env.home.as_deref(), dry_run)
        {
            receipts.push(receipt);
        }
    }
    if let Some(path) = config_path {
        if let Some(parent) = path.parent() {
            let profiles_dir = parent.join(bitty_config::file::PROFILES_DIR_NAME);
            if let Some(receipt) = fix_profile_selector(path, &profiles_dir, dry_run) {
                receipts.push(receipt);
            }
        }
    }
    if let Some(error) = config_error {
        if let Some(receipt) = hint_moved_config_key(error) {
            receipts.push(receipt);
        }
    }
    receipts
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

/// Machine output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorFormat {
    /// Human table (not a machine contract).
    Table,
    /// Single versioned JSON envelope on stdout.
    Json,
    /// Same envelope, single line (one JSON value per line).
    Jsonl,
}

impl DoctorFormat {
    /// Parses `--format` (`None` means the table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).map(|s| s.to_ascii_lowercase()) {
            None => Ok(Self::Table),
            Some(s) if s == "table" => Ok(Self::Table),
            Some(s) if s == "json" => Ok(Self::Json),
            Some(s) if s == "jsonl" => Ok(Self::Jsonl),
            Some(other) => Err(format!(
                "bitty doctor: unknown --format {other:?} (want table|json|jsonl)"
            )),
        }
    }

    /// Canonical name for help and errors.
    /// Stable contract API for generators; allowed dead until a caller needs
    /// the canonical spelling outside `parse`.
    #[allow(dead_code)]
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Json => "json",
            Self::Jsonl => "jsonl",
        }
    }
}

/// Usage and flag list for `bitty doctor` (stderr on misuse; stdout for
/// `bitty doctor --help`).
#[must_use]
pub fn doctor_usage() -> String {
    "usage: bitty doctor [--format table|json|jsonl] [--no-color] [--config PATH] [--profile NAME] [--fix [--dry-run]]\n\
     \n\
     \x20 --format SHAPE  table|json|jsonl (default table; JSON carries the same\n\
     \x20                 checks and repair receipts)\n\
     \x20 --no-color      disable ANSI coloring in table output\n\
     \x20 --config PATH   explicit user config file (init.lua)\n\
     \x20 --profile NAME  named profile layered under the user file\n\
     \x20 --fix           apply only provably safe, reversible repairs; anything\n\
     \x20                 else is reported as a hint with the exact manual command\n\
     \x20 --dry-run       with --fix: print the same receipts without writing\n\
     \x20 --safe          refuses --fix with exit 7 (safe mode never writes)\n\
     \n\
     Repairs applied by --fix (each has an undo in its receipt):\n\
     \x20 stale instance socket  remove <XDG_RUNTIME_DIR>/bitty/<id>.sock when the\n\
     \x20                        endpoint verifies and no server answers; a socket\n\
     \x20                        that connects is never touched\n\
     \x20 missing terminfo       compile terminfo/bitty.ti into the user terminfo\n\
     \x20                        dir ($XDG_DATA_HOME/terminfo, else ~/.terminfo;\n\
     \x20                        never /usr/share)\n\
     \x20 broken profile         reset the persisted selector to `default` when the\n\
     \x20 selector               selected profile is missing; the previous selector\n\
     \x20                        is kept as <file>.bak\n\
     \n\
     Hints, never applied: a config key moved by a migration (run\n\
     `bitty config migrate`), unverifiable sockets, and non-bitty TERM entries."
        .to_string()
}

/// Escapes a string for embedding in JSON output.
#[must_use]
pub fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Maps an exit code to the envelope error class.
fn error_class_for(code: i32) -> &'static str {
    match code {
        EXIT_CONFIG => "ConfigError",
        EXIT_PLUGIN => "PluginError",
        EXIT_COMPAT => "CompatibilityError",
        EXIT_RUNTIME => "Unavailable",
        EXIT_PERM => "Denied",
        EXIT_CONFLICT => "Conflict",
        _ => "Error",
    }
}

/// Renders the versioned JSON envelope (compact, single value).
#[must_use]
pub fn format_json(report: &DoctorReport) -> String {
    use std::fmt::Write as _;
    let summary = report.summary();
    let mut out = String::with_capacity(2048);
    let _ = write!(
        out,
        "{{\"v\":1,\"command\":\"doctor\",\"ok\":{}",
        report.ok()
    );
    if let Some(error) = report.fix_error {
        let _ = write!(
            out,
            ",\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}}",
            error.class(),
            error.token(),
            json_escape(error.message()),
        );
    } else {
        // A failed repair competes with the worst failing check for the error
        // object: the strongest class wins, matching `exit_code`.
        let mut error_object = report.worst_failure().map(|worst| {
            (
                worst.code,
                error_class_for(worst.code),
                worst.id,
                if worst.hint.is_empty() {
                    worst.detail.clone()
                } else {
                    worst.hint.clone()
                },
            )
        });
        if let Some(repair) = report
            .fixes
            .iter()
            .filter(|receipt| receipt.outcome == FixOutcome::Failed)
            .max_by_key(|receipt| receipt.exit_code())
        {
            let code = repair.exit_code();
            if error_object
                .as_ref()
                .is_none_or(|(current, ..)| code > *current)
            {
                error_object = Some((
                    code,
                    error_class_for(code),
                    repair.id,
                    repair.detail.clone(),
                ));
            }
        }
        if let Some((_, class, code, message)) = error_object {
            let _ = write!(
                out,
                ",\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}}",
                class,
                json_escape(code),
                json_escape(&message),
            );
        }
    }
    let _ = write!(
        out,
        ",\"result\":{{\"version\":\"{}\",\"summary\":{{\"pass\":{},\"warn\":{},\"fail\":{}}},\"checks\":[",
        json_escape(&report.version),
        summary.pass,
        summary.warn,
        summary.fail,
    );
    for (i, check) in report.checks.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"status\":\"{}\",\"code\":{},\"label\":\"{}\",\"hint\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(check.id),
            check.status.token(),
            if check.status == DoctorStatus::Fail {
                check.code
            } else {
                0
            },
            json_escape(check.label),
            json_escape(&check.hint),
            json_escape(&check.detail),
        );
    }
    out.push_str("],\"fixes\":[");
    for (i, receipt) in report.fixes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"outcome\":\"{}\",\"code\":{},\"changed\":\"{}\",\"from\":\"{}\",\"to\":\"{}\",\"undo\":\"{}\",\"hint\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(receipt.id),
            receipt.outcome.token(),
            receipt.exit_code(),
            json_escape(&receipt.changed),
            json_escape(&receipt.from),
            json_escape(&receipt.to),
            json_escape(&receipt.undo),
            json_escape(&receipt.hint),
            json_escape(&receipt.detail),
        );
    }
    out.push_str("]}}");
    out
}

/// Renders one check row for table output.
fn table_row(check: &DoctorCheck, no_color: bool) -> String {
    const GREEN: &str = "\u{1b}[32m";
    const YELLOW: &str = "\u{1b}[33m";
    const RED: &str = "\u{1b}[31m";
    const RESET: &str = "\u{1b}[0m";
    let token = match check.status {
        DoctorStatus::Pass => "PASS",
        DoctorStatus::Warn => "WARN",
        DoctorStatus::Fail => "FAIL",
    };
    let colored = if no_color {
        token.to_string()
    } else {
        let color = match check.status {
            DoctorStatus::Pass => GREEN,
            DoctorStatus::Warn => YELLOW,
            DoctorStatus::Fail => RED,
        };
        format!("{color}{token}{RESET}")
    };
    if check.hint.is_empty() {
        format!(
            "[{}] {:<10} {} — {}",
            colored, check.id, check.label, check.detail
        )
    } else {
        format!(
            "[{}] {:<10} {} — {}\n             hint: {}",
            colored, check.id, check.label, check.detail, check.hint
        )
    }
}

/// Renders one repair receipt for table output.
fn fix_table_row(receipt: &FixReceipt, no_color: bool) -> String {
    const GREEN: &str = "\u{1b}[32m";
    const CYAN: &str = "\u{1b}[36m";
    const YELLOW: &str = "\u{1b}[33m";
    const RED: &str = "\u{1b}[31m";
    const RESET: &str = "\u{1b}[0m";
    let token = match receipt.outcome {
        FixOutcome::Applied => "APPLIED",
        FixOutcome::WouldApply => "DRY-RUN",
        FixOutcome::Hint => "HINT",
        FixOutcome::Failed => "FAILED",
    };
    let colored = if no_color {
        token.to_string()
    } else {
        let color = match receipt.outcome {
            FixOutcome::Applied => GREEN,
            FixOutcome::WouldApply => CYAN,
            FixOutcome::Hint => YELLOW,
            FixOutcome::Failed => RED,
        };
        format!("{color}{token}{RESET}")
    };
    let mut out = if receipt.outcome == FixOutcome::Hint {
        format!("[{}] {:<16} {}", colored, receipt.id, receipt.hint)
    } else if receipt.changed.is_empty() {
        format!("[{}] {:<16} {}", colored, receipt.id, receipt.detail)
    } else {
        format!("[{}] {:<16} {}", colored, receipt.id, receipt.changed)
    };
    if !receipt.from.is_empty() {
        out.push_str(&format!("\n             from: {}", receipt.from));
    }
    if !receipt.to.is_empty() {
        out.push_str(&format!("\n             to: {}", receipt.to));
    }
    if !receipt.undo.is_empty() {
        out.push_str(&format!("\n             undo: {}", receipt.undo));
    }
    if !receipt.detail.is_empty() && receipt.outcome != FixOutcome::Hint {
        out.push_str(&format!("\n             note: {}", receipt.detail));
    }
    out
}

/// Renders human table output (not a machine contract).
#[must_use]
pub fn format_table(report: &DoctorReport, no_color: bool) -> String {
    let summary = report.summary();
    let mut out = format!("bitty doctor — {}\n", report.version);
    for check in &report.checks {
        out.push_str(&table_row(check, no_color));
        out.push('\n');
    }
    if report.fix_requested {
        out.push_str("repairs:\n");
        if report.fixes.is_empty() {
            out.push_str("  nothing to do\n");
        } else {
            for receipt in &report.fixes {
                out.push_str("  ");
                out.push_str(&fix_table_row(receipt, no_color));
                out.push('\n');
            }
        }
        let mut applied = 0;
        let mut planned = 0;
        let mut hints = 0;
        let mut failed = 0;
        for receipt in &report.fixes {
            match receipt.outcome {
                FixOutcome::Applied => applied += 1,
                FixOutcome::WouldApply => planned += 1,
                FixOutcome::Hint => hints += 1,
                FixOutcome::Failed => failed += 1,
            }
        }
        out.push_str(&format!(
            "repairs: {applied} applied, {planned} dry-run, {hints} hint, {failed} failed\n"
        ));
    }
    out.push_str(&format!(
        "summary: {} pass, {} warn, {} fail (exit {})\n",
        summary.pass,
        summary.warn,
        summary.fail,
        report.exit_code(),
    ));
    out
}

// ---------------------------------------------------------------------------
// Bounded external probes (no shell, no pipes, kill by PID on timeout)
// ---------------------------------------------------------------------------

/// Runs `prog args...` with a hard timeout, capturing output.
///
/// No shell is involved (direct `argv[0]`, P0 posture) and no shell pipes are
/// used. When the deadline passes the child is killed by PID and reaped, and
/// `None` is returned so callers degrade to warn/inconclusive instead of
/// hanging the diagnostics entry point.
pub fn run_bounded(prog: &str, args: &[&str], timeout_secs: u64) -> Option<std::process::Output> {
    use std::process::{Command, Stdio};
    if prog.trim().is_empty() || prog.len() > 256 {
        return None;
    }
    if args.len() > 32 || args.iter().any(|a| a.len() > 1024) {
        return None;
    }
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(1, 30));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Kill by PID, then reap so no zombie escapes doctor.
                    if child.kill().is_ok() {
                        eprintln!("bitty doctor: probe `{prog}` (pid {pid}) timed out — killed");
                    }
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Searches `PATH`-style `path_env` for `name` without spawning a shell.
///
/// Caps: at most 256 directories, each at most 4096 bytes, names at most 128
/// bytes with no control bytes. Returns the first match.
#[must_use]
pub fn find_on_path_with(path_env: Option<&str>, name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.len() > 128
        || name.contains(|c: char| c.is_control() || c == '/' || c == '\\')
    {
        return None;
    }
    let path_env = path_env?;
    #[cfg(windows)]
    let separator = ';';
    #[cfg(not(windows))]
    let separator = ':';
    for dir in path_env.split(separator).take(256) {
        if dir.is_empty() || dir.len() > 4096 {
            continue;
        }
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = candidate.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Live PATH lookup for `name`.
#[must_use]
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    find_on_path_with(std::env::var("PATH").ok().as_deref(), name)
}

/// Probes one font family via `fc-match` (bounded). Returns `None` when the
/// tool is missing or the probe timed out (caller degrades to warn).
///
/// `fc-match` exits 0 even for an unknown name (it returns the best
/// fallback), so the resolved family list is compared with the request:
/// "No Such Mono" resolving to `Noto Sans Mono` is a miss, not a hit.
/// Generic aliases (`monospace`, `sans-serif`, ...) are always a hit by
/// definition — their whole purpose is to resolve to a concrete face.
#[must_use]
pub fn probe_font(family: &str) -> Option<bool> {
    let family = family.trim();
    if family.is_empty() {
        return Some(false);
    }
    let out = run_bounded("fc-match", &["--format=%{family}", family], 3)?;
    if !out.status.success() {
        return Some(false);
    }
    Some(font_match_resolves(
        family,
        &String::from_utf8_lossy(&out.stdout),
    ))
}

/// True when an `fc-match --format=%{family}` result names the request.
///
/// Pure so the probe's semantics are testable without fontconfig.
#[must_use]
pub fn font_match_resolves(requested: &str, resolved_families: &str) -> bool {
    let requested = requested.trim();
    let generic = matches!(
        requested.to_ascii_lowercase().as_str(),
        "monospace" | "mono" | "fixed" | "sans-serif" | "sans" | "serif"
    );
    if generic {
        return true;
    }
    resolved_families
        .split(',')
        .any(|name| name.trim().eq_ignore_ascii_case(requested))
}

/// Probes a terminfo entry via `infocmp` (bounded). `None` means the tool is
/// missing or the probe timed out.
#[must_use]
pub fn probe_terminfo(term: &str) -> Option<bool> {
    if term.trim().is_empty() {
        return Some(false);
    }
    let out = run_bounded("infocmp", &[term], 3)?;
    Some(out.status.success())
}

/// Counts `/dev/dri/card*` nodes (bounded to 16). Always `0` on Windows.
#[must_use]
pub fn dri_card_count() -> usize {
    #[cfg(windows)]
    {
        0
    }
    #[cfg(not(windows))]
    {
        let entries = std::fs::read_dir("/dev/dri");
        let Ok(entries) = entries else {
            return 0;
        };
        entries
            .take(16)
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("card"))
            })
            .count()
    }
}

/// Truncates evidence strings to `max` chars on a char boundary.
fn truncate(raw: &str, max: usize) -> String {
    if raw.len() <= max {
        return raw.to_string();
    }
    let mut end = max;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &raw[..end])
}

// ---------------------------------------------------------------------------
// Tests (each check unit-testable with fixtures; output shape tests)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `bitty doctor` CLI entry point (relocated from `main.rs`, CTX-0305)
// ---------------------------------------------------------------------------

use crate::cli::Args;
use crate::config_cli::load_merged_config_typed;
use crate::version::version_semver;

/// Clipboard helpers probed on `PATH` in order (Wayland first, then X11).
const DOCTOR_CLIPBOARD_CANDIDATES: &[&str] = &["wl-copy", "xclip", "xsel"];

/// Returns true when `path` is executable (Unix exec bits; Windows: exists).
fn doctor_shell_executable(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        path.exists()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
}

/// Collects live inputs for one doctor run.
///
/// Impure (environment, filesystem, bounded external probes) and total (every
/// probe degrades to warn/inconclusive instead of panicking). Reuses the
/// `config check` load path (`load_merged_config`) so an invalid config file
/// becomes a failing `config` check (exit 3) rather than a startup abort. No
/// plugin VM is ever loaded (safe-mode posture); all external commands run
/// under `run_bounded` (no shell, no pipes, kill by PID on timeout).
fn collect_doctor_inputs(args: &Args) -> DoctorInputs {
    // Bare semver: the doctor `version` field stays the machine-comparable
    // release version; the `bitty <semver> (<channel> <commit>)` table form
    // lives in `crate::version::version_text` (#1375).
    let version = version_semver().to_string();
    let (exe_ok, exe_detail) = match std::env::current_exe() {
        Ok(path) => (true, path.display().to_string()),
        Err(_) => (false, String::new()),
    };
    let (config, keymaps, font_chain, font_probe, font_config, config_shell) =
        match load_merged_config_typed(args) {
            Ok(loaded) => {
                let file_path = loaded
                    .probed
                    .as_ref()
                    .filter(|probe| probe.path.exists())
                    .map(|probe| probe.path.clone());
                let config = if let Some(path) = file_path.as_ref() {
                    ConfigInput::Ok {
                        source: format!("file: {}", path.display()),
                    }
                } else if let Some(path) = loaded.profile_path.as_ref() {
                    ConfigInput::Ok {
                        source: format!("profile: {}", path.display()),
                    }
                } else {
                    ConfigInput::Missing
                };
                let keymaps = match bitty_config::keymap::resolve_keymaps(&loaded.merged.effective)
                {
                    Ok(maps) => KeymapInput::Ok(maps.len()),
                    Err(err) => KeymapInput::Err(err.to_string()),
                };
                let font_config = loaded.merged.effective.font.clone();
                let chain = font_config.fallback_chain();
                let probe = (font_config.family.clone(), font_config.size);
                // bitty#2928: the configured shell is the first layer of the
                // default chain, so doctor must resolve with it to report the
                // program startup would actually spawn.
                let config_shell = loaded.merged.effective.terminal.shell.clone();
                (
                    config,
                    keymaps,
                    chain,
                    Some(probe),
                    Some(font_config),
                    config_shell,
                )
            }
            Err(failure) => {
                let chain: Vec<String> = bitty_config::types::FONT_FALLBACK_CHAIN
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect();
                let config = match failure.module_diagnostic() {
                    Some(diagnostic) => ConfigInput::Module {
                        diagnostic: Box::new(diagnostic.clone()),
                    },
                    None => ConfigInput::Invalid(failure.message),
                };
                (config, KeymapInput::Skipped, chain, None, None, None)
            }
        };
    // Issue #2931: resolve the chain the renderer would use (configured
    // primary -> attempted faces -> face actually loaded), the embedded
    // symbols face's PUA coverage, and the bounded tofu counter. The probe
    // loads faces, so it runs once here and every row reads the result.
    let font_chain_input = match font_config.as_ref() {
        Some(cfg) => FontChainInput::from_probe(&bitty_render::font_defaults::probe_chain(cfg)),
        None => FontChainInput::default(),
    };
    let font_tool_available = find_on_path("fc-match").is_some();
    // `fc-match` exits 0 even for unknown names (it returns a fallback), so
    // it can only speak for system families. The embedded symbols face is
    // compiled in, not a system font: its presence and coverage are reported
    // by the `font-chain` row instead.
    let families: Vec<(String, bool)> = font_chain
        .into_iter()
        .filter(|family| family != bitty_render::embedded::EMBEDDED_SYMBOLS_FAMILY)
        .map(|family| {
            let present = probe_font(&family).unwrap_or(false);
            (family, present)
        })
        .collect();
    // CTX-1607: report the resolved text stack and, unless the user forced
    // the classic fallback, whether the shaped default can load the
    // configured family on this host. A failed probe is diagnostic only:
    // the runtime falls back and keeps rendering.
    let (preference, invalid_env) = bitty_runtime::TextStackPreference::from_env_value(
        std::env::var(bitty_runtime::TEXT_STACK_ENV).ok().as_deref(),
    );
    let probe = match (preference, font_probe) {
        (bitty_runtime::TextStackPreference::Classic, _) => None,
        (_, Some((family, size))) => Some(probe_shaped_stack(&family, size)),
        (_, None) => None,
    };
    let text_stack = TextStackInput {
        preference,
        invalid_env: invalid_env.is_some(),
        probe,
    };
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let x11 = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let dri_cards = dri_card_count();
    let clipboard_backends: Vec<String> = DOCTOR_CLIPBOARD_CANDIDATES
        .iter()
        .filter_map(|name| find_on_path(name).map(|_| (*name).to_string()))
        .collect();
    #[cfg(windows)]
    let (pty_available, pty_detail) = (true, "ConPTY available (Windows)".to_string());
    #[cfg(not(windows))]
    let (pty_available, pty_detail) = {
        let path = std::path::Path::new("/dev/ptmx");
        if path.exists() {
            (true, "/dev/ptmx present".to_string())
        } else {
            (false, "/dev/ptmx missing".to_string())
        }
    };
    let term = std::env::var("TERM")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let terminfo_found = match term.as_deref() {
        None => None,
        Some(name) => {
            // `infocmp` only searches the system database plus TERMINFO /
            // ~/.terminfo; an entry installed by `--fix` into the user
            // database is still present evidence for this check.
            let installed = RepairEnv::from_process()
                .terminfo_dir()
                .is_some_and(|dir| terminfo_entry_installed(&dir, name));
            match probe_terminfo(name) {
                Some(false) if installed => Some(true),
                other => other,
            }
        }
    };
    let shell = std::env::var("SHELL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let (shell_exists, shell_executable) = match shell.as_deref() {
        None => (false, false),
        Some(name) => {
            let path = std::path::Path::new(name);
            let exists = path.exists();
            let executable = if exists {
                doctor_shell_executable(path)
            } else {
                false
            };
            (exists, executable)
        }
    };
    // bitty#2928: resolve through the spawn layer's own chain so the reported
    // program and the spawned program can never disagree.
    let default_shell =
        match crate::spawn::resolve_default_shell(config_shell.as_deref(), shell.as_deref()) {
            Ok(resolved) => DefaultShellInput::Resolved {
                program: resolved.program,
                source: resolved.source.label(),
            },
            Err(err) => DefaultShellInput::Unresolved { tried: err.tried },
        };
    DoctorInputs {
        version,
        exe_ok,
        exe_detail,
        config,
        keymaps,
        font_tool_available,
        families,
        font_chain: font_chain_input,
        text_stack,
        wayland,
        x11,
        session_type,
        dri_cards,
        clipboard_backends,
        pty_available,
        pty_detail,
        term,
        terminfo_found,
        shell,
        shell_exists,
        shell_executable,
        default_shell,
    }
}

/// Runs `bitty doctor`; returns the process exit code.
///
/// - Extra positionals and unknown `--format` shapes fail closed (exit 2).
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "doctor"`) on stdout with diagnostics on stderr so
///   machine output is never corrupted.
/// - Exit `0` when every check passes (warns allowed), `1` on recoverable
///   failure, else the strongest category code (3 config, 5 compat, 8
///   conflict) per the accepted CLI contract.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if !args.doctor_args.is_empty() {
        eprintln!(
            "bitty doctor: unexpected argument '{}'\n{}",
            args.doctor_args[0],
            doctor_usage()
        );
        return EXIT_USAGE;
    }
    let format = match DoctorFormat::parse(args.doctor_format.as_deref()) {
        Ok(format) => format,
        Err(message) => {
            eprintln!("{message}\n{}", doctor_usage());
            return EXIT_USAGE;
        }
    };
    if args.doctor_dry_run && !args.doctor_fix {
        eprintln!("bitty doctor: --dry-run requires --fix\n{}", doctor_usage());
        return EXIT_USAGE;
    }
    // Safe mode never writes: refuse `--fix` with the typed permission-class
    // error before any probe or repair runs.
    if args.doctor_fix && args.safe {
        let error = DoctorFixError::SafeMode;
        match format {
            DoctorFormat::Table => eprintln!("{}", error.message()),
            DoctorFormat::Json | DoctorFormat::Jsonl => {
                // No probe runs: the refusal is total, so the envelope is
                // built without touching the environment.
                let report = DoctorReport {
                    version: version_semver().to_string(),
                    checks: Vec::new(),
                    fixes: Vec::new(),
                    fix_error: Some(error),
                    fix_requested: true,
                };
                println!("{}", format_json(&report));
            }
        }
        return error.code();
    }
    let inputs = collect_doctor_inputs(args);
    let mut report = assemble_report(&inputs);
    if args.doctor_fix {
        report.fix_requested = true;
        let config_path = resolved_doctor_config_path(args);
        let module_detail;
        let config_error = match &inputs.config {
            ConfigInput::Invalid(message) => Some(message.as_str()),
            ConfigInput::Module { diagnostic } => {
                module_detail = diagnostic.render();
                Some(module_detail.as_str())
            }
            ConfigInput::Missing | ConfigInput::Ok { .. } => None,
        };
        report.fixes = plan_repairs(
            config_path.as_deref(),
            config_error,
            inputs.term.as_deref(),
            inputs.terminfo_found == Some(false),
            &RepairEnv::from_process(),
            args.doctor_dry_run,
        );
    }
    match format {
        DoctorFormat::Table => {
            let no_color = args.doctor_no_color || std::env::var("NO_COLOR").is_ok();
            print!("{}", format_table(&report, no_color));
        }
        DoctorFormat::Json | DoctorFormat::Jsonl => {
            println!("{}", format_json(&report));
        }
    }
    report.exit_code()
}

/// Resolves the user config file exactly like the loader: explicit
/// `--config` > `BITTY_CONFIG` > XDG probe. Used only to locate the persisted
/// selector for the repair; the loader itself owns validation.
fn resolved_doctor_config_path(args: &Args) -> Option<PathBuf> {
    let env = std::env::var("BITTY_CONFIG").ok();
    let explicit =
        bitty_config::file::resolve_config_explicit(args.config_path.as_deref(), env.as_deref());
    bitty_config::file::probe_config_path(explicit.as_deref()).map(|probed| probed.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_inputs() -> DoctorInputs {
        DoctorInputs {
            version: "0.0.1".to_string(),
            exe_ok: true,
            exe_detail: "/usr/bin/bitty".to_string(),
            config: ConfigInput::Ok {
                source: "file: init.lua".to_string(),
            },
            keymaps: KeymapInput::Ok(12),
            font_tool_available: true,
            families: vec![
                ("JetBrainsMono Nerd Font".to_string(), true),
                ("JetBrains Mono".to_string(), true),
                ("monospace".to_string(), true),
                ("DejaVu Sans Mono".to_string(), true),
                ("Noto Sans Symbols 2".to_string(), true),
            ],
            font_chain: FontChainInput {
                configured: "JetBrainsMono Nerd Font".to_string(),
                resolved: Some("JetBrainsMono Nerd Font".to_string()),
                primary_present: true,
                substituted: false,
                attempts: vec![("JetBrainsMono Nerd Font".to_string(), true)],
                embedded_family: "Bitty Symbols Nerd".to_string(),
                embedded_present: true,
                pua_covered: true,
                uncovered_total: 0,
                uncovered_examples: Vec::new(),
                probed: 15,
            },
            text_stack: TextStackInput {
                preference: bitty_runtime::TextStackPreference::Auto,
                invalid_env: false,
                probe: Some(Ok(())),
            },
            wayland: Some("wayland-0".to_string()),
            x11: None,
            session_type: Some("wayland".to_string()),
            dri_cards: 1,
            clipboard_backends: vec!["wl-copy".to_string()],
            pty_available: true,
            pty_detail: "/dev/ptmx present".to_string(),
            term: Some("bitty".to_string()),
            terminfo_found: Some(true),
            shell: Some("/bin/bash".to_string()),
            shell_exists: true,
            shell_executable: true,
            default_shell: DefaultShellInput::Resolved {
                program: "/bin/bash".to_string(),
                source: "terminal.shell",
            },
        }
    }

    #[test]
    fn all_pass_yields_exit_zero() {
        let report = assemble_report(&healthy_inputs());
        assert_eq!(report.exit_code(), EXIT_OK);
        assert!(report.ok());
        assert_eq!(report.checks.len(), 14);
        assert!(report.checks.iter().all(|c| c.status == DoctorStatus::Pass
            || c.id == "images"
            || c.id == "config"
            || c.id == "keymaps"));
        // images is informational-warn by design; everything else passes.
        let summary = report.summary();
        assert_eq!(summary.fail, 0);
        assert_eq!(summary.pass + summary.warn, 14);
    }

    #[test]
    fn text_stack_reports_shaped_default_and_forced_classic() {
        // CTX-1607: the doctor row names the resolved production stack, and
        // a shaped-probe failure is an advisory warn (the runtime falls
        // back), never a hard failure.
        let shaped = check_text_stack(&TextStackInput {
            preference: bitty_runtime::TextStackPreference::Auto,
            invalid_env: false,
            probe: Some(Ok(())),
        });
        assert_eq!(shaped.id, "text-stack");
        assert_eq!(shaped.status, DoctorStatus::Pass);
        assert!(shaped.detail.contains("shaped"), "{}", shaped.detail);

        let forced = check_text_stack(&TextStackInput {
            preference: bitty_runtime::TextStackPreference::Classic,
            invalid_env: false,
            probe: None,
        });
        assert_eq!(forced.status, DoctorStatus::Pass);
        assert!(forced.detail.contains("classic"), "{}", forced.detail);

        let fallback = check_text_stack(&TextStackInput {
            preference: bitty_runtime::TextStackPreference::Auto,
            invalid_env: true,
            probe: Some(Err("no system fonts discovered".to_string())),
        });
        assert_eq!(fallback.status, DoctorStatus::Warn);
        assert!(
            fallback.detail.contains("crossfont fallback"),
            "{}",
            fallback.detail
        );
        assert!(
            fallback.detail.contains("invalid BITTY_TEXT_STACK"),
            "{}",
            fallback.detail
        );
    }

    #[test]
    fn warn_only_keeps_exit_zero() {
        let mut inputs = healthy_inputs();
        inputs.wayland = None;
        inputs.x11 = None;
        inputs.dri_cards = 0;
        inputs.clipboard_backends.clear();
        let report = assemble_report(&inputs);
        assert!(report.summary().warn >= 3);
        assert_eq!(report.exit_code(), EXIT_OK);
        assert!(report.ok());
    }

    #[test]
    fn image_protocols_report_bounded_kitty_not_stub() {
        // DT-12 (CTX-0509 follow-up): the images row must describe the
        // shipped bounded kitty subset, never the pre-present-path stub.
        let check = check_image_protocols();
        assert_eq!(check.id, "images");
        assert_eq!(check.status, DoctorStatus::Warn);
        let text = format!("{} {}", check.hint, check.detail);
        for marker in ["bounded", "iterm2", "sixel", "unsupported"] {
            assert!(
                text.contains(marker),
                "images row must name {marker:?}, got {text:?}"
            );
        }
        let lowered = text.to_ascii_lowercase();
        for stale in ["stub", "later slice", "no raster"] {
            assert!(
                !lowered.contains(stale),
                "images row must not repeat the pre-present-path stub wording ({stale:?}), got {text:?}"
            );
        }
    }

    #[test]
    fn generic_failure_yields_exit_one() {
        let mut inputs = healthy_inputs();
        inputs.pty_available = false;
        inputs.pty_detail = "/dev/ptmx missing".to_string();
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_GENERIC);
        assert!(!report.ok());
    }

    #[test]
    fn config_failure_yields_exit_three() {
        let mut inputs = healthy_inputs();
        inputs.config = ConfigInput::Invalid("font.size: expected number".to_string());
        inputs.keymaps = KeymapInput::Skipped;
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_CONFIG);
        let worst = report.worst_failure().expect("worst failure");
        assert_eq!(worst.id, "config");
    }

    #[test]
    fn module_config_failure_yields_exit_three_and_a_typed_redacted_row() {
        use bitty_config::{ConfigModuleCode, ConfigModuleDiagnostic, ConfigModulePhase};
        let mut inputs = healthy_inputs();
        inputs.config = ConfigInput::Module {
            diagnostic: Box::new(
                ConfigModuleDiagnostic::new(ConfigModuleCode::NotFound, ConfigModulePhase::Resolve)
                    .with_spec(b"missing.module".to_vec())
                    .with_detail("neither the flat module nor the init module exists"),
            ),
        };
        inputs.keymaps = KeymapInput::Skipped;
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_CONFIG);
        let worst = report.worst_failure().expect("worst failure");
        assert_eq!(worst.id, "config");
        // Typed-outcome parity: the row carries the contract record, not only a
        // generic invalid-config string.
        assert!(
            worst.detail.contains("E_CONFIG_MODULE_NOT_FOUND"),
            "{worst:?}"
        );
        assert!(worst.detail.contains("phase=resolve"), "{worst:?}");
        assert!(
            worst.detail.contains("module_spec=\"missing.module\""),
            "{worst:?}"
        );
        assert!(
            worst.hint.contains("E_CONFIG_MODULE_NOT_FOUND"),
            "{worst:?}"
        );
        // Redaction: no host path, canonical root, or identity tuple.
        assert!(!worst.detail.contains("lua/"), "{worst:?}");
        assert!(!worst.detail.contains("dev="), "{worst:?}");
    }

    #[test]
    fn terminfo_failure_yields_exit_five() {
        let mut inputs = healthy_inputs();
        inputs.term = Some("bitty".to_string());
        inputs.terminfo_found = Some(false);
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_COMPAT);
    }

    #[test]
    fn keymap_conflict_yields_exit_eight() {
        let mut inputs = healthy_inputs();
        inputs.keymaps = KeymapInput::Err("duplicate chord alt+h claims two actions".to_string());
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_CONFLICT);
    }

    #[test]
    fn keymap_validation_error_yields_exit_three() {
        let mut inputs = healthy_inputs();
        inputs.keymaps = KeymapInput::Err("unknown action frobnicate".to_string());
        let report = assemble_report(&inputs);
        assert_eq!(report.exit_code(), EXIT_CONFIG);
    }

    #[test]
    fn strongest_category_code_wins() {
        let mut inputs = healthy_inputs();
        inputs.config = ConfigInput::Invalid("bad".to_string());
        inputs.keymaps = KeymapInput::Skipped;
        inputs.term = Some("bitty".to_string());
        inputs.terminfo_found = Some(false);
        inputs.keymaps = KeymapInput::Err("collision on alt+h".to_string());
        let report = assemble_report(&inputs);
        // config(3) + keymaps-conflict(8) + terminfo(5) -> 8 wins.
        assert_eq!(report.exit_code(), EXIT_CONFLICT);
    }

    #[test]
    fn font_chain_reports_resolved_face_and_pua_coverage() {
        let mut input = FontChainInput {
            configured: "Menlo".to_string(),
            resolved: Some("Menlo".to_string()),
            primary_present: true,
            attempts: vec![
                ("SF Mono".to_string(), false),
                ("Menlo".to_string(), true),
                ("Monaco".to_string(), true),
            ],
            embedded_family: "Bitty Symbols Nerd".to_string(),
            embedded_present: true,
            pua_covered: true,
            probed: 15,
            ..Default::default()
        };
        let check = check_font_chain(&input);
        assert_eq!(check.id, "font-chain");
        assert_eq!(check.status, DoctorStatus::Pass, "{}", check.detail);
        // Configured -> attempted -> actually used, all named.
        assert!(
            check.detail.contains("configured \"Menlo\""),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("SF Mono (missing)"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("using Menlo"), "{}", check.detail);
        assert!(
            check
                .detail
                .contains("PUA icons covered by embedded \"Bitty Symbols Nerd\""),
            "{}",
            check.detail
        );
        // A missing configured primary is flagged even when substituted.
        input.configured = "Consolas".to_string();
        input.primary_present = false;
        input.resolved = Some("Cascadia Mono".to_string());
        input.substituted = true;
        let substituted = check_font_chain(&input);
        assert_eq!(substituted.status, DoctorStatus::Warn);
        assert!(
            substituted
                .detail
                .contains("using Cascadia Mono (platform default)"),
            "{}",
            substituted.detail
        );
        assert!(
            substituted.hint.contains("install `Consolas`"),
            "{}",
            substituted.hint
        );
    }

    #[test]
    fn font_chain_counter_reports_uncovered_glyphs() {
        let input = FontChainInput {
            configured: "Menlo".to_string(),
            resolved: Some("Menlo".to_string()),
            primary_present: true,
            substituted: false,
            attempts: vec![("Menlo".to_string(), true)],
            embedded_family: "Bitty Symbols Nerd".to_string(),
            embedded_present: true,
            pua_covered: true,
            uncovered_total: 2,
            uncovered_examples: vec!['\u{1F600}', '\u{2603}'],
            probed: 15,
        };
        let check = check_font_chain(&input);
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(
            check
                .detail
                .contains("2 glyphs have no covering face of 15 probed"),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("U+1F600") && check.detail.contains("U+2603"),
            "{}",
            check.detail
        );
        // Singular grammar for one uncovered scalar.
        let single = FontChainInput {
            uncovered_total: 1,
            uncovered_examples: vec!['\u{F8FF}'],
            ..input
        };
        let check = check_font_chain(&single);
        assert!(
            check.detail.contains("1 glyph has no covering face"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn font_chain_without_a_readable_config_is_not_probed() {
        let check = check_font_chain(&FontChainInput::default());
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("not probed"), "{}", check.detail);
    }

    #[test]
    fn font_probe_rejects_fc_match_fallbacks() {
        // `fc-match` always answers; only an answer that names the request is
        // a hit (issue #2931: doctor must flag a missing primary).
        assert!(font_match_resolves(
            "JetBrainsMono Nerd Font",
            "JetBrainsMono Nerd Font,JetBrainsMono NF"
        ));
        assert!(font_match_resolves(
            "noto sans symbols 2",
            "Noto Sans Symbols 2"
        ));
        assert!(!font_match_resolves("No Such Mono", "Noto Sans Mono"));
        assert!(!font_match_resolves("No Such Mono", ""));
        // Generic aliases resolve by definition.
        assert!(font_match_resolves("monospace", "Noto Sans Mono"));
        assert!(font_match_resolves("sans-serif", "DejaVu Sans"));
    }

    #[test]
    fn fonts_partial_is_warn_and_none_is_fail() {
        let partial = check_fonts(true, &[("Mono", true), ("Noto Sans Symbols 2", false)]);
        assert_eq!(partial.status, DoctorStatus::Warn);
        let none = check_fonts(true, &[("Mono", false), ("Noto Sans Symbols 2", false)]);
        assert_eq!(none.status, DoctorStatus::Fail);
        assert_eq!(none.code, EXIT_GENERIC);
        let no_tool = check_fonts(false, &[]);
        assert_eq!(no_tool.status, DoctorStatus::Warn);
    }

    #[test]
    fn display_prefers_wayland_then_x11_then_warn() {
        assert_eq!(
            check_display(Some("wayland-0"), None, None).status,
            DoctorStatus::Pass
        );
        assert_eq!(
            check_display(None, Some(":0"), None).status,
            DoctorStatus::Pass
        );
        assert_eq!(check_display(None, None, None).status, DoctorStatus::Warn);
        assert_eq!(
            check_display(Some("  "), Some(""), None).status,
            DoctorStatus::Warn
        );
    }

    #[test]
    fn terminfo_matrix() {
        assert_eq!(
            check_terminfo(Some("bitty"), Some(true)).status,
            DoctorStatus::Pass
        );
        assert_eq!(
            check_terminfo(Some("xterm-256color"), Some(true)).status,
            DoctorStatus::Warn
        );
        let missing = check_terminfo(Some("bitty"), Some(false));
        assert_eq!(missing.status, DoctorStatus::Fail);
        assert_eq!(missing.code, EXIT_COMPAT);
        assert_eq!(check_terminfo(None, None).status, DoctorStatus::Warn);
        assert_eq!(
            check_terminfo(Some("bitty"), None).status,
            DoctorStatus::Warn
        );
    }

    #[test]
    fn binary_rejects_empty_version() {
        let bad = check_binary("", true, "/usr/bin/bitty");
        assert_eq!(bad.status, DoctorStatus::Fail);
        let no_exe = check_binary("0.0.1", false, "");
        assert_eq!(no_exe.status, DoctorStatus::Fail);
    }

    #[test]
    fn format_parses_and_rejects() {
        assert_eq!(DoctorFormat::parse(None).unwrap(), DoctorFormat::Table);
        assert_eq!(
            DoctorFormat::parse(Some("json")).unwrap(),
            DoctorFormat::Json
        );
        assert_eq!(
            DoctorFormat::parse(Some(" JSONL ")).unwrap(),
            DoctorFormat::Jsonl
        );
        assert!(DoctorFormat::parse(Some("yaml")).is_err());
    }

    #[test]
    fn json_envelope_shape_on_success() {
        let report = assemble_report(&healthy_inputs());
        let json = format_json(&report);
        assert!(json.contains("\"v\":1"), "envelope version: {json}");
        assert!(json.contains("\"command\":\"doctor\""), "command: {json}");
        assert!(json.contains("\"ok\":true"), "ok flag: {json}");
        assert!(json.contains("\"result\":"), "result: {json}");
        assert!(!json.contains("\"error\":"), "no error on success: {json}");
        assert!(json.contains("\"id\":\"binary\""), "checks: {json}");
        assert!(
            json.contains("\"status\":\"pass\""),
            "status tokens: {json}"
        );
    }

    #[test]
    fn json_envelope_shape_on_failure() {
        let mut inputs = healthy_inputs();
        inputs.config = ConfigInput::Invalid("font.size broke".to_string());
        inputs.keymaps = KeymapInput::Skipped;
        let report = assemble_report(&inputs);
        let json = format_json(&report);
        assert!(json.contains("\"ok\":false"), "ok flag: {json}");
        assert!(json.contains("\"error\":"), "error object: {json}");
        assert!(
            json.contains("\"class\":\"ConfigError\""),
            "error class: {json}"
        );
        assert!(json.contains("\"result\":"), "result still present: {json}");
        // Balanced braces as a cheap well-formedness probe.
        assert_eq!(
            json.chars().filter(|c| *c == '{').count(),
            json.chars().filter(|c| *c == '}').count(),
            "unbalanced braces: {json}"
        );
    }

    #[test]
    fn json_escapes_control_bytes() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\ny"), "x\\ny");
        assert_eq!(json_escape("t\tb"), "t\\tb");
        assert_eq!(json_escape("\u{0}"), "\\u0000");
        let mut inputs = healthy_inputs();
        inputs.config = ConfigInput::Invalid("quote\"newline\nback\\slash".to_string());
        let json = format_json(&assemble_report(&inputs));
        assert!(json.contains("\\\""), "escaped quote: {json}");
        assert!(json.contains("\\n"), "escaped newline: {json}");
        assert!(json.contains("\\\\"), "escaped backslash: {json}");
    }

    #[test]
    fn table_shape_lists_every_check() {
        let report = assemble_report(&healthy_inputs());
        let table = format_table(&report, true);
        assert!(table.starts_with("bitty doctor — "), "header: {table}");
        for id in [
            "binary",
            "config",
            "keymaps",
            "fonts",
            "font-chain",
            "text-stack",
            "display",
            "gpu",
            "clipboard",
            "pty",
            "terminfo",
            "shell",
            "shell-default",
            "images",
            "plugins",
        ] {
            assert!(table.contains(id), "missing {id}: {table}");
        }
        assert!(table.contains("summary:"), "summary: {table}");
        assert!(
            table.contains("PASS") || table.contains("WARN"),
            "tokens: {table}"
        );
        // Color disabled: no ANSI escapes.
        assert!(!table.contains('\u{1b}'), "no-color violated: {table:?}");
        // Color enabled: PASS rows carry green.
        let colored = format_table(&report, false);
        assert!(colored.contains('\u{1b}'), "color missing: {colored:?}");
    }

    #[test]
    fn default_child_shell_row_reports_the_program_and_fails_when_unresolved() {
        // bitty#2928: doctor must name the program a bare launch spawns, and
        // an unresolved chain must be a failure (startup refuses to continue).
        let resolved = check_default_shell(&DefaultShellInput::Resolved {
            program: "C:\\Program Files\\PowerShell\\7\\pwsh.exe".to_string(),
            source: "platform default",
        });
        assert_eq!(resolved.status, DoctorStatus::Pass);
        assert_eq!(resolved.id, "shell-default");
        assert!(
            resolved.detail.contains("pwsh.exe") && resolved.detail.contains("platform default"),
            "{resolved:?}"
        );

        let unresolved = check_default_shell(&DefaultShellInput::Unresolved {
            tried: vec!["pwsh.exe", "powershell.exe", "cmd.exe"],
        });
        assert_eq!(unresolved.status, DoctorStatus::Fail);
        assert_eq!(unresolved.code, EXIT_GENERIC);
        for candidate in ["pwsh.exe", "powershell.exe", "cmd.exe"] {
            assert!(
                unresolved.hint.contains(candidate),
                "action names {candidate}: {unresolved:?}"
            );
        }
    }

    #[test]
    fn status_tokens_round_trip() {
        for (token, status) in [
            ("pass", DoctorStatus::Pass),
            ("warn", DoctorStatus::Warn),
            ("fail", DoctorStatus::Fail),
        ] {
            assert_eq!(DoctorStatus::parse(token), Some(status));
            assert_eq!(status.token(), token);
        }
        assert_eq!(DoctorStatus::parse("bogus"), None);
    }

    #[test]
    fn find_on_path_rejects_bad_names() {
        assert_eq!(find_on_path_with(Some("/usr/bin"), ""), None);
        assert_eq!(find_on_path_with(Some("/usr/bin"), "a/b"), None);
        assert_eq!(find_on_path_with(None, "sh"), None);
        // Nonexistent dirs never match.
        assert_eq!(
            find_on_path_with(Some("/nonexistent-bitty-dir-xyz"), "sh"),
            None
        );
    }

    #[test]
    fn run_bounded_captures_fast_command() {
        // `true` is a POSIX utility with no Windows counterpart, so the test
        // previously failed there (spawn failed -> `None`). Windows uses the
        // native one-shot runner `cmd /C exit 0` (the same convention as the
        // ConPTY `spawn_windows` and `cli_run` tests); Unix keeps `true`. The
        // fast child must be captured with a successful status, not killed at
        // the deadline.
        #[cfg(windows)]
        let (prog, args): (&str, &[&str]) = ("cmd", &["/C", "exit", "0"]);
        #[cfg(not(windows))]
        let (prog, args): (&str, &[&str]) = ("true", &[]);
        // 10s matches the proven one-shot bound in the ConPTY `spawn_windows`
        // tests: a healthy child exits in milliseconds, but a loaded Windows CI
        // runner can stall first launch, and this assert is about capture, not
        // latency.
        let out = run_bounded(prog, args, 10).expect("fast command must be captured");
        assert!(
            out.status.success(),
            "fast command must exit 0, got {:?}",
            out.status
        );
    }

    #[test]
    fn run_bounded_rejects_bad_input() {
        // Empty and unresolvable program names fail at spawn on every platform
        // (no shell fallback, so no PATH magic can rescue them).
        assert_eq!(run_bounded("", &[], 1), None);
        assert_eq!(
            run_bounded("definitely-not-a-bitty-binary-xyz", &[], 1),
            None
        );
    }

    #[test]
    fn run_bounded_kills_hung_command() {
        // `sleep` does not exist on Windows, where the old test passed
        // vacuously because the spawn failed before the deadline ever applied.
        // `ping -n 31 127.0.0.1` is the native Windows blocking probe (~30s,
        // present in `System32`); Unix keeps `sleep 30`. The kill-at-deadline
        // path is therefore genuinely exercised on both platforms.
        #[cfg(windows)]
        let (prog, args): (&str, &[&str]) = ("ping", &["-n", "31", "127.0.0.1"]);
        #[cfg(not(windows))]
        let (prog, args): (&str, &[&str]) = ("sleep", &["30"]);
        let start = Instant::now();
        let out = run_bounded(prog, args, 1);
        assert_eq!(out, None, "hung child must be killed at the deadline");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kill took too long: {:?}",
            start.elapsed()
        );
    }

    // -----------------------------------------------------------------------
    // `--fix` repairs (CTX-1721 / bitty#2924)
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Short, unique scratch root (pid + atomic counter; parallel-safe).
    ///
    /// Unix socket paths are bounded by `SUN_LEN` (104 bytes on macOS) and
    /// macOS `temp_dir()` (`/var/folders/…/T`, which resolves to the longer
    /// `/private/var/…`) leaves no room for the `run/bitty/<instance>.sock`
    /// fixtures, so socket fixtures anchor under the short `/tmp` spelling of
    /// the same volume there.
    fn scratch_root(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        #[cfg(target_os = "macos")]
        let base = PathBuf::from("/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();
        let path = base.join(format!(
            "bitty-doctor-fix-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch root");
        path
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    /// Recursive snapshot (relative path, kind, length, mtime, bytes) used to
    /// prove `--dry-run` never writes.
    #[cfg(unix)]
    #[derive(Debug, PartialEq, Eq)]
    struct SnapshotEntry {
        path: PathBuf,
        kind: &'static str,
        len: u64,
        modified: Option<std::time::SystemTime>,
        bytes: Option<Vec<u8>>,
    }

    #[cfg(unix)]
    fn snapshot(root: &Path) -> Vec<SnapshotEntry> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<SnapshotEntry>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                let is_dir = meta.is_dir();
                out.push(SnapshotEntry {
                    path: relative,
                    kind: if is_dir { "dir" } else { "file" },
                    len: meta.len(),
                    modified: meta.modified().ok(),
                    bytes: if is_dir {
                        None
                    } else {
                        std::fs::read(&path).ok()
                    },
                });
                if is_dir {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Binds a Unix socket with the verified `0700` dir / `0600` socket modes.
    #[cfg(unix)]
    fn bind_socket(path: &Path) -> std::os::unix::net::UnixListener {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = path.parent().expect("socket parent");
        std::fs::create_dir_all(dir).expect("socket dir");
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .expect("socket dir mode");
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind socket");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("socket mode");
        listener
    }

    #[cfg(unix)]
    fn test_env(root: &Path) -> RepairEnv {
        RepairEnv {
            runtime_dir: Some(root.join("run")),
            data_home: Some(root.join("data")),
            home: Some(root.join("home")),
            uid: bitty_ipc::current_unix_uid().unwrap_or(0),
        }
    }

    fn selector_fixture(root: &Path) -> (PathBuf, PathBuf, String) {
        let config = root.join("config").join("init.lua");
        let original = "-- selector\nreturn {\n    format = \"bitty-profile-v1\",\n    profile = \"work\",\n    -- keep me\n    window = { opacity = 0.9 },\n}\n".to_string();
        write_file(&config, &original);
        let profiles = config.parent().expect("config parent").join("profiles");
        write_file(&profiles.join("default.lua"), "return {}\n");
        (config, profiles, original)
    }

    #[cfg(unix)]
    #[test]
    fn stale_instance_socket_removed_and_live_socket_untouched() {
        let root = scratch_root("socket");
        let leaf = root.join("bitty");
        let live_path = leaf.join("live.sock");
        let live = bind_socket(&live_path);
        let stale_path = leaf.join("stale.sock");
        drop(bind_socket(&stale_path));
        let uid = bitty_ipc::current_unix_uid().expect("uid");

        let receipts = fix_stale_instance_sockets(&leaf, uid, false);
        assert_eq!(receipts.len(), 1, "receipts: {receipts:?}");
        let receipt = &receipts[0];
        assert_eq!(receipt.id, "instance-socket");
        assert_eq!(receipt.outcome, FixOutcome::Applied);
        assert!(receipt.changed.ends_with("stale.sock"), "{receipt:?}");
        assert_eq!(receipt.to, "removed");
        assert!(receipt.undo.contains("--instance stale"), "{receipt:?}");
        assert!(!stale_path.exists(), "stale socket must be removed");
        assert!(live_path.exists(), "a live socket must never be touched");
        // The live socket still answers after the scan.
        assert!(std::os::unix::net::UnixStream::connect(&live_path).is_ok());
        drop(live);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn terminfo_installs_into_temp_xdg_and_is_idempotent() {
        if find_on_path("tic").is_none() {
            eprintln!("skip terminfo repair: tic not on PATH");
            return;
        }
        let root = scratch_root("terminfo");
        let data_home = root.join("data");
        let dir = data_home.join("terminfo");
        let receipt = fix_missing_terminfo(Some("bitty"), Some(&data_home), None, false)
            .expect("first run must repair");
        assert_eq!(receipt.outcome, FixOutcome::Applied, "{receipt:?}");
        assert!(
            terminfo_entry_installed(&dir, "bitty"),
            "entry must be installed: {receipt:?}"
        );
        assert!(
            receipt.changed.starts_with(&dir.display().to_string()),
            "installed under the temp XDG root: {receipt:?}"
        );
        assert!(receipt.undo.contains("rm -f"), "undo: {receipt:?}");
        // Second run is idempotent: no receipt, no write.
        assert_eq!(
            fix_missing_terminfo(Some("bitty"), Some(&data_home), None, false),
            None
        );
        // Undo path: removing the installed entry makes the repair necessary
        // again.
        let installed = PathBuf::from(&receipt.changed);
        std::fs::remove_file(&installed).expect("undo remove");
        assert!(!terminfo_entry_installed(&dir, "bitty"));
        assert!(
            fix_missing_terminfo(Some("bitty"), Some(&data_home), None, false).is_some(),
            "undo must re-expose the missing entry"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn non_bitty_term_is_a_hint_not_an_install() {
        let root = scratch_root("terminfo-other");
        let data_home = root.join("data");
        let receipt = fix_missing_terminfo(Some("xterm-256color"), Some(&data_home), None, false)
            .expect("hint");
        assert_eq!(receipt.outcome, FixOutcome::Hint);
        assert!(receipt.hint.contains("TERM=bitty"), "{receipt:?}");
        assert!(!data_home.join("terminfo").exists(), "must not write");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn profile_selector_reset_reports_previous_value_and_undo() {
        let root = scratch_root("selector");
        let (config, profiles, original) = selector_fixture(&root);

        let receipt =
            fix_profile_selector(&config, &profiles, false).expect("selector repair receipt");
        assert_eq!(receipt.outcome, FixOutcome::Applied, "{receipt:?}");
        assert_eq!(receipt.id, "profile-selector");
        assert!(receipt.from.contains("\"work\""), "{receipt:?}");
        assert_eq!(receipt.to, "profile = \"default\"");
        assert!(
            receipt.undo.contains(".bak"),
            "undo names the backup: {receipt:?}"
        );

        let updated = std::fs::read_to_string(&config).expect("read selector");
        assert!(updated.contains("profile = \"default\""), "{updated}");
        assert!(
            updated.contains("-- keep me"),
            "comments preserved: {updated}"
        );
        assert!(
            updated.contains("opacity = 0.9"),
            "overrides preserved: {updated}"
        );
        assert_eq!(
            fix_profile_selector(&config, &profiles, false),
            None,
            "second run reports nothing to do"
        );

        // Undo path: the backup named by the receipt restores the exact bytes.
        let backup = receipt
            .undo
            .split('\'')
            .nth(1)
            .map(PathBuf::from)
            .expect("backup path in undo");
        std::fs::copy(&backup, &config).expect("restore backup");
        assert_eq!(std::fs::read_to_string(&config).expect("read"), original);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn profile_selector_without_default_profile_is_a_hint() {
        let root = scratch_root("selector-nodefault");
        let config = root.join("config").join("init.lua");
        write_file(
            &config,
            "return { format = \"bitty-profile-v1\", profile = \"work\" }\n",
        );
        let profiles = root.join("config").join("profiles");
        std::fs::create_dir_all(&profiles).expect("profiles dir");
        let receipt = fix_profile_selector(&config, &profiles, false).expect("hint receipt");
        assert_eq!(receipt.outcome, FixOutcome::Hint, "{receipt:?}");
        assert!(receipt.hint.contains("bitty init"), "{receipt:?}");
        assert_eq!(
            std::fs::read_to_string(&config).expect("read"),
            "return { format = \"bitty-profile-v1\", profile = \"work\" }\n",
            "a hint must not write"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn moved_config_key_is_a_hint_for_config_migrate() {
        let receipt = hint_moved_config_key("undeclared field 'window.pad' from user:init.lua")
            .expect("hint");
        assert_eq!(receipt.id, "config-key");
        assert_eq!(receipt.outcome, FixOutcome::Hint);
        assert!(receipt.hint.contains("bitty config migrate"), "{receipt:?}");
        assert!(receipt.changed.is_empty(), "a hint changes nothing");
        assert!(hint_moved_config_key("font.size: expected number").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_writes_nothing() {
        let root = scratch_root("dry-run");
        let env = test_env(&root);
        let leaf = root.join("run").join("bitty");
        let stale_path = leaf.join("stale.sock");
        drop(bind_socket(&stale_path));
        let (config, _profiles, _original) = selector_fixture(&root);

        let before = snapshot(&root);
        let receipts = plan_repairs(Some(&config), None, Some("bitty"), true, &env, true);
        let after = snapshot(&root);
        assert_eq!(before, after, "dry-run must not write anything");
        assert!(
            receipts
                .iter()
                .any(|receipt| receipt.id == "instance-socket"
                    && receipt.outcome == FixOutcome::WouldApply),
            "dry-run must describe the socket repair: {receipts:?}"
        );
        assert!(
            receipts
                .iter()
                .any(|receipt| receipt.id == "profile-selector"
                    && receipt.outcome == FixOutcome::WouldApply),
            "dry-run must describe the selector repair: {receipts:?}"
        );
        assert!(stale_path.exists(), "dry-run must keep the stale socket");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn second_fix_run_reports_nothing_to_do() {
        let root = scratch_root("second-run");
        let env = test_env(&root);
        let leaf = root.join("run").join("bitty");
        let stale_path = leaf.join("stale.sock");
        drop(bind_socket(&stale_path));
        let (config, _profiles, _original) = selector_fixture(&root);
        // Pre-install a terminfo entry so the repair has nothing to compile.
        write_file(
            &root.join("data").join("terminfo").join("b").join("bitty"),
            "",
        );

        let first = plan_repairs(Some(&config), None, Some("bitty"), true, &env, false);
        assert!(
            first
                .iter()
                .any(|receipt| receipt.id == "instance-socket"
                    && receipt.outcome == FixOutcome::Applied),
            "first run repairs the socket: {first:?}"
        );
        assert!(
            first.iter().any(|receipt| receipt.id == "profile-selector"
                && receipt.outcome == FixOutcome::Applied),
            "first run repairs the selector: {first:?}"
        );
        let second = plan_repairs(Some(&config), None, Some("bitty"), true, &env, false);
        assert!(
            second.is_empty(),
            "second run reports nothing to do: {second:?}"
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn safe_fix_is_refused_with_typed_error_and_exit_seven() {
        let mut args = Args::new();
        args.doctor_word = true;
        args.doctor_fix = true;
        args.safe = true;
        args.doctor_format = Some("json".to_string());
        assert_eq!(run_cli(&args), EXIT_PERM);
        assert_eq!(DoctorFixError::SafeMode.code(), EXIT_PERM);
        assert_eq!(DoctorFixError::SafeMode.class(), "Denied");
        assert_eq!(DoctorFixError::SafeMode.token(), "safe-mode-refused");
        assert!(DoctorFixError::SafeMode.message().contains("--safe"));
    }

    #[test]
    fn dry_run_without_fix_is_a_usage_error() {
        let mut args = Args::new();
        args.doctor_word = true;
        args.doctor_dry_run = true;
        assert_eq!(run_cli(&args), EXIT_USAGE);
    }

    #[test]
    fn report_exit_code_includes_failed_repairs() {
        let mut report = assemble_report(&healthy_inputs());
        report.fix_requested = true;
        report.fixes.push(FixReceipt::failed(
            "instance-socket",
            EXIT_PERM,
            "bitty/default.sock".to_string(),
            "permission denied".to_string(),
        ));
        assert_eq!(report.exit_code(), EXIT_PERM);
        assert!(!report.ok());
        // A failed repair owns the JSON error object when it is the strongest
        // class (the healthy fixture has no failing check).
        let json = format_json(&report);
        assert!(json.contains("\"ok\":false"), "{json}");
        assert!(json.contains("\"class\":\"Denied\""), "{json}");
        assert!(json.contains("\"code\":\"instance-socket\""), "{json}");
        // Hints never change the exit code.
        report.fixes = vec![FixReceipt::hint(
            "config-key",
            "run it".to_string(),
            String::new(),
        )];
        assert_eq!(report.exit_code(), EXIT_OK);
    }

    #[test]
    fn json_carries_repair_receipts() {
        let mut report = assemble_report(&healthy_inputs());
        report.fix_requested = true;
        report.fixes.push(FixReceipt::applied(
            "terminfo",
            "terminfo/b/bitty".to_string(),
            "no `bitty` terminfo entry".to_string(),
            "compiled into terminfo".to_string(),
            "rm -f 'terminfo/b/bitty'".to_string(),
            "installed into terminfo".to_string(),
        ));
        let json = format_json(&report);
        assert!(json.contains("\"fixes\":["), "{json}");
        for key in [
            "\"id\":\"terminfo\"",
            "\"outcome\":\"applied\"",
            "\"changed\":\"terminfo/b/bitty\"",
            "\"from\":\"no `bitty` terminfo entry\"",
            "\"to\":\"compiled into terminfo\"",
            "\"undo\":\"rm -f 'terminfo/b/bitty'\"",
            "\"detail\":\"installed into terminfo\"",
        ] {
            assert!(json.contains(key), "missing {key}: {json}");
        }
        assert_eq!(
            json.chars().filter(|c| *c == '{').count(),
            json.chars().filter(|c| *c == '}').count(),
            "unbalanced: {json}"
        );
    }
}
