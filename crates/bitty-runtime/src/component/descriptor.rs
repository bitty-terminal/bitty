//! Component install layout, descriptor parsing, and fail-closed
//! verification (name, version, executable name, protocol, SHA-256).
//!
//! Layout (DIR-030): `<root>/<name>/current` holds the active version and
//! `<root>/<name>/<version>/` holds `bitty-component.toml` plus the
//! executable `bitty-<name>` (`bitty-<name>.exe` on Windows).
//!
//! The descriptor is a closed subset of TOML parsed by hand: one
//! `[component]` table with exactly the keys `name`, `version`, `protocol`
//! (`[min, max]`), `executable`, and `sha256`. Unknown tables or keys,
//! duplicates, escapes, and multi-line values are rejected.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use bitty_network_wire::PROTOCOL_VERSION;
use bitty_package::Version;
use bitty_package::integrity::Sha256Hasher;

use super::{
    COMPONENT_CURRENT_FILE, COMPONENT_CURRENT_MAX_BYTES, COMPONENT_DESCRIPTOR_FILE,
    COMPONENT_DESCRIPTOR_MAX_BYTES, COMPONENT_DIGEST_BUFFER_BYTES, COMPONENT_EXECUTABLE_MAX_BYTES,
    COMPONENT_EXECUTABLE_PREFIX, COMPONENT_NAME_MAX_BYTES, COMPONENTS_DIR_NAME,
};

/// Hex length of a SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// Component root from explicit inputs.
///
/// `override_dir` is the developer-only `BITTY_COMPONENTS_DIR` value and
/// wins when non-empty; otherwise the root is
/// `<data_home>/bitty/components`, where `data_home` comes from the same
/// resolver as the plugin store ([`crate::data_home_for`]). Never consults
/// `PATH`.
#[must_use]
pub fn components_root_for(
    override_dir: Option<&str>,
    data_home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    data_home.map(|base| base.join("bitty").join(COMPONENTS_DIR_NAME))
}

/// Validate a component name against `[a-z][a-z0-9-]{0,31}`.
pub fn validate_component_name(name: &str) -> Result<(), DescriptorError> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= COMPONENT_NAME_MAX_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if valid {
        Ok(())
    } else {
        Err(DescriptorError::InvalidName)
    }
}

/// Platform executable file name for a descriptor `executable` value.
#[must_use]
pub fn executable_file_name(executable: &str) -> String {
    format!("{executable}{}", std::env::consts::EXE_SUFFIX)
}

/// A validated `bitty-component.toml` (v1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentDescriptor {
    /// Component name (`[a-z][a-z0-9-]{0,31}`).
    pub name: String,
    /// Strict semver version.
    pub version: String,
    /// Lowest supported wire protocol version.
    pub protocol_min: u16,
    /// Highest supported wire protocol version.
    pub protocol_max: u16,
    /// Executable base name (`bitty-<name>`, no path separators).
    pub executable: String,
    /// Lowercase hex SHA-256 of the executable.
    pub sha256: String,
}

/// Descriptor-level validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// Malformed descriptor syntax at a 1-based line.
    Syntax {
        /// Line number.
        line: usize,
        /// What was wrong.
        reason: &'static str,
    },
    /// A required key is missing.
    MissingKey(&'static str),
    /// The name violates `[a-z][a-z0-9-]{0,31}`.
    InvalidName,
    /// The version is not strict semver.
    InvalidVersion,
    /// The executable name is not `bitty-<name>` or contains separators.
    InvalidExecutable,
    /// The digest is not 64 lowercase hex characters.
    InvalidDigest,
    /// `protocol` is not `[min, max]` with `1 <= min <= max`.
    InvalidProtocol,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DescriptorError::Syntax { line, reason } => {
                write!(f, "descriptor line {line}: {reason}")
            }
            DescriptorError::MissingKey(key) => write!(f, "descriptor is missing '{key}'"),
            DescriptorError::InvalidName => {
                f.write_str("component name must match [a-z][a-z0-9-]{0,31}")
            }
            DescriptorError::InvalidVersion => {
                f.write_str("component version must be strict semver")
            }
            DescriptorError::InvalidExecutable => {
                f.write_str("executable must be 'bitty-<name>' without path separators")
            }
            DescriptorError::InvalidDigest => {
                f.write_str("sha256 must be 64 lowercase hex characters")
            }
            DescriptorError::InvalidProtocol => {
                f.write_str("protocol must be [min, max] with 1 <= min <= max")
            }
        }
    }
}

