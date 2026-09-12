//! `bitty inspect`: user-facing state and ownership (CTX-0173).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`inspect` introspection
//! section) as refined by `cli-contract-rfc.md`
//! (`bitty inspect`, mixed class, output envelope v1, exit codes 0-8).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty inspect <target> <value> [--format table|json|jsonl]
//!   [--no-color]` where `<target>` is one of
//!   `command|key|plugin|config|protocol` (singular; plural accepted as
//!   convenience, same result).
//! - Targets (task-required, one per `cli.md` example):
//!   - `command <id>`: registry entry (core or bundled-plugin command) with
//!     kind, class, required scopes, owner, and summary. Accepts both the dot
//!     form (`core.terminal.text`) and the colon form
//!     (`bitty-terminal.palette:toggle`); `:` and `.` compare equivalently.
//!   - `key <chord>`: shipped keymap resolution for a chord such as
//!     `ctrl+shift+m`. Bound chords name the owning action, context, and
//!     layer; unbound chords succeed with `bound: false` and name the
//!     single-owner fallback (unbound keys reach the PTY/shell).
//!   - `plugin <id>`: static bundled-catalog entry (no VM loaded) with owner
//!     (publisher prefix), version, commands, and staged-disabled state.
//!   - `config <dotted-key>`: built-in default value with its owning layer
//!     (`default`) plus the `CLI > file > profile > defaults` precedence note.
//!     Effective file values are shown by `bitty config check`; this surface
//!     never reads user files so it stays safe-mode clean.
//!   - `protocol <name>`: core protocol support state (supported, stub, or
//!     unsupported) with owner and detail.
//! - Class: local-only (no instance, no IPC, no plugin VM loaded, safe-mode
//!   clean). `--socket`/`--instance` never apply and fail closed at dispatch
//!   (exit 2) when combined with `inspect`.
//! - `--format table` (default) is human output, not a machine contract.
//!   `--format json` / `--format jsonl` emit the versioned envelope (`v: 1`,
//!   `command: "inspect"`, `ok`, `result`, plus `error` on failure) on
//!   stdout; diagnostics go to stderr so JSON is never corrupted.
//! - `--` anywhere in `inspect` mode is `UsageError` (exit 2): stray
//!   separators are never silently ignored.
//! - `bitty -- inspect ...` runs a program named `inspect` (escape hatch); the
//!   word `inspect` as first positional is always this subcommand. A program
//!   literally named `inspect` needs `bitty run -- inspect ...`.
//! - `--help` never requires an instance and never loads a plugin VM.
//!
//! # Read-only surface (no new authority)
//!
//! - Commands reuse the `ctl` registry ids (`core.*`) plus the `list`
//!   command ids (`core.list.*`) and the static bundled-plugin manifest
//!   commands (same source as `bitty list plugins`). No VM is loaded.
//! - Keys reuse `bitty-config::keymap::default_keymaps` (shipped defaults).
//! - Plugins reuse `bitty-plugin-host::bundled` static catalog (staged but
//!   disabled by default). No VM is loaded.
//! - Config reuses `bitty-config::EffectiveConfig::default()` (built-in
//!   defaults only; precedence documented, file layers owned by
//!   `bitty config check`).
//! - Protocols reuse the honest support states reported by `bitty doctor`
//!   (kitty-graphics stub, sixel unsupported) plus the `bitty-rich` OSC
//!   surfaces (hyperlink OSC 8, shell integration OSC 133) and the OSC 52
//!   clipboard policy gate.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (including unbound-key explanations with `bound: false`).
//! - `2` usage error (missing/unknown target, missing value, extra positional,
//!   unknown flag, bad `--format`, stray `--`, `--socket`/`--instance` with
//!   `inspect`, malformed key chord).
//! - `1` generic not-found (well-formed value with no entry: unknown command,
//!   plugin, config key, or protocol; `ok: false` envelope for json/jsonl).
//!
//! # Bounds (fail closed with exit 2 before any lookup)
//!
//! - Target token: 1..=`MAX_INSPECT_TARGET_LEN` bytes, no NUL.
//! - Value token: 1..=`MAX_INSPECT_VALUE_LEN` bytes, no NUL.
//! - `--format` value: at most `MAX_INSPECT_FORMAT_LEN` bytes.

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success (including unbound-key explanations).
pub const EXIT_OK: i32 = 0;
/// Generic failure: well-formed value with no entry (`NotFound`).
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum bytes for a target token (`command|key|plugin|config|protocol`).
pub const MAX_INSPECT_TARGET_LEN: usize = 32;
/// Maximum bytes for a value token (command ids are `<= 128`).
pub const MAX_INSPECT_VALUE_LEN: usize = 256;
/// Maximum bytes for a `--format` value.
pub const MAX_INSPECT_FORMAT_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Target and format
// ---------------------------------------------------------------------------

/// Inspect target (`bitty inspect <target> <value>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectTarget {
    /// Registry entry (core or plugin command).
    Command,
    /// Keymap chord resolution.
    Key,
    /// Static plugin-catalog entry.
    Plugin,
    /// Built-in default config value.
    Config,
    /// Core protocol support state.
    Protocol,
}

impl InspectTarget {
    /// Parse a target token (case-insensitive, plural accepted).
    /// Returns `None` for unknown targets (caller maps to exit 2).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_INSPECT_TARGET_LEN || raw.contains('\0') {
            return None;
        }
        match raw.trim().to_ascii_lowercase().as_str() {
            "command" | "commands" | "cmd" => Some(Self::Command),
            "key" | "keys" | "keymap" | "chord" => Some(Self::Key),
            "plugin" | "plugins" => Some(Self::Plugin),
            "config" | "cfg" | "setting" => Some(Self::Config),
            "protocol" | "protocols" | "proto" => Some(Self::Protocol),
            _ => None,
        }
    }

    /// Canonical singular name used in output `result.target`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Key => "key",
            Self::Plugin => "plugin",
            Self::Config => "config",
            Self::Protocol => "protocol",
        }
    }

    /// `NotFound` error code for this target (json envelope).
    #[must_use]
    pub fn not_found_code(self) -> &'static str {
        match self {
            Self::Command => "CommandNotFound",
            Self::Key => "KeyNotFound",
            Self::Plugin => "PluginNotFound",
            Self::Config => "ConfigNotFound",
            Self::Protocol => "ProtocolNotFound",
        }
    }
}

/// Output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectFormat {
    /// Human table (not a machine contract).
    Table,
    /// Single versioned JSON envelope.
    Json,
    /// Same envelope, single line.
    Jsonl,
}

impl InspectFormat {
    /// Parse `--format` (`None` means table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(v) => {
                if v.len() > MAX_INSPECT_FORMAT_LEN || v.contains('\0') {
                    return Err(format!(
                        "bitty inspect: unknown --format {v:?} (want table|json|jsonl)"
                    ));
                }
                match v.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty inspect: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Owned request (parsed in main.rs, validated here)
// ---------------------------------------------------------------------------

/// Validated `bitty inspect` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectRequest {
    /// Inspect target.
    pub target: InspectTarget,
    /// Raw value token (command id, chord, plugin id, config key, protocol).
    pub value: String,
    /// Output shape.
    pub format: InspectFormat,
    /// Disable ANSI coloring in table output (tables are plain; accepted for
    /// script parity with `list`/`doctor`).
    pub no_color: bool,
}

