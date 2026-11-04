//! Restricted, single-root module loader for configuration Lua (`require`).
//!
//! Implements the accepted
//! [Config Lua Require Contract](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/config-lua-require-contract.md):
//! one name grammar resolved against exactly one root (`<config-dir>/lua`), a
//! host-side per-generation cache keyed by opened file identity, typed cycle
//! and depth refusal, threefold path containment (lexical, canonical, and a
//! descriptor-anchored no-follow walk from an open root), a length-checked
//! bounded read, per-file and aggregate graph caps, and redacted typed
//! diagnostics whose rendered form contains only an allowed byte set.
//!
//! # Why a second loader
//!
//! The plugin-host rooted loader in [`crate::host`] (`LuaVm::install_require`)
//! resolves against a plugin package tree and is authorised by a manifest.
//! Configuration is free-form user Lua with different budgets and no package
//! trust, so this loader shares no code path with it, is never reachable from
//! plugin or settings VM scope, and is only installed by the configuration
//! evaluation entry point in `bitty-config`.
//!
//! # Accounting
//!
//! The loader never starts a second executor: a module chunk is compiled and
//! called from inside the `require` callback, so instructions, wall clock, and
//! accounted heap all come out of the *same* generation account the entry
//! chunk runs in. Entering a module therefore cannot refresh RC-1 or RC-2 —
//! the property the contract calls out as the one a reviewer should check
//! hardest.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use phodopus::{Callback, CallbackReturn, Context, Function, Table, Value};

use crate::host::BridgeError;
use crate::{LuaVm, VmError};

/// Maximum bytes in one module spec (`a.b.c`).
pub const MAX_MODULE_SPEC_BYTES: usize = 256;

/// Maximum `.`-separated segments in one module spec.
pub const MAX_MODULE_SPEC_SEGMENTS: usize = 16;

/// Maximum bytes in one spec segment.
pub const MAX_MODULE_SEGMENT_BYTES: usize = 64;

/// Maximum nested module evaluations (the require chain depth).
pub const MAX_MODULE_DEPTH: usize = 16;

/// Maximum distinct module identities in one generation.
pub const MAX_MODULE_COUNT: usize = 64;

/// Per-file source ceiling in bytes (mirrors the shipped config-file bound).
pub const MAX_MODULE_FILE_BYTES: usize = 64 * 1024;

/// Per-file source line ceiling (mirrors the shipped config-file bound).
pub const MAX_MODULE_FILE_LINES: usize = 2048;

/// Per-file bytes-on-one-line ceiling (mirrors the shipped config bound).
pub const MAX_MODULE_LINE_BYTES: usize = 4096;

/// Aggregate source ceiling across one generation.
pub const MAX_MODULE_AGGREGATE_BYTES: usize = 256 * 1024;

/// Ceiling for the module-authored `detail` field of a diagnostic.
pub const MAX_MODULE_DETAIL_BYTES: usize = 512;

/// Directory name below the configuration directory that holds modules.
pub const MODULE_DIR_NAME: &str = "lua";

/// Reserved first segment: `bitty` and `bitty.*` are never modules.
const RESERVED_FIRST_SEGMENT: &str = "bitty";

// ── taxonomy ─────────────────────────────────────────────────────────────────

/// Typed refusal code (`E_CONFIG_MODULE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigModuleCode {
    /// Spec fails the grammar.
    Name,
    /// The canonical root could not be canonicalized, opened, or typed.
    Root,
    /// Neither resolution of the spec exists.
    NotFound,
    /// Both resolutions exist.
    Ambiguous,
    /// A `require` re-entered an in-flight identity.
    Cycle,
    /// The require chain exceeded [`MAX_MODULE_DEPTH`].
    Depth,
    /// More than [`MAX_MODULE_COUNT`] distinct identities.
    Count,
    /// A file exceeded the per-file byte, line, or line-length ceiling.
    Size,
    /// The generation source total exceeded [`MAX_MODULE_AGGREGATE_BYTES`].
    Aggregate,
    /// Containment failed.
    Escape,
    /// The opened object is not a regular file.
    NotRegular,
    /// The bounded read failed or was short.
    Read,
    /// The file is not valid Lua source.
    Syntax,
    /// A module chunk raised an error or broke the shape contract.
    Eval,
    /// A generation budget (RC-1/RC-2) tripped inside a module.
    Budget,
}

impl ConfigModuleCode {
    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "E_CONFIG_MODULE_NAME",
            Self::Root => "E_CONFIG_MODULE_ROOT",
            Self::NotFound => "E_CONFIG_MODULE_NOT_FOUND",
            Self::Ambiguous => "E_CONFIG_MODULE_AMBIGUOUS",
            Self::Cycle => "E_CONFIG_MODULE_CYCLE",
            Self::Depth => "E_CONFIG_MODULE_DEPTH",
            Self::Count => "E_CONFIG_MODULE_COUNT",
            Self::Size => "E_CONFIG_MODULE_SIZE",
            Self::Aggregate => "E_CONFIG_MODULE_AGGREGATE",
            Self::Escape => "E_CONFIG_MODULE_ESCAPE",
            Self::NotRegular => "E_CONFIG_MODULE_NOT_REGULAR",
            Self::Read => "E_CONFIG_MODULE_READ",
            Self::Syntax => "E_CONFIG_MODULE_SYNTAX",
            Self::Eval => "E_CONFIG_MODULE_EVAL",
            Self::Budget => "E_CONFIG_MODULE_BUDGET",
        }
    }
}

impl std::fmt::Display for ConfigModuleCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Phase of the generation a refusal came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigModulePhase {
    /// Building the canonical root.
    Root,
    /// Validating the spec grammar.
    Name,
    /// Probing the two resolutions.
    Resolve,
    /// Opening the resolved object through the anchored walk.
    Open,
    /// Reading the bounded source bytes.
    Read,
    /// Compiling the source.
    Parse,
    /// A module chunk raised an error.
    Evaluate,
    /// The returned value broke the shape contract.
    Extract,
    /// An RC-1/RC-2 budget tripped.
    Budget,
}