impl std::error::Error for DescriptorError {}

impl ComponentDescriptor {
    /// Parse and validate descriptor text (fail closed).
    pub fn parse(text: &str) -> Result<Self, DescriptorError> {
        let mut in_component = false;
        let mut seen_table = false;
        let mut name = None;
        let mut version = None;
        let mut protocol = None;
        let mut executable = None;
        let mut sha256 = None;
        for (index, raw_line) in text.lines().enumerate() {
            let line_no = index + 1;
            let syntax = |reason| DescriptorError::Syntax {
                line: line_no,
                reason,
            };
            let line = strip_comment(raw_line).map_err(syntax)?.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                if line != "[component]" {
                    return Err(syntax("only the [component] table is allowed"));
                }
                if seen_table {
                    return Err(syntax("duplicate [component] table"));
                }
                seen_table = true;
                in_component = true;
                continue;
            }
            if !in_component {
                return Err(syntax("key outside the [component] table"));
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| syntax("expected key = value"))?;
            let key = key.trim();
            let value = value.trim();
            let slot = match key {
                "name" => &mut name,
                "version" => &mut version,
                "executable" => &mut executable,
                "sha256" => &mut sha256,
                "protocol" => {
                    if protocol.is_some() {
                        return Err(syntax("duplicate key"));
                    }
                    protocol = Some(parse_protocol(value).map_err(syntax)?);
                    continue;
                }
                _ => return Err(syntax("unknown key")),
            };
            if slot.is_some() {
                return Err(syntax("duplicate key"));
            }
            *slot = Some(parse_basic_string(value).map_err(syntax)?);
        }
        let name = name.ok_or(DescriptorError::MissingKey("name"))?;
        let version = version.ok_or(DescriptorError::MissingKey("version"))?;
        let (protocol_min, protocol_max) =
            protocol.ok_or(DescriptorError::MissingKey("protocol"))?;
        let executable = executable.ok_or(DescriptorError::MissingKey("executable"))?;
        let sha256 = sha256.ok_or(DescriptorError::MissingKey("sha256"))?;

        validate_component_name(&name)?;
        validate_version(&version)?;
        if protocol_min == 0 || protocol_min > protocol_max {
            return Err(DescriptorError::InvalidProtocol);
        }
        if executable != format!("{COMPONENT_EXECUTABLE_PREFIX}{name}")
            || !is_plain_file_name(&executable)
        {
            return Err(DescriptorError::InvalidExecutable);
        }
        if sha256.len() != SHA256_HEX_LEN
            || !sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(DescriptorError::InvalidDigest);
        }
        Ok(Self {
            name,
            version,
            protocol_min,
            protocol_max,
            executable,
            sha256,
        })
    }

    /// Whether the descriptor's protocol range includes Core's version.
    #[must_use]
    pub fn supports_core_protocol(&self) -> bool {
        (self.protocol_min..=self.protocol_max).contains(&PROTOCOL_VERSION)
    }
}

/// A component resolved, verified, and ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponent {
    /// The validated descriptor.
    pub descriptor: ComponentDescriptor,
    /// `<root>/<name>/<version>/` (the child's working directory).
    pub version_dir: PathBuf,
    /// Absolute path of the verified executable.
    pub executable_path: PathBuf,
}