impl InspectRequest {
    /// Validate raw fields into a request. All failures are usage errors
    /// (exit 2, stderr only, no stdout envelope).
    pub fn validate(
        target_raw: Option<&str>,
        value_raw: Option<&str>,
        format_raw: Option<&str>,
        no_color: bool,
    ) -> Result<Self, String> {
        let target_token = target_raw.ok_or_else(|| {
            format!(
                "{}\n{}",
                "bitty inspect: missing <target> (want command|key|plugin|config|protocol)",
                inspect_usage()
            )
        })?;
        let target = InspectTarget::parse(target_token).ok_or_else(|| {
            format!(
                "bitty inspect: unknown target {target_token:?} (want command|key|plugin|config|protocol)\n{}",
                inspect_usage()
            )
        })?;
        let value_token = value_raw.ok_or_else(|| {
            format!(
                "bitty inspect: missing <value> for target {:?} (e.g. {})\n{}",
                target.name(),
                example_for(target),
                inspect_usage()
            )
        })?;
        if value_token.is_empty()
            || value_token.len() > MAX_INSPECT_VALUE_LEN
            || value_token.contains('\0')
        {
            return Err(format!(
                "bitty inspect: <value> must be 1..={MAX_INSPECT_VALUE_LEN} bytes without NUL\n{}",
                inspect_usage()
            ));
        }
        // Malformed key chords fail closed here (exit 2): the chord grammar
        // is a CLI shape, not a registry lookup.
        // MSRV 1.85: no let-chains; nest instead of `if ... && let ...`.
        if target == InspectTarget::Key {
            if let Err(err) = bitty_config::keymap::Chord::parse(value_token) {
                return Err(format!(
                    "bitty inspect: invalid key chord {value_token:?}: {err}\n{}",
                    inspect_usage()
                ));
            }
        }
        let format =
            InspectFormat::parse(format_raw).map_err(|m| format!("{m}\n{}", inspect_usage()))?;
        Ok(Self {
            target,
            value: value_token.to_string(),
            format,
            no_color,
        })
    }
}

/// Example value per target for usage diagnostics.
fn example_for(target: InspectTarget) -> &'static str {
    match target {
        InspectTarget::Command => "`bitty inspect command core.terminal.text`",
        InspectTarget::Key => "`bitty inspect key ctrl+shift+m`",
        InspectTarget::Plugin => "`bitty inspect plugin bitty-terminal.workspace`",
        InspectTarget::Config => "`bitty inspect config font.size`",
        InspectTarget::Protocol => "`bitty inspect protocol kitty-graphics`",
    }
}

// ---------------------------------------------------------------------------
// Usage and help
// ---------------------------------------------------------------------------

/// Short usage for stderr (fail-closed exit 2 trailer).
#[must_use]
pub fn inspect_usage() -> String {
    "usage: bitty inspect <command|key|plugin|config|protocol> <value> [--format table|json|jsonl] [--no-color]\n\ntargets:\n  command <id>    registry entry: core.terminal.text | bitty-terminal.palette:toggle\n  key <chord>     keymap owner: ctrl+shift+m (unbound chords reach the shell)\n  plugin <id>     static catalog entry: bitty-terminal.workspace (no VM loaded)\n  config <key>    built-in default: font.size (effective file values: bitty config check)\n  protocol <name> core support state: kitty-graphics".to_string()
}

/// Full help for `bitty inspect --help` (stdout, exit 0).
#[must_use]
pub fn inspect_help_text() -> String {
    "bitty inspect — explain effective state and ownership (local, safe-mode clean)\n\
     \n\
     Usage: bitty inspect <command|key|plugin|config|protocol> <value> [--format table|json|jsonl] [--no-color]\n\
     \n\
      Targets (each value incl. missing-value errors; unknown values are NotFound, exit 1):\n  \
        command <id>    Registry entry with kind, class, scopes, owner, summary.\n  \
                        Dot form (core.terminal.text) and colon form\n  \
                        (bitty-terminal.palette:toggle) compare equivalently.\n  \
                        Core ids come from the ctl/list registry surface; plugin\n  \
                        commands come from static manifests (no VM loaded).\n  \
                        Workspace commands list both bitty-terminal.workspace:*\n  \
                        and deprecated bitty-terminal.tabs:* aliases.\n  \
        key <chord>     Shipped keymap owner for a chord (e.g. ctrl+shift+m).\n  \
                        Bound chords name action, context, and layer; unbound\n  \
                        chords succeed with bound:false (single-owner rule:\n  \
                        unbound keys reach the PTY/shell, never chrome).\n  \
        plugin <id>     Static bundled-catalog entry (no VM): owner publisher,\n  \
                        version, commands, staged-disabled state.\n  \
                        Canonical id is bitty-terminal.workspace; the old\n  \
                        bitty-terminal.tabs id still resolves with a deprecation\n  \
                        note (removal >= v0.2.0). A bitty workspace is a tab\n  \
                        group within a window (wezterm inverts this: workspace\n  \
                        > window > tab > pane).\n  \
       config <key>    Built-in default value and owning layer (default) plus\n  \
                       the CLI > file > profile > defaults precedence note.\n  \
                       Effective file values: `bitty config check`.\n  \
       protocol <name> Core support state: supported | stub | unsupported |\n  \
                       policy-gated, with owner and detail.\n\
     \n\
     Options:\n  \
       --format SHAPE  table (default, human, not a contract) | json | jsonl (envelope v1)\n  \
       --no-color      Accepted for script parity (tables are plain text).\n  \
       -h, --help      Print this help and exit (never needs an instance or VM).\n\
     \n\
     Output contract:\n  \
       Stdout carries the result; stderr carries diagnostics. JSON/JSONL use\n  \
       envelope {\"v\":1,\"command\":\"inspect\",\"ok\":true,\"result\":{\"target\":...}}.\n  \
       Usage errors (exit 2) go to stderr with no stdout envelope. Well-formed\n  \
       but unknown values emit ok:false envelopes (exit 1, class NotFound).\n\
     \n\
     Exit codes:\n  \
       0 success (unbound keys included, bound:false)\n  \
       1 generic NotFound (unknown command, plugin, config key, or protocol)\n  \
       2 usage error (missing/unknown target, missing value, extra arg, bad --format, stray --)\n\
     \n\
      Examples:\n  \
        bitty inspect command core.terminal.text\n  \
        bitty inspect key ctrl+shift+m\n  \
        bitty inspect plugin bitty-terminal.workspace\n  \
        bitty inspect config font.size\n  \
        bitty inspect protocol kitty-graphics\n  \
        bitty inspect command bitty-terminal.palette:toggle --format json\n"
    .to_string()
}

// ---------------------------------------------------------------------------
// Command registry (static core surface + bundled plugin commands)
// ---------------------------------------------------------------------------

/// One registry entry surfaced by `inspect command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    /// Stable registry id (`core.terminal.text`, `core.list.themes`,
    /// `bitty-terminal.palette:toggle`).
    pub id: &'static str,
    /// Entry kind (`command`; all surfaced entries are commands today).
    pub kind: &'static str,
    /// Entry class (`local`, `runtime`, or `extension`).
    pub class: &'static str,
    /// Required scopes (empty means no scope beyond local access).
    pub scopes: &'static [&'static str],
    /// One-line summary.
    pub summary: &'static str,
    /// Owning service or publisher (`core` or a plugin id).
    pub owner: &'static str,
}

