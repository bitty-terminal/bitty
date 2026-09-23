//! `project.toml` schema kernel (OQ-068, SEC-27 `bitty#1096`).
//!
//! `OQ-068` is Accepted (owner ruling 2026-09-23, register `bb96efe`):
//! the project definition lives in a declarative-data-only Git-tracked
//! `.wheel/` tree (`project.toml`, `agents/`, `workflows/`, `prompts/`,
//! `policies/`, `tools/`, `skills/`), discovered as `.wheel/` first with
//! `.agents/` as the compatibility fallback (implemented in
//! [`crate::wheel`], live). This module drafts the **candidate**
//! `project.toml` schema as review evidence: strict, fail-closed, pure
//! (`&str` in, [`ProjectDefinition`] out), `std` only.
//!
//! Nothing here reads the filesystem, executes anything, or grants trust:
//! a parsed definition carries project identity plus names and relative
//! paths only, and honoring one still routes through hash-bound consent
//! ([`crate::trust`]). Locate with [`crate::wheel::discover`], read and
//! consent via [`crate::trust`], then parse here.
//!
//! # Non-goals
//!
//! - No I/O and no trust assignment: this kernel never touches the
//!   filesystem and its output authorizes nothing by itself.
//! - No general TOML parser: only the strict subset below is recognized
//!   (top-level `schema_version`, `[project]`, the six section tables,
//!   bare keys, basic/literal strings, one decimal integer). Anything
//!   outside the subset — floats, booleans, dates, arrays, inline
//!   tables, dotted keys, `[[array tables]]`, multi-line strings — is
//!   denied fail-closed, never skipped.
//! - No runtime state: the schema has no state fields by construction, so
//!   the tracked-vs-state split holds for anything this kernel accepts.
//!   Git-trackedness itself is a repository property, not a parseable one,
//!   and stays with the caller.
//! - Candidate schema, review evidence: carries no stability promise and
//!   must be accepted or replaced on review before any caller honors it.
//!
//! There is no `unsafe`, no network, and no new dependency (`std` only).

use std::collections::{BTreeMap, BTreeSet};

use crate::error::ConfigError;

// ── version and bounds ────────────────────────────────────────────────────

/// `project.toml` schema version understood by this kernel.
pub const PROJECT_SCHEMA_VERSION: u32 = 1;

/// Maximum `project.toml` document size in bytes (64 KiB).
pub const MAX_PROJECT_TOML_BYTES: usize = 64 * 1024;

/// Maximum lines in one `project.toml` document.
pub const MAX_PROJECT_TOML_LINES: usize = 4096;

/// Maximum keys in one table (including the top level).
pub const MAX_TABLE_KEYS: usize = 64;

/// Maximum entries in one content section.
pub const MAX_SECTION_ENTRIES: usize = 64;

/// Maximum bytes of `[project] name` / section entry names.
pub const MAX_PROJECT_NAME_BYTES: usize = 128;

/// Maximum bytes of a section entry name.
pub const MAX_ENTRY_NAME_BYTES: usize = 64;

/// Maximum bytes of `[project] version`.
pub const MAX_PROJECT_VERSION_BYTES: usize = 32;

/// Maximum characters of `[project] description`.
pub const MAX_PROJECT_DESCRIPTION_CHARS: usize = 1024;

/// Maximum bytes of a section entry path.
pub const MAX_ENTRY_PATH_BYTES: usize = 256;

/// Maximum bytes of one `/`-separated path segment.
pub const MAX_PATH_SEGMENT_BYTES: usize = 64;

/// Maximum characters echoed back inside a diagnostic message; longer
/// content is truncated so errors never carry whole documents.
const MAX_ECHO_CHARS: usize = 64;

// ── sections ──────────────────────────────────────────────────────────────

/// The six content sections of the adopted `.wheel/` layout (OQ-068).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectSection {
    /// `agents/` directory.
    Agents,
    /// `workflows/` directory.
    Workflows,
    /// `prompts/` directory.
    Prompts,
    /// `policies/` directory.
    Policies,
    /// `tools/` directory.
    Tools,
    /// `skills/` directory.
    Skills,
}

