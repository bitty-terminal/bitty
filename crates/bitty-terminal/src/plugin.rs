//! `bitty plugin`: CLI-first plugin management (CTX-0150, issue #244).
//!
//! Canonical direction: `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/product/plugin-roadmap.md` owner
//! direction 2026-09-03 (DEC-0007): management UX is subcommand-first and the
//! durable state is a managed manifest, never hand-edited Lua. Candidate
//! layout and semantics from
//! `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/extensibility/package-management.md` and
//! `default-distribution-rfc.md` § "Managed manifest".
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty plugin list|install|remove|enable|disable|revoke|info`
//!   with `list`/`info` accepting `--format table|json|jsonl` (default
//!   table) and `--no-color` (accepted; tables are plain text).
//! - `install <id>` resolves a **bundled** plugin manifest, pins its
//!   `PluginManifest::manifest_hash()`, and grants the requested capability
//!   set only after explicit consent (`--yes`, or an interactive `[y/N]`
//!   prompt that fails closed on EOF). A capability increase on an existing
//!   record blocks until approved; unchanged or narrowed sets carry forward
//!   silently (the P0-AC-030 pattern over `bitty-plugin-host` grants).
//! - `install <path>` installs (or updates) an **external** package from a
//!   local directory into the XDG data store
//!   (`$XDG_DATA_HOME/bitty/plugins/packages/<id>/<version>/`): manifest,
//!   schema, entry point, module-tree bounds, and `compat` ranges are
//!   verified before anything is staged, added capabilities need the same
//!   consent, and the atomic `current.json` pointer is switched last. The
//!   runtime then discovers and activates the package as a visibly
//!   unverified third-party (`local-path`) source. `install <git-url>`
//!   clones a `https://<host>/<path>.git`, `git+https://…`, or
//!   `git@<host>:<path>` source with fixed-argv system `git` into an
//!   exclusive temp staging dir and then runs the same local-path pipeline
//!   unchanged. `install <archive>` unpacks a local `.tar.gz`/`.tgz`/
//!   `.tar.zst`/`.zip` file with fixed-argv system `tar`/`unzip` into the
//!   same kind of staging and then runs the same pipeline. Unsupported
//!   schemes (`file://`, `http://`, `ssh://`, …) fail closed with a naming
//!   diagnostic.
//! - `remove <id> --force` is destructive: bundled records lose their managed
//!   entry with `bitty-plugins.toml.bak` kept; installed packages delete their
//!   store record and staged tree. `enable`/`disable` are idempotent toggles
//!   in the managed manifest or the store index respectively; `enable`
//!   re-checks the hash pin and the grant coverage and fails closed on a
//!   mismatch. `revoke <id> [--cap <capability>]` drops grants without
//!   dropping the record: a full revoke clears the granted set and disables,
//!   a `--cap` revoke drops one capability and remembers it as an explicit
//!   denial (no silent re-prompt); re-grant goes through `install` consent.
//! - `list` shows every bundled plugin plus any recorded or installed extra,
//!   with source, state (`enabled`/`disabled`/`available`), pin status, and
//!   granted/requested capability counts. `info` shows one plugin's manifest
//!   plus the recorded grants and their plain-language effect statements.
//! - `install` and `enable` success prints post-install next steps (CTX-1268,
//!   issue #2144): the enable command, where the plugin landed, the
//!   per-plugin config file path with a minimal snippet derived from the
//!   manifest's declared `[settings]` defaults, manifest-provided `[hints]`
//!   lines when declared, and the `info` pointer. Display-only: no grants,
//!   no state, no plugin code runs.
//! - Class: local-only (no instance, no IPC, **no plugin VM and no plugin
//!   code is ever loaded or executed**; safe-mode clean).
//!
//! # Durable state
//!
//! Exactly one machine-managed manifest lives beside the config file:
//! `$XDG_CONFIG_HOME/bitty/bitty-plugins.toml` (fallback `~/.config/...`),
//! or the directory of an explicit `--config`/`BITTY_CONFIG` path. The
//! format is a strict, bounded TOML **subset** owned by this module: unknown
//! sections, unknown keys, duplicate keys, malformed values, over-limit
//! files, and unknown version numbers all fail closed before any mutation.
//! Only this module writes it; `bitty-config`'s Lua `init.lua` is never
//! rewritten (package-management.md: arbitrary Lua rewrite is unsafe).
//!
//! # Authority
//!
//! - Every capability identifier is validated through
//!   `bitty-plugin-host`'s closed capability grammar
//!   ([`CapabilityId`]); filesystem requests expand to
//!   `fs.read:PARAM`/`fs.write:PARAM` exactly as the host does.
//! - Nothing is granted implicitly: the recorded grant set is exactly the
//!   manifest's requested set at approval time, bound to the manifest hash.
//! - No ambient authority is created by the CLI: it cannot load plugins, it
//!   cannot widen a grant without a fresh consent, and it never executes
//!   package code (P0-AC-027).
//!
//! # Exit codes (stable taxonomy, cli-contract-rfc.md)
//!
//! - `0` success (including idempotent no-op toggles).
//! - `1` aborted consent (declined/EOF), filesystem failure after approval,
//!   missing fetch helper (`git`/`tar`/`unzip`), `git clone` failure, or
//!   archive unpack failure (nothing staged).
//! - `2` usage error (missing/unknown verb, missing `<id>`, unknown flag,
//!   bad `--format`, stray `--`, malformed plugin id, destructive `remove`
//!   without `--force`, `--yes`/`--force`/`--format` on the wrong verb).
//! - `4` plugin error (unknown/non-bundled id, invalid or corrupt managed
//!   manifest, pin mismatch, uncovered grant, blocked capability increase,
//!   rejected fetch scheme such as `file://`/`http://`/`ssh://`, malformed
//!   git URL, hostile archive member).
//!
//! # Bounds (fail closed before any filesystem mutation)
//!
//! - Raw tokens: 1..=[`MAX_PLUGIN_TOKEN_BYTES`] bytes, no NUL.
//! - `--format`: 1..=[`MAX_PLUGIN_FORMAT_BYTES`] bytes.
//! - Managed manifest: [`MAX_STATE_FILE_BYTES`] bytes,
//!   [`MAX_STATE_PLUGINS`] records, [`MAX_STATE_GRANTS`] grants per record.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bitty_config::plugin_file::{PLUGIN_FILE_EXTENSION, PLUGINS_DIR_NAME};
use bitty_plugin_host::bundled::{all_bundled_manifests, bundled_manifest_for};
use bitty_plugin_host::capability::{CapabilityId, effect_statement};
use bitty_plugin_host::grant::{GrantRecord as HostGrantRecord, GrantStore};
use bitty_plugin_host::manifest::{PluginId, PluginManifest, SettingDecl};
use bitty_runtime::plugin_runtime::{
    load_index, manifest_toml,
    package::{self, LocalInstallOptions, PackageOpError},
};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success (including idempotent no-ops).
pub const EXIT_OK: i32 = 0;
/// Aborted consent or filesystem failure after approval.
pub const EXIT_GENERIC: i32 = 1;
/// Usage error (`cli-contract-rfc.md` class `UsageError`).
pub const EXIT_USAGE: i32 = 2;
/// Plugin-level failure (unknown id, pin mismatch, blocked increase).
pub const EXIT_PLUGIN: i32 = 4;

/// Maximum bytes for one raw token (verb, id, flag value).
pub const MAX_PLUGIN_TOKEN_BYTES: usize = 256;
/// Maximum bytes for a `--format` value.
pub const MAX_PLUGIN_FORMAT_BYTES: usize = 16;
/// Managed-manifest file name beside `init.lua`.
pub const STATE_FILE_NAME: &str = "bitty-plugins.toml";
/// Suffix appended to the previous managed manifest on every rewrite.
pub const STATE_BACKUP_SUFFIX: &str = ".bak";
/// Managed-manifest format version (this module owns the codec).
pub const STATE_VERSION: u32 = 1;
/// Maximum managed-manifest file size.
pub const MAX_STATE_FILE_BYTES: usize = 64 * 1024;
/// Maximum records in one managed manifest.
pub const MAX_STATE_PLUGINS: usize = 256;
/// Maximum granted capabilities recorded per plugin.
pub const MAX_STATE_GRANTS: usize = 256;
/// Maximum managed-manifest lines.
pub const MAX_STATE_LINES: usize = 4096;
/// Hex digest length (`Sha256`, as produced by `PluginManifest::manifest_hash`).
pub const HEX_HASH_LEN: usize = 64;
/// Maximum bytes read from one consent answer line.
pub const MAX_CONSENT_LINE_BYTES: usize = 64;
/// Consent attempts before aborting (mirrors `bitty init`).
pub const MAX_CONSENT_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Parsed request (headless, bounded, fail-closed)
// ---------------------------------------------------------------------------

/// Output shape for `list`/`info` (`--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormat {
    /// Human table (default).
    Table,
    /// Versioned JSON envelope on stdout.
    Json,
    /// Same envelope, one line (identical for this command).
    Jsonl,
}

impl PluginFormat {
    /// Parse `--format` (`None` means the table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(value) => {
                if value.len() > MAX_PLUGIN_FORMAT_BYTES || value.contains('\0') {
                    return Err(format!(
                        "bitty plugin: unknown --format {value:?} (want table|json|jsonl)"
                    ));
                }
                match value.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty plugin: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

/// One `bitty plugin` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginVerb {
    /// `list`: enumerate bundled + recorded plugins.
    List,
    /// `install <id>`: pin + grant a bundled plugin.
    Install,
    /// `remove <id> --force`: drop a record (backup kept).
    Remove,
    /// `enable <id>`: re-enable a recorded plugin.
    Enable,
    /// `disable <id>`: disable a recorded plugin.
    Disable,
    /// `info <id>`: explain one plugin's static manifest + state.
    Info,
    /// `revoke <id> [--cap <capability>]`: drop grants (durable denial).
    Revoke,
}

impl PluginVerb {
    /// Parse a verb token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "list" => Some(Self::List),
            "install" => Some(Self::Install),
            "remove" => Some(Self::Remove),
            "enable" => Some(Self::Enable),
            "disable" => Some(Self::Disable),
            "info" => Some(Self::Info),
            "revoke" => Some(Self::Revoke),
            _ => None,
        }
    }

    /// Canonical verb name for output/errors.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Info => "info",
            Self::Revoke => "revoke",
        }
    }

    /// Whether the verb takes a plugin id operand.
    #[must_use]
    pub fn needs_id(self) -> bool {
        !matches!(self, Self::List)
    }
}

/// Validated request for one `bitty plugin` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRequest {
    /// Requested verb.
    pub verb: PluginVerb,
    /// Plugin id operand (validated as a token; grammar checked at resolve).
    pub id: Option<String>,
    /// Output shape for `list`/`info`.
    pub format: PluginFormat,
    /// `install --yes`: approve capability consent non-interactively.
    pub yes: bool,
    /// `remove --force`: required for the destructive removal.
    pub force: bool,
    /// `revoke --cap <capability>`: revoke one capability instead of all.
    pub cap: Option<String>,
    /// `--no-color` (accepted for parity; tables are plain text).
    pub no_color: bool,
}

/// Why parsing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginParseError {
    /// `-h`/`--help` was requested.
    Help,
    /// Usage-level failure (message already user-facing).
    Usage(String),
}

impl PluginParseError {
    /// User-facing message (without usage text).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Help => String::new(),
            Self::Usage(message) => message.clone(),
        }
    }
}

fn bare_token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_PLUGIN_TOKEN_BYTES && !token.contains('\0')
}

/// Parse the tokens captured after the `plugin` word (bounded, fail-closed).
///
/// `pre_format` carries a pre-word global `--format` value (mirrors
/// `bitty dev`); an explicit `--format` after the verb wins.
pub fn parse_plugin_request(
    raw: &[String],
    pre_format: Option<&str>,
) -> Result<PluginRequest, PluginParseError> {
    let mut verb: Option<PluginVerb> = None;
    let mut id: Option<String> = None;
    let mut format: Option<String> = None;
    let mut yes = false;
    let mut force = false;
    let mut cap: Option<String> = None;
    let mut no_color = false;

    let mut index = 0usize;
    while index < raw.len() {
        let token = raw[index].as_str();
        match token {
            "-h" | "--help" => return Err(PluginParseError::Help),
            "--yes" => {
                yes = true;
                index += 1;
                continue;
            }
            "--force" => {
                force = true;
                index += 1;
                continue;
            }
            "--cap" => {
                // Pair form: consume the next token as the capability value;
                // a missing value is usage (fail closed before dispatch).
                let Some(value) = raw.get(index + 1) else {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: --cap needs a value (a capability id)".to_string(),
                    ));
                };
                if !bare_token_ok(value) {
                    return Err(PluginParseError::Usage(format!(
                        "bitty plugin: --cap value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                    )));
                }
                if cap.is_some() {
                    return Err(PluginParseError::Usage(
                        "bitty plugin: duplicate --cap".to_string(),
                    ));
                }
                cap = Some(value.clone());
                index += 2;
                continue;
            }
            "--no-color" => {
                no_color = true;
                index += 1;
                continue;
            }
            "--" => {
                return Err(PluginParseError::Usage(
                    "bitty plugin: stray `--` separator (plugin takes no child argv)".to_string(),
                ));
            }
            _ => {}
        }
        if let Some(value) = token.strip_prefix("--format=") {
            format = Some(value.to_string());
            index += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--cap=") {
            if !bare_token_ok(value) {
                return Err(PluginParseError::Usage(format!(
                    "bitty plugin: --cap value is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
                )));
            }
            if cap.is_some() {
                return Err(PluginParseError::Usage(
                    "bitty plugin: duplicate --cap".to_string(),
                ));
            }
            cap = Some(value.to_string());
            index += 1;
            continue;
        }
        if token == "--format" {
            // Pair form: consume the next token as the value; a missing
            // value is usage (fail closed before dispatch).
            let Some(value) = raw.get(index + 1) else {
                return Err(PluginParseError::Usage(
                    "bitty plugin: --format needs a value (table|json|jsonl)".to_string(),
                ));
            };
            format = Some(value.clone());
            index += 2;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(PluginParseError::Usage(format!(
                "bitty plugin: unknown flag {token:?}"
            )));
        }
        if !bare_token_ok(token) {
            return Err(PluginParseError::Usage(format!(
                "bitty plugin: argument is empty, contains NUL, or exceeds {MAX_PLUGIN_TOKEN_BYTES} bytes"
            )));
        }
        if verb.is_none() {
            match PluginVerb::parse(token) {
                Some(parsed) => verb = Some(parsed),
                None => {
                    return Err(PluginParseError::Usage(format!(
                        "bitty plugin: unknown verb {token:?} \
                         (want list|install|remove|enable|disable|info|revoke)"
                    )));
                }
            }
            index += 1;
            continue;
        }
        if id.is_none() {
            id = Some(token.to_string());
            index += 1;
            continue;
        }
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: unexpected extra argument {token:?}"
        )));
    }

    let verb = verb.ok_or_else(|| {
        PluginParseError::Usage(
            "bitty plugin: missing verb (want list|install|remove|enable|disable|info|revoke)"
                .to_string(),
        )
    })?;

    let format = match format {
        Some(value) => PluginFormat::parse(Some(&value)).map_err(PluginParseError::Usage)?,
        None => PluginFormat::parse(pre_format).map_err(PluginParseError::Usage)?,
    };

    if verb.needs_id() && id.is_none() {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: `{}` needs a plugin id (owner.name)",
            verb.name()
        )));
    }
    if !verb.needs_id() && id.is_some() {
        return Err(PluginParseError::Usage(
            "bitty plugin: `list` takes no plugin id".to_string(),
        ));
    }
    if yes && verb != PluginVerb::Install {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --yes only applies to `install` (got `{}`)",
            verb.name()
        )));
    }
    if force && verb != PluginVerb::Remove {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --force only applies to `remove` (got `{}`)",
            verb.name()
        )));
    }
    if cap.is_some() && verb != PluginVerb::Revoke {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --cap only applies to `revoke` (got `{}`)",
            verb.name()
        )));
    }
    if (format != PluginFormat::Table || no_color)
        && !matches!(verb, PluginVerb::List | PluginVerb::Info)
    {
        return Err(PluginParseError::Usage(format!(
            "bitty plugin: --format/--no-color only apply to `list` and `info` (got `{}`)",
            verb.name()
        )));
    }

    Ok(PluginRequest {
        verb,
        id,
        format,
        yes,
        force,
        cap,
        no_color,
    })
}

/// Short usage block (`stderr` on usage failures).
#[must_use]
pub fn plugin_usage() -> String {
    "usage: bitty plugin list [--format table|json|jsonl] [--no-color]\n\
     \x20      bitty plugin install <id> [--yes]\n\
     \x20      bitty plugin install <path> [--yes]   (local directory package)\n\
     \x20      bitty plugin install <git-url> [--yes]   (https://<host>/<path>.git | git+https://... | git@<host>:<path>)\n\
     \x20      bitty plugin install <archive> [--yes]   (.tar.gz/.tgz/.tar.zst/.zip file)\n\
     \x20      bitty plugin remove <id> --force\n\
     \x20      bitty plugin enable <id>\n\
     \x20      bitty plugin disable <id>\n\
     \x20      bitty plugin revoke <id> [--cap <capability>]\n\
     \x20      bitty plugin info <id> [--format table|json|jsonl] [--no-color]\n\
     \n\
     Bundled plugins are recorded in $XDG_CONFIG_HOME/bitty/bitty-plugins.toml\n\
     (fallback ~/.config/bitty/...), or beside an explicit --config path.\n\
     External packages install into $XDG_DATA_HOME/bitty/plugins (fallback\n\
     ~/.local/share/bitty/plugins). No plugin code runs during any operation.\n\
     Exit codes: 0 success | 1 aborted/io (consent, helper, clone, unpack) |\n\
     2 usage | 4 plugin error (id, scheme, validation).\n\
     `bitty plugin --help` explains capabilities, consent, and exit codes."
        .to_string()
}

