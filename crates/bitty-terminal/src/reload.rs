//! `bitty reload`: apply the running instance's config without restarting it
//! (bitty#2927).
//!
//! Runtime-class subcommand per the accepted CLI contract. It is the
//! discoverable entry point for "I edited `init.lua`, apply it": it resolves
//! the live instance exactly like `bitty ctl` (explicit `--socket`, then
//! `--instance`, then inherited `BITTY_SOCKET`/`BITTY_INSTANCE_ID`, then the
//! exactly-one-live shortcut, else exit 6) and sends the existing
//! `core.config.reload` control verb. No second config loader and no second
//! apply path exist: the instance re-reads `init.lua` plus the active profile
//! through the same plan/validation pipeline as startup and adopts the live
//! subset through the runtime setters the file watcher already uses.
//!
//! # Why a top-level word
//!
//! `bitty ctl config reload` remains the registry verb (one resource/verb per
//! executable). `bitty reload` is a thin alias because applying config is a
//! high-frequency, single-action workflow with its own result contract
//! (changed / restart-required / rejected plus a plan version) and its own
//! exit codes; burying it two words deep behind a generic control grammar made
//! it undiscoverable. The alias sends the same verb, so authority, scopes, and
//! bounds are unchanged.
//!
//! # Applied set (never silently ignored)
//!
//! Applied live: theme/palette, font family and size (plus line height and
//! letter spacing), keymaps, Leader, appearance and statusline options,
//! decoration and window presentation, and `views` selectors. Workspaces,
//! windows, panes, their PTYs, and scrollback are preserved; nothing is
//! re-spawned and no layout is rebuilt. A changed field the runtime cannot
//! adopt live is reported as `restart-required` and stays at its launch value.
//!
//! The installed plugin set is not part of that plan (bitty#2969). The plugin
//! store is read by startup discovery only; the plugin host can rebuild one
//! generation but `PluginRuntime::reload` is teardown-then-rebuild (a failed
//! load leaves the plugin disposed, not unchanged) and enabling needs a
//! whole-store `discover()` pass. A store change since launch is therefore
//! reported as `restart-required` with the affected plugin ids and exits 4,
//! never a silent no-op; `bitty plugin enable|disable` prints the same hint.
//!
//! A daemon-owned, debounced, per-file watcher is the `bittyd` follow-up and
//! is not part of this command: `bitty reload` is an explicit, scriptable
//! request, while the in-process poll watcher keeps working independently.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` applied, unchanged, a `--dry-run` report, or restart-required (the
//!   config is valid; the restart list is printed and never silent).
//! - `2` usage error (unknown flag, stray positional, bad `--format`).
//! - `3` rejected / load error / apply error: the running plan is untouched
//!   and the diagnostic names the dotted field path.
//! - `4` the installed plugin set changed since launch and only a restart can
//!   apply it: the reply names the affected plugin ids under
//!   `plugins_restart_required` and never reads as an applied reload.
//! - `6` runtime unavailable (no live instance, ambiguous instance, transport
//!   failure, or an instance that does not serve live reload, e.g. `--safe`).
//! - `7` permission (the instance did not grant `config.modify`).
//!
//! Elevation: `core.config.reload` requires the `config.modify` scope, which
//! the servo grants only through the explicit `BITTY_CTL_ELEVATE` allowlist on
//! the instance (`BITTY_CTL_ELEVATE=config.modify`). Without it the request
//! fails closed with exit 7 and no partial state.

#![forbid(unsafe_code)]

use crate::cli::Args;
use crate::ctl::{CtlFormat, CtlTargeting};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success: applied, unchanged, dry-run report, or restart-required.
pub const EXIT_OK: i32 = 0;
/// CLI usage error (unknown flag, stray positional, bad `--format`).
pub const EXIT_USAGE: i32 = 2;
/// A plugin-set change can only be applied by a restart (bitty#2969).
///
/// Distinct from the config `restart-required` class (exit 0): a config field
/// the runtime cannot adopt live is still part of the accepted reload plan,
/// while the installed plugin set is a separate resource the reload verb never
/// applies. Reporting success would recreate the silent no-op of bitty#2969.
pub const EXIT_RESTART: i32 = 4;

