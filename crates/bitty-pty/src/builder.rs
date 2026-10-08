//! [`PtyBuilder`]: validated spawn configuration for the owned PTY API.
//!
//! Security defaults implemented here (security corpus, threat model
//! "PTY to terminal state", and the ADR-0004 wrapper row):
//!
//! - **Direct argv exec.** The program plus argument vector is passed to the
//!   platform exec path verbatim; no shell, no interpolation, ever.
//! - **Inherited session environment with overrides.** Children inherit the
//!   session environment by default (DEC-0017, Ghostty/Alacritty reference);
//!   explicit builder entries (`TERM=xterm-256color`, `COLORTERM=truecolor`,
//!   `TERM_PROGRAM=bitty`, and caller additions via [`PtyBuilder::env`])
//!   override.
//! - **Graphics-fingerprint sanitization (CTX-0194).** Inherited markers that
//!   claim graphics capabilities bitty lacks are stripped at spawn time (see
//!   [`GRAPHICS_FINGERPRINT_PREFIXES`], [`GRAPHICS_FINGERPRINT_EXACT_KEYS`],
//!   and [`should_strip_graphics_fingerprint`]); `TERM_PROGRAM` is overridden
//!   to [`DEFAULT_TERM_PROGRAM`] so terminfo/term-DB probes (e.g. chafa)
//!   fall back to symbols instead of emitting Kitty-graphics APC that bitty
//!   renders blank. Caller-explicit [`PtyBuilder::env`] entries are applied
//!   after sanitization and therefore win.
//! - **Bounded configuration.** Argument count/size and allowlist size/value
//!   length are capped so a misconfigured caller cannot smuggle unbounded
//!   data into spawn.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::PtyError;

/// Maximum number of entries in the child environment allowlist.
pub const MAX_ENV_ENTRIES: usize = 64;

/// Maximum number of inherited-environment removals.
///
/// Removals carry keys only (no values) and name variables already resident
/// in the spawner's own environment, so this bound is roomier than the
/// allowlist one: a minimized-env caller removes every inherited key
/// outside its keep list, and real session environments (cargo/nextest
/// runners included) hold hundreds of entries.
pub const MAX_ENV_REMOVE_ENTRIES: usize = 1024;

/// Maximum byte length of a single allowlisted environment value.
pub const MAX_ENV_VALUE_BYTES: usize = 4096;

/// Maximum number of arguments (excluding the program itself).
pub const MAX_ARGS: usize = 256;

/// Maximum total byte length across program + arguments.
pub const MAX_ARGV_BYTES: usize = 64 * 1024;

/// Default terminal width in columns.
pub const DEFAULT_COLS: u16 = 80;

/// Default terminal height in rows.
pub const DEFAULT_ROWS: u16 = 24;

/// Default `TERM` value set in the child environment.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// Default `COLORTERM` value set in the child environment.
pub const DEFAULT_COLORTERM: &str = "truecolor";

/// Default `TERM_PROGRAM` value set in the child environment.
///
/// Overriding the inherited `TERM_PROGRAM` (e.g. `ghostty`, `kitty`,
/// `WezTerm`) prevents term-DB probes from assuming Kitty-graphics support
/// bitty lacks (CTX-0194, chafa blank). A caller-explicit
/// `PtyBuilder::env("TERM_PROGRAM", …)` still wins.
pub const DEFAULT_TERM_PROGRAM: &str = "bitty";

/// Inherited key prefixes stripped at spawn time (CTX-0194).
///
/// Every variable whose name starts with one of these prefixes claims a host
/// terminal with graphics capabilities bitty does not implement, so keeping
/// it would mislead term-DB probes (e.g. chafa term-DB matching `ghostty`
/// over `xterm-256color` and emitting Kitty APC):
///
/// - `GHOSTTY_*`: Ghostty resources / bin dir / shell-integration flags.
/// - `WEZTERM_*`: WezTerm pane / socket / executable markers.
/// - `KITTY_*`: Kitty pid / window / listen-socket markers.
pub const GRAPHICS_FINGERPRINT_PREFIXES: &[&str] = &["GHOSTTY_", "WEZTERM_", "KITTY_"];