impl ConfigModulePhase {
    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Name => "name",
            Self::Resolve => "resolve",
            Self::Open => "open",
            Self::Read => "read",
            Self::Parse => "parse",
            Self::Evaluate => "evaluate",
            Self::Extract => "extract",
            Self::Budget => "budget",
        }
    }
}

impl std::fmt::Display for ConfigModulePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which aggregate counter tripped, when a refusal is an aggregate one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigModuleAggregate {
    /// Distinct identity count.
    Count,
    /// Require-chain depth.
    Depth,
    /// Aggregate source bytes.
    Bytes,
    /// An RC-1/RC-2 budget.
    Budget,
}

impl ConfigModuleAggregate {
    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Depth => "depth",
            Self::Bytes => "bytes",
            Self::Budget => "budget",
        }
    }
}

/// One typed loader refusal.
///
/// Field names are the contract's record shape. `module_spec` is the raw
/// `require` argument (bytes, because `E_CONFIG_MODULE_NAME` reports input the
/// grammar rejected by definition). Nothing here carries an absolute path, a
/// canonical root, or an identity tuple — see [`ConfigModuleDiagnostic::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigModuleDiagnostic {
    /// Taxonomy code.
    pub code: ConfigModuleCode,
    /// Phase of the generation.
    pub phase: ConfigModulePhase,
    /// The `require` argument, raw bytes; absent for a root refusal.
    pub module_spec: Option<Vec<u8>>,
    /// Entry, or module spec with the source position the refusal carries.
    pub source: Option<String>,
    /// The aggregate counter that tripped, when applicable.
    pub aggregate: Option<ConfigModuleAggregate>,
    /// Bounded, host- or module-authored detail.
    pub detail: Option<String>,
}

impl ConfigModuleDiagnostic {
    /// Build a diagnostic with only the required fields set.
    #[must_use]
    pub fn new(code: ConfigModuleCode, phase: ConfigModulePhase) -> Self {
        Self {
            code,
            phase,
            module_spec: None,
            source: None,
            aggregate: None,
            detail: None,
        }
    }

    /// Attach the raw `require` argument.
    #[must_use]
    pub fn with_spec(mut self, spec: impl Into<Vec<u8>>) -> Self {
        self.module_spec = Some(spec.into());
        self
    }

    /// Attach the requiring unit / source position.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Attach the tripped aggregate counter.
    #[must_use]
    pub fn with_aggregate(mut self, aggregate: ConfigModuleAggregate) -> Self {
        self.aggregate = Some(aggregate);
        self
    }

    /// Attach a bounded detail message.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The taxonomy code, for callers that branch on it.
    #[must_use]
    pub fn code(&self) -> ConfigModuleCode {
        self.code
    }

    /// Render the record.
    ///
    /// Every field is escaped with [`escape_rendered`] at the point of
    /// rendering, so the emitted bytes are the property the contract asserts:
    /// `0x20`..=`0x7E` plus tab only. Quotes around the free-text fields make
    /// the field boundary visible, so a tab cannot forge a second field.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(128);
        let _ = write!(out, "code={} phase={}", self.code, self.phase);
        if let Some(spec) = self.module_spec.as_deref() {
            let _ = write!(
                out,
                " module_spec=\"{}\"",
                escape_rendered(spec, MAX_MODULE_SPEC_BYTES)
            );
        }
        if let Some(source) = self.source.as_deref() {
            let _ = write!(
                out,
                " source=\"{}\"",
                escape_rendered(source.as_bytes(), MAX_MODULE_SPEC_BYTES)
            );
        }
        if let Some(aggregate) = self.aggregate {
            let _ = write!(out, " aggregate={}", aggregate.as_str());
        }
        if let Some(detail) = self.detail.as_deref() {
            let _ = write!(
                out,
                " detail=\"{}\"",
                escape_rendered(detail.as_bytes(), MAX_MODULE_DETAIL_BYTES)
            );
        }
        out
    }
}

impl std::fmt::Display for ConfigModuleDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// Escape one field into the allowed byte set, after truncation.
///
/// The rule is an allowlist over *bytes*: `0x20`..=`0x7E` plus tab survive,
/// every other byte becomes `\xNN`. Truncation happens first, on the input, at
/// a UTF-8 scalar boundary, and is marked with the ASCII suffix `...`, so an
/// emitted field can never end in half an escape sequence. Bytes at or above
/// `0x80` are escaped as bytes and are therefore never passed through, which
/// is what keeps invalid UTF-8 out of the structured export.
#[must_use]
pub fn escape_rendered(bytes: &[u8], cap: usize) -> String {
    let mut end = bytes.len().min(cap);
    if end < bytes.len() {
        // Back off to a scalar start so truncation cannot split a multi-byte
        // sequence (`0b10xx_xxxx` is a continuation byte).
        while end > 0 && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
    }
    let truncated = end < bytes.len();
    let mut out = String::with_capacity(end + 3);
    for &byte in &bytes[..end] {
        if byte == b'\t' || (0x20..=0x7E).contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    if truncated {
        out.push_str("...");
    }
    out
}

// ── name grammar ─────────────────────────────────────────────────────────────

/// A validated module spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSpec {
    text: String,
}

