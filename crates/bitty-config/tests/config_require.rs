//! The restricted configuration module loader, from the `bitty-config` side.
//!
//! These tests pin the layer-level contract (`bitty#2869`,
//! `config-lua-require-contract.md`): one root per entry, the four path
//! behaviors (startup salvage, strict `config check`/`doctor`, refused reload,
//! `--safe`), redaction, and the module counters on the salvaged result.

use bitty_config::file::{
    load_profile_layer, load_profile_layer_salvaging, load_user_layer, load_user_layer_salvaging,
    module_root_for_entry, parse_lua_config, parse_lua_config_salvaging,
    parse_lua_config_salvaging_with_root, parse_lua_config_with_root,
};
use bitty_config::{ConfigError, ConfigSource, LayerKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-config-require-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(&path, body).expect("write");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The module root for the canonical user entry of `dir`.
fn user_root(dir: &Path) -> PathBuf {
    module_root_for_entry(&dir.join("bitty/init.lua"), LayerKind::User)
}

fn user_source(path: PathBuf) -> ConfigSource {
    ConfigSource::new(LayerKind::User, Some(path.display().to_string()))
}

// ── root derivation ─────────────────────────────────────────────────────────

#[test]
fn module_root_is_the_lua_directory_of_the_config_directory() {
    let dir = TempDir::new("root-derivation");
    let cases = [
        ("bitty/init.lua", LayerKind::User, "bitty/lua"),
        ("bitty/config.lua", LayerKind::User, "bitty/lua"),
        ("custom/wherever.lua", LayerKind::User, "custom/lua"),
        ("bitty/profiles/work.lua", LayerKind::Profile, "bitty/lua"),
        ("custom/profiles/work.lua", LayerKind::Profile, "custom/lua"),
    ];
    for (entry, kind, root) in cases {
        let entry = dir.path().join(entry);
        assert_eq!(
            module_root_for_entry(&entry, kind),
            dir.path().join(root),
            "{entry:?} as {kind:?}"
        );
    }
}

// ── the loader at the layer boundary ────────────────────────────────────────

#[test]
fn a_module_graph_loads_through_the_user_layer() {
    let dir = TempDir::new("load-graph");
    let init = dir.write(
        "bitty/init.lua",
        "local opts = require('config.options')\nreturn { theme = opts.theme }\n",
    );
    dir.write(
        "bitty/lua/config/options.lua",
        "return { theme = 'bitty-dark' }",
    );
    let layer = load_user_layer(&init).expect("layer loads");
    assert_eq!(
        layer
            .plan
            .appearance
            .as_ref()
            .and_then(|a| a.theme.as_deref()),
        Some("bitty-dark")
    );
}

#[test]
fn a_profile_shares_the_root_of_the_user_file_beside_it() {
    let dir = TempDir::new("load-profile");
    dir.write(
        "bitty/lua/shared/theme.lua",
        "return { theme = 'tokyo-night' }",
    );
    let profile = dir.write(
        "bitty/profiles/work.lua",
        "return { theme = require('shared.theme').theme }\n",
    );
    let layer = load_profile_layer(&profile).expect("profile layer loads");
    assert_eq!(
        layer
            .plan
            .appearance
            .as_ref()
            .and_then(|a| a.theme.as_deref()),
        Some("tokyo-night")
    );
}

#[test]
fn no_module_root_means_no_require_at_all() {
    let _dir = TempDir::new("no-root");
    let source = ConfigSource::new(LayerKind::User, Some("init.lua"));
    // `require` is absent, so the chunk fails closed with a Lua error rather
    // than resolving anything: the in-memory parse path never gets a loader.
    let error =
        parse_lua_config("return require('anything')", &source).expect_err("no loader installed");
    assert!(matches!(error, ConfigError::Validation { .. }), "{error:?}");
    assert!(parse_lua_config_with_root("return { theme = 'bitty-dark' }", &source, None).is_ok());
}

// ── four paths ──────────────────────────────────────────────────────────────

#[test]
fn strict_and_salvaging_both_refuse_a_module_failure_as_a_whole_layer() {
    let dir = TempDir::new("layer-refusal");
    let init = dir.write("bitty/init.lua", "return { theme = require('missing') }");
    dir.write("bitty/lua/present.lua", "return {}");

    // Strict: the exact typed record, no salvage.
    let strict = load_user_layer(&init).expect_err("strict refuses");
    let diagnostic = strict.module_diagnostic().expect("typed module error");
    assert_eq!(
        diagnostic.code().as_str(),
        "E_CONFIG_MODULE_NOT_FOUND",
        "{strict}"
    );

    // Startup: module failures are whole-layer failures, not per-table drops,
    // so the salvaging loader also returns Err and the caller drops the layer.
    let salvaged = load_user_layer_salvaging(&init).expect_err("startup drops the layer");
    assert!(salvaged.module_diagnostic().is_some(), "{salvaged:?}");
    let profile = dir.write(
        "bitty/profiles/p.lua",
        "return { theme = require('missing') }",
    );
    assert!(load_profile_layer(&profile).is_err());
    assert!(load_profile_layer_salvaging(&profile).is_err());
}

#[test]
fn a_salvaged_layer_keeps_its_valid_tables_and_reports_module_stats() {
    let dir = TempDir::new("salvage-ok");
    dir.write("bitty/lua/theme.lua", "return { theme = 'tokyo-night' }");
    let source = user_source(dir.path().join("bitty/init.lua"));
    let content = "return { theme = require('theme').theme, window = { opacity = 1.5 } }";
    let salvaged =
        parse_lua_config_salvaging_with_root(content, &source, Some(&user_root(dir.path())))
            .expect("salvage");
    assert_eq!(
        salvaged
            .plan
            .appearance
            .as_ref()
            .and_then(|a| a.theme.as_deref()),
        Some("tokyo-night")
    );
    assert_eq!(salvaged.module_stats.modules, 1);
    assert!(salvaged.module_stats.module_bytes > 0);
    // The out-of-range table was dropped by the shipped table-level salvage,
    // while the module graph that fed the theme loaded whole: a module
    // failure is the only thing that drops the layer.
    assert_eq!(salvaged.diagnostics.len(), 1, "{:?}", salvaged.diagnostics);
}

#[test]
fn a_module_failure_is_reported_once_with_no_partial_plan() {
    let dir = TempDir::new("no-partial");
    dir.write("bitty/lua/good.lua", "return { theme = 'tokyo-night' }");
    let source = user_source(dir.path().join("bitty/init.lua"));
    let content = "require('good')\nreturn require('absent')\n";
    let error = parse_lua_config_with_root(content, &source, Some(&user_root(dir.path())))
        .expect_err("refused");
    assert!(matches!(error, ConfigError::Module { .. }), "{error:?}");
    assert!(error.to_string().contains("E_CONFIG_MODULE_NOT_FOUND"));
}

// ── redaction ───────────────────────────────────────────────────────────────

#[test]
fn the_typed_record_never_carries_a_host_path() {
    let dir = TempDir::new("redaction");
    dir.write("bitty/lua/ok.lua", "return {}");
    let source = user_source(dir.path().join("bitty/init.lua"));
    let root = user_root(dir.path());
    for (content, code) in [
        ("return require('absent')", "E_CONFIG_MODULE_NOT_FOUND"),
        ("return require('../etc/passwd')", "E_CONFIG_MODULE_NAME"),
        ("return require('bitty')", "E_CONFIG_MODULE_NAME"),
        ("return require(42)", "E_CONFIG_MODULE_NAME"),
    ] {
        let error = parse_lua_config_with_root(content, &source, Some(&root)).expect_err("refused");
        let rendered = error.to_string();
        assert!(rendered.contains(code), "{content}: {rendered}");
        let host_path = dir.path().to_string_lossy();
        assert!(
            !rendered.contains(host_path.as_ref()),
            "{content} leaked a host path: {rendered}"
        );
        assert!(!rendered.contains(&root.to_string_lossy().to_string()));
    }
}

#[test]
fn an_unrepresentable_root_reports_root_and_not_not_found() {
    let dir = TempDir::new("root-kind");
    let source = user_source(dir.path().join("bitty/init.lua"));
    // `bitty/` exists but `bitty/lua` does not: the module root is missing,
    // which is a root-layout refusal, not a missing module.
    dir.write("bitty/other.txt", "");
    let error =
        parse_lua_config_with_root("return require('x')", &source, Some(&user_root(dir.path())))
            .expect_err("refused");
    let diagnostic = error.module_diagnostic().expect("typed");
    assert_eq!(diagnostic.code().as_str(), "E_CONFIG_MODULE_ROOT");
    assert!(diagnostic.module_spec.is_none(), "no spec resolved");
    let rendered = error.to_string();
    assert!(rendered.contains("phase=root"));
    assert!(rendered.contains("source=\"init.lua\""));
    assert!(!rendered.contains(dir.path().to_string_lossy().as_ref()));
}

// ── the `--safe` read counter

#[test]
fn the_read_counter_only_moves_on_an_external_read() {
    // `--safe` selects the built-in effective configuration before any
    // external read; this counter is the one choke point every configuration
    // file read passes through, and the binary-level `--safe` test parks a
    // FIFO where a read would land. Here the hook itself is pinned: an
    // in-memory parse never moves it, a file load moves it exactly once.
    let dir = TempDir::new("read-counter");
    let init = dir.write("bitty/init.lua", "return { theme = 'bitty-dark' }");
    let before = bitty_config::file::external_config_reads();
    let source = user_source(init.clone());
    parse_lua_config("return { theme = 'bitty-dark' }", &source).expect("in-memory parse");
    assert_eq!(
        bitty_config::file::external_config_reads(),
        before,
        "an in-memory parse reads no file"
    );
    load_user_layer(&init).expect("file load");
    assert_eq!(
        bitty_config::file::external_config_reads(),
        before + 1,
        "one file load, one counted read"
    );
}

// ── no loader without a root ────────────────────────────────────────────────

#[test]
fn the_in_memory_parse_path_still_denies_require() {
    let source = ConfigSource::new(LayerKind::CoreDefaults, None::<String>);
    // A configuration that never requires keeps working with no loader, and
    // the documented deny list is what a config chunk sees.
    let plan = parse_lua_config(
        "assert(load == nil and dofile == nil and package == nil and io == nil)\n\
         assert(type(require) == 'nil')\n\
         return { theme = 'bitty-dark' }",
        &source,
    )
    .expect("plain config still loads");
    assert_eq!(
        plan.appearance.as_ref().and_then(|a| a.theme.as_deref()),
        Some("bitty-dark")
    );
    // Salvaging without a root behaves the same way.
    assert!(parse_lua_config_salvaging("return require('x')", &source).is_err());
}