/// Long help (`bitty plugin --help`).
#[must_use]
pub fn plugin_help_text() -> String {
    "bitty plugin — CLI-first plugin management (local class, no VM)\n\
     \n\
     usage: bitty plugin <verb> [args] [flags]\n\
     \n\
     verbs:\n\
     \x20 list                        Show bundled + installed plugins: source,\n\
     \x20                             state, pin status, granted/requested\n\
     \x20                             capabilities.\n\
     \x20 install <id> [--yes]        Resolve a bundled manifest, pin its hash,\n\
     \x20                             and grant requested capabilities after\n\
     \x20                             explicit consent. --yes approves without a\n\
     \x20                             prompt.\n\
     \x20 install <path> [--yes]      Install or update an external package from\n\
     \x20                             a local directory containing\n\
     \x20                             bitty-plugin.toml (lua/ module root). The\n\
     \x20                             manifest, entry point, bounds, and compat\n\
     \x20                             ranges are verified before anything is\n\
     \x20                             staged; added capabilities need consent.\n\
     \x20 install <git-url> [--yes]  Install from a git source: exactly\n\
     \x20                             https://<host>/<path>.git,\n\
     \x20                             git+https://<host>/<path>.git (the git+\n\
     \x20                             prefix is stripped before cloning), or\n\
     \x20                             git@<host>:<path>. Fixed-argv system\n\
     \x20                             `git clone --filter=blob:none --depth=1\n\
     \x20                             <url> <staging-dir>` into an exclusive\n\
     \x20                             temp dir (caller env inherited for\n\
     \x20                             gitconfig/SSH-agent/proxy/CA), then the\n\
     \x20                             same local-path pipeline. file://,\n\
     \x20                             http://, ssh://, and other schemes fail\n\
     \x20                             closed with a naming diagnostic.\n\
     \x20 install <archive> [--yes]  Install from a local archive file:\n\
     \x20                             .tar.gz/.tgz (tar -xzf), .tar.zst (tar\n\
     \x20                             --use-compress-program=unzstd -xf), or\n\
     \x20                             .zip (unzip -q -d). Fixed-argv system\n\
     \x20                             helpers unpack into an exclusive temp\n\
     \x20                             staging dir (members audited for ../,\n\
     \x20                             absolute paths, and symlinks; declared\n\
     \x20                             unpacked content bounded at 64 MiB\n\
     \x20                             before extraction), then the\n\
     \x20                             same local-path pipeline. A missing\n\
     \x20                             helper or a bad/hostile archive fails\n\
     \x20                             closed with nothing staged.\n\
     \x20 remove <id> --force         Remove a record: bundled records drop from\n\
     \x20                             the managed manifest (previous copy kept as\n\
     \x20                             .bak); installed packages delete their store\n\
     \x20                             record and package tree.\n\
     \x20 enable <id>                 Re-enable a recorded plugin. Bundled pins\n\
     \x20                             and grants are re-checked; installed records\n\
     \x20                             are switched atomically in the store index.\n\
     \x20 disable <id>                Disable without dropping the record or grant.\n\
     \x20 revoke <id> [--cap <cap>]   Revoke grants without dropping the record:\n\
     \x20                             without --cap every granted capability is\n\
     \x20                             dropped and the plugin disabled; with --cap\n\
     \x20                             one capability is dropped and remembered\n\
     \x20                             as explicitly denied (no silent re-prompt).\n\
     \x20                             Re-grant with `install` (explicit consent).\n\
     \x20 info <id>                   Manifest + recorded state, with the plain-\
     \x20                             language effect per capability.\n\
     \n\
     flags:\n\
     \x20 --format table|json|jsonl   list/info output shape (default table).\n\
     \x20 --no-color                  Accepted for parity (tables are plain text).\n\
     \x20 --yes                       install only: approve capability consent.\n\
     \x20 --force                     remove only: confirm the destructive drop.\n\
     \x20 --cap <capability>          revoke only: revoke one capability.\n\
     \n\
     download consent:\n\
     \x20 Capability grants use explicit confirm: --yes approves\n\
     \x20 non-interactively, otherwise the installer prompts\n\
     \x20 `Grant these capabilities? [y/N]` (up to 3 attempts, 64 bytes per\n\
     \x20 answer); `y`/`yes` approves, `n`/`no`/empty declines, EOF aborts.\n\
     \x20 A decline or EOF exits 1 with nothing changed. Unchanged or\n\
     \x20 narrowed capability sets carry forward silently; added\n\
     \x20 capabilities always need fresh consent. Native extension downloads\n\
     \x20 (`bitty extension install`) follow the same levels: the builtin\n\
     \x20 installer egress cdn.bitty.run:443 needs no extra grant, while\n\
     \x20 third-party hosts stay consent-gated and fail closed.\n\
     \n\
     authority:\n\
     \x20 Plugin code is never executed by any `bitty plugin` operation. A\n\
     \x20 capability is granted only when the manifest requests it and consent\n\
     \x20 is explicit; grants are bound to the exact manifest hash, so a later\n\
     \x20 manifest that adds a capability blocks until re-approved. Installed\n\
     \x20 packages come from local directories, git checkouts, or local\n\
     \x20 archives (source class `local-path`), are stored under\n\
     \x20 packages/<id>/<version>/ with an owner-only mode, and load as\n\
     \x20 visibly unverified third-party packages.\n\
     \n\
     wiring (where things land, what to edit next):\n\
     \x20 The managed manifest lives beside the config file:\n\
     \x20 $XDG_CONFIG_HOME/bitty/bitty-plugins.toml (fallback\n\
     \x20 ~/.config/bitty/...; an explicit --config path selects its own\n\
     \x20 directory). Never hand-edit it: it is the grant ledger, and a\n\
     \x20 malformed file fails closed.\n\
     \x20 Per-plugin user files live next to it, one TOML file per plugin:\n\
     \x20 $XDG_CONFIG_HOME/bitty/plugins/<owner>.<name>.toml (flat\n\
     \x20 `key = value` lines only; unknown keys are ignored, bad values\n\
     \x20 fall back to the plugin's shipped defaults). Installed packages\n\
     \x20 land under $XDG_DATA_HOME/bitty/plugins/packages/<id>/<version>/\n\
     \x20 (fallback ~/.local/share/...).\n\
     \x20 After a local `install`, the plugin is staged and disabled; enable it explicitly:\n\
     \x20 bitty plugin enable <id>\n\
     \x20 bitty plugin info <id>     # manifest, grants, and settings\n\
     \n\
     fetch:\n\
     \x20 Git and archive installs stage under an exclusive temp dir and run\n\
     \x20 zero plugin code. `git`/`tar`/`unzip` resolve via PATH (git.exe/\n\
     \x20 tar.exe on Windows); a missing helper fails closed. `unzip` may be\n\
     \x20 absent on Windows: .zip installs then fail closed with a diagnostic.\n\
     \x20 Helpers inherit the system proxy env and the system CA store (no\n\
     \x20 bundled certs, no custom TLS flags in Core).\n\
     \n\
     exit codes:\n\
     \x20 0 success (including idempotent no-ops).\n\
     \x20 1 aborted/io: declined/EOF consent, filesystem failure, missing\n\
     \x20    fetch helper, `git clone` failure, archive unpack failure.\n\
     \x20 2 usage: bad verb/flag/format/id, stray `--`, wrong-verb flags.\n\
     \x20 4 plugin error: unknown id, bad manifest/pin/grant, rejected fetch\n\
     \x20    scheme (file://, http://, ssh://, ...), malformed git URL,\n\
     \x20    hostile archive member.\n\
     \n\
     examples:\n\
     \x20 bitty plugin list\n\
     \x20 bitty plugin list --format json\n\
     \x20 bitty plugin install bitty-terminal.shell-integration --yes\n\
     \x20 bitty plugin install ./my-plugin --yes\n\
     \x20 bitty plugin install https://example.com/my-plugin.git --yes\n\
     \x20 bitty plugin install git+https://example.com/my-plugin.git --yes\n\
     \x20 bitty plugin install git@example.com:my-plugin.git --yes\n\
     \x20 bitty plugin install ./my-plugin.tar.gz --yes\n\
     \x20 bitty plugin install ./my-plugin.zip --yes\n\
     \x20 bitty plugin info xuepoo.hello\n\
     \x20 bitty plugin disable bitty-terminal.shell-integration\n\
     \x20 bitty plugin remove xuepoo.hello --force"
        .to_string()
}

// ---------------------------------------------------------------------------
// Managed manifest (strict bounded TOML subset, owned by this module)
// ---------------------------------------------------------------------------

/// One recorded plugin: source, pinned manifest hash, enabled flag, the
/// granted capability set bound to that hash, and explicitly revoked
/// (denied) capabilities that must not be silently re-prompted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRecord {
    /// Provenance (`bundled` today; closed set).
    pub source: String,
    /// Hex-encoded `PluginManifest::manifest_hash()` this grant is bound to.
    pub manifest_hash: String,
    /// Whether the plugin is enabled in the recorded desired state.
    pub enabled: bool,
    /// Granted capabilities for this exact manifest hash.
    pub granted: BTreeSet<CapabilityId>,
    /// Explicitly revoked capabilities (durable deny-loop guard; only
    /// `install` consent clears entries that are re-granted).
    pub denied: BTreeSet<CapabilityId>,
}

/// Parsed managed manifest (`bitty-plugins.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginState {
    records: BTreeMap<String, PluginRecord>,
}

/// Per-section key accumulators during [`PluginState::parse`].
#[derive(Default)]
struct SectionAccum {
    source: Option<String>,
    manifest_hash: Option<String>,
    enabled: Option<bool>,
    granted: Option<BTreeSet<CapabilityId>>,
    denied: Option<BTreeSet<CapabilityId>>,
}

/// Managed-manifest parse/validation failure (owned message incl. line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError {
    message: String,
}

impl StateError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            message: format!("line {line}: {}", message.into()),
        }
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StateError {}

impl PluginState {
    /// Empty state (no records).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a state from records (test constructor).
    #[cfg(test)]
    #[must_use]
    pub fn from_records(records: BTreeMap<String, PluginRecord>) -> Self {
        Self { records }
    }

    /// Record for `id`, if any.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&PluginRecord> {
        self.records.get(id)
    }

    /// All recorded ids in deterministic (sorted) order.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.records.keys()
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut PluginRecord> {
        self.records.get_mut(id)
    }

    fn insert(&mut self, id: String, record: PluginRecord) {
        self.records.insert(id, record);
    }

    fn remove(&mut self, id: &str) -> Option<PluginRecord> {
        self.records.remove(id)
    }

    /// Parse a managed manifest, fail-closed on anything outside the strict
    /// bounded subset this module writes.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] for over-limit files, unknown/duplicate
    /// sections or keys, malformed values, missing required keys, an
    /// unsupported `state_version`, or invalid plugin/capability identifiers.
    pub fn parse(text: &str) -> Result<Self, StateError> {
        if text.len() > MAX_STATE_FILE_BYTES {
            return Err(StateError::new(
                1,
                format!("managed manifest exceeds {MAX_STATE_FILE_BYTES} bytes"),
            ));
        }
        let mut state = Self::new();
        let mut version_seen = false;
        let mut section_id: Option<String> = None;
        let mut section_line = 0usize;
        let mut accum = SectionAccum::default();

        for (index, raw_line) in text.lines().enumerate() {
            let line = index + 1;
            if line > MAX_STATE_LINES {
                return Err(StateError::new(
                    line,
                    format!("managed manifest exceeds {MAX_STATE_LINES} lines"),
                ));
            }
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(header) = trimmed.strip_prefix('[') {
                let header = header.strip_suffix(']').ok_or_else(|| {
                    StateError::new(line, "section header is missing the closing `]`")
                })?;
                if section_id.is_some() {
                    flush_section(&mut state, section_id.take(), section_line, &mut accum)?;
                }
                let id = parse_section_header(header, line)?;
                if state.records.contains_key(&id) {
                    return Err(StateError::new(
                        line,
                        format!("duplicate section for plugin '{id}'"),
                    ));
                }
                section_id = Some(id);
                section_line = line;
                continue;
            }
            if section_id.is_none() {
                // Only `state_version = 1` is allowed before the first section.
                let (key, value) = split_assignment(trimmed, line)?;
                if key != "state_version" {
                    return Err(StateError::new(
                        line,
                        format!("expected `state_version` before any section, found '{key}'"),
                    ));
                }
                if version_seen {
                    return Err(StateError::new(line, "duplicate `state_version`"));
                }
                if value != STATE_VERSION.to_string() {
                    return Err(StateError::new(
                        line,
                        format!("unsupported state_version '{value}' (want {STATE_VERSION})"),
                    ));
                }
                version_seen = true;
                continue;
            }
            let (key, value) = split_assignment(trimmed, line)?;
            match key {
                "source" => {
                    reject_duplicate(accum.source.is_some(), "source", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if parsed != "bundled" {
                        return Err(StateError::new(
                            line,
                            format!("unsupported source '{parsed}' (want bundled)"),
                        ));
                    }
                    accum.source = Some(parsed);
                }
                "manifest_hash" => {
                    reject_duplicate(accum.manifest_hash.is_some(), "manifest_hash", line)?;
                    let parsed = parse_quoted(value, line)?;
                    if !is_lower_hex(&parsed, HEX_HASH_LEN) {
                        return Err(StateError::new(
                            line,
                            format!(
                                "manifest_hash must be {HEX_HASH_LEN} lowercase hex characters"
                            ),
                        ));
                    }
                    accum.manifest_hash = Some(parsed);
                }
                "enabled" => {
                    reject_duplicate(accum.enabled.is_some(), "enabled", line)?;
                    accum.enabled = Some(match value {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(StateError::new(
                                line,
                                format!("enabled must be true|false, found '{other}'"),
                            ));
                        }
                    });
                }
                "granted" => {
                    reject_duplicate(accum.granted.is_some(), "granted", line)?;
                    accum.granted = Some(parse_capability_array(value, line)?);
                }
                "denied" => {
                    reject_duplicate(accum.denied.is_some(), "denied", line)?;
                    accum.denied = Some(parse_capability_array(value, line)?);
                }
                other => {
                    return Err(StateError::new(
                        line,
                        format!("unknown key '{other}' in plugin section"),
                    ));
                }
            }
        }
        if section_id.is_some() {
            flush_section(&mut state, section_id.take(), section_line, &mut accum)?;
        }
        if !version_seen {
            return Err(StateError::new(1, "missing `state_version = 1`"));
        }
        if state.records.len() > MAX_STATE_PLUGINS {
            return Err(StateError::new(
                1,
                format!("managed manifest exceeds {MAX_STATE_PLUGINS} plugins"),
            ));
        }
        Ok(state)
    }

    /// Render the managed manifest deterministically (sorted records, fixed
    /// field order, sorted grants, trailing newline).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# bitty managed plugin manifest — machine-generated by `bitty plugin`.\n\
             # Do not edit by hand; use bitty plugin list|install|remove|enable|disable|revoke.\n",
        );
        let _ = writeln!(out, "state_version = {STATE_VERSION}");
        for (id, record) in &self.records {
            out.push('\n');
            let _ = writeln!(out, "[plugins.\"{id}\"]");
            let _ = writeln!(out, "source = \"{}\"", record.source);
            let _ = writeln!(out, "manifest_hash = \"{}\"", record.manifest_hash);
            let _ = writeln!(out, "enabled = {}", record.enabled);
            out.push_str("granted = [");
            for (index, capability) in record.granted.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "\"{}\"", capability.as_str());
            }
            out.push_str("]\n");
            out.push_str("denied = [");
            for (index, capability) in record.denied.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "\"{}\"", capability.as_str());
            }
            out.push_str("]\n");
        }
        out
    }
}

fn flush_section(
    state: &mut PluginState,
    id: Option<String>,
    line: usize,
    accum: &mut SectionAccum,
) -> Result<(), StateError> {
    let Some(id) = id else {
        return Ok(());
    };
    let source = accum
        .source
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `source`")))?;
    let manifest_hash = accum.manifest_hash.take().ok_or_else(|| {
        StateError::new(line, format!("plugin '{id}' is missing `manifest_hash`"))
    })?;
    let enabled = accum
        .enabled
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `enabled`")))?;
    let granted = accum
        .granted
        .take()
        .ok_or_else(|| StateError::new(line, format!("plugin '{id}' is missing `granted`")))?;
    if granted.len() > MAX_STATE_GRANTS {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' exceeds {MAX_STATE_GRANTS} granted capabilities"),
        ));
    }
    // `denied` is optional (absent in manifests written before revocation
    // existed); missing means no explicit denials.
    let denied = accum.denied.take().unwrap_or_default();
    if denied.len() > MAX_STATE_GRANTS {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' exceeds {MAX_STATE_GRANTS} denied capabilities"),
        ));
    }
    if !granted.is_disjoint(&denied) {
        return Err(StateError::new(
            line,
            format!("plugin '{id}' lists a capability as both granted and denied"),
        ));
    }
    state.insert(
        id,
        PluginRecord {
            source,
            manifest_hash,
            enabled,
            granted,
            denied,
        },
    );
    Ok(())
}

fn parse_section_header(header: &str, line: usize) -> Result<String, StateError> {
    let header = header.trim();
    let inner = header
        .strip_prefix("plugins.")
        .ok_or_else(|| StateError::new(line, "expected section `[plugins.\"<id>\"]`"))?;
    let id = parse_quoted(inner, line)?;
    PluginId::new(&id)
        .map_err(|error| StateError::new(line, format!("invalid plugin id '{id}': {error}")))?;
    Ok(id)
}

fn split_assignment(line: &str, line_no: usize) -> Result<(&str, &str), StateError> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| StateError::new(line_no, "expected `key = value`"))?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(StateError::new(line_no, "expected `key = value`"));
    }
    Ok((key, value))
}

fn reject_duplicate(seen: bool, key: &str, line: usize) -> Result<(), StateError> {
    if seen {
        return Err(StateError::new(line, format!("duplicate key '{key}'")));
    }
    Ok(())
}

fn parse_quoted(value: &str, line: usize) -> Result<String, StateError> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| StateError::new(line, "expected a double-quoted value"))?;
    if inner.contains(['"', '\\']) || inner.chars().any(char::is_control) {
        return Err(StateError::new(
            line,
            "quoted value must not contain quotes, escapes, or control characters",
        ));
    }
    Ok(inner.to_string())
}

fn parse_capability_array(value: &str, line: usize) -> Result<BTreeSet<CapabilityId>, StateError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| StateError::new(line, "granted must be an array of quoted capabilities"))?;
    let inner = inner.trim();
    let mut out = BTreeSet::new();
    if inner.is_empty() {
        return Ok(out);
    }
    for element in inner.split(',') {
        let element = element.trim();
        let capability_text = parse_quoted(element, line)?;
        let capability = CapabilityId::parse(&capability_text).map_err(|error| {
            StateError::new(
                line,
                format!("invalid capability '{capability_text}': {error}"),
            )
        })?;
        if !out.insert(capability) {
            return Err(StateError::new(
                line,
                format!("duplicate capability '{capability_text}'"),
            ));
        }
        if out.len() > MAX_STATE_GRANTS {
            return Err(StateError::new(
                line,
                format!("granted exceeds {MAX_STATE_GRANTS} capabilities"),
            ));
        }
    }
    Ok(out)
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// Managed-manifest location and IO
// ---------------------------------------------------------------------------

/// One dispatch failure with its stable exit code and envelope class.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginFailure {
    exit: i32,
    class: &'static str,
    code: &'static str,
    message: String,
}

impl PluginFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_USAGE,
            class: "UsageError",
            code: "Usage",
            message: message.into(),
        }
    }

    fn plugin(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_PLUGIN,
            class: "PluginError",
            code,
            message: message.into(),
        }
    }

    fn generic(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_GENERIC,
            class: "Internal",
            code,
            message: message.into(),
        }
    }
}

/// Resolve the managed-manifest path.
///
/// - An explicit `--config`/`BITTY_CONFIG` path selects its parent directory.
/// - Otherwise `$XDG_CONFIG_HOME/bitty`, falling back to `~/.config/bitty`.
fn resolve_state_path(
    config_path: Option<&str>,
    bitty_config_env: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, PluginFailure> {
    if let Some(explicit) =
        bitty_config::file::resolve_config_explicit(config_path, bitty_config_env)
    {
        let candidate = Path::new(&explicit);
        // An explicit directory is the config root; a file path selects its
        // parent directory (mirrors `bitty init`'s `--config` semantics).
        let dir = if candidate.is_dir() {
            candidate
        } else {
            candidate
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| {
                    PluginFailure::usage(format!(
                        "bitty plugin: cannot derive a config directory from --config '{explicit}'"
                    ))
                })?
        };
        return Ok(dir.join(STATE_FILE_NAME));
    }
    bitty_config::file::config_dir_with_env(xdg_config_home, home)
        .map(|dir| dir.join(STATE_FILE_NAME))
        .ok_or_else(|| {
            PluginFailure::usage(
                "bitty plugin: no config root ($XDG_CONFIG_HOME or $HOME unset)".to_string(),
            )
        })
}

fn load_state(path: &Path) -> Result<PluginState, PluginFailure> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginState::new());
        }
        Err(error) => {
            return Err(PluginFailure::generic(
                "IoError",
                format!("bitty plugin: cannot read '{}': {error}", path.display()),
            ));
        }
    };
    if bytes.len() > MAX_STATE_FILE_BYTES {
        return Err(PluginFailure::plugin(
            "StateInvalid",
            format!(
                "bitty plugin: '{}' exceeds {MAX_STATE_FILE_BYTES} bytes",
                path.display()
            ),
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        PluginFailure::plugin(
            "StateInvalid",
            format!("bitty plugin: '{}' is not UTF-8: {error}", path.display()),
        )
    })?;
    PluginState::parse(text).map_err(|error| {
        PluginFailure::plugin(
            "StateInvalid",
            format!(
                "bitty plugin: invalid managed manifest '{}': {error}",
                path.display()
            ),
        )
    })
}

/// Owner-only mode for the managed manifest and its `.bak` (Unix, CTX-0293).
#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

/// Owner-only mode for the directory holding the managed manifest (CTX-0293).
#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;

/// Force owner-only permissions on the managed manifest or its `.bak`.
///
/// The manifest is a capability-grant ledger; `std::fs::write` inherits the
/// process umask, so the mode is set explicitly (P0-AC-026 defense in depth,
/// CTX-0293). Non-Unix targets have no equivalent mode bits here and no-op.
#[cfg(unix)]
fn harden_managed_file(path: &Path) -> Result<(), PluginFailure> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(STATE_FILE_MODE)).map_err(
        |error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot set mode {STATE_FILE_MODE:03o} on '{}': {error}",
                    path.display()
                ),
            )
        },
    )
}

#[cfg(not(unix))]
fn harden_managed_file(_path: &Path) -> Result<(), PluginFailure> {
    Ok(())
}

/// Force owner-only permissions on the directory holding the manifest.
#[cfg(unix)]
fn harden_managed_dir(path: &Path) -> Result<(), PluginFailure> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(STATE_DIR_MODE)).map_err(
        |error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot set mode {STATE_DIR_MODE:03o} on '{}': {error}",
                    path.display()
                ),
            )
        },
    )
}

#[cfg(not(unix))]
fn harden_managed_dir(_path: &Path) -> Result<(), PluginFailure> {
    Ok(())
}

/// Write the state, keeping the previous bytes at `<file>.bak` first.
fn save_state(path: &Path, state: &PluginState) -> Result<Option<PathBuf>, PluginFailure> {
    let rendered = state.render();
    // Never write bytes this module cannot read back (defense in depth).
    PluginState::parse(&rendered).map_err(|error| {
        PluginFailure::generic(
            "Internal",
            format!("bitty plugin: internal error: rendered manifest invalid: {error}"),
        )
    })?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PluginFailure::generic(
                    "IoError",
                    format!(
                        "bitty plugin: cannot create '{}': {error}",
                        parent.display()
                    ),
                )
            })?;
            harden_managed_dir(parent)?;
        }
    }
    let mut backup = None;
    if path.exists() {
        let mut backup_name = path.as_os_str().to_os_string();
        backup_name.push(STATE_BACKUP_SUFFIX);
        let backup_path = PathBuf::from(backup_name);
        let previous = std::fs::read(path).map_err(|error| {
            PluginFailure::generic(
                "IoError",
                format!("bitty plugin: cannot read '{}': {error}", path.display()),
            )
        })?;
        std::fs::write(&backup_path, previous).map_err(|error| {
            PluginFailure::generic(
                "IoError",
                format!(
                    "bitty plugin: cannot write backup '{}': {error}",
                    backup_path.display()
                ),
            )
        })?;
        harden_managed_file(&backup_path)?;
        backup = Some(backup_path);
    }
    std::fs::write(path, rendered).map_err(|error| {
        PluginFailure::generic(
            "IoError",
            format!("bitty plugin: cannot write '{}': {error}", path.display()),
        )
    })?;
    harden_managed_file(path)?;
    Ok(backup)
}

// ---------------------------------------------------------------------------
// Init follow-ups (CTX-1132, #1913): offline-first batch recording
// ---------------------------------------------------------------------------

