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
pub const SOURCE_REVISION: &str = "3307ffa446477cf3f9b76e037c59100899c49fe9";

/// Official plugin entries, sorted by canonical id.
pub static OFFICIAL_PLUGINS: &[OfficialPlugin] = &[
    OfficialPlugin {
        id: "bitty-featured.activity",
        name: "Bitty Activity",
        repository: "https://github.com/bitty-terminal/activity.git",
        pin: "0ea39e13da193f2429e6846b13425b19aff56efb",
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
        pin: "badde42a18da6262876ff275a33ff4964a2ccb5e",
        manifest_hash: "sha256:8dbbab2287687272c12f96d5b8ebc18b8c1f492c08ac422a393a06e990b73267",
        compat_bitty: Some(">=0.0.21"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only inspection and event tracing of the Bitty plugin runtime.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.bar",
        name: "Unified Bar",
        repository: "https://github.com/bitty-terminal/bar.git",
        pin: "a00f26974c51bb2067f70302cca2dcef6685436f",
        manifest_hash: "sha256:327837b4fea90e9f90529c7761db9539ded0627937735f01ebc370a22a705bfb",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Consolidated workspace-bar, tabs, and statusline presentation over generic chrome insets.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.beacon",
        name: "Beacon",
        repository: "https://github.com/bitty-terminal/beacon.git",
        pin: "88e5998dfc1d5158d11d244cde41c2d28b0a9050",
        manifest_hash: "sha256:f84ef7369c377b6433a6805952c6b2344b37939e3e8a66abc47aff393f0a33c7",
        compat_bitty: Some(">=0.0.31"),
        compat_plugin_api: Some("^1.0"),
        description: "Spatial and semantic workspace targeting policy (Action x Target) over the live target registry and label allocator.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.composer",
        name: "Composer",
        repository: "https://github.com/bitty-terminal/composer.git",
        pin: "8be240f2ea7cbfc6a50bb9ea44cdcd2477187031",
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
        pin: "2b78ceb382e469960dc115e8441513c1389b34cb",
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
        pin: "3b84b7e13913381a8de636ab6db9abef417063c8",
        manifest_hash: "sha256:ffb3532cea97e4417aa70f51aa3f3d4fe422ef41154d1efb82c9395d15578095",
        compat_bitty: Some(">=0.0.26"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only configuration and status dashboard over bitty.runtime.inspect(): plugins, extensions, settings, profiles, and diagnostics in one panel.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.file-manager",
        name: "File Manager",
        repository: "https://github.com/bitty-terminal/file-manager.git",
        pin: "558bf5a2306225ee6fdff109cf739fa15f894a93",
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
        pin: "4f54634f08e07ec2e37b5c22e5ed8d75e481bc4d",
        manifest_hash: "sha256:fb18021aba54da25256d01edf2e6fdc6caef9012e9e1f71f1af74ac745f2ace6",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only branch, status, diff, and log panel for the Git repository behind the focused pane.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.help",
        name: "Help",
        repository: "https://github.com/bitty-terminal/help.git",
        pin: "ae1d8a9a39862ce9a71ccd7238f5621975144a47",
        manifest_hash: "sha256:8cac6d1d761547c86861e9207b3ca659d44a27a6da9c42094e8b2f09e600debc",
        compat_bitty: Some(">=0.0.31"),
        compat_plugin_api: Some("^1.0"),
        description: "Help and which-key shortcut overlay over the live keymap registry.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.history",
        name: "History",
        repository: "https://github.com/bitty-terminal/history.git",
        pin: "67178e2298293a8ba3ae59e8b5e0c61c5a5b0012",
        manifest_hash: "sha256:da55df28ae1bd0f64c2d99c0f71e00d8ee81cf3024a5b3896c9bc015c068fb8c",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Opt-in history policy over the public history-read host APIs: scoped bounded snapshot queries over transcript, commands, and own KV with redacted truncated attributed answers or typed denials.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.palette",
        name: "Palette",
        repository: "https://github.com/bitty-terminal/palette.git",
        pin: "62a5dd263bf7564c98f7052314e89eaadddf8b41",
        manifest_hash: "sha256:53e334de7f98631f68af21495d1dc9c40c7ed27211cdab5062ed0c95ef191547",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Command palette and picker UI via overlay slot, declarative primitives only.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.search",
        name: "Search",
        repository: "https://github.com/bitty-terminal/search.git",
        pin: "ee9a8f9c59ad77aa3c03c7266221b613b697f4d3",
        manifest_hash: "sha256:3d7b30a89a52feda58810a2c5071792a7af78380e6c8f7cf45c3199df5110e8d",
        compat_bitty: Some(">=0.0.24"),
        compat_plugin_api: Some("^1.0"),
        description: "Scrollback search policy over the public history snapshot API: bounded scoped transcript queries with typed denials, untrusted labels, result navigation, and grant-gated export.",
        license: Some("MIT"),
    },
];
