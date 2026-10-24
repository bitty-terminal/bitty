//! `bitty config profile` management (CTX-1404 / issue #2187).
//!
//! Local, pre-runtime operations over `$XDG_CONFIG_HOME/bitty/profiles/`:
//!
//! - `list`: bounded directory metadata only; never evaluates Lua.
//! - `path NAME`: validates the name grammar and prints the resolved path.
//! - `show NAME`: loads the single-parent `extends` chain (bounded) and
//!   prints the profile-only effective values with attribution.
//! - `use NAME`: explicit persistent activation. Validates the full chain,
//!   then rewrites only the `profile = "..."` value inside the existing
//!   versioned selector, preserving comments and user overrides. Legacy
//!   `init.lua` files require the explicit `bitty init --migrate` path.
//!
//! These commands never install/enable plugins, touch component state, spawn
//! a shell, or edit shell rc files. `BITTY_PROFILE` is a one-launch input and
//! is never an output target.

use std::path::{Path, PathBuf};

use crate::cli::Args;
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

/// Short usage for `bitty config profile` (stderr, fail-closed exit 2).
pub(crate) fn profile_usage() -> String {
    "usage: bitty config profile <list|show|use|path> [NAME]\n\
     \n\
     profiles: $XDG_CONFIG_HOME/bitty/profiles/<name>.lua\n\
     \x20 list       list profile names (sorted, bounded; no Lua evaluation)\n\
     \x20 show NAME  load + validate the extends chain and print profile values\n\
     \x20 use NAME   activate NAME by rewriting only the init.lua selector\n\
     \x20 path NAME  print the resolved profile path without creating it\n\
     \n\
     use is explicit activation; it never reloads a running instance and\n\
     never installs, enables, or grants plugin/component state."
        .to_string()
}

/// A profile sub-verb parsed from `config_args`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileVerb {
    List,
    Show,
    Use,
    Path,
}

impl ProfileVerb {
    /// Every profile sub-verb in help order (single source for shell
    /// completion).
    pub(crate) const VERBS: &[(&str, Self)] = &[
        ("list", Self::List),
        ("show", Self::Show),
        ("use", Self::Use),
        ("path", Self::Path),
    ];

    fn parse(raw: &str) -> Option<Self> {
        Self::VERBS
            .iter()
            .find(|(name, _)| *name == raw)
            .map(|(_, verb)| *verb)
    }

    fn needs_name(self) -> bool {
        matches!(self, Self::Show | Self::Use | Self::Path)
    }
}

