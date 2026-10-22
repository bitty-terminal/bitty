//! Bounded user-authored theme files (CTX-2464, OQ-047).
//!
//! A local theme is a flat, data-only TOML file under
//! `$XDG_CONFIG_HOME/bitty/themes/*.toml` (platform roots come from
//! [`crate::file::config_dir_with_platform_env`]; no host path is
//! hard-coded). Local themes are opt-in: the static 30-preset registry stays
//! the zero-I/O default, and a file is read only when the config selects it
//! explicitly through [`crate::theme::LOCAL_THEME_PREFIX`]
//! (`appearance.theme = "local:sunset"`). A local name can never change the
//! meaning of a built-in preset name.
//!
//! # Schema (v1, data only)
//!
//! ```toml
//! name = "sunset"                 # required, canonical local slug
//! category = "dark"               # required: "dark" | "light"
//! aliases = ["sunset-warm"]       # optional, <= 8 canonical slugs
//! source = "https://example"      # optional provenance URL
//! license = "MIT"                 # optional
//! background = "#1e1e2e"          # required, canonical #RRGGBB
//! foreground = "#cdd6f4"
//! cursor = "#f5e0dc"
//! selection = "#313244"
//! ansi = ["#...", ... 16 entries] # required, exactly 16 #RRGGBB strings
//! border_focused = "#33CCFF"      # optional #RRGGBB or #RRGGBBAA
//! border_idle = "#595959AA"       # optional #RRGGBB or #RRGGBBAA
//! ```
//!
//! The parser is a bounded flat-key TOML subset (string and string-array
//! values only, sharing the value helpers of the legacy per-plugin reader):
//! no sections, no dotted keys, no tables, no code. Unknown fields, duplicate
//! keys, malformed hex, wrong ANSI cardinality, and non-canonical names fail
//! closed with a bounded per-file diagnostic; the module never panics on
//! hostile input.
//!
//! # Readability gate
//!
//! Every accepted local theme runs the same contrast policy as the built-in
//! catalog ([`crate::theme::contrast_violations`]) plus the focused-outline
//! floor (AC-1 `>= 3:1` against the theme background, AC-2 `>= 3:1` against
//! the idle outline when no width cue applies). A failing theme is rejected
//! at load and reported by `bitty list themes`; it is never adopted.
//!
//! # Resource bounds
//!
//! At most [`MAX_LOCAL_THEME_FILES`] `.toml` candidates, each at most
//! [`MAX_LOCAL_THEME_BYTES`], one line at most
//! [`MAX_LOCAL_THEME_LINE_BYTES`], and only regular files: symlinks are
//! rejected without being followed, so the loader cannot traverse outside
//! the themes directory.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::theme::{
    BITTY_DARK, CustomPalette, LOCAL_THEME_PREFIX, MAX_LOCAL_THEME_NAME_BYTES, MIN_TEXT_CONTRAST,
    MIN_UI_CONTRAST, Theme, ThemeCategory, is_valid_local_theme_name, parse_theme_selection,
};

/// Directory name under the resolved config root.
pub const LOCAL_THEME_DIR_NAME: &str = "themes";

/// File extension accepted by the loader.
pub const LOCAL_THEME_EXTENSION: &str = "toml";

/// Maximum `.toml` candidates in the themes directory.
pub const MAX_LOCAL_THEME_FILES: usize = 64;

/// Maximum entries walked in the themes directory (including non-`.toml`
/// entries), so a directory flood cannot make the scan unbounded.
pub const MAX_LOCAL_THEME_DIR_ENTRIES: usize = 4096;

/// Maximum bytes read from one theme file (64 KiB).
pub const MAX_LOCAL_THEME_BYTES: usize = 64 * 1024;

/// Maximum bytes in one theme-file line.
pub const MAX_LOCAL_THEME_LINE_BYTES: usize = 4096;

/// Maximum accepted aliases per theme.
pub const MAX_LOCAL_THEME_ALIASES: usize = 8;

/// Maximum provenance/license string bytes.
pub const MAX_LOCAL_THEME_METADATA_BYTES: usize = 512;

/// One accepted local theme file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTheme {
    /// Canonical local slug (`name` field).
    pub name: String,
    /// Dark/light classification; drives the contrast policy's text roles.
    pub category: ThemeCategory,
    /// Alternate local slugs accepted in `local:<alias>` selections.
    pub aliases: Vec<String>,
    /// Optional upstream provenance URL.
    pub provenance: String,
    /// Optional license identifier.
    pub license: String,
    /// Parsed palette, including optional outline tokens.
    pub palette: CustomPalette,
    /// File name inside the themes directory (diagnostics only).
    pub file_name: String,
}

/// One rejected file (or directory-level problem), in stable file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalThemeDiagnostic {
    /// File name, or a stable placeholder for a directory-level problem.
    pub file: String,
    /// Bounded English reason (one line, no file bytes echoed).
    pub message: String,
}

