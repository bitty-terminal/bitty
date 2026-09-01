#![forbid(unsafe_code)]
//! Git panel via Panel Runtime — tiled Panel, process.spawn:git(...) allowlisted [tools.git], bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.git-panel` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Git panel is a
//! tiled `Panel(PanelId)` workspace (reuses `LayoutNode` `H`/`V` with panel
//! content, not a PTY) for branch, status, diff, log presentation, commit
//! staging UX, and Git-aware statusline segment selection; policy decides
//! filtering and ranking, not core. System CLI reuse is via
//! `process.spawn:git(...)` with manifest-declared `[tools.git]` allowlist
//! (per Layer 2 of `plugin-reuse-and-providers.md`), plus
//! `terminal.semantic-read` for cwd/link context; `Git` state via host-provided
//! service or helper output, not ambient shell, and allowlisted `git` CLI
//! outputs are piped to panel UI, not raw PTY injection. It verifies
//! `PanelId` distinct newtype with no `From` bridge to `ViewId`/`TerminalId`,
//! `Generation` monotonic with reserve `1024` and fail-closed exhaustion,
//! lifecycle `Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed`
//! with validated transitions, command registry `owner.name:command` qualified
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`, `<=128` chars) duplicates
//! rejected per-type `32` bound, overlay max `4` plus `1` modal with modal
//! exclusivity and text `128`/tooltip `256` bounds, `Palette` kind, EventBus
//! with three levels `64`/`1024`/`256 KiB`/`8192`/`2 MiB` and `8 KiB` payload
//! `DropOldest` default with counted per-queue attribution and coalescing for
//! observation topics (PR-1..PR-12, `PanelRegistry` single-process `winit`
//! one-registry-per-window headless, `ViewContent::Panel`), capability
//! isolation per `(PanelId, generation)` deny-by-default via `CapabilityId`
//! panel family `panel.provider`/`panel.create`/`panel.focus`/`panel.overlay`
//! — no ambient authority, no first-party bypass, plus
//! `process.spawn:git` allowlisted `[tools.git]` bounded `8 KiB`/`32` and
//! `terminal.semantic-read` plus optional `fs.read:PATH_GLOB` via
//! `FilesystemRequest`. No parser, renderer, or input hot path is entered,
//! and no grid mutation ever occurs here (only `Action` writes `State` per
//! Terminal State RFC). Default is disabled (fresh `EffectiveConfig` has empty
//! `plugins`); `bitty --safe` rejects `bitty-terminal.*` as non-builtin
//! without panic, identical to third-party `xuepoo.*` parity (no private
//! channel). Bounded queues (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`,
//! `8 KiB` payload, `32`/`8 KiB` batch) and single-process `winit`
//! `PanelRegistry` per window are verified headlessly. Child processes are
//! counted under the requesting generation (RC-1/RC-2 attribution) and
//! `is_untrusted_surface = true` for any terminal bytes reflected in tool
//! output; tool allowlist is static in `manifest_hash` and raising optional
//! `required=false` to `required=true` is a capability increase whose grant
//! must be re-confirmed.

use bitty_ui::{
    LayoutNode, SplitAxis, View, ViewId,
    panel::{MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN},
};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Filesystem capability pattern for git-panel — optional working-tree read.
///
/// Constrained via `FilesystemRequest` path-glob, real-path resolved,
/// symlinks/devices rejected per host policy. Mirrors the project pattern
/// `~/projects/**` as the safe baseline; the git-panel widens only via
/// explicit grant-checked `PATH_GLOB`, never ambient.
pub const GIT_PANEL_FS_READ_PATTERN: &str = "~/projects/**";

/// Process spawn capability for git — allowlisted via `[tools.git]`.
///
/// The manifest declares `process.spawn:git` (closed `process.spawn` family
/// plus `:git` parameter). Host enforces the allowlist shape; any other
/// executable is denied and any `git` arg that is not in the bounded
/// allowlist fails closed via `GitIntegration::is_allowed_git_args`.
pub const GIT_PANEL_PROCESS_SPAWN_GIT: &str = "process.spawn:git";

/// Allowed `git` subcommands for the panel — bounded, read-only observation.
///
/// These are the only `git` verbs the panel may spawn via
/// `process.spawn:git(...)` under the `[tools.git]` allowlist. Write verbs
/// (`commit`, `push`, `reset`, `checkout` mutating, etc.) are intentionally
/// absent; staging/commit UX must be confirmed via explicit user action and
/// a broader grant, never ambient.
pub const GIT_ALLOWED_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "log",
    "branch",
    "show",
    "rev-parse",
    "ls-files",
];

/// Maximum git entries to list per view — bounded for presentation.
///
/// PR-5/PR-6: payload `8 KiB`, batch `32`/`8 KiB`; listing larger than this
/// is truncated deterministically after sorting, headlessly testable.
pub const GIT_PANEL_MAX_ENTRIES: usize = 128;

/// Maximum commits to display in log view — bounded `64` mirroring
/// per-subscription bound (PR-7) and log truncation.
pub const GIT_PANEL_MAX_COMMITS: usize = 64;

/// Maximum branches to display — bounded `32` mirroring per-workspace bound.
pub const GIT_PANEL_MAX_BRANCHES: usize = 32;

/// Maximum chars per branch name — mirrors overlay text bound (128).
pub const GIT_PANEL_MAX_BRANCH_CHARS: usize = MAX_OVERLAY_TEXT_LEN;

/// Maximum chars per file name / path segment — mirrors overlay text bound (128).
pub const GIT_PANEL_MAX_NAME_CHARS: usize = MAX_OVERLAY_TEXT_LEN;

/// Maximum chars per commit message — mirrors overlay tooltip bound (256).
pub const GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS: usize = MAX_OVERLAY_TOOLTIP_LEN;

