//! Generated official plugin catalog. Do not edit by hand.
//!
//! Rendered by `just catalog-generate` from
//! `registry/official/*.toml` plus each `plugins/<name>` gitlink in
//! <https://github.com/bitty-terminal/bitty-plugins> at `SOURCE_REVISION`. The checked-in output is
//! gated by `just catalog-check`; Core never fetches registry metadata at
//! runtime.
//!
//! @generated

use crate::official::OfficialPlugin;

/// Registry repository this catalog was generated from.
pub const SOURCE_REPOSITORY: &str = "https://github.com/bitty-terminal/bitty-plugins";

/// Exact registry commit this catalog was generated from.
pub const SOURCE_REVISION: &str = "1f369cf1a9f4f3d767e1664e9c8ec4ea43a6beff";

/// Official plugin entries, sorted by canonical id.
pub static OFFICIAL_PLUGINS: &[OfficialPlugin] = &[
    OfficialPlugin {
        id: "bitty-featured.activity",
        name: "Bitty Activity",
        repository: "https://github.com/bitty-terminal/activity.git",
        pin: "28e46bcb8e9c56be4fd7f567c0f8bb4b18369773",
        manifest_hash: "sha256:21ba1eb35b7a867f1bdb33efaa10237005041381cc6f966347cf712021281045",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Privacy-first local activity timeline plugin for the Bitty terminal.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-featured.devtools",
        name: "Bitty DevTools",
        repository: "https://github.com/bitty-terminal/devtools.git",
        pin: "8c8abe30ed4644286b07c1de1b57b66c257f801a",
        manifest_hash: "sha256:a8c4755f02254e0f8c085755279f5b25951e55b02423730afe0f85ae02678f26",
        compat_bitty: Some(">=0.0.21"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only inspection and event tracing of the Bitty plugin runtime.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.bar",
        name: "Unified Bar",
        repository: "https://github.com/bitty-terminal/bar.git",
        pin: "6af2be11ee3df9f2c4fecbd73408e2c2f70a6b8c",
        manifest_hash: "sha256:651cd54d78f0f85d8db5faf745fa12b10f4a12703029b7d8646140816535c0c8",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Consolidated workspace-bar, tabs, and statusline presentation over generic chrome insets.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.composer",
        name: "Composer",
        repository: "https://github.com/bitty-terminal/composer.git",
        pin: "fcb7c776433625494dac0ebca5b5f75549c3b931",
        manifest_hash: "sha256:c9cf16a18f538ad02e5addef0cc0443d49cc6dc2df43d7f0588798951b861a33",
        compat_bitty: Some(">=0.0.21"),
        compat_plugin_api: Some("^1.0"),
        description: "Editable command-line policy over the public overlay, submit, and editor host APIs: modal buffer, chord routing, bounded paste, and external-editor round trip.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.copy-mode",
        name: "Copy mode",
        repository: "https://github.com/bitty-terminal/copy-mode.git",
        pin: "cb894b449eaba8c6286a0242f183d6c85ed6b48e",
        manifest_hash: "sha256:2c6d732acc465b3a4bbd87b123ecb927bcf43f3acd742f1db8704068536c678e",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Modal visual selection over the public history snapshot API: bounded scoped transcript queries with typed denials, untrusted labels, anchor/cursor selection, and grant-gated yank.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.dashboard",
        name: "Dashboard",
        repository: "https://github.com/bitty-terminal/dashboard.git",
        pin: "5a599158d019b278bcd2596173714dd4afb27f78",
        manifest_hash: "sha256:ff9a2e6ea3538bf9af079d0095342b96ab2000632cbdd4ae723a9e31efdb7408",
        compat_bitty: Some(">=0.0.26"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only configuration and status dashboard over bitty.runtime.inspect(): plugins, extensions, settings, profiles, and diagnostics in one panel.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.file-manager",
        name: "File Manager",
        repository: "https://github.com/bitty-terminal/file-manager.git",
        pin: "74f4124b697f55aa97ba8fcdd2cbbf40f22ffc72",
        manifest_hash: "sha256:b26f36ecbe7ce5b3fb0338c90c594f98a104de021a9b7eab4b6b6af8645fa010",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Bounded read-only directory listing, navigation, and preview for a caller-chosen root.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.git-panel",
        name: "Git Panel",
        repository: "https://github.com/bitty-terminal/git-panel.git",
        pin: "494e0e9cc27d1d5a8d4d1abe9f4207c0b60e8a8f",
        manifest_hash: "sha256:2d4ebbe92f3acb5ffa87dee15d81c75f5e1ef889940c504cad6f8a7533d1effe",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only branch, status, diff, and log panel for the Git repository behind the focused pane.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.history",
        name: "History",
        repository: "https://github.com/bitty-terminal/history.git",
        pin: "132a35050ea6ba4fcd2f522a0a42e89fd4f3be97",
        manifest_hash: "sha256:ccb50517bcf03b606df5e0e7deff8be3407f8e3bd6712ead23552c5c02e7ab66",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Opt-in history policy over the public history-read host APIs: scoped bounded snapshot queries over transcript, commands, and own KV with redacted truncated attributed answers or typed denials.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.palette",
        name: "Palette",
        repository: "https://github.com/bitty-terminal/palette.git",
        pin: "b023106bccd83db9e6efae66060b64db96f86cfe",
        manifest_hash: "sha256:291bfcd222ff0d6cfc4acfb2fc4f27a2e6f2bd993e777262ad5d2f14b7c1b884",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Command palette and picker UI via overlay slot, declarative primitives only.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.search",
        name: "Search",
        repository: "https://github.com/bitty-terminal/search.git",
        pin: "35a1275f57224670562f18948701c77f4f04f244",
        manifest_hash: "sha256:3d7b30a89a52feda58810a2c5071792a7af78380e6c8f7cf45c3199df5110e8d",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Scrollback search policy over the public history snapshot API: bounded scoped transcript queries with typed denials, untrusted labels, result navigation, and grant-gated export.",
        license: Some("MIT"),
    },
];
