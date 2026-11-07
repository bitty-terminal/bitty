#![forbid(unsafe_code)]
//! Layer-2 system-CLI allowlist enforcement (CTX-0444).
//!
//! Canonical record for the accepted `[tools.git]` slice (v1) is
//! `bitty-plugins-docs`
//! `specifications/plugin-reuse-and-providers.md`
//! (`Accepted [tools.git] contract (v1)`, CTX-0425): seven read-only verbs
//! with bounded output. This module is validation and policy — no process
//! spawning, no VM. The spawn execution surface itself is CTX-0445; this
//! module provides the fail-closed predicates that surface must call.
//!
//! The one exception to "no I/O" is the repository metadata probe
//! ([`plan_git_spawn`] / [`resolve_git_repository`], CTX-1733): before an
//! allowlisted `git` verb runs it reads the authorized tree's `.git` entry and
//! config so a planted gitfile or `core.worktree` cannot point the child at a
//! repository outside the plugin's `fs` grants. It reads only, never writes and
//! never spawns, and takes every root from its caller.
//!
//! Security boundary: allowlist bypass is a sandbox escape. Every predicate
//! fails closed (deny-by-default, explicit `false`, no silent pass).

use std::collections::VecDeque;

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

/// Environment variables never passed to a Layer-2 spawn.
///
/// Three vector classes, all fail-closed:
///
/// - **Pager / diff pagination** (CTX-0465): `PAGER`, `GIT_PAGER`.
/// - **Env-encoded config override** (CTX-0488): `GIT_CONFIG*` names are
///   read as an implicit `-c <key>=<value>` sequence (`GIT_CONFIG_COUNT` +
///   `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`, and the
///   `GIT_CONFIG_PARAMETERS` form git itself exports), re-opening the
///   override vector closed for argv.
/// - **External program execution** (CTX-0488): diff drivers, editors, ssh
///   and askpass helpers, and `GIT_EXEC_PATH` (`git-*` lookup) all execute
///   attacker-chosen programs. `GIT_DIR`/`GIT_WORK_TREE` are the env form of
///   the denied repo-escape argv flags.
pub const DENIED_SPAWN_ENV_VARS: &[&str] = &[
    // Pager program (CTX-0465).
    "PAGER",
    "GIT_PAGER",
    // Env-encoded config override (CTX-0488).
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_GLOBAL",
    // External program execution (CTX-0488).
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_PROXY_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_EXEC_PATH",
    // Repo-identity escape, env form of the denied `--git-dir`/`--work-tree`.
    "GIT_DIR",
    "GIT_WORK_TREE",
    // Repository-metadata escape (CTX-1733): each of these redirects a part of
    // the repository the child reads (git dir, index, object store, discovery
    // ceiling) without touching argv, which would defeat the `.git` metadata
    // policy for a spawn whose cwd is otherwise inside the grant.
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
];

/// Denied environment-variable name prefixes (numbered families).
///
/// `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` are the implicit `-c`
/// override pairs; any numbered suffix is denied.
pub const DENIED_SPAWN_ENV_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Whether an environment variable name is denied for a Layer-2 spawn.
///
/// ASCII case-insensitive so Windows env lookup (`GetEnvironmentVariable` is
/// case-insensitive, and git reads through it) cannot bypass with a folded
/// variant. Near-miss names (`MY_PAGER`, `GIT_TERMINAL_PROMPT`) stay allowed.
fn env_name_is_denied(name: &str) -> bool {
    if DENIED_SPAWN_ENV_VARS
        .iter()
        .any(|denied| name.eq_ignore_ascii_case(denied))
    {
        return true;
    }
    let bytes = name.as_bytes();
    DENIED_SPAWN_ENV_PREFIXES.iter().any(|prefix| {
        bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    })
}

/// Whether explicit spawn `env` is safe for a Layer-2 spawn.
///
/// Fail-closed: any entry naming a denied variable fails, regardless of
/// value — even `GIT_PAGER=cat` or `GIT_CONFIG_COUNT=0` would legitimize the
/// channel. Matching is ASCII case-insensitive with prefix families for the
/// numbered `GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*` names; near-miss names
/// stay allowed. The spawn surface must call this before `env_clear()` +
/// explicit `env()`.
///
/// [`DENIED_SPAWN_ENV_VARS`] / [`DENIED_SPAWN_ENV_PREFIXES`] are the policy
/// lists; this predicate is the enforcement point for the spawn surface
/// (`bitty-runtime` `SpawnRequest`).
#[must_use]
pub fn is_safe_spawn_env(env: &[(String, String)]) -> bool {
    for (name, _) in env {
        if env_name_is_denied(name) {
            return false;
        }
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
/// no config override (`-c` exact and `-c<key>=<val>` glued, `--config` in
/// every form including `--config-env`), no repo escape (`-C` exact and
/// `-C<path>` glued for every verb, `--git-dir` / `--work-tree` prefix), no
/// pager driver (`--paginate` / `--pager` prefix; `--no-pager` stays
/// allowed), no file-write or external-driver flags (`--output` /
/// `--ext-diff` / `--textconv` prefix), and verb-aware `branch` pinning
/// (mutating shorts/longs, bundled clusters, plus bare creation denied;
/// see inline).
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
        // Config override via `-c key=val`: exact `-c` plus glued
        // `-c<key>=<val>` (e.g. `-ccore.pager=evil`). Single-dash only:
        // `--cached` / `--color` (double-dash) must keep working.
        if arg.starts_with("-c") && !arg.starts_with("--") {
            return false;
        }
        // Repo escape via `-C <path>` / `-C<path>` (run in directory):
        // denied for every verb, not just `branch` (whose arm already
        // denies `-C` for its own copy semantics). No panel use needs it.
        if arg.starts_with("-C") && !arg.starts_with("--") {
            return false;
        }
        // Pager driver: `--paginate` / `--pager` force an external pager
        // program (external-process vector). `--no-pager` stays allowed
        // (safe default; it matches neither prefix).
        if arg.starts_with("--paginate") || arg.starts_with("--pager") {
            return false;
        }
        // File-write / external-driver / repo-escape / env-config flags.
        // Prefix denial covers both bare (`--output`) and `=...` forms
        // (`--output=/tmp/pwned.txt`); `--no-ext-diff` / `--no-textconv`
        // stay allowed (safe defaults, `--no-` prefix does not match).
        // `--config` covers `--config`, `--config=<k>=<v>`, and
        // `--config-env` uniformly (config override in every long form).
        if arg.starts_with("--output")
            || arg.starts_with("--ext-diff")
            || arg.starts_with("--textconv")
            || arg.starts_with("--git-dir")
            || arg.starts_with("--work-tree")
            || arg.starts_with("--config")
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
                || arg == "-f"
                || arg == "-u"
                || arg == "-t"
                || arg == "--set-upstream"
            {
                return false;
            }
            if arg.starts_with("--delete")
                || arg.starts_with("--move")
                || arg.starts_with("--copy")
                || arg.starts_with("--rename")
                || arg.starts_with("--force")
                || arg.starts_with("--set-upstream-to")
                || arg.starts_with("--unset-upstream")
                || arg.starts_with("--track")
                || arg.starts_with("--edit-description")
            {
                return false;
            }
            // Bundled single-dash clusters: `-av` means `-a -v`.
            // Deny any single-dash cluster containing a mutating short
            // (`d`/`D`/`m`/`M`/`c`/`C`/`f`) plus `-u` glued (`-u<value>`,
            // e.g. `-uorigin/main`, writes repo config args-alone) and
            // `-t` (boolean track; `-at` would smuggle `-t` past exact).
            // `--no-track` stays allowed: double-dash, never reaches here.
            // Allowed list shorts (`-a`/`-v`/`-l`) contain none of these
            // letters, so `-av`/`-vv` stay allowed (asserted in tests).
            if arg.starts_with('-') && !arg.starts_with("--") && arg.len() >= 2 {
                let cluster = &arg[1..];
                if cluster.contains('d')
                    || cluster.contains('D')
                    || cluster.contains('m')
                    || cluster.contains('M')
                    || cluster.contains('c')
                    || cluster.contains('C')
                    || cluster.contains('f')
                    || cluster.contains('u')
                    || cluster.contains('t')
                {
                    return false;
                }
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

// ── repository metadata policy (CTX-1733, issue #2912) ────────────────────
//
// The argv denials (`-C`, `--git-dir`, `--work-tree`) close argv-selected
// directories, but repository *metadata* is a separate channel: a plugin able
// to write the pane tree can plant a `.git` gitfile or set `core.worktree` in
// `.git/config`, and `git status`/`git show`/`git diff` then read a repository
// outside the plugin's `fs` grants. [`plan_git_spawn`] is the enforcement
// point: before any allowlisted verb runs it resolves the repository the child
// *would* use from the authorized cwd and refuses with a typed
// [`GitMetadataRefusal`] when that resolution escapes the grant.
//
// Resolution rules (fail-closed, git-compatible where it matters):
//
// - discovery walks the authorized cwd's ancestors, first `.git` wins, exactly
//   like git's own discovery — so a repository nested *above* the cwd is
//   found and policed instead of silently read;
// - a `.git` directory is the git directory, and its `core.worktree` (if any)
//   is the work tree;
// - a `.git` *file* (`gitdir: <path>`) and a `.git` symlink both name a git
//   directory elsewhere, so that target is what must stay inside the grant;
//   its work tree is the directory holding the gitfile unless `core.worktree`
//   says otherwise;
// - every resolved path (repo root, git directory, work tree, include target)
//   must be inside the authorized cwd or one of the caller's grant roots;
// - `.git/config` directives the probe will not follow (`include` /
//   `includeIf`, which can import a config that sets `core.worktree`) are
//   refused rather than read: the pane repository's config is
//   plugin-writable, so following its includes would re-open the channel;
// - the same check runs on `.git/config.worktree` (the file
//   `extensions.worktreeConfig` adds), on `.git/commondir`, and on
//   `objects/info/alternates`, because each of those redirects part of the
//   repository the child reads; `objects/info/http-alternates` is refused
//   outright, since its entries are remote URLs no containment test can hold
//   inside the grant.
//
// This is the one place in this otherwise pure module that reads the
// filesystem. It never writes and never spawns; every root comes from the
// caller (host-resolved from the plugin's manifest/effective grants), so no
// host path is hardcoded.

/// Environment variable that carries git's optional-lock switch.
pub const GIT_OPTIONAL_LOCKS_VAR: &str = "GIT_OPTIONAL_LOCKS";

/// Value that makes read-only verbs skip optional locks.
///
/// `git status` otherwise refreshes and writes `.git/index`, i.e. a read-only
/// verb mutates plugin-writable repository metadata, and it can also fail on a
/// read-only tree. Every allowlisted verb is read-only, so the mediated spawn
/// sets this unconditionally.
pub const GIT_OPTIONAL_LOCKS_DISABLED: &str = "0";

/// Read-only child environment for an allowlisted `git` verb (CTX-1733).
///
/// The spawn surface clears the environment and passes exactly these pairs:
/// [`is_safe_spawn_env`] stays true for them (they are not denied names), and
/// they carry no secret value.
pub const GIT_READ_ONLY_SPAWN_ENV: &[(&str, &str)] =
    &[(GIT_OPTIONAL_LOCKS_VAR, GIT_OPTIONAL_LOCKS_DISABLED)];

/// Owned form of [`GIT_READ_ONLY_SPAWN_ENV`] for a spawn request.
#[must_use]
pub fn git_read_only_spawn_env() -> Vec<(String, String)> {
    GIT_READ_ONLY_SPAWN_ENV
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

/// Maximum ancestor hops walked while discovering the repository root.
///
/// A bound keeps a hostile deep tree from turning the probe into unbounded
/// work; exceeding it fails closed rather than guessing.
pub const MAX_GIT_DISCOVERY_DEPTH: usize = 64;

/// Maximum bytes read from a `.git` gitfile.
pub const MAX_GITFILE_BYTES: u64 = 4 * 1024;

/// Maximum bytes read from a repository-local `.git/config`.
pub const MAX_GIT_CONFIG_BYTES: u64 = 64 * 1024;

/// Why an allowlisted `git` spawn was refused by the metadata policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitMetadataRefusalKind {
    /// The authorized cwd is not an absolute path, so no repository can be
    /// proved to be inside a grant.
    AuthorizedCwdNotAbsolute,
    /// The authorized cwd, a gitfile, or a `config` exists but cannot be
    /// read/parsed, so the repository the child would use cannot be proven
    /// safe. Fail-closed: an unprovable repository is refused.
    MetadataUnreadable,
    /// A `.git` gitfile (or `.git` symlink) names a git directory outside the
    /// grant.
    GitDirOutsideGrant,
    /// `.git/config` sets `core.worktree` (with or without `core.bare`) to a
    /// directory outside the grant.
    WorktreeOutsideGrant,
    /// A repository discovered above the authorized cwd has its root outside
    /// the grant.
    ParentRepoOutsideGrant,
    /// The object store is redirected outside the grant (`objects/info/alternates`,
    /// `objects/info/http-alternates`), so `log`/`show`/`diff` would read
    /// objects that live elsewhere.
    ObjectStoreOutsideGrant,
    /// `.git/config` carries an `include`/`includeIf` directive, a config
    /// channel the probe will not follow.
    ConfigIncludeRefused,
    /// The authorized cwd is nested deeper than [`MAX_GIT_DISCOVERY_DEPTH`], so
    /// the repository git would discover cannot be proven to be inside the
    /// grant.
    DiscoveryBoundExceeded,
}

impl GitMetadataRefusalKind {
    /// Stable label, used verbatim in audit detail.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizedCwdNotAbsolute => "authorized cwd is not absolute",
            Self::MetadataUnreadable => "repository metadata unreadable",
            Self::GitDirOutsideGrant => "git directory outside the grant",
            Self::WorktreeOutsideGrant => "core.worktree outside the grant",
            Self::ParentRepoOutsideGrant => "repository root outside the grant",
            Self::ObjectStoreOutsideGrant => "object store outside the grant",
            Self::ConfigIncludeRefused => "config include refused",
            Self::DiscoveryBoundExceeded => "repository discovery bound exceeded",
        }
    }
}

impl std::fmt::Display for GitMetadataRefusalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed refusal of a `git` spawn whose repository metadata escapes the grant.
///
/// Typed, never string-matched: callers branch on [`Self::kind`] and keep
/// [`Self::path`] (the refused path, already sanitised for audit) for the
/// record. Both are host-authored — the path is derived from the authorized
/// cwd and the grants, never from plugin bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMetadataRefusal {
    kind: GitMetadataRefusalKind,
    path: String,
}

