//! Two-tier component inventory (issue #1651, DIR-030 D6).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/components/` wins over the
//! system `/usr/lib/bitty/components/` (or platform equivalent). The user
//! tier is writable without privilege; the system tier is read-only for
//! `bitty component` (removal there needs a package manager). `PATH` is
//! never consulted.
//!
//! Install sources in v1 are local paths only through
//! `bitty component add` (a directory holding `bitty-component.toml` plus the
//! executable, or a bare `bitty-<name>` executable with `--version`). No
//! network fetch, no registry download: a URL operand fails closed with an
//! actionable diagnostic. Registry/download sources are a follow-up.
//!
//! Every function takes explicit roots; nothing reads process environment
//! here. Callers resolve roots from `XDG_DATA_HOME`/`HOME` plus the
//! test-only `BITTY_COMPONENTS_DIR` / `BITTY_SYSTEM_COMPONENTS_DIR`
//! overrides, so unit tests stay hermetic in tempdirs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::descriptor::{ComponentDescriptor, ResolveError, resolve, validate_component_name};
use super::{
    COMPONENT_CURRENT_FILE, COMPONENT_DESCRIPTOR_FILE, COMPONENT_DESCRIPTOR_MAX_BYTES,
    COMPONENT_EXECUTABLE_PREFIX,
};

/// Test-only override for the system component root (not a user
/// configuration surface; mirrors [`super::COMPONENTS_DIR_ENV`]).
pub const SYSTEM_COMPONENTS_DIR_ENV: &str = "BITTY_SYSTEM_COMPONENTS_DIR";

/// System component root on Unix (read-only for `bitty component`).
#[cfg(not(windows))]
pub const SYSTEM_COMPONENTS_DIR_DEFAULT: &str = "/usr/lib/bitty/components";

/// System component root on Windows (read-only for `bitty component`).
#[cfg(windows)]
pub const SYSTEM_COMPONENTS_DIR_DEFAULT: &str = "C:\\Program Files\\Bitty\\components";

/// Which tier a component resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSource {
    /// `$XDG_DATA_HOME/bitty/components/` (writable, wins on collision).
    User,
    /// `/usr/lib/bitty/components/` or platform equivalent (read-only).
    System,
}

impl ComponentSource {
    /// Stable label for table/JSON output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

/// System component root from an explicit override value.
///
/// A non-empty override wins (tests); otherwise the platform default is
/// returned. The default is always `Some`: the system tier is a portable
/// contract path, not a host leftover.
#[must_use]
pub fn system_components_root_for(override_dir: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    Some(PathBuf::from(SYSTEM_COMPONENTS_DIR_DEFAULT))
}

/// Both search roots for one resolution.
///
/// `user_override` is the `BITTY_COMPONENTS_DIR` value, `data_home` the
/// resolved data directory (see [`super::data_home_for`]), and
/// `system_override` the `BITTY_SYSTEM_COMPONENTS_DIR` value.
#[must_use]
pub fn component_search_roots_for(
    user_override: Option<&str>,
    data_home: Option<&Path>,
    system_override: Option<&str>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let user = super::descriptor::components_root_for(user_override, data_home);
    // An explicit empty user override falls back to the data-home root
    // inside `components_root_for`; only a missing data directory yields
    // `None` here (no-root execution).
    let system = system_components_root_for(system_override);
    (user, system)
}

/// A component resolved through the two-tier search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchedComponent {
    /// The verified resolution.
    pub resolved: super::descriptor::ResolvedComponent,
    /// Which tier won.
    pub source: ComponentSource,
}

/// Resolve `name` with user-over-system priority.
///
/// The user tier is tried first. A user `NotInstalled` falls through to the
/// system tier; any other user error (invalid `current`, digest mismatch,
/// ABI incompatibility) fails closed without consulting the system tier so
/// a tampered user install cannot silently fall back.
pub fn resolve_search(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    name: &str,
) -> Result<SearchedComponent, ResolveError> {
    validate_component_name(name).map_err(|_| ResolveError::InvalidName)?;
    if let Some(user) = user_root {
        match resolve(user, name) {
            Ok(resolved) => {
                return Ok(SearchedComponent {
                    resolved,
                    source: ComponentSource::User,
                });
            }
            Err(ResolveError::NotInstalled { .. }) => {}
            Err(ResolveError::NoRoot) => {}
            Err(other) => return Err(other),
        }
    }
    if let Some(system) = system_root {
        let resolved = resolve(system, name)?;
        return Ok(SearchedComponent {
            resolved,
            source: ComponentSource::System,
        });
    }
    // No tier had the component (or no root at all): report the user-tier
    // shape so callers emit one actionable diagnostic.
    if user_root.is_none() && system_root.is_none() {
        return Err(ResolveError::NoRoot);
    }
    Err(ResolveError::NotInstalled {
        name: name.to_owned(),
    })
}