/// Resolve the managed-manifest path for `bitty init` follow-ups.
///
/// Mirrors [`resolve_state_path`] (explicit `--config`/`BITTY_CONFIG` parent,
/// else XDG config root): the init wizard records into the same
/// `bitty-plugins.toml` `plugin install <id>` uses, so a recorded queue is
/// consumable later by `plugin install`. Returns a plain string error for the
/// init dispatch (English-only, no secrets).
pub(crate) fn init_plugin_state_path(
    config_path: Option<&str>,
    bitty_config_env: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    resolve_state_path(config_path, bitty_config_env, xdg_config_home, home)
        .map_err(|failure| failure.message)
}

/// Record first-batch bundled ids offline without fetching or consent.
///
/// For each id in `ids` (validated against the same bundled source
/// `plugin install <id>` uses via [`bundled_manifest_for`], no duplicate
/// list): when absent, inserts a fresh disabled record (`source = "bundled"`,
/// pinned `manifest_hash`, `enabled = false`, empty grants/denials) so no
/// capability is granted without explicit consent.
///
/// Merge rules (never clobber user state): an id that already has a record is
/// always skipped, whatever its stored `manifest_hash`. The existing
/// `enabled` flag, `granted` set, and durable `denied` set (the deny-loop
/// guard only consent clears) are preserved untouched, and a stale hash is
/// never silently refreshed here — hash transitions (including the
/// `GrantStore::apply_update` gate) belong to the install consent flow via
/// [`init_install_bundled_with_consent`] or a later `plugin install <id>`.
/// Only ids with no record at all write fresh disabled records.
///
/// Offline-only: filesystem only, no network, no secrets. Fails closed on an
/// unknown id, an over-limit state, or any filesystem error (nothing partially
/// written: the state is saved once at the end with a `.bak` backup).
/// Returns the number of newly recorded ids.
pub(crate) fn init_record_bundled_offline(
    state_path: &Path,
    ids: &[String],
) -> Result<usize, String> {
    let mut state = load_state(state_path).map_err(|failure| failure.message)?;
    let mut recorded = 0usize;
    for id in ids {
        let manifest = bundled_manifest_for(id.trim()).ok_or_else(|| {
            let valid = bitty_plugin_host::bundled::bundled_ids_sorted().join(", ");
            format!(
                "bitty init: unknown plugin '{}' (bundled ids: {})",
                id.trim(),
                if valid.is_empty() {
                    "(none)".to_string()
                } else {
                    valid
                }
            )
        })?;
        manifest
            .validate()
            .map_err(|error| format!("bitty init: bundled manifest '{id}' is invalid: {error}"))?;
        let manifest_hash = manifest.manifest_hash();
        // Never touch an existing record: grants, denials, and hash
        // transitions belong to the install consent flow.
        if state.get(id.trim()).is_some() {
            continue;
        }
        if state.records.len() >= MAX_STATE_PLUGINS {
            return Err(format!(
                "bitty init: too many plugins (limit {MAX_STATE_PLUGINS}) — nothing changed"
            ));
        }
        state.insert(
            id.trim().to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash,
                enabled: false,
                granted: BTreeSet::new(),
                denied: BTreeSet::new(),
            },
        );
        recorded += 1;
    }
    if recorded > 0 {
        save_state(state_path, &state).map_err(|failure| failure.message)?;
    }
    Ok(recorded)
}

/// Install one bundled plugin via the install consent flow (CTX-1132, #1913).
///
/// Reuses [`op_install`] (per-plugin `Grant these capabilities? [y/N]`
/// consent, `--yes` honors) then persists with the same backup/hardening as
/// `plugin install <id>`. Call only after explicit opt-in (interactive `y` or
/// `--yes`); offline-safe for bundled ids (no network fetch, capability
/// consent only). Returns the install summary line.
pub(crate) fn init_install_bundled_with_consent(
    state_path: &Path,
    id: &str,
    yes: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<String, String> {
    let mut state = load_state(state_path).map_err(|failure| failure.message)?;
    let result =
        op_install(&mut state, id.trim(), yes, input, output).map_err(|failure| failure.message)?;
    let summary = result.summary.clone();
    if result.changed {
        save_state(state_path, &state).map_err(|failure| failure.message)?;
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Rows (static manifest + recorded state) and rendering
// ---------------------------------------------------------------------------

/// One plugin row for `list`/`info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// Plugin id.
    pub id: String,
    /// Display name (manifest, else id).
    pub name: String,
    /// Manifest version (`-` for records without a bundled manifest).
    pub version: String,
    /// One-line description.
    pub description: String,
    /// Whether the id is in the bundled catalog.
    pub bundled: bool,
    /// Provenance label (`bundled`, `registry`, `git`, `local-path`).
    pub source: String,
    /// `enabled`/`disabled` when recorded, else `available`.
    pub state: &'static str,
    /// Recorded enabled flag.
    pub enabled: bool,
    /// Hash pin status: `Some(true)` verified, `Some(false)` stale, `None`
    /// when there is no manifest to verify against.
    pub pin_ok: Option<bool>,
    /// Capabilities the manifest requests (recorded set when no manifest).
    pub requested: BTreeSet<CapabilityId>,
    /// Capabilities recorded as granted.
    pub granted: BTreeSet<CapabilityId>,
    /// Capabilities explicitly revoked (durable denials; managed records only).
    pub denied: BTreeSet<CapabilityId>,
    /// Qualified commands from the static manifest.
    pub commands: Vec<String>,
    /// Pinned manifest hash when recorded.
    pub manifest_hash: Option<String>,
}

impl PluginRow {
    /// Capability rows for `info`/envelopes: requested, granted, effect, risk.
    #[must_use]
    pub fn capability_rows(&self) -> Vec<CapabilityRow> {
        let mut ids: BTreeSet<CapabilityId> = self.requested.clone();
        ids.extend(self.granted.iter().cloned());
        ids.extend(self.denied.iter().cloned());
        ids.into_iter()
            .map(|capability| CapabilityRow {
                effect: effect_statement(&capability).to_string(),
                high_risk: capability.is_high_risk(),
                requested: self.requested.contains(&capability),
                granted: self.granted.contains(&capability),
                denied: self.denied.contains(&capability),
                id: capability.as_str().to_string(),
            })
            .collect()
    }

    /// Explicitly revoked capabilities in sorted order (display only).
    #[must_use]
    pub fn denied_ids(&self) -> Vec<String> {
        self.denied
            .iter()
            .map(|capability| capability.as_str().to_string())
            .collect()
    }
}

/// One capability row for `info` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    /// Capability identifier.
    pub id: String,
    /// Plain-language effect statement.
    pub effect: String,
    /// Whether the identifier is high risk.
    pub high_risk: bool,
    /// Whether the manifest requests it.
    pub requested: bool,
    /// Whether it is recorded as granted.
    pub granted: bool,
    /// Whether it was explicitly revoked (re-grant needs `install` consent).
    pub denied: bool,
}

fn rows_from_state(
    state: &PluginState,
    store_root: Option<&Path>,
) -> Result<Vec<PluginRow>, PluginFailure> {
    let mut rows: Vec<PluginRow> = Vec::new();
    let mut manifests = all_bundled_manifests();
    manifests.sort_by_key(|manifest| manifest.id().as_str().to_string());
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for manifest in &manifests {
        let id = manifest.id().as_str().to_string();
        seen.insert(id.clone());
        let requested = manifest.capabilities.all_ids().map_err(|error| {
            PluginFailure::plugin(
                "ManifestInvalid",
                format!("bitty plugin: bundled manifest '{id}' is invalid: {error}"),
            )
        })?;
        let record = state.get(&id);
        let (state_label, enabled, pin_ok, granted, denied, manifest_hash) = match record {
            Some(record) => (
                if record.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                record.enabled,
                Some(record.manifest_hash == manifest.manifest_hash()),
                record.granted.clone(),
                record.denied.clone(),
                Some(record.manifest_hash.clone()),
            ),
            None => (
                "available",
                false,
                None,
                BTreeSet::new(),
                BTreeSet::new(),
                None,
            ),
        };
        rows.push(PluginRow {
            id,
            name: manifest.identity.name.clone(),
            version: manifest.identity.version.clone(),
            description: manifest.identity.description.clone(),
            bundled: true,
            source: "bundled".to_string(),
            state: state_label,
            enabled,
            pin_ok,
            requested,
            granted,
            denied,
            commands: manifest
                .lazy
                .commands
                .iter()
                .map(|command| command.id.as_str().to_string())
                .collect(),
            manifest_hash,
        });
    }

    for id in state.ids() {
        if seen.contains(id) {
            continue;
        }
        seen.insert(id.clone());
        let record = state.get(id).expect("iterated id exists");
        rows.push(PluginRow {
            id: id.clone(),
            name: id.clone(),
            version: "-".to_string(),
            description: "recorded plugin without a bundled manifest".to_string(),
            bundled: false,
            source: record.source.clone(),
            state: if record.enabled {
                "enabled"
            } else {
                "disabled"
            },
            enabled: record.enabled,
            pin_ok: None,
            requested: record.granted.clone(),
            granted: record.granted.clone(),
            denied: record.denied.clone(),
            commands: Vec::new(),
            manifest_hash: Some(record.manifest_hash.clone()),
        });
    }

    if let Some(store_root) = store_root {
        for record in load_store_index(store_root)? {
            if seen.contains(&record.plugin_id) {
                continue;
            }
            seen.insert(record.plugin_id.clone());
            rows.push(store_row(store_root, &record));
        }
    }

    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// Read the store index, mapping any failure to a read-only plugin error.
fn load_store_index(
    store_root: &Path,
) -> Result<Vec<bitty_runtime::plugin_runtime::PluginRecord>, PluginFailure> {
    load_index(store_root).map_err(|error| {
        PluginFailure::plugin(
            "StoreInvalid",
            format!(
                "bitty plugin: plugin store '{}' is unreadable: {error}",
                store_root.display()
            ),
        )
    })
}

/// Build a row for one installed (non-bundled) store record.
///
/// The staged manifest body is read best-effort for display only; a record
/// whose body is unreadable still lists, with the recorded grant standing in
/// for the requested set. Loading re-verifies the hashes and fails closed.
fn store_row(store_root: &Path, record: &bitty_runtime::plugin_runtime::PluginRecord) -> PluginRow {
    let manifest = read_store_manifest(store_root, record);
    let granted: BTreeSet<CapabilityId> = record
        .granted
        .iter()
        .filter_map(|raw| CapabilityId::parse(raw).ok())
        .collect();
    let requested = manifest
        .as_ref()
        .and_then(|manifest| manifest.capabilities.all_ids().ok())
        .unwrap_or_else(|| granted.clone());
    PluginRow {
        id: record.plugin_id.clone(),
        name: manifest
            .as_ref()
            .map(|manifest| manifest.identity.name.clone())
            .unwrap_or_else(|| record.plugin_id.clone()),
        version: record.version.clone(),
        description: manifest.as_ref().map_or_else(
            || format!("installed from a {} source", record.source_class.as_str()),
            |manifest| manifest.identity.description.clone(),
        ),
        bundled: false,
        source: record.source_class.as_str().to_string(),
        state: if record.enabled {
            "enabled"
        } else {
            "disabled"
        },
        enabled: record.enabled,
        pin_ok: None,
        requested,
        granted,
        denied: BTreeSet::new(),
        commands: manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .lazy
                    .commands
                    .iter()
                    .map(|command| command.id.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default(),
        manifest_hash: Some(record.manifest_hash.clone()),
    }
}

/// Best-effort read of one staged manifest body (display only, never authority).
///
/// The record root is validated with the same store-escape guard the runtime
/// uses, so a crafted `current.json` can never make a read-only command open a
/// file outside the store.
fn read_store_manifest(
    store_root: &Path,
    record: &bitty_runtime::plugin_runtime::PluginRecord,
) -> Option<PluginManifest> {
    let root = bitty_runtime::plugin_runtime::resolution::store_package_root(store_root, record)?;
    let bytes = std::fs::read(root.join(package::MANIFEST_FILE_NAME)).ok()?;
    manifest_toml::parse_manifest(&bytes).ok()
}

fn row_for_id(
    state: &PluginState,
    store_root: Option<&Path>,
    id: &str,
) -> Result<PluginRow, PluginFailure> {
    rows_from_state(state, store_root)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| {
            PluginFailure::plugin(
                "PluginNotFound",
                format!("bitty plugin: unknown plugin '{id}' (see `bitty plugin list`)"),
            )
        })
}

fn pin_label(pin_ok: Option<bool>) -> &'static str {
    match pin_ok {
        Some(true) => "ok",
        Some(false) => "stale",
        None => "-",
    }
}

/// Whether ANSI emphasis is allowed (unified CLI gate: `--no-color`,
/// `NO_COLOR`, `TERM=dumb`, stdout is a terminal).
fn color_enabled(no_color: bool) -> bool {
    crate::color::cli_color_enabled(no_color)
}

fn bold(text: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[1m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn format_list_table(rows: &[PluginRow], no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let header = format!(
        "{:<32} {:<10} {:<11} {:<6} {:>8}  {}",
        "ID", "SOURCE", "STATE", "PIN", "GRANTED", "VERSION"
    );
    let _ = writeln!(out, "{}", bold(&header, color));
    for row in rows {
        let granted = format!("{}/{}", row.granted.len(), row.requested.len());
        let _ = writeln!(
            out,
            "{:<32} {:<10} {:<11} {:<6} {:>8}  {}",
            row.id,
            row.source,
            row.state,
            pin_label(row.pin_ok),
            granted,
            row.version
        );
    }
    if rows.is_empty() {
        out.push_str("(no plugins)\n");
    }
    out
}

fn format_info_table(row: &PluginRow, no_color: bool) -> String {
    let color = color_enabled(no_color);
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold(&format!("plugin {}", row.id), color));
    let _ = writeln!(out, "  name:         {}", row.name);
    let _ = writeln!(out, "  version:      {}", row.version);
    let _ = writeln!(out, "  source:       {}", row.source);
    let _ = writeln!(out, "  state:        {}", row.state);
    let _ = writeln!(
        out,
        "  manifest:     {}",
        row.manifest_hash.as_deref().unwrap_or("-")
    );
    let _ = writeln!(out, "  pin:          {}", pin_label(row.pin_ok));
    let _ = writeln!(out, "  description:  {}", row.description);
    let _ = writeln!(
        out,
        "  commands:     {}",
        if row.commands.is_empty() {
            "-".to_string()
        } else {
            row.commands.join(", ")
        }
    );
    let capabilities = row.capability_rows();
    let _ = writeln!(out, "  capabilities ({}):", capabilities.len());
    for capability in &capabilities {
        let marker = if capability.denied {
            "revoked"
        } else if capability.granted {
            "granted"
        } else if capability.requested {
            "requested, not granted"
        } else {
            "not requested"
        };
        let risk = if capability.high_risk {
            " (high risk)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "    [{marker}] {}{risk} — {}",
            capability.id, capability.effect
        );
    }
    if capabilities.is_empty() {
        out.push_str("    (none requested)\n");
    }
    let revoked = row.denied_ids();
    if !revoked.is_empty() {
        let _ = writeln!(
            out,
            "  revoked ({}): {} — re-grant with `bitty plugin install {}`",
            revoked.len(),
            revoked.join(", "),
            row.id
        );
    }
    out
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn write_capability_array(out: &mut String, capabilities: &[CapabilityRow]) {
    out.push('[');
    for (index, capability) in capabilities.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"id\":\"{}\",\"effect\":\"{}\",\"high_risk\":{},\"requested\":{},\"granted\":{},\"denied\":{}}}",
            json_escape(&capability.id),
            json_escape(&capability.effect),
            capability.high_risk,
            capability.requested,
            capability.granted,
            capability.denied
        );
    }
    out.push(']');
}

fn write_plugin_object(out: &mut String, row: &PluginRow, include_manifest: bool) {
    let _ = write!(
        out,
        "{{\"id\":\"{}\",\"name\":\"{}\",\"version\":\"{}\",\"description\":\"{}\",\"bundled\":{},\"source\":\"{}\",\"state\":\"{}\",\"enabled\":{},\"pin_ok\":{}",
        json_escape(&row.id),
        json_escape(&row.name),
        json_escape(&row.version),
        json_escape(&row.description),
        row.bundled,
        json_escape(&row.source),
        row.state,
        row.enabled,
        match row.pin_ok {
            Some(value) => value.to_string(),
            None => "null".to_string(),
        }
    );
    if include_manifest {
        let _ = write!(
            out,
            ",\"manifest_hash\":{}",
            match &row.manifest_hash {
                Some(hash) => format!("\"{}\"", json_escape(hash)),
                None => "null".to_string(),
            }
        );
    }
    out.push_str(",\"capabilities\":");
    write_capability_array(out, &row.capability_rows());
    out.push_str(",\"commands\":[");
    for (index, command) in row.commands.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", json_escape(command));
    }
    out.push_str("]}");
}

/// Success envelope for `list`.
#[must_use]
pub fn format_list_envelope(rows: &[PluginRow]) -> String {
    let mut out =
        String::from("{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"list\",");
    let _ = write!(out, "\"count\":{},", rows.len());
    out.push_str("\"plugins\":[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_plugin_object(&mut out, row, false);
    }
    out.push_str("]}}");
    out
}

/// Success envelope for `info`.
#[must_use]
pub fn format_info_envelope(row: &PluginRow) -> String {
    let mut out = String::from(
        "{\"v\":1,\"command\":\"plugin\",\"ok\":true,\"result\":{\"verb\":\"info\",\"plugin\":",
    );
    write_plugin_object(&mut out, row, true);
    out.push_str("}}");
    out
}

/// Failure envelope (`ok:false`) for json/jsonl read-only output.
#[must_use]
pub fn format_error_envelope(verb: &str, class: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"plugin\",\"ok\":false,\"error\":{{\"class\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}},\"result\":{{\"verb\":\"{}\"}}}}",
        json_escape(class),
        json_escape(code),
        json_escape(message),
        json_escape(verb)
    )
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Injected environment for one `bitty plugin` dispatch (hermetic in tests).
#[derive(Debug, Clone, Default)]
pub struct PluginContext<'a> {
    /// Explicit `--config PATH` value.
    pub config_path: Option<&'a str>,
    /// `BITTY_CONFIG` environment value.
    pub bitty_config_env: Option<&'a str>,
    /// `XDG_CONFIG_HOME` environment value.
    pub xdg_config_home: Option<&'a str>,
    /// `XDG_DATA_HOME` environment value (plugin store root parent).
    pub xdg_data_home: Option<&'a str>,
    /// `HOME` environment value.
    pub home: Option<&'a str>,
    /// Pre-word global `--format` fallback (mirrors `bitty dev`).
    pub pre_format: Option<&'a str>,
    /// Pre-word `--no-color`.
    pub pre_no_color: bool,
}

struct OpOutput {
    summary: String,
    changed: bool,
}