/// Inherited exact keys stripped at spawn time (CTX-0194).
///
/// Documented, minimal, fail-safe: only keys proven (or directly companion
/// to proven) to advertise graphics bitty lacks. Functional environment
/// (`PATH`, `HOME`, `TERM`, `COLORTERM`, `SHELL`, `LANG`, …) is never listed.
///
/// - `KITTY_PID`, `KITTY_WINDOW_ID`, `KITTY_LISTEN_ON`, `KITTY_PUBLIC_KEY`:
///   explicit Kitty markers (also covered by the `KITTY_` prefix; listed
///   here for auditability).
/// - `VTE_VERSION`: VTE/VTE-based terminals advertise SIXEL in recent
///   versions; bitty implements neither SIXEL nor Kitty-graphics.
/// - `ITERM_SESSION_ID`, `ITERM_PROFILE`: iTerm2 session markers (iTerm2
///   supports inline images bitty lacks).
/// - `LC_TERMINAL`, `LC_TERMINAL_VERSION`: iTerm2 announces itself here;
///   not locale data (`LANG`/`LC_ALL`/`LC_CTYPE` stay untouched).
/// - `TERM_PROGRAM_VERSION`: companion to `TERM_PROGRAM`; the parent
///   version no longer applies once `TERM_PROGRAM` is overridden to
///   [`DEFAULT_TERM_PROGRAM`].
pub const GRAPHICS_FINGERPRINT_EXACT_KEYS: &[&str] = &[
    "KITTY_PID",
    "KITTY_WINDOW_ID",
    "KITTY_LISTEN_ON",
    "KITTY_PUBLIC_KEY",
    "VTE_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_PROGRAM_VERSION",
];

/// Returns true when `key` is a graphics-fingerprint marker stripped from
/// the inherited environment at spawn time (CTX-0194).
///
/// Fail-safe: unknown keys return false (kept). `TERM_PROGRAM` itself
/// returns false — it is overridden to [`DEFAULT_TERM_PROGRAM`], not
/// removed — and functional keys (`PATH`, `HOME`, `TERM`, `COLORTERM`,
/// `SHELL`, …) always return false.
pub fn should_strip_graphics_fingerprint(key: &OsStr) -> bool {
    let text = key.to_string_lossy();
    if GRAPHICS_FINGERPRINT_EXACT_KEYS
        .iter()
        .any(|exact| text == *exact)
    {
        return true;
    }
    GRAPHICS_FINGERPRINT_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Whether one inherited environment entry is hostile: anything that would
/// make the Windows environment-block builder refuse the whole spawn
/// (issue #1801) — an empty key, a key containing `'='` (Windows per-drive
/// entries such as `=C:` arrive this way), or NUL in the key or value.
///
/// The `'='` check runs against the lossy key text (ASCII `'='` survives
/// lossy conversion on every platform) and NUL through the platform
/// predicate, so this is testable on Unix while guarding the Windows spawn.
///
/// Explicit builder entries never reach this predicate: they stay
/// fail-closed in [`PtyBuilder::validate`] (and Lua-provided keys keep the
/// #1751 bridge bound in `bitty-lua`), so sanitizing here cannot weaken
/// either strict surface — only ambient inherited entries are skipped.
#[cfg(any(windows, test))]
pub(crate) fn inherited_env_entry_is_hostile(key: &OsStr, value: &OsStr) -> bool {
    if key.is_empty() {
        return true;
    }
    if key.to_string_lossy().contains('=') {
        return true;
    }
    if os_contains_nul(key) || os_contains_nul(value) {
        return true;
    }
    false
}

/// Maximum characters of one skipped key name in the sanitize log line.
#[cfg(any(windows, test))]
pub(crate) const MAX_NAMED_SKIPPED_KEY_CHARS: usize = 64;

/// Redacts one skipped inherited-env key name for the stderr log line.
///
/// Names are lossy text from an untrusted env block: control characters
/// (a raw ESC in a key name must not reach the terminal) become `'?'` and
/// the name is truncated, mirroring the foreground-job name bound.
#[cfg(any(windows, test))]
pub(crate) fn redact_skipped_key_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(MAX_NAMED_SKIPPED_KEY_CHARS)
        .collect()
}