impl ProjectSection {
    /// Directory name this section governs (also its TOML table name).
    #[must_use]
    pub const fn dir_name(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Workflows => "workflows",
            Self::Prompts => "prompts",
            Self::Policies => "policies",
            Self::Tools => "tools",
            Self::Skills => "skills",
        }
    }

    /// Parse a table name; unknown tables fail closed.
    pub fn parse(name: &str) -> Result<Self, ConfigError> {
        match name {
            "agents" => Ok(Self::Agents),
            "workflows" => Ok(Self::Workflows),
            "prompts" => Ok(Self::Prompts),
            "policies" => Ok(Self::Policies),
            "tools" => Ok(Self::Tools),
            "skills" => Ok(Self::Skills),
            _ => Err(ConfigError::UndeclaredField {
                field: format!("table '{}'", snip(name)),
                source: Some("project.toml".to_string()),
            }),
        }
    }

    /// All sections in layout order.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::Agents,
            Self::Workflows,
            Self::Prompts,
            Self::Policies,
            Self::Tools,
            Self::Skills,
        ]
    }
}

impl std::fmt::Display for ProjectSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.dir_name())
    }
}

// ── typed definition ──────────────────────────────────────────────────────

/// `[project]` identity: names the project, nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMeta {
    /// Project name, `[a-z0-9][a-z0-9_-]*`, at most 128 bytes.
    pub name: String,
    /// Optional version, digit-led `[0-9A-Za-z.+_-]*`, at most 32 bytes.
    pub version: Option<String>,
    /// Optional description, at most 1024 chars, no control characters
    /// other than newline and tab.
    pub description: Option<String>,
}

/// One named entry inside a content section: a name plus a relative path
/// confined to that section's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    /// Entry name, same shape as the project name, at most 64 bytes.
    pub name: String,
    /// Path relative to the `.wheel/` root, starting with
    /// `<section>/`, no absolute paths, no `.`/`..` segments.
    pub path: String,
}

/// Parsed `project.toml`: identity plus per-section entries.
///
/// Names and relative paths only — carries no authority and authorizes
/// nothing; honoring a definition requires hash-bound consent first
/// ([`crate::trust`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDefinition {
    /// Schema version (always [`PROJECT_SCHEMA_VERSION`] today).
    pub schema_version: u32,
    /// `[project]` identity.
    pub project: ProjectMeta,
    /// Per-section entries, keyed by section then entry name.
    pub sections: BTreeMap<ProjectSection, BTreeMap<String, String>>,
}

impl ProjectDefinition {
    /// Entries of one section, or `None` when the section is absent.
    #[must_use]
    pub fn entries(&self, section: ProjectSection) -> Option<&BTreeMap<String, String>> {
        self.sections.get(&section)
    }
}

// ── parsing ───────────────────────────────────────────────────────────────

/// Parse and validate a `project.toml` document.
///
/// Fail-closed on every malformed, oversized, unknown, or
/// non-declarative input; see the module docs for the recognized subset.
pub fn parse_project_toml(text: &str) -> Result<ProjectDefinition, ConfigError> {
    if text.len() > MAX_PROJECT_TOML_BYTES {
        return Err(ConfigError::InvalidInput {
            message: format!(
                "project.toml exceeds {} bytes (got {})",
                MAX_PROJECT_TOML_BYTES,
                text.len()
            ),
        });
    }
    let mut parser = Parser::default();
    for (index, raw_line) in text.lines().enumerate() {
        let lineno = index + 1;
        if lineno > MAX_PROJECT_TOML_LINES {
            return Err(ConfigError::InvalidInput {
                message: format!("project.toml exceeds {} lines", MAX_PROJECT_TOML_LINES),
            });
        }
        parser.parse_line(raw_line, lineno)?;
    }
    parser.finish()
}

/// Truncate content echoed in diagnostics so errors stay bounded.
fn snip(raw: &str) -> String {
    let clipped: String = raw.chars().take(MAX_ECHO_CHARS).collect();
    if raw.chars().count() > MAX_ECHO_CHARS {
        format!("{clipped}...")
    } else {
        clipped
    }
}

