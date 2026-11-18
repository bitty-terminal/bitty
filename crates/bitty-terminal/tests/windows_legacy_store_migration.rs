//! Windows end-to-end proof that an install upgraded across bitty#3226 keeps
//! the plugin store it already had (bitty#3236).
//!
//! The resolver fixture lives in `src/plugin_runtime.rs`; this file drives the
//! built `bitty.exe` instead, so the whole chain is proven on the target rather
//! than through an injected path: the process environment (a shell-exported
//! `HOME`, `%LOCALAPPDATA%` set, `$XDG_DATA_HOME` unset), `bitty plugin list`,
//! the one-time adoption, and discovery over the moved store.
//!
//! The whole file is `#![cfg(windows)]`: the population this describes only
//! exists on Windows, and the operation under test is the filesystem's
//! (a directory rename between the profile and the local-app-data root). The
//! platform-independent half of the policy — the legacy-root predicate and its
//! exhaustion on every other environment shape — stays in the crate's unit
//! tests, where it also runs on the Unix legs.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bitty_runtime::plugin_runtime::{
    PluginRecord, SourceClass, content_digest, manifest_toml, write_index,
};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Plugin id of the package the legacy store holds.
const LEGACY_PLUGIN: &str = "xuepoo.legacy";

/// Name the planted package declares, so the assertion fails when only the
/// index moved and the package bytes did not.
const LEGACY_PLUGIN_NAME: &str = "Legacy Store";

/// One isolated Windows install shape: a profile (what the shell exports as
/// `HOME`) beside a local-app-data root (`%LOCALAPPDATA%`).
struct Install {
    /// Scratch root holding both halves.
    base: PathBuf,
    /// Profile directory the legacy `$HOME/.local/share` tree lives under.
    home: PathBuf,
    /// Local-app-data root the post-bitty#3226 resolver prefers.
    local: PathBuf,
}

impl Install {
    /// Fresh `<scratch>/Users/u` + `<scratch>/AppData/Local` layout.
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "bitty-win-legacy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("Users").join("u");
        let local = base.join("AppData").join("Local");
        std::fs::create_dir_all(&home).expect("profile dir");
        std::fs::create_dir_all(&local).expect("local app data dir");
        Self { base, home, local }
    }

    /// Pre-bitty#3226 tier root: exactly what the old precedence wrote.
    fn legacy_root(&self, leaf: &str) -> PathBuf {
        self.home
            .join(".local")
            .join("share")
            .join("bitty")
            .join(leaf)
    }

    /// Post-bitty#3226 tier root.
    fn current_root(&self, leaf: &str) -> PathBuf {
        self.local.join("bitty").join(leaf)
    }

    /// `HOME` value the child process sees.
    fn home_arg(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }

    /// `LOCALAPPDATA` value the child process sees.
    fn local_arg(&self) -> String {
        self.local.to_string_lossy().into_owned()
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Run `bitty plugin …` as the upgraded install would: `HOME` exported, a
/// `%LOCALAPPDATA%` present, and no `$XDG_DATA_HOME` (it always outranked
/// `HOME`, so an XDG install never wrote the legacy root).
fn run_in(install: &Install, args: &[&str]) -> Output {
    let config = install.base.join("config");
    std::fs::create_dir_all(&config).expect("config dir");
    Command::new(BITTY_BIN)
        .args(args)
        .current_dir(&install.home)
        .env("XDG_CONFIG_HOME", &config)
        .env_remove("XDG_DATA_HOME")
        .env("HOME", install.home_arg())
        .env("LOCALAPPDATA", install.local_arg())
        .env_remove("APPDATA")
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PLUGIN_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("run {BITTY_BIN:?} {args:?}: {error}"))
}

/// Plant one installed package plus a store index the loader re-verifies, at
/// `root` — the shape an install that ran before the precedence change left
/// behind.
fn plant_installed_store(root: &Path) {
    let package = root.join("packages").join(LEGACY_PLUGIN).join("0.1.0");
    std::fs::create_dir_all(package.join("lua")).expect("package dirs");
    let manifest = format!(
        "[plugin]\nid = \"{LEGACY_PLUGIN}\"\nname = \"{LEGACY_PLUGIN_NAME}\"\nversion = \"0.1.0\"\n\
         description = \"installed before the %LOCALAPPDATA% precedence change\"\n\n\
         [compat]\nplugin-api = \"^1.0\"\n\n[lazy]\ncommands = []\nevents = []\n"
    );
    std::fs::write(package.join("bitty-plugin.toml"), manifest).expect("manifest");
    std::fs::write(package.join("lua").join("init.lua"), "return {}\n").expect("init");
    let hash = manifest_toml::parse_manifest(
        &std::fs::read(package.join("bitty-plugin.toml")).expect("manifest bytes"),
    )
    .expect("manifest parses")
    .manifest_hash();
    let record = PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: LEGACY_PLUGIN.to_string(),
        version: "0.1.0".to_string(),
        root: format!("packages/{LEGACY_PLUGIN}/0.1.0"),
        manifest_hash: hash,
        content_digest: content_digest(&package).expect("content digest"),
        enabled: true,
        granted: Vec::new(),
    };
    write_index(root, &[record]).expect("store index");
}

#[test]
fn upgraded_windows_install_still_lists_the_plugins_it_had_installed() {
    let install = Install::new("present");
    let legacy = install.legacy_root("plugins");
    let current = install.current_root("plugins");
    plant_installed_store(&legacy);

    // Before: the package only exists in the pre-change root.
    assert!(legacy.join("current.json").is_file());
    assert!(!current.exists());

    let output = run_in(&install, &["plugin", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(LEGACY_PLUGIN), "{text}");
    assert!(
        text.contains(&format!("\"name\":\"{LEGACY_PLUGIN_NAME}\"")),
        "the package bytes moved with the index: {text}"
    );
    assert!(text.contains("\"state\":\"enabled\""), "{text}");

    // After: the store lives in `%LOCALAPPDATA%` and the legacy tree is gone,
    // so discovery read exactly one index.
    assert!(
        current.join("current.json").is_file(),
        "the store was adopted into the current root"
    );
    assert!(
        current
            .join("packages")
            .join(LEGACY_PLUGIN)
            .join("0.1.0")
            .join("bitty-plugin.toml")
            .is_file(),
        "the package directory moved, not just the index"
    );
    assert!(!legacy.exists(), "the adopted tree is not left behind");

    // Idempotent: the next run resolves the same root and moves nothing.
    let output = run_in(&install, &["plugin", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains(LEGACY_PLUGIN));
    assert!(current.join("current.json").is_file());
}

#[test]
fn current_windows_root_untouched_when_no_legacy_store_exists() {
    let install = Install::new("absent");
    let current = install.current_root("plugins");

    let output = run_in(&install, &["plugin", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stdout(&output).contains(LEGACY_PLUGIN),
        "nothing was installed, so nothing is listed: {}",
        stdout(&output)
    );
    // A read-only verb creates no store, and the absence of a legacy tree is
    // not a migration.
    assert!(!current.exists(), "no store is created by listing");
    assert!(!install.legacy_root("plugins").exists());

    // The store is still resolvable afterwards: the empty state is a real
    // root, not a stranded legacy path.
    let output = run_in(&install, &["plugin", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}
