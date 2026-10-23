//! Live plugin-suggested keybindings over real plugin VMs (issue #2407).
//!
//! `bitty.keymaps.suggest` captures at activation; this suite proves the
//! publication layer that makes the capture live at the reserved
//! `DispatchPriority::Plugin` slot: same-generation owner-only targets,
//! deterministic ordering, generation fencing through suspend/resume/reload/
//! dispose, and fail-closed drops for unqualified, foreign, unregistered,
//! malformed, and prefix-spelled suggestions. Headless and Windows-safe
//! (temp dirs only).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_config::{ChromeAction, KeyName, KeyRef, ModKey};
use bitty_lua::BridgeError;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<bitty_lua::LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-suggested-bindings-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    })
}

/// Write a plugin package under `root/<id>/` whose `init.lua` registers the
/// declared commands and captures the given `(chord, command)` suggestions.
fn write_plugin(
    root: &Path,
    id: &str,
    commands: &[&str],
    suggestions: &[(&str, &str)],
    init_tail: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let declared = commands
        .iter()
        .map(|command| format!("\"{id}:{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Suggestion Test\"\nversion = \"0.1.0\"\n\
             description = \"plugin suggested bindings test\"\n\n\
             [compat]\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\n\n\
             [lazy]\ncommands = [{declared}]\n",
        ),
    )
    .expect("manifest");
    let mut init = String::new();
    for command in commands {
        init.push_str(&format!(
            "bitty.commands.register({{ id = \"{command}\", title = \"{command}\", \
             run = function() return true end }})\n"
        ));
    }
    for (chord, command) in suggestions {
        init.push_str(&format!(
            "bitty.keymaps.suggest({{ chord = \"{chord}\", command = \"{command}\" }})\n"
        ));
    }
    init.push_str(init_tail);
    std::fs::write(plugin.join("lua/init.lua"), init).expect("init");
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

fn alt(ch: char) -> KeyRef {
    KeyRef {
        key: KeyName::Char(ch),
        ctrl: false,
        alt: true,
        shift: false,
        super_held: false,
    }
}

const OWNER: &str = "bitty-featured.help-fixture";
const COMMAND: &str = "toggle-help";
const QUALIFIED: &str = "bitty-featured.help-fixture:toggle-help";

#[test]
fn activation_publishes_the_generation_suggestion() {
    let root = temp_dir("publish");
    write_plugin(
        &root,
        OWNER,
        &[COMMAND],
        &[("alt+?", QUALIFIED)],
        "return {}\n",
    );
    let mut runtime = runtime(root.clone());
    runtime.discover();
    runtime.activate(&pid(OWNER)).expect("activate");
    let directory = runtime.suggested_bindings().borrow();
    assert_eq!(directory.generation_of(OWNER), Some(1));
    assert_eq!(
        directory.action_for(alt('?'), ModKey::Alt),
        Some(ChromeAction::InvokeCommand(QUALIFIED.to_string()))
    );
    // The configured Mod slot applies at lookup time, never at capture.
    assert_eq!(directory.action_for(alt('?'), ModKey::Super), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_same_generation_owner_targets_are_published() {
    let root = temp_dir("owner-only");
    write_plugin(
        &root,
        OWNER,
        &[COMMAND],
        &[
            ("alt+1", QUALIFIED),
            ("alt+2", COMMAND),
            ("alt+3", "bitty-featured.other:toggle-help"),
            ("alt+4", "bitty-featured.help-fixture:missing"),
            ("alt+5", "leader w"),
            ("alt+6", "not-a-chord"),
        ],
        "return {}\n",
    );
    let mut runtime = runtime(root.clone());
    runtime.discover();
    runtime.activate(&pid(OWNER)).expect("activate");
    let directory = runtime.suggested_bindings().borrow();
    let active = directory.active();
    assert_eq!(
        active.len(),
        1,
        "only the owner-local registered target survives: {active:?}"
    );
    assert_eq!(active[0].qualified, QUALIFIED);
    // Unqualified, foreign, unregistered, prefix, and malformed suggestions
    // bind nothing and keep falling through.
    for ch in ['2', '3', '4', '5', '6'] {
        assert_eq!(
            directory.action_for(alt(ch), ModKey::Alt),
            None,
            "suggestion alt+{ch} must fail closed"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn suspend_parks_resume_restores_and_dispose_revokes() {
    let root = temp_dir("lifecycle");
    write_plugin(
        &root,
        OWNER,
        &[COMMAND],
        &[("alt+?", QUALIFIED)],
        "return {}\n",
    );
    let mut runtime = runtime(root.clone());
    runtime.discover();
    let id = pid(OWNER);
    runtime.activate(&id).expect("activate");
    let suggestion = Some(ChromeAction::InvokeCommand(QUALIFIED.to_string()));
    {
        let directory = runtime.suggested_bindings().borrow();
        assert_eq!(directory.action_for(alt('?'), ModKey::Alt), suggestion);
    }
    runtime.suspend(&id).expect("suspend (disable)");
    {
        let directory = runtime.suggested_bindings().borrow();
        assert!(directory.is_suspended(OWNER));
        assert_eq!(directory.action_for(alt('?'), ModKey::Alt), None);
    }
    runtime.resume(&id).expect("resume");
    {
        let directory = runtime.suggested_bindings().borrow();
        assert_eq!(directory.action_for(alt('?'), ModKey::Alt), suggestion);
        assert_eq!(directory.generation_of(OWNER), Some(1));
    }
    runtime.dispose(&id).expect("dispose (unload)");
    assert!(runtime.suggested_bindings().borrow().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reload_publishes_the_new_generation_only() {
    let root = temp_dir("reload");
    write_plugin(
        &root,
        OWNER,
        &[COMMAND],
        &[("alt+?", QUALIFIED)],
        "return {}\n",
    );
    let mut runtime = runtime(root.clone());
    runtime.discover();
    let id = pid(OWNER);
    runtime.activate(&id).expect("activate");
    runtime.reload(&id).expect("reload");
    {
        let directory = runtime.suggested_bindings().borrow();
        assert_eq!(directory.generation_of(OWNER), Some(2));
        assert_eq!(
            directory.action_for(alt('?'), ModKey::Alt),
            Some(ChromeAction::InvokeCommand(QUALIFIED.to_string()))
        );
        assert_eq!(directory.active().len(), 1);
    }
    // The old generation's chord is gone with the disposal that preceded the
    // republish: only the new generation's set is live.
    assert_eq!(runtime.suggested_bindings().borrow().active().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn failed_activation_publishes_nothing() {
    let root = temp_dir("failed");
    write_plugin(
        &root,
        OWNER,
        &[COMMAND],
        &[("alt+?", QUALIFIED)],
        "error(\"activation explodes\")\n",
    );
    let mut runtime = runtime(root.clone());
    runtime.discover();
    assert!(runtime.activate(&pid(OWNER)).is_err());
    assert!(
        runtime.suggested_bindings().borrow().is_empty(),
        "a rolled-back activation must leave no suggestion behind"
    );
    assert_eq!(
        runtime
            .suggested_bindings()
            .borrow()
            .action_for(alt('?'), ModKey::Alt),
        None
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn conflicting_owners_resolve_deterministically() {
    let root = temp_dir("conflict");
    for owner in ["zebra.plugin", "alpha.plugin"] {
        write_plugin(
            &root,
            owner,
            &["toggle"],
            &[("alt+?", &format!("{owner}:toggle"))],
            "return {}\n",
        );
    }
    let mut runtime = runtime(root.clone());
    runtime.discover();
    for owner in ["zebra.plugin", "alpha.plugin"] {
        runtime.activate(&pid(owner)).expect("activate");
    }
    let directory = runtime.suggested_bindings().borrow();
    assert_eq!(
        directory.action_for(alt('?'), ModKey::Alt),
        Some(ChromeAction::InvokeCommand(
            "alpha.plugin:toggle".to_string()
        )),
        "lexical owner order wins regardless of activation order"
    );
    assert_eq!(directory.active().len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}