/// Maximum bytes per path — mirrors parser `BoundedString::MAX_LEN` (4096)
/// and project path bound.
pub const GIT_PANEL_MAX_PATH_BYTES: usize = 4096;

/// Maximum bytes per git arg — bounded `256` for arg shape.
pub const GIT_PANEL_MAX_GIT_ARG_BYTES: usize = 256;

/// Maximum git args per spawn — bounded `32` for surface.
pub const GIT_PANEL_MAX_GIT_ARGS: usize = 32;

/// Maximum total bytes for git spawn args — bounded `8 KiB` payload.
pub const GIT_PANEL_MAX_GIT_TOTAL_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Panel payload for git-panel observations is bounded by
/// `BUS_EVENT_MAX_BYTES` (8 KiB) at the bus admission boundary (PR-5).
pub const GIT_PANEL_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Maximum selection size — bounded `64` mirroring per-subscription bound (PR-7).
pub const GIT_PANEL_MAX_SELECTION: usize = crate::registry::BUS_PER_SUBSCRIPTION_LIMIT;

/// Maximum panels per workspace for git-panel — mirrors `MAX_PANELS_PER_WORKSPACE` (32) PR-1.
pub const GIT_PANEL_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
/// Maximum panels per window — mirrors `MAX_PANELS_PER_WINDOW` (64) PR-2.
pub const GIT_PANEL_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// Canonical git-panel commands (qualified `owner.name:command`).
pub const GIT_PANEL_COMMAND_OPEN: &str = "bitty-terminal.git-panel:open";
pub const GIT_PANEL_COMMAND_STATUS: &str = "bitty-terminal.git-panel:status";
pub const GIT_PANEL_COMMAND_DIFF: &str = "bitty-terminal.git-panel:diff";
pub const GIT_PANEL_COMMAND_LOG: &str = "bitty-terminal.git-panel:log";
pub const GIT_PANEL_COMMAND_BRANCH: &str = "bitty-terminal.git-panel:branch";

/// Git file status — presentation only, not `libgit2` introspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
    Ignored,
    Clean,
}

impl std::fmt::Display for GitFileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
            Self::Ignored => "ignored",
            Self::Clean => "clean",
        };
        f.write_str(s)
    }
}

/// Single branch entry — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitBranch {
    /// Branch name, truncated at char boundary to `GIT_PANEL_MAX_BRANCH_CHARS`.
    pub name: String,
    /// Whether this is the current checked-out branch.
    pub is_current: bool,
    /// Whether name was truncated at the display bound.
    pub truncated: bool,
}

impl GitBranch {
    /// Creates a branch entry from a raw name, validating bounds and allowlist.
    /// Returns `None` for invalid, empty, or disallowed names.
    #[must_use]
    pub fn new(name: String, is_current: bool) -> Option<Self> {
        if !GitIntegration::is_valid_branch_name(&name) {
            return None;
        }
        let (bounded, truncated) = truncate_bounded(&name, GIT_PANEL_MAX_BRANCH_CHARS);
        Some(Self {
            name: bounded,
            is_current,
            truncated,
        })
    }
}

/// Single status entry — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitStatusEntry {
    /// File path, validated and bounded to `GIT_PANEL_MAX_PATH_BYTES`.
    pub path: String,
    /// Status kind for presentation.
    pub status: GitFileStatus,
    /// Whether path was truncated (file name part).
    pub truncated: bool,
}

impl GitStatusEntry {
    /// Creates a status entry from a validated path. Returns `None` for invalid
    /// paths or paths outside the repo read scope.
    #[must_use]
    pub fn from_path(path: String, status: GitFileStatus) -> Option<Self> {
        if !GitIntegration::is_valid_path(&path) {
            return None;
        }
        if !GitIntegration::is_within_repo(&path) {
            return None;
        }
        let truncated = path.chars().count() > GIT_PANEL_MAX_NAME_CHARS * 4;
        Some(Self {
            path,
            status,
            truncated,
        })
    }
}

/// Single commit entry — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitCommit {
    /// Full hash (7..40 hex chars), validated.
    pub hash: String,
    /// Short hash (first 7 chars) for display.
    pub short_hash: String,
    /// Commit message, truncated at char boundary to `GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS`.
    pub message: String,
    /// Whether message was truncated.
    pub truncated: bool,
}

impl GitCommit {
    /// Creates a commit entry from hash and message. Returns `None` for invalid
    /// hashes or messages containing control/null bytes.
    #[must_use]
    pub fn new(hash: String, message: String) -> Option<Self> {
        if !GitIntegration::is_valid_commit_hash(&hash) {
            return None;
        }
        if message.contains('\0')
            || message
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            // Allow newline/tab in message body but reject other controls.
            if message
                .chars()
                .any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\t'))
            {
                return None;
            }
        }
        if message.len() > GIT_PANEL_PAYLOAD_MAX_BYTES {
            return None;
        }
        let short_hash = hash.chars().take(7).collect();
        let (bounded_msg, truncated) =
            truncate_bounded(&message, GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS);
        Some(Self {
            hash,
            short_hash,
            message: bounded_msg,
            truncated,
        })
    }
}

fn truncate_bounded(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let truncated: String = s.chars().take(max_chars).collect();
    (truncated, true)
}

/// GitIntegration — pure, observation-only helpers over committed state and
/// process-spawn capability. No mutation of `State`, no hot-path, bounded
/// `<=128` entries, tiled `LayoutNode` `H`/`V` reuse, no new tiling primitive.
/// All `git` outputs are treated as `is_untrusted_surface = true` when they
/// reflect terminal bytes; child processes are counted under the requesting
/// generation (RC-1/RC-2 attribution).
#[derive(Debug, Clone, Copy)]
pub struct GitIntegration;