/// Splits inherited environment entries into kept entries plus the lossy
/// names of skipped hostile entries (see
/// [`inherited_env_entry_is_hostile`]).
///
/// Issue #1801: Windows processes can inherit env blocks with odd keys, and
/// refusing the whole spawn fail-closed leaves the user without a child.
/// The Windows backend drops the skipped entries (logging the count) and
/// spawns with the rest; when every inherited entry is hostile the child
/// still starts with just the explicit builder overrides (the minimal-env
/// fallback). Order of the kept entries is preserved.
#[cfg(any(windows, test))]
pub(crate) fn sanitize_inherited_env(
    inherited: Vec<(OsString, OsString)>,
) -> (Vec<(OsString, OsString)>, Vec<String>) {
    let mut kept = Vec::with_capacity(inherited.len());
    let mut skipped = Vec::new();
    for (key, value) in inherited {
        if inherited_env_entry_is_hostile(&key, &value) {
            skipped.push(key.to_string_lossy().into_owned());
        } else {
            kept.push((key, value));
        }
    }
    (kept, skipped)
}

/// Validated spawn configuration handed to the platform backend.
#[derive(Debug)]
pub(crate) struct SpawnConfig {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    /// Caller-requested inherited-environment removals, applied after the
    /// graphics-fingerprint strip and before the explicit [`PtyBuilder::env`]
    /// overrides (an explicit override still wins over a removal).
    pub(crate) env_remove: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// Builder for spawning a child process inside a new pseudo terminal.
///
/// Construct with [`PtyBuilder::new`], configure, then call
/// [`PtyBuilder::spawn`]. All validation happens up front; an error from
/// `spawn` means nothing was started.
#[derive(Debug, Clone)]
pub struct PtyBuilder {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    env_remove: Vec<OsString>,
    cwd: Option<PathBuf>,
    cols: u16,
    rows: u16,
}

impl PtyBuilder {
    /// Starts a builder for `program`, spawned directly as an argv[0]
    /// executable (absolute path recommended; bare names resolve through the
    /// standard PATH search without any shell involvement).
    ///
    /// Defaults: 80x24 size, inherited working directory, and default
    /// overrides (`TERM=xterm-256color`, `COLORTERM=truecolor`,
    /// `TERM_PROGRAM=bitty`). The child process inherits the session
    /// environment by default (DEC-0017); any entry added via
    /// [`PtyBuilder::env`] overrides the inherited value. Graphics
    /// fingerprint markers (see [`should_strip_graphics_fingerprint`]) are
    /// removed from the inherited environment at spawn time; explicit
    /// [`PtyBuilder::env`] entries are applied after that removal and win.
    pub fn new(program: impl Into<OsString>) -> Self {
        PtyBuilder {
            program: program.into(),
            args: Vec::new(),
            env: vec![
                (OsString::from("TERM"), OsString::from(DEFAULT_TERM)),
                (
                    OsString::from("COLORTERM"),
                    OsString::from(DEFAULT_COLORTERM),
                ),
                (
                    OsString::from("TERM_PROGRAM"),
                    OsString::from(DEFAULT_TERM_PROGRAM),
                ),
            ],
            env_remove: Vec::new(),
            cwd: None,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }

    /// Appends one argument to the child's argv vector.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments to the child's argv vector.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Allowlists one environment variable for the child.
    ///
    /// Setting the same key twice replaces the earlier value while keeping
    /// its original insertion position. Keys must be non-empty and free of
    /// `'='`; keys and values must be free of NUL.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.env.iter_mut().find(|(existing, _)| *existing == key) {
            slot.1 = value;
        } else {
            self.env.push((key, value));
        }
        self
    }

