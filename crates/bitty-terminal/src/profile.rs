//! `bitty config profile` management (CTX-1404 / issue #2187, CTX-1589).
//!
//! Local, pre-runtime operations over `$XDG_CONFIG_HOME/bitty/profiles/`:
//!
//! - `create NAME`: write a minimal, valid Lua profile (never overwrites).
//! - `clone SRC DST`: copy the source profile bytes verbatim (never
//!   overwrites).
//! - `rename OLD NEW`: rename one profile file in place.
//! - `delete NAME`: remove one profile (never `default`, never the active
//!   profile, never a profile another profile `extends`).
//! - `list`: bounded directory metadata only; never evaluates Lua.
//! - `show NAME`: loads the single-parent `extends` chain (bounded) and
//!   prints the profile-only effective values with attribution.
//! - `use NAME`: explicit persistent activation. Validates the full chain and
//!   the prospective selector (the rewritten bytes as the user layer, merged
//!   with `NAME`'s chain through the same strict gates `config check` runs),
//!   then rewrites only the `profile = "..."` value inside the existing
//!   versioned selector, preserving comments and user overrides. Legacy
//!   `init.lua` files require the explicit `bitty init --migrate` path.
//! - `get NAME PATH`: effective value at one documented dotted schema path.
//! - `set NAME PATH VALUE`: validates the value through the same strict
//!   loader `bitty config check --profile NAME` uses — the profile chain
//!   merged under the current selector/`init.lua` user layer, including
//!   `local:<name>` theme resolution, keymap resolution, runtime bounds, and
//!   the background-image root/header/decode pipeline — edits exactly that
//!   path inside the profile's returned Lua table, and commits atomically
//!   (unique sibling temporary file, `sync_all`, rename over the target,
//!   parent-directory sync). A failed validation or write leaves the previous
//!   bytes intact. The read captures the file's identity (device, inode,
//!   length, mtime) through the open handle and re-checks it immediately
//!   before the rename, so a concurrent edit is refused with the conflict
//!   class (exit 2) instead of being silently overwritten. The check narrows
//!   the lost-update window rather than eliminating it (see
//!   [`ProfileIdentity`]).
//!   Because the typed plan merges `font`,
//!   `window`, `terminal`, `layout`, `decoration`, `scrollbar`, and
//!   `appearance` as complete tables, creating one of them in a profile pins
//!   every inherited leaf first: a one-leaf edit never silently resets the
//!   other inherited values to core defaults.
//! - `path NAME`: validates the name grammar and prints the resolved path.
//!
//! `create|clone|rename|delete|list|show|use|get|set|path` all report state;
//! `--json` (an alias for `--format json`) prints one versioned envelope.
//!
//! Profiles stay Lua fragments: the CLI never introduces a second config
//! language, never serializes the typed schema back to Lua, and never
//! evaluates arbitrary profile code outside the existing `bitty-lua`
//! sandbox. These commands never install/enable plugins, touch component
//! state, spawn a shell, or edit shell rc files. `BITTY_PROFILE` is a
//! one-launch input and is never an output target.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cli::Args;
use crate::config_cli::{ConfigFormat, ConfigValue, bounded_message, config_format};
use crate::init::{InitWriteError, render_selector_lua, write_init_config};

/// Maximum profiles listed by `config profile list` (deterministic bound).
pub(crate) const PROFILE_LIST_CAP: usize = 256;

/// Maximum directory entries inspected for completion candidates.
///
/// Completion sorts the scanned names before truncating to
/// [`PROFILE_LIST_CAP`], so the emitted prefix stays deterministic for any
/// directory up to this scan bound; beyond it, scanning stops to keep the
/// helper bounded. The bound is a small multiple of the output cap so the
/// sorted prefix is stable in normal profile directories.
pub(crate) const PROFILE_SCAN_CAP: usize = PROFILE_LIST_CAP * 4;

/// Maximum bytes of one `set` value (the config line bound).
///
/// The rendered Lua line is bounded by the same limit the loader enforces per
/// line, so a value that cannot round-trip is refused before any splice.
const MAX_PROFILE_VALUE_BYTES: usize = bitty_config::file::MAX_LINE_BYTES;

/// Maximum dotted segments accepted by `get`/`set` (defense in depth).
const MAX_SCHEMA_PATH_SEGMENTS: usize = 6;

/// Maximum dotted path bytes accepted by `get`/`set`.
const MAX_SCHEMA_PATH_BYTES: usize = 128;

/// Temporary-name attempts for one atomic profile replacement.
const PROFILE_MAX_TEMP_ATTEMPTS: u32 = 16;

/// Process-local counter for unique profile temporary names.
static PROFILE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Short usage for `bitty config profile` (stderr, fail-closed exit 2).
pub(crate) fn profile_usage() -> String {
    "usage: bitty config profile <verb> [OPERANDS] [--json]\n\
     \n\
     verbs:\n\
     \x20 create NAME          create an empty profile (never overwrites)\n\
     \x20 clone SRC DST        copy SRC's Lua bytes to a new profile DST\n\
     \x20 rename OLD NEW       rename a profile file in place\n\
     \x20 delete NAME          remove a profile (never the active one)\n\
     \x20 list                 list profile names (sorted, bounded; no Lua evaluation)\n\
     \x20 show NAME            load + validate the extends chain and print profile values\n\
     \x20 use NAME             activate NAME by rewriting only the init.lua selector\n\
     \x20 get NAME PATH        print the effective value at a documented schema path\n\
     \x20 set NAME PATH VALUE  validate and write one schema path into NAME\n\
     \x20 path NAME            print the resolved profile path without creating it\n\
     \n\
     profiles: $XDG_CONFIG_HOME/bitty/profiles/<name>.lua\n\
     --json (or --format json) prints one versioned envelope per command.\n\
     Writes are validated before any file change and committed atomically;\n\
     `default` is reserved for `bitty init`. Quote values that contain spaces;\n\
     font.fallback takes a comma-separated list. `set` on a profile that only\n\
     inherits `font`, `window`, `terminal`, `layout`, `decoration`, `scrollbar`,\n\
     or `appearance` pins every inherited leaf of that table into the profile\n\
     first (reported as pinned), so one edited leaf never resets its\n\
     siblings; required keys (font family/size, terminal scrollback) are always\n\
     included.\n\
     \n\
     use is explicit activation; it never reloads a running instance and\n\
     never installs, enables, or grants plugin/component state."
        .to_string()
}

/// A profile sub-verb parsed from `config_args`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileVerb {
    Create,
    Clone,
    Rename,
    Delete,
    List,
    Show,
    Use,
    Get,
    Set,
    Path,
}

impl ProfileVerb {
    /// Every profile sub-verb in help order (single source for shell
    /// completion).
    pub(crate) const VERBS: &[(&str, Self)] = &[
        ("create", Self::Create),
        ("clone", Self::Clone),
        ("rename", Self::Rename),
        ("delete", Self::Delete),
        ("list", Self::List),
        ("show", Self::Show),
        ("use", Self::Use),
        ("get", Self::Get),
        ("set", Self::Set),
        ("path", Self::Path),
    ];

    fn parse(raw: &str) -> Option<Self> {
        Self::VERBS
            .iter()
            .find(|(name, _)| *name == raw)
            .map(|(_, verb)| *verb)
    }

    /// `(label, description)` for every operand this verb takes, in order.
    ///
    /// `pub(crate)` so shell completion can derive its dynamic operand slots
    /// from the verb list instead of duplicating the arity by hand.
    pub(crate) fn operands(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Create | Self::Delete | Self::Show | Self::Use | Self::Path => {
                &[("NAME", "profile name")]
            }
            Self::Clone => &[
                ("SRC", "source profile name"),
                ("DST", "destination profile name"),
            ],
            Self::Rename => &[("OLD", "current profile name"), ("NEW", "new profile name")],
            Self::Get => &[("NAME", "profile name"), ("PATH", "config path")],
            Self::Set => &[
                ("NAME", "profile name"),
                ("PATH", "config path"),
                ("VALUE", "value"),
            ],
            Self::List => &[],
        }
    }

    fn usage_hint(self) -> String {
        let mut out = String::from("bitty config profile ");
        out.push_str(
            Self::VERBS
                .iter()
                .find(|(_, verb)| *verb == self)
                .map_or("", |(name, _)| name),
        );
        for (label, _) in self.operands() {
            out.push(' ');
            out.push_str(label);
        }
        out
    }
}

/// A failed profile command: stable code, message, and process exit code.
#[derive(Debug)]
struct Failure {
    /// Envelope `error.code`.
    code: &'static str,
    /// User-facing one-line (bounded) diagnostic.
    message: String,
    /// Process exit code (`2` usage/conflict, `1` filesystem failure).
    exit: i32,
}

impl Failure {
    fn new(code: &'static str, message: String, exit: i32) -> Self {
        Self {
            code,
            message,
            exit,
        }
    }

    fn usage(message: String) -> Self {
        Self::new("E_PROFILE_USAGE", message, 2)
    }

    fn invalid(message: String) -> Self {
        Self::new("E_PROFILE_INVALID", message, 2)
    }

    fn conflict(message: String) -> Self {
        Self::new("E_PROFILE_CONFLICT", message, 2)
    }

    fn missing(message: String) -> Self {
        Self::new("E_PROFILE_NOT_FOUND", message, 2)
    }

    fn io(message: String) -> Self {
        Self::new("E_PROFILE_IO", message, 1)
    }

    /// The profile tool observed an internal inconsistency after its own
    /// validated edit; fail closed rather than report a false success.
    fn internal(message: String) -> Self {
        Self::new("E_PROFILE_INTERNAL", message, 2)
    }
}

/// Successful command output: human text plus the JSON `result` object.
#[derive(Debug)]
struct Reply {
    text: String,
    result: String,
}

impl Reply {
    fn new(text: String, result: String) -> Self {
        Self { text, result }
    }
}

/// Runs `bitty config profile ...`; returns the process exit code.
pub(crate) fn run_profile_subcommand(args: &Args) -> i32 {
    let format = match config_format(args) {
        Ok(format) => format,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let Some(raw_verb) = args.config_args.first() else {
        eprintln!(
            "bitty config profile: missing sub-verb\n{}",
            profile_usage()
        );
        return 2;
    };
    let Some(verb) = ProfileVerb::parse(raw_verb) else {
        eprintln!(
            "bitty config profile: unknown sub-verb '{raw_verb}' (want create|clone|rename|delete|list|show|use|get|set|path)\n{}",
            profile_usage()
        );
        return 2;
    };
    let command = format!("config profile {raw_verb}");
    let operands = match parse_operands(verb, raw_verb, &args.config_args[1..]) {
        Ok(operands) => operands,
        Err(message) => return fail(format, &command, raw_verb, Failure::usage(message)),
    };
    let dir = match profile_dir(args) {
        Ok(dir) => dir,
        Err(message) => return fail(format, &command, raw_verb, Failure::usage(message)),
    };
    let outcome = match verb {
        ProfileVerb::Create => run_create(&dir, operands[0]),
        ProfileVerb::Clone => run_clone(args, &dir, operands[0], operands[1]),
        ProfileVerb::Rename => run_rename(args, &dir, operands[0], operands[1]),
        ProfileVerb::Delete => run_delete(args, &dir, operands[0]),
        ProfileVerb::List => run_list(args, &dir),
        ProfileVerb::Show => run_show(&dir, operands[0]),
        ProfileVerb::Use => run_use(args, &dir, operands[0]),
        ProfileVerb::Get => run_get(&dir, operands[0], operands[1]),
        ProfileVerb::Set => run_set(args, &dir, operands[0], operands[1], operands[2]),
        ProfileVerb::Path => run_path(&dir, operands[0]),
    };
    match outcome {
        Ok(reply) => {
            if format == ConfigFormat::Json {
                println!("{}", success_envelope(&command, &reply.result));
            } else {
                print!("{}", reply.text);
            }
            0
        }
        Err(failure) => fail(format, &command, raw_verb, failure),
    }
}

/// Parses the exact positional operands one verb requires.
fn parse_operands<'a>(
    verb: ProfileVerb,
    raw_verb: &str,
    rest: &'a [String],
) -> Result<Vec<&'a str>, String> {
    let expected = verb.operands();
    if let Some((_, description)) = expected.get(rest.len()) {
        return Err(format!(
            "bitty config profile {raw_verb}: missing {description}\n{}",
            profile_usage()
        ));
    }
    if let Some(extra) = rest.get(expected.len()) {
        return Err(format!(
            "bitty config profile {raw_verb}: unexpected argument '{extra}' (want `{}`)\n{}",
            verb.usage_hint(),
            profile_usage()
        ));
    }
    Ok(rest.iter().map(String::as_str).collect())
}

/// Emits a failure in the requested shape and returns its exit code.
fn fail(format: ConfigFormat, command: &str, action: &str, failure: Failure) -> i32 {
    if format == ConfigFormat::Json {
        println!("{}", error_envelope(command, action, &failure));
    }
    eprintln!("{}", failure.message);
    failure.exit
}

/// Resolves the profile directory from the same config target startup uses:
/// `--config` > `BITTY_CONFIG` > XDG probe, then a sibling `profiles/`.
fn profile_dir(args: &Args) -> Result<PathBuf, String> {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    profile_dir_from(args.config_path.as_deref(), bitty_config_env.as_deref())
}

/// Profile directory from explicit config inputs (shared by `config profile`
/// and the completion candidate helper).
pub(crate) fn profile_dir_from(
    config_path: Option<&str>,
    bitty_config_env: Option<&str>,
) -> Result<PathBuf, String> {
    let explicit = bitty_config::file::resolve_config_explicit(config_path, bitty_config_env);
    let probed = bitty_config::file::probe_config_path(explicit.as_deref()).ok_or_else(|| {
        "bitty config profile: no config root ($XDG_CONFIG_HOME or $HOME unset)".to_string()
    })?;
    let parent = probed.path.parent().ok_or_else(|| {
        format!(
            "bitty config profile: '{}' has no parent directory",
            probed.path.display()
        )
    })?;
    Ok(parent.join(bitty_config::file::PROFILES_DIR_NAME))
}

/// Scan `dir` for validated profile names.
///
/// Reads at most `entry_limit` directory entries and stops after
/// `name_limit` validated names. Returns the names sorted and de-duplicated,
/// plus whether a bound cut the scan short.
fn scan_names(
    dir: &Path,
    entry_limit: usize,
    name_limit: usize,
) -> std::io::Result<(Vec<String>, bool)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false));
        }
        Err(err) => return Err(err),
    };
    Ok(collect_names(
        entries.filter_map(Result::ok).map(|entry| entry.path()),
        entry_limit,
        name_limit,
    ))
}

/// Filter an ordered path stream into validated profile-name candidates.
///
/// Split from [`scan_names`] so the entry/name limits are testable with a
/// deterministic ordering instead of filesystem `read_dir` order.
fn collect_names(
    paths: impl Iterator<Item = PathBuf>,
    entry_limit: usize,
    name_limit: usize,
) -> (Vec<String>, bool) {
    let mut names: Vec<String> = Vec::new();
    let mut truncated = false;
    for (index, path) in paths.enumerate() {
        if index >= entry_limit || names.len() >= name_limit {
            truncated = true;
            break;
        }
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            continue;
        }
        if bitty_config::file::validate_profile_name(stem).is_ok() {
            names.push(stem.to_string());
        }
    }
    names.sort();
    names.dedup();
    (names, truncated)
}

/// Bounded profile-name candidates for shell completion.
///
/// Sorted, de-duplicated, capped at [`PROFILE_LIST_CAP`], and fail-soft: an
/// unreadable or missing directory yields an empty list (never diagnostics).
pub(crate) fn candidate_names(dir: &Path) -> Vec<String> {
    match scan_names(dir, PROFILE_SCAN_CAP, PROFILE_SCAN_CAP) {
        Ok((mut names, _)) => {
            names.truncate(PROFILE_LIST_CAP);
            names
        }
        Err(_) => Vec::new(),
    }
}