impl ModuleSpec {
    /// Parse one `require` argument against the contract grammar.
    ///
    /// The grammar is total over the refusal surface: a spec can never name a
    /// parent directory, an absolute path, another volume, or a stream,
    /// because none of those forms is admitted.
    ///
    /// # Errors
    ///
    /// [`ConfigModuleCode::Name`] for every form the grammar rejects.
    pub fn parse(raw: &[u8]) -> Result<Self, ConfigModuleDiagnostic> {
        let refuse = |reason: &str| {
            ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
                .with_spec(raw.to_vec())
                .with_detail(reason.to_string())
        };
        if raw.is_empty() {
            return Err(refuse("module spec is empty"));
        }
        if raw.len() > MAX_MODULE_SPEC_BYTES {
            return Err(refuse("module spec exceeds 256 bytes"));
        }
        if !raw.iter().all(u8::is_ascii) {
            return Err(refuse("module spec must be ASCII"));
        }
        // ASCII from here on, so byte-wise checks and `str` agree.
        let text = std::str::from_utf8(raw).map_err(|_| refuse("module spec must be ASCII"))?;
        if text.starts_with('.') || text.ends_with('.') || text.contains("..") {
            return Err(refuse("module spec must not traverse"));
        }
        if text.contains(['/', '\\', ':', '\0']) {
            return Err(refuse("module spec must not contain a path separator"));
        }
        let segments: Vec<&str> = text.split('.').collect();
        if segments.len() > MAX_MODULE_SPEC_SEGMENTS {
            return Err(refuse("module spec exceeds 16 segments"));
        }
        for segment in &segments {
            if segment.is_empty() {
                return Err(refuse("module spec has an empty segment"));
            }
            if segment.len() > MAX_MODULE_SEGMENT_BYTES {
                return Err(refuse("module spec segment exceeds 64 bytes"));
            }
            let first = segment.as_bytes()[0];
            if !first.is_ascii_alphanumeric() {
                return Err(refuse("module spec segment must start alphanumeric"));
            }
            if !segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(refuse("module spec segment has an invalid character"));
            }
        }
        if segments[0].eq_ignore_ascii_case(RESERVED_FIRST_SEGMENT) {
            return Err(refuse("'bitty' is a reserved first segment"));
        }
        Ok(Self {
            text: text.to_string(),
        })
    }

    /// The spec text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The spec as `a/b/c` — the root-relative stem (no suffix).
    fn path_stem(&self) -> String {
        self.text.replace('.', "/")
    }

    /// The flat resolution, root-relative: `a/b/c.lua`.
    fn flat_relative(&self) -> String {
        format!("{}.lua", self.path_stem())
    }

    /// The init resolution, root-relative: `a/b/c/init.lua`.
    fn init_relative(&self) -> String {
        format!("{}/init.lua", self.path_stem())
    }
}

// ── per-generation state ─────────────────────────────────────────────────────

/// Identity of one opened module file.
///
/// On Unix this is the handle's `(device, inode)` — the object the walk
/// actually opened. The contract's Windows leg names `FILE_ID_INFO`; the
/// stable standard library does not expose it (`windows_by_handle` is
/// unstable) and this crate forbids `unsafe`, so Windows keys the cache by the
/// canonical path instead. That is recorded as an implementation deviation,
/// with its consequence (case-variant and hardlink aliases do not collapse
/// there), rather than described as the contract's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FileIdentity {
    /// Device and inode of the opened handle.
    #[cfg(unix)]
    Unix {
        /// Device number.
        dev: u64,
        /// Inode number.
        ino: u64,
    },
    /// Canonical path, case-folded on a case-insensitive volume.
    #[cfg(not(unix))]
    Canonical(String),
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: &std::fs::Metadata, _canonical: &Path) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self::Unix {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(_metadata: &std::fs::Metadata, canonical: &Path) -> Self {
        Self::Canonical(canonical.to_string_lossy().to_lowercase())
    }
}

/// One cached module value.
struct CachedModule {
    value: phodopus::StashedTable,
}

/// One in-flight module evaluation (a module whose own call has not returned).
struct InFlightModule {
    identity: FileIdentity,
    spec: String,
}

/// Counters a budget dashboard needs, without new thresholds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigModuleStats {
    /// Distinct identities evaluated in this generation.
    pub modules: usize,
    /// Aggregate source bytes charged to this generation.
    pub module_bytes: usize,
}

/// Outcome of opening one spec.
enum ReadOutcome {
    /// A complete identity already holds a value; return it.
    Cached(phodopus::StashedTable),
    /// Fresh source bytes, about to be compiled.
    Fresh {
        identity: FileIdentity,
        bytes: Vec<u8>,
    },
}

/// One host-side `require` step, computed while no VM borrow is held.
enum Step {
    /// The step refused before any work.
    Refused(ConfigModuleDiagnostic),
    /// The step resolved (or refused) after host-side I/O.
    Resolved(Result<ReadOutcome, ConfigModuleDiagnostic>),
}

/// One generation of the loader: root, cache, in-flight chain, and accounts.
pub(crate) struct ConfigModuleGeneration {
    root: PathBuf,
    entry: String,
    canonical_root: Option<PathBuf>,
    #[cfg(unix)]
    root_fd: Option<std::os::fd::OwnedFd>,
    root_error: Option<ConfigModuleDiagnostic>,
    root_attempted: bool,
    cache: HashMap<FileIdentity, CachedModule>,
    in_flight: Vec<InFlightModule>,
    /// The first refusal. Once set the generation is dead: later `require`
    /// calls re-report it, and the entry evaluation reports it even when the
    /// module that caused it swallowed the error with `pcall`.
    error: Option<ConfigModuleDiagnostic>,
    stats: ConfigModuleStats,
}

impl ConfigModuleGeneration {
    fn new(root: PathBuf, entry: String) -> Self {
        Self {
            root,
            entry,
            canonical_root: None,
            #[cfg(unix)]
            root_fd: None,
            root_error: None,
            root_attempted: false,
            cache: HashMap::new(),
            in_flight: Vec::new(),
            error: None,
            stats: ConfigModuleStats::default(),
        }
    }

