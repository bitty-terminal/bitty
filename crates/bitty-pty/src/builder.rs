//! [`PtyBuilder`]: validated spawn configuration for the owned PTY API.
//!
//! Security defaults implemented here (security corpus, threat model
//! "PTY to terminal state", and the ADR-0004 wrapper row):
//!
//! - **Direct argv exec.** The program plus argument vector is passed to the
//!   platform exec path verbatim; no shell, no interpolation, ever.
//! - **Minimal child environment.** The inherited environment is stripped;
//!   only explicitly allowlisted entries are forwarded, plus a single
//!   `TERM` entry by default (override via [`PtyBuilder::env`]).
//! - **Bounded configuration.** Argument count/size and allowlist size/value
//!   length are capped so a misconfigured caller cannot smuggle unbounded
//!   data into spawn.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::PtyError;

/// Maximum number of entries in the child environment allowlist.
pub const MAX_ENV_ENTRIES: usize = 64;

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

/// Validated spawn configuration handed to the platform backend.
#[derive(Debug)]
pub(crate) struct SpawnConfig {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
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
    cwd: Option<PathBuf>,
    cols: u16,
    rows: u16,
}

impl PtyBuilder {
    /// Starts a builder for `program`, spawned directly as an argv[0]
    /// executable (absolute path recommended; bare names resolve through the
    /// standard PATH search without any shell involvement).
    ///
    /// Defaults: 80x24 size, inherited working directory, and a minimal
    /// environment of exactly one variable (`TERM=xterm-256color`). Every
    /// other environment entry must be added explicitly with
    /// [`PtyBuilder::env`].
    pub fn new(program: impl Into<OsString>) -> Self {
        PtyBuilder {
            program: program.into(),
            args: Vec::new(),
            env: vec![(OsString::from("TERM"), OsString::from(DEFAULT_TERM))],
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
                // NUL cannot survive execve; reject early with a clear error
                // instead of an opaque upstream failure.
                return Err(PtyError::Upstream(
                    "program and arguments must not contain NUL bytes".to_owned(),
                ));
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
    s.as_wide().contains(&0)
}

#[cfg(unix)]
fn os_byte_len(s: &OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().len()
}

#[cfg(windows)]
fn os_byte_len(s: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    s.as_wide().len() * std::mem::size_of::<u16>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_builder() -> PtyBuilder {
        PtyBuilder::new("/bin/cat")
    }

    #[test]
    fn default_env_is_minimal_term_only() {
        let cfg = valid_builder().validate().unwrap();
        assert_eq!(cfg.env.len(), 1);
        assert_eq!(cfg.env[0].0, OsString::from("TERM"));
        assert_eq!(cfg.env[0].1, OsString::from(DEFAULT_TERM));
    }

    #[test]
    fn env_same_key_is_replaced_in_place() {
        let cfg = valid_builder()
            .env("A", "1")
            .env("B", "2")
            .env("A", "3")
            .validate()
            .unwrap();
        assert_eq!(cfg.env.len(), 3); // TERM + A + B
        assert_eq!(cfg.env[1], (OsString::from("A"), OsString::from("3")));
        assert_eq!(cfg.env[2], (OsString::from("B"), OsString::from("2")));
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
        assert!(matches!(err, PtyError::Upstream(msg) if msg.contains("NUL")));
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
}