/// `bitty config profile list`.
fn run_list(args: &Args, dir: &Path) -> Result<Reply, Failure> {
    // Inspect up to the scan bound (not the output cap) so unrelated files
    // cannot exhaust the budget before valid profiles are reached.
    let (names, truncated) = match scan_names(dir, PROFILE_SCAN_CAP, PROFILE_LIST_CAP) {
        Ok(result) => result,
        Err(err) => {
            return Err(Failure::io(format!(
                "bitty config profile list: cannot read '{}': {err}",
                dir.display()
            )));
        }
    };
    if truncated {
        eprintln!(
            "bitty config profile list: stopped at {PROFILE_SCAN_CAP} entries / \
             {PROFILE_LIST_CAP} profiles; narrow the directory"
        );
    }
    let active = active_profile_name(args);
    let mut text = String::new();
    let mut entries = String::new();
    for (index, name) in names.iter().enumerate() {
        text.push_str(name);
        text.push('\n');
        if index > 0 {
            entries.push(',');
        }
        let path = profile_path(dir, name);
        let is_active = active.as_deref() == Some(name.as_str());
        entries.push_str(&format!(
            "{{\"name\":{},\"path\":{},\"active\":{}}}",
            json_string(name),
            json_path(&path),
            is_active
        ));
    }
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"list\",\"count\":{},\"truncated\":{},\"profiles\":[{}]}}",
        names.len(),
        truncated,
        entries
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile path NAME`.
fn run_path(dir: &Path, raw_name: &str) -> Result<Reply, Failure> {
    let name = valid_name("path", raw_name)?;
    let path = profile_path(dir, &name);
    let exists = path.is_file();
    let text = format!("{}\n", path.display());
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"path\",\"name\":{},\"path\":{},\"exists\":{exists}}}",
        json_string(&name),
        json_path(&path)
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile show NAME`.
fn run_show(dir: &Path, raw_name: &str) -> Result<Reply, Failure> {
    let name = valid_name("show", raw_name)?;
    let path = profile_path(dir, &name);
    ensure_present("show", &name, &path)?;
    let chain = bitty_config::file::load_profile_chain_in_dir(&name, dir)
        .map_err(|err| invalid_profile("show", &name, &err))?;
    let merged = bitty_config::file::resolve_effective_with_profiles(
        None,
        chain.clone(),
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| invalid_profile("show", &name, &err))?;

    let mut text = format!("profile = \"{name}\"\npath = \"{}\"\n", path.display());
    let mut layers = String::new();
    for (index, layer) in chain.iter().enumerate() {
        let source = layer.source.path.as_deref().unwrap_or("(unknown)");
        text.push_str(&format!("layer[{index}] = \"{source}\"\n"));
        if index > 0 {
            layers.push(',');
        }
        layers.push_str(&json_string(source));
    }
    let mut keys = String::new();
    for (index, key) in SHOW_KEYS.iter().enumerate() {
        let Some(field) = schema_field_entry(key) else {
            return Err(Failure::invalid(format!(
                "bitty config profile show: internal schema key '{key}' is missing"
            )));
        };
        let value = (field.get)(&merged.effective);
        let source = profile_source_label(&merged, field.path);
        text.push_str(&format!(
            "{} = {} ({source})\n",
            field.path,
            display_value(&value, true)
        ));
        if index > 0 {
            keys.push(',');
        }
        keys.push_str(&format!(
            "{{\"key\":{},\"value\":{},\"type\":{},\"layer\":{},\"source\":{}}}",
            json_string(field.path),
            value.json(),
            json_string(value.kind()),
            json_string(&merged.source_of(field.path).map_or_else(
                || String::from("core-defaults"),
                |found| found.layer.label().to_string()
            )),
            json_string(&source)
        ));
    }
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"show\",\"name\":{},\"path\":{},\"layers\":[{}],\"count\":{},\"keys\":[{}]}}",
        json_string(&name),
        json_path(&path),
        layers,
        SHOW_KEYS.len(),
        keys
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile use NAME`.
fn run_use(args: &Args, dir: &Path, raw_name: &str) -> Result<Reply, Failure> {
    let name = valid_name("use", raw_name)?;
    let profile_path = profile_path(dir, &name);
    ensure_present("use", &name, &profile_path)?;
    // Reject a chain that is invalid on its own (syntax, shape, schema,
    // cycle) before any mutation. The strict post-merge gates are deliberately
    // NOT run here: they decide on the cross-layer merge below, where the
    // selector's own approved roots, theme, keymaps, and outline overrides
    // participate. Validating the profile alone refused configurations
    // `config check` accepts.
    bitty_config::file::load_profile_chain_in_dir(&name, dir)
        .map_err(|err| invalid_profile("use", &name, &err))?;
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let probed = bitty_config::file::probe_config_path(explicit.as_deref()).ok_or_else(|| {
        Failure::usage(
            "bitty config profile use: no config root ($XDG_CONFIG_HOME or $HOME unset)"
                .to_string(),
        )
    })?;
    if probed.fallback_name {
        // `config.lua` is the fallback *name*, not proof of a legacy file: a
        // migration (or an earlier `init` on that alias) can leave a valid
        // versioned selector there. The parse below decides by content, so a
        // migrated alias stays switchable while a genuine legacy file still
        // fails closed with the migration hint (#2349).
        eprintln!(
            "bitty config profile use: using fallback '{}' (canonical is 'init.lua')",
            probed.path.display()
        );
    }
    let selector = probed.path;
    let already_active = selector_profile_name(args).as_deref() == Some(name.as_str());
    if already_active {
        let text = format!(
            "bitty config profile use: profile '{name}' is already active in '{}'\n",
            selector.display()
        );
        let result = use_result(&name, &selector, false, None);
        return Ok(Reply::new(text, result));
    }
    if !selector.exists() {
        let content = render_selector_lua(&name);
        validate_prospective_selector(dir, &name, &selector, &content)?;
        return match write_init_config(&selector, &content, false) {
            Ok(_) => {
                let text = format!(
                    "bitty config profile use: activated '{name}' in '{}'\n",
                    selector.display()
                );
                Ok(Reply::new(text, use_result(&name, &selector, true, None)))
            }
            Err(InitWriteError::Refused(message)) => Err(Failure::invalid(message)),
            Err(InitWriteError::Io(message)) => Err(Failure::io(message)),
        };
    }
    // Bound-check and parse the exact bytes that will be rewritten, so the
    // selector is never read unbounded and the in-memory edit is based on a
    // validated preimage.
    let meta = match std::fs::metadata(&selector) {
        Ok(meta) => meta,
        Err(err) => {
            return Err(Failure::io(format!(
                "bitty config profile use: cannot inspect '{}': {err}",
                selector.display()
            )));
        }
    };
    if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
        return Err(Failure::invalid(format!(
            "bitty config profile use: '{}' exceeds {} bytes; refusing to rewrite it",
            selector.display(),
            bitty_config::file::MAX_CONFIG_FILE_BYTES
        )));
    }
    let content = match std::fs::read_to_string(&selector) {
        Ok(content) => content,
        Err(err) => {
            return Err(Failure::io(format!(
                "bitty config profile use: cannot read '{}': {err}",
                selector.display()
            )));
        }
    };
    let source = bitty_config::plan::ConfigSource::new(
        bitty_config::plan::LayerKind::User,
        Some(selector.display().to_string()),
    );
    let layer = match bitty_config::file::parse_lua_config(&content, &source) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!(
                "bitty config profile use: '{}' is not a valid selector: {err}",
                selector.display()
            );
            return Err(Failure::invalid(
                "bitty config profile use: run `bitty init --migrate` to migrate a legacy configuration"
                    .to_string(),
            ));
        }
    };
    if layer.selector.is_none() {
        return Err(Failure::invalid(format!(
            "bitty config profile use: '{}' is a legacy configuration, not a selector; run `bitty init --migrate` first",
            selector.display()
        )));
    }
    let Some(rewritten) = rewrite_selector_profile(&content, &name) else {
        return Err(Failure::invalid(format!(
            "bitty config profile use: cannot locate exactly one top-level short-quoted \
             `profile = \"...\"` assignment in '{}'; edit a long-bracket value manually",
            selector.display()
        )));
    };
    // Re-parse the exact bytes about to be written and require the selector
    // to name the requested profile: a rewrite that matched the wrong
    // occurrence must never report success.
    let rewritten_plan = match bitty_config::file::parse_lua_config(&rewritten, &source) {
        Ok(plan) => plan,
        Err(err) => {
            return Err(Failure::invalid(format!(
                "bitty config profile use: rewritten selector is invalid ({err}); refusing to write"
            )));
        }
    };
    match rewritten_plan.selector {
        Some(found) if found.profile == name => {}
        Some(found) => {
            return Err(Failure::invalid(format!(
                "bitty config profile use: rewritten selector still names '{}' instead of \
                 '{name}'; refusing to write",
                found.profile
            )));
        }
        None => {
            return Err(Failure::invalid(
                "bitty config profile use: rewritten selector lost its profile field; refusing to write"
                    .to_string(),
            ));
        }
    }
    // The selector is a user layer too: its own `appearance.theme` (for
    // example `local:missing`) must resolve for the *prospective* merge, not
    // just the target profile's chain. Validate before the write so `use`
    // never activates a configuration `config check` rejects.
    validate_prospective_selector(dir, &name, &selector, &rewritten)?;
    match write_init_config(&selector, &rewritten, true) {
        Ok(outcome) => {
            let mut text = String::new();
            if let Some(backup) = &outcome.backup {
                text.push_str(&format!(
                    "bitty config profile use: backed up previous selector to '{}'\n",
                    backup.display()
                ));
            }
            text.push_str(&format!(
                "bitty config profile use: activated '{name}' in '{}'\n",
                selector.display()
            ));
            Ok(Reply::new(
                text,
                use_result(&name, &selector, true, outcome.backup.as_deref()),
            ))
        }
        Err(InitWriteError::Refused(message)) => Err(Failure::invalid(message)),
        Err(InitWriteError::Io(message)) => Err(Failure::io(message)),
    }
}

/// JSON `result` object for `use`.
fn use_result(name: &str, selector: &Path, changed: bool, backup: Option<&Path>) -> String {
    format!(
        "{{\"kind\":\"profile\",\"action\":\"use\",\"name\":{},\"selector\":{},\"active\":true,\"changed\":{changed},\"backup\":{}}}",
        json_string(name),
        json_path(selector),
        backup.map_or_else(|| String::from("null"), json_path)
    )
}

/// Validates the configuration a completed switch would load: the prospective
/// selector bytes as the user layer, merged with `name`'s profile chain and
/// run through the same strict post-merge gates `bitty config check` runs.
///
/// The selector is not merely a pointer: it can carry its own overrides
/// (`appearance.theme`, background images, keymaps). Checking only the target
/// profile chain let `use` activate a selector that `config check` then
/// rejected (for example `appearance = { theme = "local:missing" }`).
fn validate_prospective_selector(
    dir: &Path,
    name: &str,
    selector: &Path,
    selector_bytes: &str,
) -> Result<(), Failure> {
    let source = bitty_config::plan::ConfigSource::new(
        bitty_config::plan::LayerKind::User,
        Some(selector.display().to_string()),
    );
    let plan = bitty_config::file::parse_lua_config(selector_bytes, &source).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile use: prospective selector '{}' is invalid: {err}",
            selector.display()
        ))
    })?;
    let plan = bitty_config::migrate(plan).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile use: prospective selector '{}' is invalid: {err}",
            selector.display()
        ))
    })?;
    let chain = bitty_config::file::load_profile_chain_in_dir(name, dir).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile use: switching to '{name}' would load an invalid profile \
             chain: {err}"
        ))
    })?;
    let merged = bitty_config::file::resolve_effective_with_profiles(
        Some(bitty_config::LayeredPlan::new(source, plan)),
        chain,
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile use: switching to '{name}' would leave '{}' invalid: {err}",
            selector.display()
        ))
    })?;
    strict_post_merge_checks(
        "use",
        &format!(
            "switching to '{name}' would leave '{}' invalid",
            selector.display()
        ),
        &merged.effective,
    )
}

/// `bitty config profile create NAME`.
fn run_create(dir: &Path, raw_name: &str) -> Result<Reply, Failure> {
    let name = valid_name("create", raw_name)?;
    reject_reserved("create", &name)?;
    let path = profile_path(dir, &name);
    ensure_absent("create", &name, &path)?;
    let content = new_profile_lua(&name);
    validate_new_profile(&content, &path).map_err(|err| invalid_profile("create", &name, &err))?;
    create_exclusive(&path, &content).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            exists_failure("create", &name, &path)
        } else {
            io_failure("create", &format!("create '{}'", path.display()), &err)
        }
    })?;
    let text = format!(
        "bitty config profile create: created '{name}' ('{}')\n\
         activate it with `bitty config profile use {name}`\n",
        path.display()
    );
    Ok(Reply::new(
        text,
        profile_result("create", &name, &path, ",\"changed\":true"),
    ))
}

/// `bitty config profile clone SRC DST`.
fn run_clone(args: &Args, dir: &Path, raw_src: &str, raw_dst: &str) -> Result<Reply, Failure> {
    let src = valid_name("clone", raw_src)?;
    let dst = valid_name("clone", raw_dst)?;
    reject_reserved("clone", &dst)?;
    let src_path = profile_path(dir, &src);
    let dst_path = profile_path(dir, &dst);
    ensure_present("clone", &src, &src_path)?;
    ensure_absent("clone", &dst, &dst_path)?;
    // Read the source exactly once and parse/validate those same bytes. A
    // second read (the old chain-load-then-copy shape) could observe a file
    // changed by an editor, a hardlink writer, or a symlink swap between the
    // validated chain and the copied bytes, succeeding with a source that was
    // never validated.
    let source = read_clone_source(dir, &src, &src_path)?;
    let CloneSource { content, chain } = source;
    let cross = merge_with_current_user("clone", &src, args, chain)?;
    strict_post_merge_checks("clone", &cross.subject(&src), &cross.merged.effective)?;
    create_exclusive(&dst_path, &content).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            exists_failure("clone", &dst, &dst_path)
        } else {
            io_failure("clone", &format!("create '{}'", dst_path.display()), &err)
        }
    })?;
    let text = format!(
        "bitty config profile clone: cloned '{src}' to '{dst}' ('{}')\n",
        dst_path.display()
    );
    let result = profile_result(
        "clone",
        &dst,
        &dst_path,
        &format!(",\"source\":{},\"changed\":true", json_string(&src)),
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile rename OLD NEW`.
fn run_rename(args: &Args, dir: &Path, raw_old: &str, raw_new: &str) -> Result<Reply, Failure> {
    let old = valid_name("rename", raw_old)?;
    let new = valid_name("rename", raw_new)?;
    reject_reserved("rename", &old)?;
    reject_reserved("rename", &new)?;
    if old == new {
        return Err(Failure::conflict(format!(
            "bitty config profile rename: source and destination are the same profile ('{old}')"
        )));
    }
    let old_path = profile_path(dir, &old);
    let new_path = profile_path(dir, &new);
    ensure_present("rename", &old, &old_path)?;
    ensure_absent("rename", &new, &new_path)?;
    reject_active("rename", &old, args)?;
    reject_referenced("rename", &old, dir)?;
    if let Err(err) = bitty_config::file::load_profile_chain_in_dir(&old, dir) {
        return Err(invalid_profile("rename", &old, &err));
    }
    // The pre-check above gives the clear "already exists" error, but it is
    // only a check: a destination created between it and the rename would be
    // silently overwritten by POSIX `rename`. Use the no-replace form where
    // the platform has one, and re-report the race as the same conflict.
    match commit_profile_rename(&old_path, &new_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(exists_failure("rename", &new, &new_path));
        }
        Err(err) => {
            return Err(io_failure(
                "rename",
                &format!(
                    "rename '{}' to '{}'",
                    old_path.display(),
                    new_path.display()
                ),
                &err,
            ));
        }
    }
    sync_parent_dir(&new_path);
    let text = format!(
        "bitty config profile rename: renamed '{old}' to '{new}' ('{}')\n",
        new_path.display()
    );
    let result = profile_result(
        "rename",
        &new,
        &new_path,
        &format!(",\"previous\":{},\"changed\":true", json_string(&old)),
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile delete NAME`.
fn run_delete(args: &Args, dir: &Path, raw_name: &str) -> Result<Reply, Failure> {
    let name = valid_name("delete", raw_name)?;
    reject_reserved("delete", &name)?;
    let path = profile_path(dir, &name);
    ensure_present("delete", &name, &path)?;
    reject_active("delete", &name, args)?;
    reject_referenced("delete", &name, dir)?;
    if let Err(err) = std::fs::remove_file(&path) {
        return Err(io_failure(
            "delete",
            &format!("remove '{}'", path.display()),
            &err,
        ));
    }
    let text = format!(
        "bitty config profile delete: deleted '{name}' ('{}')\n",
        path.display()
    );
    Ok(Reply::new(
        text,
        profile_result("delete", &name, &path, ",\"deleted\":true"),
    ))
}

/// `bitty config profile get NAME PATH`.
fn run_get(dir: &Path, raw_name: &str, raw_key: &str) -> Result<Reply, Failure> {
    let name = valid_name("get", raw_name)?;
    let field = schema_field("get", raw_key)?;
    let path = profile_path(dir, &name);
    ensure_present("get", &name, &path)?;
    let chain = bitty_config::file::load_profile_chain_in_dir(&name, dir)
        .map_err(|err| invalid_profile("get", &name, &err))?;
    let merged = bitty_config::file::resolve_effective_with_profiles(
        None,
        chain,
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| invalid_profile("get", &name, &err))?;
    let value = (field.get)(&merged.effective);
    let source = profile_source_label(&merged, field.path);
    let text = format!(
        "{} = {} ({source})\n",
        field.path,
        display_value(&value, false)
    );
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"get\",\"name\":{},\"path\":{},\"key\":{},\"value\":{},\"type\":{},\"layer\":{},\"source\":{}}}",
        json_string(&name),
        json_path(&path),
        json_string(field.path),
        value.json(),
        json_string(value.kind()),
        json_string(&merged.source_of(field.path).map_or_else(
            || String::from("core-defaults"),
            |found| found.layer.label().to_string()
        )),
        json_string(&source)
    );
    Ok(Reply::new(text, result))
}