// ---------------------------------------------------------------------------
// Request model
// ---------------------------------------------------------------------------

/// One validated `bitty reload` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReloadRequest {
    /// Report the change list without applying anything.
    pub(crate) dry_run: bool,
    /// Targeting overrides (`--socket`, `--instance`) plus the output shape.
    pub(crate) targeting: CtlTargeting,
}

/// `bitty reload` parse failure. `Help` prints help and exits 0; `Usage`
/// prints the diagnostic plus usage and exits 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReloadParseError {
    /// `-h` / `--help` (print help to stdout, exit 0).
    Help,
    /// Usage violation with a one-line diagnostic.
    Usage { message: String },
}

impl ReloadParseError {
    /// One-line stderr diagnostic (without the usage trailer).
    fn message(&self) -> String {
        match self {
            Self::Help => String::from("bitty reload: help requested"),
            Self::Usage { message } => message.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Usage and help
// ---------------------------------------------------------------------------

/// Short usage for stderr (fail-closed exit 2 trailer).
#[must_use]
pub fn reload_usage() -> String {
    String::from(
        "usage: bitty reload [--dry-run] [--socket PATH] [--instance ID] [--format table|json|jsonl]\n\
         \n\
         options:\n\
         \x20 --dry-run        Report the change list without applying it\n\
         \x20 --socket PATH    Explicit IPC socket; bypasses discovery\n\
         \x20 --instance ID    Explicit instance (1..=64 [a-z0-9_-]); resolved via discovery\n\
         \x20 --format SHAPE   Output shape: table|json|jsonl (default table)\n\
         \x20 -h, --help       Print this help and exit (never needs an instance)\n\
         \n\
         Applies theme/palette, font family and size, keymaps, Leader, appearance\n\
         and statusline options; workspaces, windows, panes, their PTYs and\n\
         scrollback are preserved. A change the runtime cannot adopt live is\n\
         reported as restart-required, never silently ignored. The installed\n\
         plugin set (bitty plugin enable|disable) is not part of the config\n\
         plan: a change since launch is reported as restart-required with the\n\
         plugin ids and exits 4 — restart bitty to apply it.\n\
         \n\
         Exit codes: 0 applied/unchanged/dry-run/restart-required (config);\n\
         \x20 2 usage; 3 rejected (dotted field path); 4 plugin-set restart;\n\
         \x20 6 no live instance; 7 permission.\n\
         \n\
         examples:\n\
         \x20 bitty reload\n\
         \x20 bitty reload --dry-run\n\
         \x20 bitty reload --format json",
    )
}

/// Full help for `bitty reload --help` (stdout, exit 0).
#[must_use]
pub fn reload_help_text() -> String {
    format!(
        "bitty reload — apply the running instance's config (runtime, needs one live instance)\n\
         \n\
         Usage: bitty reload [--dry-run] [--socket PATH] [--instance ID] [--format SHAPE]\n\
         \n\
         Re-reads the instance's init.lua plus its active profile through the same\n\
         \x20 plan/validation pipeline as startup, then applies the live subset\n\
         \x20 atomically: a failed validation leaves the running plan untouched and\n\
         \x20 reports the dotted field path. No restart and no window close.\n\
         \n\
         Targeting (precedence: --socket, --instance, BITTY_SOCKET/BITTY_INSTANCE_ID,\n\
         \x20 exactly-one-live fallback, else ambiguity error exit 6):\n  \
            --socket PATH    Explicit IPC socket; bypasses discovery\n  \
            --instance ID    Explicit instance (1..=64 [a-z0-9_-]); resolved via discovery\n  \
            --format SHAPE   Output shape: table|json|jsonl (default table)\n  \
            --dry-run        Report the change list without applying it\n  \
            -h, --help       Print this help and exit (never needs an instance)\n\
          \n\
          Applied live: theme/palette, font family and size (plus line height and\n\
          \x20 letter spacing), keymaps, Leader, appearance and statusline options,\n\
          \x20 decoration and window presentation, and views selectors.\n\
          \x20 Preserved: workspaces, windows, panes, their PTYs, and scrollback.\n\
          \x20 Not live: fields the runtime cannot adopt without a restart are listed\n\
          \x20 as restart-required and stay at their launch values.\n\
          \n\
          Result: the reply carries the running plan version, the changed dotted\n\
          \x20 paths, the rejected dotted paths (with the reason), the\n\
          \x20 restart-required paths, and the plugin ids whose installed\n\
          \x20 enabled state changed since launch. --format json emits the v1\n\
          \x20 envelope under command core.config.reload; table output is human,\n\
          \x20 not a contract.\n\
          \n\
          Plugin set: `bitty plugin enable|disable` edits the installed plugin\n\
          \x20 store, which the running instance reads only at startup. A change\n\
          \x20 since launch cannot be applied live (the plugin host's generation\n\
          \x20 rebuild is teardown-then-rebuild and re-discovery is a whole-store\n\
          \x20 pass), so it is reported as restart-required with the affected ids\n\
          \x20 and exits 4. The running plugin generations are never touched.\n\
          \n\
          Exit codes: exit 0 applied/unchanged/dry-run, or restart-required for\n\
          \x20 config fields; exit 2 usage; exit 3 rejected or load/apply error;\n\
          \x20 exit 4 the plugin set needs a restart; exit 6 no live instance,\n\
          \x20 transport failure, or an instance that does not serve live reload\n\
          \x20 (--safe); exit 7 permission.\n\
          \n\
          Elevation: the instance must grant config.modify. Launch it with\n\
          \x20 BITTY_CTL_ELEVATE=config.modify; without that the request fails\n\
          \x20 closed (exit 7) and nothing is applied.\n\
          \n\
          A daemon-owned debounced watcher is the bittyd follow-up; this command\n\
          \x20 is the explicit, scriptable request, and the in-process file watcher\n\
          \x20 keeps working independently.\n\
          \n\
          Version: {}\n",
        env!("CARGO_PKG_VERSION"),
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse `tokens` (argv words after the `reload` word).
///
/// Pure and total for unit testing: no env/fs/net. `--socket`, `--instance`,
/// and `--format` accept both the separated and `--flag=value` forms.
/// `-h`/`--help` anywhere becomes [`ReloadParseError::Help`].
pub(crate) fn parse_reload_request(tokens: &[String]) -> Result<ReloadRequest, ReloadParseError> {
    let mut socket: Option<String> = None;
    let mut instance: Option<String> = None;
    let mut format_raw: Option<String> = None;
    let mut dry_run = false;

    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(ReloadParseError::Help);
        }
        if token == "--dry-run" {
            dry_run = true;
            i += 1;
            continue;
        }
        if token == "--socket" {
            let value = tokens.get(i + 1).ok_or_else(|| ReloadParseError::Usage {
                message: String::from(
                    "bitty reload: --socket needs a path (see `bitty reload --help`)",
                ),
            })?;
            validate_socket(value)?;
            socket = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--socket=") {
            validate_socket(value)?;
            socket = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--instance" {
            let value = tokens.get(i + 1).ok_or_else(|| ReloadParseError::Usage {
                message: String::from(
                    "bitty reload: --instance needs an id (see `bitty reload --help`)",
                ),
            })?;
            validate_instance(value)?;
            instance = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--instance=") {
            validate_instance(value)?;
            instance = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--format" {
            let value = tokens.get(i + 1).ok_or_else(|| ReloadParseError::Usage {
                message: String::from("bitty reload: --format needs a value (table|json|jsonl)"),
            })?;
            CtlFormat::parse(Some(value)).map_err(|message| ReloadParseError::Usage { message })?;
            format_raw = Some(value.clone());
            i += 2;
            continue;
        }
        if let Some(value) = token.strip_prefix("--format=") {
            CtlFormat::parse(Some(value)).map_err(|message| ReloadParseError::Usage { message })?;
            format_raw = Some(value.to_string());
            i += 1;
            continue;
        }
        if token == "--" {
            return Err(ReloadParseError::Usage {
                message: String::from("bitty reload: unexpected `--` (see `bitty reload --help`)"),
            });
        }
        // `--config`/`--profile` are startup selectors: the running instance
        // reloads the sources it was launched with. Refuse them explicitly
        // instead of silently reloading a different file/profile than the
        // caller asked for.
        if token == "--config" || token.starts_with("--config=") {
            return Err(ReloadParseError::Usage {
                message: String::from(
                    "bitty reload: reloads the instance's own init.lua and active profile; \
                     --config is a startup flag (pass it when starting bitty)",
                ),
            });
        }
        if token == "--profile" || token.starts_with("--profile=") {
            return Err(ReloadParseError::Usage {
                message: String::from(
                    "bitty reload: reloads the instance's own init.lua and active profile; \
                     --profile is a startup flag (pass it when starting bitty)",
                ),
            });
        }
        if token.starts_with('-') {
            return Err(ReloadParseError::Usage {
                message: format!(
                    "bitty reload: unknown flag {token:?} (see `bitty reload --help`)"
                ),
            });
        }
        return Err(ReloadParseError::Usage {
            message: format!(
                "bitty reload: takes no positional arguments (got {token:?}; see `bitty reload --help`)"
            ),
        });
    }

    let format = CtlFormat::parse(format_raw.as_deref())
        .map_err(|message| ReloadParseError::Usage { message })?;
    Ok(ReloadRequest {
        dry_run,
        targeting: CtlTargeting {
            socket,
            instance,
            format,
        },
    })
}

/// Validate an explicit `--socket` value (shape only; OS auth at connect).
fn validate_socket(value: &str) -> Result<(), ReloadParseError> {
    if value.is_empty() || value.contains('\0') {
        return Err(ReloadParseError::Usage {
            message: String::from("bitty reload: --socket must be a non-empty path without NUL"),
        });
    }
    if value.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
        return Err(ReloadParseError::Usage {
            message: format!(
                "bitty reload: --socket path too long for AF_UNIX ({} > {} payload bytes)",
                value.len(),
                bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES
            ),
        });
    }
    Ok(())
}

/// Validate an explicit `--instance` value (grammar only; resolution at
/// connect).
fn validate_instance(value: &str) -> Result<(), ReloadParseError> {
    if value.is_empty() || value.len() > bitty_ipc::devtools::MAX_INSTANCE_ID_LEN {
        return Err(ReloadParseError::Usage {
            message: format!(
                "bitty reload: --instance must be 1..={} (got {} bytes)",
                bitty_ipc::devtools::MAX_INSTANCE_ID_LEN,
                value.len()
            ),
        });
    }
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(ReloadParseError::Usage {
            message: String::from("bitty reload: --instance must match ^[a-z0-9_-]+$"),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Runs `bitty reload`; returns the process exit code.
///
/// `--help` never needs an instance. Global `--socket`/`--instance`/`--format`
/// before the `reload` word merge with the per-command flags (a differing
/// duplicate is a usage error, exit 2).
pub(crate) fn run_cli(args: &Args) -> i32 {
    let request = match parse_reload_request(&args.reload_raw) {
        Ok(request) => request,
        Err(ReloadParseError::Help) => {
            print!("{}", reload_help_text());
            return EXIT_OK;
        }
        Err(err) => {
            eprintln!("{}\n{}", err.message(), reload_usage());
            return EXIT_USAGE;
        }
    };
    let mut targeting = request.targeting;
    // `--config`/`--profile` before the word are startup selectors for this
    // client process, not for the running instance. Refuse them instead of
    // silently reloading the instance's own sources.
    if args.config_path.is_some() || args.profile.is_some() {
        eprintln!(
            "bitty reload: reloads the instance's own init.lua and active profile; \
             --config/--profile are startup flags (pass them when starting bitty)\n{}",
            reload_usage()
        );
        return EXIT_USAGE;
    }
    if let Some(pre) = args.ctl_socket_pre.as_deref() {
        match targeting.socket.as_deref() {
            None => targeting.socket = Some(pre.to_string()),
            Some(post) if post == pre => {}
            Some(post) => {
                eprintln!(
                    "bitty reload: conflicting --socket {pre:?} vs {post:?} (pass once; see `bitty reload --help`)\n{}",
                    reload_usage()
                );
                return EXIT_USAGE;
            }
        }
    }
    if let Some(pre) = args.ctl_instance_pre.as_deref() {
        match targeting.instance.as_deref() {
            None => targeting.instance = Some(pre.to_string()),
            Some(post) if post == pre => {}
            Some(post) => {
                eprintln!(
                    "bitty reload: conflicting --instance {pre:?} vs {post:?} (pass once; see `bitty reload --help`)\n{}",
                    reload_usage()
                );
                return EXIT_USAGE;
            }
        }
    }
    // A global `--format` before the word applies when the command set none.
    let post_has_format = args
        .reload_raw
        .iter()
        .any(|t| t == "--format" || t.starts_with("--format="));
    if !post_has_format {
        if let Some(global) = args.doctor_format.as_deref() {
            match CtlFormat::parse(Some(global)) {
                Ok(fmt) => targeting.format = fmt,
                Err(message) => {
                    eprintln!("{message}\n{}", reload_usage());
                    return EXIT_USAGE;
                }
            }
        }
    }
    // A pre-word `--dry-run` lands in the generic upgrade flag (the global
    // parser's catch-all). Honor it here so `bitty --dry-run reload` never
    // applies a change silently.
    let dry_run = request.dry_run || args.upgrade_dry_run;
    crate::ctl::execute_reload(&targeting, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parse_defaults_to_table_and_no_dry_run() {
        let request = parse_reload_request(&[]).expect("bare reload parses");
        assert!(!request.dry_run);
        assert_eq!(request.targeting.format, CtlFormat::Table);
        assert_eq!(request.targeting.socket, None);
        assert_eq!(request.targeting.instance, None);
    }

    #[test]
    fn parse_accepts_flags_in_any_order_and_equals_form() {
        let request = parse_reload_request(&tokens(&[
            "--dry-run",
            "--socket=/tmp/bitty.sock",
            "--instance",
            "dev-1",
            "--format",
            "json",
        ]))
        .expect("flags parse");
        assert!(request.dry_run);
        assert_eq!(request.targeting.socket.as_deref(), Some("/tmp/bitty.sock"));
        assert_eq!(request.targeting.instance.as_deref(), Some("dev-1"));
        assert_eq!(request.targeting.format, CtlFormat::Json);
    }

    #[test]
    fn parse_help_anywhere() {
        assert_eq!(
            parse_reload_request(&tokens(&["--dry-run", "--help"])),
            Err(ReloadParseError::Help)
        );
        assert_eq!(
            parse_reload_request(&tokens(&["-h"])),
            Err(ReloadParseError::Help)
        );
    }

    #[test]
    fn parse_rejects_positionals_unknown_flags_and_bad_values() {
        for raw in [
            &["extra"][..],
            &["--"],
            &["--bogus"],
            &["--format", "bogus"],
            &["--socket"],
            &["--socket="],
            &["--instance"],
            &["--instance", "Bad/Id"],
        ] {
            assert!(
                matches!(
                    parse_reload_request(&tokens(raw)),
                    Err(ReloadParseError::Usage { .. })
                ),
                "{raw:?} must be a usage error"
            );
        }
    }

    #[test]
    fn parse_rejects_startup_selectors_with_a_targeted_message() {
        for raw in [
            &["--config", "/tmp/init.lua"][..],
            &["--config=/tmp/init.lua"],
            &["--profile", "work"],
            &["--profile=work"],
        ] {
            match parse_reload_request(&tokens(raw)) {
                Err(ReloadParseError::Usage { message }) => {
                    assert!(message.contains("startup flag"), "{raw:?}: {message}");
                }
                other => panic!("{raw:?} must be a usage error, got {other:?}"),
            }
        }
    }

    #[test]
    fn help_documents_the_applied_set_and_exit_codes() {
        let help = reload_help_text();
        assert!(help.contains("theme/palette"), "{help}");
        assert!(help.contains("keymaps"), "{help}");
        assert!(help.contains("scrollback"), "{help}");
        assert!(help.contains("restart-required"), "{help}");
        assert!(help.contains("plugin set"), "{help}");
        assert!(help.contains("--dry-run"), "{help}");
        assert!(help.contains("exit 3"), "{help}");
        assert!(help.contains("exit 4"), "{help}");
        assert!(help.contains("exit 6"), "{help}");
    }
}
