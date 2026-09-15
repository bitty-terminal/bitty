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
/// `--exec=` prefixed forms for verb smuggling via `--exec=`-style flags),
/// no config override (`-c` exact, `--config-env` prefix), no repo escape
/// (`--git-dir` / `--work-tree` prefix), no file-write or external-driver
/// flags (`--output` / `--ext-diff` / `--textconv` prefix), and verb-aware
/// `branch` pinning (mutating shorts/longs plus bare creation denied; see
/// inline).
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
        // Config override via `-c key=val` (exact only: `--cached`,
        // `--color`, etc. must keep working).
        if arg == "-c" {
            return false;
        }
        // File-write / external-driver / repo-escape / env-config flags.
        // Prefix denial covers both bare (`--output`) and `=...` forms
        // (`--output=/tmp/pwned.txt`); `--no-ext-diff` / `--no-textconv`
        // stay allowed (safe defaults, `--no-` prefix does not match).
        if arg.starts_with("--output")
            || arg.starts_with("--ext-diff")
            || arg.starts_with("--textconv")
            || arg.starts_with("--git-dir")
            || arg.starts_with("--work-tree")
            || arg.starts_with("--config-env")
        {
            return false;
        }
    }
    // Verb-aware `branch` pinning: `branch` lists refs by default, but
    // `branch <name>` CREATES a ref and `-d/-D/-m/-M/-c/-C` (+ long forms)
    // mutate/move/copy/delete refs. Deny those ONLY for `branch` — a
    // blanket `-m` denial would break legit `git log -m` / `git show -m`
    // (first-parent/diff-merge display, read-only).
    if sub == "branch" {
        for arg in &args[1..] {
            if arg == "-d"
                || arg == "-D"
                || arg == "-m"
                || arg == "-M"
                || arg == "-c"
                || arg == "-C"
            {
                return false;
            }
            if arg.starts_with("--delete")
                || arg.starts_with("--move")
                || arg.starts_with("--copy")
                || arg.starts_with("--rename")
            {
                return false;
            }
        }
        // Bare `branch <name>` creates a ref: positional (non-flag) args
        // are creation unless an explicit `--list`/`-l` forces list mode
        // (then the positional is a harmless display pattern).
        let list_mode = args[1..].iter().any(|a| a == "--list" || a == "-l");
        if !list_mode && args[1..].iter().any(|a| !a.starts_with('-')) {
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
    fn git_args_deny_c_config_override() {
        // Exact `-c` denied everywhere (config override ` -c key=val`).
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "-c".to_string(),
            "core.pager=evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "-c".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "-c".to_string()
        ]));
        // `--cached` / `--color` must NOT be caught by the `-c` denial.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--cached".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_output_redirection() {
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--output=/tmp/pwned.txt".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--output".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--output=foo".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "show".to_string(),
            "--output".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_ext_diff_driver() {
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--ext-diff".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--ext-diff=foo".to_string()
        ]));
        // Safe default stays allowed.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--no-ext-diff".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_textconv_driver() {
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--textconv".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "show".to_string(),
            "--textconv".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--textconv".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--textconv=foo".to_string()
        ]));
        // Safe default stays allowed.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--no-textconv".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_git_dir_work_tree() {
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--git-dir=/tmp/evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--git-dir".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--work-tree=/tmp/evil".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "diff".to_string(),
            "--work-tree".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_config_env() {
        assert!(!is_allowed_git_args(&[
            "log".to_string(),
            "--config-env=foo=BAR".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "status".to_string(),
            "--config-env".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_branch_mutating_shorts() {
        for flag in ["-d", "-D", "-m", "-M", "-c", "-C"] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
        }
        // Same shorts stay allowed for non-branch verbs (verb-aware pin).
        assert!(is_allowed_git_args(&["log".to_string(), "-m".to_string()]));
        assert!(is_allowed_git_args(&["show".to_string(), "-m".to_string()]));
    }

    #[test]
    fn git_args_deny_branch_mutating_longs() {
        for flag in ["--delete", "--move", "--copy", "--rename"] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
            assert!(
                !is_allowed_git_args(&["branch".to_string(), format!("{flag}=foo")]),
                "branch {flag}=foo must be denied"
            );
        }
        // Long mutating flags are branch-pinned: other verbs do not take
        // them, but the pin itself must not leak (e.g. `log --delete`
        // passes this gate and fails later as unknown flag, not as a
        // branch-mutation denial — the pin denies only for `branch`).
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "--oneline".to_string()
        ]));
    }

    #[test]
    fn git_args_deny_branch_creation_positional() {
        // Bare `branch <name>` CREATES a ref — deny.
        assert!(!is_allowed_git_args(&[
            "branch".to_string(),
            "MUTANT".to_string()
        ]));
        assert!(!is_allowed_git_args(&[
            "branch".to_string(),
            "-v".to_string(),
            "MUTANT".to_string()
        ]));
        // Explicit list mode turns the positional into a display pattern.
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "--list".to_string(),
            "MUTANT".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "-l".to_string(),
            "MUTANT".to_string()
        ]));
    }

    #[test]
    fn git_args_allow_legit_panel_use_after_hardening() {
        // Plain list + common list flags.
        assert!(is_allowed_git_args(&["branch".to_string()]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "--list".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "-a".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "-v".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "--show-current".to_string()
        ]));
        // Read-only `-m` display flag for log/show.
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "-m".to_string(),
            "--oneline".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "show".to_string(),
            "-m".to_string(),
            "--stat".to_string()
        ]));
        // Normal diff/log/show output.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--cached".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "--oneline".to_string()
        ]));
        assert!(is_allowed_git_args(&["show".to_string()]));
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