    fn stats(&self) -> ConfigModuleStats {
        self.stats
    }

    /// The spec of the module currently evaluating, if any.
    fn current_spec(&self) -> Option<String> {
        self.in_flight.last().map(|entry| entry.spec.clone())
    }

    fn refuse(&mut self, diagnostic: ConfigModuleDiagnostic) -> ConfigModuleDiagnostic {
        if self.error.is_none() {
            self.error = Some(diagnostic.clone());
        }
        diagnostic
    }

    /// Canonicalize and open the root once per generation.
    ///
    /// A root that cannot be canonicalized, opened, or typed as a directory
    /// refuses with [`ConfigModuleCode::Root`]. The record names the layout
    /// *kind* only: the canonical root is never printed (contract `N-11`).
    fn ensure_root(&mut self) -> Result<(), ConfigModuleDiagnostic> {
        if let Some(error) = self.root_error.clone() {
            return Err(error);
        }
        if self.root_attempted {
            return Ok(());
        }
        self.root_attempted = true;
        let refuse = |detail: &str, entry: &str| {
            ConfigModuleDiagnostic::new(ConfigModuleCode::Root, ConfigModulePhase::Root)
                .with_source(entry.to_string())
                .with_detail(detail.to_string())
        };
        let canonical = match std::fs::canonicalize(&self.root) {
            Ok(path) => path,
            Err(error) => {
                let diagnostic = refuse(root_layout_detail(&error), &self.entry);
                self.root_error = Some(diagnostic.clone());
                return Err(diagnostic);
            }
        };
        let metadata = match std::fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) => {
                let diagnostic = refuse(root_layout_detail(&error), &self.entry);
                self.root_error = Some(diagnostic.clone());
                return Err(diagnostic);
            }
        };
        if !metadata.is_dir() {
            let diagnostic = refuse("module root is not a directory", &self.entry);
            self.root_error = Some(diagnostic.clone());
            return Err(diagnostic);
        }
        #[cfg(unix)]
        {
            // Open the *derived* root path, not the canonical one: a symlinked
            // final component is `E_CONFIG_MODULE_ROOT` (the contract's
            // verification plan), while symlinked ancestors are resolved once
            // and permitted — that is the dotfiles and redirected-`%APPDATA%`
            // layout both documents require to keep working.
            match unix_open_root(&self.root) {
                Ok(fd) => self.root_fd = Some(fd),
                Err(error) => {
                    let io = std::io::Error::from(error);
                    let diagnostic = refuse(root_layout_detail(&io), &self.entry);
                    self.root_error = Some(diagnostic.clone());
                    return Err(diagnostic);
                }
            }
        }
        self.canonical_root = Some(canonical);
        Ok(())
    }

    /// Resolve one spec to its canonical target and root-relative remainder.
    ///
    /// Order is the contract's (`N-7`): probe both resolutions, decide
    /// `NOT_FOUND` / `AMBIGUOUS`, and only then canonicalize the form that
    /// exists.
    fn resolve(&mut self, spec: &ModuleSpec) -> Result<(PathBuf, String), ConfigModuleDiagnostic> {
        self.ensure_root()?;
        let root = self
            .canonical_root
            .clone()
            .expect("canonical root is set when ensure_root succeeds");
        let flat_relative = spec.flat_relative();
        let init_relative = spec.init_relative();
        if let Err(detail) =
            lexical_containment(&flat_relative).and_then(|()| lexical_containment(&init_relative))
        {
            return Err(ConfigModuleDiagnostic::new(
                ConfigModuleCode::Escape,
                ConfigModulePhase::Resolve,
            )
            .with_spec(spec.as_str().as_bytes().to_vec())
            .with_detail(detail.to_string()));
        }
        let flat = root.join(flat_relative);
        let init = root.join(init_relative);
        let refuse = |code: ConfigModuleCode, detail: &str| {
            ConfigModuleDiagnostic::new(code, ConfigModulePhase::Resolve)
                .with_spec(spec.as_str().as_bytes().to_vec())
                .with_detail(detail.to_string())
        };
        let candidate = match (flat.exists(), init.exists()) {
            (true, true) => {
                return Err(refuse(
                    ConfigModuleCode::Ambiguous,
                    "both the flat module and the init module exist",
                ));
            }
            (false, false) => {
                return Err(refuse(
                    ConfigModuleCode::NotFound,
                    "neither the flat module nor the init module exists",
                ));
            }
            (true, false) => flat,
            (false, true) => init,
        };
        // Canonical containment: the canonical target must stay under the
        // canonical root, which keeps an admitted in-root symlink layout
        // (dotfiles) workable while refusing a link into another tree.
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) => {
                return Err(refuse(
                    ConfigModuleCode::Escape,
                    canonicalize_detail(&error),
                ));
            }
        };
        let remainder = match remainder_beneath(&root, &canonical) {
            Some(remainder) => remainder,
            None => {
                return Err(refuse(
                    ConfigModuleCode::Escape,
                    "canonical target is outside the module root",
                ));
            }
        };
        if remainder.is_empty() {
            return Err(refuse(
                ConfigModuleCode::Escape,
                "canonical target is the module root itself",
            ));
        }
        Ok((canonical, remainder))
    }

    /// Open, type, identify, read, and charge one resolved module.
    fn read_module(
        &mut self,
        spec: &ModuleSpec,
        canonical: &Path,
        remainder: &str,
    ) -> Result<ReadOutcome, ConfigModuleDiagnostic> {
        let refuse = |code: ConfigModuleCode, phase: ConfigModulePhase, detail: &str| {
            ConfigModuleDiagnostic::new(code, phase)
                .with_spec(spec.as_str().as_bytes().to_vec())
                .with_detail(detail.to_string())
        };
        // Test-only window between canonicalization and the anchored open, so
        // the "component swapped after the containment check" control can be
        // driven deterministically. Never compiled into a shipped binary.
        #[cfg(all(test, unix))]
        tests::test_swap_hook(canonical);
        let opened = {
            #[cfg(unix)]
            {
                let root_fd = self
                    .root_fd
                    .as_ref()
                    .expect("root descriptor is open when resolve succeeds");
                unix_open_beneath(root_fd, remainder)
            }
            #[cfg(not(unix))]
            {
                let root = self
                    .canonical_root
                    .clone()
                    .expect("canonical root is set when resolve succeeds");
                windows_open_verified(&root, canonical)
            }
        };
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                // The object type decides a socket, device, or FIFO refusal:
                // `open(2)` on a socket fails with `ENXIO` before any read, so
                // the anchored open's error alone cannot name the code. The
                // stat is no-follow and root-relative, so it decides type
                // without following a linker that replaced a component.
                if let Some(code) = self.probe_non_regular(remainder) {
                    return Err(refuse(
                        code,
                        ConfigModulePhase::Open,
                        "opened object is not a regular file",
                    ));
                }
                return Err(refuse(
                    ConfigModuleCode::Escape,
                    ConfigModulePhase::Open,
                    escape_detail(&error),
                ));
            }
        };
        // Type first, and the open above was non-blocking, so a FIFO, socket,
        // device, or directory is refused before any read can wait.
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                return Err(refuse(
                    ConfigModuleCode::Read,
                    ConfigModulePhase::Read,
                    "fstat on the opened module failed",
                ));
            }
        };
        if !metadata.is_file() {
            return Err(refuse(
                ConfigModuleCode::NotRegular,
                ConfigModulePhase::Open,
                "opened object is not a regular file",
            ));
        }
        let identity = FileIdentity::from_metadata(&metadata, canonical);
        if let Some(value) = self.cache.get(&identity).map(|entry| entry.value.clone()) {
            return Ok(ReadOutcome::Cached(value));
        }
        if self
            .in_flight
            .iter()
            .any(|entry| entry.identity == identity)
        {
            return Err(self.cycle_refusal(spec, &identity));
        }
        if self.stats.modules >= MAX_MODULE_COUNT {
            return Err(refuse(
                ConfigModuleCode::Count,
                ConfigModulePhase::Resolve,
                "generation exceeded 64 distinct module identities",
            )
            .with_aggregate(ConfigModuleAggregate::Count));
        }
        let expected = metadata.len();
        if expected > MAX_MODULE_FILE_BYTES as u64 {
            return Err(refuse(
                ConfigModuleCode::Size,
                ConfigModulePhase::Read,
                "module source exceeds the 64 KiB per-file ceiling",
            ));
        }
        let bytes = match read_bounded(file, expected, MAX_MODULE_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err((code, detail)) => return Err(refuse(code, ConfigModulePhase::Read, detail)),
        };
        if let Err(detail) = check_source_lines(&bytes) {
            return Err(refuse(
                ConfigModuleCode::Size,
                ConfigModulePhase::Read,
                detail,
            ));
        }
        if self.stats.module_bytes.saturating_add(bytes.len()) > MAX_MODULE_AGGREGATE_BYTES {
            return Err(refuse(
                ConfigModuleCode::Aggregate,
                ConfigModulePhase::Read,
                "generation source total exceeds 256 KiB",
            )
            .with_aggregate(ConfigModuleAggregate::Bytes));
        }
        self.stats.module_bytes += bytes.len();
        self.stats.modules += 1;
        Ok(ReadOutcome::Fresh { identity, bytes })
    }

    /// Whether the resolved object is a special file, without following a
    /// link at any component.
    ///
    /// Used only after the anchored open already failed, so a socket, device,
    /// or FIFO can be reported as `E_CONFIG_MODULE_NOT_REGULAR` instead of a
    /// containment refusal. Returns `None` when the stat says nothing useful
    /// (a missing object, or a symlink at a component), leaving the caller
    /// with the escape refusal.
    #[cfg(unix)]
    fn probe_non_regular(&self, remainder: &str) -> Option<ConfigModuleCode> {
        use rustix::fs::AtFlags;
        let root_fd = self.root_fd.as_ref()?;
        let stat = rustix::fs::statat(root_fd, remainder, AtFlags::SYMLINK_NOFOLLOW).ok()?;
        let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        if file_type.is_file() || file_type.is_dir() || file_type.is_symlink() {
            return None;
        }
        Some(ConfigModuleCode::NotRegular)
    }

    #[cfg(not(unix))]
    fn probe_non_regular(&self, _remainder: &str) -> Option<ConfigModuleCode> {
        None
    }

    fn cycle_refusal(
        &mut self,
        spec: &ModuleSpec,
        identity: &FileIdentity,
    ) -> ConfigModuleDiagnostic {
        let mut chain: Vec<String> = self.in_flight.iter().map(|e| e.spec.clone()).collect();
        // Name the re-entered identity by the spec that first started it, so
        // the diagnostic shows the ring in import order.
        if let Some(first) = self
            .in_flight
            .iter()
            .find(|entry| &entry.identity == identity)
            .map(|entry| entry.spec.clone())
        {
            chain.push(first);
        } else {
            chain.push(spec.as_str().to_string());
        }
        self.refuse(
            ConfigModuleDiagnostic::new(ConfigModuleCode::Cycle, ConfigModulePhase::Evaluate)
                .with_spec(spec.as_str().as_bytes().to_vec())
                .with_detail(format!("require cycle: {}", chain.join(" -> "))),
        )
    }

    fn depth_refusal(&mut self, spec: &ModuleSpec) -> ConfigModuleDiagnostic {
        self.refuse(
            ConfigModuleDiagnostic::new(ConfigModuleCode::Depth, ConfigModulePhase::Evaluate)
                .with_spec(spec.as_str().as_bytes().to_vec())
                .with_aggregate(ConfigModuleAggregate::Depth)
                .with_detail("require chain exceeded 16 nested evaluations"),
        )
    }

    fn enter(&mut self, identity: FileIdentity, spec: &str) {
        self.in_flight.push(InFlightModule {
            identity,
            spec: spec.to_string(),
        });
    }

    fn finish(&mut self, identity: FileIdentity, value: phodopus::StashedTable) {
        self.in_flight.pop();
        self.cache.insert(identity, CachedModule { value });
    }

    fn abandon(&mut self) {
        self.in_flight.pop();
    }
}

