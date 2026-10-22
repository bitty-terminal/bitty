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
pub const SOURCE_REVISION: &str = "ce90559ebbc7fe144de935415fd905db4e0bf495";

/// Official plugin entries, sorted by canonical id.
pub static OFFICIAL_PLUGINS: &[OfficialPlugin] = &[
    OfficialPlugin {
        id: "bitty-featured.activity",
        name: "Bitty Activity",
        repository: "https://github.com/bitty-terminal/activity.git",
        pin: "36b1b31ef2f46899da9d34b95feed68c2113c799",
        manifest_hash: "sha256:9c09872baf20c42770e9781136730c4e1ca27e9efe264e35b6b8ebbca649ec93",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Privacy-first local activity timeline plugin for the Bitty terminal.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-featured.devtools",
        name: "Bitty DevTools",
        repository: "https://github.com/bitty-terminal/devtools.git",
        pin: "3f5369ec6cda352090b912d8a3fbfda860308376",
        manifest_hash: "sha256:babf1de7ea38088d0b229514fac7053b7dc8b63306ab2a7f99d83407f1f5e9af",
        compat_bitty: Some(">=0.0.21,<1.0"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only inspection and event tracing of the Bitty plugin runtime.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.bar",
        name: "Unified Bar",
        repository: "https://github.com/bitty-terminal/bar.git",
        pin: "5dfc2e20607a4d7efdcf1be89195b3edc22b7ca0",
        manifest_hash: "sha256:fb91eecff14294563b815c5e195c2d76d85b042aeb572914fdf8f0333ff1d77c",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Consolidated workspace-bar, tabs, and statusline presentation over generic chrome insets.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.composer",
        name: "Composer",
        repository: "https://github.com/bitty-terminal/composer.git",
        pin: "dce0f0796e5df20ca34e415321dcb48865834424",
        manifest_hash: "sha256:32e2672cd5742616cd07d55e3eb8abd9c1eb9824c57c2c7b305f89fe4aed2c8d",
        compat_bitty: Some(">=0.0.21,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Editable command-line policy over the public overlay, submit, and editor host APIs: modal buffer, chord routing, bounded paste, and external-editor round trip.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.copy-mode",
        name: "Copy mode",
        repository: "https://github.com/bitty-terminal/copy-mode.git",
        pin: "2d28ac709f38ed7977c1e7aba6c9bfc8f14afdc0",
        manifest_hash: "sha256:d1b4bfc522b827e2312188c1dffbc1334a1907b90af29b578eb42ab8f77e7c64",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Modal visual selection over the public history snapshot API: bounded scoped transcript queries with typed denials, untrusted labels, anchor/cursor selection, and grant-gated yank.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.dashboard",
        name: "Dashboard",
        repository: "https://github.com/bitty-terminal/dashboard.git",
        pin: "9c7489573484d09518081df88b8bed629680853f",
        manifest_hash: "sha256:1a7dd220ef1a3152ef8fd5dcfacd87100805298a6ffae1939239ed866815597f",
        compat_bitty: Some(">=0.0.26,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only configuration and status dashboard over bitty.runtime.inspect(): plugins, extensions, settings, profiles, and diagnostics in one panel.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.file-manager",
        name: "File Manager",
        repository: "https://github.com/bitty-terminal/file-manager.git",
        pin: "92630b4400d395df1e79ba2b37cd820f49376691",
        manifest_hash: "sha256:561801482d1d48cceab736d88936c5584cc57ae26ea8efc7809fbaa7581d459a",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Bounded read-only directory listing, navigation, and preview for a caller-chosen root.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.git-panel",
        name: "Git Panel",
        repository: "https://github.com/bitty-terminal/git-panel.git",
        pin: "54fc557fd5b24e70a426ff056f14457f57ffacf4",
        manifest_hash: "sha256:0ecd711c566742305ff6b6d24bb28838270c6bda307f2f741fb525f76c1b78e7",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Read-only branch, status, diff, and log panel for the Git repository behind the focused pane.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.history",
        name: "History",
        repository: "https://github.com/bitty-terminal/history.git",
        pin: "dcded6c19408682dcdd6dc6dc6951c667c5b3cd6",
        manifest_hash: "sha256:f4263dd2af80e8f252b86ca2b2c9a052ced87fa646675d6cac3596e3cde333c5",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Opt-in history policy over the public history-read host APIs: scoped bounded snapshot queries over transcript, commands, and own KV with redacted truncated attributed answers or typed denials.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.palette",
        name: "Palette",
        repository: "https://github.com/bitty-terminal/palette.git",
        pin: "33998726bf6cf9c47bf9c9f82d68627db8f18c7e",
        manifest_hash: "sha256:fa679b873524fe7c0fdd1ee3e8851ec9fc5ae29ee3244f27b0908ea63592094c",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Command palette and picker UI via overlay slot, declarative primitives only.",
        license: Some("MIT"),
    },
    OfficialPlugin {
        id: "bitty-terminal.search",
        name: "Search",
        repository: "https://github.com/bitty-terminal/search.git",
        pin: "f3016ef2431ba08f042745501c198070ae52f2d2",
        manifest_hash: "sha256:0e9203fcd2371db170199173515d07ee7a3aa22f1c805d4d2f213abb73e25a09",
        compat_bitty: Some(">=0.0.24,<0.1"),
        compat_plugin_api: Some("^1.0"),
        description: "Scrollback search policy over the public history snapshot API: bounded scoped transcript queries with typed denials, untrusted labels, result navigation, and grant-gated export.",
        license: Some("MIT"),
    },
];