/// Bare TOML key: `[A-Za-z0-9_-]+` (dotted/quoted keys are outside the
/// subset and denied by [`is_bare_key`]).
fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_ws(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// Intermediate parse state: one key set per table plus the top level.
#[derive(Default)]
struct Parser {
    top_keys: BTreeSet<String>,
    schema_version: Option<(u32, usize)>,
    project_keys: BTreeSet<String>,
    project: BTreeMap<String, String>,
    section_keys: BTreeMap<ProjectSection, BTreeSet<String>>,
    sections: BTreeMap<ProjectSection, BTreeMap<String, String>>,
    current: Option<Table>,
}

/// Which table key/value pairs belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Top,
    Project,
    Section(ProjectSection),
}

impl Parser {
    fn parse_line(&mut self, raw_line: &str, lineno: usize) -> Result<(), ConfigError> {
        if raw_line.bytes().any(|b| b < 0x20 && b != b'\t') {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                "raw control characters are not allowed",
            ));
        }
        let line = raw_line.as_bytes();
        let mut pos = 0;
        while pos < line.len() && is_ws(line[pos]) {
            pos += 1;
        }
        if pos >= line.len() || line[pos] == b'#' {
            return Ok(());
        }
        if line[pos] == b'[' {
            self.parse_table_header(line, pos, lineno)?;
        } else {
            self.parse_key_value(line, pos, lineno)?;
        }
        Ok(())
    }

    fn parse_table_header(
        &mut self,
        line: &[u8],
        mut pos: usize,
        lineno: usize,
    ) -> Result<(), ConfigError> {
        // `[[` array tables are outside the subset.
        pos += 1;
        if pos < line.len() && line[pos] == b'[' {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                "array tables ([[...]]) are not part of the project schema",
            ));
        }
        while pos < line.len() && is_ws(line[pos]) {
            pos += 1;
        }
        let start = pos;
        while pos < line.len() && !is_ws(line[pos]) && line[pos] != b']' && line[pos] != b'#' {
            pos += 1;
        }
        let name = std::str::from_utf8(&line[start..pos]).map_err(|_| {
            ConfigError::validation(format!("line {lineno}"), "table name is not UTF-8")
        })?;
        if !is_bare_key(name) || name.contains('.') {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                format!("invalid table header '{}'", snip(name)),
            ));
        }
        while pos < line.len() && is_ws(line[pos]) {
            pos += 1;
        }
        if pos >= line.len() || line[pos] != b']' {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                "table header must close with ']'",
            ));
        }
        pos += 1;
        expect_line_end(line, pos, lineno)?;
        let table = if name == "project" {
            if !self.project_keys.is_empty() || self.project_seen() {
                return Err(ConfigError::validation(
                    format!("line {lineno}"),
                    "duplicate [project] table",
                ));
            }
            Table::Project
        } else {
            let section =
                ProjectSection::parse(name).map_err(|_| ConfigError::UndeclaredField {
                    field: format!("table '{}'", snip(name)),
                    source: Some("project.toml".to_string()),
                })?;
            if self.section_keys.contains_key(&section) {
                return Err(ConfigError::validation(
                    format!("line {lineno}"),
                    format!("duplicate [{name}] table"),
                ));
            }
            self.section_keys.insert(section, BTreeSet::new());
            Table::Section(section)
        };
        self.current = Some(table);
        Ok(())
    }

    fn project_seen(&self) -> bool {
        self.current == Some(Table::Project) || !self.project_keys.is_empty()
    }

    fn parse_key_value(
        &mut self,
        line: &[u8],
        mut pos: usize,
        lineno: usize,
    ) -> Result<(), ConfigError> {
        let start = pos;
        while pos < line.len() && !is_ws(line[pos]) && line[pos] != b'=' && line[pos] != b'#' {
            pos += 1;
        }
        let key = std::str::from_utf8(&line[start..pos])
            .map_err(|_| ConfigError::validation(format!("line {lineno}"), "key is not UTF-8"))?;
        if !is_bare_key(key) {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                format!("invalid key '{}'", snip(key)),
            ));
        }
        while pos < line.len() && is_ws(line[pos]) {
            pos += 1;
        }
        if pos >= line.len() || line[pos] != b'=' {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                format!("key '{}' is missing '='", snip(key)),
            ));
        }
        pos += 1;
        while pos < line.len() && is_ws(line[pos]) {
            pos += 1;
        }
        if pos >= line.len() {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                format!("key '{}' is missing a value", snip(key)),
            ));
        }
        let (value, next) = parse_value(line, pos, lineno)?;
        expect_line_end(line, next, lineno)?;
        self.store_key(key, value, lineno)?;
        Ok(())
    }

    fn store_key(&mut self, key: &str, value: Value, lineno: usize) -> Result<(), ConfigError> {
        match self.current.unwrap_or(Table::Top) {
            Table::Top => {
                if key != "schema_version" {
                    return Err(ConfigError::UndeclaredField {
                        field: format!("top-level key '{key}'"),
                        source: Some("project.toml".to_string()),
                    });
                }
                if self.schema_version.is_some() {
                    return Err(ConfigError::validation(
                        format!("line {lineno}"),
                        "duplicate schema_version",
                    ));
                }
                let Value::Integer(version) = value else {
                    return Err(ConfigError::validation(
                        format!("line {lineno}"),
                        "schema_version must be a decimal integer",
                    ));
                };
                self.top_keys.insert(key.to_string());
                self.schema_version = Some((version, lineno));
            }
            Table::Project => {
                if !matches!(key, "name" | "version" | "description") {
                    return Err(ConfigError::UndeclaredField {
                        field: format!("project key '{key}'"),
                        source: Some("project.toml".to_string()),
                    });
                }
                insert_bounded(
                    &mut self.project_keys,
                    &mut self.project,
                    "project",
                    key,
                    value.into_string(lineno, "project", key)?,
                    lineno,
                )?;
            }
            Table::Section(section) => {
                let keys = self
                    .section_keys
                    .get_mut(&section)
                    .expect("section key set exists once its table opens");
                let entries = self.sections.entry(section).or_default();
                insert_bounded(
                    keys,
                    entries,
                    section.dir_name(),
                    key,
                    value.into_string(lineno, section.dir_name(), key)?,
                    lineno,
                )?;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ProjectDefinition, ConfigError> {
        let Some((version, _)) = self.schema_version else {
            return Err(ConfigError::validation(
                "schema_version",
                "missing required top-level schema_version",
            ));
        };
        if version != PROJECT_SCHEMA_VERSION {
            return Err(ConfigError::SchemaVersionUnsupported {
                found: version,
                supported: PROJECT_SCHEMA_VERSION,
            });
        }
        let name = self.project.get("name").ok_or_else(|| {
            ConfigError::validation("project.name", "missing required [project] name")
        })?;
        validate_project_name(name)?;
        let project = ProjectMeta {
            name: name.clone(),
            version: self
                .project
                .get("version")
                .map(|version| validate_project_version(version).map(|()| version.clone()))
                .transpose()?,
            description: self
                .project
                .get("description")
                .map(|description| validate_description(description).map(|()| description.clone()))
                .transpose()?,
        };
        let mut sections: BTreeMap<ProjectSection, BTreeMap<String, String>> = BTreeMap::new();
        for section in ProjectSection::all() {
            let Some(entries) = self.sections.get(&section) else {
                continue;
            };
            let mut validated: BTreeMap<String, String> = BTreeMap::new();
            for (name, path) in entries {
                validate_entry_name(name)?;
                validate_entry_path(section, path)?;
                validated.insert(name.clone(), path.clone());
            }
            sections.insert(section, validated);
        }
        Ok(ProjectDefinition {
            schema_version: version,
            project,
            sections,
        })
    }
}

/// Insert into a table's key set, enforcing the per-table key budget and
/// duplicate denial.
fn insert_bounded(
    keys: &mut BTreeSet<String>,
    values: &mut BTreeMap<String, String>,
    table: &str,
    key: &str,
    value: String,
    lineno: usize,
) -> Result<(), ConfigError> {
    if keys.contains(key) {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            format!("duplicate key '{key}' in [{table}]"),
        ));
    }
    if keys.len() >= MAX_TABLE_KEYS {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            format!("[{table}] exceeds {MAX_TABLE_KEYS} keys"),
        ));
    }
    keys.insert(key.to_string());
    values.insert(key.to_string(), value);
    if values.len() > MAX_SECTION_ENTRIES {
        // Sections share the table budget above; this second bound keeps
        // entry maps small even if the table budget ever grows.
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            format!("[{table}] exceeds {MAX_SECTION_ENTRIES} entries"),
        ));
    }
    Ok(())
}

