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
enum ProfileVerb {
    List,
    Show,
    Use,
    Path,
}

impl ProfileVerb {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "list" => Some(Self::List),
            "show" => Some(Self::Show),
            "use" => Some(Self::Use),
            "path" => Some(Self::Path),
            _ => None,
        }
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
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
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

fn run_list(dir: &Path) -> i32 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return 0;
        }
        Err(err) => {
            eprintln!(
                "bitty config profile list: cannot read '{}': {err}",
                dir.display()
            );
            return 1;
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if names.len() >= PROFILE_LIST_CAP {
            eprintln!(
                "bitty config profile list: stopped at {PROFILE_LIST_CAP} entries; narrow the directory"
            );
            break;
        }
        let path = entry.path();
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
    let theme = e
        .appearance
        .theme
        .as_deref()
        .map_or(String::from("(default)"), |theme| format!("\"{theme}\""));
    println!("appearance.theme = {theme} (profile)");
    println!("font.family = \"{}\" (profile)", e.font.family);
    println!("font.size = {} (profile)", e.font.size);
    println!("terminal.scrollback = {} (profile)", e.terminal.scrollback);
    println!("window.opacity = {} (profile)", e.window.opacity);
    println!("window.padding = {} (profile)", e.window.padding);
    println!("decoration.gaps_in = {} (profile)", e.decoration.gaps_in);
    println!("close_confirm = \"{}\" (profile)", e.close_confirm.as_str());
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
        eprintln!(
            "bitty config profile use: legacy '{}' is not a selector; run `bitty init --migrate` first",
            probed.path.display()
        );
        return 2;
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
