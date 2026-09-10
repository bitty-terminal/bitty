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

use std::path::PathBuf;
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
                "install the missing families (notably `{}` from `noto-fonts` for braille TUI graphs)",
                missing.first().unwrap_or(&"Noto Sans Symbols 2")
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

/// Image protocols, reported honestly: kitty graphics is a bounded
/// placeholder stub and sixel is unsupported, so this check is informational
/// (warn) rather than a pass.
#[must_use]
pub fn check_image_protocols() -> DoctorCheck {
    DoctorCheck::warn(
        "images",
        "Image protocols",
        "no action needed: text, braille/block fallbacks, and hyperlinks work; inline images arrive in a later slice"
            .to_string(),
        "kitty-graphics: placeholder stub only; sixel: unsupported".to_string(),
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

    /// Process exit code: `0` when nothing fails, else the strongest failing
    /// category code (`1` for generic failures).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        let mut worst = EXIT_OK;
        for check in &self.checks {
            if check.status == DoctorStatus::Fail && check.code > worst {
                worst = check.code;
            }
        }
        worst
    }

    /// Machine `ok` flag: true only when nothing fails.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.exit_code() == EXIT_OK
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
            check_image_protocols(),
            check_plugins_safe_mode(),
        ],
    }
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

/// Short usage for `bitty doctor` (stderr, fail-closed exit 2).
#[must_use]
pub fn doctor_usage() -> String {
    "usage: bitty doctor [--format table|json|jsonl] [--no-color] [--config PATH] [--profile NAME]"
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
    if let Some(worst) = report.worst_failure() {
        let _ = write!(
            out,
            ",\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}}",
            error_class_for(worst.code),
            json_escape(worst.id),
            json_escape(if worst.hint.is_empty() {
                &worst.detail
            } else {
                &worst.hint
            }),
        );
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

/// Renders human table output (not a machine contract).
#[must_use]
pub fn format_table(report: &DoctorReport, no_color: bool) -> String {
    let summary = report.summary();
    let mut out = format!("bitty doctor — {}\n", report.version);
    for check in &report.checks {
        out.push_str(&table_row(check, no_color));
        out.push('\n');
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
#[must_use]
pub fn probe_font(family: &str) -> Option<bool> {
    if family.trim().is_empty() {
        return Some(false);
    }
    let out = run_bounded("fc-match", &[family], 3)?;
    Some(out.status.success())
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

use crate::cli::{Args, version_text};
use crate::config_cli::load_merged_config;

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
    let version = version_text();
    let (exe_ok, exe_detail) = match std::env::current_exe() {
        Ok(path) => (true, path.display().to_string()),
        Err(_) => (false, String::new()),
    };
    let (config, keymaps, font_chain) = match load_merged_config(args) {
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
            let keymaps = match bitty_config::keymap::resolve_keymaps(&loaded.merged.effective) {
                Ok(maps) => KeymapInput::Ok(maps.len()),
                Err(err) => KeymapInput::Err(err.to_string()),
            };
            let chain = loaded.merged.effective.font.fallback_chain();
            (config, keymaps, chain)
        }
        Err(message) => {
            let chain: Vec<String> = bitty_config::types::FONT_FALLBACK_CHAIN
                .iter()
                .map(|name| (*name).to_string())
                .collect();
            (ConfigInput::Invalid(message), KeymapInput::Skipped, chain)
        }
    };
    let font_tool_available = find_on_path("fc-match").is_some();
    let families: Vec<(String, bool)> = font_chain
        .into_iter()
        .map(|family| {
            let present = probe_font(&family).unwrap_or(false);
            (family, present)
        })
        .collect();
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
        Some(name) => probe_terminfo(name),
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
    DoctorInputs {
        version,
        exe_ok,
        exe_detail,
        config,
        keymaps,
        font_tool_available,
        families,
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
    let inputs = collect_doctor_inputs(args);
    let report = assemble_report(&inputs);
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
        }
    }

    #[test]
    fn all_pass_yields_exit_zero() {
        let report = assemble_report(&healthy_inputs());
        assert_eq!(report.exit_code(), EXIT_OK);
        assert!(report.ok());
        assert_eq!(report.checks.len(), 12);
        assert!(report.checks.iter().all(|c| c.status == DoctorStatus::Pass
            || c.id == "images"
            || c.id == "config"
            || c.id == "keymaps"));
        // images is informational-warn by design; everything else passes.
        let summary = report.summary();
        assert_eq!(summary.fail, 0);
        assert_eq!(summary.pass + summary.warn, 12);
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
            "display",
            "gpu",
            "clipboard",
            "pty",
            "terminfo",
            "shell",
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
        let out = run_bounded("true", &[], 3);
        assert!(out.is_some());
        assert!(out.expect("output").status.success());
    }

    #[test]
    fn run_bounded_rejects_bad_input() {
        assert_eq!(run_bounded("", &[], 1), None);
        assert_eq!(
            run_bounded("definitely-not-a-bitty-binary-xyz", &[], 1),
            None
        );
    }

    #[test]
    fn run_bounded_kills_hung_command() {
        // `sleep 30` must die at the 1s deadline, not run to completion.
        let start = Instant::now();
        let out = run_bounded("sleep", &["30"], 1);
        assert_eq!(out, None);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kill took too long: {:?}",
            start.elapsed()
        );
    }
}