/// Expect only whitespace and an optional `#` comment until end of line.
fn expect_line_end(line: &[u8], mut pos: usize, lineno: usize) -> Result<(), ConfigError> {
    while pos < line.len() && is_ws(line[pos]) {
        pos += 1;
    }
    if pos < line.len() && line[pos] != b'#' {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            "unexpected content after value",
        ));
    }
    Ok(())
}

// ── values ────────────────────────────────────────────────────────────────

/// A recognized scalar value: strings, or a decimal integer (only legal
/// for `schema_version`).
enum Value {
    Text(String),
    Integer(u32),
}

impl Value {
    /// Extract text; integers are only legal for `schema_version`, so any
    /// other integer surfaces as a validation error naming the key.
    fn into_string(self, lineno: usize, table: &str, key: &str) -> Result<String, ConfigError> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Integer(_) => Err(ConfigError::validation(
                format!("line {lineno}"),
                format!("[{table}] key '{key}' must be a string"),
            )),
        }
    }
}

/// Parse one value at `pos`; returns the value plus the offset just past it.
fn parse_value(line: &[u8], pos: usize, lineno: usize) -> Result<(Value, usize), ConfigError> {
    match line[pos] {
        b'"' => {
            let (text, next) = parse_basic_string(line, pos, lineno)?;
            Ok((Value::Text(text), next))
        }
        b'\'' => {
            let (text, next) = parse_literal_string(line, pos, lineno)?;
            Ok((Value::Text(text), next))
        }
        b'0'..=b'9' => {
            let (version, next) = parse_integer(line, pos, lineno)?;
            Ok((Value::Integer(version), next))
        }
        _ => Err(ConfigError::validation(
            format!("line {lineno}"),
            "values must be double-quoted strings, literal strings, or decimal integers",
        )),
    }
}