impl GitMetadataRefusal {
    /// Build a refusal, sanitising and clamping the refused path for audit.
    #[must_use]
    pub fn new(kind: GitMetadataRefusalKind, path: &std::path::Path) -> Self {
        let rendered = sanitize_audit_path(&path.display().to_string());
        Self {
            kind,
            path: clamp_text(&rendered, MAX_TOOL_SPAWN_CWD_BYTES),
        }
    }

    /// Which metadata channel was refused.
    #[must_use]
    pub const fn kind(&self) -> GitMetadataRefusalKind {
        self.kind
    }

    /// The refused path, sanitised for audit (no `Cf`, no `Cc`).
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Bounded audit detail naming the refused channel and path.
    #[must_use]
    pub fn audit_detail(&self) -> String {
        format!(
            "git repository metadata refused ({}): '{}'",
            self.kind.as_str(),
            self.path
        )
    }
}

impl std::fmt::Display for GitMetadataRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.audit_detail())
    }
}

/// Typed refusal of the whole pre-spawn plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSpawnRefusal {
    /// The argv is not an allowlisted `[tools.git]` invocation
    /// ([`is_allowed_git_args`]).
    NotAllowlisted,
    /// The argv is allowlisted but the repository metadata the child would
    /// read is not inside the grant.
    Metadata(GitMetadataRefusal),
}

impl GitSpawnRefusal {
    /// Audit detail naming the refused path for the metadata case.
    #[must_use]
    pub fn audit_detail(&self) -> String {
        match self {
            Self::NotAllowlisted => "git spawn refused: argv not allowlisted".to_string(),
            Self::Metadata(refusal) => refusal.audit_detail(),
        }
    }

    /// The refused path, for the metadata case.
    #[must_use]
    pub fn refused_path(&self) -> Option<&str> {
        match self {
            Self::NotAllowlisted => None,
            Self::Metadata(refusal) => Some(refusal.path()),
        }
    }
}

impl std::fmt::Display for GitSpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.audit_detail())
    }
}

/// The repository the child would use, all paths inside the grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepoBinding {
    /// Repository root (the directory holding the `.git` entry git would use).
    pub repo_root: std::path::PathBuf,
    /// Git directory (`<root>/.git` or a gitfile/symlink target).
    pub git_dir: std::path::PathBuf,
    /// `core.worktree`, when the config selects a work tree other than the
    /// repository root. Absent means the root itself is the work tree.
    pub work_tree: Option<std::path::PathBuf>,
    /// `core.bare`: a bare repository has no work tree, so work-tree verbs
    /// fail in git itself rather than reading a tree.
    pub bare: bool,
}

/// A fully gated `git` spawn: authorized cwd, unchanged allowlisted argv, the
/// read-only child environment, and the resolved repository (if any).
///
/// [`Self::cwd`] is the **authorized** directory: the canonical path the host
/// permitted for this child. It is not proof of the child's actual cwd — a
/// path swapped between authorization and `spawn` is not detected here — and
/// it must be recorded as the authorized value, never as evidence of where
/// the process ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpawnPlan {
    /// Authorized working directory (see the type docs: authorized, not
    /// observed).
    pub cwd: std::path::PathBuf,
    /// The allowlisted argv, unchanged.
    pub args: Vec<String>,
    /// Closed child environment for a read-only verb
    /// ([`GIT_READ_ONLY_SPAWN_ENV`]).
    pub env: Vec<(String, String)>,
    /// Resolved repository, or `None` when no `.git` exists in the cwd or any
    /// ancestor (git then fails on its own; no metadata is read outside the
    /// grant).
    pub repo: Option<GitRepoBinding>,
}

/// Plan one allowlisted `git` spawn under the metadata policy (CTX-1733).
///
/// Two gates, in order: [`is_allowed_git_args`] (verb/flag allowlist, unchanged
/// by this function) and [`resolve_git_repository`] (the `.git` metadata
/// channel). A refusal is a typed [`GitSpawnRefusal`] and the spawn must not
/// happen; the allowed result carries the read-only env so the surface cannot
/// forget `GIT_OPTIONAL_LOCKS=0`.
///
/// `authorized_cwd` is the host-resolved pane directory; `grant_roots` are the
/// host-resolved absolute roots the plugin may read (manifest/effective `fs`
/// grants). Both are caller-supplied: this function hardcodes no host path and
/// derives nothing from the plugin. The child is assumed to run under
/// `env_clear()` plus [`GIT_READ_ONLY_SPAWN_ENV`], so `GIT_DIR`,
/// `GIT_WORK_TREE`, and the other denied names cannot redirect discovery.
///
/// # Errors
///
/// [`GitSpawnRefusal::NotAllowlisted`] for argv outside the accepted contract,
/// [`GitSpawnRefusal::Metadata`] when the repository the child would use is not
/// provably inside the grant.
pub fn plan_git_spawn(
    args: &[String],
    authorized_cwd: &std::path::Path,
    grant_roots: &[std::path::PathBuf],
) -> Result<GitSpawnPlan, GitSpawnRefusal> {
    if !is_tool_spawn_allowed(ACCEPTED_TOOL_GIT, args) {
        return Err(GitSpawnRefusal::NotAllowlisted);
    }
    let repo =
        resolve_git_repository(authorized_cwd, grant_roots).map_err(GitSpawnRefusal::Metadata)?;
    Ok(GitSpawnPlan {
        cwd: authorized_cwd.to_path_buf(),
        args: args.to_vec(),
        env: git_read_only_spawn_env(),
        repo,
    })
}