/// `bitty config profile set NAME PATH VALUE`.
fn run_set(
    args: &Args,
    dir: &Path,
    raw_name: &str,
    raw_key: &str,
    raw_value: &str,
) -> Result<Reply, Failure> {
    let name = valid_name("set", raw_name)?;
    let field = schema_field("set", raw_key)?;
    let literal = render_literal(field, raw_value).map_err(|message| {
        Failure::invalid(format!(
            "bitty config profile set: invalid value for '{}': {message}",
            field.path
        ))
    })?;
    let path = profile_path(dir, &name);
    ensure_present("set", &name, &path)?;
    let (content, identity) = read_profile("set", &name, &path)?;
    // Seed-only view: the profile chain restricted to its own layers. A leaf
    // that is invalid in isolation (for example an outline color the selector
    // overrides) must still be pinnable, so this merge does not validate; the
    // cross-layer merge decides before any write.
    let current = merged_profile_unvalidated(dir, "set", &name)?;
    let segments: Vec<&str> = field.path.split('.').collect();
    let table = segments[0];
    let mut updated = content;
    let mut pinned: Vec<&str> = Vec::new();
    // The typed plan merges these tables as complete structs: creating one
    // without its inherited leaves would reset every omitted sibling to the
    // core default. Pin the resolved inherited table first, then edit.
    if PINNED_TABLES.contains(&table) {
        let entry = parse_profile_plan(&updated, &path)
            .map_err(|err| invalid_profile("set", &name, &err))?;
        if !declares_table(&entry.plan, table) {
            let seed = inherited_table_literal(table, &current.effective);
            let pinned_body = set_lua_path(&updated, &[table], &seed).map_err(|message| {
                Failure::invalid(format!(
                    "bitty config profile set: cannot pin inherited '{table}' values into profile \
                     '{name}': {message}"
                ))
            })?;
            if let Some(violation) = profile_bound_violation(&pinned_body) {
                return Err(Failure::invalid(format!(
                    "bitty config profile set: cannot pin inherited '{table}' into '{name}': \
                     {violation}; edit the profile manually"
                )));
            }
            updated = pinned_body;
            pinned.push(table);
        }
    }
    let updated = set_lua_path(&updated, &segments, &literal).map_err(|message| {
        Failure::invalid(format!(
            "bitty config profile set: cannot set '{}' in profile '{name}': {message}",
            field.path
        ))
    })?;
    // Recheck the final body, not just the seeded table: an escaping literal or
    // a leaf appended beside a near-bound pin expands the same line past the
    // loader limit. Naming the pin and the bound here keeps the diagnostic
    // actionable instead of surfacing a generic parse error for bytes the user
    // never wrote.
    if let Some(violation) = profile_bound_violation(&updated) {
        let message = if let Some(table) = pinned.last() {
            format!(
                "bitty config profile set: cannot pin inherited '{table}' into '{name}': \
                 {violation}; edit the profile manually"
            )
        } else {
            format!(
                "bitty config profile set: cannot set '{}' in profile '{name}': {violation}; \
                 edit the profile manually",
                field.path
            )
        };
        return Err(Failure::invalid(message));
    }
    let merged = merge_candidate(args, dir, &name, &path, &updated)?;
    // Defense in depth: the bytes just spliced must read back as the requested
    // value. A mismatch means the editor wrote somewhere the merge does not
    // read (an alias, a duplicate key the parser resolved differently, or an
    // unexpected `extends` interaction); never report success for that.
    let actual = (field.get)(&merged.effective);
    let expected = requested_value(field, raw_value);
    if actual != expected {
        return Err(Failure::internal(format!(
            "bitty config profile set: internal error: '{}' re-reads as {} instead of {}; \
             refusing to write profile '{name}'",
            field.path,
            actual.json(),
            expected.json()
        )));
    }
    write_profile_atomic(&path, &updated, Some(&identity)).map_err(|err| match err {
        ProfileWriteError::Changed => changed_failure("set", &path),
        ProfileWriteError::Io(err) => {
            io_failure("set", &format!("replace '{}'", path.display()), &err)
        }
    })?;
    let value = (field.get)(&merged.effective);
    let source = profile_source_label(&merged, field.path);
    let mut text = format!(
        "bitty config profile set: set '{}' = {} in '{name}' ('{}')\n",
        field.path,
        display_value(&value, false),
        path.display()
    );
    if !pinned.is_empty() {
        let names = pinned
            .iter()
            .map(|table| format!("'{table}'"))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "bitty config profile set: pinned {names} into '{name}'\n"
        ));
    }
    let pinned_json = pinned
        .iter()
        .map(|table| json_string(table))
        .collect::<Vec<_>>()
        .join(",");
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"set\",\"name\":{},\"path\":{},\"key\":{},\"value\":{},\"type\":{},\"layer\":{},\"source\":{},\"changed\":true,\"pinned\":[{}]}}",
        json_string(&name),
        json_path(&path),
        json_string(field.path),
        value.json(),
        json_string(value.kind()),
        json_string(&merged.source_of(field.path).map_or_else(
            || String::from("core-defaults"),
            |found| found.layer.label().to_string()
        )),
        json_string(&source),
        pinned_json
    );
    Ok(Reply::new(text, result))
}

// ---------------------------------------------------------------------------
// Shared validation and filesystem helpers
// ---------------------------------------------------------------------------

/// Validates a profile name for one verb (fail-closed exit 2).
fn valid_name(verb: &str, raw: &str) -> Result<String, Failure> {
    bitty_config::file::validate_profile_name(raw)
        .map_err(|err| Failure::invalid(format!("bitty config profile {verb}: {err}")))
}

/// `<dir>/<name>.lua` for an already-validated name.
fn profile_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lua"))
}

/// Refuses the `default` profile name for creating/removing/renaming.
fn reject_reserved(verb: &str, name: &str) -> Result<(), Failure> {
    if name == "default" {
        return Err(Failure::conflict(format!(
            "bitty config profile {verb}: profile 'default' is reserved for `bitty init`; \
             use `bitty init` to generate it or `bitty config profile clone default <name>`"
        )));
    }
    Ok(())
}

/// Existing-target conflict with a stable message.
fn exists_failure(verb: &str, name: &str, path: &Path) -> Failure {
    Failure::conflict(format!(
        "bitty config profile {verb}: profile '{name}' already exists ('{}'); refusing to overwrite",
        path.display()
    ))
}

/// Preimage-changed conflict: the file moved under the command's feet.
fn changed_failure(verb: &str, path: &Path) -> Failure {
    Failure::conflict(format!(
        "bitty config profile {verb}: '{}' changed while editing; refusing to overwrite the \
         concurrent edit (re-run the command on the current file)",
        path.display()
    ))
}

/// Requires the profile file to exist and be a regular file (never a link).
fn ensure_present(verb: &str, name: &str, path: &Path) -> Result<(), Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(Failure::invalid(format!(
                    "bitty config profile {verb}: refusing to follow symlink '{}'",
                    path.display()
                )));
            }
            if !meta.is_file() {
                return Err(Failure::invalid(format!(
                    "bitty config profile {verb}: '{}' is not a regular file",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(Failure::missing(format!(
            "bitty config profile {verb}: profile '{name}' not found ('{}')",
            path.display()
        ))),
        Err(err) => Err(io_failure(
            verb,
            &format!("inspect '{}'", path.display()),
            &err,
        )),
    }
}

/// Requires the profile file to be absent (any existing entry conflicts).
fn ensure_absent(verb: &str, name: &str, path: &Path) -> Result<(), Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(exists_failure(verb, name, path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_failure(
            verb,
            &format!("inspect '{}'", path.display()),
            &err,
        )),
    }
}

/// Refuses to delete or rename the profile the next launch would activate.
///
/// Either selection source counts: a one-launch `--profile`/`BITTY_PROFILE`
/// value and the persisted `init.lua` selector. Deleting only the *other*
/// source's target would still break the remaining selector.
fn reject_active(verb: &str, name: &str, args: &Args) -> Result<(), Failure> {
    let env = std::env::var("BITTY_PROFILE").ok();
    let requested =
        bitty_config::file::resolve_profile_request(args.profile.as_deref(), env.as_deref());
    let active =
        requested.as_deref() == Some(name) || selector_profile_name(args).as_deref() == Some(name);
    if active {
        return Err(Failure::conflict(format!(
            "bitty config profile {verb}: profile '{name}' is active (--profile/BITTY_PROFILE or the \
             init.lua selector); drop the flag or switch with `bitty config profile use <other>`"
        )));
    }
    Ok(())
}

/// Refuses to delete or rename a profile another profile `extends`.
fn reject_referenced(verb: &str, name: &str, dir: &Path) -> Result<(), Failure> {
    let dependents = referencing_profiles(dir, name);
    if dependents.is_empty() {
        return Ok(());
    }
    let list = dependents
        .iter()
        .map(|dependent| format!("'{dependent}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(Failure::conflict(format!(
        "bitty config profile {verb}: profile '{name}' is extended by {list}; update that profile first"
    )))
}

/// Profiles whose top-level `extends` names `target` (bounded, fail-soft on
/// unreadable or invalid files).
fn referencing_profiles(dir: &Path, target: &str) -> Vec<String> {
    let Ok((names, _)) = scan_names(dir, PROFILE_SCAN_CAP, PROFILE_LIST_CAP) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in names {
        if name == target {
            continue;
        }
        let path = profile_path(dir, &name);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if metadata.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let source = bitty_config::plan::ConfigSource::new(
            bitty_config::plan::LayerKind::Profile,
            Some(path.display().to_string()),
        );
        if let Ok(plan) = bitty_config::file::parse_lua_config(&content, &source) {
            if plan.extends.as_deref() == Some(target) {
                out.push(name);
            }
        }
    }
    out
}

/// Effective profile name for list/delete/rename: `--profile`/`BITTY_PROFILE`
/// first, then the persisted selector (the startup order).
fn active_profile_name(args: &Args) -> Option<String> {
    let env = std::env::var("BITTY_PROFILE").ok();
    bitty_config::file::resolve_profile_request(args.profile.as_deref(), env.as_deref())
        .or_else(|| selector_profile_name(args))
}

/// Profile named by the persisted `init.lua` selector only.
fn selector_profile_name(args: &Args) -> Option<String> {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let probed = bitty_config::file::probe_config_path(explicit.as_deref())?;
    if !probed.path.exists() {
        return None;
    }
    let layer = bitty_config::file::load_user_layer(&probed.path).ok()?;
    layer.plan.selector.map(|selector| selector.profile)
}

/// Identity of the profile bytes one command read, captured from the **open
/// handle** (`fstat`), never from a later path re-stat.
///
/// [`run_set`] hands this to the atomic replace, which re-reads the current
/// file immediately before the rename and refuses the commit when the
/// identity moved. That closes the common lost-update window (an editor or a
/// second `set` replacing the file between the read and the write), but it is
/// a check, not a compare-and-swap: a writer that lands after the re-check
/// and before `rename` is still overwritten. POSIX offers no way to make
/// "replace only if this exact file is still there" atomic without holding a
/// lock (which would exclude every other writer, including the user's
/// editor), so the residual window is documented rather than claimed closed.
///
/// Identity is also only as precise as the metadata: a same-length in-place
/// edit on a filesystem whose mtime granularity is coarser than the edit
/// interval is not detected. Replacing the file (a new inode, as every
/// editor's atomic save does) is always detected on Unix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileIdentity {
    /// Byte length at read time.
    len: u64,
    /// Modification time at read time (best effort; `None` when the platform
    /// cannot report it).
    modified: Option<std::time::SystemTime>,
    /// Owning device (Unix).
    #[cfg(unix)]
    dev: u64,
    /// Inode number (Unix).
    #[cfg(unix)]
    ino: u64,
}

impl ProfileIdentity {
    /// Captures identity from the metadata of an already-open handle.
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        }
    }
}

/// Reads one profile with the loader's size bound.
///
/// One open handle supplies both the bound check and the bytes, and its
/// metadata is the identity the caller compares before replacing the file.
/// A path re-stat after the read could observe a different file than the one
/// actually read (a replacement between `stat` and `read`), which is exactly
/// the lost-update this identity exists to detect.
fn read_profile(verb: &str, name: &str, path: &Path) -> Result<(String, ProfileIdentity), Failure> {
    let file = std::fs::File::open(path)
        .map_err(|err| io_failure(verb, &format!("read '{}'", path.display()), &err))?;
    let meta = file
        .metadata()
        .map_err(|err| io_failure(verb, &format!("inspect '{}'", path.display()), &err))?;
    if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: profile '{name}' exceeds {} bytes; refusing to edit it",
            bitty_config::file::MAX_CONFIG_FILE_BYTES
        )));
    }
    let identity = ProfileIdentity::from_metadata(&meta);
    // Bound the read itself, not only the pre-read size: a file that grows
    // while it is being read must not turn one command into an unbounded
    // allocation.
    let limit = bitty_config::file::MAX_CONFIG_FILE_BYTES as u64;
    let mut content = String::new();
    file.take(limit + 1)
        .read_to_string(&mut content)
        .map_err(|err| io_failure(verb, &format!("read '{}'", path.display()), &err))?;
    if content.len() as u64 > limit {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: profile '{name}' exceeds {} bytes; refusing to edit it",
            bitty_config::file::MAX_CONFIG_FILE_BYTES
        )));
    }
    Ok((content, identity))
}

/// Parses one candidate profile body as a `Profile` layer.
fn parse_profile_plan(
    content: &str,
    path: &Path,
) -> Result<bitty_config::LayeredPlan, bitty_config::ConfigError> {
    let source = bitty_config::plan::ConfigSource::new(
        bitty_config::plan::LayerKind::Profile,
        Some(path.display().to_string()),
    );
    let plan = bitty_config::file::parse_lua_config(content, &source)?;
    let plan = bitty_config::migrate(plan)?;
    Ok(bitty_config::LayeredPlan::new(source, plan))
}

/// One clone source: the exact bytes read once and the validated chain ending
/// in the layer parsed from those bytes.
#[derive(Debug)]
struct CloneSource {
    /// Bytes read from the source; copied verbatim on success.
    content: String,
    /// Base-first `extends` chain whose last link is the candidate parsed from
    /// `content`.
    chain: Vec<bitty_config::LayeredPlan>,
}

/// Reads a clone source exactly once and parses/validates that same content.
///
/// This is the only read of the source profile on the clone path. The parser
/// re-checks the file and line bounds on the actual string, so a file that
/// grows after the metadata check still fails before any copy.
fn read_clone_source(dir: &Path, src: &str, src_path: &Path) -> Result<CloneSource, Failure> {
    // Clone creates a new destination exclusively, so the source identity is
    // not used: a source edit only changes the bytes being copied.
    let (content, _identity) = read_profile("clone", src, src_path)?;
    clone_source_from_content(dir, src, src_path, content)
}

/// Parses `content` as the clone candidate and loads its `extends` parents.
///
/// Split from [`read_clone_source`] so the single-read invariant is testable:
/// the returned [`CloneSource::content`] is the exact string that was parsed
/// and is what `clone` writes to the destination.
fn clone_source_from_content(
    dir: &Path,
    src: &str,
    src_path: &Path,
    content: String,
) -> Result<CloneSource, Failure> {
    let candidate = parse_profile_plan(&content, src_path)
        .map_err(|err| invalid_profile("clone", src, &err))?;
    let mut chain = match candidate.plan.extends.as_deref() {
        Some(parent) => {
            // Treat the source as already visited: a parent chain that names it
            // again (directly or through an ancestor) is a cycle and fails
            // before the source file is read a second time.
            let ancestors = [src.to_string()];
            bitty_config::file::load_profile_chain_in_dir_with_ancestors(parent, dir, &ancestors)
                .map_err(|err| invalid_profile("clone", src, &err))?
        }
        None => Vec::new(),
    };
    chain.push(candidate);
    if chain.len() > bitty_config::file::MAX_PROFILE_CHAIN_LEN {
        // The loader bounds the parent chain it loaded; the candidate is the
        // extra link, so the assembled chain needs the same bound here.
        return Err(invalid_profile(
            "clone",
            src,
            &bitty_config::ConfigError::validation(
                "extends",
                format!(
                    "profile extends chain exceeds {} profiles",
                    bitty_config::file::MAX_PROFILE_CHAIN_LEN
                ),
            ),
        ));
    }
    Ok(CloneSource { content, chain })
}

/// Validates a brand-new profile body (parse + migrate + strict merge) and
/// resolves the same `local:<name>` theme selection `config check` does.
fn validate_new_profile(content: &str, path: &Path) -> Result<(), bitty_config::ConfigError> {
    let layer = parse_profile_plan(content, path)?;
    let merged = bitty_config::file::resolve_effective_with_profiles(
        None,
        vec![layer],
        &bitty_config::file::CliOverrides::default(),
    )?;
    let mut effective = merged.effective;
    bitty_config::local_theme::apply_selection(&mut effective)
        .map_err(|err| bitty_config::ConfigError::validation("appearance.theme", err))?;
    Ok(())
}

