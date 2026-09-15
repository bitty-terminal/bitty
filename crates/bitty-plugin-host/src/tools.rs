#![forbid(unsafe_code)]
//! Layer-2 system-CLI allowlist enforcement (CTX-0444).
//!
//! Canonical record for the accepted `[tools.git]` slice (v1) is
//! `bitty-plugins-docs`
//! `specifications/plugin-reuse-and-providers.md`
//! (`Accepted [tools.git] contract (v1)`, CTX-0425): seven read-only verbs
//! with bounded output. This module is pure validation — no I/O, no process
//! spawning, no VM. The spawn execution surface itself is CTX-0445; this
//! module provides the fail-closed predicates that surface must call.
//!
//! Security boundary: allowlist bypass is a sandbox escape. Every predicate
//! fails closed (deny-by-default, explicit `false`, no silent pass).

/// The only accepted Layer-2 tool (CTX-0425 v1).
pub const ACCEPTED_TOOL_GIT: &str = "git";

/// Maximum tool name length (policy bound, mirrors id-segment bound).
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// Allowed `git` subcommands — read-only observation only (accepted v1).
///
/// Write verbs (`commit`, `push`, `reset`, mutating `checkout`, etc.) are
/// intentionally absent; staging or commit UX needs explicit user action plus
/// a broader grant.
pub const GIT_ALLOWED_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "log",
    "branch",
    "show",
    "rev-parse",
    "ls-files",
];

/// Maximum `git` args per spawn (accepted bound).
pub const MAX_GIT_ARGS: usize = 32;

/// Maximum bytes per `git` arg (accepted bound).
pub const MAX_GIT_ARG_BYTES: usize = 256;

/// Maximum total bytes for `git` spawn args (accepted bound, 8 KiB).
pub const MAX_GIT_TOTAL_BYTES: usize = 8 * 1024;

/// Panel observation payload bound (accepted bound, 8 KiB).
pub const PAYLOAD_MAX_BYTES: usize = 8 * 1024;

/// Whether `name` is the accepted Layer-2 tool.
///
/// Only `git` is accepted (CTX-0425 v1). Any other tool fails closed until its
/// own slice is accepted.
#[must_use]
pub fn is_accepted_tool(name: &str) -> bool {
    name == ACCEPTED_TOOL_GIT
}

/// Whether `name` is a syntactically valid tool name.
///
/// Lowercase `^[a-z0-9]+(-[a-z0-9]+)*$`, `1..64` bytes. Rejects path
/// separators (`/`, `\`), dot segments (`.`, `..`), absolute paths,
/// extensions (`.exe`), whitespace, controls, and shell metacharacters.
/// This is the PATH-manipulation guard to the extent host-resolvable: the
/// manifest may only name the tool, never a path.
#[must_use]
pub fn is_valid_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > TOOL_NAME_MAX_LEN {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    for part in name.split('-') {
        if part.is_empty() {
            return false;
        }
        for b in part.bytes() {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit()) {
                return false;
            }
        }
        let first = part.as_bytes()[0];
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return false;
        }
    }
    // First char must be alphanumeric lowercase (rejects `/`, `.`, etc.).
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    true
}

/// Whether `args` is an allowlisted `git` invocation under `[tools.git]`.
///
/// Pure, bounded, fail-closed: `args` must be non-empty, `args[0]` in
/// [`GIT_ALLOWED_SUBCOMMANDS`], `args.len() <= MAX_GIT_ARGS`, each arg
/// `1..=MAX_GIT_ARG_BYTES`, total `<= MAX_GIT_TOTAL_BYTES`, no
/// NUL/control/shell metacharacters, no risky flags (exact `--upload-pack`,
/// `--receive-pack`, `--exec` plus the `--upload-pack=` / `--receive-pack=` /
/// `--exec=` prefixed forms for verb smuggling via `--exec=`-style flags).
#[must_use]
pub fn is_allowed_git_args(args: &[String]) -> bool {
    if args.is_empty() || args.len() > MAX_GIT_ARGS {
        return false;
    }
    let mut total = 0usize;
    for arg in args {
        if arg.is_empty() || arg.len() > MAX_GIT_ARG_BYTES {
            return false;
        }
        total = total.saturating_add(arg.len());
        if total > MAX_GIT_TOTAL_BYTES {
            return false;
        }
        if arg.contains('\0') || arg.chars().any(|c| c.is_control()) {
            return false;
        }
        // Shell metacharacters — fail closed, no interpolation.
        if arg.contains(';')
            || arg.contains('&')
            || arg.contains('|')
            || arg.contains('`')
            || arg.contains('$')
            || arg.contains('(')
            || arg.contains(')')
            || arg.contains('<')
            || arg.contains('>')
            || arg.contains('\\')
            || arg.contains('"')
            || arg.contains('\'')
        {
            return false;
        }
    }
    // First arg is the subcommand and must be allowlisted.
    let sub = &args[0];
    if !GIT_ALLOWED_SUBCOMMANDS.contains(&sub.as_str()) {
        return false;
    }
    // Risky flags — fail closed (exact plus `=`-prefixed smuggling forms).
    for arg in args {
        if arg == "--upload-pack" || arg == "--receive-pack" || arg == "--exec" {
            return false;
        }
        if arg.contains("--upload-pack=") || arg.contains("--receive-pack=") {
            return false;
        }
        if arg.contains("--exec=") {
            return false;
        }
    }
    true
}