/// Why a root could not be built, in layout-kind terms only.
fn root_layout_detail(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        // ELOOP: a resolution loop at the root path.
        Some(40) => "module root resolution loop refused",
        // ENOTDIR: a path component is not a directory.
        Some(20) => "module root path is not a directory",
        _ => match error.kind() {
            std::io::ErrorKind::NotFound => "module root directory is missing",
            std::io::ErrorKind::PermissionDenied => "module root is unreadable",
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                "module root path is not a usable directory"
            }
            _ => "module root could not be resolved",
        },
    }
}

/// Canonicalization refusal detail, without a path.
fn canonicalize_detail(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        // ELOOP: a symlink loop inside the module path.
        Some(40) => "module path has a link resolution loop",
        // EACCES: a component is unreadable.
        Some(13) => "module path component is unreadable",
        _ => "module path could not be canonicalized",
    }
}

/// Containment refusal detail for an open failure, without a path.
fn escape_detail(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        // EXDEV: `RESOLVE_BENEATH` refused an absolute or `..` remainder.
        Some(18) => "anchored walk refused an out-of-root remainder",
        // ELOOP: a symlink or magic link at some component.
        Some(40) => "anchored walk refused a link component",
        // ENOTDIR: a non-directory stood in for a path component.
        Some(20) => "anchored walk refused a non-directory component",
        _ => "anchored walk refused the module path",
    }
}