/// Static core command surface: the 11 `ctl` executables plus the 3 `list`
/// executables. Summaries mirror the `ctl --help` and `list --help` texts so
/// the three surfaces can never disagree silently.
pub const CORE_COMMANDS: &[CommandInfo] = &[
    CommandInfo {
        id: "core.instance.list",
        kind: "command",
        class: "local",
        scopes: &[],
        summary: "List live instances via socket discovery (no content fetch).",
        owner: "core",
    },
    CommandInfo {
        id: "core.window.list",
        kind: "command",
        class: "runtime",
        scopes: &["view.inspect"],
        summary: "List windows of the selected instance.",
        owner: "core",
    },
    CommandInfo {
        id: "core.view.list",
        kind: "command",
        class: "runtime",
        scopes: &["view.inspect"],
        summary: "List views of the selected instance.",
        owner: "core",
    },
    CommandInfo {
        id: "core.terminal.list",
        kind: "command",
        class: "runtime",
        scopes: &["terminal.inspect"],
        summary: "List terminals of the selected instance.",
        owner: "core",
    },
    CommandInfo {
        id: "core.terminal.spawn",
        kind: "command",
        class: "runtime",
        scopes: &["terminal.manage"],
        summary: "Spawn a terminal (explicit elevation).",
        owner: "core",
    },
    CommandInfo {
        id: "core.terminal.close",
        kind: "command",
        class: "runtime",
        scopes: &["terminal.manage"],
        summary: "Close a terminal by id (explicit elevation).",
        owner: "core",
    },
    CommandInfo {
        id: "core.terminal.send",
        kind: "command",
        class: "runtime",
        scopes: &["terminal.input"],
        summary: "Send input text to the focused terminal (untrusted bytes).",
        owner: "core",
    },
    CommandInfo {
        id: "core.terminal.text",
        kind: "command",
        class: "runtime",
        scopes: &["terminal.inspect"],
        summary: "Read terminal text (untrusted observation data).",
        owner: "core",
    },
    CommandInfo {
        id: "core.view.split",
        kind: "command",
        class: "runtime",
        scopes: &["view.manage"],
        summary: "Split the focused view in a direction.",
        owner: "core",
    },
    CommandInfo {
        id: "core.view.focus",
        kind: "command",
        class: "runtime",
        scopes: &["view.manage"],
        summary: "Focus a view by id.",
        owner: "core",
    },
    CommandInfo {
        id: "core.config.reload",
        kind: "command",
        class: "runtime",
        scopes: &["config.modify"],
        summary: "Reload configuration in the live instance (explicit elevation).",
        owner: "core",
    },
    CommandInfo {
        id: "core.list.themes",
        kind: "command",
        class: "local",
        scopes: &[],
        summary: "Enumerate built-in theme presets (local).",
        owner: "core",
    },
    CommandInfo {
        id: "core.list.plugins",
        kind: "command",
        class: "local",
        scopes: &[],
        summary: "Enumerate the static bundled plugin catalog (local, no VM).",
        owner: "core",
    },
    CommandInfo {
        id: "core.list.instances",
        kind: "command",
        class: "runtime",
        scopes: &[],
        summary: "Enumerate live instances via socket discovery (no content fetch).",
        owner: "core",
    },
];

/// Normalize a registry id for comparison: `:` and `.` compare equivalently,
/// surrounding whitespace ignored. Case is preserved (registry ids are
/// lowercase; callers lowercase plugin queries separately where needed).
fn normalize_command_id(raw: &str) -> String {
    raw.trim().replace(':', ".")
}

/// Look up a core command by id (dot/colon equivalent, case-sensitive).
#[must_use]
pub fn lookup_core_command(query: &str) -> Option<&'static CommandInfo> {
    let want = normalize_command_id(query);
    CORE_COMMANDS
        .iter()
        .find(|c| normalize_command_id(c.id) == want)
}

/// One plugin-provided command from static manifests (no VM loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandInfo {
    /// Qualified command id as declared (`publisher.name:command`).
    pub id: String,
    /// Owning plugin id (`publisher.name`).
    pub owner: String,
}