/// Why a component could not be resolved (fail closed, never spawned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No component root (no data directory and no override).
    NoRoot,
    /// The requested name is invalid.
    InvalidName,
    /// `<root>/<name>/current` does not exist: the component is not
    /// installed.
    NotInstalled {
        /// Component name.
        name: String,
    },
    /// The `current` file is unreadable, oversize, or not a semver version.
    InvalidCurrent {
        /// Detail.
        reason: String,
    },
    /// The version directory or descriptor is missing or unreadable.
    Unreadable {
        /// File that failed.
        path: PathBuf,
        /// I/O error kind.
        kind: std::io::ErrorKind,
    },
    /// A file exceeds its size bound.
    TooLarge {
        /// File that is too large.
        path: PathBuf,
        /// The bound.
        limit: u64,
    },
    /// The descriptor failed validation.
    Descriptor(DescriptorError),
    /// The descriptor names a different component.
    NameMismatch {
        /// Name in the descriptor.
        found: String,
    },
    /// The descriptor version differs from `current`.
    VersionMismatch {
        /// Version in `current`.
        current: String,
        /// Version in the descriptor.
        found: String,
    },
    /// The descriptor protocol range excludes Core's protocol version.
    IncompatibleProtocol {
        /// Descriptor minimum.
        min: u16,
        /// Descriptor maximum.
        max: u16,
    },
    /// The executable or version directory is not a regular file or
    /// directory (for example a symlink).
    NotRegular {
        /// Offending path.
        path: PathBuf,
    },
    /// The executable digest does not match the descriptor.
    DigestMismatch {
        /// Digest from the descriptor.
        expected: String,
        /// Digest of the file on disk.
        actual: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NoRoot => f.write_str("no component root (data directory unavailable)"),
            ResolveError::InvalidName => f.write_str("invalid component name"),
            ResolveError::NotInstalled { name } => write!(f, "component '{name}' is not installed"),
            ResolveError::InvalidCurrent { reason } => {
                write!(f, "invalid 'current' file: {reason}")
            }
            ResolveError::Unreadable { path, kind } => {
                write!(f, "cannot read {}: {kind}", path.display())
            }
            ResolveError::TooLarge { path, limit } => {
                write!(f, "{} exceeds {limit} bytes", path.display())
            }
            ResolveError::Descriptor(error) => write!(f, "invalid descriptor: {error}"),
            ResolveError::NameMismatch { found } => {
                write!(f, "descriptor names component '{found}'")
            }
            ResolveError::VersionMismatch { current, found } => {
                write!(
                    f,
                    "descriptor version '{found}' differs from current '{current}'"
                )
            }
            ResolveError::IncompatibleProtocol { min, max } => write!(
                f,
                "component protocol range [{min}, {max}] excludes core protocol {PROTOCOL_VERSION}"
            ),
            ResolveError::NotRegular { path } => {
                write!(f, "{} is not a regular file or directory", path.display())
            }
            ResolveError::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "executable sha256 {actual} does not match descriptor {expected}"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<DescriptorError> for ResolveError {
    fn from(error: DescriptorError) -> Self {
        ResolveError::Descriptor(error)
    }
}

/// Resolve and verify component `name` under `root`.
///
/// Reads `current`, the descriptor, and the executable (each bounded),
/// checks every descriptor field, and verifies the executable's SHA-256.
/// Call this before every spawn; the broker does.
pub fn resolve(root: &Path, name: &str) -> Result<ResolvedComponent, ResolveError> {
    validate_component_name(name).map_err(|_| ResolveError::InvalidName)?;
    let component_dir = root.join(name);
    let current_path = component_dir.join(COMPONENT_CURRENT_FILE);
    let current_bytes = match read_bounded(&current_path, COMPONENT_CURRENT_MAX_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(ResolveError::Unreadable {
            kind: std::io::ErrorKind::NotFound,
            ..
        }) => {
            return Err(ResolveError::NotInstalled {
                name: name.to_owned(),
            });
        }
        Err(error) => return Err(error),
    };
    let current = std::str::from_utf8(&current_bytes)
        .map_err(|_| ResolveError::InvalidCurrent {
            reason: "not UTF-8".into(),
        })?
        .trim_end_matches(['\n', '\r']);
    validate_version(current).map_err(|_| ResolveError::InvalidCurrent {
        reason: "not a strict semver version".into(),
    })?;

    let version_dir = component_dir.join(current);
    require_kind(&version_dir, false)?;
    let descriptor_path = version_dir.join(COMPONENT_DESCRIPTOR_FILE);
    let descriptor_bytes = read_bounded(&descriptor_path, COMPONENT_DESCRIPTOR_MAX_BYTES as u64)?;
    let descriptor_text = std::str::from_utf8(&descriptor_bytes).map_err(|_| {
        ResolveError::Descriptor(DescriptorError::Syntax {
            line: 0,
            reason: "descriptor is not UTF-8",
        })
    })?;
    let descriptor = ComponentDescriptor::parse(descriptor_text)?;
    if descriptor.name != name {
        return Err(ResolveError::NameMismatch {
            found: descriptor.name,
        });
    }
    if descriptor.version != current {
        return Err(ResolveError::VersionMismatch {
            current: current.to_owned(),
            found: descriptor.version,
        });
    }
    if !descriptor.supports_core_protocol() {
        return Err(ResolveError::IncompatibleProtocol {
            min: descriptor.protocol_min,
            max: descriptor.protocol_max,
        });
    }

    let executable_path = version_dir.join(executable_file_name(&descriptor.executable));
    require_kind(&executable_path, true)?;
    let actual = digest_bounded(&executable_path, COMPONENT_EXECUTABLE_MAX_BYTES)?;
    if actual != descriptor.sha256 {
        return Err(ResolveError::DigestMismatch {
            expected: descriptor.sha256,
            actual,
        });
    }
    Ok(ResolvedComponent {
        descriptor,
        version_dir,
        executable_path,
    })
}

/// Strict semver that is also a single safe path segment.
fn validate_version(version: &str) -> Result<(), DescriptorError> {
    if !is_plain_file_name(version) || Version::parse(version).is_err() {
        return Err(DescriptorError::InvalidVersion);
    }
    Ok(())
}

/// Non-empty, not `.`/`..`, and free of separators, NUL, and controls.
fn is_plain_file_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c == '/' || c == '\\' || c == ':' || c.is_control() || c.is_whitespace())
}