/// Re-verify a joined root-relative path before the loader uses it.
///
/// The grammar already makes a traversal component unrepresentable, and this
/// check runs anyway: the threat-model row for traversal says the lexical
/// check is what a future grammar edit would still hit. It is a pure string
/// check over the joined form, so it cannot be satisfied by a spec the grammar
/// rejected and cannot be skipped by a spec the grammar admitted.
fn lexical_containment(relative: &str) -> Result<(), &'static str> {
    if relative.is_empty() {
        return Err("joined module path is empty");
    }
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err("joined module path is absolute");
    }
    if relative.contains(['\\', ':', '\0']) {
        return Err("joined module path contains a separator or stream form");
    }
    for component in relative.split('/') {
        if component.is_empty() {
            return Err("joined module path has an empty component");
        }
        if component == "." || component == ".." {
            return Err("joined module path has a traversal component");
        }
    }
    Ok(())
}

/// Windows reserved device names, which the canonical step rejects
/// (case-insensitive, with or without an extension).
///
/// A pure predicate so its rules are covered on every host; the loader only
/// consults it on Windows, where the names actually resolve to devices.
#[must_use]
pub fn windows_reserved_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches([' ', '.']);
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
    {
        return true;
    }
    let upper = stem.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (bytes[..3] == *b"COM" || bytes[..3] == *b"LPT")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// Trailing dots or spaces, which Windows strips from a path component.
#[must_use]
pub fn windows_trailing_dot_or_space(component: &str) -> bool {
    component.ends_with(' ') || component.ends_with('.')
}

/// The canonical target's root-relative remainder.
///
/// Case-insensitive on Windows, where the volume's own comparison is
/// case-insensitive, so two differently cased paths cannot smuggle one file
/// past a containment check that already passed.
fn remainder_beneath(root: &Path, canonical: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        let root_text = root.to_string_lossy().to_lowercase().replace('\\', "/");
        let target = canonical
            .to_string_lossy()
            .to_lowercase()
            .replace('\\', "/");
        let remainder = target.strip_prefix(&root_text)?;
        return Some(remainder.trim_start_matches('/').to_string());
    }
    #[cfg(not(windows))]
    {
        canonical
            .strip_prefix(root)
            .ok()
            .map(|remainder| remainder.to_string_lossy().replace('\\', "/"))
    }
}

/// Line-count and line-length bounds over the accepted source bytes.
fn check_source_lines(bytes: &[u8]) -> Result<(), &'static str> {
    let mut lines = 0usize;
    let mut current = 0usize;
    for &byte in bytes {
        current += 1;
        if byte == b'\n' {
            lines += 1;
            current = 0;
        }
        if current > MAX_MODULE_LINE_BYTES {
            return Err("module source exceeds 4096 bytes on one line");
        }
    }
    if current > 0 {
        lines += 1;
    }
    if lines > MAX_MODULE_FILE_LINES {
        return Err("module source exceeds 2048 lines");
    }
    Ok(())
}

/// One bounded, length-checked read.
///
/// `take(cap + 1).read_to_end()` then a byte-count equality against the size
/// the handle reported. A short read, a failed read, or a read at or beyond
/// the ceiling is a typed refusal and **no prefix** is returned: a prefix cut
/// at a statement boundary can still compile, so accepting one would be the
/// fail-open on content the contract forbids.
fn read_bounded(
    file: std::fs::File,
    expected: u64,
    cap: usize,
) -> Result<Vec<u8>, (ConfigModuleCode, &'static str)> {
    use std::io::Read;
    // `O_NONBLOCK` is cleared only after the non-regular rejection, because a
    // blocking open of a FIFO would wait for a writer.
    #[cfg(unix)]
    {
        if let Ok(flags) = rustix::fs::fcntl_getfl(&file) {
            let _ = rustix::fs::fcntl_setfl(&file, flags - rustix::fs::OFlags::NONBLOCK);
        }
    }
    let mut bytes = Vec::new();
    file.take((cap as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            (
                ConfigModuleCode::Read,
                "module read through the handle failed",
            )
        })?;
    if bytes.len() > cap {
        return Err((
            ConfigModuleCode::Read,
            "module read passed the 64 KiB per-file ceiling",
        ));
    }
    if bytes.len() as u64 != expected {
        return Err((
            ConfigModuleCode::Read,
            "module read returned a byte count that differs from the handle size",
        ));
    }
    Ok(bytes)
}