/// Collect plugin commands from the static bundled catalog, sorted by id.
#[must_use]
pub fn list_plugin_commands() -> Vec<PluginCommandInfo> {
    let mut out = Vec::new();
    for manifest in bitty_plugin_host::bundled::all_bundled_manifests() {
        let owner = manifest.id().to_string();
        for cmd in &manifest.lazy.commands {
            out.push(PluginCommandInfo {
                id: cmd.as_str().to_string(),
                owner: owner.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Look up a plugin command by id (dot/colon equivalent, case-sensitive).
#[must_use]
pub fn lookup_plugin_command(query: &str) -> Option<PluginCommandInfo> {
    let want = normalize_command_id(query);
    list_plugin_commands()
        .into_iter()
        .find(|c| normalize_command_id(&c.id) == want)
}

// ---------------------------------------------------------------------------
// Key inspection (shipped keymap resolution)
// ---------------------------------------------------------------------------

/// Key-chord resolution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// Canonical chord (`ctrl+shift+m`).
    pub canonical: String,
    /// Whether the chord is bound in the shipped defaults.
    pub bound: bool,
    /// Owning action canonical name (e.g. `copy_to_clipboard`).
    pub action: Option<String>,
    /// Keymap context (always `global` today).
    pub context: Option<String>,
    /// Owning layer (`default` for shipped entries).
    pub owner: String,
}

/// Resolve a chord against the shipped defaults.
///
/// The query was already validated by [`InspectRequest::validate`]
/// (`Chord::parse` succeeds); a parse failure here maps to unbound rather
/// than panicking (defense in depth for direct API callers).
#[must_use]
pub fn inspect_key(query: &str) -> KeyInfo {
    let parsed = bitty_config::keymap::Chord::parse(query);
    let canonical = parsed
        .as_ref()
        .map_or_else(|_| query.trim().to_string(), |chord| chord.canonical());
    let defaults = bitty_config::keymap::default_keymaps().unwrap_or_default();
    if let Ok(want) = parsed {
        for entry in &defaults {
            if entry.chord.canonical() == want.canonical() {
                return KeyInfo {
                    canonical: entry.chord.canonical(),
                    bound: true,
                    action: Some(entry.action.canonical()),
                    context: Some(entry.context.clone()),
                    owner: if entry.from_default {
                        "default".to_string()
                    } else {
                        "user".to_string()
                    },
                };
            }
        }
    }
    KeyInfo {
        canonical,
        bound: false,
        action: None,
        context: None,
        owner: "pty".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Plugin inspection (static catalog, no VM)
// ---------------------------------------------------------------------------

/// Static plugin entry surfaced by `inspect plugin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedPlugin {
    /// Fully qualified id (`bitty-terminal.workspace`; old `bitty-terminal.tabs` still resolves).
    pub id: String,
    /// Human name.
    pub name: String,
    /// Manifest version.
    pub version: String,
    /// One-line description.
    pub description: String,
    /// Whether this id is in the bundled catalog (always true here).
    pub bundled: bool,
    /// Whether enabled (always false: bundled is staged disabled by default).
    pub enabled: bool,
    /// Owning publisher (id prefix before the first `.`).
    pub owner: String,
    /// Qualified commands (`plugin-id:command`) from the static manifest.
    pub commands: Vec<String>,
}

/// Look up a plugin by id (case-insensitive, trimmed).
///
/// Accepts both the canonical `bitty-terminal.workspace` and the deprecated
/// `bitty-terminal.tabs` alias (removal ≥ v0.2.0) via
/// `bundled::bundled_manifest_for`. Old path resolves to the tabs shim with
/// the same commands/claims; pair with `bundled::deprecated_alias_warning`
/// to surface the deprecation.
#[must_use]
pub fn inspect_plugin(query: &str) -> Option<InspectedPlugin> {
    let want = query.trim().to_ascii_lowercase();
    // Alias-aware lookup: canonical list holds workspace, but old id still resolves.
    // `bundled_manifest_for` is exact-case; also try lowercased for CLI case-insensitivity.
    let manifest = bitty_plugin_host::bundled::bundled_manifest_for(query.trim())
        .or_else(|| bitty_plugin_host::bundled::bundled_manifest_for(&want))
        .or_else(|| {
            bitty_plugin_host::bundled::all_bundled_manifests()
                .into_iter()
                .find(|m| m.id().to_string().to_ascii_lowercase() == want)
        })?;
    let id = manifest.id().to_string();
    let owner = id.split('.').next().unwrap_or(&id).to_string();
    let mut commands: Vec<String> = manifest
        .lazy
        .commands
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    commands.sort();
    Some(InspectedPlugin {
        id: id.clone(),
        name: manifest.identity.name.clone(),
        version: manifest.identity.version.clone(),
        description: manifest.identity.description.clone(),
        bundled: true,
        enabled: false,
        owner,
        commands,
    })
}

// ---------------------------------------------------------------------------
// Config inspection (built-in defaults, safe-mode clean)
// ---------------------------------------------------------------------------

/// Built-in default config value surfaced by `inspect config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInfo {
    /// Canonical dotted key (`font.size`).
    pub key: String,
    /// Default value rendered for humans and JSON.
    pub value: String,
    /// Owning layer (always `default`: no file is read here).
    pub source: String,
    /// Owner of the config system.
    pub owner: String,
}

/// Look up a built-in default config value (case-insensitive, trimmed).
///
/// Accepted keys are the dotted `EffectiveConfig` fields plus the `theme`
/// alias for `appearance.theme`. Unknown keys return `None` (caller maps to
/// `NotFound`, exit 1). Values come from `EffectiveConfig::default()` so the
/// defaults single-source with startup; file/profile/CLI layers are owned by
/// `bitty config check` (named in the detail, never read here).
#[must_use]
pub fn inspect_config(query: &str) -> Option<ConfigInfo> {
    let key = query.trim().to_ascii_lowercase();
    let canonical = match key.as_str() {
        "theme" => "appearance.theme",
        _ => key.as_str(),
    };
    let defaults = bitty_config::EffectiveConfig::default();
    let value = match canonical {
        "font.family" => defaults.font.family.clone(),
        "font.size" => render_f32(defaults.font.size),
        "font.line_height" => render_f32(defaults.font.line_height),
        "font.letter_spacing" => render_f32(defaults.font.letter_spacing),
        "appearance.theme" => defaults
            .appearance
            .theme
            .clone()
            .unwrap_or_else(|| "(unset)".to_string()),
        "window.opacity" => render_f32(defaults.window.opacity),
        "window.padding" => defaults.window.padding.to_string(),
        "terminal.scrollback" => defaults.terminal.scrollback.to_string(),
        "terminal.scroll_lines_per_notch" => defaults.terminal.scroll_lines_per_notch.to_string(),
        "terminal.scroll_pixels_per_notch" => defaults.terminal.scroll_pixels_per_notch.to_string(),
        "terminal.shell" => defaults
            .terminal
            .shell
            .clone()
            .unwrap_or_else(|| "(unset: $SHELL or /bin/sh)".to_string()),
        "selection.auto_copy" => defaults.selection.auto_copy.to_string(),
        "layout.gaps_in" => defaults.layout.gaps_in.to_string(),
        "layout.gaps_out" => defaults.layout.gaps_out.to_string(),
        // CTX-0333: Core-owned decoration knobs, including the content inset.
        "decoration.gaps_in" => defaults.decoration.gaps_in.to_string(),
        "decoration.gaps_out" => defaults.decoration.gaps_out.to_string(),
        "decoration.border" => defaults.decoration.border.to_string(),
        "decoration.radius" => defaults.decoration.radius.to_string(),
        "decoration.content_inset" => defaults.decoration.content_inset.to_string(),
        // CTX-0344: focus/idle outline widths inherit `decoration.border`.
        "decoration.border_width" => defaults.decoration.border.to_string(),
        "decoration.border_width_focused" => defaults.decoration.border.to_string(),
        "decoration.border_width_idle" => defaults.decoration.border.to_string(),
        // CTX-0341 (RFC-0002): resolved panel animation contract defaults.
        "appearance.animations.enabled" => defaults.animations.enabled.to_string(),
        "appearance.animations.reduced_motion" => {
            defaults.animations.reduced_motion.as_str().to_string()
        }
        "appearance.animations.duration_ms.open" => {
            defaults.animations.duration_ms.open.to_string()
        }
        "appearance.animations.duration_ms.close" => {
            defaults.animations.duration_ms.close.to_string()
        }
        "appearance.animations.duration_ms.focus" => {
            defaults.animations.duration_ms.focus.to_string()
        }
        "appearance.animations.duration_ms.workspace" => {
            defaults.animations.duration_ms.workspace.to_string()
        }
        "appearance.animations.easing.open" => defaults.animations.easing.open.as_str().to_string(),
        "appearance.animations.easing.close" => {
            defaults.animations.easing.close.as_str().to_string()
        }
        "appearance.animations.easing.focus" => {
            defaults.animations.easing.focus.as_str().to_string()
        }
        "appearance.animations.easing.workspace" => {
            defaults.animations.easing.workspace.as_str().to_string()
        }
        "scrollbar.mode" => defaults.scrollbar.mode.as_str().to_string(),
        "scrollbar.width" => defaults.scrollbar.width.to_string(),
        // CTX-0260: hover-focus opt-in (default off = click-to-focus).
        "mouse.focus_follows_mouse" => defaults.mouse.focus_follows_mouse.to_string(),
        // CTX-0334: hover-activation dwell delay in milliseconds.
        "mouse.focus_follows_mouse_delay_ms" => {
            defaults.mouse.focus_follows_mouse_delay_ms.to_string()
        }
        // CTX-0236: leader/mod for the shipped chrome map (default Alt).
        "mod_key" => defaults.mod_key.canonical().to_string(),
        _ => return None,
    };
    Some(ConfigInfo {
        key: canonical.to_string(),
        value,
        source: "default".to_string(),
        owner: "core (bitty-config)".to_string(),
    })
}

/// Render an `f32` default without trailing noise (`12.0` stays `12`).
fn render_f32(value: f32) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

// ---------------------------------------------------------------------------
// Protocol inspection (core support states)
// ---------------------------------------------------------------------------

/// Core protocol support state surfaced by `inspect protocol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolInfo {
    /// Canonical protocol name (`kitty-graphics`).
    pub name: String,
    /// Support state: `supported` | `stub` | `unsupported` | `policy-gated`.
    pub status: String,
    /// Owner of the implementation (`core`).
    pub owner: String,
    /// Honest detail (what works today, what is deferred).
    pub detail: String,
}

/// Static protocol catalog (single source for table and JSON).
const PROTOCOLS: &[(&str, &str, &str)] = &[
    (
        "kitty-graphics",
        "stub",
        "Bounded placeholder stub only (bitty-rich KittyGraphicsStub): escape intake parses, no raster present. Inline images arrive in a later slice.",
    ),
    (
        "sixel",
        "unsupported",
        "Sixel is unsupported: sequences are ignored, text and fallbacks keep working.",
    ),
    (
        "hyperlink",
        "supported",
        "OSC 8 hyperlinks render via bitty-rich spans with a scheme allowlist; untrusted targets never execute.",
    ),
    (
        "shell-integration",
        "supported",
        "OSC 133 shell-integration zones record via bitty-rich; exit codes and regions are observation data.",
    ),
    (
        "clipboard",
        "policy-gated",
        "OSC 52 clipboard reads/writes go through the clipboard policy and the suspicious-paste inspection gate; no silent delivery.",
    ),
];

/// Look up a protocol by name (case-insensitive) with short aliases.
#[must_use]
pub fn inspect_protocol(query: &str) -> Option<ProtocolInfo> {
    let want = query.trim().to_ascii_lowercase();
    let canonical = match want.as_str() {
        "kitty" | "kitty_graphics" | "kitty-graphics" | "kittygraphics" => "kitty-graphics",
        "sixel" => "sixel",
        "hyperlink" | "hyperlinks" | "osc-8" | "osc8" => "hyperlink",
        "shell-integration" | "shell_integration" | "shell" | "osc-133" | "osc133" => {
            "shell-integration"
        }
        "clipboard" | "osc-52" | "osc52" => "clipboard",
        _ => want.as_str(),
    };
    PROTOCOLS
        .iter()
        .find(|(name, _, _)| *name == canonical)
        .map(|(name, status, detail)| ProtocolInfo {
            name: (*name).to_string(),
            status: (*status).to_string(),
            owner: "core".to_string(),
            detail: (*detail).to_string(),
        })
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in JSON output (control bytes safe).
#[must_use]
pub fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

use std::fmt::Write as _;

/// Render the success envelope for a resolved inspection.
#[must_use]
pub fn format_success_envelope(_request: &InspectRequest, result_json: &str) -> String {
    format!("{{\"v\":1,\"command\":\"inspect\",\"ok\":true,\"result\":{result_json}}}")
}

/// Render the failure envelope for a well-formed but unknown value.
#[must_use]
pub fn format_error_envelope(request: &InspectRequest, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"inspect\",\"ok\":false,\"error\":{{\"class\":\"NotFound\",\"code\":\"{}\",\"message\":\"{}\"}},\"result\":{{\"target\":\"{}\",\"query\":\"{}\"}}}}",
        request.target.not_found_code(),
        json_escape(message),
        request.target.name(),
        json_escape(&request.value),
    )
}

/// Result object for a core command entry.
#[must_use]
pub fn result_command_json(query: &str, info: &CommandInfo) -> String {
    let mut scopes = String::from("[");
    for (i, scope) in info.scopes.iter().enumerate() {
        if i > 0 {
            scopes.push(',');
        }
        let _ = write!(scopes, "\"{}\"", json_escape(scope));
    }
    scopes.push(']');
    format!(
        "{{\"target\":\"command\",\"query\":\"{}\",\"id\":\"{}\",\"kind\":\"{}\",\"class\":\"{}\",\"scopes\":{scopes},\"owner\":\"{}\",\"summary\":\"{}\"}}",
        json_escape(query),
        json_escape(info.id),
        json_escape(info.kind),
        json_escape(info.class),
        json_escape(info.owner),
        json_escape(info.summary),
    )
}

/// Result object for a plugin-provided command entry.
#[must_use]
pub fn result_plugin_command_json(query: &str, info: &PluginCommandInfo) -> String {
    format!(
        "{{\"target\":\"command\",\"query\":\"{}\",\"id\":\"{}\",\"kind\":\"command\",\"class\":\"extension\",\"scopes\":[],\"owner\":\"{}\",\"summary\":\"Plugin command from static manifest (no VM loaded; invocation scope checked by the plugin host at runtime).\"}}",
        json_escape(query),
        json_escape(&info.id),
        json_escape(&info.owner),
    )
}

/// Result object for a key resolution.
#[must_use]
pub fn result_key_json(query: &str, info: &KeyInfo) -> String {
    let action = info
        .action
        .as_deref()
        .map_or_else(|| "null".to_string(), |a| format!("\"{}\"", json_escape(a)));
    let context = info
        .context
        .as_deref()
        .map_or_else(|| "null".to_string(), |c| format!("\"{}\"", json_escape(c)));
    format!(
        "{{\"target\":\"key\",\"query\":\"{}\",\"canonical\":\"{}\",\"bound\":{},\"action\":{action},\"context\":{context},\"owner\":\"{}\"}}",
        json_escape(query),
        json_escape(&info.canonical),
        info.bound,
        json_escape(&info.owner),
    )
}

/// Result object for a plugin entry.
#[must_use]
pub fn result_plugin_json(query: &str, plugin: &InspectedPlugin) -> String {
    let mut commands = String::from("[");
    for (i, cmd) in plugin.commands.iter().enumerate() {
        if i > 0 {
            commands.push(',');
        }
        let _ = write!(commands, "\"{}\"", json_escape(cmd));
    }
    commands.push(']');
    format!(
        "{{\"target\":\"plugin\",\"query\":\"{}\",\"id\":\"{}\",\"name\":\"{}\",\"version\":\"{}\",\"description\":\"{}\",\"bundled\":{},\"enabled\":{},\"owner\":\"{}\",\"commands\":{commands}}}",
        json_escape(query),
        json_escape(&plugin.id),
        json_escape(&plugin.name),
        json_escape(&plugin.version),
        json_escape(&plugin.description),
        plugin.bundled,
        plugin.enabled,
        json_escape(&plugin.owner),
    )
}

/// Result object for a config value.
#[must_use]
pub fn result_config_json(query: &str, info: &ConfigInfo) -> String {
    format!(
        "{{\"target\":\"config\",\"query\":\"{}\",\"key\":\"{}\",\"value\":\"{}\",\"source\":\"{}\",\"owner\":\"{}\",\"precedence\":\"CLI > file > profile > defaults\"}}",
        json_escape(query),
        json_escape(&info.key),
        json_escape(&info.value),
        json_escape(&info.source),
        json_escape(&info.owner),
    )
}

/// Result object for a protocol entry.
#[must_use]
pub fn result_protocol_json(query: &str, info: &ProtocolInfo) -> String {
    format!(
        "{{\"target\":\"protocol\",\"query\":\"{}\",\"name\":\"{}\",\"status\":\"{}\",\"owner\":\"{}\",\"detail\":\"{}\"}}",
        json_escape(query),
        json_escape(&info.name),
        json_escape(&info.status),
        json_escape(&info.owner),
        json_escape(&info.detail),
    )
}

// ---------------------------------------------------------------------------
// Table rendering (human, not a machine contract)
// ---------------------------------------------------------------------------

/// Render a core command entry as a human table.
#[must_use]
pub fn format_command_table(query: &str, info: &CommandInfo) -> String {
    let scopes = if info.scopes.is_empty() {
        "(none: local)".to_string()
    } else {
        info.scopes.join(", ")
    };
    format!(
        "command {}\n  id: {}\n  kind: {}\n  class: {}\n  scopes: {}\n  owner: {}\n  summary: {}\n",
        query, info.id, info.kind, info.class, scopes, info.owner, info.summary
    )
}

/// Render a plugin-provided command entry as a human table.
#[must_use]
pub fn format_plugin_command_table(query: &str, info: &PluginCommandInfo) -> String {
    format!(
        "command {}\n  id: {}\n  kind: command\n  class: extension\n  scopes: (none declared in static manifest)\n  owner: {}\n  summary: Plugin command from static manifest (no VM loaded).\n",
        query, info.id, info.owner
    )
}

/// Render a key resolution as a human table.
#[must_use]
pub fn format_key_table(query: &str, info: &KeyInfo) -> String {
    if info.bound {
        format!(
            "key {}\n  canonical: {}\n  bound: yes\n  action: {}\n  context: {}\n  owner: {} (shipped defaults; user overrides via config keymaps)\n",
            query,
            info.canonical,
            info.action.as_deref().unwrap_or("(none)"),
            info.context.as_deref().unwrap_or("(none)"),
            info.owner
        )
    } else {
        format!(
            "key {}\n  canonical: {}\n  bound: no\n  owner: pty (unbound keys reach the shell; single-owner rule)\n",
            query, info.canonical
        )
    }
}

/// Render a plugin entry as a human table.
#[must_use]
pub fn format_plugin_table(query: &str, plugin: &InspectedPlugin) -> String {
    let commands = if plugin.commands.is_empty() {
        "-".to_string()
    } else {
        plugin.commands.join(", ")
    };
    let base = format!(
        "plugin {}\n  id: {}\n  name: {}\n  version: {}\n  description: {}\n  bundled: {}\n  enabled: {}\n  owner: {}\n  commands: {}\n",
        query,
        plugin.id,
        plugin.name,
        plugin.version,
        plugin.description,
        if plugin.bundled { "yes" } else { "no" },
        if plugin.enabled { "yes" } else { "no" },
        plugin.owner,
        commands
    );
    if bitty_plugin_host::bundled::is_deprecated_bundled_alias(query.trim())
        || bitty_plugin_host::bundled::is_deprecated_bundled_alias(&plugin.id)
    {
        format!("{base}  note: deprecated alias for bitty-terminal.workspace (removal >= v0.2.0)\n")
    } else {
        base
    }
}

/// Render a config value as a human table.
#[must_use]
pub fn format_config_table(query: &str, info: &ConfigInfo) -> String {
    format!(
        "config {}\n  key: {}\n  value: {}\n  source: {} (built-in default; effective file values: `bitty config check`)\n  owner: {}\n  precedence: CLI > file > profile > defaults\n",
        query, info.key, info.value, info.source, info.owner
    )
}

/// Render a protocol entry as a human table.
#[must_use]
pub fn format_protocol_table(query: &str, info: &ProtocolInfo) -> String {
    format!(
        "protocol {}\n  name: {}\n  status: {}\n  owner: {}\n  detail: {}\n",
        query, info.name, info.status, info.owner, info.detail
    )
}

// ---------------------------------------------------------------------------
// Dispatch (called by main.rs; returns process exit code)
// ---------------------------------------------------------------------------

/// Run `bitty inspect` from a validated request; prints to stdout,
/// diagnostics to stderr, and returns the process exit code.
///
/// - Every target resolves locally (no instance, no IPC, no VM).
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "inspect"`) on stdout with diagnostics on stderr.
/// - Well-formed but unknown values emit `ok: false` envelopes for json/jsonl
///   (exit 1) and stderr-only diagnostics for table.
pub fn run_inspect(request: &InspectRequest) -> i32 {
    let emit_json = matches!(request.format, InspectFormat::Json | InspectFormat::Jsonl);
    match request.target {
        InspectTarget::Command => {
            if let Some(info) = lookup_core_command(&request.value) {
                match request.format {
                    InspectFormat::Table => {
                        print!("{}", format_command_table(&request.value, info));
                    }
                    InspectFormat::Json | InspectFormat::Jsonl => {
                        println!(
                            "{}",
                            format_success_envelope(
                                request,
                                &result_command_json(&request.value, info)
                            )
                        );
                    }
                }
                return EXIT_OK;
            }
            if let Some(info) = lookup_plugin_command(&request.value) {
                match request.format {
                    InspectFormat::Table => {
                        print!("{}", format_plugin_command_table(&request.value, &info));
                    }
                    InspectFormat::Json | InspectFormat::Jsonl => {
                        println!(
                            "{}",
                            format_success_envelope(
                                request,
                                &result_plugin_command_json(&request.value, &info)
                            )
                        );
                    }
                }
                return EXIT_OK;
            }
            let message = format!(
                "bitty inspect: unknown command {:?} (core ids include {}; plugin commands come from static manifests)",
                request.value,
                core_command_ids_hint(),
            );
            if emit_json {
                println!("{}", format_error_envelope(request, &message));
                eprintln!("{message}");
            } else {
                eprintln!("{message}");
            }
            EXIT_GENERIC
        }
        InspectTarget::Key => {
            let info = inspect_key(&request.value);
            match request.format {
                InspectFormat::Table => {
                    print!("{}", format_key_table(&request.value, &info));
                }
                InspectFormat::Json | InspectFormat::Jsonl => {
                    println!(
                        "{}",
                        format_success_envelope(request, &result_key_json(&request.value, &info))
                    );
                }
            }
            EXIT_OK
        }
        InspectTarget::Plugin => match inspect_plugin(&request.value) {
            Some(plugin) => {
                match request.format {
                    InspectFormat::Table => {
                        print!("{}", format_plugin_table(&request.value, &plugin));
                    }
                    InspectFormat::Json | InspectFormat::Jsonl => {
                        println!(
                            "{}",
                            format_success_envelope(
                                request,
                                &result_plugin_json(&request.value, &plugin)
                            )
                        );
                    }
                }
                EXIT_OK
            }
            None => {
                let message = format!(
                    "bitty inspect: unknown plugin {:?} (bundled ids: {})",
                    request.value,
                    bundled_plugin_ids_hint(),
                );
                if emit_json {
                    println!("{}", format_error_envelope(request, &message));
                    eprintln!("{message}");
                } else {
                    eprintln!("{message}");
                }
                EXIT_GENERIC
            }
        },
        InspectTarget::Config => match inspect_config(&request.value) {
            Some(info) => {
                match request.format {
                    InspectFormat::Table => {
                        print!("{}", format_config_table(&request.value, &info));
                    }
                    InspectFormat::Json | InspectFormat::Jsonl => {
                        println!(
                            "{}",
                            format_success_envelope(
                                request,
                                &result_config_json(&request.value, &info)
                            )
                        );
                    }
                }
                EXIT_OK
            }
            None => {
                let message = format!(
                    "bitty inspect: unknown config key {:?} (try font.size, font.family, appearance.theme, window.opacity, terminal.scrollback, selection.auto_copy, layout.gaps_in, decoration.content_inset, scrollbar.mode, mouse.focus_follows_mouse, mouse.focus_follows_mouse_delay_ms, appearance.animations.enabled, appearance.animations.reduced_motion, appearance.animations.duration_ms.open, appearance.animations.easing.open, mod_key)",
                    request.value,
                );
                if emit_json {
                    println!("{}", format_error_envelope(request, &message));
                    eprintln!("{message}");
                } else {
                    eprintln!("{message}");
                }
                EXIT_GENERIC
            }
        },
        InspectTarget::Protocol => match inspect_protocol(&request.value) {
            Some(info) => {
                match request.format {
                    InspectFormat::Table => {
                        print!("{}", format_protocol_table(&request.value, &info));
                    }
                    InspectFormat::Json | InspectFormat::Jsonl => {
                        println!(
                            "{}",
                            format_success_envelope(
                                request,
                                &result_protocol_json(&request.value, &info)
                            )
                        );
                    }
                }
                EXIT_OK
            }
            None => {
                let message = format!(
                    "bitty inspect: unknown protocol {:?} (try kitty-graphics, sixel, hyperlink, shell-integration, clipboard)",
                    request.value,
                );
                if emit_json {
                    println!("{}", format_error_envelope(request, &message));
                    eprintln!("{message}");
                } else {
                    eprintln!("{message}");
                }
                EXIT_GENERIC
            }
        },
    }
}

/// Short hint of core command ids for not-found diagnostics (bounded).
fn core_command_ids_hint() -> String {
    let mut out = String::new();
    for (i, cmd) in CORE_COMMANDS.iter().take(6).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(cmd.id);
    }
    out.push_str(", ... ");
    out
}

/// Short hint of bundled plugin ids for not-found diagnostics (bounded).
fn bundled_plugin_ids_hint() -> String {
    let mut ids: Vec<String> = bitty_plugin_host::bundled::all_bundled_manifests()
        .iter()
        .map(|m| m.id().to_string())
        .collect();
    ids.sort();
    let mut out = String::new();
    for (i, id) in ids.iter().take(5).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(id);
    }
    if ids.len() > 5 {
        out.push_str(", ...");
    }
    out
}

// ---------------------------------------------------------------------------
// `bitty inspect` CLI entry point (relocated from `main.rs`, CTX-0305)
// ---------------------------------------------------------------------------

use crate::cli::Args;

/// Runs `bitty inspect <target> <value>`; returns the process exit code.
///
/// - Extra positionals, unknown targets, missing values, bad `--format`,
///   stray `--`, and `--socket`/`--instance` alongside `inspect` fail closed
///   (exit 2, stderr only, no stdout envelope). `inspect` is local-only: no
///   targeting flag ever applies.
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "inspect"`) on stdout with diagnostics on stderr.
/// - Well-formed but unknown values emit `ok: false` envelopes for json/jsonl
///   (exit 1, class `NotFound`) and stderr-only diagnostics for table.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if !args.inspect_args.is_empty() {
        eprintln!(
            "bitty inspect: unexpected argument '{}'\n{}",
            args.inspect_args[0],
            inspect_usage()
        );
        return EXIT_USAGE;
    }
    // Local-only: targeting flags never apply to `inspect` (no instance is
    // contacted). Fail closed rather than silently ignoring them.
    if args.ctl_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_socket.is_some()
        || args.list_instance.is_some()
    {
        eprintln!(
            "bitty inspect: --socket/--instance do not apply (inspect is local, no instance)\n{}",
            inspect_usage()
        );
        return EXIT_USAGE;
    }
    let request = match InspectRequest::validate(
        args.inspect_target.as_deref(),
        args.inspect_value.as_deref(),
        args.inspect_format.as_deref(),
        args.inspect_no_color,
    ) {
        Ok(req) => req,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };
    run_inspect(&request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_singular_plural_and_aliases() {
        assert_eq!(
            InspectTarget::parse("command"),
            Some(InspectTarget::Command)
        );
        assert_eq!(
            InspectTarget::parse("Commands"),
            Some(InspectTarget::Command)
        );
        assert_eq!(InspectTarget::parse("cmd"), Some(InspectTarget::Command));
        assert_eq!(InspectTarget::parse("key"), Some(InspectTarget::Key));
        assert_eq!(InspectTarget::parse("KEYS"), Some(InspectTarget::Key));
        assert_eq!(InspectTarget::parse("plugin"), Some(InspectTarget::Plugin));
        assert_eq!(InspectTarget::parse("plugins"), Some(InspectTarget::Plugin));
        assert_eq!(InspectTarget::parse("config"), Some(InspectTarget::Config));
        assert_eq!(InspectTarget::parse("cfg"), Some(InspectTarget::Config));
        assert_eq!(
            InspectTarget::parse("protocol"),
            Some(InspectTarget::Protocol)
        );
        assert_eq!(
            InspectTarget::parse("protocols"),
            Some(InspectTarget::Protocol)
        );
        assert_eq!(InspectTarget::parse("fonts"), None);
        assert_eq!(InspectTarget::parse(""), None);
        assert_eq!(InspectTarget::parse("command;rm"), None);
    }

    #[test]
    fn format_parses_with_table_default() {
        assert_eq!(InspectFormat::parse(None).unwrap(), InspectFormat::Table);
        assert_eq!(
            InspectFormat::parse(Some("json")).unwrap(),
            InspectFormat::Json
        );
        assert_eq!(
            InspectFormat::parse(Some("JSONL")).unwrap(),
            InspectFormat::Jsonl
        );
        assert!(InspectFormat::parse(Some("yaml")).is_err());
    }

    #[test]
    fn request_rejects_missing_and_bad_shapes() {
        assert!(InspectRequest::validate(None, None, None, false).is_err());
        assert!(InspectRequest::validate(Some("nope"), Some("x"), None, false).is_err());
        assert!(InspectRequest::validate(Some("command"), None, None, false).is_err());
        assert!(InspectRequest::validate(Some("command"), Some(""), None, false).is_err());
        assert!(
            InspectRequest::validate(Some("command"), Some("core.view.list"), Some("yaml"), false)
                .is_err()
        );
        // Malformed chord is a usage error (exit 2 at dispatch).
        assert!(InspectRequest::validate(Some("key"), Some(""), None, false).is_err());
    }

    #[test]
    fn command_lookup_covers_core_and_colon_form() {
        let core = lookup_core_command("core.terminal.text").expect("core id");
        assert_eq!(core.owner, "core");
        assert_eq!(core.class, "runtime");
        assert!(core.scopes.contains(&"terminal.inspect"));
        // Colon/dot equivalence for core ids.
        assert!(lookup_core_command("core.terminal:text").is_some());
        assert!(lookup_core_command("core.nope.nope").is_none());
        // Plugin commands resolve from static manifests (no VM).
        let plugin_cmds = list_plugin_commands();
        assert!(!plugin_cmds.is_empty());
        let first = plugin_cmds[0].id.clone();
        assert!(lookup_plugin_command(&first).is_some());
        // Unknown command is a clean miss (NotFound at dispatch, not a panic).
        assert!(lookup_plugin_command("nope.nope:nope").is_none());
    }

    #[test]
    fn key_bound_and_unbound_shapes() {
        // A shipped default chord resolves with an owner action.
        let bound = inspect_key("alt+h");
        assert!(bound.bound);
        assert!(bound.action.is_some());
        assert_eq!(bound.owner, "default");
        // Case-insensitive canonicalization.
        let upper = inspect_key("Alt+H");
        assert_eq!(upper.canonical, bound.canonical);
        // Well-formed but unbound chords succeed with bound:false (PTY owner).
        let free = inspect_key("ctrl+shift+m");
        if free.bound {
            assert!(free.action.is_some());
        } else {
            assert_eq!(free.owner, "pty");
            assert_eq!(free.action, None);
        }
    }

    #[test]
    fn plugin_lookup_is_case_insensitive() {
        let plugin = inspect_plugin("bitty-terminal.workspace").expect("bundled workspace");
        assert_eq!(plugin.owner, "bitty-terminal");
        assert!(plugin.bundled);
        assert!(!plugin.enabled);
        assert!(!plugin.commands.is_empty());
        // Canonical lists both new and deprecated old commands.
        assert!(
            plugin
                .commands
                .iter()
                .any(|c| c == "bitty-terminal.workspace:new")
        );
        assert!(
            plugin
                .commands
                .iter()
                .any(|c| c == "bitty-terminal.tabs:new")
        );
        assert!(plugin.id == "bitty-terminal.workspace");
        let upper = inspect_plugin("BITTY-TERMINAL.WORKSPACE").expect("case-insensitive");
        assert_eq!(upper.id, plugin.id);
        // Deprecated alias still resolves.
        let old = inspect_plugin("bitty-terminal.tabs").expect("deprecated alias resolves");
        assert_eq!(old.commands, plugin.commands);
        let old_upper = inspect_plugin("BITTY-TERMINAL.TABS").expect("alias case-insensitive");
        assert_eq!(old_upper.id, "bitty-terminal.tabs");
        assert!(inspect_plugin("nope.nope").is_none());
    }

    #[test]
    fn config_lookup_covers_defaults_and_alias() {
        let size = inspect_config("font.size").expect("font.size");
        assert_eq!(size.key, "font.size");
        assert_eq!(size.source, "default");
        assert!(!size.value.is_empty());
        let theme = inspect_config("theme").expect("theme alias");
        assert_eq!(theme.key, "appearance.theme");
        let dotted = inspect_config("appearance.theme").expect("dotted theme");
        assert_eq!(dotted.key, "appearance.theme");
        // CTX-0236: the leader/mod default is inspectable like every scalar.
        let mod_key = inspect_config("mod_key").expect("mod_key");
        assert_eq!(mod_key.key, "mod_key");
        assert_eq!(mod_key.value, "alt");
        // CTX-0260: hover-focus default (off) is inspectable.
        let hover = inspect_config("mouse.focus_follows_mouse").expect("mouse key");
        assert_eq!(hover.key, "mouse.focus_follows_mouse");
        assert_eq!(hover.value, "false");
        // CTX-0334: the hover-activation dwell delay (0 ms) is inspectable.
        let hover_delay =
            inspect_config("mouse.focus_follows_mouse_delay_ms").expect("mouse delay key");
        assert_eq!(hover_delay.key, "mouse.focus_follows_mouse_delay_ms");
        assert_eq!(hover_delay.value, "0");
        assert!(inspect_config("font.nope").is_none());
        assert!(inspect_config("").is_none());
    }

    #[test]
    fn protocol_lookup_covers_aliases() {
        let kitty = inspect_protocol("kitty-graphics").expect("kitty");
        assert_eq!(kitty.status, "stub");
        assert_eq!(kitty.owner, "core");
        assert!(inspect_protocol("kitty").is_some());
        assert_eq!(
            inspect_protocol("sixel").expect("sixel").status,
            "unsupported"
        );
        assert_eq!(inspect_protocol("osc-8").expect("osc-8").name, "hyperlink");
        assert!(inspect_protocol("nope").is_none());
    }

    #[test]
    fn envelopes_are_machine_readable() {
        let req = InspectRequest::validate(
            Some("command"),
            Some("core.terminal.text"),
            Some("json"),
            false,
        )
        .unwrap();
        let info = lookup_core_command("core.terminal.text").unwrap();
        let env = format_success_envelope(&req, &result_command_json("core.terminal.text", info));
        assert!(env.contains("\"v\":1"));
        assert!(env.contains("\"command\":\"inspect\""));
        assert!(env.contains("\"ok\":true"));
        assert!(env.contains("core.terminal.text"));
        assert!(env.contains("terminal.inspect"));
        let key_req =
            InspectRequest::validate(Some("key"), Some("alt+h"), Some("json"), false).unwrap();
        let key_env =
            format_success_envelope(&key_req, &result_key_json("alt+h", &inspect_key("alt+h")));
        assert!(key_env.contains("\"target\":\"key\""));
        assert!(key_env.contains("\"bound\":true"));
        let missing =
            InspectRequest::validate(Some("plugin"), Some("nope.nope"), Some("json"), false)
                .unwrap();
        let err = format_error_envelope(&missing, "bitty inspect: unknown plugin \"nope.nope\"");
        assert!(err.contains("\"ok\":false"));
        assert!(err.contains("\"class\":\"NotFound\""));
        assert!(err.contains("PluginNotFound"));
    }

    #[test]
    fn json_escapes_control_bytes() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\ny"), "x\\ny");
        assert_eq!(json_escape("\u{0}"), "\\u0000");
    }

    #[test]
    fn tables_name_owner_and_state() {
        let info = lookup_core_command("core.view.split").unwrap();
        let table = format_command_table("core.view.split", info);
        assert!(table.contains("core.view.split"));
        assert!(table.contains("view.manage"));
        assert!(table.contains("core"));
        let key_table = format_key_table("alt+h", &inspect_key("alt+h"));
        assert!(key_table.contains("bound: yes"));
        let unbound = inspect_key("ctrl+shift+f9");
        if !unbound.bound {
            let table = format_key_table("ctrl+shift+f9", &unbound);
            assert!(table.contains("bound: no"));
            assert!(table.contains("pty"));
        }
        let plugin = inspect_plugin("bitty-terminal.workspace").unwrap();
        let table = format_plugin_table("bitty-terminal.workspace", &plugin);
        assert!(table.contains("bitty-terminal.workspace"));
        assert!(table.contains("bitty-terminal"));
        // Deprecated alias shows a removal note; canonical does not.
        let old = inspect_plugin("bitty-terminal.tabs").unwrap();
        let old_table = format_plugin_table("bitty-terminal.tabs", &old);
        assert!(old_table.contains("bitty-terminal.tabs"));
        assert!(old_table.contains("deprecated alias"));
        assert!(!table.contains("deprecated alias"));
        let config = inspect_config("font.size").unwrap();
        let table = format_config_table("font.size", &config);
        assert!(table.contains("font.size"));
        assert!(table.contains("default"));
        let proto = inspect_protocol("kitty-graphics").unwrap();
        let table = format_protocol_table("kitty-graphics", &proto);
        assert!(table.contains("kitty-graphics"));
        assert!(table.contains("stub"));
    }
}
