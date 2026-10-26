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
//! - `use NAME`: explicit persistent activation. Validates the full chain,
//!   then rewrites only the `profile = "..."` value inside the existing
//!   versioned selector, preserving comments and user overrides. Legacy
//!   `init.lua` files require the explicit `bitty init --migrate` path.
//! - `get NAME PATH`: effective value at one documented dotted schema path.
//! - `set NAME PATH VALUE`: validates the value through the same strict
//!   loader `bitty config check` uses (including `local:<name>` theme
//!   resolution), edits exactly that path inside the profile's returned Lua
//!   table, and commits atomically (unique sibling temporary file,
//!   `sync_all`, rename over the target). A failed validation or write leaves
//!   the previous bytes intact. Because the typed plan merges `font`,
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

use std::io::Write;
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
     first (reported as inherited-pinned), so one edited leaf never resets its\n\
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
    fn operands(self) -> &'static [(&'static str, &'static str)] {
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
        Err(message) => return fail(format, &command, Failure::usage(message)),
    };
    let dir = match profile_dir(args) {
        Ok(dir) => dir,
        Err(message) => return fail(format, &command, Failure::usage(message)),
    };
    let outcome = match verb {
        ProfileVerb::Create => run_create(&dir, operands[0]),
        ProfileVerb::Clone => run_clone(&dir, operands[0], operands[1]),
        ProfileVerb::Rename => run_rename(args, &dir, operands[0], operands[1]),
        ProfileVerb::Delete => run_delete(args, &dir, operands[0]),
        ProfileVerb::List => run_list(args, &dir),
        ProfileVerb::Show => run_show(&dir, operands[0]),
        ProfileVerb::Use => run_use(args, &dir, operands[0]),
        ProfileVerb::Get => run_get(&dir, operands[0], operands[1]),
        ProfileVerb::Set => run_set(&dir, operands[0], operands[1], operands[2]),
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
        Err(failure) => fail(format, &command, failure),
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
fn fail(format: ConfigFormat, command: &str, failure: Failure) -> i32 {
    if format == ConfigFormat::Json {
        println!("{}", error_envelope(command, &failure));
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
    // Validate the whole chain (and the same `local:<name>` theme selection
    // the strict loader resolves) before any mutation.
    let current = merged_profile(dir, "use", &name)?;
    check_local_theme("use", &name, &current.effective)?;
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
fn run_clone(dir: &Path, raw_src: &str, raw_dst: &str) -> Result<Reply, Failure> {
    let src = valid_name("clone", raw_src)?;
    let dst = valid_name("clone", raw_dst)?;
    reject_reserved("clone", &dst)?;
    let src_path = profile_path(dir, &src);
    let dst_path = profile_path(dir, &dst);
    ensure_present("clone", &src, &src_path)?;
    ensure_absent("clone", &dst, &dst_path)?;
    let chain = match bitty_config::file::load_profile_chain_in_dir(&src, dir) {
        Ok(chain) => chain,
        Err(err) => return Err(invalid_profile("clone", &src, &err)),
    };
    let merged = match bitty_config::file::resolve_effective_with_profiles(
        None,
        chain,
        &bitty_config::file::CliOverrides::default(),
    ) {
        Ok(merged) => merged,
        Err(err) => return Err(invalid_profile("clone", &src, &err)),
    };
    check_local_theme("clone", &src, &merged.effective)?;
    let content = read_profile("clone", &src, &src_path)?;
    validate_new_profile(&content, &dst_path)
        .map_err(|err| invalid_profile("clone", &src, &err))?;
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
    if let Err(err) = std::fs::rename(&old_path, &new_path) {
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
fn run_set(dir: &Path, raw_name: &str, raw_key: &str, raw_value: &str) -> Result<Reply, Failure> {
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
    let content = read_profile("set", &name, &path)?;
    let current = merged_profile(dir, "set", &name)?;
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
            updated = set_lua_path(&updated, &[table], &seed).map_err(|message| {
                Failure::invalid(format!(
                    "bitty config profile set: cannot pin inherited '{table}' values into profile \
                     '{name}': {message}"
                ))
            })?;
            pinned.push(table);
        }
    }
    let updated = set_lua_path(&updated, &segments, &literal).map_err(|message| {
        Failure::invalid(format!(
            "bitty config profile set: cannot set '{}' in profile '{name}': {message}",
            field.path
        ))
    })?;
    let merged = merge_candidate(dir, &name, &path, &updated)?;
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
    write_profile_atomic(&path, &updated)
        .map_err(|err| io_failure("set", &format!("replace '{}'", path.display()), &err))?;
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
            "bitty config profile set: pinned inherited {names} values into '{name}' so the \
             edited leaf keeps every inherited sibling\n"
        ));
    }
    let pinned_json = pinned
        .iter()
        .map(|table| json_string(table))
        .collect::<Vec<_>>()
        .join(",");
    let result = format!(
        "{{\"kind\":\"profile\",\"action\":\"set\",\"name\":{},\"path\":{},\"key\":{},\"value\":{},\"type\":{},\"layer\":{},\"source\":{},\"changed\":true,\"inherited_pinned\":[{}]}}",
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

/// Reads one profile with the loader's size bound.
fn read_profile(verb: &str, name: &str, path: &Path) -> Result<String, Failure> {
    let meta = std::fs::metadata(path)
        .map_err(|err| io_failure(verb, &format!("inspect '{}'", path.display()), &err))?;
    if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
        return Err(Failure::invalid(format!(
            "bitty config profile {verb}: profile '{name}' exceeds {} bytes; refusing to edit it",
            bitty_config::file::MAX_CONFIG_FILE_BYTES
        )));
    }
    std::fs::read_to_string(path)
        .map_err(|err| io_failure(verb, &format!("read '{}'", path.display()), &err))
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
/// the edited entry, and merge/validate the whole chain (including the
/// `local:<name>` theme selection the strict loader resolves).
fn merge_candidate(
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
    let merged = bitty_config::file::resolve_effective_with_profiles(
        None,
        chain,
        &bitty_config::file::CliOverrides::default(),
    )
    .map_err(|err| invalid_profile("set", name, &err))?;
    check_local_theme("set", name, &merged.effective)?;
    Ok(merged)
}

/// Loads and merges one profile's current `extends` chain.
fn merged_profile(
    dir: &Path,
    verb: &str,
    name: &str,
) -> Result<bitty_config::MergedConfig, Failure> {
    let chain = bitty_config::file::load_profile_chain_in_dir(name, dir)
        .map_err(|err| invalid_profile(verb, name, &err))?;
    bitty_config::file::resolve_effective_with_profiles(
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

/// One double-quoted Lua string.
fn lua_string_literal(raw: &str) -> String {
    format!("\"{}\"", crate::init::init_lua_escape(raw))
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

/// Runs the strict loader's `local:<name>` theme resolution over a candidate
/// merge, so `set`/`clone`/`use` never accept a body `config check` rejects.
fn check_local_theme(
    verb: &str,
    name: &str,
    effective: &bitty_config::EffectiveConfig,
) -> Result<(), Failure> {
    let mut effective = effective.clone();
    bitty_config::local_theme::apply_selection(&mut effective).map_err(|err| {
        Failure::invalid(format!(
            "bitty config profile {verb}: invalid profile '{name}': {err}"
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
/// the user may write `Super` or `Paste_From_Primary`, while the re-read
/// effective value is `super`/`paste_from_primary`.
fn requested_value(field: &SchemaField, raw: &str) -> ConfigValue {
    match field.path {
        "mod_key" => {
            if let Ok(key) = bitty_config::keymap::ModKey::parse(raw) {
                return ConfigValue::Text(key.canonical().to_string());
            }
        }
        "mouse.right" | "mouse.middle" => {
            if let Ok(binding) = bitty_config::keymap::MouseButtonBinding::parse(raw, field.path) {
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

/// Replaces a profile atomically: unique sibling temporary file, `sync_all`,
/// then rename over the target. Any failure removes the temporary file and
/// leaves the previous bytes intact.
fn write_profile_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
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
            Err(err) => return Err(err),
        }
    }
    let Some((temp_path, mut file)) = temp else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temporary profile names",
        ));
    };
    let write_result = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(&temp_path, permissions);
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
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
            Ok(format!("\"{}\"", crate::init::init_lua_escape(text)))
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
                items.push(format!("\"{}\"", crate::init::init_lua_escape(item)));
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
fn error_envelope(command: &str, failure: &Failure) -> String {
    let message = bounded_message(&failure.message);
    format!(
        "{{\"v\":1,\"command\":{},\"ok\":false,\"error\":{{\"class\":\"config\",\"code\":{},\"message\":{}}}}}",
        json_string(command),
        json_string(failure.code),
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
        let err = write_profile_atomic(&target, "return {}\n").expect_err("rename over dir");
        assert!(err.kind() != std::io::ErrorKind::NotFound, "{err}");
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
    fn error_envelope_is_one_bounded_line() {
        let failure = Failure::conflict(String::from("bitty config profile create: exists"));
        let envelope = error_envelope("config profile create", &failure);
        assert!(envelope.starts_with('{') && envelope.ends_with('}'));
        assert!(!envelope.contains('\n'));
        assert_eq!(
            envelope,
            "{\"v\":1,\"command\":\"config profile create\",\"ok\":false,\"error\":{\"class\":\"config\",\"code\":\"E_PROFILE_CONFLICT\",\"message\":\"bitty config profile create: exists\"}}"
        );
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