// ── anchored open ────────────────────────────────────────────────────────────

#[cfg(unix)]
fn unix_open_root(root: &Path) -> Result<std::os::fd::OwnedFd, rustix::io::Errno> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Resolve `remainder` beneath `root_fd`.
///
/// Linux uses `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
/// RESOLVE_NO_MAGICLINKS`, so containment is measured from the descriptor and
/// a component swapped after canonicalization is refused where it is opened.
/// Where `openat2` is unavailable (an older kernel, or macOS and the BSDs) the
/// same rule is a per-component `openat` walk with `O_NOFOLLOW | O_DIRECTORY`
/// at every step — never a by-path open.
#[cfg(unix)]
fn unix_open_beneath(
    root_fd: &std::os::fd::OwnedFd,
    remainder: &str,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let oflags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::ResolveFlags;
        let resolve =
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        match rustix::fs::openat2(root_fd, remainder, oflags, Mode::empty(), resolve) {
            Ok(fd) => return Ok(std::fs::File::from(fd)),
            // NOSYS/EINVAL: this kernel or libc has no `openat2`. Fall through
            // to the per-component walk, which is the contract's stated
            // fallback and refuses a swapped component at the component.
            Err(error)
                if error == rustix::io::Errno::NOSYS || error == rustix::io::Errno::INVAL => {}
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }
    let mut current = rustix::io::dup(root_fd)?;
    let components: Vec<&str> = remainder.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let last = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        let final_component = index == last;
        let flags = if final_component {
            oflags | OFlags::NOFOLLOW
        } else {
            oflags | OFlags::NOFOLLOW | OFlags::DIRECTORY
        };
        let fd = rustix::fs::openat(&current, *component, flags, Mode::empty())?;
        if final_component {
            return Ok(std::fs::File::from(fd));
        }
        current = fd;
    }
    Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

// ── Windows walk (compiled and cfg-covered; runs only on Windows) ────────────

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open the link itself, never its target.
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// `FILE_FLAG_BACKUP_SEMANTICS`: needed to open a directory handle.
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `FILE_ATTRIBUTE_REPARSE_POINT`: the object is a reparse point.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Open every component below the canonical root with the reparse-point flag
/// and refuse any object that carries a reparse attribute or is not the
/// expected type.
#[cfg(windows)]
fn windows_open_verified(root: &Path, canonical: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let remainder = canonical.strip_prefix(root).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "outside the module root")
    })?;
    let mut current = root.to_path_buf();
    for component in remainder.components() {
        current.push(component);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&current)?;
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "reparse point refused",
            ));
        }
        if !metadata.is_dir() {
            return Ok(file);
        }
    }
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(canonical)
}

// ── the `require` bridge ─────────────────────────────────────────────────────

impl LuaVm {
    /// Install the restricted configuration `require` global.
    ///
    /// The loader lives for one generation only: the cache, the in-flight
    /// chain, and the byte accounts never outlive the VM. Nothing is read
    /// until the first `require`, so a configuration that splits no modules
    /// never touches the module root and never reports
    /// [`ConfigModuleCode::Root`].
    ///
    /// `entry` labels the generation in the one diagnostic that has no
    /// `module_spec` (a root refusal), so the record still names what was
    /// being evaluated.
    ///
    /// # Errors
    ///
    /// [`VmError::Load`] when a configuration loader is already installed.
    pub fn install_config_require(
        &mut self,
        root: impl Into<PathBuf>,
        entry: impl Into<String>,
    ) -> Result<(), VmError> {
        if self.config_modules.is_some() {
            return Err(VmError::Load(
                "configuration module loader already installed".into(),
            ));
        }
        let generation = Rc::new(RefCell::new(ConfigModuleGeneration::new(
            root.into(),
            entry.into(),
        )));
        let state = generation.clone();
        self.lua.enter(|ctx| {
            crate::stdlib::scrub_config_require_surface(ctx);
            ctx.set_global("require", require_callback(&ctx, state));
        });
        self.config_modules = Some(generation);
        Ok(())
    }

    /// Module counters for the generation's budget record.
    #[must_use]
    pub fn config_module_stats(&self) -> Option<ConfigModuleStats> {
        self.config_modules
            .as_ref()
            .map(|state| state.borrow().stats())
    }

    /// The generation's first loader refusal, if any.
    ///
    /// A refusal ends the generation: the caller turns it into a typed
    /// configuration error regardless of whether the requiring chunk caught
    /// the Lua error with `pcall`.
    #[must_use]
    pub fn config_module_refusal(&self) -> Option<ConfigModuleDiagnostic> {
        self.config_modules
            .as_ref()
            .and_then(|state| state.borrow().error.clone())
    }

    /// The module in flight when the entry chunk failed, for attribution.
    #[must_use]
    pub fn config_module_in_flight(&self) -> Option<String> {
        self.config_modules
            .as_ref()
            .and_then(|state| state.borrow().current_spec())
    }
}