    /// Sets the child's working directory.
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Removes one inherited environment variable from the child.
    ///
    /// The removal applies to the inherited session environment only: it runs
    /// after the graphics-fingerprint strip and before the explicit
    /// [`PtyBuilder::env`] overrides, so a caller-explicit `env` entry for
    /// the same key still wins. Removing a key that is absent (or also added
    /// explicitly) is a no-op, never an error. This is the minimized-env
    /// primitive behind the hosted-editor leaf (W-103 G-2): the caller passes
    /// every inherited key outside its keep-allowlist and the child starts
    /// without ambient credentials. Keys must be non-empty and free of `'='`
    /// and NUL, like [`PtyBuilder::env`].
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        let key = key.into();
        if !self.env_remove.contains(&key) {
            self.env_remove.push(key);
        }
        self
    }

    /// Sets the initial terminal size in columns x rows (both >= 1).
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Validates the configuration and spawns the child in a fresh PTY.
    ///
    /// On unsupported platforms this returns [`PtyError::Unsupported`] and
    /// starts nothing.
    pub fn spawn(self) -> Result<crate::pty::Pty, PtyError> {
        let config = self.validate()?;
        crate::platform::spawn_session(&config)
    }

    pub(crate) fn validate(self) -> Result<SpawnConfig, PtyError> {
        if self.program.is_empty() {
            return Err(PtyError::EmptyProgram);
        }
        let mut nul_free_items: Vec<&OsStr> = Vec::new();
        nul_free_items.push(&self.program);
        nul_free_items.extend(self.args.iter().map(|s| s.as_os_str()));
        for item in nul_free_items.drain(..) {
            if os_contains_nul(item) {
                // NUL cannot survive execve; reject early with a typed
                // validation error (CTX-0478) instead of an opaque upstream
                // failure.
                return Err(PtyError::NulInProgram);
            }
        }
        for (key, value) in &self.env {
            if os_contains_nul(key.as_os_str()) || os_contains_nul(value.as_os_str()) {
                return Err(PtyError::InvalidEnvVar {
                    key: key.to_string_lossy().into_owned(),
                    reason: "must not contain NUL bytes",
                });
            }
        }
        if self.args.len() > MAX_ARGS {
            return Err(PtyError::Upstream(format!(
                "argument count {} exceeds limit {MAX_ARGS}",
                self.args.len()
            )));
        }
        let argv_total: usize =
            os_byte_len(&self.program) + self.args.iter().map(|a| os_byte_len(a)).sum::<usize>();
        if argv_total > MAX_ARGV_BYTES {
            return Err(PtyError::Upstream(format!(
                "argv byte length {argv_total} exceeds limit {MAX_ARGV_BYTES}"
            )));
        }
        if self.env.len() > MAX_ENV_ENTRIES {
            return Err(PtyError::Upstream(format!(
                "environment allowlist size {} exceeds limit {MAX_ENV_ENTRIES}",
                self.env.len()
            )));
        }
        if self.env_remove.len() > MAX_ENV_REMOVE_ENTRIES {
            return Err(PtyError::Upstream(format!(
                "environment removal count {} exceeds limit {MAX_ENV_REMOVE_ENTRIES}",
                self.env_remove.len()
            )));
        }
        for key in &self.env_remove {
            let key_text = key.to_string_lossy();
            if key.is_empty() {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "removal key must not be empty",
                });
            }
            if key_text.contains('=') {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "removal key must not contain '='",
                });
            }
            if os_contains_nul(key.as_os_str()) {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "removal key must not contain NUL bytes",
                });
            }
        }
        for (key, value) in &self.env {
            let key_text = key.to_string_lossy();
            if key.is_empty() {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "key must not be empty",
                });
            }
            if key_text.contains('=') {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "key must not contain '='",
                });
            }
            if os_byte_len(value) > MAX_ENV_VALUE_BYTES {
                return Err(PtyError::InvalidEnvVar {
                    key: key_text.into_owned(),
                    reason: "value exceeds MAX_ENV_VALUE_BYTES",
                });
            }
        }
        if self.cols == 0 || self.rows == 0 {
            return Err(PtyError::InvalidSize {
                cols: self.cols,
                rows: self.rows,
            });
        }
        if let Some(cwd) = &self.cwd {
            if cwd.as_os_str().is_empty() {
                return Err(PtyError::InvalidCwd("path must not be empty".to_owned()));
            }
        }
        Ok(SpawnConfig {
            program: self.program,
            args: self.args,
            env: self.env,
            env_remove: self.env_remove,
            cwd: self.cwd,
            cols: self.cols,
            rows: self.rows,
        })
    }
}