/// Validates a candidate edit through the exact strict loader `config check`
/// uses: parse the new body, resolve the current `extends` chain, substitute
/// the edited entry, and merge/validate the chain — including the current user
/// selector above it and the `local:<name>` theme selection the strict loader
/// resolves.
///
/// The merge the gates see is the one `config check --profile NAME` sees
/// (user layer + candidate chain). The returned value is the profile-only
/// merge, unvalidated: pin seeding, the round-trip re-read, and `set`'s
/// attribution stay on the profile the command targets, and a leaf that is
/// invalid in isolation must still be repairable by the cross-layer result.
fn merge_candidate(
    args: &Args,
    dir: &Path,
    name: &str,
    path: &Path,
    content: &str,
) -> Result<bitty_config::MergedConfig, Failure> {
    let candidate =
        parse_profile_plan(content, path).map_err(|err| invalid_profile("set", name, &err))?;
    let mut chain = bitty_config::file::load_profile_chain_in_dir(name, dir)
        .map_err(|err| invalid_profile("set", name, &err))?;
    if let Some(entry) = chain.last_mut() {
        *entry = candidate;
    } else {
        chain.push(candidate);
    }
    let merged = bitty_config::file::resolve_effective_with_profiles_unvalidated(
        None,
        chain.clone(),
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| invalid_profile("set", name, &err))?;
    // The selector is a user layer too: its theme, approved roots, keymaps,
    // and outline overrides participate in what the next launch loads, so the
    // gates must see the cross-layer result. Checking the profile alone both
    // refused valid configurations (selector-approved image roots, a focused
    // outline the selector replaces) and accepted invalid ones (a keymap
    // duplicate or `local:missing` theme only the selector introduced).
    let cross = merge_with_current_user("set", name, args, chain)?;
    strict_post_merge_checks("set", &cross.subject(name), &cross.merged.effective)?;
    Ok(merged)
}

/// The current user file (versioned selector or legacy `init.lua`) loaded as a
/// [`bitty_config::LayerKind::User`] layer.
struct CurrentUserLayer {
    /// Path the layer was read from, for diagnostics.
    path: PathBuf,
    /// Parsed, validated, migrated layer.
    layer: bitty_config::LayeredPlan,
}

/// Loads the user file `config check` merges above the profile chain.
///
/// Resolution matches startup: `--config` > `BITTY_CONFIG` > the XDG probe
/// (`init.lua`, then the `config.lua` alias). A missing default probe means the
/// profile chain is the whole configuration; a missing **explicit**
/// `--config`/`BITTY_CONFIG` path fails closed exactly like `config check`,
/// and a present but invalid file fails closed because the strict loader would
/// reject the next launch too.
fn current_user_layer(verb: &str, args: &Args) -> Result<Option<CurrentUserLayer>, Failure> {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let Some(probed) = bitty_config::file::probe_config_path(explicit.as_deref()) else {
        return Ok(None);
    };
    if !probed.path.exists() {
        if !probed.explicit {
            return Ok(None);
        }
        // An explicit path is a user input, not the default probe: the strict
        // loader fails closed here, so a profile command must not silently
        // validate a configuration `config check` cannot load.
        let explicit_from_cli = args
            .config_path
            .as_deref()
            .is_some_and(|path| !path.trim().is_empty());
        return Err(Failure::usage(if explicit_from_cli {
            format!(
                "bitty config profile {verb}: --config '{}' not found",
                probed.path.display()
            )
        } else {
            format!(
                "bitty config profile {verb}: BITTY_CONFIG '{}' not found",
                probed.path.display()
            )
        }));
    }
    let layer = bitty_config::file::load_user_layer(&probed.path).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile {verb}: invalid config file '{}': {err}",
            probed.path.display()
        ))
    })?;
    Ok(Some(CurrentUserLayer {
        path: probed.path,
        layer,
    }))
}

/// A cross-layer merge and the user file that participated in it.
struct CrossLayerMerge {
    /// Validated merge of the user layer above the profile chain.
    merged: bitty_config::MergedConfig,
    /// Selector/`init.lua` path when a user layer participated.
    user_path: Option<PathBuf>,
}

impl CrossLayerMerge {
    /// Attribution clause for a strict post-merge gate failure.
    fn subject(&self, name: &str) -> String {
        match &self.user_path {
            Some(path) => format!(
                "profile '{name}' merged with '{}' is invalid",
                path.display()
            ),
            None => format!("invalid profile '{name}'"),
        }
    }
}

/// Merges `chain` under the current user layer exactly as
/// `config check --profile NAME` does, so a profile command never validates a
/// configuration the next launch would not load.
fn merge_with_current_user(
    verb: &str,
    name: &str,
    args: &Args,
    chain: Vec<bitty_config::LayeredPlan>,
) -> Result<CrossLayerMerge, Failure> {
    let user = current_user_layer(verb, args)?;
    let (file, path) = match user {
        Some(user) => (Some(user.layer), Some(user.path)),
        None => (None, None),
    };
    let merged = bitty_config::file::resolve_effective_with_profiles(
        file,
        chain,
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| match &path {
        Some(path) => Failure::invalid(format!(
            "bitty config profile {verb}: profile '{name}' merged with '{}' is invalid: {err}",
            path.display()
        )),
        None => invalid_profile(verb, name, &err),
    })?;
    Ok(CrossLayerMerge {
        merged,
        user_path: path,
    })
}

/// Loads and merges one profile's current `extends` chain **for seeding only**.
///
/// Merge only, never a gate: the returned effective config may be invalid in
/// isolation because the current user layer overrides the offending leaf.
/// Callers that decide whether a write is valid must use
/// [`merge_with_current_user`] instead.
fn merged_profile_unvalidated(
    dir: &Path,
    verb: &str,
    name: &str,
) -> Result<bitty_config::MergedConfig, Failure> {
    let chain = bitty_config::file::load_profile_chain_in_dir(name, dir)
        .map_err(|err| invalid_profile(verb, name, &err))?;
    bitty_config::file::resolve_effective_with_profiles_unvalidated(
        None,
        chain,
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| invalid_profile(verb, name, &err))
}

/// Tables whose Lua declaration the typed plan merges as one complete struct.
///
/// A layer that declares one of these tables replaces the whole inherited
/// table: `bitty-config` builds a complete typed struct for every present
/// table (`crate::file::build_plan_from_data`), so leaves omitted inside the
/// table fall back to the core defaults instead of keeping the inherited
/// value. `set` therefore pins the complete resolved table into a profile
/// before creating it, and a one-leaf edit never drops a sibling.
///
/// `appearance` is included because its table-level `theme`/`colors` scalars
/// reset as soon as the table appears; its `animations`/`style` sub-tables are
/// pinned as well so the created table is complete.
const PINNED_TABLES: &[&str] = &[
    "font",
    "window",
    "terminal",
    "layout",
    "decoration",
    "scrollbar",
    "appearance",
];

/// Whether one profile body declares `key` itself.
fn declares_table(plan: &bitty_config::ConfigPlan, key: &str) -> bool {
    match key {
        "font" => plan.font.is_some(),
        "window" => plan.window.is_some(),
        "terminal" => plan.terminal.is_some(),
        "layout" => plan.layout.is_some(),
        "decoration" => plan.decoration.is_some(),
        "scrollbar" => plan.scrollbar.is_some(),
        "appearance" => plan.appearance.is_some(),
        // Not a pinned table: treat it as declared so no caller seeds it.
        _ => true,
    }
}

/// Complete Lua literal pinning every inherited leaf of `key`.
fn inherited_table_literal(key: &str, effective: &bitty_config::EffectiveConfig) -> String {
    match key {
        "font" => font_table_literal(&effective.font),
        "window" => window_table_literal(&effective.window),
        "terminal" => terminal_table_literal(&effective.terminal),
        "layout" => layout_table_literal(&effective.layout),
        "decoration" => decoration_table_literal(&effective.decoration),
        "scrollbar" => scrollbar_table_literal(&effective.scrollbar),
        "appearance" => appearance_table_literal(&effective.appearance, &effective.animations),
        // Guarded by `PINNED_TABLES`; an empty table keeps the caller honest
        // instead of inventing an unknown key.
        _ => String::from("{}"),
    }
}

/// One double-quoted Lua short string, fully encoded.
///
/// Unlike [`crate::init::init_lua_escape`] (wizard answers are control-free by
/// construction), a pinned literal echoes values that already came from a user
/// profile. A raw newline, carriage return, tab, or other control character
/// makes the spliced line invalid Lua, so it is escaped here: `set` must never
/// refuse a profile whose inherited value the loader accepts
/// (`font.family = [[A\nB]]` is a valid family). ASCII controls use the fixed
/// `\xHH` form; higher `Cc` code points use `\u{...}` so the string stays
/// valid UTF-8.
fn lua_string_literal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() && (ch as u32) < 0x80 => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\x{:02x}", ch as u32));
            }
            ch if ch.is_control() => {
                let _ =
                    std::fmt::Write::write_fmt(&mut out, format_args!("\\u{{{:x}}}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One Lua string-list literal (`{ "a", "b" }`, `{}` when empty).
fn lua_list_literal(items: &[String]) -> String {
    if items.is_empty() {
        return String::from("{}");
    }
    format!(
        "{{ {} }}",
        items
            .iter()
            .map(|item| lua_string_literal(item))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// One single-line Lua table literal from ordered `key = value` entries.
fn lua_table_literal(entries: &[(&str, String)]) -> String {
    if entries.is_empty() {
        return String::from("{}");
    }
    format!(
        "{{ {} }}",
        entries
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `font`'s Lua spelling of a ligature policy.
fn ligature_policy_literal(policy: bitty_config::types::LigaturePolicy) -> &'static str {
    match policy {
        bitty_config::types::LigaturePolicy::Never => "never",
        bitty_config::types::LigaturePolicy::Cursor => "cursor",
        bitty_config::types::LigaturePolicy::Always => "always",
    }
}

/// Canonical `#RRGGBB` spelling for one palette role.
fn hex_rgb(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/// Complete `font` table literal (family/size are required by the schema).
fn font_table_literal(font: &bitty_config::FontConfig) -> String {
    lua_table_literal(&[
        ("family", lua_string_literal(&font.family)),
        ("size", format!("{}", font.size)),
        ("line_height", format!("{}", font.line_height)),
        ("letter_spacing", format!("{}", font.letter_spacing)),
        ("fallback", lua_list_literal(&font.fallback)),
        ("features", lua_list_literal(&font.features)),
        (
            "disable_ligatures",
            lua_string_literal(ligature_policy_literal(font.disable_ligatures)),
        ),
    ])
}

/// Complete `window` table literal, including keys `config check` does not
/// print (blur radius and the background image leaves).
fn window_table_literal(window: &bitty_config::WindowConfig) -> String {
    let mut entries = vec![
        ("opacity", format!("{}", window.opacity)),
        ("padding", window.padding.to_string()),
        ("radius_px", window.radius_px.to_string()),
        ("blur_radius", window.blur_radius.to_string()),
    ];
    if let Some(image) = &window.background_image {
        entries.push(("background_image", lua_string_literal(image)));
    }
    if let Some(fit) = window.background_fit {
        entries.push(("background_fit", lua_string_literal(fit.as_str())));
    }
    if let Some(opacity) = window.background_opacity {
        entries.push(("background_opacity", format!("{opacity}")));
    }
    if let Some(position) = window.background_position {
        entries.push(("background_position", lua_string_literal(position.as_str())));
    }
    lua_table_literal(&entries)
}

/// Complete `terminal` table literal (scrollback is required by the schema and
/// is always included).
fn terminal_table_literal(terminal: &bitty_config::TerminalConfig) -> String {
    let mut entries = vec![("scrollback", terminal.scrollback.to_string())];
    if let Some(shell) = &terminal.shell {
        entries.push(("shell", lua_string_literal(shell)));
    }
    entries.push((
        "scroll_lines_per_notch",
        terminal.scroll_lines_per_notch.to_string(),
    ));
    entries.push((
        "scroll_pixels_per_notch",
        terminal.scroll_pixels_per_notch.to_string(),
    ));
    entries.push((
        "page_scroll_percent",
        terminal.page_scroll_percent.to_string(),
    ));
    entries.push((
        "cursor_style",
        lua_string_literal(terminal.cursor_style.as_str()),
    ));
    entries.push(("bell", lua_string_literal(terminal.bell.as_str())));
    lua_table_literal(&entries)
}

/// Complete `layout` table literal.
fn layout_table_literal(layout: &bitty_config::LayoutConfig) -> String {
    lua_table_literal(&[
        ("gaps_in", layout.gaps_in.to_string()),
        ("gaps_out", layout.gaps_out.to_string()),
        ("resize_step", format!("{}", layout.resize_step)),
    ])
}

/// Complete `decoration` table literal, including the optional color, width,
/// background-image, and tint leaves.
fn decoration_table_literal(decoration: &bitty_config::DecorationConfig) -> String {
    let mut entries = vec![
        ("gaps_in", decoration.gaps_in.to_string()),
        ("gaps_out", decoration.gaps_out.to_string()),
        ("border", decoration.border.to_string()),
        ("radius", decoration.radius.to_string()),
        ("content_inset", decoration.content_inset.to_string()),
    ];
    for (key, color) in [
        ("border_color", decoration.border_color),
        ("border_color_focused", decoration.border_color_focused),
        ("border_color_idle", decoration.border_color_idle),
    ] {
        if let Some(color) = color {
            entries.push((key, lua_string_literal(&color.to_hex())));
        }
    }
    for (key, width) in [
        ("border_width", decoration.border_width),
        ("border_width_focused", decoration.border_width_focused),
        ("border_width_idle", decoration.border_width_idle),
    ] {
        if let Some(width) = width {
            entries.push((key, width.to_string()));
        }
    }
    if let Some(image) = &decoration.background_image {
        entries.push(("background_image", lua_string_literal(image)));
    }
    if let Some(fit) = decoration.background_fit {
        entries.push(("background_fit", lua_string_literal(fit.as_str())));
    }
    if let Some(roots) = &decoration.background_image_roots {
        entries.push(("background_image_roots", lua_list_literal(roots)));
    }
    if let Some(tint) = decoration.background_tint {
        entries.push(("background_tint", lua_string_literal(&tint.to_hex())));
    }
    if let Some(strength) = decoration.background_tint_strength {
        entries.push(("background_tint_strength", format!("{strength}")));
    }
    lua_table_literal(&entries)
}

/// Complete `scrollbar` table literal.
fn scrollbar_table_literal(scrollbar: &bitty_config::ScrollbarConfig) -> String {
    lua_table_literal(&[
        ("mode", lua_string_literal(scrollbar.mode.as_str())),
        ("width", scrollbar.width.to_string()),
    ])
}

/// Complete `appearance` table literal.
///
/// `theme` and `colors` scalar-replace whenever the `appearance` table is
/// present, so both are pinned; `animations`/`style` deep-merge per leaf but
/// are pinned as well so the created table is complete and later edits cannot
/// depend on which sub-table happened to be present. An inline palette has no
/// outline-token or category fields (those exist only on local theme files,
/// which this merge path never applies), so the five data leaves are complete
/// here.
fn appearance_table_literal(
    appearance: &bitty_config::AppearanceConfig,
    animations: &bitty_config::AnimationsConfig,
) -> String {
    let mut entries = Vec::new();
    if let Some(theme) = &appearance.theme {
        entries.push(("theme", lua_string_literal(theme)));
    }
    if let Some(colors) = &appearance.colors {
        entries.push(("colors", palette_table_literal(colors)));
    }
    entries.push(("animations", animations_table_literal(animations)));
    if let Some(style) = &appearance.style {
        entries.push(("style", style_table_literal(style)));
    }
    lua_table_literal(&entries)
}

/// Complete `appearance.animations` table literal (resolved values).
fn animations_table_literal(animations: &bitty_config::AnimationsConfig) -> String {
    let durations = [
        ("open", animations.duration_ms.open),
        ("close", animations.duration_ms.close),
        ("focus", animations.duration_ms.focus),
        ("workspace", animations.duration_ms.workspace),
        ("move", animations.duration_ms.r#move),
        ("resize", animations.duration_ms.resize),
        ("drag", animations.duration_ms.drag),
    ];
    let easings = [
        ("open", animations.easing.open.as_str()),
        ("close", animations.easing.close.as_str()),
        ("focus", animations.easing.focus.as_str()),
        ("workspace", animations.easing.workspace.as_str()),
        ("move", animations.easing.r#move.as_str()),
        ("resize", animations.easing.resize.as_str()),
        ("drag", animations.easing.drag.as_str()),
    ];
    lua_table_literal(&[
        ("enabled", animations.enabled.to_string()),
        (
            "reduced_motion",
            lua_string_literal(animations.reduced_motion.as_str()),
        ),
        (
            "duration_ms",
            lua_table_literal(
                &durations
                    .iter()
                    .map(|(key, value)| (*key, value.to_string()))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "easing",
            lua_table_literal(
                &easings
                    .iter()
                    .map(|(key, value)| (*key, lua_string_literal(value)))
                    .collect::<Vec<_>>(),
            ),
        ),
    ])
}

/// Complete `appearance.style` table literal (only declared leaves; an
/// unset leaf says nothing and keeps inheriting).
fn style_table_literal(style: &bitty_config::StyleConfig) -> String {
    let mut entries = Vec::new();
    for (key, color) in [
        ("glow_color", style.glow_color),
        ("shadow_color", style.shadow_color),
    ] {
        if let Some(color) = color {
            entries.push((key, lua_string_literal(&color.to_hex())));
        }
    }
    if let Some(width) = style.glow_width {
        entries.push(("glow_width", width.to_string()));
    }
    for (key, offset) in [
        ("shadow_offset_x", style.shadow_offset_x),
        ("shadow_offset_y", style.shadow_offset_y),
    ] {
        if let Some(offset) = offset {
            entries.push((key, offset.to_string()));
        }
    }
    if let Some(fade) = style.fade_unfocused {
        entries.push(("fade_unfocused", format!("{fade}")));
    }
    lua_table_literal(&entries)
}

/// One inline `appearance.colors` palette literal (`#RRGGBB`, 16 ANSI roles).
fn palette_table_literal(palette: &bitty_config::CustomPalette) -> String {
    let ansi = palette
        .ansi
        .iter()
        .map(|rgb| lua_string_literal(&hex_rgb(*rgb)))
        .collect::<Vec<_>>()
        .join(", ");
    lua_table_literal(&[
        (
            "background",
            lua_string_literal(&hex_rgb(palette.background)),
        ),
        (
            "foreground",
            lua_string_literal(&hex_rgb(palette.foreground)),
        ),
        ("cursor", lua_string_literal(&hex_rgb(palette.cursor))),
        ("selection", lua_string_literal(&hex_rgb(palette.selection))),
        ("ansi", format!("{{ {ansi} }}")),
    ])
}

/// Runs the strict post-merge gates `bitty config check` runs over a merged
/// candidate, so no profile command accepts a body the strict loader rejects.
///
/// `resolve_effective_with_profiles` already runs `EffectiveConfig::validate`
/// (shape, bounds, outline AC-1/AC-2). `config check` additionally resolves
/// the `local:<name>` theme selection, resolves the merged keymap table,
/// maps the result onto the runtime bounds, and runs the background-image
/// pipeline (approved-root policy, regular file, header sniff, decode and
/// admission bounds). A profile that passes the merge but fails one of those
/// gates must be refused before any bytes are written; otherwise `set`,
/// `clone`, or `use` reports success for a config `config check` then
/// rejects, and the diagnostic blames the user's file.
///
/// `subject` is the attribution clause after the verb (`invalid profile
/// 'work'`, or the selector the switch would leave invalid).
fn strict_post_merge_checks(
    verb: &str,
    subject: &str,
    effective: &bitty_config::EffectiveConfig,
) -> Result<(), Failure> {
    let mut resolved = effective.clone();
    bitty_config::local_theme::apply_selection(&mut resolved).map_err(|err| {
        Failure::invalid(format!("bitty config profile {verb}: {subject}: {err}"))
    })?;
    // `config check` resolves the effective keymap table and exits 2 on a
    // duplicate/invalid binding; a profile edit must not bypass that.
    if let Err(err) = bitty_config::keymap::resolve_keymaps(&resolved) {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: {subject}: keymaps: {err}"
        )));
    }
    let runtime = crate::config_cli::runtime_config_from_effective_for(
        &resolved,
        crate::config_cli::startup_prefer_light(),
    )
    .map_err(|err| Failure::invalid(format!("bitty config profile {verb}: {subject}: {err}")))?;
    bitty_runtime::validate_background_images(&runtime).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile {verb}: {subject}: {err}; fix the image path or add its \
             directory to decoration.background_image_roots"
        ))
    })?;
    Ok(())
}

/// The typed value one `set` value string must read back as.
///
/// `render_literal` already accepted the raw value, so these conversions
/// cannot fail in normal operation; a conversion that still fails falls back
/// to an unmatched value, which the caller turns into a refused write instead
/// of a panic.
///
/// Text fields whose typed form canonicalizes an accepted spelling (the mod
/// key and the chrome mouse-button aliases) must compare in canonical form:
/// the user may write `Super`, `"Super"`, or `Paste_From_Primary`, while the
/// re-read effective value is `super`/`paste_from_primary`.
fn requested_value(field: &SchemaField, raw: &str) -> ConfigValue {
    // Quoted CLI values are supported (`render_literal` strips one matching
    // pair), so canonicalize the same stripped spelling here. Otherwise a
    // quoted alias such as `"Super"` would compare as the literal string
    // `Super` and the round-trip check would refuse the write.
    let canonical = strip_matching_quotes(raw);
    match field.path {
        "mod_key" => {
            if let Ok(key) = bitty_config::keymap::ModKey::parse(canonical) {
                return ConfigValue::Text(key.canonical().to_string());
            }
        }
        "mouse.right" | "mouse.middle" => {
            if let Ok(binding) =
                bitty_config::keymap::MouseButtonBinding::parse(canonical, field.path)
            {
                return ConfigValue::Text(binding.canonical());
            }
        }
        _ => {}
    }
    match field.kind {
        FieldKind::Text => ConfigValue::Text(strip_matching_quotes(raw).to_string()),
        FieldKind::Integer => ConfigValue::Number(
            raw.trim()
                .parse::<u32>()
                .map_or_else(|_| raw.trim().to_string(), |value| value.to_string()),
        ),
        FieldKind::Number => ConfigValue::Number(
            raw.trim()
                .parse::<f32>()
                .map_or_else(|_| raw.trim().to_string(), |value| format!("{value}")),
        ),
        FieldKind::Bool => ConfigValue::Bool(raw.trim().eq_ignore_ascii_case("true")),
        FieldKind::List => ConfigValue::List(if raw.trim().is_empty() {
            Vec::new()
        } else {
            raw.trim()
                .split(',')
                .map(|item| item.trim().to_string())
                .collect()
        }),
    }
}

/// Creates a brand-new profile file exclusively, writes it, and syncs it.
fn create_exclusive(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let result = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(err) = result {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(err);
    }
    Ok(())
}

/// Failure from one atomic profile replacement.
#[derive(Debug)]
enum ProfileWriteError {
    /// The target no longer matches the identity the edit was computed from;
    /// nothing was replaced and the concurrent bytes survive.
    Changed,
    /// Filesystem failure; the temporary file has been removed.
    Io(std::io::Error),
}

/// Replaces a profile atomically: unique sibling temporary file, `sync_all`,
/// a pre-commit identity re-check, rename over the target, then a
/// parent-directory sync. Any failure removes the temporary file and leaves
/// the previous bytes intact.
///
/// `expected` is the identity captured when the caller read the bytes the
/// replacement was computed from. `None` skips the check (callers that did
/// not read an existing preimage). The re-check runs after the replacement is
/// staged and immediately before `rename`, so it observes a concurrent
/// replace or in-place edit that landed during validation. It narrows the
/// lost-update window but cannot close it: a writer landing between the check
/// and the rename is still overwritten (see [`ProfileIdentity`]).
fn write_profile_atomic(
    path: &Path,
    content: &str,
    expected: Option<&ProfileIdentity>,
) -> Result<(), ProfileWriteError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent).map_err(ProfileWriteError::Io)?;
    }
    let name = path.file_name().map_or_else(
        || String::from("profile.lua"),
        |name| name.to_string_lossy().into_owned(),
    );
    let permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut temp = None;
    for _ in 0..PROFILE_MAX_TEMP_ATTEMPTS {
        let counter = PROFILE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.tmp-{}-{counter}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(ProfileWriteError::Io(err)),
        }
    }
    let Some((temp_path, mut file)) = temp else {
        return Err(ProfileWriteError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temporary profile names",
        )));
    };
    let write_result = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ProfileWriteError::Io(err));
    }
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(&temp_path, permissions);
    }
    #[cfg(test)]
    run_commit_hook(path);
    if let Some(expected) = expected {
        if let Err(err) = verify_profile_unchanged(path, expected) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ProfileWriteError::Io(err));
    }
    // The file bytes are synced above; the directory entry that now names
    // them must be synced too, or a crash can lose the replace while the
    // profile still reads as the previous bytes.
    sync_parent_dir(path);
    Ok(())
}

