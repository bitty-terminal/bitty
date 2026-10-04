//! Composer ownership cutover (W-103 S-5, CTX-0929).
//!
//! Decides whether the ACTIVE `composer` plugin owns the editing UX or the
//! retained Core edit/submit path applies. The plugin owns only when it is
//! installed, activated, and granted every composer capability; every other
//! state falls back to the retained Core behavior with a diagnostic:
//!
//! - safe mode: retained, identical to before (no plugin VM is consulted);
//! - zero-plugin startup (no plugin runtime): retained;
//! - plugin absent (uninstalled) or not activated: retained;
//! - version or capability mismatch: the activation layer already disables
//!   fail-closed with a diagnostic and no partial activation (no VM, no
//!   command ownership, capture released); the owner stays retained.
//!
//! Version and capability validation itself lives in
//! `bitty_runtime::plugin_runtime` (compat checked at activation, grants are
//! the manifest-declared intersection); this module only reads the resulting
//! lifecycle state and grant snapshot, so it cannot weaken the gate.

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{LifecycleState, PluginRuntime};

/// Plugin id of the first-party Composer extension (W-82 delivery shape).
///
/// Matches the `composer` package manifest (`bitty-plugin.toml`); provenance
/// comes from discovery, never from this literal.
pub(crate) const COMPOSER_PLUGIN_ID: &str = "bitty-terminal.composer";

/// Exact public capability set the Composer extension consumes (W-82).
///
/// No filesystem, process-spawn, network, clipboard, or terminal-input
/// authority beyond these three grants; the editor temp path never leaves
/// Core.
pub(crate) const COMPOSER_REQUIRED_CAPABILITIES: &[&str] = &[
    "ui.overlay.focus",
    "terminal.input.submit",
    "process.editor",
];

/// Composer session verbs (plugin-qualified `<id>:<verb>` at dispatch).
///
/// Mirrors the package manifest `[lazy].commands` verbs; dispatch fails
/// closed when the plugin did not register a verb.
pub(crate) const COMPOSER_COMMAND_OPEN: &str = "open";
pub(crate) const COMPOSER_COMMAND_SUBMIT: &str = "submit";
pub(crate) const COMPOSER_COMMAND_CANCEL: &str = "cancel";
pub(crate) const COMPOSER_COMMAND_CLOSE: &str = "close";
pub(crate) const COMPOSER_COMMAND_EDITOR: &str = "editor";

/// Every verb Core may dispatch to the composer plugin (closed set).
pub(crate) const COMPOSER_KNOWN_VERBS: &[&str] = &[
    COMPOSER_COMMAND_OPEN,
    COMPOSER_COMMAND_SUBMIT,
    COMPOSER_COMMAND_CANCEL,
    COMPOSER_COMMAND_CLOSE,
    COMPOSER_COMMAND_EDITOR,
];

/// Who owns the composer editing UX for this input path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerOwner {
    /// The ACTIVE composer plugin owns editing via overlay/capture/submit/editor.
    Plugin,
    /// The retained Core edit/submit path applies, for the named reason.
    RetainedCore(RetainedReason),
}

impl ComposerOwner {
    /// True when the ACTIVE plugin owns the editing UX.
    pub(crate) fn plugin_owns(&self) -> bool {
        matches!(self, Self::Plugin)
    }
}

/// Why the retained Core path applies instead of the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetainedReason {
    /// Safe recovery mode: no third-party VM is consulted, identical to before.
    SafeMode,
    /// No plugin runtime exists (zero-plugin startup).
    NoPluginRuntime,
    /// The composer package is not installed (never discovered).
    NotInstalled,
    /// Installed but not activated (suspended, disposed, failed, or a version
    /// mismatch the activation layer disabled fail-closed).
    NotActive {
        /// Lifecycle snapshot (`Unloaded`, `Failed(..)`, ...).
        state: String,
    },
    /// Activated without every composer capability: no partial activation, the
    /// missing grant is named.
    CapabilityMismatch {
        /// First required capability the grant snapshot lacks.
        missing: String,
    },
}