#[cfg(unix)]
fn os_contains_nul(s: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().contains(&0)
}

#[cfg(windows)]
fn os_contains_nul(s: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().any(|unit| unit == 0)
}

#[cfg(unix)]
fn os_byte_len(s: &OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().len()
}

#[cfg(windows)]
fn os_byte_len(s: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().count() * std::mem::size_of::<u16>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_builder() -> PtyBuilder {
        PtyBuilder::new("/bin/cat")
    }

    #[test]
    fn default_env_contains_term_and_colorterm() {
        let cfg = valid_builder().validate().unwrap();
        assert_eq!(cfg.env.len(), 3);
        assert_eq!(cfg.env[0].0, OsString::from("TERM"));
        assert_eq!(cfg.env[0].1, OsString::from(DEFAULT_TERM));
        assert_eq!(cfg.env[1].0, OsString::from("COLORTERM"));
        assert_eq!(cfg.env[1].1, OsString::from(DEFAULT_COLORTERM));
        assert_eq!(cfg.env[2].0, OsString::from("TERM_PROGRAM"));
        assert_eq!(cfg.env[2].1, OsString::from(DEFAULT_TERM_PROGRAM));
    }

    #[test]
    fn default_term_program_is_bitty() {
        let cfg = valid_builder().validate().unwrap();
        let slot = cfg
            .env
            .iter()
            .find(|(k, _)| k == "TERM_PROGRAM")
            .expect("TERM_PROGRAM default present");
        assert_eq!(slot.1, OsString::from(DEFAULT_TERM_PROGRAM));
    }

    #[test]
    fn explicit_term_program_overrides_default() {
        let cfg = valid_builder()
            .env("TERM_PROGRAM", "custom-term")
            .validate()
            .unwrap();
        let matches: Vec<_> = cfg
            .env
            .iter()
            .filter(|(k, _)| k == "TERM_PROGRAM")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, OsString::from("custom-term"));
    }

    #[test]
    fn graphics_fingerprint_exact_keys_are_stripped() {
        for key in GRAPHICS_FINGERPRINT_EXACT_KEYS {
            assert!(
                should_strip_graphics_fingerprint(OsStr::new(key)),
                "{key} should be stripped"
            );
        }
    }

    #[test]
    fn graphics_fingerprint_prefixes_are_stripped() {
        for key in [
            "GHOSTTY_BIN_DIR",
            "GHOSTTY_RESOURCES_DIR",
            "GHOSTTY_SHELL_INTEGRATION_NO_SUDO",
            "WEZTERM_PANE",
            "WEZTERM_EXECUTABLE",
            "WEZTERM_UNIX_SOCKET",
            "KITTY_PID",
            "KITTY_WINDOW_ID",
            "KITTY_LISTEN_ON",
        ] {
            assert!(
                should_strip_graphics_fingerprint(OsStr::new(key)),
                "{key} should be stripped"
            );
        }
    }

    #[test]
    fn functional_env_is_never_stripped() {
        for key in [
            "PATH",
            "HOME",
            "TERM",
            "COLORTERM",
            "TERM_PROGRAM",
            "SHELL",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "USER",
            "LOGNAME",
            "BITTY_PROBE",
        ] {
            assert!(
                !should_strip_graphics_fingerprint(OsStr::new(key)),
                "{key} must be kept"
            );
        }
    }

    #[test]
    fn env_same_key_is_replaced_in_place() {
        let cfg = valid_builder()
            .env("A", "1")
            .env("B", "2")
            .env("A", "3")
            .validate()
            .unwrap();
        assert_eq!(cfg.env.len(), 5); // TERM + COLORTERM + TERM_PROGRAM + A + B
        assert_eq!(cfg.env[3], (OsString::from("A"), OsString::from("3")));
        assert_eq!(cfg.env[4], (OsString::from("B"), OsString::from("2")));
    }

    #[test]
    fn default_size_is_80x24() {
        let cfg = valid_builder().validate().unwrap();
        assert_eq!((cfg.cols, cfg.rows), (DEFAULT_COLS, DEFAULT_ROWS));
    }

    #[test]
    fn size_override_is_kept() {
        let cfg = valid_builder().size(120, 40).validate().unwrap();
        assert_eq!((cfg.cols, cfg.rows), (120, 40));
    }

    #[test]
    fn env_key_with_equals_is_rejected() {
        let err = valid_builder().env("A=B", "v").validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { ref key, .. } if key == "A=B"));
    }

    #[test]
    fn empty_env_key_is_rejected() {
        let err = valid_builder().env("", "v").validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { .. }));
    }

    #[test]
    fn oversized_env_value_is_rejected() {
        let big = "x".repeat(MAX_ENV_VALUE_BYTES + 1);
        let err = valid_builder().env("BIG", big).validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { reason, .. }
                if reason.contains("MAX_ENV_VALUE_BYTES")));
    }

    #[test]
    fn empty_program_is_rejected() {
        // pty-gate-exempt: builder validation only (`validate()` never
        // spawns); passes on every platform without a PTY (CTX-0267).
        let err = PtyBuilder::new("").validate().unwrap_err();
        assert!(matches!(err, PtyError::EmptyProgram));
    }

    #[test]
    fn zero_size_is_rejected() {
        let err = valid_builder().size(0, 24).validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidSize { cols: 0, rows: 24 }));
        let err = valid_builder().size(80, 0).validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidSize { cols: 80, rows: 0 }));
    }

    #[test]
    fn too_many_args_are_rejected() {
        let err = valid_builder()
            .args((0..=MAX_ARGS).map(|i| i.to_string()))
            .validate()
            .unwrap_err();
        assert!(matches!(err, PtyError::Upstream(ref msg) if msg.contains("argument count")));
    }

    #[cfg(unix)]
    #[test]
    fn nul_in_arg_is_rejected_on_unix() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'a', 0, b'b']);
        let err = valid_builder().arg(bad).validate().unwrap_err();
        // CTX-0478: NUL is a typed program/NUL rejection, not an opaque
        // upstream/backend failure.
        assert!(matches!(err, PtyError::NulInProgram));
    }

    #[cfg(unix)]
    #[test]
    fn nul_in_env_value_is_rejected_on_unix() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'v', 0]);
        let err = valid_builder().env("K", bad).validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { .. }));
    }

    #[test]
    fn cwd_is_preserved() {
        let cfg = valid_builder().cwd("/tmp").validate().unwrap();
        assert_eq!(cfg.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn env_remove_is_stored_deduplicated() {
        let cfg = valid_builder()
            .env_remove("GHOSTLY_TOKEN")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("GHOSTLY_TOKEN")
            .validate()
            .unwrap();
        assert_eq!(
            cfg.env_remove,
            vec![
                OsString::from("GHOSTLY_TOKEN"),
                OsString::from("AWS_SECRET_ACCESS_KEY"),
            ]
        );
        // Explicit allowlist entries are untouched by removals.
        assert_eq!(cfg.env.len(), 3);
    }

    #[test]
    fn env_remove_defaults_to_empty() {
        let cfg = valid_builder().validate().unwrap();
        assert!(cfg.env_remove.is_empty());
    }

    #[test]
    fn env_remove_rejects_bad_keys() {
        let err = valid_builder().env_remove("").validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { .. }));
        let err = valid_builder().env_remove("A=B").validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { .. }));
    }

    #[test]
    fn env_remove_over_cap_is_rejected() {
        let builder = (0..=MAX_ENV_REMOVE_ENTRIES)
            .fold(valid_builder(), |b, i| b.env_remove(format!("SCRUB_{i}")));
        let err = builder.validate().unwrap_err();
        assert!(matches!(err, PtyError::Upstream(ref msg) if msg.contains("removal count")));
    }

    #[test]
    fn sanitize_keeps_valid_entries_and_reports_nothing_skipped() {
        // Issue #1801: the common case — a clean inherited block passes
        // through untouched with a zero skipped count.
        let (kept, skipped) = sanitize_inherited_env(vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("HOME"), OsString::from("/home/u")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
        ]);
        assert!(skipped.is_empty());
        assert_eq!(
            kept,
            vec![
                (OsString::from("PATH"), OsString::from("/bin")),
                (OsString::from("HOME"), OsString::from("/home/u")),
                (OsString::from("LANG"), OsString::from("C.UTF-8")),
            ]
        );
    }

    #[test]
    fn sanitize_skips_equals_and_empty_keys_with_count_and_names() {
        // Issue #1801: Windows per-drive markers (`=C:`) and other odd
        // inherited keys are skipped — counted and named — while the rest
        // survive in order, so the spawn proceeds with a minimal-env
        // fallback when everything is hostile.
        let (kept, skipped) = sanitize_inherited_env(vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("=C:"), OsString::from("C:\\work")),
            (OsString::from("A=B"), OsString::from("v")),
            (OsString::from(""), OsString::from("v")),
            (OsString::from("HOME"), OsString::from("/home/u")),
        ]);
        assert_eq!(kept.len(), 2, "only PATH and HOME survive");
        assert_eq!(kept[0].0, OsString::from("PATH"));
        assert_eq!(kept[1].0, OsString::from("HOME"));
        assert_eq!(skipped.len(), 3, "skipped count stays exact");
        assert!(
            skipped.iter().any(|name| name == "=C:"),
            "drive marker named"
        );
        assert!(skipped.iter().any(|name| name == "A=B"));
    }

    #[test]
    fn sanitize_skips_nul_entries() {
        #[cfg(unix)]
        fn nul_os(bytes: &[u8]) -> OsString {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(bytes.to_vec())
        }
        #[cfg(windows)]
        fn nul_os(units: &[u16]) -> OsString {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(units)
        }
        #[cfg(unix)]
        let hostile = vec![
            (nul_os(b"K\0"), OsString::from("v")),
            (OsString::from("K"), nul_os(b"v\0")),
            (OsString::from("OK"), OsString::from("ok")),
        ];
        #[cfg(windows)]
        let hostile = vec![
            (nul_os(&['K' as u16, 0]), OsString::from("v")),
            (OsString::from("K"), nul_os(&['v' as u16, 0])),
            (OsString::from("OK"), OsString::from("ok")),
        ];
        let (kept, skipped) = sanitize_inherited_env(hostile);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, OsString::from("OK"));
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn sanitize_empty_inherited_block_is_empty() {
        let (kept, skipped) = sanitize_inherited_env(Vec::new());
        assert!(kept.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn explicit_drive_style_key_stays_rejected() {
        // Fail-closed parity for caller-provided entries: the sanitizer
        // only ever touches ambient inherited entries, so an explicit
        // `=C:`-shaped key is still a validation error, never a silent
        // skip (no regression on the strict builder bound, sibling to the
        // #1751 Lua-bridge strictness).
        let err = valid_builder().env("=C:", "v").validate().unwrap_err();
        assert!(matches!(err, PtyError::InvalidEnvVar { .. }));
    }

    #[test]
    fn skipped_key_names_are_redacted_for_stderr() {
        // CodeRabbit hardening on #1823: names come from an untrusted env
        // block, so control characters (including ESC) must not reach the
        // terminal raw, and long names are truncated while the count stays
        // exact.
        assert_eq!(redact_skipped_key_name("=C:"), "=C:");
        assert_eq!(
            redact_skipped_key_name("A\x1b]0;pwned\x07B"),
            "A?]0;pwned?B"
        );
        let long = "K".repeat(MAX_NAMED_SKIPPED_KEY_CHARS + 16);
        let redacted = redact_skipped_key_name(&long);
        assert_eq!(redacted.len(), MAX_NAMED_SKIPPED_KEY_CHARS);
        assert!(redacted.chars().all(|c| c == 'K'));
    }
}