/// Resolve the repository the child would use from `authorized_cwd`, failing
/// closed when that resolution leaves the authorized cwd or the grants.
///
/// # Errors
///
/// A [`GitMetadataRefusal`] naming the escaping channel and path.
pub fn resolve_git_repository(
    authorized_cwd: &std::path::Path,
    grant_roots: &[std::path::PathBuf],
) -> Result<Option<GitRepoBinding>, GitMetadataRefusal> {
    if authorized_cwd.as_os_str().is_empty() || !authorized_cwd.is_absolute() {
        return Err(GitMetadataRefusal::new(
            GitMetadataRefusalKind::AuthorizedCwdNotAbsolute,
            authorized_cwd,
        ));
    }
    for (hop, dir) in authorized_cwd.ancestors().enumerate() {
        if hop >= MAX_GIT_DISCOVERY_DEPTH {
            // Fail closed at the bound: beyond it, an ancestor repository
            // could exist unproven.
            return Err(GitMetadataRefusal::new(
                GitMetadataRefusalKind::DiscoveryBoundExceeded,
                authorized_cwd,
            ));
        }
        let meta_path = dir.join(".git");
        let meta = match std::fs::symlink_metadata(&meta_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                return Err(GitMetadataRefusal::new(
                    GitMetadataRefusalKind::MetadataUnreadable,
                    &meta_path,
                ));
            }
        };
        let is_gitfile = !meta.is_dir();
        let git_dir = if is_gitfile {
            resolve_gitfile_target(&meta_path, meta.file_type().is_symlink())?
        } else {
            meta_path.clone()
        };
        // Git's own discovery: the repository root is the directory holding
        // the `.git` entry. The authorized cwd is inside the grant by
        // construction; an *ancestor* root has to prove it is too, or a
        // repository above the pane would be read on the plugin's behalf.
        if dir != authorized_cwd && !path_within_grant(authorized_cwd, grant_roots, dir) {
            return Err(GitMetadataRefusal::new(
                GitMetadataRefusalKind::ParentRepoOutsideGrant,
                dir,
            ));
        }
        if !path_within_grant(authorized_cwd, grant_roots, &git_dir) {
            return Err(GitMetadataRefusal::new(
                GitMetadataRefusalKind::GitDirOutsideGrant,
                &git_dir,
            ));
        }
        // Both config files git may read: the repository config and, when
        // `extensions.worktreeConfig` is on, `config.worktree`. `core.worktree`
        // in either one selects the tree the child reads, so both are checked.
        let config = read_core_config(&git_dir.join("config"))?;
        let worktree_config = read_core_config(&git_dir.join("config.worktree"))?;
        for config in [&config, &worktree_config] {
            if let Some(include) = config.include.as_deref() {
                return Err(GitMetadataRefusal::new(
                    GitMetadataRefusalKind::ConfigIncludeRefused,
                    std::path::Path::new(include),
                ));
            }
        }
        let mut work_tree = None;
        for config in [&config, &worktree_config] {
            if let Some(raw) = config.worktree.as_deref() {
                let resolved = resolve_worktree(&git_dir, raw);
                if !path_within_grant(authorized_cwd, grant_roots, &resolved) {
                    return Err(GitMetadataRefusal::new(
                        GitMetadataRefusalKind::WorktreeOutsideGrant,
                        &resolved,
                    ));
                }
                // `config.worktree` wins over `config`, matching git.
                work_tree = Some(resolved);
            }
        }
        // Metadata pointer files: git follows them to the real git dir or
        // object store, so their targets are exactly as sensitive as a
        // gitfile target.
        check_metadata_pointer(
            &git_dir.join("commondir"),
            &git_dir,
            authorized_cwd,
            grant_roots,
            GitMetadataRefusalKind::GitDirOutsideGrant,
        )?;
        check_metadata_pointer(
            &git_dir.join("objects").join("info").join("alternates"),
            &git_dir.join("objects"),
            authorized_cwd,
            grant_roots,
            GitMetadataRefusalKind::ObjectStoreOutsideGrant,
        )?;
        reject_http_alternates(&git_dir)?;
        return Ok(Some(GitRepoBinding {
            repo_root: dir.to_path_buf(),
            git_dir,
            work_tree,
            bare: config.bare || worktree_config.bare,
        }));
    }
    Ok(None)
}

/// Resolve a `core.worktree` value the way git does: relative values are taken
/// from the git directory, absolute values as given.
fn resolve_worktree(git_dir: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let candidate = std::path::Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        git_dir.join(candidate)
    }
}

/// Refuse a metadata pointer file (`commondir`, `objects/info/alternates`)
/// whose targets leave the grant.
///
/// Missing is legitimate; unreadable fails closed. `alternates` is one path per
/// line, resolved (per git) against `base`; `commondir` is a single path.
fn check_metadata_pointer(
    pointer: &std::path::Path,
    base: &std::path::Path,
    authorized_cwd: &std::path::Path,
    grant_roots: &[std::path::PathBuf],
    kind: GitMetadataRefusalKind,
) -> Result<(), GitMetadataRefusal> {
    let text = match read_bounded_text(pointer, MAX_GIT_CONFIG_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(()),
        Err(()) => {
            return Err(GitMetadataRefusal::new(
                GitMetadataRefusalKind::MetadataUnreadable,
                pointer,
            ));
        }
    };
    for line in text.lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(entry);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base.join(candidate)
        };
        if !path_within_grant(authorized_cwd, grant_roots, &resolved) {
            return Err(GitMetadataRefusal::new(kind, &resolved));
        }
    }
    Ok(())
}

/// Refuse `objects/info/http-alternates`.
///
/// The entries are URLs, not filesystem paths, so there is no containment test
/// that could keep them inside the grant: a repository-local file that names a
/// remote object store is refused outright.
fn reject_http_alternates(git_dir: &std::path::Path) -> Result<(), GitMetadataRefusal> {
    let path = git_dir.join("objects").join("info").join("http-alternates");
    match read_bounded_text(&path, MAX_GIT_CONFIG_BYTES) {
        Ok(None) => Ok(()),
        Ok(Some(text)) => {
            if text.lines().all(|line| line.trim().is_empty()) {
                Ok(())
            } else {
                Err(GitMetadataRefusal::new(
                    GitMetadataRefusalKind::ObjectStoreOutsideGrant,
                    &path,
                ))
            }
        }
        Err(()) => Err(GitMetadataRefusal::new(
            GitMetadataRefusalKind::MetadataUnreadable,
            &path,
        )),
    }
}

/// Resolve the git directory named by a `.git` gitfile or `.git` symlink.
///
/// Both channels move the git directory away from the pane tree, which is the
/// escape this exists to police; the target is resolved relative to the entry
/// that named it, per git's gitfile contract.
fn resolve_gitfile_target(
    meta_path: &std::path::Path,
    is_symlink: bool,
) -> Result<std::path::PathBuf, GitMetadataRefusal> {
    let unreadable =
        || GitMetadataRefusal::new(GitMetadataRefusalKind::MetadataUnreadable, meta_path);
    let base = meta_path
        .parent()
        .map_or_else(|| meta_path.to_path_buf(), std::path::Path::to_path_buf);
    let raw = if is_symlink {
        std::fs::read_link(meta_path).map_err(|_| unreadable())?
    } else {
        let text = match read_bounded_text(meta_path, MAX_GITFILE_BYTES) {
            Ok(Some(text)) => text,
            _ => return Err(unreadable()),
        };
        let trimmed = text.trim();
        let rest = trimmed.strip_prefix("gitdir:").ok_or_else(unreadable)?;
        std::path::PathBuf::from(rest.trim())
    };
    if raw.as_os_str().is_empty() {
        return Err(unreadable());
    }
    if raw.is_absolute() {
        Ok(raw)
    } else {
        Ok(base.join(raw))
    }
}

/// Parsed `core.*` facts plus any config-include directive.
#[derive(Debug, Default)]
struct CoreConfig {
    worktree: Option<String>,
    bare: bool,
    include: Option<String>,
}

/// Read and parse `<git_dir>/config`, if it exists.
///
/// A missing config is legitimate (a repository works without one); anything
/// that exists but cannot be read, is not a regular file (a symlinked config is
/// refused rather than followed), or is over [`MAX_GIT_CONFIG_BYTES`] fails
/// closed.
fn read_core_config(path: &std::path::Path) -> Result<CoreConfig, GitMetadataRefusal> {
    match read_bounded_text(path, MAX_GIT_CONFIG_BYTES) {
        Ok(Some(text)) => Ok(parse_core_config(&text)),
        Ok(None) => Ok(CoreConfig::default()),
        Err(()) => Err(GitMetadataRefusal::new(
            GitMetadataRefusalKind::MetadataUnreadable,
            path,
        )),
    }
}

/// Read at most `limit` bytes of regular-file text.
///
/// `Ok(None)` means the file does not exist; `Err(())` means it exists but
/// cannot be used (non-regular file, unreadable, over the limit, or not UTF-8).
fn read_bounded_text(path: &std::path::Path, limit: u64) -> Result<Option<String>, ()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !meta.is_file() || meta.len() > limit {
        return Err(());
    }
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| ()),
        Err(_) => Err(()),
    }
}

/// Minimal git-config reader for the metadata policy.
///
/// Deliberately narrow: section headers, `key = value` pairs, git's comment
/// rule (a `#`/`;` at line start or after whitespace ends the value), and
/// double-quoted values with the standard escapes. It extracts only what the
/// policy needs — `core.worktree`, `core.bare`, and the presence of an
/// `include`/`includeIf` path — and never writes.
fn parse_core_config(text: &str) -> CoreConfig {
    let mut config = CoreConfig::default();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .split([']', ' ', '\t'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            section = name;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = parse_config_value(value);
        match section.as_str() {
            "core" => match key.as_str() {
                "worktree" => config.worktree = Some(value),
                "bare" => {
                    config.bare = matches!(value.as_str(), "true" | "yes" | "on" | "1");
                }
                _ => {}
            },
            // `include` and `includeif` both import another config file,
            // which can carry `core.worktree`; the probe refuses to follow
            // them rather than parse a second, plugin-reachable file.
            section
                if (section == "include" || section.starts_with("includeif"))
                    && key == "path"
                    && config.include.is_none() =>
            {
                config.include = Some(value);
            }
            _ => {}
        }
    }
    config
}

/// Parse one config value: trim, cut a trailing comment, unquote.
fn parse_config_value(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut quoted = false;
    let mut escaped = false;
    let mut end = trimmed.len();
    for (index, ch) in trimmed.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' | ';' if !quoted => {
                let preceded_by_space = index > 0
                    && trimmed[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace);
                if preceded_by_space {
                    end = index;
                    break;
                }
            }
            _ => {}
        }
    }
    let value = trimmed[..end].trim();
    let value = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Whether `candidate` is inside `cwd` or one of the grant roots.