/// Re-checks the preimage identity immediately before the replace commits.
///
/// The current file is opened and measured through that handle, so a
/// replacement (new inode) and an in-place edit (same inode, new mtime or
/// length) are both detected. A file that disappeared is reported as a
/// concurrent change: the bytes the edit was computed from no longer exist.
fn verify_profile_unchanged(
    path: &Path,
    expected: &ProfileIdentity,
) -> Result<(), ProfileWriteError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileWriteError::Changed);
        }
        Err(err) => return Err(ProfileWriteError::Io(err)),
    };
    let metadata = file.metadata().map_err(ProfileWriteError::Io)?;
    if ProfileIdentity::from_metadata(&metadata) == *expected {
        Ok(())
    } else {
        Err(ProfileWriteError::Changed)
    }
}

#[cfg(test)]
type ProfileCommitHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    /// One-shot test seam: runs after the replacement is staged and synced
    /// but before the preimage re-check, emulating a concurrent writer that
    /// lands between `set`'s read and its commit.
    static PROFILE_COMMIT_HOOK: std::cell::RefCell<Option<ProfileCommitHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Arms the one-shot commit hook for the current test thread.
#[cfg(test)]
fn with_commit_hook(hook: impl FnOnce(&Path) + 'static) {
    PROFILE_COMMIT_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Runs and clears the one-shot commit hook (test builds only).
#[cfg(test)]
fn run_commit_hook(path: &Path) {
    let hook = PROFILE_COMMIT_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
}

/// Commits one profile rename, mapping a raced destination onto the conflict
/// class in [`run_rename`].
///
/// Production is [`rename_no_replace`]; unit tests replace the platform call
/// through [`PROFILE_RENAME_OVERRIDE`] so the `AlreadyExists` mapping is
/// exercised on every host, not only where `RENAME_NOREPLACE` exists.
fn commit_profile_rename(old: &Path, new: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(rename) = PROFILE_RENAME_OVERRIDE.with(|slot| slot.borrow_mut().take()) {
        return rename(old, new);
    }
    rename_no_replace(old, new)
}

#[cfg(test)]
type ProfileRenameOverride = Box<dyn FnOnce(&Path, &Path) -> std::io::Result<()>>;

#[cfg(test)]
thread_local! {
    /// One-shot test seam replacing the platform rename. The closure owns the
    /// simulated race (for example creating the destination and returning
    /// `AlreadyExists`) so the CLI mapping is deterministic.
    static PROFILE_RENAME_OVERRIDE: std::cell::RefCell<Option<ProfileRenameOverride>> =
        const { std::cell::RefCell::new(None) };
}

/// Arms the one-shot rename override for the current test thread.
#[cfg(test)]
fn with_rename_override(rename: impl FnOnce(&Path, &Path) -> std::io::Result<()> + 'static) {
    PROFILE_RENAME_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(Box::new(rename)));
}

/// Renames `old` to `new` without replacing an existing destination.
///
/// POSIX `rename` silently overwrites, so the caller's pre-check is only a
/// courtesy: a file created between the check and the call would be lost.
/// Linux `RENAME_NOREPLACE` closes that window; a filesystem or kernel
/// without the flag (or a platform without a safe no-replace wrapper) falls
/// back to the pre-checked rename that this command always used.
///
/// Platform coverage and the recorded gaps:
/// - **Linux (glibc):** `nix::fcntl::renameat2(..., RENAME_NOREPLACE)` below,
///   with `EINVAL`/`ENOSYS`/`EOPNOTSUPP` falling back to the pre-checked
///   rename (old kernel, or a filesystem such as NFS that lacks the flag).
/// - **Linux (musl):** the kernel and `libc` both support `renameat2`, but
///   the pinned `nix` 0.31.3 gates its safe wrapper to `target_env = "gnu"`.
///   Calling `libc::renameat2` directly needs `unsafe`, which this binary
///   forbids crate-wide, so musl keeps the pre-checked fallback. This is a
///   documented coverage gap (bitty#2707 / CTX-1602); a safe wrapper
///   (`rustix` or a narrow reviewed exception) can close it without changing
///   this command's contract.
/// - **macOS:** `renamex_np(RENAME_EXCL)` is declared by `libc`, but it is
///   likewise an `unsafe` FFI call, so it is not reachable under
///   `#![forbid(unsafe_code)]`. Same documented gap.
/// - **Windows:** no std no-replace rename; the pre-checked rename remains.
fn rename_no_replace(old: &Path, new: &Path) -> std::io::Result<()> {
    rename_with_attempt(old, new, try_rename_no_replace)
}

/// Runs the platform no-replace attempt when the build has one, otherwise the
/// pre-checked [`std::fs::rename`] fallback.
///
/// Split from [`rename_no_replace`] so the unsupported-platform routing is
/// testable on every host: `attempt` returning `None` means "this build has
/// no safe no-replace primitive", which is exactly the fallback contract.
fn rename_with_attempt(
    old: &Path,
    new: &Path,
    attempt: impl FnOnce(&Path, &Path) -> Option<std::io::Result<()>>,
) -> std::io::Result<()> {
    if let Some(result) = attempt(old, new) {
        return result;
    }
    std::fs::rename(old, new)
}

/// One platform no-replace attempt.
///
/// `None` means this build has no safe no-replace primitive and the caller
/// must fall back; `Some(Err)` is a real failure (for example a raced
/// destination returning `AlreadyExists`) that must not be retried as a
/// clobbering rename.
fn try_rename_no_replace(old: &Path, new: &Path) -> Option<std::io::Result<()>> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::errno::Errno;
        use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};
        match renameat2(AT_FDCWD, old, AT_FDCWD, new, RenameFlags::RENAME_NOREPLACE) {
            Ok(()) => Some(Ok(())),
            // Unsupported flag: keep the legacy pre-checked rename.
            Err(Errno::EINVAL | Errno::ENOSYS | Errno::EOPNOTSUPP) => None,
            Err(err) => Some(Err(std::io::Error::from(err))),
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = (old, new);
        None
    }
}

/// Flushes the directory entry for `path` after a rename or atomic replace.
///
/// Best-effort: platforms that cannot fsync a directory (Windows) simply keep
/// the historical behavior, and a directory that cannot be opened adds no
/// second failure to an already successful write.
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
}

/// Prefixed filesystem failure (exit 1).
fn io_failure(verb: &str, action: &str, err: &std::io::Error) -> Failure {
    Failure::io(format!(
        "bitty config profile {verb}: cannot {action}: {err}"
    ))
}

/// Invalid profile body/chain failure (exit 2).
fn invalid_profile(verb: &str, name: &str, err: &bitty_config::ConfigError) -> Failure {
    Failure::invalid(format!(
        "bitty config profile {verb}: invalid profile '{name}': {err}"
    ))
}

/// Minimal valid profile body written by `create`.
fn new_profile_lua(name: &str) -> String {
    format!(
        "-- bitty profile '{name}', created by `bitty config profile create`.\n\
         -- Values here override the shipped defaults; activate this profile with\n\
         -- `bitty config profile use {name}`. Unknown keys fail closed; validate\n\
         -- with `bitty config check`.\n\
         return {{\n}}\n"
    )
}

// ---------------------------------------------------------------------------
// Documented schema paths (`get`/`set`)
// ---------------------------------------------------------------------------

/// How `set` renders one CLI value into Lua source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    /// Double-quoted string (schema validates enums and ranges).
    Text,
    /// Non-negative integer literal (`u32` schema fields).
    Integer,
    /// Finite `f32` literal.
    Number,
    /// `true` / `false`.
    Bool,
    /// Comma-separated string list (`{ "a", "b" }`).
    List,
}

/// One documented dotted schema path supported by `get`/`set`.
#[derive(Debug)]
struct SchemaField {
    /// Canonical dotted key (same spelling `config check` prints).
    path: &'static str,
    /// Lua literal kind accepted by `set`.
    kind: FieldKind,
    /// Effective-value getter over the typed schema.
    get: fn(&bitty_config::EffectiveConfig) -> ConfigValue,
}

/// Keys `config profile show` prints (kept from CTX-1404 for compatibility).
const SHOW_KEYS: &[&str] = &[
    "appearance.theme",
    "font.family",
    "font.size",
    "terminal.scrollback",
    "window.opacity",
    "window.padding",
    "decoration.gaps_in",
    "close_confirm",
];