/// Run one `bitty plugin` invocation; returns the process exit code.
pub fn run_plugin_subcommand(
    raw: &[String],
    context: &PluginContext<'_>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let request = match parse_plugin_request(raw, context.pre_format) {
        Ok(request) => request,
        Err(PluginParseError::Help) => {
            let _ = output.write_all(plugin_help_text().as_bytes());
            let _ = output.write_all(b"\n");
            return EXIT_OK;
        }
        Err(error) => {
            eprintln!("{}\n{}", error.message(), plugin_usage());
            return EXIT_USAGE;
        }
    };

    // Post-word `--no-color` wins; a pre-word global flag is honored too.
    let no_color = request.no_color || context.pre_no_color;
    let path = match resolve_state_path(
        context.config_path,
        context.bitty_config_env,
        context.xdg_config_home,
        context.home,
    ) {
        Ok(path) => path,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return failure.exit;
        }
    };
    let mut state = match load_state(&path) {
        Ok(state) => state,
        Err(failure) => {
            eprintln!("{}", failure.message);
            return failure.exit;
        }
    };
    // Resolved once from injected environment values (never process env here).
    let store_root = crate::plugin_runtime::store_root_for(context.xdg_data_home, context.home);
    // Config dir for next-steps display (CTX-1268): the parent of the
    // managed manifest, so an explicit `--config`/`BITTY_CONFIG` selects
    // its own `plugins/` directory for the copy-paste path.
    let config_dir: Option<PathBuf> = path.parent().map(Path::to_path_buf);

    match request.verb {
        PluginVerb::List => match rows_from_state(&state, store_root.as_deref()) {
            Ok(rows) => {
                match request.format {
                    PluginFormat::Table => {
                        let _ = output.write_all(format_list_table(&rows, no_color).as_bytes());
                    }
                    PluginFormat::Json | PluginFormat::Jsonl => {
                        let _ = writeln!(output, "{}", format_list_envelope(&rows));
                    }
                }
                EXIT_OK
            }
            Err(failure) => fail_read_only(&request, &failure, output),
        },
        PluginVerb::Info => {
            let id = request.id.as_deref().expect("info requires an id");
            match resolve_plugin_id(id) {
                Ok(_) => {}
                Err(failure) => return fail_read_only(&request, &failure, output),
            }
            match row_for_id(&state, store_root.as_deref(), id) {
                Ok(row) => {
                    match request.format {
                        PluginFormat::Table => {
                            let _ = output.write_all(format_info_table(&row, no_color).as_bytes());
                        }
                        PluginFormat::Json | PluginFormat::Jsonl => {
                            let _ = writeln!(output, "{}", format_info_envelope(&row));
                        }
                    }
                    EXIT_OK
                }
                Err(failure) => fail_read_only(&request, &failure, output),
            }
        }
        PluginVerb::Install => {
            let operand = request.id.as_deref().expect("install requires an operand");
            if is_source_operand(operand) {
                return install_source(
                    &request,
                    operand,
                    store_root.as_deref(),
                    config_dir.as_deref(),
                    input,
                    output,
                );
            }
            match op_install(&mut state, operand, request.yes, input, output) {
                Ok(result) => {
                    let code = finish_mutation(&path, state, result, output);
                    if code == EXIT_OK {
                        // Bundled success: the manifest is in the catalog
                        // (op_install just used it), so a lookup miss falls
                        // back to generic next steps, never to an error.
                        let manifest = bundled_manifest(operand).ok();
                        write_next_steps(
                            output,
                            operand,
                            manifest.as_ref(),
                            config_dir.as_deref(),
                            None,
                        );
                    }
                    code
                }
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
        PluginVerb::Enable => {
            let id = request.id.as_deref().expect("enable requires an id");
            if is_managed_operand(&state, id) {
                match op_enable(&mut state, id) {
                    Ok(result) => {
                        let code = finish_mutation(&path, state, result, output);
                        if code == EXIT_OK {
                            let manifest = bundled_manifest(id).ok();
                            write_next_steps(
                                output,
                                id,
                                manifest.as_ref(),
                                config_dir.as_deref(),
                                None,
                            );
                        }
                        code
                    }
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                set_store_enabled(&store_root, id, true, config_dir.as_deref(), output)
            }
        }
        PluginVerb::Disable => {
            let id = request.id.as_deref().expect("disable requires an id");
            if is_managed_operand(&state, id) {
                match op_disable(&mut state, id) {
                    Ok(result) => finish_mutation(&path, state, result, output),
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                set_store_enabled(&store_root, id, false, config_dir.as_deref(), output)
            }
        }
        PluginVerb::Remove => {
            let id = request.id.as_deref().expect("remove requires an id");
            if is_managed_operand(&state, id) {
                match op_remove(&mut state, id, request.force) {
                    Ok(result) => finish_mutation(&path, state, result, output),
                    Err(failure) => {
                        eprintln!("{}", failure.message);
                        failure.exit
                    }
                }
            } else {
                uninstall_store(&store_root, id, request.force, output)
            }
        }
        PluginVerb::Revoke => {
            let id = request.id.as_deref().expect("revoke requires an id");
            if !is_managed_operand(&state, id) {
                eprintln!(
                    "bitty plugin: `revoke {id}` needs a managed record (bundled plugin); \
                     installed packages revoke through `bitty plugin remove {id} --force` \
                     followed by `bitty plugin install <path>` to re-grant"
                );
                return EXIT_PLUGIN;
            }
            match op_revoke(&mut state, id, request.cap.as_deref()) {
                Ok(result) => finish_mutation(&path, state, result, output),
                Err(failure) => {
                    eprintln!("{}", failure.message);
                    failure.exit
                }
            }
        }
    }
}

/// Whether an operand addresses the managed (bundled) catalog rather than the
/// installed package store.
fn is_managed_operand(state: &PluginState, id: &str) -> bool {
    state.get(id).is_some() || bundled_manifest_for(id).is_some()
}

/// Whether an `install` operand names a filesystem source rather than a
/// bundled plugin id.
///
/// Path separators, leading `.`/`~`, an existing directory or file, an
/// archive suffix, and any git/URL spelling all select the package-manager
/// path. A bare dotted token is a bundled id; the id grammar cannot contain
/// separators, so the two never overlap.
fn is_source_operand(token: &str) -> bool {
    if classify_archive_kind(token).is_some() {
        return true;
    }
    if normalize_git_source(token).is_some() || rejected_scheme(token).is_some() {
        return true;
    }
    token.contains('/')
        || token.contains('\\')
        || token.starts_with('.')
        || token.starts_with('~')
        || Path::new(token).is_dir()
        || Path::new(token).is_file()
}

/// Naming diagnostic for a rejected fetch scheme (fail-closed, exit 4).
///
/// Names the scheme so `file://`, `http://`, `ssh://`, `git://`, `git:`,
/// `ftp://`, and other non-git spellings are distinguishable from a malformed
/// git URL. Returns `None` when the token carries no URL scheme at all.
fn rejected_scheme(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    // Order matters: check the longest distinctive prefixes first.
    for scheme in [
        "file://", "http://", "https://", "ssh://", "git://", "ftp://", "ftps://", "git:",
    ] {
        if lower.starts_with(scheme) {
            // `https://` and `git@` have allowed git spellings; only report
            // them here when they fail the git allowlist (the caller checks
            // `normalize_git_source` first).
            return Some(scheme.to_string());
        }
    }
    None
}

/// Local archive kind selected by file-name suffix (case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    /// `.tar.gz` / `.tgz` (gzip-compressed tar).
    TarGz,
    /// `.tar.zst` (zstd-compressed tar).
    TarZst,
    /// `.zip`.
    Zip,
}

/// Classify an `install` operand as a local archive by suffix.
///
/// Returns `None` for non-archive spellings. The file itself is validated
/// later (must exist, must be a regular file, size-bounded); the suffix alone
/// selects the unpack helper.
fn classify_archive_kind(token: &str) -> Option<ArchiveKind> {
    let lower = token.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else if lower.ends_with(".tar.zst") {
        Some(ArchiveKind::TarZst)
    } else if lower.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else {
        None
    }
}

/// Normalize an `install` operand to a cloneable git URL.
///
/// Accepts exactly three spellings (case-insensitive scheme where applicable):
/// `https://<host>/<path>.git`, `git+https://<host>/<path>.git` (the `git+`
/// prefix is stripped before cloning; git itself does not understand it),
/// and scp-like `git@<host>:<path>`. Returns the URL to pass to
/// `git clone` (stripped for `git+https`), or `None` when the token is not an
/// allowed git spelling. Malformed git-looking tokens fail closed in the
/// caller with a naming diagnostic; this function only recognizes the
/// allowlist.
fn normalize_git_source(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    if let Some(inner) = token
        .strip_prefix("git+")
        .or_else(|| token.strip_prefix("GIT+"))
        .or_else(|| {
            if lower.starts_with("git+") {
                Some(&token[4..])
            } else {
                None
            }
        })
    {
        // `git+https://…` only: `git+` plus any other scheme is rejected.
        // The scheme is normalized to lowercase: git picks its transport
        // from the literal scheme text, and a mixed-case `Https://` would
        // make it look for a nonexistent `git-remote-Https` helper.
        if inner.to_ascii_lowercase().starts_with("https://") && is_valid_https_git_url(inner) {
            return Some(format!("https://{}", &inner[8..]));
        }
        return None;
    }
    if lower.starts_with("https://") {
        if is_valid_https_git_url(token) {
            return Some(format!("https://{}", &token[8..]));
        }
        return None;
    }
    if token.starts_with("git@") {
        if is_valid_scp_git_source(token) {
            return Some(token.to_string());
        }
        return None;
    }
    None
}

/// Whether `token` is a valid `https://` git URL for cloning.
///
/// Strict allowlist: `https://` host (`[A-Za-z0-9.-]`, leading alnum,
/// optional `:port`) `/` path (`[A-Za-z0-9._/-]`, non-empty) ending in
/// `.git`. Anything else (whitespace, control, backslash, quotes, shell
/// metacharacters, `=` so `--config`/`--upload-pack=` cannot hide in the
/// path, `@` so credentials cannot hide in the URL) fails closed. The length
/// is already bounded by [`MAX_PLUGIN_TOKEN_BYTES`] at parse time.
fn is_valid_https_git_url(token: &str) -> bool {
    let Some(rest) = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("HTTPS://"))
        .or_else(|| {
            if token.len() >= 8 && token[..8].eq_ignore_ascii_case("https://") {
                Some(&token[8..])
            } else {
                None
            }
        })
    else {
        return false;
    };
    if rest.is_empty() || rest.len() > MAX_PLUGIN_TOKEN_BYTES {
        return false;
    }
    if rest
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        return false;
    }
    // No shell metacharacters, no backslash, no credential `@`, no `=` (which
    // would let `--config`/`--upload-pack=` hide in the path as one argv).
    if rest.contains([
        '\\', '"', '\'', '`', '$', '|', ';', '&', '<', '>', '(', ')', '{', '}', '*', '?', '#', '@',
        '=', '!', '+', ',', '%',
    ]) {
        return false;
    }
    // A leading dash would let the URL be parsed as a git flag without the
    // end-of-options separator; reject regardless (defense in depth: the
    // clone argv also carries `--`).
    if rest.starts_with('-') {
        return false;
    }
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let (host_port, path) = (&rest[..slash], &rest[slash + 1..]);
    if host_port.is_empty() || path.is_empty() {
        return false;
    }
    if !host_port
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
    {
        return false;
    }
    let host = host_port.split(':').next().unwrap_or("");
    if host.is_empty()
        || !host
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return false;
    }
    if let Some(port) = host_port.split(':').nth(1) {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if host_port.split(':').count() != 2 {
            return false;
        }
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
    {
        return false;
    }
    if path.contains("//") {
        return false;
    }
    path.ends_with(".git") && path.len() > ".git".len()
}

/// Whether `token` is a valid scp-like `git@<host>:<path>` source.
///
/// Strict allowlist: literal `git@`, host (`[A-Za-z0-9.-]`, leading alnum),
/// `:`, path (`[A-Za-z0-9._/-]`, non-empty, no leading dash). The `.git`
/// suffix is conventional but not required for this spelling. The same
/// metacharacter denylist as [`is_valid_https_git_url`] applies.
fn is_valid_scp_git_source(token: &str) -> bool {
    let Some(rest) = token.strip_prefix("git@") else {
        return false;
    };
    if rest.is_empty() || rest.len() > MAX_PLUGIN_TOKEN_BYTES {
        return false;
    }
    if rest
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        return false;
    }
    if rest.contains([
        '\\', '"', '\'', '`', '$', '|', ';', '&', '<', '>', '(', ')', '{', '}', '*', '?', '#', '@',
        '=', '!', '+', ',', '%',
    ]) {
        return false;
    }
    if rest.starts_with('-') {
        return false;
    }
    let Some(colon) = rest.find(':') else {
        return false;
    };
    let (host, path) = (&rest[..colon], &rest[colon + 1..]);
    if host.is_empty() || path.is_empty() {
        return false;
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return false;
    }
    if !host
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
    {
        return false;
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
    {
        return false;
    }
    if path.contains("//") || path.starts_with('-') {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Fetch staging (CTX-1103, #1901): exclusive temp dirs, fixed-argv helpers
// ---------------------------------------------------------------------------

/// Maximum archive file size staged (64 MiB; the declared unpacked content is
/// bounded separately by [`MAX_FETCH_UNPACKED_BYTES`], and the module-tree
/// ceilings still bound the committed content to 16 MiB / 4096 files).
const MAX_FETCH_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum total declared unpacked bytes one archive may hold (64 MiB, same
/// as the compressed-file cap).
///
/// Zip-bomb defense: a highly compressible archive can declare far more
/// temporary disk than its file size suggests, so the declared total from
/// the pre-extraction listing (`tar -tv` / `unzip -l` sizes) is rejected
/// above this limit before anything is extracted, and the same total is
/// enforced again while walking the extracted tree (defense in depth).
const MAX_FETCH_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum bytes of one helper listing (`tar -tf` / `unzip -Z1`) parsed.
const MAX_FETCH_LIST_BYTES: usize = 256 * 1024;
/// Maximum archive members listed (2x the module-tree file ceiling + slack
/// for directory entries).
const MAX_FETCH_MEMBERS: usize = 8192;
/// Maximum bytes kept from a failed helper's stderr (bounded detail).
const MAX_FETCH_STDERR_BYTES: usize = 2048;
/// Maximum bytes for one archive member path (the ratified 1024-byte ceiling).
const MAX_FETCH_MEMBER_PATH_BYTES: usize = 1024;

/// Exclusive temp staging dir for one git/archive fetch (removed on drop
/// unless disarmed by forgetting — the caller moves the payload out through
/// the package manager first, then drops the guard to clean the temp copy).
struct ScopedFetchDir {
    path: PathBuf,
}

impl ScopedFetchDir {
    fn create(tag: &str) -> Result<Self, String> {
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt as _;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..100 {
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "bitty-plugin-fetch-{tag}-{}-{sequence}-{nanos}",
                std::process::id()
            ));
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "bitty plugin: cannot create staging dir '{}': {error}",
                        path.display()
                    ));
                }
            }
        }
        Err("bitty plugin: cannot create staging dir (no fresh name after retries)".to_string())
    }
}

impl Drop for ScopedFetchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Bounded, sanitized tail of a failed helper's stderr (no control bytes).
fn fetch_stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let tail: String = text
        .chars()
        .rev()
        .take(MAX_FETCH_STDERR_BYTES)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    tail.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Repository-location variables never inherited by a spawned helper.
///
/// `git` (and hooks that export these, e.g. `pre-commit`) lets `GIT_DIR` /
/// `GIT_WORK_TREE` / `GIT_INDEX_FILE` override the command line, so a clone
/// running under such a parent could lock and overwrite the caller's index.
/// Only these location variables are scrubbed: gitconfig, SSH-agent, proxy,
/// and CA handling still come free from the inherited environment.
const GIT_REPO_ENV_VARS: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

/// Run one fixed-argv helper; map every failure to a user-facing message.
///
/// `tool` resolves via `PATH` (`Command::new` finds `git.exe`/`tar.exe` on
/// Windows). The child inherits the caller's environment (system
/// gitconfig/SSH-agent/proxy/CA handling comes free, mirroring
/// `component_seed.rs` `SystemTransport` posture). No shell is ever
/// constructed: every element of `argv` is inert data.
fn run_fetch_tool(tool: &str, argv: &[String]) -> Result<std::process::Output, String> {
    run_fetch_tool_full(tool, argv, &[], false)
}

/// [`run_fetch_tool`] with a scoped environment override.
///
/// `extra_env` sets additional variables for this invocation only (the
/// process environment is never mutated); `scrub_git_repo_env` removes the
/// [`GIT_REPO_ENV_VARS`] location variables so a hooked parent cannot
/// redirect the child into the caller's repository.
fn run_fetch_tool_full(
    tool: &str,
    argv: &[String],
    extra_env: &[(&str, &str)],
    scrub_git_repo_env: bool,
) -> Result<std::process::Output, String> {
    let mut command = std::process::Command::new(tool);
    command.args(argv);
    command.envs(extra_env.iter().copied());
    if scrub_git_repo_env {
        for var in GIT_REPO_ENV_VARS {
            command.env_remove(var);
        }
    }
    command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            match tool {
                "git" => "bitty plugin: `git` not found (install git first, or install from a local directory or archive)".to_string(),
                "tar" => "bitty plugin: `tar` not found (install tar first, or install from a local directory)".to_string(),
                "unzip" => "bitty plugin: `unzip` not found (install unzip first, or repack as .tar.gz; .zip installs fail closed without it)".to_string(),
                _ => format!("bitty plugin: `{tool}` not found (install {tool} first)"),
            }
        } else {
            format!("bitty plugin: cannot run `{tool}`: {error}")
        }
    })
}

/// Fixed-argv `git clone --filter=blob:none --depth=1 -- <url> <dest>`.
///
/// The `--` end-of-options separator plus the leading-dash rejection in
/// [`is_valid_https_git_url`]/[`is_valid_scp_git_source`] closes
/// `--upload-pack=`/`--config` smuggling via the URL: the URL is always one
/// inert argv element, never a flag. Repository-location variables are
/// scrubbed ([`GIT_REPO_ENV_VARS`]) so a clone under a git hook cannot lock
/// the caller's index; everything else is inherited.
fn run_git_clone(url: &str, dest: &Path) -> Result<(), String> {
    let argv = vec![
        "clone".to_string(),
        "--filter=blob:none".to_string(),
        "--depth=1".to_string(),
        "--".to_string(),
        url.to_string(),
        dest.to_string_lossy().into_owned(),
    ];
    let output = run_fetch_tool_full("git", &argv, &[], true)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: `git clone` failed ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// List tar members (names only, never extracted here).
fn list_tar_members(archive: &Path, kind: ArchiveKind) -> Result<Vec<String>, String> {
    let archive_arg = archive.to_string_lossy().into_owned();
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec!["-tzf".to_string(), archive_arg],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-tf".to_string(),
            archive_arg,
        ],
        ArchiveKind::Zip => {
            return Err("bitty plugin: internal error: tar lister called for .zip".to_string());
        }
    };
    let output = run_fetch_tool("tar", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive members ({}; {tail})",
            output.status
        ));
    }
    parse_member_list(&output.stdout)
}

/// List zip members via `unzip -Z1` (bare names, one per line).
fn list_zip_members(archive: &Path) -> Result<Vec<String>, String> {
    let argv = vec!["-Z1".to_string(), archive.to_string_lossy().into_owned()];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive members ({}; {tail})",
            output.status
        ));
    }
    parse_member_list(&output.stdout)
}

/// Parse and bound one helper listing (UTF-8, member-count bounded).
fn parse_member_list(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(format!(
            "bitty plugin: archive member list exceeds {MAX_FETCH_LIST_BYTES} bytes (refusing hostile archive)"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        "bitty plugin: archive member names are not UTF-8 (refusing hostile archive)".to_string()
    })?;
    let members: Vec<String> = text.lines().map(str::to_string).collect();
    if members.len() > MAX_FETCH_MEMBERS {
        return Err(format!(
            "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
            members.len()
        ));
    }
    Ok(members)
}

/// Declared unpacked total for a tar archive via `tar -tv` sizes.
///
/// Never extracts: the verbose listing carries one size field per member.
/// Returns the entry count plus the summed declared bytes so the caller can
/// cross-check the count against the names listing (a mismatch fails closed).
fn declared_tar_unpacked_bytes(archive: &Path, kind: ArchiveKind) -> Result<(usize, u64), String> {
    let archive_arg = archive.to_string_lossy().into_owned();
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec!["-tzvf".to_string(), archive_arg],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-tvf".to_string(),
            archive_arg,
        ],
        ArchiveKind::Zip => {
            return Err(
                "bitty plugin: internal error: tar size lister called for .zip".to_string(),
            );
        }
    };
    // `bsdtar` localizes `%b` month names via the process locale; the parser
    // below only accepts the English `TAR_VERBOSE_MONTHS`, so this one
    // invocation runs under the C locale. The names listing keeps the user's
    // locale, and the process environment is never mutated.
    let output = run_fetch_tool_full("tar", &argv, &[("LC_ALL", "C")], false)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive sizes ({}; {tail})",
            output.status
        ));
    }
    parse_tar_verbose_total(&output.stdout)
}

/// BSD `tar -tv` month abbreviations (libarchive output spells the date as
/// `Mon DD HH:MM` or `Mon DD YYYY` where GNU spells `YYYY-MM-DD HH:MM`).
const TAR_VERBOSE_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Whether a field looks like a GNU `tar -tv` date (`YYYY-MM-DD`).
fn is_tar_verbose_date(field: &str) -> bool {
    field.len() == 10
        && field.as_bytes()[4] == b'-'
        && field.as_bytes()[7] == b'-'
        && field
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

/// Whether a field looks like a `tar -tv` time (`HH:MM`).
fn is_tar_verbose_time(field: &str) -> bool {
    field.len() == 5
        && field.as_bytes()[2] == b':'
        && field
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 2 || b.is_ascii_digit())
}

/// Parse `tar -tv` output into an entry count plus a declared byte total.
///
/// Accepts GNU (`PERMS OWNER SIZE YYYY-MM-DD HH:MM NAME`) and BSD/libarchive
/// (`PERMS NLINK USER GROUP SIZE Mon DD HH:MM|YYYY NAME`) shapes by anchoring
/// the size on the date field (the GNU date is searched first so a BSD month
/// name inside a GNU filename can never misalign the parse). Every line must
/// match one shape with a numeric size, a date, a time-or-year, and a name;
/// anything else fails closed rather than guessing.
fn parse_tar_verbose_total(bytes: &[u8]) -> Result<(usize, u64), String> {
    let refuse = |why: &str| {
        format!("bitty plugin: cannot parse archive sizes ({why}; refusing hostile archive)")
    };
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(refuse("size listing too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refuse("size listing is not UTF-8"))?;
    let mut count = 0usize;
    let mut total = 0u64;
    for line in text.lines() {
        if line.is_empty() {
            return Err(refuse("blank size entry"));
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // GNU shape first: the size sits immediately before the date field.
        let size = if let Some(date_at) = fields.iter().position(|f| is_tar_verbose_date(f)) {
            let Some(size_text) = date_at.checked_sub(1).and_then(|i| fields.get(i)) else {
                return Err(refuse("malformed size entry"));
            };
            // The date must be followed by a time and a name.
            let (Some(time), Some(_)) = (fields.get(date_at + 1), fields.get(date_at + 2)) else {
                return Err(refuse("malformed size entry"));
            };
            if !is_tar_verbose_time(time) {
                return Err(refuse("malformed size entry"));
            }
            size_text
        } else if let Some(month_at) = fields.iter().position(|f| TAR_VERBOSE_MONTHS.contains(f)) {
            // BSD shape: the size sits immediately before the month, followed
            // by a 1-2 digit day and a time (`HH:MM`) or year (`YYYY`).
            let Some(size_text) = month_at.checked_sub(1).and_then(|i| fields.get(i)) else {
                return Err(refuse("malformed size entry"));
            };
            let (Some(day), Some(clock)) = (fields.get(month_at + 1), fields.get(month_at + 2))
            else {
                return Err(refuse("malformed size entry"));
            };
            let day_ok =
                !day.is_empty() && day.len() <= 2 && day.bytes().all(|b| b.is_ascii_digit());
            let clock_ok = is_tar_verbose_time(clock)
                || (clock.len() == 4 && clock.bytes().all(|b| b.is_ascii_digit()));
            if !day_ok || !clock_ok || fields.get(month_at + 3).is_none() {
                return Err(refuse("malformed size entry"));
            }
            size_text
        } else {
            return Err(refuse("malformed size entry"));
        };
        let size: u64 = size.parse().map_err(|_| refuse("non-numeric size entry"))?;
        total = total.saturating_add(size);
        count += 1;
        if count > MAX_FETCH_MEMBERS {
            return Err(format!(
                "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
                count + 1
            ));
        }
        // Immediate refusal once the running total passes the enforcement
        // bound: returning here (instead of stopping early with a partial
        // count) keeps the declared-size diagnostic instead of surfacing a
        // misleading listing-count mismatch downstream.
        if total > MAX_FETCH_UNPACKED_BYTES {
            return Err(format!(
                "bitty plugin: archive declares {total} unpacked bytes (limit {MAX_FETCH_UNPACKED_BYTES}; refusing hostile archive)"
            ));
        }
    }
    Ok((count, total))
}

/// Declared unpacked total for a zip archive via `unzip -l` sizes.
///
/// Never extracts: the table carries one `Length` field per member row.
/// Returns the data-row count plus the summed lengths so the caller can
/// cross-check the count against the `-Z1` names listing.
fn declared_zip_unpacked_bytes(archive: &Path) -> Result<(usize, u64), String> {
    let argv = vec!["-l".to_string(), archive.to_string_lossy().into_owned()];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot list archive sizes ({}; {tail})",
            output.status
        ));
    }
    parse_zip_list_total(&output.stdout)
}