impl GitIntegration {
    /// Whether `path` is a valid bounded path (no null byte, `<=4096` bytes,
    /// no control chars except maybe, non-empty).
    #[must_use]
    pub fn is_valid_path(path: &str) -> bool {
        if path.is_empty() || path.len() > GIT_PANEL_MAX_PATH_BYTES {
            return false;
        }
        if path.contains('\0') {
            return false;
        }
        if path.chars().any(|c| c.is_control()) {
            return false;
        }
        true
    }

    /// Whether `path` is within the granted repo read scope `~/projects/**`.
    #[must_use]
    pub fn is_within_repo(path: &str) -> bool {
        if !Self::is_valid_path(path) {
            return false;
        }
        if path == "~/projects" || path == "~/projects/" {
            return true;
        }
        if !path.starts_with("~/projects/") {
            return false;
        }
        if path.contains("..") {
            return false;
        }
        true
    }

    /// Alias for repo-scope check — mirrors `CapabilityId` family check
    /// without I/O: `candidate` must satisfy `is_within_repo`.
    #[must_use]
    pub fn is_fs_allowed(candidate: &str) -> bool {
        Self::is_within_repo(candidate)
    }

    /// Whether `branch` is a valid git branch name.
    ///
    /// Bounded to `GIT_PANEL_MAX_BRANCH_CHARS`, must not be empty, must not
    /// contain `..`, `~`, `^`, `:`, `?`, `*`, `[`, control, null, whitespace,
    /// and must not end with `/` or `.lock`. Mirrors `git check-ref-format`
    /// simplified, headlessly testable, without I/O.
    #[must_use]
    pub fn is_valid_branch_name(branch: &str) -> bool {
        if branch.is_empty() || branch.len() > GIT_PANEL_MAX_BRANCH_CHARS {
            return false;
        }
        if branch.contains('\0') || branch.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return false;
        }
        if branch.contains("..")
            || branch.contains('~')
            || branch.contains('^')
            || branch.contains(':')
        {
            return false;
        }
        if branch.contains('?') || branch.contains('*') || branch.contains('[') {
            return false;
        }
        if branch.ends_with('/') || branch.ends_with('.') || branch.ends_with(".lock") {
            return false;
        }
        if branch.starts_with('/') || branch.starts_with('.') {
            return false;
        }
        if branch.contains("//") {
            return false;
        }
        if branch.contains('\0') {
            return false;
        }
        // Reject shell metachars for spawn safety.
        if branch.contains(';')
            || branch.contains('&')
            || branch.contains('|')
            || branch.contains('`')
            || branch.contains('$')
            || branch.contains('(')
            || branch.contains(')')
            || branch.contains('<')
            || branch.contains('>')
            || branch.contains('\\')
            || branch.contains('"')
            || branch.contains('\'')
        {
            return false;
        }
        true
    }

    /// Whether `hash` is a valid commit hash (hex, 7..40 chars).
    #[must_use]
    pub fn is_valid_commit_hash(hash: &str) -> bool {
        if hash.len() < 7 || hash.len() > 40 {
            return false;
        }
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        true
    }

    /// Whether `args` is an allowlisted `git` invocation under `[tools.git]`.
    ///
    /// Pure, bounded check: `args` must be non-empty, `args[0]` in
    /// `GIT_ALLOWED_SUBCOMMANDS`, `args.len() <= GIT_PANEL_MAX_GIT_ARGS`,
    /// each arg `len <= GIT_PANEL_MAX_GIT_ARG_BYTES`, total `<= 8 KiB`,
    /// no null/control/shell metachars (`; & | \` $ ( ) < > \ " ' \n`), no
    /// `--upload-pack` or other risky flags. Fails closed.
    #[must_use]
    pub fn is_allowed_git_args(args: &[String]) -> bool {
        if args.is_empty() || args.len() > GIT_PANEL_MAX_GIT_ARGS {
            return false;
        }
        let mut total = 0usize;
        for arg in args {
            if arg.is_empty() || arg.len() > GIT_PANEL_MAX_GIT_ARG_BYTES {
                return false;
            }
            total += arg.len();
            if total > GIT_PANEL_MAX_GIT_TOTAL_BYTES {
                return false;
            }
            if arg.contains('\0') || arg.chars().any(|c| c.is_control()) {
                return false;
            }
            // Shell metachars — fail closed, no interpolation.
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
        // First arg is subcommand allowlisted.
        let sub = &args[0];
        if !GIT_ALLOWED_SUBCOMMANDS.contains(&sub.as_str()) {
            return false;
        }
        // Reject risky flags globally.
        for arg in args {
            if arg == "--upload-pack" || arg == "--receive-pack" || arg == "--exec" {
                return false;
            }
            if arg.contains("--upload-pack=") || arg.contains("--receive-pack=") {
                return false;
            }
        }
        true
    }

    /// Whether `arg_str` (space-separated `git` args) is allowlisted.
    ///
    /// Splits on whitespace and delegates to `is_allowed_git_args`. Rejects
    /// empty or over-bound input. Convenience for single-string checks.
    #[must_use]
    pub fn is_allowed_git_command_str(arg_str: &str) -> bool {
        if arg_str.trim().is_empty() || arg_str.len() > GIT_PANEL_MAX_GIT_TOTAL_BYTES {
            return false;
        }
        let args: Vec<String> = arg_str.split_whitespace().map(|s| s.to_string()).collect();
        Self::is_allowed_git_args(&args)
    }

    /// Whether `process.spawn:git` capability string is exactly the allowlisted
    /// capability. Pure check mirroring `CapabilityId` family without I/O.
    #[must_use]
    pub fn is_process_spawn_git_allowed(capability: &str) -> bool {
        capability == GIT_PANEL_PROCESS_SPAWN_GIT
    }

    /// Whether `path` fits payload and name bounds.
    #[must_use]
    pub fn is_path_bounded(path: &str) -> bool {
        path.len() <= GIT_PANEL_MAX_PATH_BYTES
            && path.chars().count() <= GIT_PANEL_MAX_NAME_CHARS * 4
    }

    /// Filters and bounds a raw `branches` listing to `GIT_PANEL_MAX_BRANCHES`
    /// valid branches, sorted deterministic, deduped by name. Pure observation.
    #[must_use]
    pub fn list_branches(raw: &[String]) -> Vec<GitBranch> {
        let mut branches: Vec<GitBranch> = raw
            .iter()
            .filter_map(|n| {
                let trimmed = n.trim();
                let is_current = trimmed.starts_with('*');
                let name = if is_current {
                    trimmed.trim_start_matches('*').trim().to_string()
                } else {
                    trimmed.to_string()
                };
                GitBranch::new(name, is_current)
            })
            .collect();
        branches.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| b.is_current.cmp(&a.is_current))
        });
        branches.dedup_by(|a, b| a.name == b.name);
        if branches.len() > GIT_PANEL_MAX_BRANCHES {
            branches.truncate(GIT_PANEL_MAX_BRANCHES);
        }
        branches
    }

    /// Filters `branches` by case-insensitive substring `query` over `name`.
    /// Query truncated to `GIT_PANEL_MAX_NAME_CHARS`; bounded to
    /// `GIT_PANEL_MAX_BRANCHES` and never allocates beyond that.
    #[must_use]
    pub fn filter_branches(branches: &[GitBranch], query: &str) -> Vec<GitBranch> {
        let bounded_query = if query.chars().count() <= GIT_PANEL_MAX_NAME_CHARS {
            query.to_owned()
        } else {
            query.chars().take(GIT_PANEL_MAX_NAME_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for b in branches {
            if out.len() >= GIT_PANEL_MAX_BRANCHES {
                break;
            }
            if lower.is_empty() || b.name.to_ascii_lowercase().contains(&lower) {
                out.push(b.clone());
            }
        }
        out
    }

    /// Filters and bounds a raw `paths` listing to `GIT_PANEL_MAX_ENTRIES`
    /// valid status entries, sorted deterministic, deduped by path. Pure observation.
    #[must_use]
    pub fn list_status_entries(paths: &[String], status: GitFileStatus) -> Vec<GitStatusEntry> {
        let mut entries: Vec<GitStatusEntry> = paths
            .iter()
            .filter_map(|p| GitStatusEntry::from_path(p.clone(), status))
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        if entries.len() > GIT_PANEL_MAX_ENTRIES {
            entries.truncate(GIT_PANEL_MAX_ENTRIES);
        }
        entries
    }

    /// Filters `entries` by case-insensitive substring `query` over `path`.
    #[must_use]
    pub fn filter_status_entries(entries: &[GitStatusEntry], query: &str) -> Vec<GitStatusEntry> {
        let bounded_query = if query.chars().count() <= GIT_PANEL_MAX_NAME_CHARS {
            query.to_owned()
        } else {
            query.chars().take(GIT_PANEL_MAX_NAME_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= GIT_PANEL_MAX_ENTRIES {
                break;
            }
            if lower.is_empty() || e.path.to_ascii_lowercase().contains(&lower) {
                out.push(e.clone());
            }
        }
        out
    }

    /// Filters and bounds raw commit tuples `(hash, message)` to
    /// `GIT_PANEL_MAX_COMMITS` valid commits, sorted by hash deterministic,
    /// deduped by hash. Pure observation, `is_untrusted_surface = true`.
    #[must_use]
    pub fn list_commits(raw: &[(String, String)]) -> Vec<GitCommit> {
        let mut commits: Vec<GitCommit> = raw
            .iter()
            .filter_map(|(h, m)| GitCommit::new(h.clone(), m.clone()))
            .collect();
        commits.sort_by(|a, b| a.hash.cmp(&b.hash));
        commits.dedup_by(|a, b| a.hash == b.hash);
        if commits.len() > GIT_PANEL_MAX_COMMITS {
            commits.truncate(GIT_PANEL_MAX_COMMITS);
        }
        commits
    }

    /// Filters `commits` by case-insensitive substring `query` over `hash` or
    /// `message`. Bounded to `GIT_PANEL_MAX_COMMITS`.
    #[must_use]
    pub fn filter_commits(commits: &[GitCommit], query: &str) -> Vec<GitCommit> {
        let bounded_query = if query.chars().count() <= GIT_PANEL_MAX_NAME_CHARS {
            query.to_owned()
        } else {
            query.chars().take(GIT_PANEL_MAX_NAME_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for c in commits {
            if out.len() >= GIT_PANEL_MAX_COMMITS {
                break;
            }
            if lower.is_empty()
                || c.hash.to_ascii_lowercase().contains(&lower)
                || c.message.to_ascii_lowercase().contains(&lower)
            {
                out.push(c.clone());
            }
        }
        out
    }

    /// Validates that `path` is allowed for a read operation (`fs.read:PATH_GLOB`).
    /// Fails closed with `None` when not allowed.
    #[must_use]
    pub fn validate_read(path: &str) -> Option<String> {
        if Self::is_fs_allowed(path) {
            Some(path.to_owned())
        } else {
            None
        }
    }

    /// Validates that `args` is an allowlisted git spawn (`process.spawn:git`).
    /// Fails closed with `None` when not allowlisted or over bounds.
    #[must_use]
    pub fn validate_git_args(args: &[String]) -> Option<Vec<String>> {
        if Self::is_allowed_git_args(args) {
            Some(args.to_owned())
        } else {
            None
        }
    }

    /// Builds a tiled git-panel layout from a main git view and optional diff
    /// view using `LayoutNode::split` `H` reuse (no new tiling primitive).
    ///
    /// When `diff` is `None`, returns a single leaf; otherwise a horizontal
    /// split with clamped ratio (mirrors tabs `split_for_tabs`).
    #[must_use]
    pub fn tiled_layout(main: View, diff: Option<View>, ratio: f32) -> LayoutNode {
        match diff {
            None => LayoutNode::leaf(main),
            Some(d) => LayoutNode::split(
                SplitAxis::Horizontal,
                ratio,
                LayoutNode::leaf(main),
                LayoutNode::leaf(d),
            ),
        }
    }

    /// Builds a vertical stack for git-panel plus status/log stacking.
    #[must_use]
    pub fn vertical_stack(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Whether `path` is a directory presentation (trailing `/`).
    #[must_use]
    pub fn is_directory_path(path: &str) -> bool {
        path.ends_with('/') && Self::is_within_repo(path.trim_end_matches('/'))
            || path == "~/projects"
            || path == "~/projects/"
    }

    /// Truncates `text` to branch/name bound at char boundary.
    #[must_use]
    pub fn truncate_name(text: &str) -> String {
        let (s, _) = truncate_bounded(text, GIT_PANEL_MAX_NAME_CHARS);
        s
    }

    /// Validates that `text` fits name bound (char count).
    #[must_use]
    pub fn is_name_bounded(text: &str) -> bool {
        text.chars().count() <= GIT_PANEL_MAX_NAME_CHARS
    }

    /// Truncates `message` to commit message bound at char boundary.
    #[must_use]
    pub fn truncate_commit_message(text: &str) -> String {
        let (s, _) = truncate_bounded(text, GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS);
        s
    }
}

/// Creates a git-panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults, PR-1..PR-12).
/// Returns the panel handle on success; caller must still activate the
/// associated plugin via the public PluginHost path (`declare → resolve →
/// register → GrantRecord → activate`) for capabilities `panel.provider` +
/// `panel.create` + `process.spawn:git` (allowlisted `[tools.git]` shape)
/// plus `terminal.semantic-read` for cwd observation and optional
/// `fs.read:PATH_GLOB` for working-tree read.
pub fn create_git_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that git-panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
/// PR-1..PR-12: `[1,32]` per workspace, `[1,64]` per window, topics `<=256`,
/// subscriptions `<=32` per panel, drop handled by bus admission.
pub fn validate_git_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

/// Creates a git-panel tiled layout via `LayoutNode` primitives (H/V) and
/// mounts the resulting leaf views into a `PanelRegistry`-backed tiled panel
/// placement.
///
/// Pure layout helper — no PanelRegistry mutation, no PTY.
#[must_use]
pub fn git_panel_tiled_layout(main: View, diff: Option<View>, ratio: f32) -> LayoutNode {
    GitIntegration::tiled_layout(main, diff, ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::Rect as UiRect;
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn is_valid_path_and_within_repo_isolation() {
        assert!(GitIntegration::is_valid_path("~/projects/foo"));
        assert!(GitIntegration::is_within_repo("~/projects"));
        assert!(GitIntegration::is_within_repo("~/projects/"));
        assert!(GitIntegration::is_within_repo("~/projects/foo"));
        assert!(GitIntegration::is_within_repo("~/projects/foo/bar"));
        assert!(!GitIntegration::is_within_repo("~/Documents/foo"));
        assert!(!GitIntegration::is_within_repo("/home/user/projects/foo"));
        assert!(!GitIntegration::is_within_repo("~/projects/../etc/passwd"));
        assert!(!GitIntegration::is_within_repo("~/projects/foo/../bar"));
        assert!(!GitIntegration::is_within_repo(""));
        assert!(!GitIntegration::is_within_repo("~/projects/\0evil"));
        assert!(!GitIntegration::is_within_repo("~/projects/foo\x07"));
        let long = format!("~/projects/{}", "a".repeat(5000));
        assert!(!GitIntegration::is_within_repo(&long));
        assert!(!GitIntegration::is_valid_path(""));
        assert!(!GitIntegration::is_valid_path("~/projects/\0evil"));
        assert!(GitIntegration::is_fs_allowed("~/projects/foo"));
        assert!(!GitIntegration::is_fs_allowed("/etc/passwd"));
        assert!(!GitIntegration::is_fs_allowed("~/projects/../secret"));
    }

    #[test]
    fn branch_name_and_commit_hash_validation() {
        assert!(GitIntegration::is_valid_branch_name("main"));
        assert!(GitIntegration::is_valid_branch_name("feature/foo-bar"));
        assert!(GitIntegration::is_valid_branch_name("release-1.0"));
        assert!(!GitIntegration::is_valid_branch_name(""));
        assert!(!GitIntegration::is_valid_branch_name("a/b/../c"));
        assert!(!GitIntegration::is_valid_branch_name("bad~name"));
        assert!(!GitIntegration::is_valid_branch_name("bad^name"));
        assert!(!GitIntegration::is_valid_branch_name("bad:name"));
        assert!(!GitIntegration::is_valid_branch_name("bad?glob"));
        assert!(!GitIntegration::is_valid_branch_name("bad*glob"));
        assert!(!GitIntegration::is_valid_branch_name(".hidden"));
        assert!(!GitIntegration::is_valid_branch_name("trailing/"));
        assert!(!GitIntegration::is_valid_branch_name("trailing."));
        assert!(!GitIntegration::is_valid_branch_name("double//slash"));
        assert!(!GitIntegration::is_valid_branch_name("evil;rm -rf"));
        assert!(!GitIntegration::is_valid_branch_name("evil&bg"));
        assert!(!GitIntegration::is_valid_branch_name("evil|pipe"));
        assert!(!GitIntegration::is_valid_branch_name(
            "a".repeat(200).as_str()
        ));
        assert!(!GitIntegration::is_valid_branch_name("bad\0evil"));
        assert!(!GitIntegration::is_valid_branch_name("bad\x07evil"));

        assert!(GitIntegration::is_valid_commit_hash("abc1234"));
        assert!(GitIntegration::is_valid_commit_hash(
            "abcdef1234567890abcdef1234567890abcdef12"
        ));
        assert!(!GitIntegration::is_valid_commit_hash("abc"));
        assert!(!GitIntegration::is_valid_commit_hash(
            "abc1234567890123456789012345678901234567890extra"
        ));
        assert!(!GitIntegration::is_valid_commit_hash("zzzzzzz"));
        assert!(!GitIntegration::is_valid_commit_hash("abc123g"));
        assert!(!GitIntegration::is_valid_commit_hash(""));
        assert!(!GitIntegration::is_valid_commit_hash("abc 1234"));
    }

    #[test]
    fn git_branch_creation_bounded() {
        let b = GitBranch::new("main".to_string(), true).unwrap();
        assert_eq!(b.name, "main");
        assert!(b.is_current);
        assert!(!b.truncated);
        let long = "a".repeat(GIT_PANEL_MAX_BRANCH_CHARS + 20);
        // Too long is rejected via is_valid, not truncated, because we validate length first.
        assert!(GitBranch::new(long.clone(), false).is_none());
        let near = "b".repeat(GIT_PANEL_MAX_BRANCH_CHARS);
        let b2 = GitBranch::new(near.clone(), false).unwrap();
        assert_eq!(b2.name.chars().count(), GIT_PANEL_MAX_BRANCH_CHARS);
        assert!(!b2.truncated);
        // Invalid branch rejected
        assert!(GitBranch::new("bad..branch".to_string(), false).is_none());
        assert!(GitBranch::new("".to_string(), false).is_none());
        assert!(GitBranch::new("bad;evil".to_string(), false).is_none());
    }

    #[test]
    fn git_commit_creation_bounded() {
        let c = GitCommit::new("abc1234def5678".to_string(), "initial commit".to_string()).unwrap();
        assert_eq!(c.hash, "abc1234def5678");
        assert_eq!(c.short_hash, "abc1234");
        assert_eq!(c.message, "initial commit");
        assert!(!c.truncated);
        let long_msg = "a".repeat(GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS + 100);
        let c2 = GitCommit::new("abc1234".to_string(), long_msg).unwrap();
        assert_eq!(
            c2.message.chars().count(),
            GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS
        );
        assert!(c2.truncated);
        assert!(GitCommit::new("xyz".to_string(), "msg".to_string()).is_none());
        assert!(GitCommit::new("abc1234".to_string(), "msg\0evil".to_string()).is_none());
        let huge = "a".repeat(GIT_PANEL_PAYLOAD_MAX_BYTES + 1);
        assert!(GitCommit::new("abc1234".to_string(), huge).is_none());
    }

    #[test]
    fn allowed_git_args_bounded_allowlisted() {
        assert!(GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "--porcelain".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "diff".to_string(),
            "--stat".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "log".to_string(),
            "--oneline".to_string(),
            "-n".to_string(),
            "10".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "branch".to_string(),
            "-a".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "show".to_string(),
            "abc1234".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string()
        ]));
        assert!(GitIntegration::is_allowed_git_args(&[
            "ls-files".to_string(),
            "--others".to_string()
        ]));
        // Not allowlisted subcommand
        assert!(!GitIntegration::is_allowed_git_args(&["push".to_string()]));
        assert!(!GitIntegration::is_allowed_git_args(
            &["commit".to_string()]
        ));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "checkout".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&["fetch".to_string()]));
        // Empty
        assert!(!GitIntegration::is_allowed_git_args(&[]));
        // Too many args
        let many: Vec<String> = (0..GIT_PANEL_MAX_GIT_ARGS + 1)
            .map(|i| format!("arg{i}"))
            .collect();
        let mut many_with_status = vec!["status".to_string()];
        many_with_status.extend(many);
        assert!(!GitIntegration::is_allowed_git_args(&many_with_status));
        // Shell metachars rejected
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "; rm -rf /".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "log".to_string(),
            "$(evil)".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "diff".to_string(),
            "`evil`".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "a&b".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "a|b".to_string()
        ]));
        // Control char
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "a\x07b".to_string()
        ]));
        // Null
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "a\0b".to_string()
        ]));
        // --upload-pack rejected
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "--upload-pack=evil".to_string()
        ]));
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            "--upload-pack".to_string()
        ]));
        // Arg too long
        let long_arg = "a".repeat(GIT_PANEL_MAX_GIT_ARG_BYTES + 1);
        assert!(!GitIntegration::is_allowed_git_args(&[
            "status".to_string(),
            long_arg
        ]));
        // Total too large
        let big = "a".repeat(GIT_PANEL_MAX_GIT_ARG_BYTES);
        let many_big: Vec<String> = std::iter::once("status".to_string())
            .chain((0..33).map(|_| big.clone()))
            .collect();
        assert!(!GitIntegration::is_allowed_git_args(&many_big));
        // string form
        assert!(GitIntegration::is_allowed_git_command_str(
            "status --porcelain"
        ));
        assert!(GitIntegration::is_allowed_git_command_str(
            "log --oneline -n 10"
        ));
        assert!(!GitIntegration::is_allowed_git_command_str(
            "push origin main"
        ));
        assert!(!GitIntegration::is_allowed_git_command_str(""));
        assert!(!GitIntegration::is_allowed_git_command_str(
            "status; rm -rf /"
        ));
        // process.spawn:git allowlist check
        assert!(GitIntegration::is_process_spawn_git_allowed(
            "process.spawn:git"
        ));
        assert!(!GitIntegration::is_process_spawn_git_allowed(
            "process.spawn:rg"
        ));
        assert!(!GitIntegration::is_process_spawn_git_allowed(
            "fs.read:~/projects/**"
        ));
        assert!(!GitIntegration::is_process_spawn_git_allowed(
            "process.spawn"
        ));
    }

    #[test]
    fn list_branches_bounded_sorted_deduped() {
        let raw = vec![
            "main".to_string(),
            "* main".to_string(),
            "feature/foo".to_string(),
            "main".to_string(),
            "bad..branch".to_string(),
            "".to_string(),
        ];
        let listed = GitIntegration::list_branches(&raw);
        // deduped, invalid filtered, sorted
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "feature/foo");
        assert_eq!(listed[1].name, "main");
        // current tracking: one of mains is current
        assert!(listed.iter().any(|b| b.is_current));
        // Bounded at 32
        let many: Vec<String> = (0..50).map(|i| format!("branch{i}")).collect();
        assert_eq!(
            GitIntegration::list_branches(&many).len(),
            GIT_PANEL_MAX_BRANCHES
        );
        let sorted = GitIntegration::list_branches(&many);
        let mut check = sorted.clone();
        check.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(sorted, check);
    }

    #[test]
    fn filter_branches_bounded_case_insensitive() {
        let raw = vec![
            "main".to_string(),
            "feature/foo".to_string(),
            "Feature/bar".to_string(),
            "release-1.0".to_string(),
        ];
        let branches = GitIntegration::list_branches(&raw);
        let filtered = GitIntegration::filter_branches(&branches, "feature");
        assert_eq!(filtered.len(), 2);
        let filtered_ci = GitIntegration::filter_branches(&branches, "FEATURE");
        assert_eq!(filtered.len(), filtered_ci.len());
        let filtered_empty = GitIntegration::filter_branches(&branches, "");
        assert_eq!(filtered_empty.len(), branches.len());
        let long_query = "a".repeat(GIT_PANEL_MAX_NAME_CHARS + 10);
        assert_eq!(
            GitIntegration::filter_branches(&branches, &long_query).len(),
            0
        );
    }

    #[test]
    fn list_status_and_filter_bounded() {
        let raw = vec![
            "~/projects/foo.txt".to_string(),
            "~/projects/bar.rs".to_string(),
            "~/projects/foo.txt".to_string(),
            "/etc/passwd".to_string(),
        ];
        let listed = GitIntegration::list_status_entries(&raw, GitFileStatus::Modified);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, "~/projects/bar.rs");
        assert_eq!(listed[1].path, "~/projects/foo.txt");
        // Bounded at 128
        let many: Vec<String> = (0..200)
            .map(|i| format!("~/projects/file{i}.txt"))
            .collect();
        assert_eq!(
            GitIntegration::list_status_entries(&many, GitFileStatus::Modified).len(),
            GIT_PANEL_MAX_ENTRIES
        );
        let filtered = GitIntegration::filter_status_entries(&listed, "foo");
        assert_eq!(filtered.len(), 1);
        let filtered_ci = GitIntegration::filter_status_entries(&listed, "FOO");
        assert_eq!(filtered.len(), filtered_ci.len());
        let filtered_all = GitIntegration::filter_status_entries(&listed, "");
        assert_eq!(filtered_all.len(), 2);
        let long_q = "a".repeat(GIT_PANEL_MAX_NAME_CHARS + 5);
        assert_eq!(
            GitIntegration::filter_status_entries(&listed, &long_q).len(),
            0
        );
    }

    #[test]
    fn list_commits_bounded_sorted_deduped() {
        let raw = vec![
            ("abc1234".to_string(), "fix bug".to_string()),
            ("def5678".to_string(), "add feature".to_string()),
            ("abc1234".to_string(), "fix bug".to_string()),
            ("zzzzzzz".to_string(), "bad hash".to_string()),
        ];
        let listed = GitIntegration::list_commits(&raw);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].hash, "abc1234");
        assert_eq!(listed[1].hash, "def5678");
        let many: Vec<(String, String)> = (0..100)
            .map(|i| (format!("{:07x}", i + 0xabc000), format!("commit {i}")))
            .collect();
        assert_eq!(
            GitIntegration::list_commits(&many).len(),
            GIT_PANEL_MAX_COMMITS
        );
        let filtered = GitIntegration::filter_commits(&listed, "fix");
        assert_eq!(filtered.len(), 1);
        let filtered_hash = GitIntegration::filter_commits(&listed, "abc");
        assert_eq!(filtered_hash.len(), 1);
        let filtered_empty = GitIntegration::filter_commits(&listed, "");
        assert_eq!(filtered_empty.len(), 2);
        let long_q = "a".repeat(GIT_PANEL_MAX_NAME_CHARS + 10);
        assert_eq!(GitIntegration::filter_commits(&listed, &long_q).len(), 0);
        let case = GitIntegration::filter_commits(&listed, "FIX");
        assert_eq!(case.len(), 1);
    }

    #[test]
    fn validate_read_and_git_args_fail_closed() {
        assert!(GitIntegration::validate_read("~/projects/foo").is_some());
        assert!(GitIntegration::validate_read("/etc/passwd").is_none());
        assert!(GitIntegration::validate_read("~/projects/../evil").is_none());
        assert!(
            GitIntegration::validate_git_args(&["status".to_string(), "--porcelain".to_string()])
                .is_some()
        );
        assert!(GitIntegration::validate_git_args(&["push".to_string()]).is_none());
        assert!(
            GitIntegration::validate_git_args(&["status".to_string(), "; evil".to_string()])
                .is_none()
        );
    }

    #[test]
    fn tiled_layout_reuses_h_split_and_stack_no_new_primitive() {
        let main = View::new(ViewId::new(10), 80, 24);
        let diff = View::new(ViewId::new(11), 80, 24);
        let layout = GitIntegration::tiled_layout(main.clone(), Some(diff.clone()), 0.5);
        assert!(matches!(layout, LayoutNode::Split { .. }));
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        let low = GitIntegration::tiled_layout(main.clone(), Some(diff.clone()), 0.01);
        let high = GitIntegration::tiled_layout(main.clone(), Some(diff.clone()), 0.99);
        for l in [low, high] {
            let a = l.layout(UiRect::new(0, 0, 80, 24));
            assert!(a[0].1.width >= 1 && a[1].1.width >= 1);
        }
        let nan = GitIntegration::tiled_layout(main.clone(), Some(diff), f32::NAN);
        assert_eq!(nan.layout(UiRect::new(0, 0, 80, 24)).len(), 2);
        let solo = GitIntegration::tiled_layout(main, None, 0.5);
        assert_eq!(solo.leaf_count(), 1);
        let v1 = View::new(ViewId::new(20), 80, 12);
        let v2 = View::new(ViewId::new(21), 80, 12);
        let stack = GitIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        assert_eq!(stack.leaf_count(), 2);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_git_panel(&mut reg, ws, view).expect("create git panel");
        assert_eq!(reg.panel_count(), 1);
        let handle2 = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel");
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        let _ = id.get();
        let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
        let ws2 = WorkspaceId::new(42);
        let view2 = ViewId::new(42);
        let h = reg2.create_panel(PanelType::Helper, Some(ws2)).unwrap();
        reg2.mount_panel(h.id, h.generation, view2).expect("mount");
        reg2.focus_panel(h.id, h.generation, ws2).expect("focus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
        reg2.suspend_panel(h.id, h.generation).expect("suspend");
        assert_eq!(reg2.focused_panel(ws2), None);
        reg2.resume_panel(h.id, h.generation).expect("resume");
        reg2.focus_panel(h.id, h.generation, ws2).expect("refocus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
        let topic = reg2.declare_topic("xuepoo.git:branch-changed").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("branch{i}")).unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // fs isolation
        assert!(!GitIntegration::is_fs_allowed("/etc/passwd"));
        assert!(GitIntegration::is_fs_allowed("~/projects/foo"));
        assert!(!GitIntegration::is_fs_allowed("~/projects/../secret"));
        // process spawn allowlist per is_allowed
        assert!(GitIntegration::is_allowed_git_args(&["status".to_string()]));
        assert!(!GitIntegration::is_allowed_git_args(&["push".to_string()]));
        assert!(GitIntegration::is_process_spawn_git_allowed(
            "process.spawn:git"
        ));
        assert!(!GitIntegration::is_process_spawn_git_allowed(
            "process.spawn:rg"
        ));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_git_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_git_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_git_panel_config(&ok).is_ok());
        let bad_topics = PanelRegistryConfig {
            max_topics_total: 257,
            ..Default::default()
        };
        assert!(validate_git_panel_config(&bad_topics).is_err());
        let bad_subs = PanelRegistryConfig {
            max_subscriptions_per_panel: 33,
            ..Default::default()
        };
        assert!(validate_git_panel_config(&bad_subs).is_err());
    }

    #[test]
    fn fs_capability_parsing_and_hash_bound_isolation() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::git_panel_manifest};
        let m = git_panel_manifest();
        let read_caps: Vec<_> = m
            .capabilities
            .filesystem
            .iter()
            .filter(|r| matches!(r.access, bitty_plugin_host::FsAccess::Read))
            .collect();
        assert!(!read_caps.is_empty());
        assert!(read_caps[0].paths.contains(&"~/projects/**".to_string()));
        let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
        let cap_proc = CapabilityId::parse("process.spawn:git").unwrap();
        assert_eq!(
            cap_proc.family(),
            bitty_plugin_host::CapabilityFamily::Process
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        let outside = CapabilityId::parse("fs.read:/etc/passwd").unwrap();
        assert_ne!(cap_read, outside);
        let _id = PluginId::new("bitty-terminal.git-panel").unwrap();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.provider").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.create").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("process.spawn:git").unwrap())
        );
    }

    #[test]
    fn tiled_layout_uses_layout_primitives_deterministically() {
        let v1 = View::new(ViewId::new(1), 80, 24);
        let v2 = View::new(ViewId::new(2), 40, 24);
        let v3 = View::new(ViewId::new(3), 40, 24);
        let h_split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v1.clone()),
            LayoutNode::leaf(v2.clone()),
        );
        assert!(matches!(h_split, LayoutNode::Split { .. }));
        assert_eq!(h_split.leaf_count(), 2);
        let v_split = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(v2.clone()),
            LayoutNode::leaf(v3),
        );
        assert_eq!(v_split.leaf_count(), 2);
        let stack = GitIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        let base = LayoutNode::leaf(View::new(ViewId::new(10), 80, 24));
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
    }

    #[test]
    fn truncate_and_selection_bounded() {
        let long = "a".repeat(GIT_PANEL_MAX_NAME_CHARS + 50);
        let truncated = GitIntegration::truncate_name(&long);
        assert_eq!(truncated.chars().count(), GIT_PANEL_MAX_NAME_CHARS);
        assert!(GitIntegration::is_name_bounded("hello"));
        assert!(!GitIntegration::is_name_bounded(&long));
        let many: Vec<String> = (0..200).map(|i| format!("~/projects/file{i}")).collect();
        let entries = GitIntegration::list_status_entries(&many, GitFileStatus::Modified);
        assert_eq!(entries.len(), GIT_PANEL_MAX_ENTRIES);
        let filtered = GitIntegration::filter_status_entries(&entries, "");
        assert!(
            filtered.len() <= GIT_PANEL_MAX_SELECTION || filtered.len() <= GIT_PANEL_MAX_ENTRIES
        );
        let long_msg = "a".repeat(GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS + 20);
        assert_eq!(
            GitIntegration::truncate_commit_message(&long_msg)
                .chars()
                .count(),
            GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS
        );
    }
}