/// Parse a `"..."` basic string with the standard escape set.
fn parse_basic_string(
    line: &[u8],
    mut pos: usize,
    lineno: usize,
) -> Result<(String, usize), ConfigError> {
    // pos points at the opening quote.
    pos += 1;
    let mut out = String::new();
    loop {
        if pos >= line.len() {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                "unterminated string (multi-line strings are not supported)",
            ));
        }
        match line[pos] {
            b'"' => return Ok((out, pos + 1)),
            b'\\' => {
                pos += 1;
                if pos >= line.len() {
                    return Err(ConfigError::validation(
                        format!("line {lineno}"),
                        "dangling escape at end of line",
                    ));
                }
                match line[pos] {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'u' => {
                        let (chr, next) = parse_hex_escape(line, pos + 1, 4, lineno)?;
                        out.push(chr);
                        pos = next - 1;
                    }
                    b'U' => {
                        let (chr, next) = parse_hex_escape(line, pos + 1, 8, lineno)?;
                        out.push(chr);
                        pos = next - 1;
                    }
                    _ => {
                        return Err(ConfigError::validation(
                            format!("line {lineno}"),
                            "unknown string escape (supported: \\n \\t \\r \\\" \\\\ \\b \\f \\u \\U)",
                        ));
                    }
                }
                pos += 1;
            }
            0x00..=0x1F => {
                return Err(ConfigError::validation(
                    format!("line {lineno}"),
                    "raw control characters are not allowed inside strings",
                ));
            }
            _ => {
                let rest = std::str::from_utf8(&line[pos..]).map_err(|_| {
                    ConfigError::validation(format!("line {lineno}"), "string content is not UTF-8")
                })?;
                let chr = rest.chars().next().expect("non-empty UTF-8 tail");
                out.push(chr);
                pos += chr.len_utf8();
            }
        }
    }
}

/// Parse a `'...'` literal string (no escapes).
fn parse_literal_string(
    line: &[u8],
    mut pos: usize,
    lineno: usize,
) -> Result<(String, usize), ConfigError> {
    pos += 1;
    let start = pos;
    while pos < line.len() && line[pos] != b'\'' {
        if line[pos] < 0x20 {
            return Err(ConfigError::validation(
                format!("line {lineno}"),
                "raw control characters are not allowed inside strings",
            ));
        }
        pos += 1;
    }
    if pos >= line.len() {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            "unterminated string (multi-line strings are not supported)",
        ));
    }
    let text = std::str::from_utf8(&line[start..pos])
        .map_err(|_| ConfigError::validation(format!("line {lineno}"), "string is not UTF-8"))?
        .to_string();
    Ok((text, pos + 1))
}