impl RetainedReason {
    /// Stable machine-readable code for logs and diagnostics.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::SafeMode => "safe-mode",
            Self::NoPluginRuntime => "no-plugin-runtime",
            Self::NotInstalled => "not-installed",
            Self::NotActive { .. } => "not-active",
            Self::CapabilityMismatch { .. } => "capability-mismatch",
        }
    }

    /// Bounded human diagnostic naming the plugin and the blocking fact.
    pub(crate) fn diagnostic(&self) -> String {
        match self {
            Self::SafeMode => format!(
                "composer plugin '{COMPOSER_PLUGIN_ID}' skipped (--safe, no VM): retained Core composer applies"
            ),
            Self::NoPluginRuntime => format!(
                "composer plugin '{COMPOSER_PLUGIN_ID}' has no plugin runtime (zero-plugin startup): retained Core composer applies"
            ),
            Self::NotInstalled => format!(
                "composer plugin '{COMPOSER_PLUGIN_ID}' is not installed: retained Core composer applies"
            ),
            Self::NotActive { state } => format!(
                "composer plugin '{COMPOSER_PLUGIN_ID}' is not active (state {state}, version mismatch disables with no partial activation): retained Core composer applies"
            ),
            Self::CapabilityMismatch { missing } => format!(
                "composer plugin '{COMPOSER_PLUGIN_ID}' lacks capability '{missing}' (no partial activation): retained Core composer applies"
            ),
        }
    }
}

