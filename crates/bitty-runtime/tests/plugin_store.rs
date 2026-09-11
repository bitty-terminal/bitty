//! Gap B store tests: atomic pointer staging, fail-closed manifest/content
//! re-verification, native artifact rejection, local-path unverified marking,
//! bounds, and provenance-ordered discovery.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRecord, PluginRuntime, PluginRuntimeConfig, SettingsSource,
    SnapshotSource, SourceClass, content_digest, load_index, write_index,
};

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-plugin-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("canonical path")
}

fn manifest_body(id: &str, version: &str) -> String {
    format!(
        r#"[plugin]
id = "{id}"
name = "Store Test"
version = "{version}"
description = "store test package"

[compat]
plugin-api = "^1.0"

[lazy]
commands = ["{id}:summary"]
events = []
"#
    )
}

/// Write `<package_root>/bitty-plugin.toml` and `<package_root>/lua/init.lua`.
fn write_package(package_root: &Path, id: &str, version: &str, init_src: &str) {
    std::fs::create_dir_all(package_root.join("lua")).expect("package dirs");
    std::fs::write(
        package_root.join("bitty-plugin.toml"),
        manifest_body(id, version),
    )
    .expect("manifest");
    std::fs::write(package_root.join("lua/init.lua"), init_src).expect("init");
}

fn manifest_hash(package_root: &Path) -> String {
    let bytes = std::fs::read(package_root.join("bitty-plugin.toml")).expect("manifest bytes");
    bitty_runtime::plugin_runtime::manifest_toml::parse_manifest(&bytes)
        .expect("manifest parses")
        .manifest_hash()
}

/// Build an installed-store record pointing at an on-disk package.
fn installed_record(relative_root: &str, package_root: &Path) -> PluginRecord {
    PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: "bitty-featured.store".to_string(),
        version: "0.1.0".to_string(),
        root: relative_root.to_string(),
        manifest_hash: manifest_hash(package_root),
        content_digest: content_digest(package_root).expect("digest"),
        enabled: true,
        granted: Vec::new(),
    }
}

fn runtime(store_root: Option<PathBuf>, safe_mode: bool, tag: &str) -> PluginRuntime {
    let data = temp_dir(&format!("state-{tag}"));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: Some(data),
        store_root,
        bundled_roots: Vec::new(),
        third_party_roots: Vec::new(),
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([(
            "version",
            LuaValue::Integer(1),
        )]))),
    })
}

const INIT: &str = r#"
bitty.commands.register({
  id = "summary",
  title = "Store summary",
  run = function(_args) return "ok" end,
})
return {}
"#;