/// Parse `\uXXXX` / `\UXXXXXXXX` at `pos`; returns the char plus the
/// offset just past the digits.
fn parse_hex_escape(
    line: &[u8],
    pos: usize,
    digits: usize,
    lineno: usize,
) -> Result<(char, usize), ConfigError> {
    if pos + digits > line.len() {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            "truncated unicode escape",
        ));
    }
    let hex = std::str::from_utf8(&line[pos..pos + digits]).map_err(|_| {
        ConfigError::validation(format!("line {lineno}"), "unicode escape is not UTF-8")
    })?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            "unicode escape must be hexadecimal",
        ));
    }
    let scalar = u32::from_str_radix(hex, 16).map_err(|_| {
        ConfigError::validation(format!("line {lineno}"), "unicode escape out of range")
    })?;
    let chr = char::from_u32(scalar)
        .filter(|c| !c.is_control())
        .ok_or_else(|| {
            ConfigError::validation(
                format!("line {lineno}"),
                "unicode escape must name a non-control scalar value",
            )
        })?;
    Ok((chr, pos + digits))
}

/// Parse a decimal integer run (`[0-9_]+`); must end at whitespace,
/// `#`, or end of line so floats, exponents, and dates are denied.
fn parse_integer(line: &[u8], mut pos: usize, lineno: usize) -> Result<(u32, usize), ConfigError> {
    let start = pos;
    while pos < line.len() && (line[pos].is_ascii_digit() || line[pos] == b'_') {
        pos += 1;
    }
    if pos < line.len() && !is_ws(line[pos]) && line[pos] != b'#' {
        return Err(ConfigError::validation(
            format!("line {lineno}"),
            "integers must be plain decimals (no floats, exponents, or dates)",
        ));
    }
    let digits: String = line[start..pos]
        .iter()
        .filter(|b| **b != b'_')
        .map(|b| *b as char)
        .collect();
    digits
        .parse::<u32>()
        .map_err(|_| {
            ConfigError::validation(format!("line {lineno}"), "integer is out of u32 range")
        })
        .map(|version| (version, pos))
}

// ── field validation ──────────────────────────────────────────────────────

/// Project/entry name shape: `[a-z0-9][a-z0-9_-]*`, length-bounded.
fn validate_name_shape(field: &str, name: &str, limit: usize) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::validation(field, "name must not be empty"));
    }
    if name.len() > limit {
        return Err(ConfigError::validation(
            field,
            format!("name exceeds {limit} bytes (got {})", name.len()),
        ));
    }
    let mut bytes = name.bytes();
    let first = bytes.next().expect("name is non-empty (checked above)");
    let ok = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !ok {
        return Err(ConfigError::validation(
            field,
            format!("name '{}' must match [a-z0-9][a-z0-9_-]*", snip(name)),
        ));
    }
    Ok(())
}

fn validate_project_name(name: &str) -> Result<(), ConfigError> {
    validate_name_shape("project.name", name, MAX_PROJECT_NAME_BYTES)
}

fn validate_project_version(version: &str) -> Result<(), ConfigError> {
    if version.len() > MAX_PROJECT_VERSION_BYTES {
        return Err(ConfigError::validation(
            "project.version",
            format!(
                "version exceeds {MAX_PROJECT_VERSION_BYTES} bytes (got {})",
                version.len()
            ),
        ));
    }
    let mut bytes = version.bytes();
    let first = bytes.next().unwrap_or(b'0');
    let ok = first.is_ascii_digit()
        && bytes
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'_' || b == b'-');
    if version.is_empty() || !ok {
        return Err(ConfigError::validation(
            "project.version",
            format!("version '{}' must start with a digit", snip(version)),
        ));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<(), ConfigError> {
    if description.chars().count() > MAX_PROJECT_DESCRIPTION_CHARS {
        return Err(ConfigError::validation(
            "project.description",
            format!("description exceeds {MAX_PROJECT_DESCRIPTION_CHARS} characters"),
        ));
    }
    if description
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(ConfigError::validation(
            "project.description",
            "description must not contain control characters other than newline and tab",
        ));
    }
    Ok(())
}