/// Parse an `unzip -l` table into a data-row count plus a summed total.
///
/// Skips the `Archive:`/`Length` header and consumes rows after the first
/// `-----` separator until the footer separator; every row must lead with a
/// numeric length (names may contain spaces, which is why only the first
/// field is read). Anything else fails closed.
fn parse_zip_list_total(bytes: &[u8]) -> Result<(usize, u64), String> {
    let refuse = |why: &str| {
        format!("bitty plugin: cannot parse archive sizes ({why}; refusing hostile archive)")
    };
    if bytes.len() > MAX_FETCH_LIST_BYTES {
        return Err(refuse("size listing too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refuse("size listing is not UTF-8"))?;
    let mut lines = text.lines();
    let mut in_rows = false;
    let mut count = 0usize;
    let mut total = 0u64;
    let mut closed = false;
    for line in &mut lines {
        if line.starts_with("-----") {
            if in_rows {
                closed = true;
                break;
            }
            in_rows = true;
            continue;
        }
        if !in_rows {
            continue;
        }
        if line.trim().is_empty() {
            return Err(refuse("blank size entry"));
        }
        let Some(first) = line.split_whitespace().next() else {
            return Err(refuse("malformed size entry"));
        };
        let size: u64 = first
            .parse()
            .map_err(|_| refuse("non-numeric size entry"))?;
        total = total.saturating_add(size);
        count += 1;
        if count > MAX_FETCH_MEMBERS {
            return Err(format!(
                "bitty plugin: archive holds {} members (limit {MAX_FETCH_MEMBERS}; refusing hostile archive)",
                count + 1
            ));
        }
    }
    if !closed {
        return Err(refuse("size table has no footer"));
    }
    Ok((count, total))
}

/// Validate one archive member name (fail closed on traversal).
///
/// Rejects absolute members (`/…`), Windows separators (`\`, `:` so drive
/// letters cannot hide), `..` path segments, over-long paths, and control
/// characters. Leading `./` and trailing `/` (directory entries) are
/// tolerated and stripped before the checks. An empty root entry (`./`)
/// is allowed.
fn validate_archive_member(raw: &str) -> Result<(), String> {
    if raw.is_empty() {
        return Err(
            "bitty plugin: archive holds an empty member name (refusing hostile archive)"
                .to_string(),
        );
    }
    if raw.len() > MAX_FETCH_MEMBER_PATH_BYTES {
        return Err(format!(
            "bitty plugin: archive member {raw:?} exceeds {MAX_FETCH_MEMBER_PATH_BYTES} bytes (refusing hostile archive)"
        ));
    }
    if raw.contains('\0') || raw.chars().any(|c| c.is_control() && c != '\t') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} carries control characters (refusing hostile archive)"
        ));
    }
    // Tolerate the `./` prefix tar emits for `tar -C src .` archives and the
    // trailing `/` on directory entries; everything else is checked literally
    // (no normalization — normalization is where traversal bugs hide).
    let mut name = raw;
    if let Some(stripped) = name.strip_prefix("./") {
        name = stripped;
    } else if name == "." {
        return Ok(());
    }
    name = name.strip_suffix('/').unwrap_or(name);
    if name.is_empty() {
        // `./` or `/` root entries (the latter is stripped to empty only when
        // the raw member was exactly `/`, which is absolute and rejected
        // below — but `./` legitimately reduces to empty here).
        if raw == "./" || raw == "." {
            return Ok(());
        }
        return Err(format!(
            "bitty plugin: archive member {raw:?} is not a usable path (refusing hostile archive)"
        ));
    }
    if raw.starts_with('/') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} is absolute (refusing hostile archive)"
        ));
    }
    if name.starts_with('/') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} is absolute (refusing hostile archive)"
        ));
    }
    if raw.contains('\\') || raw.contains(':') {
        return Err(format!(
            "bitty plugin: archive member {raw:?} carries a Windows separator or drive (refusing hostile archive)"
        ));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(format!(
                "bitty plugin: archive member {raw:?} has an empty path segment (refusing hostile archive)"
            ));
        }
        if component == ".." {
            return Err(format!(
                "bitty plugin: archive member {raw:?} escapes its directory (refusing hostile archive)"
            ));
        }
    }
    Ok(())
}

/// Audit every listed member before anything is extracted.
fn audit_archive_members(members: &[String]) -> Result<(), String> {
    if members.is_empty() {
        return Err("bitty plugin: archive holds no members (refusing empty archive)".to_string());
    }
    for member in members {
        validate_archive_member(member)?;
    }
    Ok(())
}

/// Extract a tar archive (fixed argv, confined with `-C`).
fn extract_tar(archive: &Path, dest: &Path, kind: ArchiveKind) -> Result<(), String> {
    let argv: Vec<String> = match kind {
        ArchiveKind::TarGz => vec![
            "-xzf".to_string(),
            archive.to_string_lossy().into_owned(),
            "-C".to_string(),
            dest.to_string_lossy().into_owned(),
        ],
        ArchiveKind::TarZst => vec![
            "--use-compress-program=unzstd".to_string(),
            "-xf".to_string(),
            archive.to_string_lossy().into_owned(),
            "-C".to_string(),
            dest.to_string_lossy().into_owned(),
        ],
        ArchiveKind::Zip => {
            return Err("bitty plugin: internal error: tar extractor called for .zip".to_string());
        }
    };
    let output = run_fetch_tool("tar", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot unpack archive ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// Extract a zip archive (`unzip -q <archive> -d <dest>`, fixed argv).
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let argv = vec![
        "-q".to_string(),
        archive.to_string_lossy().into_owned(),
        "-d".to_string(),
        dest.to_string_lossy().into_owned(),
    ];
    let output = run_fetch_tool("unzip", &argv)?;
    if !output.status.success() {
        let tail = fetch_stderr_tail(&output.stderr);
        return Err(format!(
            "bitty plugin: cannot unpack archive ({}; {tail})",
            output.status
        ));
    }
    Ok(())
}

/// Audit the extracted tree: no symlinks, total regular-file bytes bounded.
///
/// Both GNU `tar` and `unzip` refuse to follow a just-created symlink dir
/// component during the same extraction (verified), so a symlink member lands
/// inside staging as a link and is caught here before the package manager
/// ever reads the tree. The walked byte total re-enforces
/// [`MAX_FETCH_UNPACKED_BYTES`] as defense in depth (the declared total was
/// already checked pre-extraction; the listing and the payload could still
/// disagree). The staging dir is removed by the caller on any failure.
fn audit_extracted_tree(root: &Path) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    let mut total = 0u64;
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            format!(
                "bitty plugin: cannot inspect unpacked archive '{}': {error}",
                dir.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("bitty plugin: cannot inspect unpacked archive entry: {error}")
            })?;
            seen += 1;
            if seen > MAX_FETCH_MEMBERS {
                return Err(format!(
                    "bitty plugin: unpacked archive exceeds {MAX_FETCH_MEMBERS} entries (refusing hostile archive)"
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "bitty plugin: cannot inspect unpacked member '{}': {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "bitty plugin: archive member '{}' is a symlink (refusing hostile archive)",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                if total > MAX_FETCH_UNPACKED_BYTES {
                    return Err(format!(
                        "bitty plugin: unpacked archive exceeds {MAX_FETCH_UNPACKED_BYTES} bytes (refusing hostile archive)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Pick the package root inside an extracted archive.
///
/// Archives either lay the manifest at the extraction root
/// (`bitty-plugin.toml`) or under one top-level directory
/// (`my-plugin/bitty-plugin.toml`); both are accepted. Anything else falls
/// through to the extraction root so the existing manifest-missing diagnostic
/// names the problem.
fn find_plugin_root(extract_dir: &Path) -> PathBuf {
    if extract_dir.join("bitty-plugin.toml").is_file() {
        return extract_dir.to_path_buf();
    }
    let Ok(entries) = std::fs::read_dir(extract_dir) else {
        return extract_dir.to_path_buf();
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut has_files = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if path.is_dir() {
            dirs.push(path);
        } else {
            has_files = true;
        }
    }
    if !has_files && dirs.len() == 1 && dirs[0].join("bitty-plugin.toml").is_file() {
        return dirs.remove(0);
    }
    extract_dir.to_path_buf()
}

/// Unpack one local archive file into `dest` (must exist, must be empty).
///
/// Lists members first (never extracted here), audits every name for
/// traversal, rejects declared unpacked totals above
/// [`MAX_FETCH_UNPACKED_BYTES`] before anything is extracted (zip-bomb
/// defense), extracts with the fixed-argv helper, then audits the tree for
/// symlinks and re-enforces the byte total. Any failure is fail-closed with
/// nothing trusted.
fn unpack_archive_into(archive: &Path, dest: &Path, kind: ArchiveKind) -> Result<(), String> {
    let members = match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => list_tar_members(archive, kind)?,
        ArchiveKind::Zip => list_zip_members(archive)?,
    };
    audit_archive_members(&members)?;
    // Zip-bomb gate: the declared total must fit before extraction starts.
    // The entry count must agree with the names listing; a mismatch means
    // the two listing passes disagreed and fails closed.
    let (declared_count, declared_total) = match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => declared_tar_unpacked_bytes(archive, kind)?,
        ArchiveKind::Zip => declared_zip_unpacked_bytes(archive)?,
    };
    if declared_count != members.len() {
        return Err(
            "bitty plugin: archive size listing disagrees with its member listing (refusing hostile archive)"
                .to_string(),
        );
    }
    if declared_total > MAX_FETCH_UNPACKED_BYTES {
        return Err(format!(
            "bitty plugin: archive declares {declared_total} unpacked bytes (limit {MAX_FETCH_UNPACKED_BYTES}; refusing hostile archive)"
        ));
    }
    match kind {
        ArchiveKind::TarGz | ArchiveKind::TarZst => extract_tar(archive, dest, kind)?,
        ArchiveKind::Zip => extract_zip(archive, dest)?,
    }
    audit_extracted_tree(dest)
}

/// Install or update one plugin from a local directory into the XDG store.
fn install_source(
    request: &PluginRequest,
    source: &str,
    store_root: Option<&Path>,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    // Git spellings (allowlist) clone first, then share the local pipeline.
    if let Some(url) = normalize_git_source(source) {
        return install_git_source(request, source, &url, store_root, config_dir, input, output);
    }
    // A URL-looking operand that is not an allowed git spelling fails closed
    // with a naming diagnostic (http downgrade, file escape, ssh without
    // git@, option-smuggling spellings that still look like URLs).
    if rejected_scheme(source).is_some() || looks_like_url(source) {
        return reject_fetch_scheme(source);
    }
    if let Some(kind) = classify_archive_kind(source) {
        return install_archive_source(
            request, source, kind, store_root, config_dir, input, output,
        );
    }
    install_local_path(
        request,
        source,
        Path::new(source),
        store_root,
        config_dir,
        input,
        output,
    )
}

/// Whether a token looks like a URL even when its scheme is not allowlisted.
///
/// Catches `GIT+HTTPS://…` case variants, `ssh:…`, and leading-dash
/// smuggling spellings (`--upload-pack=…`, `--config …`) that reach this
/// layer as one argv element, so they fail closed as scheme errors (exit 4)
/// instead of masquerading as bundled ids.
fn looks_like_url(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("://")
        || lower.starts_with("git@")
        || lower.starts_with("git:")
        || lower.starts_with("ssh:")
        || token.starts_with('-')
}

/// Fail closed on a rejected fetch scheme with a naming diagnostic.
fn reject_fetch_scheme(source: &str) -> i32 {
    let lower = source.to_ascii_lowercase();
    let scheme = if lower.starts_with("file://") {
        "file://"
    } else if lower.starts_with("http://") {
        "http://"
    } else if lower.starts_with("ssh://") || lower.starts_with("ssh:") {
        "ssh://"
    } else if lower.starts_with("git://") || lower.starts_with("git:") {
        "git://"
    } else if lower.starts_with("ftp://") || lower.starts_with("ftps://") {
        "ftp://"
    } else if lower.contains("://") {
        "URL"
    } else if source.starts_with('-') {
        "flag-like"
    } else {
        "non-git"
    };
    if scheme == "http://" {
        eprintln!(
            "bitty plugin: refusing {scheme} source {source:?}: plugin git sources require https:// (or git+https:// / git@); \
             install from a local directory or archive instead"
        );
    } else if scheme == "flag-like" {
        eprintln!(
            "bitty plugin: refusing flag-like source {source:?}: plugin sources are bundled ids, local paths, \
             https://<host>/<path>.git, git+https://..., git@<host>:<path>, or local .tar.gz/.tgz/.tar.zst/.zip files"
        );
    } else {
        eprintln!(
            "bitty plugin: refusing {scheme} source {source:?}: plugin git sources are https://<host>/<path>.git, \
             git+https://..., or git@<host>:<path> only; install from a local directory or archive instead"
        );
    }
    EXIT_PLUGIN
}

/// Install from a git URL: clone into exclusive temp staging, then the
/// existing local-path pipeline unchanged (manifest/schema/compat, hash pin,
/// consent, atomic `current.json`). No plugin code runs at any step.
fn install_git_source(
    request: &PluginRequest,
    display: &str,
    url: &str,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let staging = match ScopedFetchDir::create("git") {
        Ok(staging) => staging,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    };
    let checkout = staging.path.join("src");
    if let Err(message) = run_git_clone(url, &checkout) {
        eprintln!("{message}");
        return EXIT_GENERIC;
    }
    install_local_path(
        request, display, &checkout, store_root, config_dir, input, output,
    )
}

/// Install from a local archive file: unpack into exclusive temp staging,
/// then the existing local-path pipeline unchanged.
fn install_archive_source(
    request: &PluginRequest,
    display: &str,
    kind: ArchiveKind,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    let canonical = match std::fs::canonicalize(display) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    let metadata = match std::fs::metadata(&canonical) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    if !metadata.is_file() {
        eprintln!("bitty plugin: invalid source: '{display}' is not a file");
        return EXIT_GENERIC;
    }
    if metadata.len() > MAX_FETCH_ARCHIVE_BYTES {
        eprintln!(
            "bitty plugin: archive '{display}' exceeds {MAX_FETCH_ARCHIVE_BYTES} bytes (refusing hostile archive)"
        );
        return EXIT_PLUGIN;
    }
    // Snapshot the archive into staging first so a concurrent replace of the
    // user path cannot change the bytes between listing and extraction.
    let staging = match ScopedFetchDir::create("archive") {
        Ok(staging) => staging,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_GENERIC;
        }
    };
    let suffix = match kind {
        ArchiveKind::TarGz => "payload.tar.gz",
        ArchiveKind::TarZst => "payload.tar.zst",
        ArchiveKind::Zip => "payload.zip",
    };
    let staged_archive = staging.path.join(suffix);
    let bytes = match std::fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("bitty plugin: invalid source: '{display}': {error}");
            return EXIT_GENERIC;
        }
    };
    if bytes.len() as u64 > MAX_FETCH_ARCHIVE_BYTES {
        eprintln!(
            "bitty plugin: archive '{display}' exceeds {MAX_FETCH_ARCHIVE_BYTES} bytes (refusing hostile archive)"
        );
        return EXIT_PLUGIN;
    }
    if let Err(error) = std::fs::write(&staged_archive, &bytes) {
        eprintln!("bitty plugin: cannot stage archive: {error}");
        return EXIT_GENERIC;
    }
    let extract_dir = staging.path.join("src");
    if let Err(error) = std::fs::create_dir(&extract_dir) {
        eprintln!("bitty plugin: cannot create staging dir: {error}");
        return EXIT_GENERIC;
    }
    if let Err(message) = unpack_archive_into(&staged_archive, &extract_dir, kind) {
        // Distinguish validation (plugin error, exit 4) from helper/io
        // failures (exit 1) by message shape: hostile-archive audits refuse
        // explicitly, everything else is an io/helper failure. `unpack` ends
        // with the extracted-tree audit, so no second walk is needed here.
        eprintln!("{message}");
        if message.contains("refusing hostile archive")
            || message.contains("no members")
            || message.contains("empty archive")
            || message.contains("disagrees with its member listing")
        {
            return EXIT_PLUGIN;
        }
        return EXIT_GENERIC;
    }
    let root = find_plugin_root(&extract_dir);
    install_local_path(
        request, display, &root, store_root, config_dir, input, output,
    )
}

