//! Gap B: XDG plugin source resolution, staging, and fail-closed integrity.
//!
//! The plugin store lives under `$XDG_DATA_HOME/bitty/plugins/` with the
//! ratified layout (RFC B.2):
//!
//! ```text
//! packages/<plugin-id>/<version>/bitty-plugin.toml   # stored manifest body
//! packages/<plugin-id>/<version>/lua/                # require module root
//! current.json                                       # atomic active pointer
//! ```
//!
//! `current.json` is a bounded, versioned index mapping each plugin id to its
//! resolved [`PluginRecord`] (RFC B.4). It is replaced by write-temp-then-rename
//! so a reader sees one whole revision or the previous one, never a partial
//! write. Loading is read-only and never touches the network.
//!
//! Resolution re-verifies `manifest_hash` and `content_digest` against the
//! record before the package joins activation; a missing body, mismatch, or
//! path escape is typed and fail-closed (RFC B.3). `local-path` development
//! records are read-only, re-digested, and visibly unverified (RFC B.5).

use std::path::{Component, Path, PathBuf};

use bitty_lua::LuaValue;
use bitty_plugin_host::manifest::PluginId;

use super::manifest_toml::parse_manifest;
use super::store::{encode_json, parse_json};
use super::{
    PLUGIN_MANIFEST_MAX_BYTES, PLUGIN_MODULE_MAX_FILES, PLUGIN_MODULE_PATH_MAX_BYTES,
    PLUGIN_MODULE_TREE_MAX_BYTES, PluginPackage, PluginRuntimeError, SourceClass,
};

/// Fixed name of the atomic active pointer.
pub const CURRENT_POINTER_FILE: &str = "current.json";
/// Schema version of the `current.json` index.
pub const PLUGIN_INDEX_STATE_VERSION: i64 = 1;
/// Maximum bytes accepted for the whole index (bounded untrusted read).
///
/// Reuses the manifest body ceiling: the index holds only locators and digests,
/// so it can never legitimately approach this size.
pub const PLUGIN_INDEX_MAX_BYTES: usize = PLUGIN_MANIFEST_MAX_BYTES;
/// SHA-256 hex digest length.
const HEX_DIGEST_LEN: usize = 64;
/// Native in-process artifacts that must never appear in a module tree.
const NATIVE_ARTIFACT_EXTENSIONS: [&str; 4] = ["so", "dll", "dylib", "node"];

/// One resolved active source record (RFC B.4).
///
/// This extends the shipped managed-manifest record beyond
/// `{source, manifest_hash, enabled, granted}` with the resolved locator the
/// runtime needs to find and verify a non-bundled package: the source class,
/// owner-qualified identity, version, store-relative (or canonical
/// development) root, and the installed-tree `content_digest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    /// Closed source class (`bundled`, `registry`, `git`, `local-path`).
    pub source_class: SourceClass,
    /// Owner-qualified stable plugin identity.
    pub plugin_id: String,
    /// Resolved plugin version; must match the stored manifest body.
    pub version: String,
    /// Store-relative package root, or the canonical absolute `local-path`
    /// development root.
    pub root: String,
    /// Consent-bound canonical manifest hash; grants stay bound to it.
    pub manifest_hash: String,
    /// Digest of the installed module tree for integrity and drift detection.
    pub content_digest: String,
    /// Desired load state.
    pub enabled: bool,
    /// Granted capability identifiers bound to `manifest_hash`.
    pub granted: Vec<String>,
}