fn validate_entry_name(name: &str) -> Result<(), ConfigError> {
    validate_name_shape("section entry", name, MAX_ENTRY_NAME_BYTES)
}

/// Entry paths stay inside their section by construction: relative, no
/// `.`/`..` segments, conservative segment charset, and prefixed with
/// `<section>/` so a manifest can never name outside its own directory.
fn validate_entry_path(section: ProjectSection, path: &str) -> Result<(), ConfigError> {
    if path.len() > MAX_ENTRY_PATH_BYTES {
        return Err(ConfigError::validation(
            format!("{section} entry"),
            format!(
                "path exceeds {MAX_ENTRY_PATH_BYTES} bytes (got {})",
                path.len()
            ),
        ));
    }
    let prefix = format!("{}/", section.dir_name());
    if !path.starts_with(&prefix) {
        return Err(ConfigError::validation(
            format!("{section} entry"),
            format!(
                "path '{}' must stay inside '{}/'",
                snip(path),
                section.dir_name()
            ),
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ConfigError::validation(
                format!("{section} entry"),
                format!(
                    "path '{}' must not contain empty or dot segments",
                    snip(path)
                ),
            ));
        }
        if segment.len() > MAX_PATH_SEGMENT_BYTES {
            return Err(ConfigError::validation(
                format!("{section} entry"),
                format!("path segment '{}' is too long", snip(segment)),
            ));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return Err(ConfigError::validation(
                format!("{section} entry"),
                format!(
                    "path segment '{}' uses characters outside [A-Za-z0-9_.-]",
                    snip(segment)
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "schema_version = 1\n[project]\nname = \"demo\"\n";

    fn parse(text: &str) -> ProjectDefinition {
        parse_project_toml(text).expect("valid fixture must parse")
    }

    #[test]
    fn minimal_definition_parses() {
        let def = parse(MINIMAL);
        assert_eq!(def.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(def.project.name, "demo");
        assert_eq!(def.project.version, None);
        assert_eq!(def.project.description, None);
        assert!(def.sections.is_empty());
    }

    #[test]
    fn full_definition_parses_with_all_sections() {
        let def = parse(
            "schema_version = 1 # trailing comment\n\
             [project]\n\
             name = \"demo\"\n\
             version = '1.2.0'\n\
             description = \"line one\\nline two\"\n\
             [agents]\n\
             main = \"agents/main.md\"\n\
             [workflows]\n\
             review = \"workflows/review.md\"\n\
             [prompts]\n\
             fix = \"prompts/fix.md\"\n\
             [policies]\n\
             guard = \"policies/guard.md\"\n\
             [tools]\n\
             lint = \"tools/lint.md\"\n\
             [skills]\n\
             rust = \"skills/rust.md\"\n",
        );
        assert_eq!(def.project.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            def.project.description.as_deref(),
            Some("line one\nline two")
        );
        for section in ProjectSection::all() {
            assert_eq!(def.entries(section).map(BTreeMap::len), Some(1));
        }
        assert_eq!(
            def.entries(ProjectSection::Agents).expect("agents section")["main"],
            "agents/main.md"
        );
    }

    #[test]
    fn missing_schema_version_denied() {
        let err = parse_project_toml("[project]\nname = \"demo\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn unsupported_schema_version_denied() {
        let err =
            parse_project_toml("schema_version = 2\n[project]\nname = \"demo\"\n").unwrap_err();
        assert_eq!(
            err,
            ConfigError::SchemaVersionUnsupported {
                found: 2,
                supported: PROJECT_SCHEMA_VERSION,
            }
        );
    }

    #[test]
    fn missing_project_name_denied() {
        let err =
            parse_project_toml("schema_version = 1\n[project]\nversion = \"1\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn unknown_top_level_key_denied() {
        let err =
            parse_project_toml("schema_version = 1\nfuture = \"x\"\n[project]\nname = \"demo\"\n")
                .unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn unknown_table_denied() {
        let err = parse_project_toml("schema_version = 1\n[runtime]\nfoo = \"x\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn unknown_project_key_denied() {
        let err = parse_project_toml(
            "schema_version = 1\n[project]\nname = \"demo\"\nexec = \"run.sh\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UndeclaredField { .. }));
    }

    #[test]
    fn duplicate_key_denied() {
        let err = parse_project_toml("schema_version = 1\n[project]\nname = \"a\"\nname = \"b\"\n")
            .unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn duplicate_table_denied() {
        let err = parse_project_toml(
            "schema_version = 1\n[project]\nname = \"a\"\n[project]\nname = \"b\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn non_string_values_denied() {
        for value in ["true", "1.5", "{ a = 1 }", "[\"a\"]", "1979-05-27"] {
            let doc = format!("schema_version = 1\n[project]\nname = {value}\n");
            assert!(
                parse_project_toml(&doc).is_err(),
                "value {value} must be denied"
            );
        }
    }

    #[test]
    fn integer_schema_version_with_underscores_parses_digits() {
        // `1_0` reads as 10, which the version gate then denies: underscores
        // are digit separators only, never a way past the gate.
        let err =
            parse_project_toml("schema_version = 1_0\n[project]\nname = \"demo\"\n").unwrap_err();
        assert_eq!(
            err,
            ConfigError::SchemaVersionUnsupported {
                found: 10,
                supported: PROJECT_SCHEMA_VERSION,
            }
        );
    }

    #[test]
    fn absolute_and_parent_paths_denied() {
        for path in [
            "/etc/passwd",
            "agents/../../escape.md",
            "workflows/x.md",
            "agents/.hidden/../x.md",
            "./agents/x.md",
            "agents//x.md",
        ] {
            let doc = format!(
                "schema_version = 1\n[project]\nname = \"demo\"\n[agents]\nmain = \"{path}\"\n"
            );
            assert!(
                parse_project_toml(&doc).is_err(),
                "path {path} must be denied"
            );
        }
    }

    #[test]
    fn bad_names_and_versions_denied() {
        for name in ["", "Demo", "-x", "a b", "a/b"] {
            let doc = format!("schema_version = 1\n[project]\nname = \"{name}\"\n");
            assert!(
                parse_project_toml(&doc).is_err(),
                "name '{name}' must be denied"
            );
        }
        let err = parse_project_toml(
            "schema_version = 1\n[project]\nname = \"demo\"\nversion = \"v1\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn string_escapes_decode() {
        let def = parse(
            "schema_version = 1\n[project]\nname = \"demo\"\ndescription = \"A\\u0021B\\tC\"\n",
        );
        assert_eq!(def.project.description.as_deref(), Some("A!B\tC"));
    }

    #[test]
    fn hash_inside_string_is_not_a_comment() {
        let def = parse("schema_version = 1\n[project]\nname = \"demo\"\ndescription = \"a#b\"\n");
        assert_eq!(def.project.description.as_deref(), Some("a#b"));
    }

    #[test]
    fn unterminated_and_multiline_strings_denied() {
        assert!(parse_project_toml("schema_version = 1\n[project]\nname = \"abc\n").is_err());
        assert!(parse_project_toml("schema_version = 1\n[project]\nname = \"\"\"\n").is_err());
    }

    #[test]
    fn oversize_document_denied() {
        let big = format!(
            "schema_version = 1\n# {}\n[project]\nname = \"demo\"\n",
            "x".repeat(MAX_PROJECT_TOML_BYTES)
        );
        assert!(matches!(
            parse_project_toml(&big).unwrap_err(),
            ConfigError::InvalidInput { .. }
        ));
    }

    #[test]
    fn oversize_name_denied_without_echoing_it_whole() {
        let long = "a".repeat(MAX_PROJECT_NAME_BYTES + 1);
        let doc = format!("schema_version = 1\n[project]\nname = \"{long}\"\n");
        let err = parse_project_toml(&doc).unwrap_err();
        let rendered = format!("{err:?}");
        assert!(rendered.len() < long.len(), "diagnostic must stay bounded");
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn section_labels_stable() {
        assert_eq!(ProjectSection::Agents.dir_name(), "agents");
        assert_eq!(ProjectSection::Skills.to_string(), "skills");
        assert_eq!(
            ProjectSection::parse("workflows"),
            Ok(ProjectSection::Workflows)
        );
        assert!(ProjectSection::parse("runtime").is_err());
    }
}