/// Shared local-path pipeline tail: consent bound to the staged snapshot,
/// then `package::install_local_dir` (manifest, entry point, bounds, compat,
/// hash pin, atomic `current.json`). The `source` argument selects the input
/// dir; `display` names the original operand in consent prompts.
fn install_local_path(
    request: &PluginRequest,
    display: &str,
    source: &Path,
    store_root: &Path,
    config_dir: Option<&Path>,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> i32 {
    // Installation stages and records the package, but explicit `enable` is
    // the authorization boundary for first execution. Keep the public
    // library default unchanged for existing developer/runtime callers.
    let options = LocalInstallOptions::staged_and_disabled();
    let approve_all = request.yes;
    // Consent is bound to the staged snapshot the installer reviewed: the
    // callback only decides, and the installer commits that same snapshot, so
    // a source swapped while the prompt is open can never widen the grant.
    let outcome = {
        let mut consent =
            |consent_request: &package::ConsentRequest| -> Result<bool, PackageOpError> {
                if approve_all {
                    return Ok(true);
                }
                match ask_source_consent(input, output, consent_request, display) {
                    Ok(approved) => Ok(approved),
                    Err(failure) => Err(PackageOpError::ConsentAborted(failure.message)),
                }
            };
        package::install_local_dir(store_root, source, &options, &mut consent)
    };
    match outcome {
        Ok(Some(report)) => {
            report_install(&report, output);
            // Post-install next steps (CTX-1268): the installer verified
            // `source/bitty-plugin.toml`, so re-read that body best-effort
            // for settings/hints; a concurrent replace falls back to
            // generic next steps, never to an error.
            let installed_under = store_root.join(&report.root);
            let manifest = read_source_manifest_for_next_steps(source);
            write_next_steps(
                output,
                &report.plugin_id,
                manifest.as_ref(),
                config_dir,
                Some(&installed_under),
            );
            EXIT_OK
        }
        Ok(None) => {
            eprintln!("bitty plugin: capability grant was not approved — nothing changed");
            EXIT_GENERIC
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

fn report_install(report: &package::InstallReport, output: &mut dyn std::io::Write) {
    let action = if report.updated {
        "updated"
    } else {
        "installed"
    };
    let _ = writeln!(
        output,
        "bitty plugin: {action} '{}' (version {}, {} source, manifest {}, {} capabilit{} granted)",
        report.plugin_id,
        report.version,
        report.source_class.as_str(),
        short_hash(&report.manifest_hash),
        report.granted.len(),
        if report.granted.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    if let Some(previous) = &report.previous_version {
        if previous != &report.version {
            let _ = writeln!(
                output,
                "bitty plugin: previous version {previous} retained for rollback under {}",
                report.root.rsplit_once('/').map_or("-", |(dir, _)| dir)
            );
        }
    }
    // CTX-0417: post-commit cleanup is non-fatal but must be visible: the
    // install already succeeded, so warn instead of failing.
    if let Some(warning) = &report.prune_warning {
        eprintln!("warning: bitty plugin: {warning}");
    }
}

// ---------------------------------------------------------------------------
// Post-install next steps (CTX-1268, bitty#2144)
// ---------------------------------------------------------------------------

/// How many declared `[settings]` defaults the next-steps snippet shows
/// before truncating with a count line (keeps install output bounded).
const NEXT_STEPS_SNIPPET_SETTINGS: usize = 3;

/// Escape one string for a TOML basic string in next-steps snippets.
///
/// Manifest string defaults are bounded but attacker-controlled; raw control
/// characters (including an ESC the settings grammar tolerates inside a
/// default) must never reach the terminal unescaped. Emits valid TOML basic
/// strings: `\\`, `\"`, `\n`, `\r`, `\t`, and `\uXXXX` for other C0/C1
/// controls and DEL; every other character passes through unchanged.
fn escape_toml_basic(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Render one declared setting as a minimal TOML config line
/// (`key = default`) for the next-steps snippet.
fn setting_snippet_line(decl: &SettingDecl) -> String {
    use bitty_plugin_host::manifest::SettingDefault;
    let rendered = match &decl.default {
        SettingDefault::Bool(value) => value.to_string(),
        SettingDefault::Int(value) => value.to_string(),
        SettingDefault::String(value) => format!("\"{}\"", escape_toml_basic(value)),
        SettingDefault::Strings(items) => {
            let quoted: Vec<String> = items
                .iter()
                .map(|item| format!("\"{}\"", escape_toml_basic(item)))
                .collect();
            format!("[{}]", quoted.join(", "))
        }
    };
    format!("{} = {rendered}", decl.key)
}

/// Render post-install next steps for one plugin (CTX-1268, bitty#2144).
///
/// Data-driven and display-only: the enable command, the per-plugin config
/// file path, a minimal snippet derived from the manifest's declared
/// `[settings]` defaults, and the `bitty plugin info <id>` pointer always
/// render; manifest-provided `[hints] next_steps` lines append when the
/// manifest declares them, otherwise the output stays clean generic.
/// `installed_under` names the store package root for external installs
/// (`None` for bundled plugins, which ship with bitty). Nothing here grants
/// authority, writes state, or runs plugin code.
fn format_next_steps(
    id: &str,
    settings: &[SettingDecl],
    next_steps: &[String],
    config_file: Option<&Path>,
    installed_under: Option<&Path>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Next steps for '{id}':");
    let _ = writeln!(out, "  enable: bitty plugin enable {id}");
    match installed_under {
        Some(root) => {
            let _ = writeln!(out, "  installed: {}", root.display());
        }
        None => {
            let _ = writeln!(out, "  installed: bundled with bitty (no package tree)");
        }
    }
    match config_file {
        Some(path) => {
            let _ = writeln!(out, "  configure: {}", path.display());
        }
        None => {
            let _ = writeln!(
                out,
                "  configure: <config-dir>/bitty/plugins/{id}.toml (no config root resolved)"
            );
        }
    }
    let shown: Vec<String> = settings
        .iter()
        .take(NEXT_STEPS_SNIPPET_SETTINGS)
        .map(setting_snippet_line)
        .collect();
    if shown.is_empty() {
        out.push_str("    # (no settings declared; nothing to configure)\n");
    } else {
        out.push_str("    # minimal example (create the file if absent):\n");
        for line in &shown {
            let _ = writeln!(out, "    {line}");
        }
        let rest = settings.len() - shown.len();
        if rest > 0 {
            let _ = writeln!(
                out,
                "    # (... {rest} more setting{} declared)",
                if rest == 1 { "" } else { "s" }
            );
        }
        out.push_str("    # (undeclared keys are ignored)\n");
    }
    if !next_steps.is_empty() {
        out.push_str("  hints:\n");
        for hint in next_steps {
            let _ = writeln!(out, "    - {hint}");
        }
    }
    let _ = writeln!(out, "  more: bitty plugin info {id}");
    out
}

/// Display path of one plugin's user config file
/// (`<config-dir>/plugins/<owner>.<name>.toml`, CTX-1195).
///
/// The id is already validated (`PluginId` grammar: one dot, no separators),
/// so the join cannot escape the `plugins/` directory; it is display-only
/// (no I/O happens here).
fn plugin_config_file_display(config_dir: Option<&Path>, id: &str) -> Option<PathBuf> {
    config_dir.map(|dir| {
        dir.join(PLUGINS_DIR_NAME)
            .join(format!("{id}.{PLUGIN_FILE_EXTENSION}"))
    })
}

/// Best-effort manifest read for next-steps rendering (store installs).
///
/// The installer already verified this body; a read/parse failure here (a
/// concurrent replace between commit and render) falls back to generic
/// next steps without settings or hints, never to an error.
fn read_source_manifest_for_next_steps(source: &Path) -> Option<PluginManifest> {
    let bytes = std::fs::read(source.join(package::MANIFEST_FILE_NAME)).ok()?;
    manifest_toml::parse_manifest(&bytes).ok()
}

/// Write next steps for one successful install/enable (display-only).
///
/// `manifest` carries the verified manifest when at hand (bundled catalog or
/// the just-installed source body); `None` renders the clean generic shape
/// (no snippet, no hints) without failing the command.
fn write_next_steps(
    output: &mut dyn std::io::Write,
    id: &str,
    manifest: Option<&PluginManifest>,
    config_dir: Option<&Path>,
    installed_under: Option<&Path>,
) {
    let config_file = plugin_config_file_display(config_dir, id);
    let (settings, next_steps): (&[SettingDecl], &[String]) = match manifest {
        Some(manifest) => (&manifest.settings, &manifest.next_steps),
        None => (&[], &[]),
    };
    let _ = writeln!(
        output,
        "{}",
        format_next_steps(
            id,
            settings,
            next_steps,
            config_file.as_deref(),
            installed_under
        )
    );
}

/// Toggle one installed store record.
fn set_store_enabled(
    store_root: &Option<PathBuf>,
    id: &str,
    enabled: bool,
    config_dir: Option<&Path>,
    output: &mut dyn std::io::Write,
) -> i32 {
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    match package::set_enabled(store_root, id, enabled) {
        Ok(changed) => {
            let state = if enabled { "enabled" } else { "disabled" };
            let _ = writeln!(
                output,
                "bitty plugin: '{id}' {}",
                if changed {
                    state.to_string()
                } else {
                    format!("already {state}")
                }
            );
            if enabled {
                // Best-effort store manifest for next steps (a missing index
                // entry or an unreadable body falls back to generic next
                // steps, never to an error).
                let record = load_index(store_root)
                    .ok()
                    .and_then(|records| records.into_iter().find(|record| record.plugin_id == id));
                let (manifest, installed_under) = match record.as_ref() {
                    Some(record) => (
                        read_store_manifest(store_root, record),
                        bitty_runtime::plugin_runtime::resolution::store_package_root(
                            store_root, record,
                        ),
                    ),
                    None => (None, None),
                };
                write_next_steps(
                    output,
                    id,
                    manifest.as_ref(),
                    config_dir,
                    installed_under.as_deref(),
                );
            }
            EXIT_OK
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

/// Uninstall one installed store record (destructive; requires `--force`).
fn uninstall_store(
    store_root: &Option<PathBuf>,
    id: &str,
    force: bool,
    output: &mut dyn std::io::Write,
) -> i32 {
    if !force {
        eprintln!(
            "bitty plugin: `remove {id}` is destructive; re-run with --force \
             (the installed package tree is deleted)"
        );
        return EXIT_USAGE;
    }
    let Some(store_root) = store_root else {
        eprintln!("bitty plugin: no data root ($XDG_DATA_HOME or $HOME unset)");
        return EXIT_USAGE;
    };
    match package::uninstall(store_root, id) {
        Ok(report) => {
            let _ = writeln!(
                output,
                "bitty plugin: removed '{}' (version {}, {} package tree)",
                report.plugin_id,
                report.version,
                if report.tree_removed {
                    "deleted"
                } else {
                    "already absent"
                }
            );
            EXIT_OK
        }
        Err(error) => {
            eprintln!("bitty plugin: {error}");
            package_failure_exit(&error)
        }
    }
}

/// Interactive consent for capabilities a local install newly requests.
fn ask_source_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    request: &package::ConsentRequest,
    source: &str,
) -> Result<bool, PluginFailure> {
    let _ = writeln!(
        output,
        "bitty plugin: '{}' (version {}, manifest {}) from '{source}' requests new capabilities:",
        request.plugin_id,
        request.version,
        short_hash(&request.manifest_hash)
    );
    for capability in &request.added {
        let effect = CapabilityId::parse(capability)
            .map(|capability| effect_statement(&capability).to_string())
            .unwrap_or_else(|_| "unknown capability".to_string());
        let _ = writeln!(output, "  - {capability} — {effect}");
    }
    // Bounded read (bitty#1919): same helper as `ask_consent` so a large
    // piped answer cannot grow memory without bound.
    for attempt in 1..=MAX_CONSENT_ATTEMPTS {
        let _ = write!(output, "Grant these capabilities? [y/N] ");
        let _ = output.flush();
        let Some(answer) = read_plugin_consent_answer(input, output, attempt) else {
            continue;
        };
        let answer = answer?;
        let answer = answer.trim();
        if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        if answer.is_empty()
            || answer.eq_ignore_ascii_case("n")
            || answer.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        let _ = writeln!(output, "bitty plugin: please answer 'y' or 'n'");
    }
    Err(PluginFailure::generic(
        "ConsentAborted",
        "bitty plugin: no valid consent answer after repeated attempts — nothing changed"
            .to_string(),
    ))
}

/// Stable exit code for a package-manager failure.
fn package_failure_exit(error: &PackageOpError) -> i32 {
    match error {
        PackageOpError::Manifest { .. }
        | PackageOpError::BundledIdReserved { .. }
        | PackageOpError::ModuleTree { .. }
        | PackageOpError::Incompatible { .. } => EXIT_PLUGIN,
        PackageOpError::InvalidSource(_)
        | PackageOpError::ConsentAborted(_)
        | PackageOpError::Store(_) => EXIT_GENERIC,
    }
}

fn fail_read_only(
    request: &PluginRequest,
    failure: &PluginFailure,
    output: &mut dyn std::io::Write,
) -> i32 {
    if request.format != PluginFormat::Table {
        let _ = writeln!(
            output,
            "{}",
            format_error_envelope(
                request.verb.name(),
                failure.class,
                failure.code,
                &failure.message
            )
        );
    }
    eprintln!("{}", failure.message);
    failure.exit
}

fn finish_mutation(
    path: &Path,
    state: PluginState,
    result: OpOutput,
    output: &mut dyn std::io::Write,
) -> i32 {
    if result.changed {
        match save_state(path, &state) {
            Ok(backup) => {
                if let Some(backup) = backup {
                    let _ = writeln!(
                        output,
                        "bitty plugin: backed up previous manifest to '{}'",
                        backup.display()
                    );
                }
            }
            Err(failure) => {
                eprintln!("{}", failure.message);
                return failure.exit;
            }
        }
    }
    let _ = writeln!(output, "{}", result.summary);
    EXIT_OK
}

fn resolve_plugin_id(raw: &str) -> Result<PluginId, PluginFailure> {
    if raw.len() > MAX_PLUGIN_TOKEN_BYTES || raw.contains('\0') {
        return Err(PluginFailure::usage(
            "bitty plugin: plugin id is empty, contains NUL, or is too long".to_string(),
        ));
    }
    PluginId::new(raw).map_err(|error| {
        PluginFailure::usage(format!(
            "bitty plugin: malformed plugin id '{raw}': {error}"
        ))
    })
}

fn bundled_manifest(id: &str) -> Result<PluginManifest, PluginFailure> {
    let manifest = bundled_manifest_for(id).ok_or_else(|| {
        PluginFailure::plugin(
            "UnknownPlugin",
            format!(
                "bitty plugin: '{id}' is not a bundled plugin; install an external package \
                 from a local directory (`bitty plugin install <path>`), a git URL \
                 (`https://<host>/<path>.git`, `git+https://...`, `git@<host>:<path>`), \
                 or a local archive (`.tar.gz`/`.tgz`/`.tar.zst`/`.zip`)"
            ),
        )
    })?;
    manifest.validate().map_err(|error| {
        PluginFailure::plugin(
            "ManifestInvalid",
            format!("bitty plugin: bundled manifest '{id}' is invalid: {error}"),
        )
    })?;
    Ok(manifest)
}

fn requested_capabilities(
    manifest: &PluginManifest,
) -> Result<BTreeSet<CapabilityId>, PluginFailure> {
    manifest.capabilities.all_ids().map_err(|error| {
        PluginFailure::plugin(
            "ManifestInvalid",
            format!(
                "bitty plugin: manifest '{}' expands to an invalid capability: {error}",
                manifest.id()
            ),
        )
    })
}

fn not_installed(id: &str) -> PluginFailure {
    PluginFailure::plugin(
        "PluginNotFound",
        format!("bitty plugin: '{id}' is not installed (see `bitty plugin list`)"),
    )
}

fn op_install(
    state: &mut PluginState,
    id: &str,
    yes: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<OpOutput, PluginFailure> {
    let plugin_id = resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let requested = requested_capabilities(&manifest)?;
    let existing = state.get(id).cloned();
    let now = now_unix_secs();

    // P0-AC-030 pattern: only capabilities absent from the recorded grant
    // need consent; unchanged/narrowed sets carry forward silently.
    // Revoked-then-requested capabilities are absent from `granted`, so they
    // always need explicit re-grant consent (no silent re-prompt loops).
    let needed: BTreeSet<CapabilityId> = match &existing {
        Some(record) => requested.difference(&record.granted).cloned().collect(),
        None => requested.clone(),
    };

    let approved = if needed.is_empty() {
        true
    } else if yes {
        let _ = writeln!(
            output,
            "bitty plugin: --yes approved {} capabilit{} for '{id}' (manifest {})",
            needed.len(),
            if needed.len() == 1 { "y" } else { "ies" },
            short_hash(&manifest_hash)
        );
        true
    } else {
        ask_consent(input, output, id, &manifest, &needed, existing.is_some())?
    };
    if !approved {
        return Err(PluginFailure::generic(
            "ConsentDeclined",
            format!("bitty plugin: capability grant for '{id}' was not approved — nothing changed"),
        ));
    }

    // Explicit consent is the re-grant action: requested capabilities leave
    // the denial set. Only consent clears denials; enable/disable/revoke
    // never do (deny-loop guard).
    let denied: BTreeSet<CapabilityId> = match &existing {
        Some(record) => record.denied.difference(&requested).cloned().collect(),
        None => BTreeSet::new(),
    };

    // Manifest-hash changes commit through the grant engine so the P0-AC-030
    // update gate owns the transition (OQ-012 grant lifecycle): stale hashes
    // and unapproved additions fail closed with no state change.
    if let Some(record) = &existing {
        if record.manifest_hash != manifest_hash {
            let mut engine = GrantStore::new();
            engine.insert(HostGrantRecord::granted(
                plugin_id.clone(),
                record.manifest_hash.clone(),
                record.granted.clone(),
                now,
            ));
            engine
                .apply_update(
                    &plugin_id,
                    &record.manifest_hash,
                    &manifest_hash,
                    &requested,
                    true,
                    now,
                )
                .map_err(|error| {
                    PluginFailure::plugin(
                        "CapabilityBlocked",
                        format!("bitty plugin: '{id}' update rejected by the grant gate: {error}"),
                    )
                })?;
        }
    }

    let changed = match &existing {
        Some(record) => {
            record.manifest_hash != manifest_hash
                || !record.enabled
                || record.granted != requested
                || record.denied != denied
        }
        None => true,
    };
    if changed {
        state.insert(
            id.to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: manifest_hash.clone(),
                enabled: true,
                granted: requested.clone(),
                denied,
            },
        );
    }
    let action = match &existing {
        None => "installed",
        Some(_) if !changed => "already installed",
        Some(_) => "updated",
    };
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: {action} '{id}' (version {}, manifest {}, {} capabilit{} granted, enabled)",
            manifest.identity.version,
            short_hash(&manifest_hash),
            requested.len(),
            if requested.len() == 1 { "y" } else { "ies" }
        ),
        changed,
    })
}

fn op_enable(state: &mut PluginState, id: &str) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let manifest = bundled_manifest(id)?;
    let manifest_hash = manifest.manifest_hash();
    let record = state.get(id).ok_or_else(|| not_installed(id))?;
    if record.manifest_hash != manifest_hash {
        return Err(PluginFailure::plugin(
            "PinMismatch",
            format!(
                "bitty plugin: '{id}' manifest changed since consent (pin {}, current {}); \
                 review with `bitty plugin info {id}` and re-run `bitty plugin install {id}`",
                short_hash(&record.manifest_hash),
                short_hash(&manifest_hash)
            ),
        ));
    }
    let requested = requested_capabilities(&manifest)?;
    if !requested.is_subset(&record.granted) {
        return Err(PluginFailure::plugin(
            "CapabilityBlocked",
            format!(
                "bitty plugin: '{id}' requests capabilities that are not granted; \
                 re-run `bitty plugin install {id}` to review and approve"
            ),
        ));
    }
    if record.enabled {
        return Ok(OpOutput {
            summary: format!("bitty plugin: '{id}' already enabled"),
            changed: false,
        });
    }
    let granted_len = record.granted.len();
    state.get_mut(id).expect("checked").enabled = true;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: enabled '{id}' (manifest {}, {granted_len} capabilit{})",
            short_hash(&manifest_hash),
            if granted_len == 1 {
                "y granted"
            } else {
                "ies granted"
            }
        ),
        changed: true,
    })
}

fn op_disable(state: &mut PluginState, id: &str) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let record = state.get(id).ok_or_else(|| not_installed(id))?;
    let manifest_hash = record.manifest_hash.clone();
    let granted = record.granted.len();
    if !record.enabled {
        return Ok(OpOutput {
            summary: format!("bitty plugin: '{id}' already disabled"),
            changed: false,
        });
    }
    state.get_mut(id).expect("checked").enabled = false;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: disabled '{id}' (manifest {}, {} grant{} kept)",
            short_hash(&manifest_hash),
            granted,
            if granted == 1 { "" } else { "s" }
        ),
        changed: true,
    })
}

fn op_remove(state: &mut PluginState, id: &str, force: bool) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    if !force {
        return Err(PluginFailure::usage(format!(
            "bitty plugin: `remove {id}` is destructive; re-run with --force \
             (the previous manifest is backed up to {STATE_FILE_NAME}{STATE_BACKUP_SUFFIX})"
        )));
    }
    let record = state.remove(id).ok_or_else(|| not_installed(id))?;
    Ok(OpOutput {
        summary: format!(
            "bitty plugin: removed '{id}' (manifest {}, {} grant{} dropped)",
            short_hash(&record.manifest_hash),
            record.granted.len(),
            if record.granted.len() == 1 { "" } else { "s" }
        ),
        changed: true,
    })
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// Monotonic-ish host time for grant records (Unix seconds; 0 when the clock
/// is unavailable). Used only as opaque `decided_at` evidence, never ordering.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Revoke grants for a recorded plugin without dropping the record.
///
/// - Without `capability`: every granted capability is dropped and the plugin
///   disabled (mirrors the grant-store full revoke; the record stays so the
///   decision is durable and auditable).
/// - With `capability`: one capability is dropped and remembered as an
///   explicit denial (no silent re-prompt); when the last granted capability
///   goes, the plugin is disabled exactly like a full revoke. Re-granting a
///   denied capability requires explicit `install` consent.
fn op_revoke(
    state: &mut PluginState,
    id: &str,
    capability: Option<&str>,
) -> Result<OpOutput, PluginFailure> {
    resolve_plugin_id(id)?;
    let record = state.get(id).cloned().ok_or_else(|| not_installed(id))?;
    let Some(capability) = capability else {
        if record.granted.is_empty() && !record.enabled {
            return Ok(OpOutput {
                summary: format!("bitty plugin: '{id}' already revoked"),
                changed: false,
            });
        }
        let count = record.granted.len();
        let kept_denied = record.denied.len();
        let entry = state.get_mut(id).expect("checked");
        entry.granted.clear();
        entry.enabled = false;
        return Ok(OpOutput {
            summary: format!(
                "bitty plugin: revoked '{id}' ({} grant{} dropped{}; re-grant with `bitty plugin install {id}`)",
                count,
                if count == 1 { "" } else { "s" },
                if kept_denied == 0 {
                    String::new()
                } else {
                    format!(
                        ", {} explicit denial{} kept",
                        kept_denied,
                        if kept_denied == 1 { "" } else { "s" }
                    )
                },
            ),
            changed: true,
        });
    };
    let capability = CapabilityId::parse(capability).map_err(|error| {
        PluginFailure::usage(format!("bitty plugin: invalid capability: {error}"))
    })?;
    if !record.granted.contains(&capability) {
        return Err(PluginFailure::plugin(
            "CapabilityNotGranted",
            format!(
                "bitty plugin: '{}' is not granted for '{id}' (nothing revoked)",
                capability.as_str()
            ),
        ));
    }
    let entry = state.get_mut(id).expect("checked");
    entry.granted.remove(&capability);
    entry.denied.insert(capability.clone());
    let fully_revoked = entry.granted.is_empty();
    if fully_revoked {
        entry.enabled = false;
    }
    Ok(OpOutput {
        summary: if fully_revoked {
            format!(
                "bitty plugin: revoked '{}' for '{id}' (last grant; plugin disabled, denial kept; re-grant with `bitty plugin install {id}`)",
                capability.as_str()
            )
        } else {
            format!(
                "bitty plugin: revoked '{}' for '{id}' (denial kept; re-grant with `bitty plugin install {id}`)",
                capability.as_str()
            )
        },
        changed: true,
    })
}

/// Ask the interactive capability-consent question (fails closed).
///
/// Returns `Ok(true)` when the user approves, `Ok(false)` on an explicit
/// decline, and `Err` when input ends before an answer (aborted). The read
/// itself is bounded (bitty#1919, same class as #1916): at most
/// `MAX_CONSENT_LINE_BYTES + 2` bytes are buffered per answer (room for a
/// 64-byte answer plus `\r\n`); an unterminated longer line is rejected as
/// overlong after its remainder is discarded through bounded `fill_buf`
/// takes, so a large piped answer cannot grow memory without bound and no
/// byte-index truncation can split a UTF-8 character.
fn ask_consent(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    id: &str,
    manifest: &PluginManifest,
    needed: &BTreeSet<CapabilityId>,
    update: bool,
) -> Result<bool, PluginFailure> {
    let noun = if needed.len() == 1 {
        "capability"
    } else {
        "capabilities"
    };
    let _ = writeln!(
        output,
        "bitty plugin: '{id}' {} {} new {noun} (manifest {}):",
        if update { "requests" } else { "requires" },
        needed.len(),
        short_hash(&manifest.manifest_hash())
    );
    for capability in needed {
        let risk = if capability.is_high_risk() {
            " (high risk)"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "  - {}{risk} — {}",
            capability.as_str(),
            effect_statement(capability)
        );
    }
    for attempt in 1..=MAX_CONSENT_ATTEMPTS {
        let _ = write!(output, "Grant these capabilities? [y/N]: ");
        let _ = output.flush();
        let Some(answer) = read_plugin_consent_answer(input, output, attempt) else {
            continue;
        };
        let answer = answer?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => {
                let _ = writeln!(
                    output,
                    "  (answer y or n — try again [{attempt}/{MAX_CONSENT_ATTEMPTS}])"
                );
            }
        }
    }
    Err(PluginFailure::generic(
        "ConsentAborted",
        format!(
            "bitty plugin: aborted (too many invalid answers, limit {MAX_CONSENT_ATTEMPTS}) — nothing changed"
        ),
    ))
}