/// Runs `bitty config profile ...`; returns the process exit code.
pub(crate) fn run_profile_subcommand(args: &Args) -> i32 {
    let Some(raw_verb) = args.config_args.first() else {
        eprintln!(
            "bitty config profile: missing sub-verb\n{}",
            profile_usage()
        );
        return 2;
    };
    let Some(verb) = ProfileVerb::parse(raw_verb) else {
        eprintln!(
            "bitty config profile: unknown sub-verb '{raw_verb}' (want list|show|use|path)\n{}",
            profile_usage()
        );
        return 2;
    };
    let rest = &args.config_args[1..];
    let name = if verb.needs_name() {
        match rest {
            [name] => Some(name.as_str()),
            [] => {
                eprintln!(
                    "bitty config profile {}: missing profile name\n{}",
                    raw_verb,
                    profile_usage()
                );
                return 2;
            }
            [extra, ..] => {
                eprintln!(
                    "bitty config profile {}: unexpected argument '{extra}'\n{}",
                    raw_verb,
                    profile_usage()
                );
                return 2;
            }
        }
    } else {
        if let Some(extra) = rest.first() {
            eprintln!(
                "bitty config profile list: unexpected argument '{extra}'\n{}",
                profile_usage()
            );
            return 2;
        }
        None
    };
    let dir = match profile_dir(args) {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    match verb {
        ProfileVerb::List => run_list(&dir),
        ProfileVerb::Path => match name {
            Some(name) => run_path(&dir, name),
            None => 2,
        },
        ProfileVerb::Show => match name {
            Some(name) => run_show(&dir, name),
            None => 2,
        },
        ProfileVerb::Use => match name {
            Some(name) => run_use(args, &dir, name),
            None => 2,
        },
    }
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

fn run_list(dir: &Path) -> i32 {
    // Inspect up to the scan bound (not the output cap) so unrelated files
    // cannot exhaust the budget before valid profiles are reached.
    let (names, truncated) = match scan_names(dir, PROFILE_SCAN_CAP, PROFILE_LIST_CAP) {
        Ok(result) => result,
        Err(err) => {
            eprintln!(
                "bitty config profile list: cannot read '{}': {err}",
                dir.display()
            );
            return 1;
        }
    };
    if truncated {
        eprintln!(
            "bitty config profile list: stopped at {PROFILE_SCAN_CAP} entries / \
             {PROFILE_LIST_CAP} profiles; narrow the directory"
        );
    }
    for name in names {
        println!("{name}");
    }
    0
}

fn run_path(dir: &Path, raw_name: &str) -> i32 {
    let name = match bitty_config::file::validate_profile_name(raw_name) {
        Ok(name) => name,
        Err(err) => {
            eprintln!("bitty config profile path: {err}");
            return 2;
        }
    };
    println!("{}", dir.join(format!("{name}.lua")).display());
    0
}

fn run_show(dir: &Path, raw_name: &str) -> i32 {
    let name = match bitty_config::file::validate_profile_name(raw_name) {
        Ok(name) => name,
        Err(err) => {
            eprintln!("bitty config profile show: {err}");
            return 2;
        }
    };
    let path = dir.join(format!("{name}.lua"));
    if !path.exists() {
        eprintln!(
            "bitty config profile show: profile '{name}' not found ('{}')",
            path.display()
        );
        return 2;
    }
    let chain = match bitty_config::file::load_profile_chain_in_dir(&name, dir) {
        Ok(chain) => chain,
        Err(err) => {
            eprintln!("bitty config profile show: invalid profile '{name}': {err}");
            return 2;
        }
    };
    let merged = match bitty_config::file::resolve_effective_with_profiles(
        None,
        chain.clone(),
        &bitty_config::file::CliOverrides::default(),
    ) {
        Ok(merged) => merged,
        Err(err) => {
            eprintln!("bitty config profile show: invalid profile '{name}': {err}");
            return 2;
        }
    };
    println!("profile = \"{name}\"");
    println!("path = \"{}\"", path.display());
    for (index, layer) in chain.iter().enumerate() {
        let source = layer
            .source
            .path
            .as_deref()
            .unwrap_or("(unknown)")
            .to_string();
        println!("layer[{index}] = \"{source}\"");
    }
    let e = &merged.effective;
    // Real attribution: a sparse preset (for example `minimal`, which sets
    // only `theme`) keeps the shipped defaults for every other row; labeling
    // those `(profile)` overstates the profile's content (#2349). Extends
    // ancestors are named by their own file path.
    let source = |field: &str| -> String {
        match merged.source_of(field) {
            Some(found) if found.layer == bitty_config::LayerKind::Profile => match &found.path {
                Some(path) => format!("profile: {path}"),
                None => "profile".to_string(),
            },
            Some(found) if found.layer == bitty_config::LayerKind::CoreDefaults => {
                "default".to_string()
            }
            Some(found) => found.layer.label().to_string(),
            None => "default".to_string(),
        }
    };
    let theme = e
        .appearance
        .theme
        .as_deref()
        .map_or(String::from("(default)"), |theme| format!("\"{theme}\""));
    println!(
        "appearance.theme = {theme} ({})",
        source("appearance.theme")
    );
    println!(
        "font.family = \"{}\" ({})",
        e.font.family,
        source("font.family")
    );
    println!("font.size = {} ({})", e.font.size, source("font.size"));
    println!(
        "terminal.scrollback = {} ({})",
        e.terminal.scrollback,
        source("terminal.scrollback")
    );
    println!(
        "window.opacity = {} ({})",
        e.window.opacity,
        source("window.opacity")
    );
    println!(
        "window.padding = {} ({})",
        e.window.padding,
        source("window.padding")
    );
    println!(
        "decoration.gaps_in = {} ({})",
        e.decoration.gaps_in,
        source("decoration.gaps_in")
    );
    println!(
        "close_confirm = \"{}\" ({})",
        e.close_confirm.as_str(),
        source("close_confirm")
    );
    0
}

fn run_use(args: &Args, dir: &Path, raw_name: &str) -> i32 {
    let name = match bitty_config::file::validate_profile_name(raw_name) {
        Ok(name) => name,
        Err(err) => {
            eprintln!("bitty config profile use: {err}");
            return 2;
        }
    };
    let profile_path = dir.join(format!("{name}.lua"));
    if !profile_path.exists() {
        eprintln!(
            "bitty config profile use: profile '{name}' not found ('{}')",
            profile_path.display()
        );
        return 2;
    }
    // Validate the whole chain before any mutation.
    if let Err(err) = bitty_config::file::load_profile_chain_in_dir(&name, dir) {
        eprintln!("bitty config profile use: invalid profile '{name}': {err}");
        return 2;
    }
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let probed = match bitty_config::file::probe_config_path(explicit.as_deref()) {
        Some(probed) => probed,
        None => {
            eprintln!("bitty config profile use: no config root ($XDG_CONFIG_HOME or $HOME unset)");
            return 2;
        }
    };
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
    if !selector.exists() {
        let content = render_selector_lua(&name);
        return match write_init_config(&selector, &content, false) {
            Ok(_) => {
                println!(
                    "bitty config profile use: activated '{name}' in '{}'",
                    selector.display()
                );
                0
            }
            Err(InitWriteError::Refused(message)) => {
                eprintln!("{message}");
                2
            }
            Err(InitWriteError::Io(message)) => {
                eprintln!("{message}");
                1
            }
        };
    }
    // Bound-check and parse the exact bytes that will be rewritten, so the
    // selector is never read unbounded and the in-memory edit is based on a
    // validated preimage.
    let meta = match std::fs::metadata(&selector) {
        Ok(meta) => meta,
        Err(err) => {
            eprintln!(
                "bitty config profile use: cannot inspect '{}': {err}",
                selector.display()
            );
            return 1;
        }
    };
    if meta.len() > bitty_config::file::MAX_CONFIG_FILE_BYTES as u64 {
        eprintln!(
            "bitty config profile use: '{}' exceeds {} bytes; refusing to rewrite it",
            selector.display(),
            bitty_config::file::MAX_CONFIG_FILE_BYTES
        );
        return 2;
    }
    let content = match std::fs::read_to_string(&selector) {
        Ok(content) => content,
        Err(err) => {
            eprintln!(
                "bitty config profile use: cannot read '{}': {err}",
                selector.display()
            );
            return 1;
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
            eprintln!(
                "bitty config profile use: run `bitty init --migrate` to migrate a legacy configuration"
            );
            return 2;
        }
    };
    if layer.selector.is_none() {
        eprintln!(
            "bitty config profile use: '{}' is a legacy configuration, not a selector; run `bitty init --migrate` first",
            selector.display()
        );
        return 2;
    }
    let Some(rewritten) = rewrite_selector_profile(&content, &name) else {
        eprintln!(
            "bitty config profile use: cannot locate exactly one top-level `profile = \"...\"` assignment in '{}'",
            selector.display()
        );
        return 2;
    };
    if let Err(err) = bitty_config::file::parse_lua_config(&rewritten, &source) {
        eprintln!(
            "bitty config profile use: rewritten selector is invalid ({err}); refusing to write"
        );
        return 2;
    }
    match write_init_config(&selector, &rewritten, true) {
        Ok(outcome) => {
            if let Some(backup) = &outcome.backup {
                println!(
                    "bitty config profile use: backed up previous selector to '{}'",
                    backup.display()
                );
            }
            println!(
                "bitty config profile use: activated '{name}' in '{}'",
                selector.display()
            );
            0
        }
        Err(InitWriteError::Refused(message)) => {
            eprintln!("{message}");
            2
        }
        Err(InitWriteError::Io(message)) => {
            eprintln!("{message}");
            1
        }
    }
}

/// Rewrites exactly one top-level `profile = "..."` assignment, preserving
/// every other byte (comments, formatting, and user overrides).
fn rewrite_selector_profile(content: &str, new_profile: &str) -> Option<String> {
    let mut out = String::with_capacity(content.len() + new_profile.len());
    let mut rewrites = 0usize;
    for line in content.split_inclusive('\n') {
        let (body, ending) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        let trimmed = body.trim_start();
        let is_assignment = trimmed.strip_prefix("profile").is_some_and(|rest| {
            rest.chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '=')
        });
        if is_assignment {
            if let Some(eq) = trimmed.find('=') {
                let after_eq = &trimmed[eq + 1..];
                if let Some(open) = after_eq.find('"') {
                    let rest = &after_eq[open + 1..];
                    if let Some(close) = rest.find('"') {
                        let prefix_len = body.len() - after_eq.len() + open + 1;
                        out.push_str(&body[..prefix_len]);
                        out.push_str(new_profile);
                        out.push_str(&rest[close..]);
                        out.push_str(ending);
                        rewrites += 1;
                        continue;
                    }
                }
            }
        }
        out.push_str(body);
        out.push_str(ending);
    }
    if rewrites == 1 { Some(out) } else { None }
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
}