/// One installed version of one component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    /// Version string (directory name, strict semver).
    pub version: String,
    /// Which tier holds this version.
    pub source: ComponentSource,
    /// `<root>/<name>/<version>/`.
    pub version_dir: PathBuf,
    /// Absolute executable path.
    pub executable_path: PathBuf,
    /// Descriptor protocol range.
    pub protocol_min: u16,
    /// Descriptor protocol range.
    pub protocol_max: u16,
    /// Whether the protocol range includes Core's wire version
    /// (ABI-compat check before activation).
    pub compatible: bool,
}

/// One component name with its installed versions and active pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSummary {
    /// Component name.
    pub name: String,
    /// Installed versions, sorted ascending, user shadowing system on the
    /// same version string.
    pub versions: Vec<InstalledVersion>,
    /// Version named by the winning `current` file, if any.
    pub active_version: Option<String>,
    /// Which tier owns the winning `current` file.
    pub active_source: Option<ComponentSource>,
    /// Executable path of the active version, when it is installed.
    pub active_path: Option<PathBuf>,
}

/// Discover every installed component across both tiers.
///
/// Enumeration is best-effort and bounded: unreadable roots yield no names,
/// malformed descriptors are skipped (spawn-time `resolve` still fails
/// closed). The winning `current` is the user tier when its file exists and
/// is non-empty, else the system tier.
pub fn discover_components(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
) -> Vec<ComponentSummary> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for root in [user_root, system_root].into_iter().flatten() {
        for name in list_component_names(root) {
            names.insert(name);
        }
    }
    let mut out = Vec::new();
    for name in names {
        let mut by_version: BTreeMap<String, InstalledVersion> = BTreeMap::new();
        // User first so system entries with the same version do not clobber.
        if let Some(user) = user_root {
            for installed in list_versions(user, &name, ComponentSource::User) {
                by_version.insert(installed.version.clone(), installed);
            }
        }
        if let Some(system) = system_root {
            for installed in list_versions(system, &name, ComponentSource::System) {
                by_version
                    .entry(installed.version.clone())
                    .or_insert(installed);
            }
        }
        let versions: Vec<InstalledVersion> = by_version.into_values().collect();
        let (active_version, active_source) = read_active_version(user_root, system_root, &name);
        let active_path = active_version.as_ref().and_then(|active| {
            versions
                .iter()
                .find(|entry| &entry.version == active)
                .map(|entry| entry.executable_path.clone())
        });
        out.push(ComponentSummary {
            name,
            versions,
            active_version,
            active_source,
            active_path,
        });
    }
    out
}

/// Actionable hint for a missing optional component (soft-fail, never a
/// panic). Names the local-path install command; v1 performs no download.
#[must_use]
pub fn missing_component_hint(name: &str) -> String {
    format!(
        "component '{name}' is not installed; install it with `bitty component add <path>` \
         (local directory or `bitty-<name>` executable plus `--version <semver>`; v1 has no registry download)"
    )
}

/// Actionable hint for an ABI-incompatible component.
#[must_use]
pub fn incompatible_component_hint(name: &str, min: u16, max: u16, core: u16) -> String {
    format!(
        "component '{name}' protocol range [{min}, {max}] excludes core protocol {core}; \
         install a compatible build with `bitty component add <path>`"
    )
}

/// Whether `requirement` (caret `^X.Y.Z`, as in plugin `[components]`) is
/// satisfied by `version`. Caret semantics with Cargo rules: `^0.0.1` admits
/// exactly `0.0.1`; `^0.1` admits `>=0.1.0, <0.2.0`; `^1.2.3` admits
/// `>=1.2.3, <2.0.0`. Partial requirements (`^0.1`, `^1`) pad missing parts
/// with zero. Malformed inputs never satisfy.
#[must_use]
pub fn version_satisfies_caret(version: &str, requirement: &str) -> bool {
    let requirement = requirement.trim();
    let bare = requirement.strip_prefix('^').unwrap_or(requirement);
    let Some((want_major, want_minor, want_patch)) = parse_caret_base(bare) else {
        return false;
    };
    let Ok(have) = bitty_package::Version::parse(version) else {
        return false;
    };
    // Prerelease requirements are exact on the prerelease string; a stable
    // release never satisfies a prerelease requirement and vice versa.
    // The installed `version` is always strict semver (three parts).
    if want_major > 0 {
        if have.major != want_major {
            return false;
        }
        return (have.minor, have.patch) >= (want_minor, want_patch);
    }
    if want_minor > 0 {
        return have.major == 0 && have.minor == want_minor && have.patch >= want_patch;
    }
    // ^0.0.x admits exactly that patch.
    have.major == 0 && have.minor == 0 && have.patch == want_patch
}