/// Read one consent answer with a bounded buffer.
///
/// Twin of `component.rs` `read_consent_answer` (bitty#1916); keep the two in
/// sync. Returns `None` when the answer was overlong (already reported; the
/// caller retries without consuming an extra attempt branch), otherwise
/// `Some(Ok(line))` for a complete line or `Some(Err(..))` on EOF/read
/// error/invalid UTF-8 (fail closed, nothing changed).
fn read_plugin_consent_answer(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    attempt: usize,
) -> Option<Result<String, PluginFailure>> {
    // Window: a 64-byte answer plus a `\r\n` terminator. A line that does
    // not terminate inside the window is overlong and rejected. Bytes are
    // taken via `fill_buf`/`consume` (never a wide `read`) so a single
    // answer never swallows the start of the next line.
    const WINDOW: usize = MAX_CONSENT_LINE_BYTES + 2;
    let mut answer: Vec<u8> = Vec::with_capacity(WINDOW);
    let mut terminated = false;
    let mut failed = false;
    while answer.len() < WINDOW {
        let ate: usize = match input.fill_buf() {
            Ok(&[]) => break, // EOF: line ends here, if any.
            Err(_) => {
                failed = true;
                break;
            }
            Ok(buffered) => {
                let mut end = buffered.len().min(WINDOW - answer.len());
                if let Some(pos) = buffered[..end].iter().position(|&b| b == b'\n') {
                    end = pos + 1;
                    terminated = true;
                }
                answer.extend_from_slice(&buffered[..end]);
                end
            }
        };
        input.consume(ate);
        if terminated {
            break;
        }
    }
    if failed || (answer.is_empty() && !terminated) {
        return Some(Err(PluginFailure::generic(
            "ConsentAborted",
            "bitty plugin: aborted (end of input) — nothing changed".to_string(),
        )));
    }
    // A short EOF-terminated line (window not filled) is a complete
    // answer; only a filled window without a newline is overlong.
    if !terminated && answer.len() < WINDOW {
        terminated = true;
    }
    if !terminated {
        // Overlong: discard the line remainder (never past its newline,
        // so a queued next answer survives) so the next attempt starts
        // fresh. Fixed-window takes keep memory bounded no matter how
        // long the piped line is.
        loop {
            match input.fill_buf() {
                Ok(&[]) | Err(_) => break,
                Ok(buffered) => {
                    if let Some(pos) = buffered.iter().position(|&b| b == b'\n') {
                        let ate = pos + 1;
                        input.consume(ate);
                        break;
                    }
                    let ate = buffered.len();
                    input.consume(ate);
                }
            }
        }
        let _ = writeln!(
            output,
            "  (answer too long — at most {MAX_CONSENT_LINE_BYTES} bytes — try again [{attempt}/{MAX_CONSENT_ATTEMPTS}])"
        );
        return None;
    }
    match String::from_utf8(answer) {
        Ok(line) => Some(Ok(line)),
        Err(_) => Some(Err(PluginFailure::generic(
            "ConsentAborted",
            "bitty plugin: aborted (end of input) — nothing changed".to_string(),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests (TDD surface: parsing, codec, dispatch; no VM, temp dirs only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_ok(raw: &[&str]) -> PluginRequest {
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        parse_plugin_request(&owned, None).expect("request parses")
    }

    fn parse_error(raw: &[&str]) -> String {
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        match parse_plugin_request(&owned, None) {
            Err(PluginParseError::Usage(message)) => message,
            other => panic!("expected usage error, got {other:?}"),
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bitty-ctx0150-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn run(raw: &[&str], config_path: Option<&str>, input: &str) -> (i32, String, PathBuf) {
        let dir = scratch_dir("dispatch");
        let target = dir.join("init.lua");
        let owned_config = config_path
            .map(ToString::to_string)
            .unwrap_or_else(|| target.display().to_string());
        let context = PluginContext {
            config_path: Some(owned_config.as_str()),
            ..PluginContext::default()
        };
        let owned: Vec<String> = raw.iter().map(|token| (*token).to_string()).collect();
        let mut input = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let code = run_plugin_subcommand(&owned, &context, &mut input, &mut output);
        (code, String::from_utf8(output).expect("utf-8"), dir)
    }

    fn state_path(dir: &Path) -> PathBuf {
        dir.join(STATE_FILE_NAME)
    }

    // ── request parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_list_defaults() {
        let request = parse_ok(&["list"]);
        assert_eq!(request.verb, PluginVerb::List);
        assert_eq!(request.format, PluginFormat::Table);
        assert_eq!(request.id, None);
        assert!(!request.yes && !request.force);
    }

    #[test]
    fn parse_install_id_yes_and_format_pairs() {
        let request = parse_ok(&["install", "bitty-terminal.workspace", "--yes"]);
        assert_eq!(request.verb, PluginVerb::Install);
        assert_eq!(request.id.as_deref(), Some("bitty-terminal.workspace"));
        assert!(request.yes);

        let request = parse_ok(&["list", "--format", "json"]);
        assert_eq!(request.format, PluginFormat::Json);
        let request = parse_ok(&["info", "bitty-terminal.workspace", "--format=jsonl"]);
        assert_eq!(request.format, PluginFormat::Jsonl);
        let request = parse_ok(&["remove", "bitty-terminal.workspace", "--force"]);
        assert!(request.force);
    }

    #[test]
    fn parse_uses_pre_word_format_fallback() {
        let owned = vec!["list".to_string()];
        let request = parse_plugin_request(&owned, Some("json")).expect("parses");
        assert_eq!(request.format, PluginFormat::Json);
        // Explicit post-word value wins.
        let owned = vec![
            "list".to_string(),
            "--format".to_string(),
            "table".to_string(),
        ];
        let request = parse_plugin_request(&owned, Some("json")).expect("parses");
        assert_eq!(request.format, PluginFormat::Table);
    }

    #[test]
    fn parse_rejects_unknown_verb_flags_and_extras() {
        assert!(parse_error(&["frobnicate"]).contains("unknown verb"));
        assert!(parse_error(&["list", "--bogus"]).contains("unknown flag"));
        assert!(parse_error(&["list", "extra"]).contains("takes no plugin id"));
        assert!(parse_error(&["install"]).contains("needs a plugin id"));
        assert!(parse_error(&["install", "a", "b"]).contains("extra argument"));
        assert!(parse_error(&["info", "a", "--format", "yaml"]).contains("unknown --format"));
        assert!(parse_error(&["list", "--format"]).contains("--format needs a value"));
        assert!(parse_error(&["list", "--"]).contains("stray `--`"));
    }

    #[test]
    fn parse_rejects_flags_on_the_wrong_verb() {
        assert!(parse_error(&["list", "--yes"]).contains("--yes only applies"));
        assert!(parse_error(&["enable", "a", "--force"]).contains("--force only applies"));
        assert!(
            parse_error(&["install", "a", "--format", "json"]).contains("only apply to `list`")
        );
        assert!(parse_error(&["remove", "a", "--no-color"]).contains("only apply to `list`"));
    }

    #[test]
    fn parse_bounds_tokens_and_help() {
        let long = "x".repeat(MAX_PLUGIN_TOKEN_BYTES + 1);
        assert!(parse_error(&["install", &long]).contains("exceeds"));
        let owned = vec!["--help".to_string()];
        assert_eq!(
            parse_plugin_request(&owned, None),
            Err(PluginParseError::Help)
        );
    }

    // ── managed-manifest codec ────────────────────────────────────────────

    fn sample_state() -> PluginState {
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut records = BTreeMap::new();
        records.insert(
            "bitty-terminal.workspace".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "ab".repeat(32),
                enabled: true,
                granted,
                denied: BTreeSet::new(),
            },
        );
        PluginState::from_records(records)
    }

    #[test]
    fn state_round_trips_deterministically() {
        let rendered = sample_state().render();
        let parsed = PluginState::parse(&rendered).expect("parses");
        assert_eq!(parsed, sample_state());
        assert_eq!(parsed.render(), rendered);
        assert!(rendered.contains("state_version = 1"));
        assert!(rendered.contains("[plugins.\"bitty-terminal.workspace\"]"));
        assert!(rendered.contains("granted = [\"terminal.semantic-read\"]"));
        // Empty state renders and parses.
        let empty = PluginState::new().render();
        assert_eq!(
            PluginState::parse(&empty).expect("empty parses"),
            PluginState::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_manifest_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch_dir("state-mode");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("seed permissive scratch dir");
        let path = state_path(&dir);

        let backup = save_state(&path, &sample_state()).expect("first save");
        assert!(backup.is_none(), "first save has nothing to back up");
        let manifest_mode = std::fs::metadata(&path)
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(manifest_mode, 0o600, "managed manifest must be owner-only");
        let dir_mode = std::fs::metadata(&dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "managed directory must be owner-only");

        let backup = save_state(&path, &sample_state())
            .expect("second save")
            .expect("second save backs up the previous manifest");
        let backup_mode = std::fs::metadata(&backup)
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(backup_mode, 0o600, "backup must be owner-only");
    }

    #[test]
    fn state_parse_fails_closed_on_tampering() {
        let good = sample_state().render();
        for (bad, needle) in [
            (
                good.replace("state_version = 1", "state_version = 2"),
                "state_version",
            ),
            (
                good.replace("source = \"bundled\"", "source = \"git\""),
                "source",
            ),
            (
                good.replace(
                    "abababababababababababababababababababababababababababababababab",
                    "not-a-hash",
                ),
                "manifest_hash",
            ),
            (good.replace("enabled = true", "enabled = yes"), "enabled"),
            (
                good.replace(
                    "granted = [\"terminal.semantic-read\"]",
                    "granted = [\"nope\"]",
                ),
                "capability",
            ),
            (good.replace("manifest_hash", "bogus_key"), "unknown key"),
            (format!("{good}\n[plugins.\"x.y\"]\n"), "missing `source`"),
        ] {
            assert!(PluginState::parse(&bad).is_err(), "must reject: {needle}");
        }
        // Unknown top-level key before any section.
        assert!(PluginState::parse("nope = 1").is_err());
        // Missing version.
        assert!(PluginState::parse("# only a comment\n").is_err());
        // Duplicate plugin section.
        let dup = format!("{good}{good}");
        assert!(PluginState::parse(&dup).is_err());
        // Oversized file.
        let oversized = "a".repeat(MAX_STATE_FILE_BYTES + 1);
        assert!(PluginState::parse(&oversized).is_err());
        // Quoted value with an escape is not part of the subset.
        assert!(
            PluginState::parse("state_version = 1\n[plugins.\"a.b\"]\nsource = \"bun\\\"dled\"\n")
                .is_err()
        );
    }

    #[test]
    fn state_path_resolution_prefers_explicit_config_then_xdg() {
        let explicit = resolve_state_path(Some("/tmp/bitty-x/init.lua"), None, None, None)
            .expect("explicit path");
        assert_eq!(explicit, Path::new("/tmp/bitty-x").join(STATE_FILE_NAME));

        let env_config =
            resolve_state_path(None, Some("/env/bitty/init.lua"), None, None).expect("env path");
        assert_eq!(env_config, Path::new("/env/bitty").join(STATE_FILE_NAME));

        let xdg = resolve_state_path(None, None, Some("/xdg"), Some("/home/u")).expect("xdg");
        assert_eq!(xdg, Path::new("/xdg/bitty").join(STATE_FILE_NAME));

        let home = resolve_state_path(None, None, None, Some("/home/u")).expect("home");
        assert_eq!(
            home,
            Path::new("/home/u/.config/bitty").join(STATE_FILE_NAME)
        );

        assert!(resolve_state_path(None, None, None, None).is_err());
        assert!(resolve_state_path(Some("init.lua"), None, None, None).is_err());
    }

    // ── dispatch lifecycle ────────────────────────────────────────────────

    #[test]
    fn lifecycle_install_list_disable_enable_remove() {
        let dir = scratch_dir("lifecycle");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // install --yes grants the manifest capabilities and pins the hash.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("installed"), "{out}");
        let file = state_path(&dir);
        let text = std::fs::read_to_string(&file).expect("state written");
        assert!(text.contains("[plugins.\"bitty-terminal.shell-integration\"]"));
        assert!(text.contains("granted = [\"terminal.semantic-read\"]"));
        assert!(text.contains("enabled = true"));

        // list shows enabled + pinned.
        let (code, out, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("bitty-terminal.shell-integration"));
        assert!(out.contains("enabled"));
        assert!(out.contains("ok"));
        assert!(out.contains("1/1"));

        // disable is idempotent.
        let (code, out, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("disabled"), "{out}");
        let (code, out, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("already disabled"), "{out}");

        // enable restores it, keeping the grant.
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("enabled"), "{out}");

        // remove without --force fails closed and keeps the record.
        let (code, out, _) = run(
            &["remove", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(out.is_empty());
        assert!(
            std::fs::read_to_string(&file)
                .expect("kept")
                .contains("shell-integration")
        );

        // remove --force drops it and keeps a .bak of the previous state.
        let (code, out, _) = run(
            &["remove", "bitty-terminal.shell-integration", "--force"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("removed"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(!text.contains("shell-integration"));
        let backup = dir.join(format!("{STATE_FILE_NAME}{STATE_BACKUP_SUFFIX}"));
        assert!(backup.exists(), "backup kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_consent_fails_closed_on_eof_and_decline() {
        let dir = scratch_dir("consent");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // EOF: aborted, no state file created.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_GENERIC, "{out}");
        assert!(!state_path(&dir).exists(), "no state on abort");

        // Explicit decline: same, with the prompt shown.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "n\n",
        );
        assert_eq!(code, EXIT_GENERIC, "{out}");
        assert!(out.contains("terminal.semantic-read"), "{out}");
        assert!(out.contains("Read structured terminal content"), "{out}");
        assert!(!state_path(&dir).exists(), "no state on decline");

        // Approval writes the record.
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "y\n",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(state_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_rejects_unknown_and_non_bundled_ids() {
        let dir = scratch_dir("unknown");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        let (code, _, _) = run(&["install", "xuepoo.markdown", "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(&["install", "not_an_id", "--yes"], Some(&config), "");
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_offline_never_clobbers_existing_records() {
        // CodeRabbit 4241676524: a pre-existing record with grants + a stale
        // hash must survive `init --plugins` untouched (no silent disable,
        // no grant/denial loss); hash transitions belong to the install
        // consent flow. Absent ids still get fresh disabled records.
        let dir = scratch_dir("init-noclobber");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let path = crate::plugin::init_plugin_state_path(
            Some(&config),
            None,
            None,
            Some(&dir.display().to_string()),
        )
        .expect("state path resolves");
        let id = "bitty-terminal.shell-integration".to_string();

        // Seed a user record: enabled with a grant + denial under a stale hash.
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut denied = BTreeSet::new();
        denied.insert(CapabilityId::parse("ui.rich").expect("cap"));
        let mut records = BTreeMap::new();
        records.insert(
            id.clone(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "00".repeat(32),
                enabled: true,
                granted: granted.clone(),
                denied: denied.clone(),
            },
        );
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, PluginState::from_records(records).render()).expect("seed");
        let before = std::fs::read_to_string(&path).expect("seed readable");

        // Offline recording must skip the existing id (0 newly recorded).
        let recorded =
            init_record_bundled_offline(&path, std::slice::from_ref(&id)).expect("offline ok");
        assert_eq!(recorded, 0);
        let after = std::fs::read_to_string(&path).expect("state readable");
        assert_eq!(after, before, "existing record untouched");
        let state = PluginState::parse(&after).expect("parses");
        let kept = state.get(&id).expect("record kept");
        assert!(kept.enabled, "enabled preserved");
        assert_eq!(kept.granted, granted, "grants preserved");
        assert_eq!(kept.denied, denied, "denials preserved");
        assert_eq!(kept.manifest_hash, "00".repeat(32), "stale hash preserved");

        // Absent ids still get fresh disabled records as today.
        std::fs::remove_file(&path).expect("clear");
        let recorded =
            init_record_bundled_offline(&path, std::slice::from_ref(&id)).expect("offline ok");
        assert_eq!(recorded, 1);
        let state =
            PluginState::parse(&std::fs::read_to_string(&path).expect("readable")).expect("parses");
        let fresh = state.get(&id).expect("fresh record");
        assert!(!fresh.enabled, "fresh record disabled");
        assert!(fresh.granted.is_empty() && fresh.denied.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_fails_closed_on_pin_mismatch() {
        let dir = scratch_dir("pin");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let file = state_path(&dir);
        let mut records = BTreeMap::new();
        records.insert(
            "bitty-terminal.shell-integration".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "00".repeat(32),
                enabled: false,
                granted: BTreeSet::new(),
                denied: BTreeSet::new(),
            },
        );
        std::fs::write(&file, PluginState::from_records(records).render()).expect("seed state");

        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let text = std::fs::read_to_string(&file).expect("state kept");
        assert!(
            text.contains("enabled = false"),
            "no mutation on pin mismatch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_fails_closed() {
        let dir = scratch_dir("corrupt");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        std::fs::write(state_path(&dir), "state_version = 1\nbogus = 1\n").expect("seed");
        let (code, _, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_info_json_envelopes_are_versioned() {
        let dir = scratch_dir("envelopes");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");

        let (code, out, _) = run(&["list", "--format", "json"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("\"v\":1"), "{out}");
        assert!(out.contains("\"command\":\"plugin\""), "{out}");
        assert!(out.contains("\"state\":\"enabled\""), "{out}");
        assert!(out.contains("\"pin_ok\":true"), "{out}");

        let (code, out, _) = run(
            &[
                "info",
                "bitty-terminal.shell-integration",
                "--format",
                "json",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("\"verb\":\"info\""), "{out}");
        assert!(out.contains("\"manifest_hash\""), "{out}");
        assert!(out.contains("Read structured terminal content"), "{out}");
        assert!(out.contains("\"granted\":true"), "{out}");

        // Unknown plugin fails closed with a failure envelope in json mode.
        let (code, out, _) = run(
            &["info", "xuepoo.markdown", "--format", "json"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        assert!(out.contains("\"ok\":false"), "{out}");
        assert!(out.contains("PluginNotFound"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent_prompt_marks_high_risk_capabilities() {
        let mut output = Vec::new();
        let manifest = bundled_manifest_for("bitty-terminal.shell-integration").expect("bundled");
        let mut needed = BTreeSet::new();
        needed.insert(CapabilityId::parse("terminal.raw-read").expect("known high risk"));
        let mut input = Cursor::new(b"n\n".to_vec());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("answered");
        assert!(!approved);
        let text = String::from_utf8(output).expect("utf-8");
        assert!(text.contains("(high risk)"), "{text}");
        assert!(text.contains("Read raw terminal bytes"), "{text}");
        assert!(text.contains("Grant these capabilities? [y/N]:"), "{text}");
    }

    fn consent_fixture() -> (PluginManifest, BTreeSet<CapabilityId>) {
        let manifest = bundled_manifest_for("bitty-terminal.shell-integration").expect("bundled");
        let mut needed = BTreeSet::new();
        needed.insert(CapabilityId::parse("terminal.raw-read").expect("known high risk"));
        (manifest, needed)
    }

    #[test]
    fn consent_overlong_line_is_rejected_and_stream_resyncs() {
        // A 500-byte piped answer must not grow memory without bound: it
        // is rejected as overlong, its remainder discarded, and the next
        // queued answer (`y`) still applies.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let stdin = format!("{}\ny\n", "x".repeat(500));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("resyncs to the queued approval");
        assert!(approved);
        let shown = String::from_utf8(output).expect("utf-8");
        assert!(shown.contains("too long"), "{shown}");
    }

    #[test]
    fn consent_multibyte_at_boundary_does_not_panic() {
        // 63 ASCII bytes + `e` with acute (2 bytes) put byte 64
        // mid-character: the old `truncate(64)` panicked here. The bounded
        // read evaluates the full line instead (invalid answer, no panic),
        // then the queued `n` declines.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let stdin = format!("{}é\nn\n", "a".repeat(63));
        let mut input = std::io::BufReader::new(stdin.as_bytes());
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("boundary answer must not panic");
        assert!(!approved);
    }

    #[test]
    fn consent_short_eof_terminated_line_still_counts() {
        // `printf 'n'` (no trailing newline) declines exactly like `n\n`:
        // a short EOF-terminated line is a complete answer, not overlong.
        let (manifest, needed) = consent_fixture();
        let mut output = Vec::new();
        let mut input = std::io::BufReader::new(&b"n"[..]);
        let approved = ask_consent(&mut input, &mut output, "a.b", &manifest, &needed, false)
            .expect("short EOF-terminated answer counts");
        assert!(!approved);
        let shown = String::from_utf8(output).expect("utf-8");
        assert!(!shown.contains("too long"), "{shown}");
    }

    // ── revocation (CTX-0765, #1381) ────────────────────────────────────────

    #[test]
    fn parse_revoke_with_cap_pair_and_equals_forms() {
        let request = parse_ok(&["revoke", "bitty-terminal.workspace", "--cap", "ui.rich"]);
        assert_eq!(request.verb, PluginVerb::Revoke);
        assert_eq!(request.id.as_deref(), Some("bitty-terminal.workspace"));
        assert_eq!(request.cap.as_deref(), Some("ui.rich"));
        let request = parse_ok(&["revoke", "bitty-terminal.workspace", "--cap=ui.rich"]);
        assert_eq!(request.cap.as_deref(), Some("ui.rich"));
        let request = parse_ok(&["revoke", "bitty-terminal.workspace"]);
        assert_eq!(request.cap, None);
    }

    #[test]
    fn parse_cap_is_revoke_only() {
        assert!(
            parse_error(&["install", "bitty-terminal.workspace", "--cap", "ui.rich"])
                .contains("--cap only applies to `revoke`")
        );
        assert!(
            parse_error(&["revoke", "bitty-terminal.workspace", "--cap"])
                .contains("--cap needs a value")
        );
        assert!(
            parse_error(&[
                "revoke",
                "bitty-terminal.workspace",
                "--cap",
                "ui.rich",
                "--cap",
                "ui.rich"
            ])
            .contains("duplicate --cap")
        );
        assert!(parse_error(&["revoke"]).contains("needs a plugin id"));
    }

    fn install_shell_integration(config: &str) {
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
    }

    // ── post-install next steps (CTX-1268, bitty#2144) ─────────────────────

    fn next_steps_setting(
        key: &str,
        kind: bitty_plugin_host::manifest::SettingKind,
        default: bitty_plugin_host::manifest::SettingDefault,
    ) -> SettingDecl {
        SettingDecl::new(key.to_string(), kind, default).expect("fixture setting validates")
    }

    #[test]
    fn next_steps_generic_shape_is_stable() {
        // Golden: no settings, no hints, no config root, bundled install.
        let text = format_next_steps("xuepoo.demo", &[], &[], None, None);
        assert!(text.contains("Next steps for 'xuepoo.demo':"), "{text}");
        assert!(
            text.contains("enable: bitty plugin enable xuepoo.demo"),
            "{text}"
        );
        assert!(
            text.contains("installed: bundled with bitty (no package tree)"),
            "{text}"
        );
        assert!(text.contains("no config root resolved"), "{text}");
        assert!(
            text.contains("(no settings declared; nothing to configure)"),
            "{text}"
        );
        assert!(!text.contains("hints:"), "{text}");
        assert!(
            text.contains("more: bitty plugin info xuepoo.demo"),
            "{text}"
        );
    }

    #[test]
    fn next_steps_snippet_renders_settings_with_safe_escaping() {
        use bitty_plugin_host::manifest::{SettingDefault, SettingKind};
        let settings = vec![
            next_steps_setting("show", SettingKind::Bool, SettingDefault::Bool(true)),
            next_steps_setting("name_max_chars", SettingKind::Int, SettingDefault::Int(32)),
            next_steps_setting(
                "edge",
                SettingKind::String,
                SettingDefault::String("bot\"tom\nline".to_string()),
            ),
            next_steps_setting(
                "modules",
                SettingKind::StringArray,
                SettingDefault::Strings(vec!["workspaces".to_string(), "a\\b".to_string()]),
            ),
        ];
        let text = format_next_steps(
            "bitty-terminal.bar",
            &settings,
            &[],
            Some(Path::new(
                "/home/u/.config/bitty/plugins/bitty-terminal.bar.toml",
            )),
            Some(Path::new(
                "/home/u/.local/share/bitty/plugins/packages/bitty-terminal.bar/0.1.0",
            )),
        );
        assert!(
            text.contains("configure: /home/u/.config/bitty/plugins/bitty-terminal.bar.toml"),
            "{text}"
        );
        assert!(
            text.contains(
                "installed: /home/u/.local/share/bitty/plugins/packages/bitty-terminal.bar/0.1.0"
            ),
            "{text}"
        );
        assert!(text.contains("show = true"), "{text}");
        assert!(text.contains("name_max_chars = 32"), "{text}");
        // Quotes and newlines are escaped: valid TOML, no raw control bytes.
        assert!(text.contains("edge = \"bot\\\"tom\\nline\""), "{text}");
        assert!(!text.contains("bot\"tom\n"), "{text}");
        // Snippet truncates at three settings with a count line: the fourth
        // (`modules`) is counted, not shown.
        assert!(!text.contains("modules = "), "{text}");
        assert!(text.contains("(... 1 more setting declared)"), "{text}");
        assert!(text.contains("(undeclared keys are ignored)"), "{text}");
    }

    #[test]
    fn next_steps_hints_append_and_escape_helpers_hold() {
        // Manifest hints render under `hints:`; absent hints stay generic.
        let text = format_next_steps(
            "xuepoo.demo",
            &[],
            &["Add `modules = [\"clock\"]` to your bar config.".to_string()],
            None,
            None,
        );
        assert!(text.contains("hints:"), "{text}");
        assert!(text.contains("- Add `modules = [\"clock\"]`"), "{text}");

        // Escaping never emits raw controls or bare quotes.
        assert_eq!(escape_toml_basic("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_toml_basic("a\nb\rc\td"), "a\\nb\\rc\\td");
        assert_eq!(
            escape_toml_basic("nul\x00esc\x1bdel\x7f"),
            "nul\\u0000esc\\u001Bdel\\u007F"
        );
        assert_eq!(escape_toml_basic("caf\u{e9}\u{2014}"), "caf\u{e9}\u{2014}");
    }

    #[test]
    fn install_and_enable_print_next_steps() {
        let dir = scratch_dir("next-steps");
        let target = dir.join("init.lua");
        let config = target.display().to_string();

        // Install success prints the generic next steps (bundled manifest
        // declares no settings and no hints).
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("installed"), "{out}");
        assert!(
            out.contains("Next steps for 'bitty-terminal.shell-integration':"),
            "{out}"
        );
        assert!(
            out.contains("enable: bitty plugin enable bitty-terminal.shell-integration"),
            "{out}"
        );
        // Path separators are platform-native (`\` on Windows): normalize
        // before matching the managed `plugins/` layout.
        let portable = out.replace('\\', "/");
        assert!(
            portable.contains("plugins/bitty-terminal.shell-integration.toml"),
            "{out}"
        );
        assert!(
            out.contains("more: bitty plugin info bitty-terminal.shell-integration"),
            "{out}"
        );

        // Enable success (after a disable) prints them again.
        let (code, _, _) = run(
            &["disable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("enabled"), "{out}");
        assert!(
            out.contains("Next steps for 'bitty-terminal.shell-integration':"),
            "{out}"
        );
        assert!(
            out.contains("enable: bitty plugin enable bitty-terminal.shell-integration"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_full_clears_grants_disables_and_is_idempotent() {
        let dir = scratch_dir("revoke-full");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);

        let (code, out, _) = run(
            &["revoke", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("revoked"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(text.contains("granted = []"), "{text}");
        assert!(text.contains("enabled = false"), "{text}");
        assert!(text.contains("denied = []"), "{text}");

        // Second revoke is an idempotent no-op (no rewrite, still success).
        let (code, out, _) = run(
            &["revoke", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("already revoked"), "{out}");

        // list reflects the revoked grant count without touching other rows.
        let (code, out, _) = run(&["list"], Some(&config), "");
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("0/1"), "{out}");

        // enable fails closed: the manifest requests more than is granted.
        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_cap_records_denial_blocks_enable_and_shows_in_info() {
        let dir = scratch_dir("revoke-cap");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);

        // Single granted capability out: escalates to disabled, denial kept.
        let (code, out, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "terminal.semantic-read",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("denial kept"), "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(text.contains("granted = []"), "{text}");
        assert!(
            text.contains("denied = [\"terminal.semantic-read\"]"),
            "{text}"
        );
        assert!(text.contains("enabled = false"), "{text}");

        // info surfaces the revocation in table and json shapes.
        let (code, out, _) = run(
            &["info", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("[revoked] terminal.semantic-read"), "{out}");
        assert!(out.contains("revoked (1)"), "{out}");
        let (code, out, _) = run(
            &[
                "info",
                "bitty-terminal.shell-integration",
                "--format",
                "json",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("\"denied\":true"), "{out}");

        // enable stays blocked while the denial stands.
        let (code, _, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoke_rejects_unknown_plugin_and_ungranted_capability() {
        let dir = scratch_dir("revoke-reject");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);
        let before = std::fs::read_to_string(&file).expect("state written");

        let (code, _, _) = run(&["revoke", "xuepoo.markdown"], Some(&config), "");
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "ui.rich",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_PLUGIN);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "not.a.cap!!",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_USAGE);
        assert_eq!(
            std::fs::read_to_string(&file).expect("kept"),
            before,
            "failed revokes change nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn regrant_after_revoke_needs_consent_and_clears_denial() {
        let dir = scratch_dir("regrant");
        let target = dir.join("init.lua");
        let config = target.display().to_string();
        install_shell_integration(&config);
        let file = state_path(&dir);
        let (code, _, _) = run(
            &[
                "revoke",
                "bitty-terminal.shell-integration",
                "--cap",
                "terminal.semantic-read",
            ],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK);

        // Declined consent changes nothing: the denial persists.
        let (code, _, _) = run(
            &["install", "bitty-terminal.shell-integration"],
            Some(&config),
            "n\n",
        );
        assert_eq!(code, EXIT_GENERIC);
        let text = std::fs::read_to_string(&file).expect("kept");
        assert!(
            text.contains("denied = [\"terminal.semantic-read\"]"),
            "{text}"
        );

        // Explicit consent re-grants and clears the denial (re-grant flow).
        let (code, out, _) = run(
            &["install", "bitty-terminal.shell-integration", "--yes"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let text = std::fs::read_to_string(&file).expect("state rewritten");
        assert!(
            text.contains("granted = [\"terminal.semantic-read\"]"),
            "{text}"
        );
        assert!(text.contains("denied = []"), "{text}");
        assert!(text.contains("enabled = true"), "{text}");

        // enable works again after the re-grant.
        let (code, out, _) = run(
            &["enable", "bitty-terminal.shell-integration"],
            Some(&config),
            "",
        );
        assert_eq!(code, EXIT_OK, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_manifest_denied_codec_round_trips_and_rejects_overlap() {
        let mut records = BTreeMap::new();
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("terminal.semantic-read").expect("cap"));
        let mut denied = BTreeSet::new();
        denied.insert(CapabilityId::parse("ui.rich").expect("cap"));
        records.insert(
            "bitty-terminal.shell-integration".to_string(),
            PluginRecord {
                source: "bundled".to_string(),
                manifest_hash: "ab".repeat(32),
                enabled: false,
                granted,
                denied,
            },
        );
        let rendered = PluginState::from_records(records).render();
        assert!(rendered.contains("denied = [\"ui.rich\"]"), "{rendered}");
        let parsed = PluginState::parse(&rendered).expect("parses");
        assert_eq!(parsed.render(), rendered);

        // A capability cannot be both granted and denied (hostile edit fails closed).
        let hostile = rendered.replace(
            "denied = [\"ui.rich\"]",
            "denied = [\"terminal.semantic-read\"]",
        );
        assert!(PluginState::parse(&hostile).is_err());

        // Unknown capabilities in `denied` fail closed like `granted`.
        let hostile = rendered.replace("denied = [\"ui.rich\"]", "denied = [\"nope.everything\"]");
        assert!(PluginState::parse(&hostile).is_err());
    }

    #[test]
    fn unified_gate_piped_plugin_tables_carry_no_ansi() {
        // CTX-1065: piped plugin list/info tables carry no ANSI. The
        // harness stdout is piped, so the live gate resolves to plain.
        use std::io::IsTerminal as _;
        assert!(
            !std::io::stdout().is_terminal(),
            "test harness stdout must be piped for this assertion"
        );
        assert!(!color_enabled(false));
        assert!(!color_enabled(true));
        let table = format_list_table(&[], false);
        assert!(
            !table.contains("\u{1b}"),
            "piped plugin list must carry no ANSI, got {table:?}"
        );
        assert!(table.contains("(no plugins)"));
        // Explicit opt-out stays plain as well.
        let plain = format_list_table(&[], true);
        assert!(!plain.contains("\u{1b}"));
    }

    // ── fetch sources (CTX-1103, #1901) ────────────────────────────────────

    #[test]
    fn git_spellings_normalize_and_reject_hostile_urls() {
        // Allowed spellings normalize (git+ stripped for cloning).
        assert_eq!(
            normalize_git_source("https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("git+https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("git@example.com:a/b.git").as_deref(),
            Some("git@example.com:a/b.git")
        );
        // Port in https URL is accepted.
        assert!(normalize_git_source("https://example.com:8443/a/b.git").is_some());
        // scp without .git is accepted for this spelling.
        assert!(normalize_git_source("git@example.com:a/b").is_some());
        // Mixed-case schemes normalize to a lowercase `https://` transport
        // (git would otherwise look for a `git-remote-Https` helper).
        assert_eq!(
            normalize_git_source("Https://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );
        assert_eq!(
            normalize_git_source("GIT+HTTPS://example.com/a/b.git").as_deref(),
            Some("https://example.com/a/b.git")
        );

        // Rejected: http downgrade, file escape, ssh without git@, git://.
        for hostile in [
            "http://example.com/a/b.git",
            "file:///tmp/a.git",
            "ssh://example.com/a/b.git",
            "git://example.com/a/b.git",
            "https://example.com/a/b",
            "https://example.com/a/b.git ",
            "https://example.com/--upload-pack=evil.git",
            "https://example.com/a/b.git--config",
            "--upload-pack=touch evil",
            "--config",
            "-u",
            "git+ssh://example.com/a.git",
            "git+file:///tmp/a.git",
            "GIT@example.com:a/b.git",
            "https://user@example.com/a/b.git",
            "https://example.com/a b.git",
        ] {
            assert!(
                normalize_git_source(hostile).is_none(),
                "must reject hostile git spelling: {hostile:?}"
            );
        }
        // Rejected schemes carry a naming diagnostic.
        assert_eq!(
            rejected_scheme("file:///tmp/a.git").as_deref(),
            Some("file://")
        );
        assert_eq!(
            rejected_scheme("http://example.com/a.git").as_deref(),
            Some("http://")
        );
        // Archive suffixes select helpers case-insensitively.
        assert_eq!(
            classify_archive_kind("pkg.TAR.GZ"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(classify_archive_kind("pkg.tgz"), Some(ArchiveKind::TarGz));
        assert_eq!(
            classify_archive_kind("pkg.tar.zst"),
            Some(ArchiveKind::TarZst)
        );
        assert_eq!(classify_archive_kind("pkg.ZIP"), Some(ArchiveKind::Zip));
        assert_eq!(classify_archive_kind("pkg.toml"), None);
        // Git and archive operands select the source path (even bare names).
        assert!(is_source_operand("https://example.com/a/b.git"));
        assert!(is_source_operand("git@example.com:a/b.git"));
        assert!(is_source_operand("pkg.zip"));
        assert!(is_source_operand("./my-plugin"));
        assert!(!is_source_operand("bitty-terminal.shell-integration"));
    }

    #[test]
    fn archive_members_reject_traversal_absolute_and_windows_paths() {
        for good in [
            "bitty-plugin.toml",
            "lua/init.lua",
            "lua/",
            "./",
            "./lua/init.lua",
        ] {
            validate_archive_member(good).expect("good member validates");
        }
        for hostile in [
            "",
            "/tmp/absolute.txt",
            "../evil.txt",
            "a/../../b.txt",
            "a/../b.txt",
            "..",
            "a//b.txt",
            "C:\\evil.txt",
            "a:b.txt",
            "a\\b.txt",
        ] {
            assert!(
                validate_archive_member(hostile).is_err(),
                "must reject hostile member: {hostile:?}"
            );
        }
    }
    /// Test-only `git` with the ambient repository location scrubbed.
    ///
    /// Cargo tests can run under `git hooks` (or any parent that exports
    /// `GIT_DIR`/`GIT_WORK_TREE`/…); without scrubbing, `GIT_DIR` overrides
    /// `-C` and the fixture commands would write into the developer's real
    /// repository. Signing is pinned off so `commit` works on machines with
    /// `commit.gpgsign=true`.
    fn test_git() -> std::process::Command {
        let mut command = std::process::Command::new("git");
        for var in super::GIT_REPO_ENV_VARS {
            command.env_remove(var);
        }
        command.args(["-c", "commit.gpgsign=false"]);
        command
    }

    #[test]
    fn git_clone_fixed_argv_succeeds_on_a_local_path() {
        // Transport-level proof (bypasses scheme validation): fixed-argv
        // `git clone --filter=blob:none --depth=1 -- <src> <dest>` works and
        // inherits env. Uses a local path as the repo (no network). Skips
        // (does not panic) when `git` is absent.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = scratch_dir("git-clone");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("lua")).expect("lua dir");
        std::fs::write(repo.join("bitty-plugin.toml"), "x = 1\n").expect("manifest");
        std::fs::write(repo.join("lua/init.lua"), "-- hi\n").expect("init");
        let repo_arg = repo.display().to_string();
        let status = test_git()
            .args(["init", "-q", &repo_arg])
            .status()
            .expect("git init");
        assert!(status.success());
        for (key, value) in [("user.email", "t@t"), ("user.name", "t")] {
            let status = test_git()
                .args(["-C", &repo_arg, "config", key, value])
                .status()
                .expect("git config");
            assert!(status.success());
        }
        let status = test_git()
            .args(["-C", &repo_arg, "add", "-A"])
            .status()
            .expect("git add");
        assert!(status.success());
        let status = test_git()
            .args(["-C", &repo_arg, "commit", "-qm", "init"])
            .status()
            .expect("git commit");
        assert!(status.success());
        let dest = base.join("clone");
        run_git_clone(&repo_arg, &dest).expect("clone succeeds");
        assert!(dest.join("bitty-plugin.toml").is_file());
        assert!(dest.join("lua/init.lua").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn archive_unpack_round_trips_and_rejects_symlinks() {
        // Skip when helpers are absent (Windows without tar/unzip): the
        // production path fails closed with a diagnostic instead.
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = scratch_dir("archive-roundtrip");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("lua")).expect("lua dir");
        std::fs::write(src.join("bitty-plugin.toml"), "x = 1\n").expect("manifest");
        std::fs::write(src.join("lua/init.lua"), "-- hi\n").expect("init");
        let archive = base.join("pkg.tar.gz");
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                &archive.display().to_string(),
                "-C",
                &src.display().to_string(),
                ".",
            ])
            .status()
            .expect("tar pack");
        assert!(status.success());
        let dest = base.join("out");
        std::fs::create_dir(&dest).expect("dest dir");
        unpack_archive_into(&archive, &dest, ArchiveKind::TarGz).expect("unpacks");
        audit_extracted_tree(&dest).expect("no symlinks and within byte bound");
        assert_eq!(find_plugin_root(&dest), dest);

        // A symlink member lists fine but fails closed at audit (never
        // trusted by the package manager).
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink as symlink_file;
            let link_src = base.join("link-src");
            std::fs::create_dir_all(&link_src).expect("link src");
            symlink_file("/tmp", link_src.join("evil-link")).expect("symlink");
            std::fs::write(link_src.join("bitty-plugin.toml"), "x = 1\n").expect("m");
            let link_archive = base.join("link.tar.gz");
            let status = std::process::Command::new("tar")
                .args([
                    "-czf",
                    &link_archive.display().to_string(),
                    "-C",
                    &link_src.display().to_string(),
                    ".",
                ])
                .status()
                .expect("tar pack link");
            assert!(status.success());
            let link_dest = base.join("link-out");
            std::fs::create_dir(&link_dest).expect("link dest");
            // Extraction itself lands the link inside staging (tools refuse
            // to follow it); the audit then fails closed.
            let _ = extract_tar(&link_archive, &link_dest, ArchiveKind::TarGz);
            assert!(
                audit_extracted_tree(&link_dest).is_err(),
                "symlink member must fail closed"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn archive_size_listings_parse_gnu_bsd_and_zip_shapes() {
        // GNU `tar -tv` (the local `tar --version` shape): size sits before
        // the `YYYY-MM-DD HH:MM` date, names may hold spaces.
        let gnu = b"drwxr-xr-x fuyu/fuyu         0 2026-10-11 01:20 ./\n\
            -rw-r--r-- fuyu/fuyu         6 2026-10-11 01:20 ./file with space.txt\n\
            lrwxrwxrwx fuyu/fuyu         4 2026-10-11 01:20 ./link -> /tmp\n";
        let (count, total) = parse_tar_verbose_total(gnu).expect("gnu parses");
        assert_eq!((count, total), (3, 10));
        // BSD/libarchive `tar -tv` (`Mon DD HH:MM` dates, link-count field).
        let bsd = b"drwxr-xr-x  2 user  group  96 Oct 11 01:20 dir/\n\
            -rw-r--r--  1 user  group  12 Oct 11 01:20 file\n";
        let (count, total) = parse_tar_verbose_total(bsd).expect("bsd parses");
        assert_eq!((count, total), (2, 108));
        // A line with no date shape fails closed instead of guessing a size.
        assert!(parse_tar_verbose_total(b"garbage line\n").is_err());
        // `unzip -l` tables: rows between the separators, first field sizes.
        let zipped = b"Archive:  /tmp/a.zip\n  Length      Date    Time    Name\n\
            ---------  ---------- -----   ----\n        6  2026-10-11 01:20   file with space.txt\n\
                  0  2026-10-11 01:20   lua/\n---------                     -------\n\
                  6                     2 files\n";
        let (count, total) = parse_zip_list_total(zipped).expect("zip parses");
        assert_eq!((count, total), (2, 6));
        assert!(parse_zip_list_total(b"Archive: x\nno table here\n").is_err());
    }
}