/// Result of scanning the themes directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalThemeCatalog {
    /// Directory that was scanned, when a config root resolved.
    pub dir: Option<PathBuf>,
    /// Accepted themes, sorted by name.
    pub themes: Vec<LocalTheme>,
    /// Per-file diagnostics; rejected files never appear in `themes`.
    pub diagnostics: Vec<LocalThemeDiagnostic>,
}

impl LocalThemeCatalog {
    /// Finds an accepted theme by canonical name or alias.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&LocalTheme> {
        let name = name.trim().to_ascii_lowercase();
        self.themes
            .iter()
            .find(|theme| theme.name == name || theme.aliases.iter().any(|alias| alias == &name))
    }

    /// Canonical names in stable order, for bounded diagnostics.
    #[must_use]
    pub fn available_names(&self) -> Vec<&str> {
        self.themes
            .iter()
            .map(|theme| theme.name.as_str())
            .collect()
    }
}

/// A local theme selection that was applied to an effective config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLocalTheme {
    /// Canonical slug from the file.
    pub name: String,
    /// Dark/light classification.
    pub category: ThemeCategory,
    /// File name inside the themes directory.
    pub file_name: String,
}

/// Typed parse/readability failure for one local theme file.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalThemeError {
    /// File exceeds [`MAX_LOCAL_THEME_BYTES`].
    Oversize {
        /// Actual byte length.
        actual: usize,
    },
    /// File is not valid UTF-8.
    InvalidUtf8,
    /// One line exceeds [`MAX_LOCAL_THEME_LINE_BYTES`].
    LineTooLong {
        /// 1-based line number.
        line: usize,
    },
    /// Structural garbage or an invalid value.
    Malformed {
        /// 1-based line number.
        line: usize,
        /// Bounded reason.
        reason: String,
    },
    /// A field outside the closed schema.
    UnknownField {
        /// 1-based line number.
        line: usize,
        /// Field name (bounded key grammar, never file bytes).
        field: String,
    },
    /// A required field is absent.
    MissingField(&'static str),
    /// A theme fails the readability gate.
    Unreadable {
        /// Role label (for example `foreground` or `border.focused`).
        role: &'static str,
        /// Measured WCAG contrast ratio.
        ratio: f64,
        /// Applicable floor.
        floor: f64,
    },
}

impl std::fmt::Display for LocalThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversize { actual } => write!(
                f,
                "theme file exceeds {MAX_LOCAL_THEME_BYTES} bytes ({actual} bytes)"
            ),
            Self::InvalidUtf8 => f.write_str("theme file must be valid UTF-8"),
            Self::LineTooLong { line } => {
                write!(f, "line {line} exceeds {MAX_LOCAL_THEME_LINE_BYTES} bytes")
            }
            Self::Malformed { line: 0, reason } => write!(f, "theme file malformed: {reason}"),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::UnknownField { line, field } => write!(f, "line {line}: unknown field '{field}'"),
            Self::MissingField(field) => write!(f, "missing required field '{field}'"),
            Self::Unreadable { role, ratio, floor } => write!(
                f,
                "readability: {role} contrast {ratio:.2}:1 is below {floor:.1}:1"
            ),
        }
    }
}

impl std::error::Error for LocalThemeError {}

/// Resolved themes directory from the live environment (platform roots).
#[must_use]
pub fn local_themes_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let appdata = std::env::var("APPDATA").ok();
    let localappdata = std::env::var("LOCALAPPDATA").ok();
    local_themes_dir_with_platform_env(
        xdg.as_deref(),
        home.as_deref(),
        appdata.as_deref(),
        localappdata.as_deref(),
    )
}

/// Pure themes-directory resolver with injected platform roots.
#[must_use]
pub fn local_themes_dir_with_platform_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
    appdata: Option<&str>,
    localappdata: Option<&str>,
) -> Option<PathBuf> {
    crate::file::config_dir_with_platform_env(xdg_config_home, home, appdata, localappdata)
        .map(|dir| dir.join(LOCAL_THEME_DIR_NAME))
}

/// Scans the live themes directory. Errors are data, never panics.
#[must_use]
pub fn load_local_themes() -> LocalThemeCatalog {
    match local_themes_dir() {
        Some(dir) => load_local_themes_from_dir(&dir),
        None => LocalThemeCatalog::default(),
    }
}