/// Pure ownership decision (W-103 S-5 cutover rule).
///
/// `safe_mode` is the process `--safe` latch: it wins over every plugin
/// state and never consults a VM. Otherwise the plugin owns only when the
/// runtime reports it `Active` with every required capability granted.
pub(crate) fn decide_composer_owner(
    safe_mode: bool,
    plugin: Option<&PluginRuntime>,
) -> ComposerOwner {
    if safe_mode {
        return ComposerOwner::RetainedCore(RetainedReason::SafeMode);
    }
    let Some(runtime) = plugin else {
        return ComposerOwner::RetainedCore(RetainedReason::NoPluginRuntime);
    };
    let Ok(id) = PluginId::new(COMPOSER_PLUGIN_ID) else {
        return ComposerOwner::RetainedCore(RetainedReason::NotInstalled);
    };
    match runtime.state(&id) {
        None => ComposerOwner::RetainedCore(RetainedReason::NotInstalled),
        Some(LifecycleState::Active) => match runtime.services(&id) {
            None => ComposerOwner::RetainedCore(RetainedReason::NotActive {
                state: String::from("Active-without-services"),
            }),
            Some(services) => match COMPOSER_REQUIRED_CAPABILITIES
                .iter()
                .find(|capability| !services.has_granted_capability(capability))
            {
                None => ComposerOwner::Plugin,
                Some(missing) => ComposerOwner::RetainedCore(RetainedReason::CapabilityMismatch {
                    missing: (*missing).to_string(),
                }),
            },
        },
        Some(state) => ComposerOwner::RetainedCore(RetainedReason::NotActive {
            state: format!("{state:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::fixture;
    use super::*;

    #[test]
    fn safe_mode_wins_over_every_plugin_state() {
        // Identical to before: safe mode never consults a VM, even with no
        // runtime at all.
        assert_eq!(
            decide_composer_owner(true, None),
            ComposerOwner::RetainedCore(RetainedReason::SafeMode)
        );
        let root = fixture::temp_dir("safe-wins");
        let mut runtime = fixture::runtime_for(vec![root.clone()]);
        let _ = runtime.discover();
        assert_eq!(
            decide_composer_owner(true, Some(&runtime)),
            ComposerOwner::RetainedCore(RetainedReason::SafeMode)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zero_plugin_startup_falls_back_to_retained_core() {
        // E-ADD-4: no plugin runtime (zero-plugin startup) keeps the
        // retained Core behavior.
        let owner = decide_composer_owner(false, None);
        assert_eq!(
            owner,
            ComposerOwner::RetainedCore(RetainedReason::NoPluginRuntime)
        );
        assert!(!owner.plugin_owns());
        assert!(
            owner
                .clone()
                .retained_diagnostic_for_test()
                .contains(COMPOSER_PLUGIN_ID)
        );
    }

    #[test]
    fn uninstalled_plugin_falls_back_to_retained_core() {
        // E-ADD-4/S-7: a runtime with no composer package (uninstalled)
        // keeps the retained Core behavior.
        let root = fixture::temp_dir("uninstalled");
        let mut runtime = fixture::runtime_for(vec![root.clone()]);
        let discovered = runtime.discover();
        assert!(discovered.is_empty(), "fixture root holds no package");
        let owner = decide_composer_owner(false, Some(&runtime));
        assert_eq!(
            owner,
            ComposerOwner::RetainedCore(RetainedReason::NotInstalled)
        );
        assert!(!owner.plugin_owns());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn version_mismatch_disables_with_diagnostic_and_no_partial_activation() {
        // E-ADD-4: an impossible `compat.bitty` floor fails closed at
        // activation (typed incompatible state, no VM) and the owner stays
        // retained with a diagnostic.
        let root = fixture::temp_dir("mismatch");
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=999.0",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[
                COMPOSER_COMMAND_OPEN,
                COMPOSER_COMMAND_SUBMIT,
                COMPOSER_COMMAND_CANCEL,
                COMPOSER_COMMAND_CLOSE,
                COMPOSER_COMMAND_EDITOR,
            ],
        );
        let mut runtime = fixture::runtime_for(vec![root.clone()]);
        let discovered = runtime.discover();
        assert_eq!(discovered.len(), 1, "composer package must discover");
        let id = PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        let error = runtime.activate(&id).expect_err("mismatch must fail");
        assert_eq!(error.code(), "E_INCOMPATIBLE");
        assert!(error.to_string().contains(">=999.0"));
        // No partial activation: never Active, owns no command, holds no
        // capture, grants nothing.
        assert!(!matches!(runtime.state(&id), Some(LifecycleState::Active)));
        assert!(
            !runtime.host_owns_command(&format!("{COMPOSER_PLUGIN_ID}:{}", COMPOSER_COMMAND_OPEN))
        );
        assert!(runtime.services(&id).is_none());
        let owner = decide_composer_owner(false, Some(&runtime));
        let RetainedReason::NotActive { state } = owner.clone().retained_for_test() else {
            panic!("mismatch must map to retained not-active, got {owner:?}");
        };
        assert!(
            state.contains("Failed"),
            "state names the failure, got {state}"
        );
        assert!(owner.diagnostic_for_test().contains(COMPOSER_PLUGIN_ID));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn active_plugin_with_all_capabilities_owns_editing() {
        // S-5: an ACTIVE composer plugin with the full grant set owns the
        // editing UX via overlay/capture/submit/editor.
        let root = fixture::temp_dir("active");
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=0.0.1",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[
                COMPOSER_COMMAND_OPEN,
                COMPOSER_COMMAND_SUBMIT,
                COMPOSER_COMMAND_CANCEL,
                COMPOSER_COMMAND_CLOSE,
                COMPOSER_COMMAND_EDITOR,
            ],
        );
        let mut runtime = fixture::runtime_for(vec![root.clone()]);
        runtime.discover();
        let id = PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        let report = runtime.activate(&id).expect("compatible plugin activates");
        assert_eq!(report.state, LifecycleState::Active);
        let owner = decide_composer_owner(false, Some(&runtime));
        assert_eq!(owner, ComposerOwner::Plugin);
        assert!(owner.plugin_owns());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn suspended_plugin_falls_back_to_retained_core() {
        // S-7 rollback shape: a suspended (disabled) plugin no longer owns.
        let root = fixture::temp_dir("suspended");
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=0.0.1",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[COMPOSER_COMMAND_OPEN],
        );
        let mut runtime = fixture::runtime_for(vec![root.clone()]);
        runtime.discover();
        let id = PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        runtime.activate(&id).expect("activate");
        assert_eq!(
            decide_composer_owner(false, Some(&runtime)),
            ComposerOwner::Plugin
        );
        runtime.suspend(&id).expect("suspend (disable)");
        let owner = decide_composer_owner(false, Some(&runtime));
        let RetainedReason::NotActive { state } = owner.clone().retained_for_test() else {
            panic!("suspended must map to retained not-active, got {owner:?}");
        };
        assert!(state.contains("Suspended"), "got {state}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn diagnostics_name_plugin_and_blocking_fact() {
        let cases = [
            (RetainedReason::SafeMode, "safe-mode", COMPOSER_PLUGIN_ID),
            (
                RetainedReason::NoPluginRuntime,
                "no-plugin-runtime",
                COMPOSER_PLUGIN_ID,
            ),
            (
                RetainedReason::NotInstalled,
                "not-installed",
                COMPOSER_PLUGIN_ID,
            ),
            (
                RetainedReason::NotActive {
                    state: String::from("Failed(E_INCOMPATIBLE)"),
                },
                "not-active",
                "Failed",
            ),
            (
                RetainedReason::CapabilityMismatch {
                    missing: String::from("process.editor"),
                },
                "capability-mismatch",
                "process.editor",
            ),
        ];
        for (reason, code, needle) in cases {
            assert_eq!(reason.code(), code);
            assert!(
                reason.diagnostic().contains(needle),
                "diagnostic must name the blocking fact: {}",
                reason.diagnostic()
            );
        }
    }

    // Test-only projections over the crate-private enums.
    impl ComposerOwner {
        fn retained_for_test(self) -> RetainedReason {
            let Self::RetainedCore(reason) = self else {
                panic!("expected retained owner, got {self:?}");
            };
            reason
        }

        fn diagnostic_for_test(&self) -> String {
            let Self::RetainedCore(reason) = self else {
                panic!("expected retained owner, got {self:?}");
            };
            reason.diagnostic()
        }

        fn retained_diagnostic_for_test(self) -> String {
            let Self::RetainedCore(reason) = self else {
                panic!("expected retained owner, got {self:?}");
            };
            reason.diagnostic()
        }
    }
}

/// Test-only composer plugin fixtures shared by the cutover, fallback, and
/// rollback-drill tests in this crate (W-103 S-5/S-7, E-ADD-4).
#[cfg(test)]
pub(crate) mod fixture {
    use bitty_runtime::plugin_runtime::{
        EmptySettings, PluginRuntime, PluginRuntimeConfig, UnavailableSnapshot,
    };
    use std::rc::Rc;

    /// Test-only plugin runtime over explicit discovery roots (no store, no
    /// backend: `open_store` serves `in_memory` with `data_dir: None`, and
    /// the version-mismatch path rolls back before any store work).
    pub(crate) fn runtime_for(third_party_roots: Vec<std::path::PathBuf>) -> PluginRuntime {
        PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: None,
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots,
            settings: Rc::new(EmptySettings),
            snapshot: Rc::new(UnavailableSnapshot),
        })
    }

    pub(crate) fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bitty-composer-owner-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Writes one dev-root package: `<root>/<id>/bitty-plugin.toml` plus
    /// `<root>/<id>/lua/init.lua` registering every declared command (the
    /// activation layer refuses partial registration).
    pub(crate) fn write_plugin(
        root: &std::path::Path,
        id: &str,
        compat_bitty: &str,
        capabilities: &[&str],
        commands: &[&str],
    ) {
        let plugin = root.join(id);
        std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
        let caps = capabilities
            .iter()
            .map(|c| format!("{c} = true"))
            .collect::<Vec<_>>()
            .join("\n");
        let cmds = commands
            .iter()
            .map(|c| format!("\"{id}:{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"Composer Owner Test\"\nversion = \"0.0.1\"\n\
                 description = \"composer ownership test\"\n\n\
                 [compat]\nbitty = \"{compat_bitty}\"\nplugin-api = \"^1.0\"\n\n\
                 [capabilities]\n{caps}\n\n\
                 [lazy]\ncommands = [{cmds}]\nevents = []\n",
            ),
        )
        .expect("manifest");
        let registrations = commands
            .iter()
            .map(|c| {
                format!(
                    "bitty.commands.register({{ id = \"{c}\", title = \"{c}\", run = function() return true end }})"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            plugin.join("lua/init.lua"),
            format!("{registrations}\nreturn {{}}\n"),
        )
        .expect("init");
    }
}