/// Require a regular file (`file = true`) or directory, never a symlink.
fn require_kind(path: &Path, file: bool) -> Result<(), ResolveError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| ResolveError::Unreadable {
        path: path.to_owned(),
        kind: error.kind(),
    })?;
    let ok = if file {
        metadata.file_type().is_file()
    } else {
        metadata.file_type().is_dir()
    };
    if ok {
        Ok(())
    } else {
        Err(ResolveError::NotRegular {
            path: path.to_owned(),
        })
    }
}

/// Read at most `limit` bytes; a longer file fails closed.
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, ResolveError> {
    let unreadable = |error: std::io::Error| ResolveError::Unreadable {
        path: path.to_owned(),
        kind: error.kind(),
    };
    let file = fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(ResolveError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

/// SHA-256 hex of the file at `path`, streamed through a fixed
/// [`COMPONENT_DIGEST_BUFFER_BYTES`] buffer (DIR-030 D5): memory stays flat
/// whatever the file size. A file longer than `limit` fails closed.
fn digest_bounded(path: &Path, limit: u64) -> Result<String, ResolveError> {
    let unreadable = |error: std::io::Error| ResolveError::Unreadable {
        path: path.to_owned(),
        kind: error.kind(),
    };
    let mut file = fs::File::open(path)
        .map_err(unreadable)?
        .take(limit.saturating_add(1));
    let mut hasher = Sha256Hasher::new();
    let mut buf = vec![0u8; COMPONENT_DIGEST_BUFFER_BYTES];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(unreadable(error)),
        };
        hasher.update(&buf[..n]);
        if hasher.len() > limit {
            return Err(ResolveError::TooLarge {
                path: path.to_owned(),
                limit,
            });
        }
    }
    Ok(hasher.finalize_hex())
}

/// Remove a `#` comment that is outside a basic string.
fn strip_comment(line: &str) -> Result<&str, &'static str> {
    let mut in_string = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => return Ok(&line[..index]),
            _ => {}
        }
    }
    if in_string {
        return Err("unterminated string");
    }
    Ok(line)
}

/// `"..."` with no escapes and no embedded quotes.
fn parse_basic_string(value: &str) -> Result<String, &'static str> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or("expected a double-quoted string")?;
    if inner.contains('"') || inner.contains('\\') {
        return Err("escapes and embedded quotes are not allowed");
    }
    if inner.chars().any(char::is_control) {
        return Err("control characters are not allowed");
    }
    Ok(inner.to_owned())
}