///
/// Lexical normalisation plus canonicalisation when the path exists, so a
/// symlink cannot present as an in-grant path while resolving elsewhere.
fn path_within_grant(
    authorized_cwd: &std::path::Path,
    grant_roots: &[std::path::PathBuf],
    candidate: &std::path::Path,
) -> bool {
    let candidate = resolve_for_compare(candidate);
    if candidate.starts_with(resolve_for_compare(authorized_cwd)) {
        return true;
    }
    grant_roots
        .iter()
        .any(|root| candidate.starts_with(resolve_for_compare(root)))
}

/// Compare-ready form of a path: lexical normalisation, then canonicalisation
/// when possible.
fn resolve_for_compare(path: &std::path::Path) -> std::path::PathBuf {
    let lexical = normalize_lexically(path);
    std::fs::canonicalize(&lexical).unwrap_or(lexical)
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// `..` at a filesystem root is the root itself, matching path resolution.
fn normalize_lexically(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `c` is a Unicode `Format` (`Cf`) code point.
///
/// Curated `Cf` table: this crate takes no Unicode-tables dependency, and every
/// range below is `Cf` (Unicode 15) or a deprecated format block a renderer may
/// still honour. Bidi overrides (`U+202A..=U+202E`), the zero-width family
/// (`U+200B..=U+200F`), and invisible tag characters are the audit-spoofing
/// classes this exists to remove.
#[must_use]
pub fn is_audit_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{E0001}'
            | '\u{FEFF}'
            | '\u{0600}'..='\u{0605}'
            | '\u{0890}'..='\u{0891}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// Make text about to be recorded in an audit record bidi-safe.
///
/// A recorded directory name can be plugin-influenced. A right-to-left override
/// (`U+202E`) makes the record read as a different path than the one acted on,
/// and a zero-width joiner (`U+200D`) can hide a segment boundary; both are
/// `Cf`, so they are **removed** rather than escaped. `Cc` characters
/// (newlines included) become `?` so a recorded name cannot forge a log line.
#[must_use]
pub fn sanitize_audit_path(text: &str) -> String {
    text.chars()
        .filter_map(|c| {
            if is_audit_format_char(c) {
                None
            } else if c.is_control() {
                Some('?')
            } else {
                Some(c)
            }
        })
        .collect()
}

// ── audit ledger (CTX-1152, issue #1985) ──────────────────────────────────

/// Maximum tool-spawn audit entries retained ([`ToolSpawnLedger`], drop-oldest).
///
/// Precedent: [`crate::fs_authz::MAX_FS_AUDIT_ENTRIES`] (`1024`) and
/// [`crate::secrets::MAX_SECRET_AUDIT_ENTRIES`] (`1024`).
pub const MAX_TOOL_SPAWN_AUDIT_ENTRIES: usize = 1024;

/// Maximum args named in one audit entry's arg list (the remainder is
/// counted in [`ToolSpawnEntry::args_omitted`], never silently dropped).
///
/// Precedent: [`crate::fs_authz::MAX_FS_AUDIT_ITEMS`] (`8`) and
/// [`crate::secrets::MAX_SECRET_AUDIT_ITEMS`] (`8`).
pub const MAX_TOOL_SPAWN_AUDIT_ARGS: usize = 8;

/// Maximum bytes stored per arg preview in one audit entry.
///
/// Gate-bounded args already reach [`MAX_GIT_ARG_BYTES`] (`256`) each; the
/// ledger keeps a shorter preview so one entry cannot bank kilobytes of
/// caller-chosen text for later read-back (bulk exfiltration via audit).
/// Truncation is char-boundary safe. Over-bound args still authorize on
/// their full text — only the recorded preview is shortened.
pub const MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES: usize = 64;

/// Maximum bytes of the caller identity stored in one audit entry.
///
/// Precedent: [`crate::identity::MAX_ONTOLOGY_ID_BYTES`] (`128`, the plugin
/// and client identity bound). Over-bound callers still authorize — only
/// the recorded identity is clamped.
pub const MAX_TOOL_SPAWN_CALLER_BYTES: usize = 128;

/// Maximum bytes of a host-resolved working directory stored in one audit
/// entry (CTX-1715).
///
/// Matches the `bitty-ipc` execution cwd bound (`4096` bytes). Only the host
/// ever supplies this value; a plugin can never reach the field with a path.
pub const MAX_TOOL_SPAWN_CWD_BYTES: usize = 4096;

/// Allow/deny outcome recorded in the [`ToolSpawnLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolSpawnDecision {
    /// The spawn is allowlisted.
    Allow,
    /// The spawn is denied (unknown tool, invalid name, or args outside
    /// the allowlist).
    Deny,
}

impl ToolSpawnDecision {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl std::fmt::Display for ToolSpawnDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One tool-spawn audit entry: who asked, what was decided, and why.
///
/// Tool name, caller identity, bounded arg previews, and (CTX-1715) the
/// host-resolved working directory only. Environment is never accepted by
/// this API: secret values travel via env and `secret://` handles
/// ([`crate::secret_tiers`] discipline), so env must never enter logs or
/// audit. `secret://` handle references are names, never values, and record
/// like any other arg. Bounded and sequence-numbered (no wall-clock),
/// mirroring [`crate::fs_authz::FsAuditLedger`] and
/// [`crate::secrets::SecretAuditLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpawnEntry {
    /// Monotonic sequence (no wall-clock).
    pub seq: u64,
    /// Caller identity that requested the spawn (clamped to
    /// [`MAX_TOOL_SPAWN_CALLER_BYTES`]).
    pub caller: String,
    /// Tool name as presented (clamped to [`TOOL_NAME_MAX_LEN`]).
    pub tool: String,
    /// Outcome.
    pub decision: ToolSpawnDecision,
    /// Arg previews involved (capped at [`MAX_TOOL_SPAWN_AUDIT_ARGS`], each
    /// truncated to [`MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES`]).
    pub args: Vec<String>,
    /// Args beyond the per-entry cap (counted, never silently dropped).
    pub args_omitted: usize,
    /// Host-resolved working directory for this spawn, when the host resolved
    /// one (CTX-1715). Never plugin-supplied: the value is the canonical
    /// **authorized** path — the directory the host permitted for the child —
    /// recorded for attribution. It is *not* proof of the child's actual cwd
    /// (a path swapped between authorization and spawn is not detected here),
    /// and it must never be read as evidence of where the process ran.
    /// `Cf`/`Cc` characters are stripped/replaced before it is recorded
    /// (CTX-1733, [`sanitize_audit_path`]). `None` means the host default
    /// (no child cwd) or a decision that preceded resolution.
    pub cwd: Option<String>,
    /// Bounded host-authored detail (tool/caller/counts/cwd only, never env).
    pub detail: String,
}

/// Bounded append-only ledger of tool-spawn authorization decisions.
///
/// Drop-oldest when full (accepted v1 default, mirroring the FS and secret
/// audit ledgers); `dropped` counts evicted entries for `bitty plugin doctor`.
#[derive(Debug, Clone, Default)]
pub struct ToolSpawnLedger {
    entries: VecDeque<ToolSpawnEntry>,
    dropped: u64,
    next_seq: u64,
}

impl ToolSpawnLedger {
    /// Empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one evaluated spawn decision (exactly one entry per call).
    ///
    /// CodeQL models any call to these `push_*` entry points as
    /// log-tainted on the grounds that the ledger can hold caller bytes;
    /// the payload is attribution-only by construction (tool name, caller
    /// identity, and truncated arg previews — env is not a parameter, so
    /// it cannot enter; see the `tool_ledger_never_records_env` test),
    /// which is the documented barrier the analyzer cannot see. Treat new
    /// `push_*` call sites as log-tainted until they carry the same
    /// attribution-only proof.
    ///
    /// # Logging
    ///
    /// Log-tainted: arguments must be attribution (tool/args/caller),
    /// never environment values.
    pub fn push_decision(&mut self, tool: &str, args: &[String], caller: &str, allowed: bool) {
        self.push_decision_with_cwd(tool, args, caller, allowed, None);
    }

    /// Record one evaluated spawn decision with the host-resolved working
    /// directory (CTX-1715).
    ///
    /// `cwd` is host-derived by construction: it names the canonical
    /// directory the host authorized for the child (or `None` for the host
    /// default / a decision that preceded resolution). It is never a
    /// plugin-supplied path: the plugin selects only the `pane` semantic and
    /// the host resolves it. The recorded copy is clamped to
    /// [`MAX_TOOL_SPAWN_CWD_BYTES`] and sanitised by [`sanitize_audit_path`]
    /// (CTX-1733); neither affects authorization, and the value stays the
    /// *authorized* path, never proof of the child's actual cwd.
    ///
    /// # Logging
    ///
    /// Log-tainted: arguments must be attribution (tool/args/caller/cwd),
    /// never environment values.
    pub fn push_decision_with_cwd(
        &mut self,
        tool: &str,
        args: &[String],
        caller: &str,
        allowed: bool,
        cwd: Option<&str>,
    ) {
        let decision = if allowed {
            ToolSpawnDecision::Allow
        } else {
            ToolSpawnDecision::Deny
        };
        self.push(decision, tool, args, caller, cwd, None);
    }

    /// Record a `git` spawn refused by the repository metadata policy
    /// (CTX-1733, issue #2912).
    ///
    /// Exactly one `Deny` entry, with the refused channel and path in the
    /// detail — the path comes from [`GitMetadataRefusal`], which sanitises it
    /// for audit, so a plugin-influenced directory name cannot spoof the
    /// record with bidi overrides or zero-width characters.
    ///
    /// # Logging
    ///
    /// Log-tainted: arguments must be attribution (args/caller/cwd/refusal),
    /// never environment values.
    pub fn push_git_metadata_refusal(
        &mut self,
        args: &[String],
        caller: &str,
        authorized_cwd: Option<&str>,
        refusal: &GitMetadataRefusal,
    ) {
        self.push(
            ToolSpawnDecision::Deny,
            ACCEPTED_TOOL_GIT,
            args,
            caller,
            authorized_cwd,
            Some(&refusal.audit_detail()),
        );
    }

    /// Bounded append shared by both outcomes.
    fn push(
        &mut self,
        decision: ToolSpawnDecision,
        tool: &str,
        args: &[String],
        caller: &str,
        cwd: Option<&str>,
        extra_detail: Option<&str>,
    ) {
        if self.entries.len() >= MAX_TOOL_SPAWN_AUDIT_ENTRIES {
            self.entries.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let kept: Vec<String> = args
            .iter()
            .take(MAX_TOOL_SPAWN_AUDIT_ARGS)
            .map(|arg| clamp_text(&sanitize_audit_path(arg), MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES))
            .collect();
        let args_omitted = args.len().saturating_sub(kept.len());
        let tool = clamp_text(tool, TOOL_NAME_MAX_LEN);
        let caller = clamp_text(caller, MAX_TOOL_SPAWN_CALLER_BYTES);
        let cwd = cwd.map(|path| clamp_text(&sanitize_audit_path(path), MAX_TOOL_SPAWN_CWD_BYTES));
        let cwd_detail = match cwd.as_deref() {
            Some(path) => format!(", cwd '{path}'"),
            None => String::new(),
        };
        let extra_detail = match extra_detail {
            Some(extra) => format!("; {extra}"),
            None => String::new(),
        };
        let detail = format!(
            "tool spawn {decision} '{tool}' by '{caller}' ({} args, {args_omitted} omitted{cwd_detail}){extra_detail}",
            kept.len(),
        );
        self.entries.push_back(ToolSpawnEntry {
            seq,
            caller,
            tool,
            decision,
            args: kept,
            args_omitted,
            cwd,
            detail,
        });
    }

    /// Retained entries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &ToolSpawnEntry> + '_ {
        self.entries.iter()
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted by the bound so far.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Clamp `text` to `limit` bytes on a char boundary.
///
/// Over-bound input still authorizes on its full text — only the recorded
/// copy is shortened, so clamping never widens or narrows the gate.
fn clamp_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Authorize one tool spawn and record the outcome exactly once.
///
/// This is the attributed spawn decision path for `[tools.*]` (CTX-1152):
/// it evaluates [`is_tool_spawn_allowed`] once and appends exactly one
/// ledger entry (allow or deny). Attributed spawn callers must enter here —
/// or via [`crate::host::PluginHost::authorize_tool_spawn`], which delegates
/// here — rather than calling [`is_tool_spawn_allowed`] directly, so no
/// attributed decision escapes the ledger. The live runtime dispatch
/// (the `bitty-runtime` spawn service, CTX-1248/issue #2068) enforces the
/// same predicates and records its own `ToolSpawnLedger` per mediated
/// spawn; this helper records the host-side decisions. Takes no env:
/// environment values never enter the ledger ([`crate::secret_tiers`]
/// discipline).
#[must_use]
pub fn check_tool_spawn(
    ledger: &mut ToolSpawnLedger,
    tool: &str,
    args: &[String],
    caller: &str,
) -> bool {
    let allowed = is_tool_spawn_allowed(tool, args);
    ledger.push_decision(tool, args, caller, allowed);
    allowed
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
    fn git_args_deny_branch_force_exact_and_long() {
        // Exact `-f` (bare + with positional hardening: positional backstop
        // would also catch `branch -f <name>`, explicit deny is hardening).
        for args in [
            vec!["branch", "-f"],
            vec!["branch", "-f", "newbranch"],
            vec!["branch", "-f=foo"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Prefix `--force` (bare / `=` / separate-value forms).
        for args in [
            vec!["branch", "--force"],
            vec!["branch", "--force=foo"],
            vec!["branch", "--force", "newbranch"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Bundled: `-af` / `-vf` smuggle `-f` past exact.
        for flag in ["-af", "-vf", "-fa"] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
        }
    }

    #[test]
    fn git_args_deny_branch_upstream() {
        // Exact `-u` + glued `-u<value>` (writes repo config args-alone).
        for args in [
            vec!["branch", "-u"],
            vec!["branch", "-u", "origin/main"],
            vec!["branch", "-uorigin/main"],
            vec!["branch", "-u=foo"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Prefix `--set-upstream-to` (bare / `=` / separate value) plus the
        // deprecated exact `--set-upstream` (fatal on modern git, denied
        // here as defense-in-depth for older toolchains; same config write).
        for args in [
            vec!["branch", "--set-upstream-to"],
            vec!["branch", "--set-upstream-to=origin/main"],
            vec!["branch", "--set-upstream-to", "origin/main"],
            vec!["branch", "--set-upstream"],
            vec!["branch", "--set-upstream", "origin/main"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Prefix `--unset-upstream` (bare / `=` / with branch operand).
        for args in [
            vec!["branch", "--unset-upstream"],
            vec!["branch", "--unset-upstream=foo"],
            vec!["branch", "--unset-upstream", "mybranch"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Bundled: `-au` smuggles `-u` past exact.
        for flag in ["-au", "-ua"] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
        }
    }

    #[test]
    fn git_args_deny_branch_track() {
        // Exact `-t` (bare + bundled smuggling).
        for args in [vec!["branch", "-t"], vec!["branch", "-t=foo"]] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Prefix `--track` (bare / `=` / separate operands).
        // `--no-track` stays allowed (safe default, `starts_with("--track")`
        // does not match the `--no-` prefix).
        for args in [
            vec!["branch", "--track"],
            vec!["branch", "--track=foo"],
            vec!["branch", "--track", "newbranch", "origin/main"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
        // Bundled: `-at` smuggles `-t` past exact.
        for flag in ["-at", "-ta"] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
        }
    }

    #[test]
    fn git_args_deny_branch_edit_description() {
        // Writes branch description (bare / `=` / with branch operand).
        for args in [
            vec!["branch", "--edit-description"],
            vec!["branch", "--edit-description=foo"],
            vec!["branch", "--edit-description", "mybranch"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "branch {args:?} must be denied"
            );
        }
    }

    #[test]
    fn git_args_deny_branch_bundled_shorts() {
        // Single-dash clusters containing any of d/D/m/M/c/C/f/u/t denied.
        for flag in [
            "-ad", "-aD", "-am", "-aM", "-ac", "-aC", "-af", "-au", "-at", "-avm", "-da", "-Dm",
            "-vd",
        ] {
            assert!(
                !is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must be denied"
            );
        }
        // Glued `-u<value>` form.
        assert!(!is_allowed_git_args(&[
            "branch".to_string(),
            "-uorigin/main".to_string()
        ]));
    }

    #[test]
    fn git_args_allow_branch_list_clusters() {
        // Verified basis: `-av`/`-vv`/`-a`/`-v` contain none of the denied
        // cluster letters d/D/m/M/c/C/f/u/t, and `--list`/`--show-current`
        // are double-dash (cluster rule is single-dash only) — confirm here
        // so a future allowed flag containing a denied letter fails loudly.
        for flag in ["-av", "-vv", "-a", "-v"] {
            let cluster = flag.strip_prefix('-').expect("single-dash flag");
            assert!(
                !cluster.contains(['d', 'D', 'm', 'M', 'c', 'C', 'f', 'u', 't']),
                "{flag} must contain none of the denied cluster letters"
            );
            assert!(
                is_allowed_git_args(&["branch".to_string(), flag.to_string()]),
                "branch {flag} must stay allowed"
            );
        }
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "--list".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "branch".to_string(),
            "--show-current".to_string()
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
    fn probe_glued_c_config_override_denied() {
        // CTX-0465 hostile probe: glued `-c<key>=<val>` bypasses the exact
        // `-c` denial and smuggles `core.pager` / arbitrary config.
        for args in [
            vec!["log", "-ccore.pager=evil"],
            vec!["status", "-cfoo=bar"],
            vec!["diff", "-ccore.pager=less -R"],
            vec!["show", "-cprotocol.ext.allow=always"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "glued -c bypass {args:?} must be denied"
            );
        }
        // Legit double-dash forms must keep working.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--cached".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "--oneline".to_string()
        ]));
    }

    #[test]
    fn probe_config_long_form_denied_uniformly() {
        // CTX-0465 hostile probe: `--config` (non-env) bypasses the
        // `--config-env`-only denial.
        for args in [
            vec!["log", "--config", "core.pager=evil"],
            vec!["status", "--config=core.pager=evil"],
            vec!["diff", "--config", "protocol.ext.allow=always"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "long --config bypass {args:?} must be denied"
            );
        }
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--cached".to_string()
        ]));
    }

    #[test]
    fn probe_capital_c_repo_escape_denied_all_verbs() {
        // CTX-0465 hostile probe: `-C <path>` / `-C<path>` (run-in-directory)
        // is denied only in the `branch` arm; every other verb lets it
        // through as a repo escape.
        for verb in GIT_ALLOWED_SUBCOMMANDS {
            for args in [
                vec![*verb, "-C"],
                vec![*verb, "-C/tmp/evil"],
                vec![*verb, "-C", "/tmp/evil"],
            ] {
                let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                assert!(
                    !is_allowed_git_args(&owned),
                    "repo-escape -C bypass {args:?} must be denied"
                );
            }
        }
        // Legit panel use keeps working.
        assert!(is_allowed_git_args(&["branch".to_string()]));
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "-m".to_string(),
            "--oneline".to_string()
        ]));
        assert!(is_allowed_git_args(&["show".to_string(), "-m".to_string()]));
    }

    #[test]
    fn probe_pager_driver_flags_denied() {
        // CTX-0465 hostile probe: pager-driving flags force an external
        // pager program (external-process vector via captured pipes).
        for args in [
            vec!["log", "--paginate"],
            vec!["diff", "--pager"],
            vec!["status", "--paginate=never"],
            vec!["show", "--pager=evil"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
            assert!(
                !is_allowed_git_args(&owned),
                "pager bypass {args:?} must be denied"
            );
        }
        // Safe defaults and legit patch display keep working.
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--no-pager".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "log".to_string(),
            "-p".to_string(),
            "--oneline".to_string()
        ]));
        assert!(is_allowed_git_args(&[
            "diff".to_string(),
            "--no-ext-diff".to_string()
        ]));
    }

    #[test]
    fn probe_pager_env_passthrough_denied() {
        // CTX-0465 hostile probe: explicit spawn env carrying PAGER /
        // GIT_PAGER re-opens the pager vector closed for argv/config.
        assert!(!is_safe_spawn_env(&[(
            "GIT_PAGER".to_string(),
            "cat".to_string()
        )]));
        assert!(!is_safe_spawn_env(&[(
            "PAGER".to_string(),
            "less -R".to_string()
        )]));
        assert!(!is_safe_spawn_env(&[
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("GIT_PAGER".to_string(), "cat".to_string()),
        ]));
        // Benign env stays allowed; near-miss names are exact-match only.
        assert!(is_safe_spawn_env(&[]));
        assert!(is_safe_spawn_env(&[(
            "LANG".to_string(),
            "C.UTF-8".to_string()
        )]));
        assert!(is_safe_spawn_env(&[(
            "MY_PAGER".to_string(),
            "x".to_string()
        )]));
    }

    #[test]
    fn probe_git_config_env_override_denied() {
        // CTX-0488 hostile probe: env-encoded `-c` override re-opens the
        // config vector closed for argv (`-c` glued/exact, `--config*`).
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
        ] {
            assert!(
                !is_safe_spawn_env(&[(name.to_string(), "core.pager=evil".to_string())]),
                "{name} must be denied"
            );
        }
        // Case-folded variants must not bypass (Windows env lookup folds case).
        for name in ["git_config_count", "Git_Config_Key_0", "git_config_value_0"] {
            assert!(
                !is_safe_spawn_env(&[(name.to_string(), "1".to_string())]),
                "{name} must be denied case-insensitively"
            );
        }
        // The full env-encoded override triple is denied even when each entry
        // is individually plausible.
        assert!(!is_safe_spawn_env(&[
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "core.pager".to_string()),
            ("GIT_CONFIG_VALUE_0".to_string(), "evil".to_string()),
        ]));
    }

    #[test]
    fn probe_git_external_process_env_denied() {
        // CTX-0488 hostile probe: external diff/editor/ssh/askpass programs
        // and `git-*` exec-path lookup re-open process execution.
        for name in [
            "GIT_EXTERNAL_DIFF",
            "GIT_DIFF_OPTS",
            "GIT_EDITOR",
            "GIT_SEQUENCE_EDITOR",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_PROXY_COMMAND",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_EXEC_PATH",
            "GIT_DIR",
            "GIT_WORK_TREE",
        ] {
            assert!(
                !is_safe_spawn_env(&[(name.to_string(), "/tmp/evil".to_string())]),
                "{name} must be denied"
            );
        }
        // Safe/decorative settings and near-miss names stay allowed.
        assert!(is_safe_spawn_env(&[
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ("GIT_ATTR_NOSYSTEM".to_string(), "1".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("MY_PAGER".to_string(), "x".to_string()),
        ]));
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

    // ── ToolSpawnLedger (CTX-1152, issue #1985) ──

    fn spawn_args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn tool_ledger_records_allow_with_attribution() {
        let mut ledger = ToolSpawnLedger::new();
        assert!(ledger.is_empty());
        let allowed = check_tool_spawn(&mut ledger, "git", &spawn_args(&["status"]), "git-panel");
        assert!(allowed);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.dropped(), 0);
        let entry = ledger.iter().next().expect("exactly one entry");
        assert_eq!(entry.seq, 0);
        assert_eq!(entry.caller, "git-panel");
        assert_eq!(entry.tool, "git");
        assert_eq!(entry.decision, ToolSpawnDecision::Allow);
        assert_eq!(entry.args, spawn_args(&["status"]));
        assert_eq!(entry.args_omitted, 0);
        assert_eq!(entry.cwd, None, "host-side decisions carry no cwd");
    }

    #[test]
    fn tool_ledger_records_the_host_resolved_cwd() {
        // CTX-1715: the mediated spawn surface records the resolved pane
        // directory; host-side decisions (no cwd) stay `None`.
        let mut ledger = ToolSpawnLedger::new();
        ledger.push_decision_with_cwd(
            "git",
            &spawn_args(&["status"]),
            "git-panel",
            true,
            Some("/home/user/repo"),
        );
        let entry = ledger.iter().next().expect("exactly one entry");
        assert_eq!(entry.cwd.as_deref(), Some("/home/user/repo"));
        assert!(entry.detail.contains("cwd '/home/user/repo'"));
        assert_eq!(entry.decision, ToolSpawnDecision::Allow);

        let long = format!("/{}", "a".repeat(MAX_TOOL_SPAWN_CWD_BYTES + 8));
        ledger.push_decision_with_cwd(
            "git",
            &spawn_args(&["status"]),
            "git-panel",
            false,
            Some(&long),
        );
        let entry = ledger.iter().nth(1).expect("second entry");
        assert_eq!(
            entry.cwd.as_deref().map(str::len),
            Some(MAX_TOOL_SPAWN_CWD_BYTES),
            "the recorded cwd is clamped to the bound"
        );
    }

    #[test]
    fn tool_ledger_records_deny_for_unaccepted_tool_and_bad_args() {
        let mut ledger = ToolSpawnLedger::new();
        assert!(!check_tool_spawn(
            &mut ledger,
            "rg",
            &spawn_args(&["status"]),
            "git-panel"
        ));
        assert!(!check_tool_spawn(
            &mut ledger,
            "git",
            &spawn_args(&["push", "origin"]),
            "git-panel"
        ));
        assert_eq!(ledger.len(), 2);
        let entries: Vec<&ToolSpawnEntry> = ledger.iter().collect();
        assert!(
            entries
                .iter()
                .all(|entry| entry.decision == ToolSpawnDecision::Deny),
            "allow and deny must both record"
        );
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[0].tool, "rg");
        assert_eq!(entries[1].tool, "git");
    }

    #[test]
    fn tool_ledger_bound_drops_oldest() {
        let mut ledger = ToolSpawnLedger::new();
        for i in 0..MAX_TOOL_SPAWN_AUDIT_ENTRIES + 10 {
            let verb = if i % 2 == 0 { "status" } else { "push" };
            let allowed = check_tool_spawn(&mut ledger, "git", &spawn_args(&[verb]), "bound-probe");
            assert_eq!(allowed, i % 2 == 0);
        }
        assert_eq!(ledger.len(), MAX_TOOL_SPAWN_AUDIT_ENTRIES);
        assert_eq!(ledger.dropped(), 10);
        let oldest = ledger.iter().next().expect("oldest retained entry");
        assert_eq!(oldest.seq, 10);
    }

    #[test]
    fn tool_ledger_truncates_args_to_documented_bounds() {
        let mut ledger = ToolSpawnLedger::new();
        let long = "a".repeat(MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES + 16);
        let multibyte = "é".repeat(MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES);
        let mut args = vec!["status".to_string(), long.clone(), multibyte.clone()];
        for i in 0..MAX_TOOL_SPAWN_AUDIT_ARGS {
            args.push(format!("--flag{i}"));
        }
        assert!(check_tool_spawn(
            &mut ledger,
            "git",
            &args,
            "truncation-probe"
        ));
        let entry = ledger.iter().next().expect("exactly one entry");
        assert_eq!(entry.args.len(), MAX_TOOL_SPAWN_AUDIT_ARGS);
        assert_eq!(
            entry.args_omitted,
            args.len() - MAX_TOOL_SPAWN_AUDIT_ARGS,
            "remainder must be counted"
        );
        for preview in &entry.args {
            assert!(
                preview.len() <= MAX_TOOL_SPAWN_ARG_PREVIEW_BYTES,
                "preview over bound: {preview:?}"
            );
        }
        // Full over-bound arg text must not survive anywhere in the entry.
        assert!(!entry.args.iter().any(|arg| arg == &long));
        assert!(!entry.detail.contains(&long));
        assert!(!entry.detail.contains(&multibyte));
    }

    #[test]
    fn tool_ledger_never_records_env() {
        // secret_tiers discipline: secret values travel via env and
        // `secret://` handles, so env must never enter the ledger. The
        // decision API takes no env; this test pins that a secret-shaped
        // value live at the call site cannot leak into entries.
        let mut ledger = ToolSpawnLedger::new();
        let env = [(
            "GIT_TOKEN".to_string(),
            "sk-live-ENVSECRETMARKER-never-in-ledger-0001".to_string(),
        )];
        assert_eq!(env.len(), 1, "env is live at the call site");
        assert!(check_tool_spawn(
            &mut ledger,
            "git",
            &spawn_args(&["status"]),
            "env-probe"
        ));
        let secret = &env[0].1;
        let dump = format!("{ledger:?}");
        assert!(
            !dump.contains(secret.as_str()),
            "env value leaked into ledger debug"
        );
        let entry = ledger.iter().next().expect("exactly one entry");
        assert!(!entry.detail.contains(secret.as_str()));
        assert!(!entry.args.iter().any(|arg| arg.contains(secret.as_str())));
    }

    #[test]
    fn tool_ledger_records_handle_refs_as_names_only() {
        // `secret://` handles are names, never values (scrubber
        // discipline): recording the reference is names-only audit,
        // consistent with the secret ledgers.
        assert!(crate::secrets::SecretHandle::is_handle_ref(
            "secret://api-key"
        ));
        let mut ledger = ToolSpawnLedger::new();
        assert!(check_tool_spawn(
            &mut ledger,
            "git",
            &spawn_args(&["show", "secret://api-key"]),
            "handle-probe",
        ));
        let entry = ledger.iter().next().expect("exactly one entry");
        assert!(entry.args.iter().any(|arg| arg == "secret://api-key"));
    }

    #[test]
    fn tool_ledger_clamps_caller_and_tool_identities() {
        let mut ledger = ToolSpawnLedger::new();
        let long_caller = "c".repeat(MAX_TOOL_SPAWN_CALLER_BYTES + 8);
        let long_tool = "t".repeat(TOOL_NAME_MAX_LEN + 8);
        assert!(!check_tool_spawn(
            &mut ledger,
            &long_tool,
            &spawn_args(&["status"]),
            &long_caller
        ));
        let entry = ledger.iter().next().expect("exactly one entry");
        assert_eq!(entry.decision, ToolSpawnDecision::Deny);
        assert!(
            entry.caller.len() <= MAX_TOOL_SPAWN_CALLER_BYTES,
            "caller over bound"
        );
        assert!(entry.tool.len() <= TOOL_NAME_MAX_LEN, "tool over bound");
    }

    // ── repository metadata policy (CTX-1733, issue #2912) ──

    use std::path::{Path, PathBuf};

    /// Unique scratch root under the platform temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "bitty-git-metadata-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    /// Real `git` with the ambient repository location scrubbed.
    ///
    /// The test process can run under a hook that exports `GIT_DIR` (or one of
    /// the metadata redirects this policy denies), which would override the
    /// fixture's own repository; scrubbing makes every fixture hermetic.
    fn test_git() -> std::process::Command {
        let mut command = std::process::Command::new("git");
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(var);
        }
        command
    }

    /// Real repository at `dir`.
    fn git_init(dir: &Path) {
        std::fs::create_dir_all(dir).expect("repo dir");
        let status = test_git()
            .arg("init")
            .arg("--quiet")
            .arg(dir)
            .status()
            .expect("git must be available for these probes");
        assert!(status.success(), "git init must succeed");
    }

    /// Commit an empty marker commit so the fixture's history is readable.
    fn git_commit(dir: &Path, message: &str) {
        let status = test_git()
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "policy-probe")
            .env("GIT_AUTHOR_EMAIL", "policy-probe@example.invalid")
            .env("GIT_COMMITTER_NAME", "policy-probe")
            .env("GIT_COMMITTER_EMAIL", "policy-probe@example.invalid")
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                message,
            ])
            .status()
            .expect("git commit");
        assert!(status.success(), "fixture commit must succeed");
    }

    /// First ancestor of `dir` (excluding `dir` itself) that carries a `.git`
    /// entry, i.e. the repository git would discover from `dir`.
    fn ancestor_git(dir: &Path) -> Option<PathBuf> {
        dir.ancestors()
            .skip(1)
            .map(|ancestor| ancestor.join(".git"))
            .find(|candidate| std::fs::symlink_metadata(candidate).is_ok())
    }

    fn append_config(repo_git_dir: &Path, text: &str) {
        let path = repo_git_dir.join("config");
        let mut config = std::fs::read_to_string(&path).expect("repo config");
        config.push_str(text);
        std::fs::write(&path, config).expect("write repo config");
    }

    fn metadata_refusal(refusal: GitSpawnRefusal) -> GitMetadataRefusal {
        match refusal {
            GitSpawnRefusal::Metadata(refusal) => refusal,
            GitSpawnRefusal::NotAllowlisted => {
                panic!("the argv is allowlisted; the metadata gate must be the refuser")
            }
        }
    }

    #[test]
    fn git_metadata_refuses_gitfile_pointing_outside_the_grant() {
        let root = scratch("gitfile-escape");
        let grant = root.join("grant");
        let pane = grant.join("pane");
        let outside = root.join("outside");
        std::fs::create_dir_all(&pane).expect("pane");
        git_init(&outside);
        let target = outside.join(".git");
        std::fs::write(pane.join(".git"), format!("gitdir: {}\n", target.display()))
            .expect("planted gitfile");

        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &pane, &[grant])
                .expect_err("a gitfile naming an out-of-grant git dir must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::GitDirOutsideGrant);
        assert!(
            refusal.path().ends_with("outside/.git"),
            "the refused path names the gitfile target: {}",
            refusal.path()
        );
        assert!(
            refusal
                .audit_detail()
                .contains("git directory outside the grant")
        );
    }

    #[test]
    fn planted_gitfile_reads_an_out_of_grant_repository_without_the_policy() {
        // Threat proof, not policy proof: with no metadata gate an allowlisted
        // verb running in the pane really does read the out-of-grant
        // repository, which is the defect the policy above closes.
        let root = scratch("threat-proof");
        let grant = root.join("grant");
        let pane = grant.join("pane");
        let outside = root.join("outside");
        std::fs::create_dir_all(&pane).expect("pane");
        git_init(&outside);
        std::fs::write(outside.join("secret.txt"), b"outside the grant").expect("outside file");
        git_commit(&outside, "outside-grant-commit");
        std::fs::write(
            pane.join(".git"),
            format!("gitdir: {}\n", outside.join(".git").display()),
        )
        .expect("planted gitfile");

        // `log` is one of the seven allowlisted verbs, and it reads the history
        // that exists only in the out-of-grant repository.
        let output = test_git()
            .current_dir(&pane)
            .args(["log", "--oneline"])
            .output()
            .expect("git must be available");
        assert!(
            output.status.success(),
            "git follows the planted gitfile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("outside-grant-commit"),
            "the allowlisted verb reads the out-of-grant history: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        // The policy refuses exactly that spawn.
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status", "--porcelain"]),
                &pane,
                std::slice::from_ref(&grant),
            )
            .expect_err("the planted gitfile is the escape the policy closes"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::GitDirOutsideGrant);
    }

    #[test]
    fn git_metadata_refuses_core_worktree_outside_the_grant() {
        let root = scratch("worktree-escape");
        let grant = root.join("grant");
        let repo = grant.join("repo");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        git_init(&repo);

        // Absolute `core.worktree` outside the grant.
        append_config(
            &repo.join(".git"),
            &format!("\n[core]\n\tworktree = {}\n", outside.display()),
        );
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status"]),
                &repo,
                std::slice::from_ref(&grant),
            )
            .expect_err("core.worktree outside the grant must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::WorktreeOutsideGrant);
        assert!(
            refusal.path().ends_with("outside"),
            "the refused path names the work tree: {}",
            refusal.path()
        );

        // Relative `core.worktree` is resolved against the git dir, so a
        // `..`-walk that lands outside the grant is the same escape.
        append_config(&repo.join(".git"), "\tworktree = ../../../outside\n");
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status"]),
                &repo,
                std::slice::from_ref(&grant),
            )
            .expect_err("relative core.worktree escaping the grant must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::WorktreeOutsideGrant);

        // `core.bare` with a work tree elsewhere is the same channel.
        append_config(
            &repo.join(".git"),
            &format!("\tbare = true\n\tworktree = {}\n", outside.display()),
        );
        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &repo, &[grant])
                .expect_err("core.bare with an out-of-grant work tree must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::WorktreeOutsideGrant);
    }

    #[test]
    fn git_metadata_refuses_bare_without_worktree_is_allowed() {
        // A bare repository with no work tree cannot read a work tree at all:
        // git itself refuses work-tree verbs, so the spawn is not an escape.
        let root = scratch("bare");
        let repo = root.join("repo");
        git_init(&repo);
        append_config(&repo.join(".git"), "\tbare = true\n");
        let plan = plan_git_spawn(&spawn_args(&["log", "--oneline"]), &repo, &[root])
            .expect("a bare repo with no work tree has nothing outside the grant to read");
        let binding = plan.repo.expect("repo resolved");
        assert!(binding.bare);
        assert!(binding.work_tree.is_none());
    }

    #[test]
    fn git_metadata_refuses_config_include() {
        let root = scratch("include");
        let grant = root.join("grant");
        let repo = grant.join("repo");
        let outside = root.join("outside.cfg");
        git_init(&repo);
        std::fs::write(&outside, b"[core]\n\tworktree = /tmp\n").expect("outside config");
        append_config(
            &repo.join(".git"),
            &format!("\n[include]\n\tpath = {}\n", outside.display()),
        );

        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &repo, &[grant])
                .expect_err("a config include is a channel the probe will not follow"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::ConfigIncludeRefused);
        assert!(refusal.path().ends_with("outside.cfg"));
    }

    #[test]
    fn git_metadata_refuses_unparsable_gitfile() {
        let root = scratch("bad-gitfile");
        git_init(&root);
        let pane = root.join("pane");
        std::fs::create_dir_all(&pane).expect("pane");
        std::fs::write(pane.join(".git"), b"not a gitfile\n").expect("hostile gitfile");
        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &pane, &[root])
                .expect_err("an unparsable gitfile must fail closed"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::MetadataUnreadable);
    }

    #[cfg(unix)]
    #[test]
    fn git_metadata_refuses_symlinked_git_dir_outside_the_grant() {
        let root = scratch("symlink-escape");
        let grant = root.join("grant");
        let pane = grant.join("pane");
        let outside = root.join("outside");
        std::fs::create_dir_all(&pane).expect("pane");
        git_init(&outside);
        std::os::unix::fs::symlink(outside.join(".git"), pane.join(".git"))
            .expect("planted .git symlink");
        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &pane, &[grant])
                .expect_err("a .git symlink to an out-of-grant git dir must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::GitDirOutsideGrant);
    }

    #[test]
    fn git_metadata_refuses_repository_root_above_the_grant() {
        // A nested cwd has no `.git` of its own; git would walk up to the
        // parent repository. That is legitimate only when the root is inside
        // the grant.
        let root = scratch("parent-root");
        let repo = root.join("repo");
        git_init(&repo);
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).expect("nested");
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status"]),
                &nested,
                std::slice::from_ref(&nested),
            )
            .expect_err("a repository root outside the grant must be refused"),
        );
        assert_eq!(
            refusal.kind(),
            GitMetadataRefusalKind::ParentRepoOutsideGrant
        );
        assert!(
            refusal.path().ends_with("repo"),
            "the refused path names the out-of-grant root: {}",
            refusal.path()
        );
    }

    #[test]
    fn git_metadata_refuses_object_store_and_commondir_escapes() {
        let root = scratch("pointer-escape");
        let grant = root.join("grant");
        let repo = grant.join("repo");
        let outside = root.join("outside");
        git_init(&outside);
        git_init(&repo);
        let info = repo.join(".git").join("objects").join("info");
        std::fs::create_dir_all(&info).expect("objects/info");

        // Object store redirected outside the grant: `log`/`show`/`diff`
        // would read objects that live there.
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", outside.join(".git").join("objects").display()),
        )
        .expect("alternates");
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["log", "--oneline"]),
                &repo,
                std::slice::from_ref(&grant),
            )
            .expect_err("an out-of-grant object store must be refused"),
        );
        assert_eq!(
            refusal.kind(),
            GitMetadataRefusalKind::ObjectStoreOutsideGrant
        );
        assert!(refusal.path().ends_with("objects"));

        // A remote object store has no containment test that can hold it.
        std::fs::remove_file(info.join("alternates")).expect("clear alternates");
        std::fs::write(
            info.join("http-alternates"),
            b"https://example.invalid/objects\n",
        )
        .expect("http-alternates");
        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["log"]), &repo, std::slice::from_ref(&grant))
                .expect_err("a remote object store must be refused"),
        );
        assert_eq!(
            refusal.kind(),
            GitMetadataRefusalKind::ObjectStoreOutsideGrant
        );
        std::fs::remove_file(info.join("http-alternates")).expect("clear http-alternates");

        // `commondir` relocates the git directory itself.
        std::fs::write(
            repo.join(".git").join("commondir"),
            format!("{}\n", outside.join(".git").display()),
        )
        .expect("commondir");
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status"]),
                &repo,
                std::slice::from_ref(&grant),
            )
            .expect_err("an out-of-grant commondir must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::GitDirOutsideGrant);
    }

    #[test]
    fn git_metadata_refuses_config_worktree_file_escape() {
        // `extensions.worktreeConfig` makes git read `.git/config.worktree`,
        // which can carry `core.worktree` just like the main config.
        let root = scratch("config-worktree");
        let grant = root.join("grant");
        let repo = grant.join("repo");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        git_init(&repo);
        std::fs::write(
            repo.join(".git").join("config.worktree"),
            format!("[core]\n\tworktree = {}\n", outside.display()),
        )
        .expect("config.worktree");
        let refusal = metadata_refusal(
            plan_git_spawn(
                &spawn_args(&["status"]),
                &repo,
                std::slice::from_ref(&grant),
            )
            .expect_err("core.worktree in config.worktree must be refused"),
        );
        assert_eq!(refusal.kind(), GitMetadataRefusalKind::WorktreeOutsideGrant);
        assert!(refusal.path().ends_with("outside"));
    }

    #[test]
    fn git_metadata_refuses_a_tree_deeper_than_the_discovery_bound() {
        // Past the bound the repository git would discover cannot be proven to
        // be inside the grant, so the probe fails closed instead of guessing.
        let root = scratch("too-deep");
        let mut deep = root.clone();
        for _ in 0..=MAX_GIT_DISCOVERY_DEPTH {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).expect("deep tree");
        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &deep, std::slice::from_ref(&root))
                .expect_err("a tree beyond the discovery bound cannot be proven safe"),
        );
        assert_eq!(
            refusal.kind(),
            GitMetadataRefusalKind::DiscoveryBoundExceeded
        );
    }

    #[test]
    fn git_metadata_allows_nested_cwd_inside_an_in_grant_repository() {
        let root = scratch("nested-ok");
        let grant = root.join("grant");
        git_init(&grant);
        let nested = grant.join("nested");
        std::fs::create_dir_all(&nested).expect("nested");
        let plan = plan_git_spawn(
            &spawn_args(&["status"]),
            &nested,
            std::slice::from_ref(&grant),
        )
        .expect("the parent repository is inside the grant");
        let binding = plan.repo.expect("repo resolved");
        assert_eq!(binding.repo_root, grant);
        assert_eq!(binding.git_dir, grant.join(".git"));
    }

    #[test]
    fn git_metadata_allows_in_grant_repository_and_the_verb_runs() {
        let root = scratch("in-grant");
        let grant = root.join("grant");
        git_init(&grant);
        std::fs::write(grant.join("tracked.txt"), b"fixture").expect("fixture file");

        let plan = plan_git_spawn(
            &spawn_args(&["status", "--porcelain"]),
            &grant,
            std::slice::from_ref(&grant),
        )
        .expect("an in-grant repository must still be allowed");
        let binding = plan.repo.as_ref().expect("repo resolved");
        assert_eq!(binding.repo_root, grant);
        assert_eq!(binding.git_dir, grant.join(".git"));
        assert!(binding.work_tree.is_none());
        assert!(!binding.bare);
        assert_eq!(plan.cwd, grant, "the plan records the authorized cwd");

        // The constructed command really runs the gated verb.
        let mut command = test_git();
        command.current_dir(&plan.cwd);
        command.args(&plan.args);
        for (name, value) in &plan.env {
            command.env(name, value);
        }
        let output = command.output().expect("git must be available");
        assert!(
            output.status.success(),
            "the gated verb must run: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "?? tracked.txt",
            "status reports the untracked fixture"
        );
    }

    #[test]
    fn git_metadata_allows_no_repository_at_all() {
        let root = scratch("no-repo");
        // The platform temp dir can itself live inside a repository (the
        // TMPDIR of this workspace does), so derive the expected answer from
        // the fixture instead of assuming an empty environment: no ancestor
        // repository means git fails on its own; an ancestor repository the
        // grant does not cover is refused, exactly like the parent-root case.
        match ancestor_git(&root) {
            None => {
                let plan =
                    plan_git_spawn(&spawn_args(&["status"]), &root, std::slice::from_ref(&root))
                        .expect("no repository reads nothing outside the grant");
                assert!(plan.repo.is_none());
            }
            Some(outer) => {
                let refusal = metadata_refusal(
                    plan_git_spawn(&spawn_args(&["status"]), &root, std::slice::from_ref(&root))
                        .expect_err("an ancestor repository outside the grant must be refused"),
                );
                assert_eq!(
                    refusal.kind(),
                    GitMetadataRefusalKind::ParentRepoOutsideGrant
                );
                assert_eq!(Path::new(refusal.path()), outer.parent().expect("repo dir"));
            }
        }
    }

    #[test]
    fn git_metadata_refuses_relative_authorized_cwd() {
        let plan = plan_git_spawn(
            &spawn_args(&["status"]),
            Path::new("relative/pane"),
            &[PathBuf::from("/tmp")],
        );
        assert_eq!(
            plan,
            Err(GitSpawnRefusal::Metadata(GitMetadataRefusal::new(
                GitMetadataRefusalKind::AuthorizedCwdNotAbsolute,
                Path::new("relative/pane"),
            )))
        );
    }

    #[test]
    fn git_metadata_refuses_before_the_allowlist_does_not_run() {
        let root = scratch("not-allowlisted");
        assert_eq!(
            plan_git_spawn(&spawn_args(&["push", "origin"]), &root, &[]),
            Err(GitSpawnRefusal::NotAllowlisted)
        );
        assert_eq!(
            plan_git_spawn(&spawn_args(&["log", "-C", "/tmp"]), &root, &[]),
            Err(GitSpawnRefusal::NotAllowlisted)
        );
    }

    #[test]
    fn git_spawn_env_disables_optional_locks_for_read_only_verbs() {
        let root = scratch("optional-locks");
        git_init(&root);
        let plan = plan_git_spawn(&spawn_args(&["status"]), &root, std::slice::from_ref(&root))
            .expect("an in-grant repository is gated, not refused");
        assert_eq!(
            plan.env,
            vec![(
                GIT_OPTIONAL_LOCKS_VAR.to_string(),
                GIT_OPTIONAL_LOCKS_DISABLED.to_string()
            )],
            "the only child env entry is the optional-lock switch"
        );
        assert_eq!(git_read_only_spawn_env(), plan.env);
        assert_eq!(GIT_READ_ONLY_SPAWN_ENV.len(), 1);
        assert!(
            is_safe_spawn_env(&plan.env),
            "the read-only env must not carry a denied variable name"
        );
        assert!(
            !env_name_is_denied(GIT_OPTIONAL_LOCKS_VAR),
            "GIT_OPTIONAL_LOCKS must stay passable: it is the hardening, not a vector"
        );
    }

    #[test]
    fn git_metadata_redirect_env_names_are_denied() {
        for name in [
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CEILING_DIRECTORIES",
        ] {
            assert!(
                !is_safe_spawn_env(&[(name.to_string(), "/tmp/evil".to_string())]),
                "{name} redirects repository metadata and must be denied"
            );
        }
    }

    #[test]
    fn audit_detail_is_bidi_safe() {
        // Real bidi override (U+202E) and zero-width joiner (U+200D) in a
        // plugin-influenced directory name, plus a newline that would forge a
        // log line.
        let hostile = "repo\u{202E}gpj.exe\u{200D}/pane\nforged";
        assert!(is_audit_format_char('\u{202E}'));
        assert!(is_audit_format_char('\u{200D}'));
        assert!(!is_audit_format_char('a'));
        let cleaned = sanitize_audit_path(hostile);
        assert_eq!(cleaned, "repogpj.exe/pane?forged");
        assert!(!cleaned.chars().any(is_audit_format_char));

        let mut ledger = ToolSpawnLedger::new();
        ledger.push_decision_with_cwd(
            "git",
            &spawn_args(&["status", "hostile\u{202E}arg"]),
            "bidi-probe",
            true,
            Some(hostile),
        );
        let entry = ledger.iter().next().expect("exactly one entry");
        assert!(!entry.detail.contains('\u{202E}'));
        assert!(!entry.detail.contains('\u{200D}'));
        assert!(!entry.detail.contains('\n'));
        let recorded_cwd = entry.cwd.as_deref().expect("cwd recorded");
        assert!(!recorded_cwd.contains('\u{202E}'));
        assert!(!recorded_cwd.contains('\u{200D}'));
        assert!(
            entry.args.iter().all(|arg| !arg.contains('\u{202E}')),
            "arg previews are sanitised too"
        );
    }

    #[test]
    fn metadata_refusal_records_the_refused_path_in_the_ledger() {
        let root = scratch("refusal-audit");
        let grant = root.join("grant");
        let pane = grant.join("pane");
        // The refused path carries a bidi override, so the recorded detail
        // proves the sanitisation runs on the refusal path as well.
        let outside = root.join("out\u{202E}side");
        std::fs::create_dir_all(&pane).expect("pane");
        git_init(&outside);
        std::fs::write(
            pane.join(".git"),
            format!("gitdir: {}\n", outside.join(".git").display()),
        )
        .expect("planted gitfile");

        let refusal = metadata_refusal(
            plan_git_spawn(&spawn_args(&["status"]), &pane, &[grant])
                .expect_err("out-of-grant gitfile must be refused"),
        );
        assert!(
            !refusal.path().contains('\u{202E}'),
            "the refusal path is sanitised: {}",
            refusal.path()
        );
        assert!(refusal.path().contains("outside"));

        let mut ledger = ToolSpawnLedger::new();
        ledger.push_git_metadata_refusal(
            &spawn_args(&["status"]),
            "git-panel",
            Some(&pane.display().to_string()),
            &refusal,
        );
        assert_eq!(ledger.len(), 1);
        let entry = ledger.iter().next().expect("exactly one entry");
        assert_eq!(entry.decision, ToolSpawnDecision::Deny);
        assert_eq!(entry.tool, "git");
        assert!(
            entry.detail.contains("git directory outside the grant"),
            "the detail names the refused channel: {}",
            entry.detail
        );
        assert!(
            entry.detail.contains("cwd '"),
            "the detail keeps the authorized cwd: {}",
            entry.detail
        );
        assert!(!entry.detail.contains('\u{202E}'));
    }
}