/// Documented scalar schema paths settable from a profile.
///
/// This is a projection of the typed Rust schema (`bitty-config/src/types.rs`)
/// over the dotted keys of the configuration reference; it is not a second
/// validator. Every value written here is re-read by the strict loader and
/// rejected when the schema rejects it.
const SCHEMA_FIELDS: &[SchemaField] = &[
    SchemaField {
        path: "appearance.theme",
        kind: FieldKind::Text,
        get: get_theme,
    },
    SchemaField {
        path: "font.family",
        kind: FieldKind::Text,
        get: get_font_family,
    },
    SchemaField {
        path: "font.size",
        kind: FieldKind::Number,
        get: get_font_size,
    },
    SchemaField {
        path: "font.line_height",
        kind: FieldKind::Number,
        get: get_font_line_height,
    },
    SchemaField {
        path: "font.letter_spacing",
        kind: FieldKind::Number,
        get: get_font_letter_spacing,
    },
    SchemaField {
        path: "font.fallback",
        kind: FieldKind::List,
        get: get_font_fallback,
    },
    SchemaField {
        path: "window.opacity",
        kind: FieldKind::Number,
        get: get_window_opacity,
    },
    SchemaField {
        path: "window.padding",
        kind: FieldKind::Integer,
        get: get_window_padding,
    },
    SchemaField {
        path: "window.radius_px",
        kind: FieldKind::Integer,
        get: get_window_radius_px,
    },
    SchemaField {
        path: "terminal.scrollback",
        kind: FieldKind::Integer,
        get: get_terminal_scrollback,
    },
    SchemaField {
        path: "terminal.shell",
        kind: FieldKind::Text,
        get: get_terminal_shell,
    },
    SchemaField {
        path: "terminal.scroll_lines_per_notch",
        kind: FieldKind::Integer,
        get: get_scroll_lines_per_notch,
    },
    SchemaField {
        path: "terminal.scroll_pixels_per_notch",
        kind: FieldKind::Integer,
        get: get_scroll_pixels_per_notch,
    },
    SchemaField {
        path: "terminal.page_scroll_percent",
        kind: FieldKind::Integer,
        get: get_page_scroll_percent,
    },
    SchemaField {
        path: "terminal.cursor_style",
        kind: FieldKind::Text,
        get: get_cursor_style,
    },
    SchemaField {
        path: "terminal.bell",
        kind: FieldKind::Text,
        get: get_bell,
    },
    SchemaField {
        path: "selection.auto_copy",
        kind: FieldKind::Bool,
        get: get_auto_copy,
    },
    SchemaField {
        path: "history.capture.transcript",
        kind: FieldKind::Bool,
        get: get_capture_transcript,
    },
    SchemaField {
        path: "history.capture.commands",
        kind: FieldKind::Bool,
        get: get_capture_commands,
    },
    SchemaField {
        path: "layout.gaps_in",
        kind: FieldKind::Integer,
        get: get_layout_gaps_in,
    },
    SchemaField {
        path: "layout.gaps_out",
        kind: FieldKind::Integer,
        get: get_layout_gaps_out,
    },
    SchemaField {
        path: "layout.resize_step",
        kind: FieldKind::Number,
        get: get_layout_resize_step,
    },
    SchemaField {
        path: "decoration.gaps_in",
        kind: FieldKind::Integer,
        get: get_decoration_gaps_in,
    },
    SchemaField {
        path: "decoration.gaps_out",
        kind: FieldKind::Integer,
        get: get_decoration_gaps_out,
    },
    SchemaField {
        path: "decoration.border",
        kind: FieldKind::Integer,
        get: get_decoration_border,
    },
    SchemaField {
        path: "decoration.radius",
        kind: FieldKind::Integer,
        get: get_decoration_radius,
    },
    SchemaField {
        path: "decoration.content_inset",
        kind: FieldKind::Integer,
        get: get_decoration_content_inset,
    },
    SchemaField {
        path: "scrollbar.mode",
        kind: FieldKind::Text,
        get: get_scrollbar_mode,
    },
    SchemaField {
        path: "scrollbar.width",
        kind: FieldKind::Integer,
        get: get_scrollbar_width,
    },
    SchemaField {
        path: "mouse.focus_follows_mouse",
        kind: FieldKind::Bool,
        get: get_focus_follows_mouse,
    },
    SchemaField {
        path: "mouse.focus_follows_mouse_delay_ms",
        kind: FieldKind::Integer,
        get: get_focus_delay_ms,
    },
    SchemaField {
        path: "mouse.right",
        kind: FieldKind::Text,
        get: get_mouse_right,
    },
    SchemaField {
        path: "mouse.middle",
        kind: FieldKind::Text,
        get: get_mouse_middle,
    },
    SchemaField {
        path: "mod_key",
        kind: FieldKind::Text,
        get: get_mod_key,
    },
    SchemaField {
        path: "close_confirm",
        kind: FieldKind::Text,
        get: get_close_confirm,
    },
    SchemaField {
        path: "appearance.animations.enabled",
        kind: FieldKind::Bool,
        get: get_animations_enabled,
    },
    SchemaField {
        path: "session.restore_on_startup",
        kind: FieldKind::Bool,
        get: get_session_restore,
    },
];

fn get_theme(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    e.appearance
        .theme
        .as_deref()
        .map_or(ConfigValue::Null, |theme| {
            ConfigValue::Text(theme.to_string())
        })
}
fn get_font_family(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.font.family.clone())
}
fn get_font_size(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.font.size))
}
fn get_font_line_height(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.font.line_height))
}
fn get_font_letter_spacing(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.font.letter_spacing))
}
fn get_font_fallback(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::List(e.font.fallback.clone())
}
fn get_window_opacity(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.window.opacity))
}
fn get_window_padding(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.window.padding))
}
fn get_window_radius_px(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.window.radius_px))
}
fn get_terminal_scrollback(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.terminal.scrollback))
}
fn get_terminal_shell(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    e.terminal
        .shell
        .as_deref()
        .map_or(ConfigValue::Null, |shell| {
            ConfigValue::Text(shell.to_string())
        })
}
fn get_scroll_lines_per_notch(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.terminal.scroll_lines_per_notch))
}
fn get_scroll_pixels_per_notch(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.terminal.scroll_pixels_per_notch))
}
fn get_page_scroll_percent(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.terminal.page_scroll_percent))
}
fn get_cursor_style(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.terminal.cursor_style.as_str().to_string())
}
fn get_bell(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.terminal.bell.as_str().to_string())
}
fn get_auto_copy(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Bool(e.selection.auto_copy)
}
fn get_capture_transcript(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Bool(e.history.capture.transcript)
}
fn get_capture_commands(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Bool(e.history.capture.commands)
}
fn get_layout_gaps_in(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.layout.gaps_in))
}
fn get_layout_gaps_out(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.layout.gaps_out))
}
fn get_layout_resize_step(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.layout.resize_step))
}
fn get_decoration_gaps_in(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.decoration.gaps_in))
}
fn get_decoration_gaps_out(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.decoration.gaps_out))
}
fn get_decoration_border(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.decoration.border))
}
fn get_decoration_radius(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.decoration.radius))
}
fn get_decoration_content_inset(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.decoration.content_inset))
}
fn get_scrollbar_mode(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.scrollbar.mode.as_str().to_string())
}
fn get_scrollbar_width(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.scrollbar.width))
}
fn get_focus_follows_mouse(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Bool(e.mouse.focus_follows_mouse)
}
fn get_focus_delay_ms(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Number(format!("{}", e.mouse.focus_follows_mouse_delay_ms))
}
fn get_mouse_right(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.mouse.right.canonical().to_string())
}
fn get_mouse_middle(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.mouse.middle.canonical().to_string())
}
fn get_mod_key(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.mod_key.canonical().to_string())
}
fn get_close_confirm(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Text(e.close_confirm.as_str().to_string())
}
fn get_animations_enabled(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    ConfigValue::Bool(e.animations.enabled)
}
fn get_session_restore(e: &bitty_config::EffectiveConfig) -> ConfigValue {
    e.session
        .restore_on_startup
        .map_or(ConfigValue::Null, ConfigValue::Bool)
}

/// Catalog entry for one canonical path (internal, key list is trusted).
fn schema_field_entry(path: &str) -> Option<&'static SchemaField> {
    SCHEMA_FIELDS.iter().find(|field| field.path == path)
}

/// Resolves a user-supplied schema path (fail-closed, exit 2).
fn schema_field(verb: &str, raw: &str) -> Result<&'static SchemaField, Failure> {
    validate_schema_path(verb, raw)?;
    let trimmed = raw.trim();
    let canonical = if trimmed.eq_ignore_ascii_case("theme") {
        "appearance.theme"
    } else {
        trimmed
    };
    schema_field_entry(canonical).ok_or_else(|| {
        Failure::invalid(format!(
            "bitty config profile {verb}: unknown config path '{raw}' (only documented scalar schema \
             paths are supported; see the configuration reference)"
        ))
    })
}

/// Rejects dotted-path traversal and malformed keys before the catalog lookup.
fn validate_schema_path(verb: &str, raw: &str) -> Result<(), Failure> {
    let trimmed = raw.trim();
    if trimmed.len() > MAX_SCHEMA_PATH_BYTES {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: config path exceeds {MAX_SCHEMA_PATH_BYTES} bytes"
        )));
    }
    let segments: Vec<&str> = trimmed.split('.').collect();
    if segments.len() > MAX_SCHEMA_PATH_SEGMENTS {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: config path exceeds {MAX_SCHEMA_PATH_SEGMENTS} segments"
        )));
    }
    if segments.iter().any(|segment| {
        segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: invalid config path '{raw}': use dotted lowercase schema \
             keys (no paths, no separators)"
        )));
    }
    Ok(())
}

/// Renders one CLI value as a Lua literal for the field's schema kind.
fn render_literal(field: &SchemaField, raw: &str) -> Result<String, String> {
    if raw.len() > MAX_PROFILE_VALUE_BYTES {
        return Err(format!("value exceeds {MAX_PROFILE_VALUE_BYTES} bytes"));
    }
    if raw.chars().any(char::is_control) {
        return Err(String::from("value must not contain control characters"));
    }
    match field.kind {
        FieldKind::Text => {
            let text = strip_matching_quotes(raw);
            if text.trim().is_empty() {
                return Err(String::from("value must be non-empty"));
            }
            Ok(lua_string_literal(text))
        }
        FieldKind::Integer => raw
            .trim()
            .parse::<u32>()
            .map(|value| value.to_string())
            .map_err(|_| format!("'{raw}' is not a non-negative integer")),
        FieldKind::Number => {
            let value: f32 = raw
                .trim()
                .parse()
                .map_err(|_| format!("'{raw}' is not a finite number"))?;
            if !value.is_finite() {
                return Err(format!("'{raw}' is not a finite number"));
            }
            Ok(format!("{value}"))
        }
        FieldKind::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(String::from("true")),
            "false" => Ok(String::from("false")),
            _ => Err(format!("'{raw}' is not a boolean (want true or false)")),
        },
        FieldKind::List => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(String::from("{}"));
            }
            let mut items = Vec::new();
            for item in trimmed.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    return Err(String::from(
                        "list contains an empty entry (want comma-separated values)",
                    ));
                }
                items.push(lua_string_literal(item));
            }
            Ok(format!("{{ {} }}", items.join(", ")))
        }
    }
}

/// Removes one pair of matching shell/Lua quotes (`"x"` / `'x'`).
fn strip_matching_quotes(raw: &str) -> &str {
    let trimmed = raw.trim();
    for quote in ['"', '\''] {
        if trimmed.len() >= 2 && trimmed.starts_with(quote) && trimmed.ends_with(quote) {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

/// Returns the loader bound a rendered profile body would breach, if any.
///
/// A pin is one Lua table literal on one line, and the final edit can append
/// to that same line. The loader rejects a line over
/// [`bitty_config::file::MAX_LINE_BYTES`] and a file over
/// [`bitty_config::file::MAX_CONFIG_FILE_BYTES`]; checking the final spliced
/// body before merging lets `set` name the pin and the bound instead of
/// reporting a generic `invalid profile` for a file the user did not write.
fn profile_bound_violation(content: &str) -> Option<String> {
    if content
        .lines()
        .any(|line| line.len() > bitty_config::file::MAX_LINE_BYTES)
    {
        return Some(format!(
            "the rendered line would exceed the {}-byte line bound",
            bitty_config::file::MAX_LINE_BYTES
        ));
    }
    if content.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES {
        return Some(format!(
            "the rendered profile would exceed the {} KiB profile bound",
            bitty_config::file::MAX_CONFIG_FILE_BYTES / 1024
        ));
    }
    None
}

/// Human rendering of one typed value (`config check` row style).
fn display_value(value: &ConfigValue, theme_default: bool) -> String {
    match value {
        ConfigValue::Text(text) => format!("\"{text}\""),
        ConfigValue::Number(number) => number.clone(),
        ConfigValue::Bool(boolean) => boolean.to_string(),
        ConfigValue::Null => {
            if theme_default {
                String::from("(default)")
            } else {
                String::from("(unset)")
            }
        }
        ConfigValue::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| format!("\"{item}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Attribution label for one profile-only effective field.
fn profile_source_label(merged: &bitty_config::MergedConfig, field: &str) -> String {
    match merged.source_of(field) {
        Some(found) if found.layer == bitty_config::LayerKind::Profile => match &found.path {
            Some(path) => format!("profile: {path}"),
            None => String::from("profile"),
        },
        Some(found) if found.layer == bitty_config::LayerKind::CoreDefaults => {
            String::from("default")
        }
        Some(found) => found.layer.label().to_string(),
        None => String::from("default"),
    }
}

/// JSON envelope for a successful profile command.
fn success_envelope(command: &str, result: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":{},\"ok\":true,\"result\":{result}}}",
        json_string(command)
    )
}

/// JSON envelope for a failed profile command.
///
/// Mirrors the `config` error envelopes (`config_cli.rs`): the typed `error`
/// object is accompanied by a `result` object so one envelope shape describes
/// both outcomes.
fn error_envelope(command: &str, action: &str, failure: &Failure) -> String {
    let message = bounded_message(&failure.message);
    format!(
        "{{\"v\":1,\"command\":{},\"ok\":false,\"error\":{{\"class\":\"config\",\"code\":{},\"message\":{}}},\"result\":{{\"kind\":\"profile\",\"action\":{},\"valid\":false,\"name\":null,\"diagnostics\":[{{\"severity\":\"error\",\"message\":{}}}]}}}}",
        json_string(command),
        json_string(failure.code),
        json_string(&message),
        json_string(action),
        json_string(&message)
    )
}

/// JSON `result` object for mutating commands.
fn profile_result(action: &str, name: &str, path: &Path, extra: &str) -> String {
    format!(
        "{{\"kind\":\"profile\",\"action\":{},\"name\":{},\"path\":{}{extra}}}",
        json_string(action),
        json_string(name),
        json_path(path)
    )
}

/// One JSON string literal.
fn json_string(raw: &str) -> String {
    format!("\"{}\"", crate::list::json_escape(raw))
}

/// One JSON path literal.
fn json_path(path: &Path) -> String {
    json_string(&path.display().to_string())
}

// ---------------------------------------------------------------------------
// Minimal Lua table editor (one dotted path, comments preserved)
// ---------------------------------------------------------------------------

/// One direct field of a Lua table constructor.
struct LuaField {
    key: String,
    /// First byte of the value expression.
    value_start: usize,
    /// One past the last byte of the value expression (excludes a trailing
    /// comment and whitespace).
    value_end: usize,
}

/// Replaces (or inserts) one dotted path inside a profile's returned table.
///
/// The editor understands exactly the plain-data Lua subset the config
/// sandbox accepts: a `return { ... }` table with identifier keys, scalar or
/// table values, short/long strings, and comments. Anything else fails closed
/// with a message asking the user to edit the profile manually; the caller
/// still re-validates the rendered bytes before writing.
fn set_lua_path(content: &str, segments: &[&str], literal: &str) -> Result<String, String> {
    let bytes = content.as_bytes();
    let (open, close) = find_return_table(bytes)?;
    // Nothing but whitespace, comments, or empty statements may follow the
    // returned table; editing a chunk with trailing code is out of scope.
    let mut tail = close + 1;
    loop {
        tail = skip_ignorable(bytes, tail, bytes.len())?;
        if tail < bytes.len() && bytes[tail] == b';' {
            tail += 1;
            continue;
        }
        break;
    }
    if tail != bytes.len() {
        return Err(String::from(
            "the profile continues after its returned table; edit it manually",
        ));
    }
    set_in_table(content, open, close, segments, literal)
}

/// Finds the top-level `return {` table pair.
fn find_return_table(bytes: &[u8]) -> Result<(usize, usize), String> {
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => i = skip_short_string(bytes, i, bytes.len())?,
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i = skip_comment(bytes, i, bytes.len())?;
            }
            b'[' => {
                if let Some(opener) = long_bracket_opener(bytes, i) {
                    i = skip_long_bracket(bytes, i, opener, bytes.len())?;
                } else {
                    i += 1;
                }
            }
            b'{' | b'(' => {
                depth += 1;
                i += 1;
            }
            b'}' | b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'r' if depth == 0 && keyword_at(bytes, i, b"return") => {
                let start = skip_ignorable(bytes, i + b"return".len(), bytes.len())?;
                if start < bytes.len() && bytes[start] == b'{' {
                    let close = find_table_close(bytes, start)?;
                    return Ok((start, close));
                }
                return Err(String::from(
                    "the profile must return a table literal (`return { ... }`) so values can be \
                     edited",
                ));
            }
            _ => i += 1,
        }
    }
    Err(String::from(
        "the profile has no top-level `return { ... }` table; edit it manually",
    ))
}

/// Whether `keyword` starts at `i` and is not part of a longer identifier.
fn keyword_at(bytes: &[u8], i: usize, keyword: &[u8]) -> bool {
    if !bytes[i..].starts_with(keyword) {
        return false;
    }
    let end = i + keyword.len();
    if end < bytes.len() && is_ident(bytes[end]) {
        return false;
    }
    if i > 0 && is_ident(bytes[i - 1]) {
        return false;
    }
    true
}

/// Index of the `}` matching the `{` at `open`.
fn find_table_close(bytes: &[u8], open: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(i);
                }
                i += 1;
            }
            b'\'' | b'"' => i = skip_short_string(bytes, i, bytes.len())?,
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i = skip_comment(bytes, i, bytes.len())?;
            }
            b'[' => {
                if let Some(opener) = long_bracket_opener(bytes, i) {
                    i = skip_long_bracket(bytes, i, opener, bytes.len())?;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Err(String::from("the profile's returned table is not closed"))
}

/// Parses the direct fields of the table spanning `open..=close`.
fn parse_fields(bytes: &[u8], open: usize, close: usize) -> Result<Vec<LuaField>, String> {
    let mut fields = Vec::new();
    let mut i = open + 1;
    loop {
        i = skip_ignorable(bytes, i, close)?;
        if i >= close {
            break;
        }
        if bytes[i] == b',' || bytes[i] == b';' {
            i += 1;
            continue;
        }
        if !is_ident_start(bytes[i]) {
            return Err(format!(
                "unsupported table field syntax near byte {i}; edit the profile manually"
            ));
        }
        let key_start = i;
        while i < close && is_ident(bytes[i]) {
            i += 1;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).into_owned();
        i = skip_ignorable(bytes, i, close)?;
        if i >= close || bytes[i] != b'=' {
            return Err(format!(
                "unsupported table field syntax near byte {i}; edit the profile manually"
            ));
        }
        i += 1;
        i = skip_ignorable(bytes, i, close)?;
        let value_start = i;
        let mut depth = 0usize;
        while i < close {
            match bytes[i] {
                b'{' | b'(' => {
                    depth += 1;
                    i += 1;
                }
                b'[' => {
                    if let Some(opener) = long_bracket_opener(bytes, i) {
                        i = skip_long_bracket(bytes, i, opener, close)?;
                    } else {
                        depth += 1;
                        i += 1;
                    }
                }
                b'}' | b')' | b']' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    i += 1;
                }
                b'\'' | b'"' => i = skip_short_string(bytes, i, close)?,
                b'-' if i + 1 < close && bytes[i + 1] == b'-' => {
                    if depth == 0 {
                        break;
                    }
                    i = skip_comment(bytes, i, close)?;
                }
                b',' | b';' if depth == 0 => break,
                _ => i += 1,
            }
        }
        let mut value_end = i;
        while value_end > value_start && bytes[value_end - 1].is_ascii_whitespace() {
            value_end -= 1;
        }
        if value_end == value_start {
            return Err(String::from(
                "unsupported empty table value; edit the profile manually",
            ));
        }
        fields.push(LuaField {
            key,
            value_start,
            value_end,
        });
        if i < close && (bytes[i] == b',' || bytes[i] == b';') {
            i += 1;
        }
    }
    Ok(fields)
}