#[test]
fn write_index_is_atomic_and_round_trips() {
    let store = temp_dir("roundtrip");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let record = installed_record("packages/bitty-featured.store/0.1.0", &package);

    write_index(&store, std::slice::from_ref(&record)).expect("write index");
    assert!(
        store.join("current.json").is_file(),
        "pointer must exist after commit"
    );
    assert!(
        !store.join("current.json.tmp").exists(),
        "no temp file is left behind"
    );
    let loaded = load_index(&store).expect("load index");
    assert_eq!(loaded, vec![record.clone()]);

    // A second commit atomically replaces the pointer.
    let mut updated = record.clone();
    updated.version = "0.1.0".to_string();
    updated.granted = vec!["terminal.semantic-read".to_string()];
    write_index(&store, std::slice::from_ref(&updated)).expect("rewrite index");
    assert!(!store.join("current.json.tmp").exists());
    assert_eq!(load_index(&store).expect("reload"), vec![updated]);

    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn installed_plugin_resolves_and_activates_from_pointer() {
    let store = temp_dir("installed");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let record = installed_record("packages/bitty-featured.store/0.1.0", &package);
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = runtime(Some(store.clone()), false, "installed");
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let id = PluginId::new("bitty-featured.store").expect("id");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Unloaded));

    let report = rt.activate(&id).expect("activate");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.source_class, SourceClass::Registry);
    assert!(!report.unverified, "installed packages are verified");
    assert_eq!(report.commands, 1);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn manifest_hash_mismatch_fails_closed() {
    let store = temp_dir("hash");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let mut record = installed_record("packages/bitty-featured.store/0.1.0", &package);
    record.manifest_hash = "f".repeat(64);
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = runtime(Some(store.clone()), false, "hash");
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_err(), "hash mismatch must be reported");
    assert_eq!(rt.package_count(), 0, "the package must not be registered");
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn content_digest_mismatch_fails_closed() {
    let store = temp_dir("digest");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let record = installed_record("packages/bitty-featured.store/0.1.0", &package);
    write_index(&store, std::slice::from_ref(&record)).expect("write index");
    // Tamper with the module tree after the digest was recorded.
    std::fs::write(package.join("lua/init.lua"), "return { tampered = true }").expect("tamper");

    let mut rt = runtime(Some(store.clone()), false, "digest");
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_err(), "digest mismatch must be reported");
    assert_eq!(rt.package_count(), 0);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn native_artifact_is_rejected() {
    let store = temp_dir("native");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    std::fs::write(package.join("lua/evil.so"), b"\x7fELF").expect("native artifact");
    let record = PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: "bitty-featured.store".to_string(),
        version: "0.1.0".to_string(),
        root: "packages/bitty-featured.store/0.1.0".to_string(),
        manifest_hash: manifest_hash(&package),
        // Any well-formed digest; the native check fires first and fails closed.
        content_digest: "a".repeat(64),
        enabled: true,
        granted: Vec::new(),
    };
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = runtime(Some(store.clone()), false, "native");
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_err(),
        "native artifacts are never loadable modules"
    );
    assert_eq!(rt.package_count(), 0);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn local_path_dev_is_unverified_and_drift_is_tolerated() {
    let dev = temp_dir("local-path");
    let package = dev.join("package");
    write_package(&package, "bitty-featured.dev", "0.1.0", INIT);
    let record = PluginRecord {
        source_class: SourceClass::LocalPath,
        plugin_id: "bitty-featured.dev".to_string(),
        version: "0.1.0".to_string(),
        root: canonical(&package).display().to_string(),
        manifest_hash: manifest_hash(&package),
        content_digest: content_digest(&package).expect("digest"),
        enabled: true,
        granted: Vec::new(),
    };

    let store = temp_dir("local-path-store");
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = runtime(Some(store.clone()), false, "localpath");
    let id = PluginId::new("bitty-featured.dev").expect("id");
    assert_eq!(rt.discover().len(), 1);
    let report = rt.activate(&id).expect("local-path activates read-only");
    assert_eq!(report.source_class, SourceClass::LocalPath);
    assert!(report.unverified, "local-path is never treated as verified");

    // Drift after resolution: still loadable, still visibly unverified.
    rt.dispose(&id).expect("dispose");
    std::fs::write(package.join("lua/init.lua"), "return { drifted = true }").expect("drift write");
    let report = rt
        .activate(&id)
        .expect("drift does not fail closed for local-path");
    assert!(report.unverified, "drift keeps the package unverified");

    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&dev);
}

#[test]
fn local_path_non_canonical_root_is_rejected() {
    let dev = temp_dir("local-path-noncanonical");
    let package = dev.join("package");
    write_package(&package, "bitty-featured.dev", "0.1.0", INIT);
    let mut record = PluginRecord {
        source_class: SourceClass::LocalPath,
        plugin_id: "bitty-featured.dev".to_string(),
        version: "0.1.0".to_string(),
        root: canonical(&package).display().to_string(),
        manifest_hash: manifest_hash(&package),
        content_digest: content_digest(&package).expect("digest"),
        enabled: true,
        granted: Vec::new(),
    };
    // A non-canonical spelling must never be followed silently. `lua/..`
    // canonicalizes back to the package root, so the recorded spelling differs
    // from the canonical path the loader resolves.
    record.root = format!("{}/lua/..", record.root);

    let store = temp_dir("local-path-noncanonical-store");
    write_index(&store, std::slice::from_ref(&record)).expect("write index");
    let mut rt = runtime(Some(store.clone()), false, "noncanonical");
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_err(),
        "non-canonical local-path root must fail closed"
    );
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&dev);
}