fn require_callback<'gc>(
    ctx: &Context<'gc>,
    state: Rc<RefCell<ConfigModuleGeneration>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _exec, mut stack| {
        let raw = match stack.get(0) {
            Value::String(text) => text.as_bytes().to_vec(),
            _ => {
                return Err(record(
                    ctx,
                    state.as_ref(),
                    ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
                        .with_detail("require expects a module name string"),
                ));
            }
        };
        let spec = match ModuleSpec::parse(&raw) {
            Ok(spec) => spec,
            Err(diagnostic) => return Err(record(ctx, state.as_ref(), diagnostic)),
        };
        let spec_text = spec.as_str().to_string();
        // An already-refused generation re-reports the first refusal and does
        // no further work, so a `pcall` cannot keep loading after a refusal.
        if let Some(diagnostic) = state.borrow().error.clone() {
            return Err(record(ctx, state.as_ref(), diagnostic));
        }

        // Host-side work happens with no VM borrow held, and the state borrow
        // is dropped before any diagnostic is recorded, so a refusal can never
        // re-enter the `RefCell`.
        let step = {
            let mut generation = state.borrow_mut();
            // Depth is counted on the nesting stack, before the 17th
            // evaluation starts.
            if generation.in_flight.len() >= MAX_MODULE_DEPTH {
                Step::Refused(generation.depth_refusal(&spec))
            } else {
                Step::Resolved(match generation.resolve(&spec) {
                    Ok((canonical, remainder)) => {
                        match generation.read_module(&spec, &canonical, &remainder) {
                            Ok(outcome) => Ok(outcome),
                            Err(diagnostic) => Err(generation.refuse(diagnostic)),
                        }
                    }
                    Err(diagnostic) => Err(generation.refuse(diagnostic)),
                })
            }
        };

        let bytes = match step {
            Step::Refused(diagnostic) => return Err(record(ctx, state.as_ref(), diagnostic)),
            Step::Resolved(Ok(ReadOutcome::Cached(stashed))) => {
                let table: Table = ctx.fetch(&stashed);
                stack.replace(ctx, table);
                return Ok(CallbackReturn::Return);
            }
            Step::Resolved(Ok(ReadOutcome::Fresh { identity, bytes })) => {
                state.borrow_mut().enter(identity.clone(), &spec_text);
                (identity, bytes)
            }
            Step::Resolved(Err(diagnostic)) => {
                return Err(record(ctx, state.as_ref(), diagnostic));
            }
        };
        let (identity, source) = bytes;

        // Refuse a precompiled chunk header explicitly instead of letting the
        // parser guess: bytecode never takes the load path.
        if source.starts_with(b"\x1bLua") {
            state.borrow_mut().abandon();
            let diagnostic =
                ConfigModuleDiagnostic::new(ConfigModuleCode::Syntax, ConfigModulePhase::Parse)
                    .with_spec(spec_text.clone().into_bytes())
                    .with_detail("binary chunk header refused; only source modules load");
            let diagnostic = state.borrow_mut().refuse(diagnostic);
            return Err(record(ctx, state.as_ref(), diagnostic));
        }

        let function = match phodopus::Closure::load(ctx, Some(spec_text.as_str()), &source) {
            Ok(closure) => Function::from(closure),
            Err(error) => {
                state.borrow_mut().abandon();
                let message = error.to_string();
                let diagnostic =
                    ConfigModuleDiagnostic::new(ConfigModuleCode::Syntax, ConfigModulePhase::Parse)
                        .with_spec(spec_text.clone().into_bytes())
                        .with_source(source_location(&spec_text, &message))
                        .with_detail(message);
                let diagnostic = state.borrow_mut().refuse(diagnostic);
                return Err(record(ctx, state.as_ref(), diagnostic));
            }
        };

        let store = store_callback(&ctx, state.clone(), identity, spec_text);
        let composed = Function::compose(&ctx, vec![function, store.into()]);
        stack.clear();
        Ok(CallbackReturn::Call {
            function: composed,
            then: None,
        })
    })
}

/// Cache the module's value when its chunk returns, and pop the in-flight
/// entry so a later `require` of the same identity is a cache hit.
fn store_callback<'gc>(
    ctx: &Context<'gc>,
    state: Rc<RefCell<ConfigModuleGeneration>>,
    identity: FileIdentity,
    spec: String,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _exec, mut stack| {
        let shape_ok = stack.len() == 1 && matches!(stack.get(0), Value::Table(_));
        let table = match stack.get(0) {
            Value::Table(table) if shape_ok => table,
            _ => {
                state.borrow_mut().abandon();
                let diagnostic =
                    ConfigModuleDiagnostic::new(ConfigModuleCode::Eval, ConfigModulePhase::Extract)
                        .with_spec(spec.clone().into_bytes())
                        .with_detail("module must return exactly one table");
                let diagnostic = state.borrow_mut().refuse(diagnostic);
                return Err(record(ctx, state.as_ref(), diagnostic));
            }
        };
        let stashed = ctx.stash(table);
        state.borrow_mut().finish(identity.clone(), stashed);
        stack.replace(ctx, table);
        Ok(CallbackReturn::Return)
    })
}

/// Record a refusal once and turn it into the Lua error the caller sees.
fn record<'gc>(
    ctx: Context<'gc>,
    state: &RefCell<ConfigModuleGeneration>,
    diagnostic: ConfigModuleDiagnostic,
) -> phodopus::Error<'gc> {
    let rendered = diagnostic.render();
    state.borrow_mut().refuse(diagnostic);
    // The class names the loader, so a caught error still identifies itself.
    BridgeError::new("config-module", "E_CONFIG_MODULE", rendered).to_error(ctx)
}

/// `chunk:line:` prefix of a parser or runtime message, when it carries one.
pub(crate) fn source_location(spec: &str, message: &str) -> String {
    let prefix = format!("{spec}:");
    if let Some(rest) = message.strip_prefix(&prefix) {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with(':') {
            return format!("{spec}:{digits}");
        }
    }
    spec.to_string()
}

#[cfg(test)]
#[path = "config_require_tests.rs"]
mod tests;