/// Applies one path edit inside the table spanning `open..=close`.
fn set_in_table(
    content: &str,
    open: usize,
    close: usize,
    segments: &[&str],
    literal: &str,
) -> Result<String, String> {
    let bytes = content.as_bytes();
    let fields = parse_fields(bytes, open, close)?;
    let key = segments[0];
    let found = fields.iter().rev().find(|field| field.key == key);
    if segments.len() == 1 {
        if let Some(field) = found {
            return Ok(format!(
                "{}{}{}",
                &content[..field.value_start],
                literal,
                &content[field.value_end..]
            ));
        }
        return insert_field(content, open, close, &fields, key, literal);
    }
    match found {
        Some(field) => {
            let mut start = field.value_start;
            while start < field.value_end && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            if start >= field.value_end || bytes[start] != b'{' {
                return Err(format!(
                    "'{key}' is not a table in this profile; edit it manually"
                ));
            }
            let inner_close = find_table_close(bytes, start)?;
            set_in_table(content, start, inner_close, &segments[1..], literal)
        }
        None => {
            let mut value = literal.to_string();
            for segment in segments[1..].iter().rev() {
                value = format!("{{ {segment} = {value} }}");
            }
            insert_field(content, open, close, &fields, key, &value)
        }
    }
}

/// Appends one field to a table, preserving comments and the closing brace's
/// line. A missing trailing separator is added to the previous field first so
/// a trailing comment can never swallow the separator.
fn insert_field(
    content: &str,
    open: usize,
    close: usize,
    fields: &[LuaField],
    key: &str,
    value: &str,
) -> Result<String, String> {
    let bytes = content.as_bytes();
    let mut stage = content.to_string();
    let mut close_at = close;
    if let Some(last) = fields.last() {
        // Look past whitespace *and* comments: a separator after a trailing
        // comment is already present, and adding a second one (`value, -- c`
        // plus the existing `,` line) would make the table a Lua syntax error.
        let cursor = skip_ignorable(bytes, last.value_end, close).unwrap_or(close);
        if cursor >= close || (bytes[cursor] != b',' && bytes[cursor] != b';') {
            stage.insert(last.value_end, ',');
            close_at += 1;
        }
    }
    let line_start = stage[..close_at]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let close_indent = &stage[line_start..close_at];
    if line_start > open + 1 && close_indent.chars().all(char::is_whitespace) {
        let indent = format!("{close_indent}    ");
        return Ok(format!(
            "{}{indent}{key} = {value},\n{}",
            &stage[..line_start],
            &stage[line_start..]
        ));
    }
    // Inline table: a field already present always ends with a comma after
    // the normalization above. Reuse existing whitespace instead of doubling
    // it.
    let separator = if fields.is_empty()
        || stage[..close_at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        ""
    } else {
        " "
    };
    let trailing = if fields.is_empty() { "" } else { " " };
    Ok(format!(
        "{}{separator}{key} = {value}{trailing}{}",
        &stage[..close_at],
        &stage[close_at..]
    ))
}

/// Skips whitespace and Lua comments starting at `i` (bounded by `limit`).
fn skip_ignorable(bytes: &[u8], mut i: usize, limit: usize) -> Result<usize, String> {
    loop {
        while i < limit && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < limit && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i = skip_comment(bytes, i, limit)?;
            continue;
        }
        return Ok(i);
    }
}

/// Skips one `--` comment (line or long-bracket form).
fn skip_comment(bytes: &[u8], i: usize, limit: usize) -> Result<usize, String> {
    let mut cursor = i + 2;
    if let Some(opener) = long_bracket_opener(bytes, cursor) {
        return skip_long_bracket(bytes, cursor, opener, limit);
    }
    while cursor < limit && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    Ok(cursor)
}

/// Skips one short-quoted Lua string starting at `i`.
fn skip_short_string(bytes: &[u8], i: usize, limit: usize) -> Result<usize, String> {
    let quote = bytes[i];
    let mut cursor = i + 1;
    while cursor < limit {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            byte if byte == quote => return Ok(cursor + 1),
            _ => cursor += 1,
        }
    }
    Err(String::from("unterminated string in the profile"))
}

/// Length of a long-bracket opener (`[[`, `[=[`, ...) at `i`, if any.
fn long_bracket_opener(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut cursor = i + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'[') {
        Some(cursor - i + 1)
    } else {
        None
    }
}