/// Parse the numeric base of a caret requirement, padding partial versions
/// (`1` -> `1.0.0`, `0.1` -> `0.1.0`). Rejects prerelease/build suffixes in
/// the requirement (plugin `[components]` uses plain caret ranges).
fn parse_caret_base(bare: &str) -> Option<(u32, u32, u32)> {
    let bare = bare.trim();
    if bare.is_empty()
        || bare.contains('-')
        || bare.contains('+')
        || bare.bytes().any(|b| !(b.is_ascii_digit() || b == b'.'))
    {
        // Fall through to strict parsing for exotic inputs (which then
        // fails closed); this keeps `^0.0.1-alpha` rejected.
        let parsed = bitty_package::Version::parse(bare).ok()?;
        if parsed.prerelease.is_some() || parsed.build.is_some() {
            return None;
        }
        return Some((parsed.major, parsed.minor, parsed.patch));
    }
    let parts: Vec<&str> = bare.split('.').collect();
    if parts.len() > 3 || parts.is_empty() {
        return None;
    }
    let mut nums = [0u32; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 10 {
            return None;
        }
        nums[index] = part.parse::<u32>().ok()?;
    }
    Some((nums[0], nums[1], nums[2]))
}

fn list_component_names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if validate_component_name(&name).is_ok() {
            names.push(name);
        }
    }
    names
}