/// Load and validate the `current.json` index.
///
/// An absent index is an empty store (no installed packages). A present index
/// that is over the byte ceiling, malformed, or carries invalid records is a
/// fail-closed integrity error.
///
/// # Errors
///
/// [`PluginRuntimeError::Integrity`] for an over-limit or malformed index;
/// [`PluginRuntimeError::Io`] for a read failure.
pub fn load_index(store_root: &Path) -> Result<Vec<PluginRecord>, PluginRuntimeError> {
    let path = store_root.join(CURRENT_POINTER_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|error| PluginRuntimeError::Io(format!("plugin index metadata: {error}")))?;
    if metadata.len() as usize > PLUGIN_INDEX_MAX_BYTES {
        return Err(integrity(
            "current.json",
            "plugin index exceeds the byte ceiling",
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| PluginRuntimeError::Io(format!("plugin index read: {error}")))?;
    parse_index(&text)
}

/// Atomically write the `current.json` index (write-temp-then-rename).
///
/// This is the staging side of the contract, used by the package manager and by
/// tests; runtime loading never calls it. Records are written in deterministic
/// plugin-id order and the encoded index is bounded before any write.
///
/// # Errors
///
/// [`PluginRuntimeError::Integrity`] when the encoded index exceeds
/// [`PLUGIN_INDEX_MAX_BYTES`]; [`PluginRuntimeError::Io`] on a write failure.
pub fn write_index(store_root: &Path, records: &[PluginRecord]) -> Result<(), PluginRuntimeError> {
    let text = encode_index(records)?;
    std::fs::create_dir_all(store_root)
        .map_err(|error| PluginRuntimeError::Io(format!("plugin store create: {error}")))?;
    let target = store_root.join(CURRENT_POINTER_FILE);
    let temp = store_root.join("current.json.tmp");
    std::fs::write(&temp, text.as_bytes())
        .map_err(|error| PluginRuntimeError::Io(format!("plugin index write: {error}")))?;
    match std::fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows cannot rename over an existing file; the fallback still
            // never leaves the temp file as the pointer.
            let _ = std::fs::remove_file(&target);
            std::fs::rename(&temp, &target)
                .map_err(|error| PluginRuntimeError::Io(format!("plugin index commit: {error}")))
        }
    }
}

/// Resolve one record into a verified [`PluginPackage`], fail closed.
///
/// For installed classes (`bundled`, `registry`, `git`) the store-relative root
/// is canonicalized inside the store and the manifest body, manifest hash, and
/// content digest are re-verified. For `local-path` the canonical absolute
/// development path is required and content drift only marks the package
/// unverified (RFC B.5), while a manifest-hash mismatch still fails closed.
///
/// # Errors
///
/// [`PluginRuntimeError::NotFound`] when the recorded body is absent;
/// [`PluginRuntimeError::Integrity`] on any hash, digest, identity, or path
/// mismatch; [`PluginRuntimeError::Manifest`]/[`ModuleTree`] for a body that
/// violates the schema or bounds.
///
/// [`ModuleTree`]: PluginRuntimeError::ModuleTree
pub fn resolve_record(
    store_root: &Path,
    record: &PluginRecord,
) -> Result<PluginPackage, PluginRuntimeError> {
    let plugin = record.plugin_id.clone();
    let (package_root, mut unverified) = match record.source_class {
        SourceClass::LocalPath => {
            let recorded = PathBuf::from(&record.root);
            if !recorded.is_absolute() {
                return Err(integrity(
                    &plugin,
                    "local-path root must be an absolute path",
                ));
            }
            let canonical =
                std::fs::canonicalize(&recorded).map_err(|error| PluginRuntimeError::NotFound {
                    plugin: plugin.clone(),
                    detail: format!("local-path root unreadable: {error}"),
                })?;
            // Never follow a changed path or a new symlink target silently: the
            // recorded root must already be the canonical development path.
            if canonical != recorded {
                return Err(integrity(
                    &plugin,
                    "local-path root is not the canonical recorded path",
                ));
            }
            (canonical, true)
        }
        _ => {
            if !is_safe_relative(&record.root) {
                return Err(integrity(
                    &plugin,
                    "installed root must be a safe store-relative path",
                ));
            }
            let store_canonical = std::fs::canonicalize(store_root).map_err(|error| {
                PluginRuntimeError::NotFound {
                    plugin: plugin.clone(),
                    detail: format!("store root unreadable: {error}"),
                }
            })?;
            let joined = store_root.join(&record.root);
            let canonical =
                std::fs::canonicalize(&joined).map_err(|error| PluginRuntimeError::NotFound {
                    plugin: plugin.clone(),
                    detail: format!("package root missing: {error}"),
                })?;
            if !canonical.starts_with(&store_canonical) {
                return Err(integrity(&plugin, "package root escapes the plugin store"));
            }
            (canonical, false)
        }
    };

    let module_root = module_root_for(&package_root);
    let manifest_path = package_root.join("bitty-plugin.toml");
    let metadata =
        std::fs::metadata(&manifest_path).map_err(|error| PluginRuntimeError::NotFound {
            plugin: plugin.clone(),
            detail: format!("manifest body missing: {error}"),
        })?;
    if metadata.len() as usize > PLUGIN_MANIFEST_MAX_BYTES {
        return Err(integrity(
            &plugin,
            "manifest body exceeds the 256 KiB ceiling",
        ));
    }
    let bytes = std::fs::read(&manifest_path)
        .map_err(|error| PluginRuntimeError::Io(format!("manifest read: {error}")))?;
    let manifest = parse_manifest(&bytes).map_err(|detail| PluginRuntimeError::Manifest {
        plugin: plugin.clone(),
        detail,
    })?;
    if manifest.id().as_str() != record.plugin_id {
        return Err(integrity(
            &plugin,
            "manifest id does not match the resolved record",
        ));
    }
    if manifest.identity.version != record.version {
        return Err(integrity(
            &plugin,
            "manifest version does not match the resolved record",
        ));
    }
    if !manifest
        .manifest_hash()
        .eq_ignore_ascii_case(&record.manifest_hash)
    {
        return Err(integrity(&plugin, "manifest hash mismatch"));
    }

    let id = manifest.id().clone();
    let digest = scan_module_tree(id.as_str(), &module_root)?;
    if !digest.eq_ignore_ascii_case(&record.content_digest) {
        if record.source_class == SourceClass::LocalPath {
            // Drift is reported, not hidden: the package stays unverified until
            // it is re-resolved (RFC B.5 rule 3).
            unverified = true;
        } else {
            return Err(integrity(&plugin, "content digest mismatch"));
        }
    }

    Ok(PluginPackage {
        manifest,
        module_root,
        source_class: record.source_class,
        unverified,
    })
}