/// Whether spawning `tool` with `args` is allowlisted.
///
/// Fail-closed: `tool` must be a valid name and the accepted tool, and `args`
/// must satisfy [`is_allowed_git_args`] (for `git`). Any other executable is
/// denied; any non-allowlisted verb or smuggled flag is denied.
#[must_use]
pub fn is_tool_spawn_allowed(tool: &str, args: &[String]) -> bool {
    if !is_valid_tool_name(tool) {
        return false;
    }
    if !is_accepted_tool(tool) {
        return false;
    }
    // Only `git` is accepted today; dispatch is exhaustive.
    if tool == ACCEPTED_TOOL_GIT {
        return is_allowed_git_args(args);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_tool_is_git_only() {
        assert!(is_accepted_tool("git"));
        assert!(!is_accepted_tool("rg"));
        assert!(!is_accepted_tool("fd"));
        assert!(!is_accepted_tool("bat"));
        assert!(!is_accepted_tool(""));
        assert!(!is_accepted_tool("Git"));
        assert!(!is_accepted_tool("GIT"));
        assert!(!is_accepted_tool("git.exe"));
    }

    #[test]
    fn tool_names_reject_path_manipulation() {
        assert!(is_valid_tool_name("git"));
        assert!(is_valid_tool_name("rg"));
        assert!(is_valid_tool_name("my-tool"));
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("/usr/bin/git"));
        assert!(!is_valid_tool_name("./git"));
        assert!(!is_valid_tool_name("../evil"));
        assert!(!is_valid_tool_name(".."));
        assert!(!is_valid_tool_name("git.exe"));
        assert!(!is_valid_tool_name("git;evil"));
        assert!(!is_valid_tool_name("git evil"));
        assert!(!is_valid_tool_name("git:evil"));
        assert!(!is_valid_tool_name("git/evil"));
        assert!(!is_valid_tool_name("git\\evil"));
        assert!(!is_valid_tool_name("-git"));
        assert!(!is_valid_tool_name("git-"));
        assert!(!is_valid_tool_name("Git"));
        assert!(!is_valid_tool_name("git\0evil"));
        assert!(!is_valid_tool_name(&"a".repeat(TOOL_NAME_MAX_LEN + 1)));
    }

    #[test]
    fn git_args_allowlist_admits_read_only_verbs() {
        for verb in GIT_ALLOWED_SUBCOMMANDS {
            assert!(
                is_allowed_git_args(&[(*verb).to_string()]),
                "verb {verb} must be allowlisted"
            );
        }
        assert_eq!(GIT_ALLOWED_SUBCOMMANDS.len(), 7);
        assert!(is_allowed_git_args(&[
            "status".to_string(),
            "--porcelain".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "--oneline".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_write_verbs() {
        for verb in [
            "commit", "push", "reset", "checkout", "fetch", "pull", "clone",
        ] {
            assert!(
                !is_allowed_git_args(&[verb.to_string()]),
                "write verb {verb} must be denied"
            );
        }
        assert!(!is_allowed_git_args(&[]));
    }

    #[test]
    fn git_args_deny_verb_smuggling_via_exec_flags() {
        // Exact risky flags.
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--exec".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--upload-pack".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--receive-pack".to_string()
        ]));
        // `=`-prefixed smuggling forms (verb smuggling via `--exec=`-style flags).
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--exec=evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--upload-pack=evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--receive-pack=evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "foo".to_string(),
            "--exec=/bin/sh".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_shell_metachars_and_bounds() {
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "; rm -rf /".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "$(evil)".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "`evil`".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "a&b".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "a|b".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "a\0b".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "a\x07b".to_string()
        ]));
        // Bounds.
        let long = "a".repeat(MAX_GIT_ARG_BYTES + 1);
        assert!(!is_allowed_git_args(&["status".to_string(), long]));
        let many: Vec<String> = std::iter::once("status".to_string())
            .chain((0..MAX_GIT_ARGS).map(|i| format!("arg{i}")))
            .collect();
        assert!(!is_allowed_git_args(&many));
    }

    #[test]
    fn spawn_allowed_requires_accepted_tool_and_allowlisted_args() {
        assert!(is_tool_spawn_allowed(
            "git",
            &["status".to_string(), "--porcelain".to_string()]
        ));
        assert!(!is_tool_spawn_allowed("rg", &["status".to_string()]));
        assert!(!is_tool_spawn_allowed(
            "git",
            &["push".to_string(), "origin".to_string()]
        ));
        assert!(!is_tool_spawn_allowed(
            "git",
            &["log".to_string(), "--exec=evil".to_string()]
        ));
        assert!(!is_tool_spawn_allowed(
            "/usr/bin/git",
            &["status".to_string()]
        ));
        assert!(!is_tool_spawn_allowed("", &["status".to_string()]));
        assert!(!is_tool_spawn_allowed("git", &[]));
    }
}