fn list_versions(root: &Path, name: &str, source: ComponentSource) -> Vec<InstalledVersion> {
    let component_dir = root.join(name);
    let Ok(entries) = std::fs::read_dir(&component_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        // Bounded plain-file-name check mirrors the resolver (no separators,
        // no whitespace/controls); strict semver is enforced by parsing the
        // descriptor below.
        if version.is_empty() || version == "." || version == ".." {
            continue;
        }
        let version_dir = component_dir.join(&version);
        let descriptor_path = version_dir.join(COMPONENT_DESCRIPTOR_FILE);
        let Ok(bytes) = std::fs::read(&descriptor_path) else {
            continue;
        };
        if bytes.len() > COMPONENT_DESCRIPTOR_MAX_BYTES {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(descriptor) = ComponentDescriptor::parse(text) else {
            continue;
        };
        if descriptor.name != name || descriptor.version != version {
            continue;
        }
        let executable = super::descriptor::executable_file_name(&descriptor.executable);
        let executable_path = version_dir.join(&executable);
        // The executable prefix check is already enforced by descriptor
        // parsing (`bitty-<name>`); skip entries whose file name escapes.
        if !executable.starts_with(COMPONENT_EXECUTABLE_PREFIX) {
            continue;
        }
        out.push(InstalledVersion {
            version,
            source,
            version_dir,
            executable_path,
            protocol_min: descriptor.protocol_min,
            protocol_max: descriptor.protocol_max,
            compatible: descriptor.supports_core_protocol(),
        });
    }
    out.sort_by_key(|entry| version_key(&entry.version));
    out
}

fn read_active_version(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    name: &str,
) -> (Option<String>, Option<ComponentSource>) {
    for (root, source) in [
        (user_root, ComponentSource::User),
        (system_root, ComponentSource::System),
    ]
    .into_iter()
    .filter_map(|(root, source)| root.map(|root| (root, source)))
    {
        let current_path = root.join(name).join(COMPONENT_CURRENT_FILE);
        let Ok(bytes) = std::fs::read(&current_path) else {
            continue;
        };
        if bytes.len() > super::COMPONENT_CURRENT_MAX_BYTES {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let trimmed = text.trim_end_matches(['\n', '\r']).trim();
        if trimmed.is_empty() {
            continue;
        }
        // User wins on collision: the first non-empty pointer (user tier
        // first) is authoritative, even when it names a version that is no
        // longer installed (surfaced by `list` as active-but-missing).
        return (Some(trimmed.to_owned()), Some(source));
    }
    (None, None)
}

fn version_key(version: &str) -> (u32, u32, u32, u8, String) {
    match bitty_package::Version::parse(version) {
        Ok(parsed) => (
            parsed.major,
            parsed.minor,
            parsed.patch,
            u8::from(parsed.prerelease.is_some()),
            parsed.prerelease.clone().unwrap_or_default(),
        ),
        Err(_) => (u32::MAX, u32::MAX, u32::MAX, u8::MAX, version.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::components_root_for;
    use crate::component::data_home_for;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-inventory-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn install_version(root: &Path, name: &str, version: &str, protocol: &str) -> PathBuf {
        let version_dir = root.join(name).join(version);
        std::fs::create_dir_all(&version_dir).expect("version dir");
        let executable_name = format!("{COMPONENT_EXECUTABLE_PREFIX}{name}");
        let file_name = super::super::descriptor::executable_file_name(&executable_name);
        let executable_path = version_dir.join(&file_name);
        std::fs::write(&executable_path, format!("{name}-{version}")).expect("executable");
        let bytes = std::fs::read(&executable_path).expect("read");
        let digest = bitty_package::integrity::sha256_hex(&bytes);
        let text = format!(
            "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = {protocol}\nexecutable = \"{executable_name}\"\nsha256 = \"{digest}\"\n"
        );
        std::fs::write(version_dir.join(COMPONENT_DESCRIPTOR_FILE), text).expect("descriptor");
        executable_path
    }

    fn set_current(root: &Path, name: &str, version: &str) {
        std::fs::create_dir_all(root.join(name)).expect("component dir");
        std::fs::write(
            root.join(name).join(COMPONENT_CURRENT_FILE),
            format!("{version}\n"),
        )
        .expect("current");
    }

    #[test]
    fn system_default_is_a_portable_contract_path() {
        let root = system_components_root_for(None).expect("default");
        assert_eq!(
            root,
            PathBuf::from(SYSTEM_COMPONENTS_DIR_DEFAULT),
            "system default must be the contract path"
        );
    }

    #[test]
    fn system_override_wins_and_empty_falls_back() {
        assert_eq!(
            system_components_root_for(Some("override-system")),
            Some(PathBuf::from("override-system"))
        );
        assert_eq!(
            system_components_root_for(Some("  ")),
            Some(PathBuf::from(SYSTEM_COMPONENTS_DIR_DEFAULT))
        );
    }

    #[test]
    fn user_root_beats_system_on_collision() {
        let base = scratch("override");
        let user = base.join("user");
        let system = base.join("system");
        install_version(&user, "net", "0.0.1", "[1, 1]");
        install_version(&system, "net", "0.0.1", "[1, 1]");
        set_current(&user, "net", "0.0.1");
        set_current(&system, "net", "0.0.1");
        std::fs::write(
            user.join("net")
                .join("0.0.1")
                .join(crate::component::executable_file_name("bitty-net")),
            "user-bytes",
        )
        .expect("overwrite user executable");
        // Recompute the user digest so the user tier stays valid and wins.
        let bytes = std::fs::read(
            user.join("net")
                .join("0.0.1")
                .join(crate::component::executable_file_name("bitty-net")),
        )
        .expect("read");
        let digest = bitty_package::integrity::sha256_hex(&bytes);
        let text = format!(
            "[component]\nname = \"net\"\nversion = \"0.0.1\"\nprotocol = [1, 1]\nexecutable = \"bitty-net\"\nsha256 = \"{digest}\"\n"
        );
        std::fs::write(
            user.join("net")
                .join("0.0.1")
                .join(COMPONENT_DESCRIPTOR_FILE),
            text,
        )
        .expect("descriptor");

        let found = resolve_search(Some(&user), Some(&system), "net").expect("resolve");
        assert_eq!(found.source, ComponentSource::User);
        assert!(
            found.resolved.executable_path.starts_with(&user),
            "user tier must win: {}",
            found.resolved.executable_path.display()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_user_falls_through_to_system() {
        let base = scratch("fallthrough");
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(&user).expect("user root");
        install_version(&system, "net", "0.0.1", "[1, 1]");
        set_current(&system, "net", "0.0.1");

        let found = resolve_search(Some(&user), Some(&system), "net").expect("resolve");
        assert_eq!(found.source, ComponentSource::System);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tampered_user_does_not_fall_back_to_system() {
        let base = scratch("tamper");
        let user = base.join("user");
        let system = base.join("system");
        install_version(&user, "net", "0.0.1", "[1, 1]");
        install_version(&system, "net", "0.0.1", "[1, 1]");
        set_current(&user, "net", "0.0.1");
        set_current(&system, "net", "0.0.1");
        // Tamper without updating the descriptor digest.
        std::fs::write(
            user.join("net")
                .join("0.0.1")
                .join(crate::component::executable_file_name("bitty-net")),
            "tampered",
        )
        .expect("tamper");

        let error = resolve_search(Some(&user), Some(&system), "net").expect_err("must fail");
        assert!(
            matches!(error, ResolveError::DigestMismatch { .. }),
            "tampered user install must fail closed, not fall back: {error:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn abi_mismatch_fails_closed() {
        let base = scratch("abi");
        let user = base.join("user");
        install_version(&user, "net", "0.0.1", "[99, 99]");
        set_current(&user, "net", "0.0.1");
        let error = resolve_search(Some(&user), None, "net").expect_err("must fail");
        assert!(
            matches!(error, ResolveError::IncompatibleProtocol { .. }),
            "protocol mismatch must deny activation: {error:?}"
        );
        let hint = incompatible_component_hint("net", 99, 99, crate::component::PROTOCOL_VERSION);
        assert!(hint.contains("bitty component add"), "{hint}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_component_reports_actionable_hint() {
        let base = scratch("missing");
        let user = base.join("user");
        std::fs::create_dir_all(&user).expect("user root");
        let error = resolve_search(Some(&user), None, "net").expect_err("missing");
        assert!(
            matches!(error, ResolveError::NotInstalled { .. }),
            "{error:?}"
        );
        let hint = missing_component_hint("net");
        assert!(hint.contains("bitty component add"), "{hint}");
        assert!(hint.contains("local"), "{hint}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn no_roots_report_no_root() {
        let error = resolve_search(None, None, "net").expect_err("no roots");
        assert_eq!(error, ResolveError::NoRoot);
    }

    #[test]
    fn discover_merges_tiers_with_user_priority() {
        let base = scratch("discover");
        let user = base.join("user");
        let system = base.join("system");
        install_version(&user, "net", "0.0.2", "[1, 1]");
        install_version(&system, "net", "0.0.1", "[1, 1]");
        install_version(&system, "net", "0.0.2", "[1, 1]");
        install_version(&system, "ai", "0.0.1", "[99, 99]");
        set_current(&user, "net", "0.0.2");
        set_current(&system, "net", "0.0.1");
        set_current(&system, "ai", "0.0.1");

        let all = discover_components(Some(&user), Some(&system));
        assert_eq!(all.len(), 2, "{all:?}");
        let net = all.iter().find(|entry| entry.name == "net").expect("net");
        assert_eq!(net.active_version.as_deref(), Some("0.0.2"));
        assert_eq!(net.active_source, Some(ComponentSource::User));
        // Same version in both tiers: user shadows system, so 0.0.2 appears once.
        assert_eq!(net.versions.len(), 2, "{net:?}");
        assert!(
            net.versions
                .iter()
                .find(|entry| entry.version == "0.0.2")
                .is_some_and(|entry| entry.source == ComponentSource::User),
            "{net:?}"
        );
        let ai = all.iter().find(|entry| entry.name == "ai").expect("ai");
        assert_eq!(ai.active_source, Some(ComponentSource::System));
        assert!(
            !ai.versions[0].compatible,
            "protocol [99,99] must be flagged incompatible"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn caret_requirements_follow_cargo_semantics() {
        assert!(version_satisfies_caret("0.0.1", "^0.0.1"));
        assert!(!version_satisfies_caret("0.0.2", "^0.0.1"));
        assert!(version_satisfies_caret("0.1.0", "^0.1"));
        assert!(version_satisfies_caret("0.1.9", "^0.1"));
        assert!(!version_satisfies_caret("0.2.0", "^0.1"));
        assert!(version_satisfies_caret("1.2.3", "^1.2.3"));
        assert!(version_satisfies_caret("1.9.0", "^1.2.3"));
        assert!(!version_satisfies_caret("2.0.0", "^1.2.3"));
        assert!(!version_satisfies_caret("0.0.1", "not-a-version"));
        assert!(!version_satisfies_caret("not-a-version", "^0.0.1"));
    }

    #[test]
    fn search_roots_derive_from_xdg_and_overrides() {
        let data = Path::new("data-home");
        let (user, system) =
            component_search_roots_for(Some("override-user"), Some(data), Some("override-system"));
        assert_eq!(user, Some(PathBuf::from("override-user")));
        assert_eq!(system, Some(PathBuf::from("override-system")));

        let home = data_home_for(Some("xdg-home"), None).expect("data home");
        let (user, _) = component_search_roots_for(None, Some(&home), None);
        assert_eq!(
            user,
            Some(components_root_for(None, Some(&home)).expect("user root"))
        );
    }
}