/// Skips one long-bracket string/comment starting at `i`.
fn skip_long_bracket(bytes: &[u8], i: usize, opener: usize, limit: usize) -> Result<usize, String> {
    let equals = opener - 2;
    let mut cursor = i + opener;
    while cursor < limit {
        if bytes[cursor] == b']' {
            let mut close = cursor + 1;
            let mut seen = 0usize;
            while seen < equals && bytes.get(close) == Some(&b'=') {
                close += 1;
                seen += 1;
            }
            if seen == equals && bytes.get(close) == Some(&b']') {
                return Ok(close + 1);
            }
        }
        cursor += 1;
    }
    Err(String::from("unterminated long bracket in the profile"))
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Rewrites exactly one top-level `profile = "..."` assignment, preserving
/// every other byte (comments, formatting, and user overrides).
///
/// The scanner skips short strings, comments (line and long-bracket), and
/// long-bracket strings, so a historical selector kept in a comment can never
/// be mistaken for the live assignment. The value must be a short-quoted
/// string: a long-bracket value is valid Lua but not editable by this
/// byte-preserving rewrite, so the function refuses (`None`) instead of
/// reporting a switch it did not make.
fn rewrite_selector_profile(content: &str, new_profile: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let mut assignments = 0usize;
    let mut rewritable: Option<(usize, usize)> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => i = skip_short_string(bytes, i, bytes.len()).ok()?,
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i = skip_comment(bytes, i, bytes.len()).ok()?;
            }
            b'[' if long_bracket_opener(bytes, i).is_some() => {
                let opener = long_bracket_opener(bytes, i)?;
                i = skip_long_bracket(bytes, i, opener, bytes.len()).ok()?;
            }
            byte if is_ident_start(byte) => {
                let start = i;
                let mut end = i + 1;
                while end < bytes.len() && is_ident(bytes[end]) {
                    end += 1;
                }
                i = end;
                if bytes[start..end] != *b"profile" {
                    continue;
                }
                let Ok(eq) = skip_ignorable(bytes, end, bytes.len()) else {
                    continue;
                };
                if bytes.get(eq) != Some(&b'=') {
                    continue;
                }
                assignments += 1;
                let Ok(value_start) = skip_ignorable(bytes, eq + 1, bytes.len()) else {
                    continue;
                };
                if bytes.get(value_start) == Some(&b'"') {
                    let string_end = skip_short_string(bytes, value_start, bytes.len()).ok()?;
                    if assignments == 1 {
                        rewritable = Some((value_start + 1, string_end - 1));
                    }
                }
            }
            _ => i += 1,
        }
    }
    if assignments != 1 {
        return None;
    }
    let (start, end) = rewritable?;
    let mut out = String::with_capacity(content.len() + new_profile.len());
    out.push_str(&content[..start]);
    out.push_str(new_profile);
    out.push_str(&content[end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bitty-profile-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// `(profiles dir, args)` rooted at a scratch tree with a valid selector.
    ///
    /// The explicit `--config` keeps the merge/gate path hermetic: no test
    /// reads the host's `$XDG_CONFIG_HOME` or `$BITTY_CONFIG` layer.
    fn scratch_profile_root(tag: &str) -> (PathBuf, Args) {
        let root = scratch_dir(tag);
        let profiles = root.join("profiles");
        std::fs::create_dir_all(&profiles).expect("profiles dir");
        let selector = root.join("init.lua");
        // A plain user layer (no selector envelope): valid, and it carries no
        // `profile` key, so no test depends on the host's active profile.
        std::fs::write(&selector, "return {}\n").expect("selector");
        let mut args = Args::new();
        args.config_path = Some(selector.display().to_string());
        (profiles, args)
    }

    /// Temporary siblings left behind by an atomic replacement.
    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect()
    }

    #[test]
    fn entry_budget_does_not_count_unrelated_files() {
        // 300 non-profile files before the profiles: the old
        // `PROFILE_LIST_CAP` entry limit stopped at index 256 and printed
        // nothing, even though the directory held valid profiles.
        let dir = scratch_dir("entry-budget");
        let mut ordered: Vec<PathBuf> = Vec::new();
        for index in 0..300 {
            let path = dir.join(format!("unrelated-{index:04}.txt"));
            std::fs::write(&path, "not a profile\n").expect("unrelated file");
            ordered.push(path);
        }
        for name in ["work", "alpha", "zeta"] {
            let path = dir.join(format!("{name}.lua"));
            std::fs::write(&path, "return {}\n").expect("profile file");
            ordered.push(path);
        }
        let (names, truncated) =
            collect_names(ordered.into_iter(), PROFILE_SCAN_CAP, PROFILE_LIST_CAP);
        assert_eq!(
            names,
            vec!["alpha".to_string(), "work".to_string(), "zeta".to_string()]
        );
        assert!(
            !truncated,
            "a directory below the scan bound is not truncated"
        );
    }

    #[test]
    fn rewrite_preserves_comments_and_overrides() {
        let content = "-- selector\nreturn {\n    format = \"bitty-profile-v1\",\n    profile = \"default\",\n    -- keep me\n    window = { opacity = 0.9, padding = 8 },\n}\n";
        let rewritten = rewrite_selector_profile(content, "work").expect("one assignment");
        assert!(rewritten.contains("profile = \"work\""));
        assert!(rewritten.contains("-- keep me"));
        assert!(rewritten.contains("window = { opacity = 0.9, padding = 8 }"));
        assert!(rewritten.contains("format = \"bitty-profile-v1\""));
    }

    #[test]
    fn rewrite_refuses_zero_or_duplicate_assignments() {
        assert!(
            rewrite_selector_profile("return { format = \"bitty-profile-v1\" }", "work").is_none()
        );
        let duplicate = "return {\n profile = \"a\",\n profile = \"b\",\n}\n";
        assert!(rewrite_selector_profile(duplicate, "work").is_none());
    }

    #[test]
    fn set_replaces_inline_value_and_preserves_trailing_comment() {
        let content = "return {\n    font = { family = \"Mono\", size = 12 }, -- keep\n}\n";
        let updated = set_lua_path(content, &["font", "size"], "14").expect("splice");
        assert_eq!(
            updated,
            "return {\n    font = { family = \"Mono\", size = 14 }, -- keep\n}\n"
        );
        assert!(
            bitty_config::file::parse_lua_config(
                &updated,
                &bitty_config::ConfigSource::new(bitty_config::LayerKind::Profile, Some("p.lua"))
            )
            .is_ok()
        );
    }

    #[test]
    fn set_inserts_missing_nested_tables() {
        let updated =
            set_lua_path("return {\n}\n", &["decoration", "gaps_in"], "4").expect("splice");
        assert_eq!(updated, "return {\n    decoration = { gaps_in = 4 },\n}\n");
        let plan = bitty_config::file::parse_lua_config(
            &updated,
            &bitty_config::ConfigSource::new(bitty_config::LayerKind::Profile, Some("p.lua")),
        )
        .expect("valid profile");
        let decoration = plan.decoration.expect("decoration plan");
        assert_eq!(decoration.gaps_in, 4);
        // Inline empty tables stay inline.
        assert_eq!(
            set_lua_path("return {}\n", &["decoration", "gaps_in"], "4").expect("inline"),
            "return {decoration = { gaps_in = 4 }}\n"
        );
    }

    #[test]
    fn set_appends_scalars_and_keeps_leading_comments() {
        let content =
            "return {\n    -- appearance first\n    appearance = { theme = \"dark\" },\n}\n";
        let updated = set_lua_path(content, &["mod_key"], "\"super\"").expect("splice");
        assert_eq!(
            updated,
            "return {\n    -- appearance first\n    appearance = { theme = \"dark\" },\n    mod_key = \"super\",\n}\n"
        );
        // A missing trailing comma is added before a trailing comment.
        let commented = "return {\n    font = { size = 12 } -- note\n}\n";
        let updated = set_lua_path(commented, &["mod_key"], "\"super\"").expect("splice");
        assert_eq!(
            updated,
            "return {\n    font = { size = 12 }, -- note\n    mod_key = \"super\",\n}\n"
        );
    }

    #[test]
    fn set_uses_the_last_duplicate_key() {
        let content = "return { font = { size = 10 }, font = { size = 12 } }\n";
        let updated = set_lua_path(content, &["font", "size"], "14").expect("splice");
        assert_eq!(
            updated,
            "return { font = { size = 10 }, font = { size = 14 } }\n"
        );
    }

    #[test]
    fn set_appends_inline_fields_without_doubling_spaces() {
        let content = "return { font = { family = \"M\", size = 12 } }\n";
        let updated =
            set_lua_path(content, &["font", "line_height"], "1.2").expect("inline insert");
        assert_eq!(
            updated,
            "return { font = { family = \"M\", size = 12, line_height = 1.2 } }\n"
        );
    }

    #[test]
    fn set_keeps_a_separator_that_follows_a_trailing_comment() {
        // The separator already present on the next line must be seen through
        // the comment; inserting a second one would be a Lua syntax error and
        // `set` would blame the user's file for it.
        let content = "return {\n    font = { family = \"M\", size = 12 } -- note\n    ,\n}\n";
        let updated = set_lua_path(content, &["mod_key"], "\"super\"").expect("splice");
        assert_eq!(
            updated,
            "return {\n    font = { family = \"M\", size = 12 } -- note\n    ,\n    \
             mod_key = \"super\",\n}\n"
        );
        assert!(!updated.contains(",,"), "no doubled separator: {updated}");
        bitty_config::file::parse_lua_config(
            &updated,
            &bitty_config::ConfigSource::new(bitty_config::LayerKind::Profile, Some("p.lua")),
        )
        .expect("spliced profile parses");
    }

    #[test]
    fn rewrite_skips_comments_and_long_bracket_values() {
        // Live value is a long-bracket string: the byte-preserving editor
        // cannot rewrite it, so `use` must refuse instead of editing the
        // historical selector kept in the comment.
        let long_value = "return {\n    format = \"bitty-profile-v1\",\n    --[[ historical \
                          selector:\n    profile = \"default\"\n    ]]\n    profile = \
                          [[alpha]],\n}\n";
        assert!(rewrite_selector_profile(long_value, "beta").is_none());

        // Live value is short-quoted: only that assignment changes. The old
        // line scanner also matched the comment and refused with a misleading
        // "exactly one" error.
        let short_value = "return {\n    format = \"bitty-profile-v1\",\n    --[[ historical \
                           selector:\n    profile = \"default\"\n    ]]\n    profile = \
                           \"alpha\",\n}\n";
        let rewritten = rewrite_selector_profile(short_value, "beta").expect("one live assignment");
        assert!(rewritten.contains("--[[ historical selector:"));
        assert!(rewritten.contains("profile = \"default\""));
        assert!(rewritten.contains("profile = \"beta\""));
        assert!(!rewritten.contains("profile = \"alpha\""));
        // Offsets outside the rewritten value are byte-identical.
        let (before, after) = short_value.split_once("alpha").expect("live value present");
        assert!(rewritten.starts_with(before), "{rewritten}");
        assert!(rewritten.ends_with(after), "{rewritten}");
    }

    #[cfg(unix)]
    #[test]
    fn referencing_profiles_skips_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = scratch_dir("referencing-symlink");
        std::fs::write(dir.join("base.lua"), "return {}\n").expect("base");
        std::fs::write(dir.join("real.lua"), "return { extends = \"base\" }\n").expect("real");
        symlink(dir.join("real.lua"), dir.join("linked.lua")).expect("symlink");
        assert_eq!(
            referencing_profiles(&dir, "base"),
            vec![String::from("real")],
            "a symlinked profile never counts as a dependent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_refuses_non_table_intermediate_and_non_table_chunk() {
        let err = set_lua_path("return { font = \"mono\" }\n", &["font", "size"], "12")
            .expect_err("scalar parent");
        assert!(err.contains("not a table"), "{err}");
        let err = set_lua_path("return font()\n", &["font", "size"], "12").expect_err("call");
        assert!(err.contains("table literal"), "{err}");
        let err =
            set_lua_path("return {}\nprint('x')\n", &["font"], "12").expect_err("trailing code");
        assert!(err.contains("continues"), "{err}");
    }

    #[test]
    fn set_skips_comments_and_strings_when_finding_the_table() {
        let content =
            "-- return { not real }\nreturn {\n    terminal = { shell = \"/bin/sh\" },\n}\n";
        let updated =
            set_lua_path(content, &["terminal", "shell"], "\"/bin/fish\"").expect("splice");
        assert!(updated.contains("shell = \"/bin/fish\""));
        assert!(updated.contains("-- return { not real }"));
    }

    #[test]
    fn render_literal_quotes_escapes_and_rejects_bad_values() {
        let text = schema_field_entry("font.family").expect("text field");
        assert_eq!(
            render_literal(text, "JetBrains Mono").expect("text"),
            "\"JetBrains Mono\""
        );
        assert_eq!(
            render_literal(text, "\"quoted name\"").expect("quoted"),
            "\"quoted name\""
        );
        assert_eq!(
            render_literal(text, "a\\b\"c").expect("escaped"),
            "\"a\\\\b\\\"c\""
        );
        assert!(render_literal(text, "bad\nvalue").is_err());

        let integer = schema_field_entry("window.padding").expect("integer field");
        assert_eq!(render_literal(integer, "8").expect("int"), "8");
        assert!(render_literal(integer, "-1").is_err());
        assert!(render_literal(integer, "8.5").is_err());

        let number = schema_field_entry("window.opacity").expect("number field");
        assert_eq!(render_literal(number, "0.95").expect("float"), "0.95");
        assert!(render_literal(number, "NaN").is_err());

        let boolean = schema_field_entry("selection.auto_copy").expect("bool field");
        assert_eq!(render_literal(boolean, "true").expect("true"), "true");
        assert!(render_literal(boolean, "yes").is_err());

        let list = schema_field_entry("font.fallback").expect("list field");
        assert_eq!(
            render_literal(list, "Mono, Sans").expect("list"),
            "{ \"Mono\", \"Sans\" }"
        );
        assert_eq!(render_literal(list, "").expect("empty list"), "{}");
        assert!(render_literal(list, "a,,b").is_err());
    }

    #[test]
    fn schema_paths_reject_traversal_and_unknown_keys() {
        assert!(schema_field("set", "font.size").is_ok());
        assert!(schema_field("set", "theme").is_ok());
        for bad in [
            "../../etc/passwd",
            "../font.size",
            "font/size",
            "font..size",
            "font.size.extra.deep.path.value",
        ] {
            let err = schema_field("set", bad).expect_err(bad);
            assert_eq!(err.code, "E_PROFILE_INVALID", "{bad}");
        }
        let err = schema_field("set", "font.unknown").expect_err("unknown");
        assert!(
            err.message.contains("unknown config path"),
            "{}",
            err.message
        );
    }

    #[test]
    fn atomic_replace_cleans_up_after_rename_failure() {
        // Replacing a directory fails at rename time; the temporary file must
        // be removed and the directory must stay intact.
        let dir = scratch_dir("atomic-cleanup");
        let target = dir.join("work.lua");
        std::fs::create_dir(&target).expect("target dir");
        let err = write_profile_atomic(&target, "return {}\n", None).expect_err("rename over dir");
        assert!(
            matches!(&err, ProfileWriteError::Io(err) if err.kind() != std::io::ErrorKind::NotFound),
            "{err:?}"
        );
        assert!(target.is_dir(), "target directory preserved");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary file removed: {leftovers:?}"
        );
    }

    #[test]
    fn set_refuses_a_concurrent_edit_and_keeps_its_bytes() {
        // A writer landing between `set`'s read and its commit must win: the
        // command refuses with the conflict class (exit 2), the concurrent
        // bytes survive, and no staged temporary file is left behind.
        let (dir, args) = scratch_profile_root("set-lost-update");
        let path = dir.join("work.lua");
        std::fs::write(&path, "return {}\n").expect("profile");
        let concurrent = "return { window = { padding = 99 } }\n";
        with_commit_hook(move |path| {
            std::fs::write(path, concurrent).expect("concurrent edit");
        });

        let failure = run_set(&args, &dir, "work", "window.padding", "8")
            .expect_err("a concurrent edit must refuse the write");
        assert_eq!(failure.code, "E_PROFILE_CONFLICT");
        assert_eq!(failure.exit, 2);
        assert!(
            failure.message.contains("changed while editing"),
            "names the race: {}",
            failure.message
        );
        assert!(
            failure.message.contains(&path.display().to_string()),
            "names the file: {}",
            failure.message
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("profile"),
            concurrent,
            "the concurrent bytes are never overwritten"
        );
        assert!(
            temp_leftovers(&dir).is_empty(),
            "no temporary file remains: {:?}",
            temp_leftovers(&dir)
        );
        let _ = std::fs::remove_dir_all(dir.parent().expect("scratch root"));
    }

    #[test]
    fn profile_identity_detects_edits_replacement_and_removal() {
        let dir = scratch_dir("profile-identity");
        let path = dir.join("work.lua");
        std::fs::write(&path, "return {}\n").expect("profile");
        let (_, identity) = read_profile("set", "work", &path).expect("read");
        verify_profile_unchanged(&path, &identity).expect("unchanged bytes are accepted");

        // In-place edit: same handle target, different bytes.
        std::fs::write(&path, "return { font = { size = 14 } }\n").expect("edit");
        assert!(matches!(
            verify_profile_unchanged(&path, &identity),
            Err(ProfileWriteError::Changed)
        ));

        // Replacement: the path now names a different file.
        let (_, replaced) = read_profile("set", "work", &path).expect("read");
        let other = dir.join("other.lua");
        std::fs::write(&other, "return { terminal = { scrollback = 500 } }\n").expect("other");
        std::fs::rename(&other, &path).expect("replace");
        assert!(matches!(
            verify_profile_unchanged(&path, &replaced),
            Err(ProfileWriteError::Changed)
        ));

        // Removal is a change too, not a filesystem failure.
        std::fs::remove_file(&path).expect("remove");
        assert!(matches!(
            verify_profile_unchanged(&path, &replaced),
            Err(ProfileWriteError::Changed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_envelope_is_one_bounded_line() {
        let failure = Failure::conflict(String::from("bitty config profile create: exists"));
        let envelope = error_envelope("config profile create", "create", &failure);
        assert!(envelope.starts_with('{') && envelope.ends_with('}'));
        assert!(!envelope.contains('\n'));
        assert_eq!(
            envelope,
            "{\"v\":1,\"command\":\"config profile create\",\"ok\":false,\"error\":{\"class\":\"config\",\"code\":\"E_PROFILE_CONFLICT\",\"message\":\"bitty config profile create: exists\"},\"result\":{\"kind\":\"profile\",\"action\":\"create\",\"valid\":false,\"name\":null,\"diagnostics\":[{\"severity\":\"error\",\"message\":\"bitty config profile create: exists\"}]}}"
        );
    }

    #[test]
    fn clone_source_parses_and_returns_the_same_bytes() {
        let dir = scratch_dir("clone-source");
        let src_path = dir.join("src.lua");
        let parent_path = dir.join("parent.lua");
        std::fs::write(&parent_path, "return {}\n").expect("parent profile");

        // The source's own bytes fail to parse even though the parent chain is
        // valid: the error must come from the candidate parse, not from a
        // parent-chain loader.
        let broken = "return { extends = \"parent\", font = ".to_string();
        let err = clone_source_from_content(&dir, "src", &src_path, broken)
            .expect_err("a syntax error must fail before any copy");
        assert!(
            err.message.contains("invalid profile 'src'"),
            "source attribution: {}",
            err.message
        );
        assert!(
            err.message.contains("parse error")
                || err.message.contains("validation error at 'config'"),
            "the refusal is the source parse: {}",
            err.message
        );
        assert!(
            !err.message.contains("profile 'parent'"),
            "not a parent-chain error: {}",
            err.message
        );

        // An oversized body fails on the same parse, before any copy.
        let oversized = format!(
            "-- {}\nreturn {{}}\n",
            "x".repeat(bitty_config::file::MAX_CONFIG_FILE_BYTES)
        );
        let err = clone_source_from_content(&dir, "src", &src_path, oversized)
            .expect_err("an oversized body must fail");
        assert!(
            err.message.contains("invalid profile 'src'") && err.message.contains("exceeds"),
            "size bound on the parsed bytes: {}",
            err.message
        );

        // Valid bytes: the returned content is byte-identical to what was
        // parsed, and the chain is the parent followed by the candidate.
        let content = "-- exact bytes\nreturn { extends = \"parent\" }\n".to_string();
        let source =
            clone_source_from_content(&dir, "src", &src_path, content.clone()).expect("valid");
        assert_eq!(source.content, content, "clone copies the parsed bytes");
        assert_eq!(source.chain.len(), 2, "parent then candidate");
        let expected_parent = parent_path.display().to_string();
        let expected_src = src_path.display().to_string();
        assert_eq!(
            source.chain[0].source.path.as_deref(),
            Some(expected_parent.as_str())
        );
        assert_eq!(
            source.chain[1].source.path.as_deref(),
            Some(expected_src.as_str())
        );

        // A self-parent is rejected without loading the source as its own
        // parent (which would be a second read of the same file).
        let cycle = "return { extends = \"src\" }\n".to_string();
        let err = clone_source_from_content(&dir, "src", &src_path, cycle)
            .expect_err("a self-parent must fail");
        assert!(
            err.message.contains("cycle"),
            "cycle diagnostic: {}",
            err.message
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lua_string_literal_escapes_every_control_character() {
        assert_eq!(lua_string_literal("plain"), "\"plain\"");
        assert_eq!(lua_string_literal("a\\b\"c"), "\"a\\\\b\\\"c\"");
        assert_eq!(lua_string_literal("A\nB\rC\tD"), "\"A\\nB\\rC\\tD\"");
        assert_eq!(lua_string_literal("a\0b"), "\"a\\x00b\"");
        assert_eq!(lua_string_literal("ends\\"), "\"ends\\\\\"");
        assert_eq!(lua_string_literal("lit\\n"), "\"lit\\\\n\"");
        assert_eq!(lua_string_literal("\u{1b}"), "\"\\x1b\"");
        assert_eq!(lua_string_literal("\u{7f}"), "\"\\x7f\"");
        assert_eq!(lua_string_literal("\u{80}"), "\"\\u{80}\"");
        assert_eq!(lua_string_literal("\u{85}"), "\"\\u{85}\"");

        // The encoded literal must read back as the same value through the
        // strict parser: an escaped pin is only useful if it round-trips.
        // Covers NUL, a lone trailing backslash, the literal text `\n`
        // (backslash + n), CRLF, C1 controls, and the last scalar.
        let raw = "A\0B\\C\\nD\r\nE\tF\u{1b}\u{85}\u{80}\u{10ffff}G";
        let literal = lua_string_literal(raw);
        assert!(
            !literal.contains('\n') && !literal.contains('\r') && !literal.contains('\t'),
            "control characters stay escaped: {literal:?}"
        );
        let body = format!("return {{ font = {{ family = {literal}, size = 12 }} }}\n");
        let plan = bitty_config::file::parse_lua_config(
            &body,
            &bitty_config::ConfigSource::new(bitty_config::LayerKind::Profile, Some("p.lua")),
        )
        .expect("escaped literal parses");
        assert_eq!(plan.font.expect("font plan").family, raw);
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn rename_no_replace_never_overwrites_an_existing_destination() {
        let dir = scratch_dir("rename-no-replace");
        let old = dir.join("old.lua");
        let new = dir.join("new.lua");
        std::fs::write(&old, "return { old = 1 }\n").expect("old profile");
        std::fs::write(&new, "return { new = 2 }\n").expect("new profile");
        let err = rename_no_replace(&old, &new).expect_err("destination exists");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&new).expect("destination"),
            "return { new = 2 }\n",
            "the raced destination is never overwritten"
        );
        assert!(old.is_file(), "the source stays in place");
        std::fs::remove_file(&new).expect("clear destination");
        rename_no_replace(&old, &new).expect("free destination renames");
        assert!(!old.exists());
        assert!(new.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_falls_back_when_no_no_replace_primitive_exists() {
        // Unsupported platform (no safe `RENAME_NOREPLACE`/`RENAME_EXCL`
        // wrapper): `rename_with_attempt(None)` must take the pre-checked
        // fallback and still perform the move.
        let dir = scratch_dir("rename-fallback");
        let old = dir.join("old.lua");
        let new = dir.join("new.lua");
        std::fs::write(&old, "return { fallback = true }\n").expect("old profile");
        rename_with_attempt(&old, &new, |_, _| None).expect("fallback rename");
        assert!(!old.exists(), "fallback removed the source name");
        assert_eq!(
            std::fs::read_to_string(&new).expect("destination"),
            "return { fallback = true }\n"
        );

        // A real no-replace failure is surfaced, never retried as a clobbering
        // rename that would destroy the raced destination.
        let source = dir.join("source.lua");
        let raced = dir.join("raced.lua");
        std::fs::write(&source, "return { source = true }\n").expect("source profile");
        std::fs::write(&raced, "return { raced = true }\n").expect("raced destination");
        let err = rename_with_attempt(&source, &raced, |_, _| {
            Some(Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
        })
        .expect_err("attempt failure is surfaced");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&raced).expect("destination"),
            "return { raced = true }\n",
            "the raced destination is preserved"
        );
        assert!(source.is_file(), "the source stays in place");

        // A successful attempt is authoritative too: the fallback never runs
        // a second rename behind it.
        let untouched = dir.join("untouched.lua");
        rename_with_attempt(&source, &untouched, |_, _| Some(Ok(()))).expect("attempt succeeded");
        assert!(source.is_file(), "the attempt owns the move");
        assert!(!untouched.exists(), "no fallback rename after the attempt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_maps_a_raced_destination_to_the_conflict_class() {
        // The pre-check (`ensure_absent`) passed, then a concurrent creator
        // landed. The no-replace commit reports `AlreadyExists`, and
        // `run_rename` must turn that into the same conflict/exit-2 class as
        // the pre-check, leaving both files byte-identical to the race state.
        let (dir, args) = scratch_profile_root("rename-race");
        let old_path = dir.join("race-old.lua");
        let new_path = dir.join("race-new.lua");
        std::fs::write(&old_path, "return {}\n").expect("source profile");
        let concurrent = "return { concurrent = true }\n";
        with_rename_override(move |_, new| {
            std::fs::write(new, concurrent).expect("concurrent creator");
            Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
        });

        let failure = run_rename(&args, &dir, "race-old", "race-new")
            .expect_err("a destination created after the pre-check must conflict");
        assert_eq!(failure.code, "E_PROFILE_CONFLICT");
        assert_eq!(failure.exit, 2);
        assert!(
            failure.message.contains("already exists"),
            "conflict message: {}",
            failure.message
        );
        assert!(
            failure.message.contains(&new_path.display().to_string()),
            "names the destination: {}",
            failure.message
        );
        assert_eq!(
            std::fs::read_to_string(&new_path).expect("destination"),
            concurrent,
            "the raced destination is unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(&old_path).expect("source"),
            "return {}\n",
            "the source stays intact"
        );
        let _ = std::fs::remove_dir_all(dir.parent().expect("scratch root"));
    }

    #[test]
    fn success_envelope_escapes_the_command() {
        let envelope = success_envelope("config \"profile\"", "{\"kind\":\"profile\"}");
        assert_eq!(
            envelope,
            "{\"v\":1,\"command\":\"config \\\"profile\\\"\",\"ok\":true,\"result\":{\"kind\":\"profile\"}}"
        );
    }
}