#[test]
fn module_tree_byte_ceiling_is_enforced() {
    // One file above the 16 MiB aggregate ceiling must fail closed. The
    // 1024-byte path ceiling is covered portably by the `resolution` unit test
    // (`path_ceiling_is_enforced_at_ratified_bound`): macOS caps `PATH_MAX` at
    // 1024, so an over-limit path cannot be materialized on disk there.
    let store = temp_dir("bound-bytes");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", "return {}");
    let oversized = vec![b'a'; 17 * 1024 * 1024];
    std::fs::write(package.join("lua/blob.lua"), &oversized).expect("oversized file");
    assert!(
        content_digest(&package).is_err(),
        "tree over 16 MiB must be rejected"
    );
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn discovery_orders_bundled_then_installed_then_dev() {
    let id = "bitty-featured.order";

    // Bundled root (winning provenance).
    let bundled = temp_dir("order-bundled");
    write_package(&bundled.join(id), id, "0.1.0", INIT);

    // Installed record for a second, store-only id.
    let store = temp_dir("order-store");
    let installed_id = "bitty-featured.installed";
    let package = store.join("packages").join(installed_id).join("0.1.0");
    write_package(&package, installed_id, "0.1.0", INIT);
    let record = PluginRecord {
        source_class: SourceClass::Git,
        plugin_id: installed_id.to_string(),
        version: "0.1.0".to_string(),
        root: format!("packages/{installed_id}/0.1.0"),
        manifest_hash: manifest_hash(&package),
        content_digest: content_digest(&package).expect("digest"),
        enabled: true,
        granted: Vec::new(),
    };
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    // Dev root, third id.
    let dev = temp_dir("order-dev");
    let dev_id = "bitty-featured.devonly";
    write_package(&dev.join(dev_id), dev_id, "0.1.0", INIT);

    let data = temp_dir("order-state");
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: Some(store.clone()),
        bundled_roots: vec![bundled.clone()],
        third_party_roots: vec![dev.clone()],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::Nil)),
    });
    rt.discover();

    let order: Vec<String> = rt
        .discovered_ids()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        order,
        vec![id.to_string(), installed_id.to_string(), dev_id.to_string()]
    );

    // The colliding id keeps bundled provenance; the dev-only id is local-path.
    let id = PluginId::new(id).expect("id");
    assert_eq!(
        rt.activate(&id).expect("bundled activate").source_class,
        SourceClass::Bundled
    );
    let dev_id = PluginId::new(dev_id).expect("id");
    let report = rt.activate(&dev_id).expect("dev activate");
    assert_eq!(report.source_class, SourceClass::LocalPath);
    assert!(report.unverified);

    let _ = std::fs::remove_dir_all(&bundled);
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&dev);
}

#[test]
fn safe_mode_skips_installed_store_but_loads_bundled() {
    // Installed (third-party) records are not read under `--safe`.
    let store = temp_dir("safe-store");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let record = installed_record("packages/bitty-featured.store/0.1.0", &package);
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    // A trusted bundled root still loads under `--safe`.
    let bundled = temp_dir("safe-bundled");
    let bundled_id = "bitty-featured.bundled";
    write_package(&bundled.join(bundled_id), bundled_id, "0.1.0", INIT);

    let data = temp_dir("safe-state");
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: true,
        data_dir: Some(data),
        store_root: Some(store.clone()),
        bundled_roots: vec![bundled.clone()],
        third_party_roots: Vec::new(),
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::Nil)),
    });
    rt.discover();

    let installed = PluginId::new("bitty-featured.store").expect("id");
    assert!(
        rt.state(&installed).is_none(),
        "an installed record is not read or discovered under --safe"
    );
    let bundled_plugin = PluginId::new(bundled_id).expect("id");
    let report = rt
        .activate(&bundled_plugin)
        .expect("bundled activates under --safe");
    assert!(!report.skipped_safe_mode);
    assert_eq!(report.source_class, SourceClass::Bundled);

    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&bundled);
}

#[test]
fn safe_mode_does_not_read_store_tree() {
    // A malformed index proves whether the store tree was read: `--safe` must
    // not read it (RFC A.4 rule 6 / R-009), while the default path reports the
    // integrity failure.
    let store = temp_dir("safe-no-read");
    std::fs::create_dir_all(&store).expect("store dir");
    std::fs::write(store.join("current.json"), b"not a plugin index").expect("malformed index");

    let mut safe = runtime(Some(store.clone()), true, "safe-no-read");
    let safe_results = safe.discover();
    assert!(
        safe_results.is_empty(),
        "--safe must not read the store tree: {safe_results:?}"
    );
    assert_eq!(safe.package_count(), 0);

    let mut normal = runtime(Some(store.clone()), false, "normal-no-read");
    let normal_results = normal.discover();
    assert!(
        normal_results.iter().any(|(_, result)| result.is_err()),
        "the default path reports the malformed index: {normal_results:?}"
    );

    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn bundled_record_in_store_fails_closed() {
    // A store record may not claim first-party provenance: bundled packages are
    // shipped with the application from a configured trusted root, never the
    // user-writable index.
    let store = temp_dir("bundled-record");
    let package = store.join("packages/bitty-featured.store/0.1.0");
    write_package(&package, "bitty-featured.store", "0.1.0", INIT);
    let mut record = installed_record("packages/bitty-featured.store/0.1.0", &package);
    record.source_class = SourceClass::Bundled;
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = runtime(Some(store.clone()), false, "bundled-record");
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.is_err(),
        "a self-declared bundled store record must fail closed"
    );
    assert_eq!(rt.package_count(), 0);
    let _ = std::fs::remove_dir_all(&store);
}