/// Scans `dir` for bounded, valid local theme files.
///
/// A missing directory is an empty catalog (not an error). Exceeding
/// [`MAX_LOCAL_THEME_FILES`] rejects the whole directory fail-closed: the
/// catalog keeps the diagnostic and no themes, so a hostile directory cannot
/// silently get a truncated subset.
#[must_use]
pub fn load_local_themes_from_dir(dir: &Path) -> LocalThemeCatalog {
    let mut catalog = LocalThemeCatalog {
        dir: Some(dir.to_path_buf()),
        ..LocalThemeCatalog::default()
    };
    // A symlinked themes directory would make every child reachable through
    // a path outside the config root: reject it wholesale, never resolve it.
    if let Ok(metadata) = std::fs::symlink_metadata(dir) {
        if metadata.file_type().is_symlink() {
            catalog.diagnostics.push(LocalThemeDiagnostic {
                file: String::from(LOCAL_THEME_DIR_NAME),
                message: String::from("themes directory is a symlink; use a real directory"),
            });
            return catalog;
        }
        if !metadata.is_dir() {
            catalog.diagnostics.push(LocalThemeDiagnostic {
                file: String::from(LOCAL_THEME_DIR_NAME),
                message: String::from("themes path is not a directory"),
            });
            return catalog;
        }
    }
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return catalog,
        Err(_) => {
            catalog.diagnostics.push(LocalThemeDiagnostic {
                file: String::from(LOCAL_THEME_DIR_NAME),
                message: String::from("themes directory is not readable"),
            });
            return catalog;
        }
    };
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut too_many = false;
    let mut scanned = 0usize;
    for entry in read_dir {
        scanned += 1;
        if scanned > MAX_LOCAL_THEME_DIR_ENTRIES {
            catalog.diagnostics.push(LocalThemeDiagnostic {
                file: String::from(LOCAL_THEME_DIR_NAME),
                message: format!(
                    "contains more than {MAX_LOCAL_THEME_DIR_ENTRIES} directory entries; keep the themes directory small"
                ),
            });
            return catalog;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                catalog.diagnostics.push(LocalThemeDiagnostic {
                    file: String::from(LOCAL_THEME_DIR_NAME),
                    message: String::from("directory entry is not readable"),
                });
                continue;
            }
        };
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !file_name.to_ascii_lowercase().ends_with(".toml") {
            continue;
        }
        if files.len() == MAX_LOCAL_THEME_FILES {
            too_many = true;
            break;
        }
        files.push((file_name, entry.path()));
    }
    if too_many {
        catalog.diagnostics.push(LocalThemeDiagnostic {
            file: String::from(LOCAL_THEME_DIR_NAME),
            message: format!(
                "contains more than {MAX_LOCAL_THEME_FILES} .toml files; at most {MAX_LOCAL_THEME_FILES} are accepted"
            ),
        });
        return catalog;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    // Name and alias collisions are resolved by file order: the first file
    // wins and every later duplicate is rejected with a diagnostic, so a
    // selection can never be ambiguous.
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    for (file_name, path) in files {
        match read_one(&file_name, &path) {
            Ok(theme) => {
                let mut collision = None;
                for candidate in std::iter::once(&theme.name).chain(theme.aliases.iter()) {
                    if let Some(first) = claimed.get(candidate) {
                        collision = Some((candidate.clone(), first.clone()));
                        break;
                    }
                }
                if let Some((name, first)) = collision {
                    catalog.diagnostics.push(LocalThemeDiagnostic {
                        file: file_name,
                        message: format!(
                            "name or alias '{name}' is already provided by '{first}'; choose a unique name"
                        ),
                    });
                    continue;
                }
                for candidate in std::iter::once(&theme.name).chain(theme.aliases.iter()) {
                    claimed.insert(candidate.clone(), theme.file_name.clone());
                }
                catalog.themes.push(theme);
            }
            Err(error) => catalog.diagnostics.push(LocalThemeDiagnostic {
                file: file_name,
                message: error.to_string(),
            }),
        }
    }
    catalog.themes.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// Applies an explicit `local:<name>` selection to `effective`.
///
/// The built-in/zero-I/O path returns `Ok(None)` without touching the
/// filesystem. A local selection scans [`local_themes_dir`], adopts the
/// parsed palette into `appearance.colors`, and re-validates the effective
/// config so the outline AC-1/AC-2 contract runs against the local
/// background. Dual `light:/dark:` selections cannot reference local themes
/// in v1 (the built-in halves keep their zero-I/O resolution).
///
/// # Errors
///
/// Bounded diagnostic when the file is missing, rejected, fails readability,
/// or is combined with an inline `appearance.colors` table.
pub fn apply_selection(
    effective: &mut crate::types::EffectiveConfig,
) -> Result<Option<ActiveLocalTheme>, String> {
    // Fast path: every built-in/default selection returns before reading even
    // an environment variable, so the shipped catalog stays strictly zero-I/O.
    if selected_local_reference(effective).is_none() {
        return Ok(None);
    }
    match local_themes_dir() {
        Some(dir) => apply_selection_from_dir(effective, &dir),
        None => {
            if selected_local_reference(effective).is_some() {
                Err(String::from(
                    "appearance.theme selects a local theme but no config root ($XDG_CONFIG_HOME, %APPDATA%, or $HOME) is available",
                ))
            } else {
                Ok(None)
            }
        }
    }
}

/// [`apply_selection`] against an explicit directory (tests and embedders).
///
/// # Errors
///
/// Same as [`apply_selection`].
pub fn apply_selection_from_dir(
    effective: &mut crate::types::EffectiveConfig,
    dir: &Path,
) -> Result<Option<ActiveLocalTheme>, String> {
    let Some(reference) = selected_local_reference(effective) else {
        return Ok(None);
    };
    say_no_dual(effective)?;
    let catalog = load_local_themes_from_dir(dir);
    let name = reference
        .strip_prefix(LOCAL_THEME_PREFIX)
        .unwrap_or(&reference);
    let Some(theme) = catalog.find(name) else {
        let available = if catalog.available_names().is_empty() {
            String::from("(none)")
        } else {
            catalog.available_names().join(", ")
        };
        // Name the exact file diagnostic when the selected slug matches a
        // rejected `<name>.toml`; otherwise summarize the remaining rejects.
        let specific = catalog
            .diagnostics
            .iter()
            .find(|diagnostic| {
                Path::new(&diagnostic.file)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
            })
            .map(|diagnostic| format!(": {}", diagnostic.message))
            .unwrap_or_default();
        return Err(format!(
            "unknown local theme '{name}' in '{}'{specific}; available local themes: {available}; rejected files: {} (run 'bitty list themes' for diagnostics)",
            dir.display(),
            catalog.diagnostics.len()
        ));
    };
    // Re-applying the same selection is idempotent (a reload may hand this
    // loader an effective config that already carries the previous local
    // palette). Any *different* inline palette is an ambiguous source and
    // fails closed instead of silently choosing one.
    if effective
        .appearance
        .colors
        .is_some_and(|existing| existing != theme.palette)
    {
        return Err(format!(
            "appearance.theme selects '{reference}' but appearance.colors is also set; choose one custom-palette source"
        ));
    }
    let mut updated = effective.clone();
    updated.appearance.colors = Some(theme.palette);
    updated.validate().map_err(|error| {
        format!(
            "local theme '{name}' in '{}' is not a valid effective palette: {error}",
            dir.display()
        )
    })?;
    *effective = updated;
    Ok(Some(ActiveLocalTheme {
        name: theme.name.clone(),
        category: theme.category,
        file_name: theme.file_name.clone(),
    }))
}

fn selected_local_reference(effective: &crate::types::EffectiveConfig) -> Option<String> {
    let raw = effective.appearance.theme.as_deref()?;
    let selection = parse_theme_selection(raw).ok()?;
    match selection {
        crate::theme::ThemeSelection::Single(name)
            if crate::theme::is_local_theme_reference(&name) =>
        {
            Some(name)
        }
        crate::theme::ThemeSelection::Single(_) => None,
        crate::theme::ThemeSelection::Dual { light, dark }
            if crate::theme::is_local_theme_reference(&light)
                || crate::theme::is_local_theme_reference(&dark) =>
        {
            Some(format!("{light},{dark}"))
        }
        crate::theme::ThemeSelection::Dual { .. } => None,
    }
}

fn say_no_dual(effective: &crate::types::EffectiveConfig) -> Result<(), String> {
    if let Some(raw) = effective.appearance.theme.as_deref() {
        if let Ok(crate::theme::ThemeSelection::Dual { .. }) = parse_theme_selection(raw) {
            return Err(String::from(
                "local themes cannot be used in a 'light:<name>,dark:<name>' selection; select one local theme for both appearances",
            ));
        }
    }
    Ok(())
}

/// Reads, parses, gates, and bounds one candidate file.
fn read_one(file_name: &str, path: &Path) -> Result<LocalTheme, LocalThemeError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| LocalThemeError::Malformed {
        line: 0,
        reason: String::from("file metadata is not readable"),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalThemeError::Malformed {
            line: 0,
            reason: String::from("symlinks are not accepted; use a regular .toml file"),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(LocalThemeError::Malformed {
            line: 0,
            reason: String::from("not a regular file"),
        });
    }
    if metadata.len() as usize > MAX_LOCAL_THEME_BYTES {
        return Err(LocalThemeError::Oversize {
            actual: metadata.len() as usize,
        });
    }
    let file = std::fs::File::open(path).map_err(|_| LocalThemeError::Malformed {
        line: 0,
        reason: String::from("file is not readable"),
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_LOCAL_THEME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalThemeError::Malformed {
            line: 0,
            reason: String::from("file read failed"),
        })?;
    if bytes.len() > MAX_LOCAL_THEME_BYTES {
        return Err(LocalThemeError::Oversize {
            actual: bytes.len(),
        });
    }
    let mut theme = parse_local_theme(&bytes)?;
    theme.file_name = file_name.to_string();
    validate_readability(&theme)?;
    Ok(theme)
}

/// Parses one local theme file (pure; no filesystem access).
///
/// The `file_name` field is empty here; [`load_local_themes_from_dir`] fills
/// it after a successful read.
///
/// # Errors
///
/// [`LocalThemeError`] naming the line and/or field; never panics.
pub fn parse_local_theme(bytes: &[u8]) -> Result<LocalTheme, LocalThemeError> {
    if bytes.len() > MAX_LOCAL_THEME_BYTES {
        return Err(LocalThemeError::Oversize {
            actual: bytes.len(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| LocalThemeError::InvalidUtf8)?;
    let mut fields: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        if raw_line.len() > MAX_LOCAL_THEME_LINE_BYTES {
            return Err(LocalThemeError::LineTooLong { line: line_number });
        }
        let line = crate::plugin_file::strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            return Err(LocalThemeError::Malformed {
                line: line_number,
                reason: String::from("sections are not supported (flat keys only)"),
            });
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(LocalThemeError::Malformed {
                line: line_number,
                reason: String::from("expected 'key = value'"),
            });
        };
        let key = raw_key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(LocalThemeError::Malformed {
                line: line_number,
                reason: String::from("keys are bare lowercase [a-z0-9_] words"),
            });
        }
        if !matches!(
            key,
            "name"
                | "category"
                | "aliases"
                | "source"
                | "license"
                | "background"
                | "foreground"
                | "cursor"
                | "selection"
                | "ansi"
                | "border_focused"
                | "border_idle"
        ) {
            return Err(LocalThemeError::UnknownField {
                line: line_number,
                field: key.to_string(),
            });
        }
        if fields.contains_key(key) {
            return Err(LocalThemeError::Malformed {
                line: line_number,
                reason: format!("duplicate field '{key}'"),
            });
        }
        fields.insert(key.to_string(), (line_number, raw_value.trim().to_string()));
    }

    let string_field = |field: &'static str, max: usize| -> Result<String, LocalThemeError> {
        let (line, raw) = fields
            .get(field)
            .ok_or(LocalThemeError::MissingField(field))?;
        crate::plugin_file::parse_quoted_bounded(raw, max).map_err(|reason| {
            LocalThemeError::Malformed {
                line: *line,
                reason: format!("{field}: {reason}"),
            }
        })
    };
    let optional_string = |field: &str, max: usize| -> Result<String, LocalThemeError> {
        match fields.get(field) {
            None => Ok(String::new()),
            Some((line, raw)) => {
                crate::plugin_file::parse_quoted_bounded(raw, max).map_err(|reason| {
                    LocalThemeError::Malformed {
                        line: *line,
                        reason: format!("{field}: {reason}"),
                    }
                })
            }
        }
    };
    let name = string_field("name", MAX_LOCAL_THEME_NAME_BYTES)?;
    if !is_valid_local_theme_name(&name) {
        return Err(LocalThemeError::Malformed {
            line: fields.get("name").map_or(0, |(line, _)| *line),
            reason: String::from(
                "name must be a lowercase slug of [a-z0-9_-], starting with a letter or digit",
            ),
        });
    }
    let category_raw = string_field("category", 16)?;
    let category = match category_raw.as_str() {
        "dark" => ThemeCategory::Dark,
        "light" => ThemeCategory::Light,
        _ => {
            return Err(LocalThemeError::Malformed {
                line: fields.get("category").map_or(0, |(line, _)| *line),
                reason: String::from("category must be \"dark\" or \"light\""),
            });
        }
    };
    let aliases = match fields.get("aliases") {
        None => Vec::new(),
        Some((line, raw)) => {
            let parsed = crate::plugin_file::parse_string_array(raw).map_err(|reason| {
                LocalThemeError::Malformed {
                    line: *line,
                    reason: format!("aliases: {reason}"),
                }
            })?;
            if parsed.len() > MAX_LOCAL_THEME_ALIASES {
                return Err(LocalThemeError::Malformed {
                    line: *line,
                    reason: format!("aliases: at most {MAX_LOCAL_THEME_ALIASES} entries"),
                });
            }
            let mut seen = Vec::new();
            for alias in parsed {
                if !is_valid_local_theme_name(&alias) || alias == name {
                    return Err(LocalThemeError::Malformed {
                        line: *line,
                        reason: format!(
                            "aliases: '{alias}' must be a distinct canonical local slug"
                        ),
                    });
                }
                if seen.contains(&alias) {
                    return Err(LocalThemeError::Malformed {
                        line: *line,
                        reason: format!("aliases: duplicate '{alias}'"),
                    });
                }
                seen.push(alias);
            }
            seen
        }
    };
    let provenance = optional_string("source", MAX_LOCAL_THEME_METADATA_BYTES)?;
    let license = optional_string("license", MAX_LOCAL_THEME_METADATA_BYTES)?;
    let color = |field: &'static str| -> Result<[u8; 3], LocalThemeError> {
        let raw = string_field(field, 7)?;
        CustomPalette::parse_hex_rgb(&raw).ok_or_else(|| LocalThemeError::Malformed {
            line: fields.get(field).map_or(0, |(line, _)| *line),
            reason: format!("{field}: must be canonical \"#RRGGBB\" hex"),
        })
    };
    let background = color("background")?;
    let foreground = color("foreground")?;
    let cursor = color("cursor")?;
    let selection = color("selection")?;
    let ansi = match fields.get("ansi") {
        None => return Err(LocalThemeError::MissingField("ansi")),
        Some((line, raw)) => {
            let parsed = crate::plugin_file::parse_string_array(raw).map_err(|reason| {
                LocalThemeError::Malformed {
                    line: *line,
                    reason: format!("ansi: {reason}"),
                }
            })?;
            if parsed.len() != crate::theme::CUSTOM_PALETTE_ANSI_COUNT {
                return Err(LocalThemeError::Malformed {
                    line: *line,
                    reason: format!(
                        "ansi: must contain exactly {} entries (found {})",
                        crate::theme::CUSTOM_PALETTE_ANSI_COUNT,
                        parsed.len()
                    ),
                });
            }
            let mut colors = [[0u8; 3]; crate::theme::CUSTOM_PALETTE_ANSI_COUNT];
            for (index, (slot, raw)) in colors.iter_mut().zip(parsed.iter()).enumerate() {
                *slot = CustomPalette::parse_hex_rgb(raw).ok_or_else(|| {
                    LocalThemeError::Malformed {
                        line: *line,
                        reason: format!("ansi[{index}]: must be canonical \"#RRGGBB\" hex"),
                    }
                })?;
            }
            colors
        }
    };
    let outline =
        |field: &'static str| -> Result<Option<crate::types::OutlineColor>, LocalThemeError> {
            match fields.get(field) {
                None => Ok(None),
                Some((line, raw)) => {
                    let parsed =
                        crate::plugin_file::parse_quoted_bounded(raw, 9).map_err(|reason| {
                            LocalThemeError::Malformed {
                                line: *line,
                                reason: format!("{field}: {reason}"),
                            }
                        })?;
                    crate::types::OutlineColor::parse(&parsed)
                        .map(Some)
                        .ok_or_else(|| LocalThemeError::Malformed {
                            line: *line,
                            reason: format!("{field}: must be \"#RRGGBB\" or \"#RRGGBBAA\""),
                        })
                }
            }
        };
    let palette = CustomPalette {
        background,
        foreground,
        cursor,
        selection,
        ansi,
        border_focused: outline("border_focused")?,
        border_idle: outline("border_idle")?,
        category: Some(category),
    };
    Ok(LocalTheme {
        name,
        category,
        aliases,
        provenance,
        license,
        palette,
        file_name: String::new(),
    })
}