/// Compute the canonical content digest of a package's module tree.
///
/// The scheme is deterministic across platforms: files are sorted by their
/// normalized `/`-separated relative path and hashed as
/// `path || 0x00 || bytes || 0x0a`. It matches
/// `bitty-package::source::digest_local_content`, so a `local-path` record
/// written by the package manager verifies here.
///
/// # Errors
///
/// [`PluginRuntimeError::ModuleTree`] when the tree violates the file, byte, or
/// path bounds or contains a native artifact or escape.
pub fn content_digest(package_root: &Path) -> Result<String, PluginRuntimeError> {
    scan_module_tree("<content-digest>", &module_root_for(package_root))
}

/// Walk a module tree, enforce the RFC bounds, and return its content digest.
///
/// Rejects native artifacts (`.so`, `.dll`, `.dylib`, `.node`), paths over
/// [`PLUGIN_MODULE_PATH_MAX_BYTES`], trees over [`PLUGIN_MODULE_MAX_FILES`] or
/// [`PLUGIN_MODULE_TREE_MAX_BYTES`], and any file whose canonical path escapes
/// the canonical root (traversal or symlink).
pub(crate) fn scan_module_tree(plugin: &str, root: &Path) -> Result<String, PluginRuntimeError> {
    let canonical =
        std::fs::canonicalize(root).map_err(|error| PluginRuntimeError::ModuleTree {
            plugin: plugin.to_string(),
            detail: format!("module root unreadable: {error}"),
        })?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![canonical.clone()];
    while let Some(directory) = stack.pop() {
        let listing =
            std::fs::read_dir(&directory).map_err(|error| PluginRuntimeError::ModuleTree {
                plugin: plugin.to_string(),
                detail: format!("module directory unreadable: {error}"),
            })?;
        for entry in listing {
            let entry = entry.map_err(|error| PluginRuntimeError::ModuleTree {
                plugin: plugin.to_string(),
                detail: format!("module entry unreadable: {error}"),
            })?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|error| PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: format!("module metadata unreadable: {error}"),
                })?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            files += 1;
            if files > PLUGIN_MODULE_MAX_FILES {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: "module tree exceeds the 4096-file ceiling".to_string(),
                });
            }
            bytes = bytes.saturating_add(metadata.len());
            if bytes > PLUGIN_MODULE_TREE_MAX_BYTES as u64 {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: "module tree exceeds the 16 MiB ceiling".to_string(),
                });
            }
            if path.as_os_str().len() > PLUGIN_MODULE_PATH_MAX_BYTES {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: "module path exceeds the 1024-byte ceiling".to_string(),
                });
            }
            if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
                if NATIVE_ARTIFACT_EXTENSIONS.contains(&extension) {
                    return Err(PluginRuntimeError::ModuleTree {
                        plugin: plugin.to_string(),
                        detail: "native artifacts are not loadable modules".to_string(),
                    });
                }
            }
            let resolved =
                std::fs::canonicalize(&path).map_err(|error| PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: format!("module path unresolved: {error}"),
                })?;
            if !resolved.starts_with(&canonical) {
                return Err(PluginRuntimeError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: "module path escapes the plugin root".to_string(),
                });
            }
            let data = std::fs::read(&path).map_err(|error| PluginRuntimeError::ModuleTree {
                plugin: plugin.to_string(),
                detail: format!("module file unreadable: {error}"),
            })?;
            entries.push((normalized_relative(&canonical, &path), data));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut buffer = Vec::new();
    for (relative, data) in &entries {
        buffer.extend_from_slice(relative.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(data);
        buffer.push(b'\n');
    }
    Ok(bitty_ipc::frame_digest::sha256_hex(&buffer))
}