/// `[min, max]` with two unsigned 16-bit integers.
fn parse_protocol(value: &str) -> Result<(u16, u16), &'static str> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or("protocol must be an array [min, max]")?;
    let mut parts = inner.split(',').map(str::trim);
    let (Some(min), Some(max), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("protocol must have exactly two entries");
    };
    let parse = |raw: &str| {
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return Err("protocol entries must be unsigned integers");
        }
        raw.parse::<u16>()
            .map_err(|_| "protocol entry out of range")
    };
    Ok((parse(min)?, parse(max)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn text(name: &str, version: &str, protocol: &str, executable: &str, sha: &str) -> String {
        format!(
            "# installed by the package manager\n[component]\nname = \"{name}\"   # grammar\nversion = \"{version}\"\nprotocol = {protocol}\nexecutable = \"{executable}\"\nsha256 = \"{sha}\"\n"
        )
    }

    #[test]
    fn parses_the_v1_example() {
        let descriptor =
            ComponentDescriptor::parse(&text("net", "0.1.0", "[1, 1]", "bitty-net", DIGEST))
                .expect("valid");
        assert_eq!(descriptor.name, "net");
        assert_eq!(descriptor.version, "0.1.0");
        assert_eq!((descriptor.protocol_min, descriptor.protocol_max), (1, 1));
        assert!(descriptor.supports_core_protocol());
    }

    #[test]
    fn name_grammar() {
        for good in ["net", "ai", "a", "net-2", &"a".repeat(32)] {
            assert!(validate_component_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "Net",
            "2net",
            "-net",
            "net_x",
            "net.x",
            "ne/t",
            &"a".repeat(33),
        ] {
            assert!(validate_component_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_invalid_fields() {
        let cases = [
            (
                text("Net", "0.1.0", "[1, 1]", "bitty-Net", DIGEST),
                DescriptorError::InvalidName,
            ),
            (
                text("net", "1.0", "[1, 1]", "bitty-net", DIGEST),
                DescriptorError::InvalidVersion,
            ),
            (
                text("net", "0.1.0", "[2, 1]", "bitty-net", DIGEST),
                DescriptorError::InvalidProtocol,
            ),
            (
                text("net", "0.1.0", "[0, 1]", "bitty-net", DIGEST),
                DescriptorError::InvalidProtocol,
            ),
            (
                text("net", "0.1.0", "[1, 1]", "../bitty-net", DIGEST),
                DescriptorError::InvalidExecutable,
            ),
            (
                text("net", "0.1.0", "[1, 1]", "bin/bitty-net", DIGEST),
                DescriptorError::InvalidExecutable,
            ),
            (
                text("net", "0.1.0", "[1, 1]", "bitty-other", DIGEST),
                DescriptorError::InvalidExecutable,
            ),
            (
                text(
                    "net",
                    "0.1.0",
                    "[1, 1]",
                    "bitty-net",
                    &DIGEST.to_uppercase(),
                ),
                DescriptorError::InvalidDigest,
            ),
            (
                text("net", "0.1.0", "[1, 1]", "bitty-net", &DIGEST[1..]),
                DescriptorError::InvalidDigest,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(ComponentDescriptor::parse(&input), Err(expected), "{input}");
        }
    }

    #[test]
    fn rejects_unknown_keys_tables_and_duplicates() {
        let base = text("net", "0.1.0", "[1, 1]", "bitty-net", DIGEST);
        for extra in [
            "path = \"x\"\n",
            "[other]\n",
            "[component]\n",
            "name = \"net\"\n",
            "protocol = [1, 1]\n",
        ] {
            let input = format!("{base}{extra}");
            assert!(
                matches!(
                    ComponentDescriptor::parse(&input),
                    Err(DescriptorError::Syntax { .. })
                ),
                "{extra}"
            );
        }
        assert!(matches!(
            ComponentDescriptor::parse("name = \"net\"\n"),
            Err(DescriptorError::Syntax { line: 1, .. })
        ));
        assert_eq!(
            ComponentDescriptor::parse("[component]\nname = \"net\"\n"),
            Err(DescriptorError::MissingKey("version"))
        );
        assert!(ComponentDescriptor::parse(&base.replace("\"net\"", "\"n\\u0065t\"")).is_err());
        assert!(ComponentDescriptor::parse(&base.replace("[1, 1]", "[1, 1, 1]")).is_err());
        assert!(ComponentDescriptor::parse(&base.replace("[1, 1]", "[1, 70000]")).is_err());
    }

    #[test]
    fn root_override_wins_and_never_uses_path() {
        let data = Path::new("data-home");
        assert_eq!(
            components_root_for(Some("override-root"), Some(data)),
            Some(PathBuf::from("override-root"))
        );
        assert_eq!(
            components_root_for(Some("  "), Some(data)),
            Some(data.join("bitty").join("components"))
        );
        assert_eq!(components_root_for(None, None), None);
    }

    #[test]
    fn streamed_digest_matches_one_shot_and_enforces_the_limit() {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0920-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("blob");
        // Several full buffers plus a partial one.
        let size = COMPONENT_DIGEST_BUFFER_BYTES * 3 + 17;
        let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).expect("write");
        let streamed = digest_bounded(&path, size as u64).expect("digest");
        assert_eq!(streamed, bitty_package::integrity::sha256_hex(&bytes));
        assert!(matches!(
            digest_bounded(&path, size as u64 - 1),
            Err(ResolveError::TooLarge { .. })
        ));
        assert!(matches!(
            digest_bounded(&dir.join("missing"), size as u64),
            Err(ResolveError::Unreadable { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn executable_file_name_uses_platform_suffix() {
        let expected = if cfg!(windows) {
            "bitty-net.exe"
        } else {
            "bitty-net"
        };
        assert_eq!(executable_file_name("bitty-net"), expected);
    }
}