/// Runs the built-in readability policy plus the focused-outline floor.
fn validate_readability(theme: &LocalTheme) -> Result<(), LocalThemeError> {
    let base = &BITTY_DARK;
    let outline_focused = theme.palette.border_focused.unwrap_or(base.border_focused);
    let outline_idle = theme.palette.border_idle.unwrap_or(base.border_idle);
    let probe = Theme {
        name: crate::theme::LOCAL_THEME_BASE_NAME,
        category: theme.category,
        aliases: &[],
        source: "local",
        license: "",
        background: theme.palette.background,
        foreground: theme.palette.foreground,
        cursor: theme.palette.cursor,
        selection: theme.palette.selection,
        border_focused: outline_focused,
        border_idle: outline_idle,
        ansi: theme.palette.ansi,
    };
    if let Some(row) = crate::theme::contrast_violations(&probe).first() {
        return Err(LocalThemeError::Unreadable {
            role: row.role.label(),
            ratio: row.ratio,
            floor: row.floor,
        });
    }
    let ac1 = outline_focused.contrast_over(theme.palette.background);
    if ac1 < MIN_UI_CONTRAST {
        return Err(LocalThemeError::Unreadable {
            role: "border.focused",
            ratio: ac1,
            floor: MIN_UI_CONTRAST,
        });
    }
    if outline_focused != outline_idle {
        let ac2 = outline_focused.contrast_with(outline_idle, theme.palette.background);
        if ac2 < MIN_UI_CONTRAST {
            return Err(LocalThemeError::Unreadable {
                role: "border.focused-on-idle",
                ratio: ac2,
                floor: MIN_UI_CONTRAST,
            });
        }
    }
    // The text floor is named explicitly for diagnostics even though
    // `contrast_violations` already includes it; this keeps the owner-facing
    // guarantee (`normal text >= 4.5:1`) visible in code.
    if crate::types::contrast_ratio(theme.palette.foreground, theme.palette.background)
        < MIN_TEXT_CONTRAST
    {
        return Err(LocalThemeError::Unreadable {
            role: "foreground",
            ratio: crate::types::contrast_ratio(theme.palette.foreground, theme.palette.background),
            floor: MIN_TEXT_CONTRAST,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-local-theme-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn slack_ansi() -> String {
        // Bitty Dark's shipped ANSI entries: the passing fixture must clear
        // the same readability policy as a real theme.
        [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
        ]
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ")
    }

    fn valid_theme(name: &str) -> String {
        format!(
            "name = \"{name}\"\ncategory = \"dark\"\n\
             source = \"https://example.invalid/themes/{name}\"\nlicense = \"MIT\"\n\
             background = \"#1e1e2e\"\nforeground = \"#cdd6f4\"\n\
             cursor = \"#f5e0dc\"\nselection = \"#313244\"\nansi = [{}]\n",
            slack_ansi()
        )
    }

    #[test]
    fn parses_a_passing_local_theme() {
        let theme = parse_local_theme(valid_theme("sunset").as_bytes()).expect("valid");
        assert_eq!(theme.name, "sunset");
        assert_eq!(theme.category, ThemeCategory::Dark);
        assert_eq!(theme.palette.background, [0x1e, 0x1e, 0x2e]);
        assert_eq!(theme.palette.ansi.len(), 16);
        validate_readability(&theme).expect("readable");
    }

    #[test]
    fn parses_optional_outline_tokens() {
        let raw = format!(
            "{}border_focused = \"#33CCFF\"\nborder_idle = \"#595959AA\"\n",
            valid_theme("bordered")
        );
        let theme = parse_local_theme(raw.as_bytes()).expect("valid");
        assert!(theme.palette.border_focused.is_some());
        assert!(theme.palette.border_idle.is_some());
        validate_readability(&theme).expect("readable");
    }

    #[test]
    fn rejects_a_failing_readability_theme() {
        let raw = valid_theme("dim")
            .replace("foreground = \"#cdd6f4\"", "foreground = \"#202030\"")
            .replace("cursor = \"#f5e0dc\"", "cursor = \"#202030\"");
        let theme = parse_local_theme(raw.as_bytes()).expect("structurally valid");
        let error = validate_readability(&theme).expect_err("unreadable");
        assert!(
            matches!(error, LocalThemeError::Unreadable { .. }),
            "{error}"
        );
    }

    #[test]
    fn rejects_malformed_hex_with_the_field_path() {
        let raw = valid_theme("bad").replace("background = \"#1e1e2e\"", "background = \"1e1e2e\"");
        let error = parse_local_theme(raw.as_bytes()).expect_err("bad hex");
        let text = error.to_string();
        assert!(text.contains("background"), "{text}");
        assert!(text.contains("#RRGGBB"), "{text}");
    }

    #[test]
    fn rejects_short_ansi_and_unknown_fields() {
        // Leave only 15 entries by trimming the last array item.
        let ansi = slack_ansi();
        let last_comma = ansi.rfind(',').expect("commas");
        let short = valid_theme("short").replace(&ansi, &ansi[..last_comma]);
        let error = parse_local_theme(short.as_bytes()).expect_err("short ansi");
        assert!(error.to_string().contains("exactly 16"), "{error}");

        let unknown = format!(
            "{}\nbackground_url = \"https://x\"\n",
            valid_theme("unknown")
        );
        let error = parse_local_theme(unknown.as_bytes()).expect_err("unknown field");
        assert!(
            matches!(error, LocalThemeError::UnknownField { .. }),
            "{error}"
        );
    }

    #[test]
    fn rejects_oversized_file() {
        let mut raw = valid_theme("huge").into_bytes();
        raw.resize(MAX_LOCAL_THEME_BYTES + 1, b' ');
        let error = parse_local_theme(&raw).expect_err("oversize");
        assert!(matches!(error, LocalThemeError::Oversize { .. }), "{error}");
    }

    #[test]
    fn rejects_too_many_files_and_symlinks() {
        let dir = temp_dir("many");
        for index in 0..=MAX_LOCAL_THEME_FILES {
            std::fs::write(
                dir.join(format!("t{index}.toml")),
                valid_theme(&format!("t{index}")),
            )
            .expect("write");
        }
        let catalog = load_local_themes_from_dir(&dir);
        assert!(catalog.themes.is_empty());
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|d| d.message.contains("more than")),
            "{catalog:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);

        #[cfg(unix)]
        {
            let dir = temp_dir("symlink");
            std::fs::write(dir.join("real.toml"), valid_theme("real")).expect("write");
            std::os::unix::fs::symlink(dir.join("real.toml"), dir.join("link.toml"))
                .expect("symlink");
            let catalog = load_local_themes_from_dir(&dir);
            assert_eq!(catalog.themes.len(), 1);
            assert!(
                catalog
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("symlink")),
                "{catalog:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);

            // A symlinked themes directory is rejected wholesale.
            let real = temp_dir("real-themes");
            std::fs::write(real.join("sunset.toml"), valid_theme("sunset")).expect("write");
            let link_parent = temp_dir("link-parent");
            let link = link_parent.join("themes");
            std::os::unix::fs::symlink(&real, &link).expect("dir symlink");
            let catalog = load_local_themes_from_dir(&link);
            assert!(catalog.themes.is_empty());
            assert!(
                catalog
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("directory is a symlink")),
                "{catalog:?}"
            );
            let _ = std::fs::remove_dir_all(&real);
            let _ = std::fs::remove_dir_all(&link_parent);
        }
    }

    #[test]
    fn selection_applies_palette_and_rejects_duals_or_missing_files() {
        let dir = temp_dir("apply");
        std::fs::write(dir.join("sunset.toml"), valid_theme("sunset")).expect("write");
        let mut effective = crate::types::EffectiveConfig::default();
        effective.appearance.theme = Some(String::from("local:sunset"));
        let active = apply_selection_from_dir(&mut effective, &dir)
            .expect("apply")
            .expect("local");
        assert_eq!(active.name, "sunset");
        assert_eq!(
            effective.appearance.colors.expect("palette").background,
            [0x1e, 0x1e, 0x2e]
        );

        let mut missing = crate::types::EffectiveConfig::default();
        missing.appearance.theme = Some(String::from("local:missing"));
        let error = apply_selection_from_dir(&mut missing, &dir).expect_err("missing");
        assert!(error.contains("unknown local theme"), "{error}");

        let mut dual = crate::types::EffectiveConfig::default();
        dual.appearance.theme = Some(String::from("light:local:sunset,dark:dracula"));
        let error = apply_selection_from_dir(&mut dual, &dir).expect_err("dual");
        assert!(error.contains("cannot be used"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_local_selection_defers_outline_contract_until_palette_is_applied() {
        let dir = temp_dir("pending-outline");
        let light = "name = \"paper\"\ncategory = \"light\"\n\
                     background = \"#FFFFFF\"\nforeground = \"#1F2328\"\n\
                     cursor = \"#0969DA\"\nselection = \"#B6D7FF\"\n\
                     ansi = [\"#24292F\", \"#CF222E\", \"#116329\", \"#4D2D00\", \
                     \"#0969DA\", \"#8250DF\", \"#1B7C83\", \"#6E7781\", \
                     \"#57606A\", \"#A40E26\", \"#1A7F37\", \"#633C01\", \
                     \"#1C76D9\", \"#8861CE\", \"#2B8096\", \"#8C959F\"]\n\
                     border_focused = \"#0969DA\"\nborder_idle = \"#1F2328\"\n";
        std::fs::write(dir.join("paper.toml"), light).expect("write");

        let mut effective = crate::types::EffectiveConfig::default();
        effective.appearance.theme = Some(String::from("local:paper"));
        // These borders clear 3:1 on the white local ground but not on dark.
        effective.decoration.border_color_focused =
            Some(crate::types::OutlineColor::parse("#0969DA").expect("hex"));
        effective.decoration.border_color_idle =
            Some(crate::types::OutlineColor::parse("#1F2328").expect("hex"));
        effective
            .validate()
            .expect("pre-apply validation defers the outline contract");

        apply_selection_from_dir(&mut effective, &dir).expect("apply");
        effective.validate().expect("post-apply outline contract");
        assert_eq!(effective.effective_theme().category, ThemeCategory::Light);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applied_local_theme_carries_its_category_into_the_effective_theme() {
        let dir = temp_dir("category");
        let raw = valid_theme("paper").replace("category = \"dark\"", "category = \"light\"");
        std::fs::write(dir.join("paper.toml"), raw).expect("write");
        let mut effective = crate::types::EffectiveConfig::default();
        effective.appearance.theme = Some(String::from("local:paper"));
        apply_selection_from_dir(&mut effective, &dir).expect("apply");
        let theme = effective.effective_theme();
        assert_eq!(theme.category, ThemeCategory::Light);
        assert_eq!(theme.name, crate::theme::LOCAL_THEME_BASE_NAME);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_selection_never_reads_local_files() {
        let dir = temp_dir("builtin");
        std::fs::write(dir.join("sunset.toml"), valid_theme("sunset")).expect("write");
        let mut effective = crate::types::EffectiveConfig::default();
        effective.appearance.theme = Some(String::from("bitty-dark"));
        assert_eq!(
            apply_selection_from_dir(&mut effective, &dir).expect("apply"),
            None
        );
        assert!(effective.appearance.colors.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_edit_keeps_the_previous_palette_value_untouched() {
        let dir = temp_dir("invalid");
        std::fs::write(dir.join("sunset.toml"), valid_theme("sunset")).expect("write");
        let mut effective = crate::types::EffectiveConfig::default();
        effective.appearance.theme = Some(String::from("local:sunset"));
        apply_selection_from_dir(&mut effective, &dir).expect("first apply");
        let good = effective.appearance.colors.expect("palette");
        std::fs::write(dir.join("sunset.toml"), "name = \"sunset\"\n").expect("break");
        let error = apply_selection_from_dir(&mut effective, &dir).expect_err("invalid edit");
        assert!(error.contains("missing required field"), "{error}");
        assert_eq!(effective.appearance.colors, Some(good), "last good kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