/// The module root a package is loaded from: `<root>/lua` when present, else
/// the package root itself.
fn module_root_for(package_root: &Path) -> PathBuf {
    let lua = package_root.join("lua");
    if lua.is_dir() {
        lua
    } else {
        package_root.to_path_buf()
    }
}

/// Normalized `/`-separated path relative to `root`.
fn normalized_relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `root` is a non-empty relative path with only normal components
/// (no `..`, `.`, or root), so joining it cannot escape the store.
fn is_safe_relative(root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let path = Path::new(root);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_hex_digest(candidate: &str) -> bool {
    candidate.len() == HEX_DIGEST_LEN && candidate.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn integrity(plugin: &str, detail: impl Into<String>) -> PluginRuntimeError {
    PluginRuntimeError::Integrity {
        plugin: plugin.to_string(),
        detail: detail.into(),
    }
}

fn string_value(value: &str) -> LuaValue {
    LuaValue::String(value.to_string())
}

fn as_str(value: &LuaValue) -> Option<&str> {
    match value {
        LuaValue::String(text) => Some(text),
        _ => None,
    }
}

fn as_integer(value: &LuaValue) -> Option<i64> {
    match value {
        LuaValue::Integer(integer) => Some(*integer),
        _ => None,
    }
}

fn as_bool(value: &LuaValue) -> Option<bool> {
    match value {
        LuaValue::Bool(flag) => Some(*flag),
        _ => None,
    }
}

fn as_string_array(value: &LuaValue) -> Option<Vec<String>> {
    match value {
        LuaValue::Table(pairs) => pairs
            .iter()
            .map(|(_, child)| as_str(child).map(str::to_string))
            .collect(),
        _ => None,
    }
}

fn parse_index(text: &str) -> Result<Vec<PluginRecord>, PluginRuntimeError> {
    let value = parse_json(text)
        .map_err(|detail| integrity("current.json", format!("index parse: {detail}")))?;
    let version = value
        .get("state_version")
        .and_then(as_integer)
        .ok_or_else(|| integrity("current.json", "index is missing `state_version`"))?;
    if version != PLUGIN_INDEX_STATE_VERSION {
        return Err(integrity(
            "current.json",
            "unsupported plugin index state_version",
        ));
    }
    let plugins = value
        .get("plugins")
        .ok_or_else(|| integrity("current.json", "index is missing `plugins`"))?;
    let LuaValue::Table(pairs) = plugins else {
        return Err(integrity("current.json", "`plugins` must be an object"));
    };
    let mut records = Vec::with_capacity(pairs.len());
    for (key, entry) in pairs {
        let Some(plugin_id) = as_str(key) else {
            return Err(integrity(
                "current.json",
                "`plugins` keys must be plugin ids",
            ));
        };
        records.push(parse_record(plugin_id, entry)?);
    }
    records.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    Ok(records)
}

fn parse_record(map_key: &str, entry: &LuaValue) -> Result<PluginRecord, PluginRuntimeError> {
    let source_label = entry
        .get("source_class")
        .and_then(as_str)
        .ok_or_else(|| integrity(map_key, "record is missing `source_class`"))?;
    let source_class = SourceClass::parse(source_label)
        .ok_or_else(|| integrity(map_key, "record has an unknown `source_class`"))?;
    let plugin_id = require_string(entry, map_key, "plugin_id")?;
    if plugin_id != map_key {
        return Err(integrity(
            map_key,
            "record key does not match its `plugin_id`",
        ));
    }
    PluginId::new(&plugin_id)
        .map_err(|error| integrity(map_key, format!("invalid plugin_id: {error}")))?;
    let version = require_string(entry, map_key, "version")?;
    let root = require_string(entry, map_key, "root")?;
    if root.len() > PLUGIN_MODULE_PATH_MAX_BYTES {
        return Err(integrity(
            map_key,
            "record root exceeds the 1024-byte ceiling",
        ));
    }
    let manifest_hash = require_string(entry, map_key, "manifest_hash")?;
    if !is_hex_digest(&manifest_hash) {
        return Err(integrity(
            map_key,
            "manifest_hash must be 64 hex characters",
        ));
    }
    let content_digest = require_string(entry, map_key, "content_digest")?;
    if !is_hex_digest(&content_digest) {
        return Err(integrity(
            map_key,
            "content_digest must be 64 hex characters",
        ));
    }
    let enabled = entry
        .get("enabled")
        .and_then(as_bool)
        .ok_or_else(|| integrity(map_key, "record is missing boolean `enabled`"))?;
    let granted = entry
        .get("granted")
        .and_then(as_string_array)
        .ok_or_else(|| integrity(map_key, "record `granted` must be an array of strings"))?;
    Ok(PluginRecord {
        source_class,
        plugin_id,
        version,
        root,
        manifest_hash,
        content_digest,
        enabled,
        granted,
    })
}

fn require_string(
    entry: &LuaValue,
    plugin: &str,
    field: &str,
) -> Result<String, PluginRuntimeError> {
    entry
        .get(field)
        .and_then(as_str)
        .map(str::to_string)
        .ok_or_else(|| integrity(plugin, format!("record is missing string `{field}`")))
}

fn encode_index(records: &[PluginRecord]) -> Result<String, PluginRuntimeError> {
    let mut sorted: Vec<&PluginRecord> = records.iter().collect();
    sorted.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    let plugins: Vec<(LuaValue, LuaValue)> = sorted
        .iter()
        .map(|record| (string_value(&record.plugin_id), record_value(record)))
        .collect();
    let root = LuaValue::Table(vec![
        (
            string_value("state_version"),
            LuaValue::Integer(PLUGIN_INDEX_STATE_VERSION),
        ),
        (string_value("plugins"), LuaValue::Table(plugins)),
    ]);
    let text = encode_json(&root);
    if text.len() > PLUGIN_INDEX_MAX_BYTES {
        return Err(integrity(
            "current.json",
            "encoded plugin index exceeds the byte ceiling",
        ));
    }
    Ok(text)
}

fn record_value(record: &PluginRecord) -> LuaValue {
    LuaValue::Table(vec![
        (
            string_value("source_class"),
            string_value(record.source_class.as_str()),
        ),
        (string_value("plugin_id"), string_value(&record.plugin_id)),
        (string_value("version"), string_value(&record.version)),
        (string_value("root"), string_value(&record.root)),
        (
            string_value("manifest_hash"),
            string_value(&record.manifest_hash),
        ),
        (
            string_value("content_digest"),
            string_value(&record.content_digest),
        ),
        (string_value("enabled"), LuaValue::Bool(record.enabled)),
        (
            string_value("granted"),
            LuaValue::array(
                record
                    .granted
                    .iter()
                    .cloned()
                    .map(LuaValue::String)
                    .collect(),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> PluginRecord {
        PluginRecord {
            source_class: SourceClass::Registry,
            plugin_id: id.to_string(),
            version: "1.0.0".to_string(),
            root: format!("packages/{id}/1.0.0"),
            manifest_hash: "a".repeat(64),
            content_digest: "b".repeat(64),
            enabled: true,
            granted: vec!["terminal.semantic-read".to_string()],
        }
    }

    #[test]
    fn index_round_trips_deterministically() {
        let first = record("bitty.beta");
        let second = record("bitty.alpha");
        let encoded = encode_index(&[first.clone(), second.clone()]).expect("encode");
        // Deterministic plugin-id ordering regardless of input order.
        let decoded = parse_index(&encoded).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].plugin_id, "bitty.alpha");
        assert_eq!(decoded[1].plugin_id, "bitty.beta");
        assert_eq!(decoded[0], second);
        assert_eq!(decoded[1], first);
        // Repeated encoding is byte-stable.
        assert_eq!(encode_index(&decoded).expect("re-encode"), encoded);
    }

    #[test]
    fn index_rejects_malformed_shapes() {
        assert!(parse_index("{\"state_version\":2,\"plugins\":{}}").is_err());
        assert!(parse_index("{\"state_version\":1}").is_err());
        assert!(parse_index("{\"state_version\":1,\"plugins\":\"x\"}").is_err());
        assert!(parse_index("not json").is_err());
    }

    #[test]
    fn index_rejects_bad_digests() {
        let mut bad = record("bitty.bad");
        bad.manifest_hash = "zz".to_string();
        let encoded = encode_index(&[bad]).expect("encode");
        assert!(parse_index(&encoded).is_err());
    }

    #[test]
    fn relative_root_guard() {
        assert!(is_safe_relative("packages/bitty.a/1.0.0"));
        assert!(!is_safe_relative(""));
        assert!(!is_safe_relative("../escape"));
        assert!(!is_safe_relative("packages/../../escape"));
        assert!(!is_safe_relative("/abs/path"));
        assert!(!is_safe_relative("./packages/bitty.a"));
    }
}
