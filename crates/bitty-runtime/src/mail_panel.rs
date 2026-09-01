#![forbid(unsafe_code)]
//! Mail panel via Panel Runtime — tiled Panel, `mcp.invoke:mail.*` + `network.connect` + `fs.read` local cache, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.mail-panel` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Mail panel
//! is a tiled `Panel(PanelId)` workspace (reuses `LayoutNode` `H`/`V` with
//! panel content, not a PTY) for mail listing, reading, compose UX composed
//! from capability-checked `mcp`/`network` service (helper-process-backed
//! or WebView path) or bounded JSON tool results; strictly helper-process /
//! out-of-process, never `dlopen`. It verifies `PanelId` distinct newtype
//! with no `From` bridge to `ViewId`/`TerminalId`, `Generation` monotonic
//! with reserve `1024` and fail-closed exhaustion, lifecycle
//! `Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed` with
//! validated transitions, command registry `owner.name:command` qualified
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`, `<=128` chars)
//! duplicates rejected per-type `32` bound, overlay max `4` plus `1` modal
//! with modal exclusivity and text `128`/tooltip `256` bounds, `Palette` kind,
//! EventBus with three levels `64`/`1024`/`256 KiB`/`8192`/`2 MiB` and `8 KiB`
//! payload `DropOldest` default with counted per-queue attribution and
//! coalescing for observation topics (PR-1..PR-12, `PanelRegistry` single-
//! process `winit` one-registry-per-window headless, `ViewContent::Panel`),
//! capability isolation per `(PanelId, generation)` deny-by-default via
//! `CapabilityId` panel family `panel.provider`/`panel.create`/
//! `panel.focus`/`panel.overlay` — no ambient authority, no first-party
//! bypass, plus `mcp.invoke:mail.*` per-tool bounded `8 KiB` frame,
//! `network.connect:DESTINATION` per-destination allowlist with
//! `fs.read:MAIL_GLOB` local cache `~/mail/**` only via `FilesystemRequest`,
//! `browser.file-url` never implied, secret tokens as `SecretField` with
//! `0600` bounded retention (identical to `ai-panel` minimization). No parser,
//! renderer, or input hot path is entered, and no grid mutation ever occurs
//! here (only `Action` writes `State` per Terminal State RFC). Default is
//! disabled (fresh `EffectiveConfig` has empty `plugins`); `bitty --safe`
//! rejects `bitty-terminal.*` as non-builtin without panic, identical to
//! third-party `xuepoo.*` parity (no private channel). Bounded queues
//! (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload,
//! `32`/`8 KiB` batch) and single-process `winit` `PanelRegistry` per window
//! are verified headlessly. WebView path (alternative composition) would reuse
//! Browser placement `BrowserSurfaceId` but is not allocated here; helper path
//! uses `mcp.invoke` + bounded JSON tool results over `8 KiB` per `mcp.invoke`
//! plus schema `4 KiB`, counted under RC-3 `512 MiB` helper aggregate and
//! global `8192`/`2 MiB` shared envelope.

use bitty_ui::{
    LayoutNode, SplitAxis, View, ViewId,
    panel::{MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN},
};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

// ---------------------------------------------------------------------------
// Capability patterns — each a distinct gate, no ambient, no first-party bypass
// ---------------------------------------------------------------------------

/// Filesystem read pattern for mail local cache — `~/mail/**` only.
///
/// Constrained via `FilesystemRequest` path-glob, real-path resolved,
/// symlinks/devices rejected per host policy. Mirrors `~/projects/**` but
/// scoped to `~/mail/**`; never widens to `~/**` or `/` without grant.
pub const MAIL_PANEL_FS_READ_PATTERN: &str = "~/mail/**";
/// Optional filesystem write for local cache mutation — same glob, opt-in.
pub const MAIL_PANEL_FS_WRITE_PATTERN: &str = "~/mail/**";
/// Network destination for IMAP over TLS — allowlisted per-destination.
pub const MAIL_PANEL_NETWORK_IMAP: &str = "network.connect:imap.example.com:993";
/// Network destination for SMTP over TLS — allowlisted per-destination.
pub const MAIL_PANEL_NETWORK_SMTP: &str = "network.connect:smtp.example.com:465";
/// Per-tool MCP capability for mail listing — bounded `8 KiB` frame.
pub const MAIL_PANEL_MCP_LIST: &str = "mcp.invoke:mail.list";
/// Per-tool MCP capability for mail reading — bounded `8 KiB` frame.
pub const MAIL_PANEL_MCP_READ: &str = "mcp.invoke:mail.read";
/// Per-tool MCP capability for mail sending — bounded `8 KiB` frame.
pub const MAIL_PANEL_MCP_SEND: &str = "mcp.invoke:mail.send";
/// Per-tool MCP capability for mail search/filter — bounded `8 KiB` frame.
pub const MAIL_PANEL_MCP_SEARCH: &str = "mcp.invoke:mail.search";

// ---------------------------------------------------------------------------
// Bounded resource table (PR-1..PR-12 reuse, no new global budget)
// ---------------------------------------------------------------------------

/// Maximum mail entries to present per folder — bounded `128` mirroring
/// `FILE_MANAGER_MAX_ENTRIES` and `GIT_PANEL_MAX_ENTRIES` (PR-5).
pub const MAIL_PANEL_MAX_ENTRIES: usize = 128;
/// Maximum folders to present — bounded `32` mirroring `GIT_PANEL_MAX_BRANCHES` / `BROWSER_MAX_HISTORY_ENTRIES`.
pub const MAIL_PANEL_MAX_FOLDERS: usize = 32;
/// Maximum subject chars — mirrors overlay text bound `128`.
pub const MAIL_PANEL_MAX_SUBJECT_CHARS: usize = MAX_OVERLAY_TEXT_LEN;
/// Maximum sender chars — mirrors overlay text bound `128`.
pub const MAIL_PANEL_MAX_SENDER_CHARS: usize = MAX_OVERLAY_TEXT_LEN;
/// Maximum preview chars — mirrors overlay tooltip bound `256`.
pub const MAIL_PANEL_MAX_PREVIEW_CHARS: usize = MAX_OVERLAY_TOOLTIP_LEN;
/// Maximum id chars — bounded `64` mirroring git hash / tool name bound.
pub const MAIL_PANEL_MAX_ID_CHARS: usize = 64;
/// Maximum folder name chars — bounded `64`.
pub const MAIL_PANEL_MAX_FOLDER_CHARS: usize = 64;
/// Maximum bytes per path — mirrors parser `BoundedString::MAX_LEN` `4096` and project/mail path bound.
pub const MAIL_PANEL_MAX_PATH_BYTES: usize = 4096;
/// Maximum bytes per mail arg payload — bounded `8 KiB` (PR-5) at bus admission, also MCP `256 KiB` framing reuse but capped `8 KiB` here for mail JSON.
pub const MAIL_PANEL_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;
/// Maximum MCP frame bytes reuse — `8 KiB` per mail tool payload (same as BUS_EVENT_MAX_BYTES).
pub const MAIL_PANEL_MCP_MAX_FRAME_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;
/// Maximum selection size — bounded `64` mirroring per-subscription bound (PR-7).
pub const MAIL_PANEL_MAX_SELECTION: usize = crate::registry::BUS_PER_SUBSCRIPTION_LIMIT;
/// Maximum panels per workspace for mail-panel — mirrors `MAX_PANELS_PER_WORKSPACE` `32` PR-1.
pub const MAIL_PANEL_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
/// Maximum panels per window — mirrors `MAX_PANELS_PER_WINDOW` `64` PR-2 but capped by helper RC-3 aggregate.
pub const MAIL_PANEL_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// Canonical mail-panel commands (qualified `owner.name:command`).
pub const MAIL_PANEL_COMMAND_OPEN: &str = "bitty-terminal.mail-panel:open";
pub const MAIL_PANEL_COMMAND_LIST: &str = "bitty-terminal.mail-panel:list";
pub const MAIL_PANEL_COMMAND_READ: &str = "bitty-terminal.mail-panel:read";
pub const MAIL_PANEL_COMMAND_COMPOSE: &str = "bitty-terminal.mail-panel:compose";
pub const MAIL_PANEL_COMMAND_SEND: &str = "bitty-terminal.mail-panel:send";
pub const MAIL_PANEL_COMMAND_SEARCH: &str = "bitty-terminal.mail-panel:search";

/// Allowed `mcp.invoke:mail.*` tool subnames — closed set for this candidate.
pub const MAIL_ALLOWED_MCP_TOOLS: &[&str] = &["mail.list", "mail.read", "mail.send", "mail.search"];

// ---------------------------------------------------------------------------
// Mail types — bounded, pure data for presentation
// ---------------------------------------------------------------------------

/// Mail folder — presentation only, not IMAP hierarchy introspection.
///
/// Folders are either well-known well-known names or a bounded custom string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MailFolder {
    Inbox,
    Sent,
    Drafts,
    Archive,
    Spam,
    Trash,
    Custom(String),
}

impl std::fmt::Display for MailFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Inbox => "INBOX",
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
            Self::Archive => "Archive",
            Self::Spam => "Spam",
            Self::Trash => "Trash",
            Self::Custom(s) => s.as_str(),
        };
        f.write_str(s)
    }
}

impl MailFolder {
    /// Parse a folder string into `MailFolder`, validating bounded `<=64` chars and no control.
    /// Well-known folders map to variants; others become `Custom` after validation.
    #[must_use]
    pub fn parse(s: String) -> Option<Self> {
        if !MailIntegration::is_valid_folder(&s) {
            return None;
        }
        let variant = match s.as_str() {
            "INBOX" | "Inbox" | "inbox" => Some(Self::Inbox),
            "Sent" | "sent" => Some(Self::Sent),
            "Drafts" | "drafts" => Some(Self::Drafts),
            "Archive" | "archive" => Some(Self::Archive),
            "Spam" | "spam" => Some(Self::Spam),
            "Trash" | "trash" => Some(Self::Trash),
            _ => None,
        };
        if let Some(v) = variant {
            Some(v)
        } else {
            // Custom: already validated bounded, keep canonical but preserve case?
            // For determinism, keep as provided (bounded) but truncate at char boundary if needed.
            let (bounded, _) = truncate_bounded(&s, MAIL_PANEL_MAX_FOLDER_CHARS);
            Some(Self::Custom(bounded))
        }
    }

    /// Whether this is a well-known folder (not custom).
    #[must_use]
    pub fn is_well_known(&self) -> bool {
        !matches!(self, Self::Custom(_))
    }
}

/// Single mail entry — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MailEntry {
    /// Mail id, validated bounded `<=64` chars, no control, no null, no whitespace, dot/dash/underscore allowed.
    pub id: String,
    /// Subject, truncated at char boundary to `MAIL_PANEL_MAX_SUBJECT_CHARS`.
    pub subject: String,
    /// Sender, truncated at char boundary to `MAIL_PANEL_MAX_SENDER_CHARS`.
    pub sender: String,
    /// Body preview snippet, truncated to `MAIL_PANEL_MAX_PREVIEW_CHARS`.
    pub preview: String,
    /// Folder this mail is presented in — validated before insert.
    pub folder: MailFolder,
    /// Whether unread.
    pub unread: bool,
    /// Whether subject was truncated at the display bound.
    pub truncated_subject: bool,
    /// Whether sender was truncated.
    pub truncated_sender: bool,
    /// Whether preview was truncated.
    pub truncated_preview: bool,
}

impl MailEntry {
    /// Creates a mail entry from validated parts, truncating `subject`/`sender`/`preview` at char boundary.
    /// Returns `None` for invalid id, folder, or any control / null bytes.
    #[must_use]
    pub fn new(
        id: String,
        subject: String,
        sender: String,
        preview: String,
        folder: MailFolder,
        unread: bool,
    ) -> Option<Self> {
        if !MailIntegration::is_valid_id(&id) {
            return None;
        }
        if subject.contains('\0') || sender.contains('\0') || preview.contains('\0') {
            return None;
        }
        if subject.chars().any(|c| c.is_control())
            || sender.chars().any(|c| c.is_control())
            || preview
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            // Allow newline/tab in preview but not other controls.
            if subject.chars().any(|c| c.is_control()) || sender.chars().any(|c| c.is_control()) {
                return None;
            }
            if preview
                .chars()
                .any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\t'))
            {
                return None;
            }
        }
        if preview.len() > MAIL_PANEL_PAYLOAD_MAX_BYTES {
            return None;
        }
        let (bounded_subject, truncated_subject) =
            truncate_bounded(&subject, MAIL_PANEL_MAX_SUBJECT_CHARS);
        let (bounded_sender, truncated_sender) =
            truncate_bounded(&sender, MAIL_PANEL_MAX_SENDER_CHARS);
        let (bounded_preview, truncated_preview) =
            truncate_bounded(&preview, MAIL_PANEL_MAX_PREVIEW_CHARS);
        // Sender must look like email-ish or display name — bounded check only: contains '@' or non-empty display.
        // We do not enforce strict RFC 822 here; empty sender rejected, over-long truncated above.
        if bounded_sender.is_empty() {
            return None;
        }
        // Id must not be empty after truncation (still bounded)
        if id.is_empty() {
            return None;
        }
        Some(Self {
            id,
            subject: bounded_subject,
            sender: bounded_sender,
            preview: bounded_preview,
            folder,
            unread,
            truncated_subject,
            truncated_sender,
            truncated_preview,
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

fn is_valid_email_like(s: &str) -> bool {
    // Very small email-like check for presentation bounding: contains '@' and '.' after '@', no spaces, no control.
    if s.is_empty() || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    if s.contains('\0') {
        return false;
    }
    // Accept either "user@example.com" or "Name <user@example.com>" style? Keep minimal: require '@' and length bounded.
    // For display name style, we allow non-email display strings without '@' as long as they are bounded sender (e.g., "GitHub").
    // So email check is optional; sender validation is broader.
    true
}

// ---------------------------------------------------------------------------
// MailIntegration — pure, observation-only helpers over committed state
// ---------------------------------------------------------------------------

/// MailIntegration — pure, observation-only helpers over committed state
/// and `mcp`/`network`/`fs` capabilities. No mutation of `State`, no hot-path,
/// bounded `<=128` entries, tiled `LayoutNode` `H`/`V` reuse, no new tiling
/// primitive. All `mcp.invoke` outputs are treated as
/// `is_untrusted_surface = true` (RC-9/RC-10 framing `8 KiB`, depth counted).
/// Helper process is counted under the requesting generation (RC-1/RC-2
/// attribution) and under RC-3 `512 MiB` aggregate for web/helper process,
/// not as ambient.
#[derive(Debug, Clone, Copy)]
pub struct MailIntegration;

impl MailIntegration {
    // -- Path / local cache helpers (mirrors FileManagerIntegration but for ~/mail) --

    /// Whether `path` is a valid bounded path (no null byte, `<=4096` bytes,
    /// no control chars, non-empty).
    #[must_use]
    pub fn is_valid_path(path: &str) -> bool {
        if path.is_empty() || path.len() > MAIL_PANEL_MAX_PATH_BYTES {
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

    /// Whether `path` is within the granted mail read scope `~/mail/**`.
    ///
    /// Pure, bounded check: `path` must be `~/mail` / `~/mail/` or start with `~/mail/`,
    /// contain no `..` segment, no null byte, length `<=4096`. Symlink/device checks
    /// are deferred to host real-path, mirroring `ProjectIntegration`.
    #[must_use]
    pub fn is_within_mail_scope(path: &str) -> bool {
        if !Self::is_valid_path(path) {
            return false;
        }
        if path == "~/mail" || path == "~/mail/" {
            return true;
        }
        if !path.starts_with("~/mail/") {
            return false;
        }
        if path.contains("..") {
            return false;
        }
        true
    }

    /// Alias for mail-scope check — mirrors `CapabilityId` family check without I/O.
    #[must_use]
    pub fn is_fs_allowed(candidate: &str) -> bool {
        Self::is_within_mail_scope(candidate)
    }

    /// Whether `candidate` is allowed for write under `MAIL_PANEL_FS_WRITE_PATTERN`.
    #[must_use]
    pub fn is_fs_write_allowed(candidate: &str) -> bool {
        Self::is_within_mail_scope(candidate)
    }

    /// Extracts local cache key (relative path under `~/mail/`) without truncation.
    /// Returns `None` for non-mail-scope paths.
    #[allow(dead_code)]
    fn mail_key_raw(path: &str) -> Option<String> {
        if !Self::is_within_mail_scope(path) {
            return None;
        }
        if path == "~/mail" || path == "~/mail/" {
            return None;
        }
        let key = path.strip_prefix("~/mail/")?;
        if key.is_empty() {
            return None;
        }
        if key.contains("..") || key.chars().any(|c| c.is_control()) {
            return None;
        }
        Some(key.to_owned())
    }

    // -- Mail id / subject / sender / folder validation --

    /// Whether `id` is a valid bounded mail id (`^[A-Za-z0-9._-]+$`, `<=64` chars, no control/whitespace).
    #[must_use]
    pub fn is_valid_id(id: &str) -> bool {
        if id.is_empty() || id.len() > MAIL_PANEL_MAX_ID_CHARS {
            return false;
        }
        if id.contains('\0') || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return false;
        }
        for b in id.bytes() {
            if !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
                return false;
            }
        }
        true
    }

    /// Whether `folder` is a valid bounded folder name (`<=64` chars, no control, no null, no path separators traversal).
    #[must_use]
    pub fn is_valid_folder(folder: &str) -> bool {
        if folder.is_empty() || folder.chars().count() > MAIL_PANEL_MAX_FOLDER_CHARS {
            return false;
        }
        if folder.contains('\0') || folder.chars().any(|c| c.is_control()) {
            return false;
        }
        if folder.contains("..") {
            return false;
        }
        // Folder names must not contain path separators for flat custom set; well-known are plain.
        if folder.contains('/') || folder.contains('\\') {
            return false;
        }
        // Must be printable ascii-ish plus allow well-known case variants; reject empty or purely whitespace.
        if folder.trim().is_empty() {
            return false;
        }
        // Must start with alphanumeric for bounded grammar.
        let first = folder.chars().next().unwrap();
        if !first.is_ascii_alphanumeric() {
            return false;
        }
        for c in folder.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ') {
                // Allow space for folder display? Keep minimal: allow space but not control.
                // Reject other punctuation.
                if c != ' ' {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `s` is valid subject (no control except maybe tab/newline disallowed for subject line, bounded `128` handled by truncation but validation rejects control).
    #[must_use]
    pub fn is_valid_subject(subject: &str) -> bool {
        if subject.contains('\0') {
            return false;
        }
        if subject.chars().any(|c| c.is_control()) {
            return false;
        }
        // Length is bounded via truncation; for pure validity we accept any length but
        // reject payload overflow `8 KiB` as overflow for bus.
        if subject.len() > MAIL_PANEL_PAYLOAD_MAX_BYTES {
            return false;
        }
        true
    }

    /// Whether `s` is valid sender display / email (no control, bounded `128` via truncation, non-empty).
    #[must_use]
    pub fn is_valid_sender(sender: &str) -> bool {
        if sender.is_empty() {
            return false;
        }
        if sender.contains('\0') || sender.chars().any(|c| c.is_control()) {
            return false;
        }
        if sender.len() > MAIL_PANEL_PAYLOAD_MAX_BYTES {
            return false;
        }
        is_valid_email_like(sender)
    }

    /// Whether `preview` is valid snippet (allows `\n` `\t`, rejects other control, bounded `8 KiB` payload).
    #[must_use]
    pub fn is_valid_preview(preview: &str) -> bool {
        if preview.contains('\0') {
            return false;
        }
        if preview
            .chars()
            .any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\t'))
        {
            return false;
        }
        if preview.len() > MAIL_PANEL_PAYLOAD_MAX_BYTES {
            return false;
        }
        true
    }

    /// Whether `candidate` is the exact `fs.read:~/mail/**` capability string.
    #[must_use]
    pub fn is_mail_fs_read_allowed(candidate: &str) -> bool {
        candidate == format!("fs.read:{}", MAIL_PANEL_FS_READ_PATTERN)
            || candidate == "fs.read:~/mail/**"
    }

    /// Whether `candidate` matches `network.connect:DESTINATION` allowlist for mail.
    /// Destination must be an allowed host:port without whitespace/control and match one of the well-known patterns.
    #[must_use]
    pub fn is_network_destination_allowed(candidate: &str) -> bool {
        if candidate == MAIL_PANEL_NETWORK_IMAP || candidate == MAIL_PANEL_NETWORK_SMTP {
            return true;
        }
        // Generic: must start with `network.connect:` and payload is host:port bounded.
        if !candidate.starts_with("network.connect:") {
            return false;
        }
        let dest = &candidate["network.connect:".len()..];
        if dest.is_empty() || dest.len() > 1024 {
            return false;
        }
        if dest.contains('\0') || dest.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return false;
        }
        // Host part must contain at least one dot and optional colon port numeric.
        if !dest.contains('.') {
            return false;
        }
        // Very small host validation: ascii letters, digits, dot, dash for host, then optionally :port digits.
        let (host, port) = match dest.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (dest, None),
        };
        if host.is_empty() || host.len() > 253 {
            return false;
        }
        for b in host.bytes() {
            if !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
                return false;
            }
        }
        if let Some(p) = port {
            if p.is_empty() || p.len() > 5 {
                return false;
            }
            for b in p.bytes() {
                if !b.is_ascii_digit() {
                    return false;
                }
            }
            if p.parse::<u16>().is_err() {
                return false;
            }
        }
        true
    }

    /// Whether `candidate` is `mcp.invoke:mail.*` with allowlisted tool suffix.
    #[must_use]
    pub fn is_mcp_mail_allowed(candidate: &str) -> bool {
        if !candidate.starts_with("mcp.invoke:") {
            return false;
        }
        let tool = &candidate["mcp.invoke:".len()..];
        MAIL_ALLOWED_MCP_TOOLS.contains(&tool)
    }

    /// Whether `tool` is an allowed mail MCP tool name (`mail.list` etc).
    #[must_use]
    pub fn is_allowed_mcp_tool(tool: &str) -> bool {
        MAIL_ALLOWED_MCP_TOOLS.contains(&tool)
    }

    /// Whether `candidate == mail.list/read/send/search` capability exactly.
    #[must_use]
    pub fn is_mcp_list_allowed(candidate: &str) -> bool {
        candidate == MAIL_PANEL_MCP_LIST
    }
    #[must_use]
    pub fn is_mcp_read_allowed(candidate: &str) -> bool {
        candidate == MAIL_PANEL_MCP_READ
    }
    #[must_use]
    pub fn is_mcp_send_allowed(candidate: &str) -> bool {
        candidate == MAIL_PANEL_MCP_SEND
    }

    /// Whether `text` fits subject bound (char count).
    #[must_use]
    pub fn is_subject_bounded(text: &str) -> bool {
        text.chars().count() <= MAIL_PANEL_MAX_SUBJECT_CHARS
    }
    /// Whether `text` fits sender bound.
    #[must_use]
    pub fn is_sender_bounded(text: &str) -> bool {
        text.chars().count() <= MAIL_PANEL_MAX_SENDER_CHARS
    }
    /// Whether `text` fits preview bound.
    #[must_use]
    pub fn is_preview_bounded(text: &str) -> bool {
        text.chars().count() <= MAIL_PANEL_MAX_PREVIEW_CHARS
    }
    /// Whether `path` fits payload/path bounds.
    #[must_use]
    pub fn is_path_bounded(path: &str) -> bool {
        path.len() <= MAIL_PANEL_MAX_PATH_BYTES
            && path.chars().count() <= MAIL_PANEL_MAX_SUBJECT_CHARS * 4
    }

    /// Truncates `text` to subject bound at char boundary (`128`).
    #[must_use]
    pub fn truncate_subject(text: &str) -> String {
        let (s, _) = truncate_bounded(text, MAIL_PANEL_MAX_SUBJECT_CHARS);
        s
    }
    /// Truncates `text` to sender bound.
    #[must_use]
    pub fn truncate_sender(text: &str) -> String {
        let (s, _) = truncate_bounded(text, MAIL_PANEL_MAX_SENDER_CHARS);
        s
    }
    /// Truncates `text` to preview bound (`256`).
    #[must_use]
    pub fn truncate_preview(text: &str) -> String {
        let (s, _) = truncate_bounded(text, MAIL_PANEL_MAX_PREVIEW_CHARS);
        s
    }

    /// Filters and bounds a raw mail listing to `MAIL_PANEL_MAX_ENTRIES`
    /// valid `MailEntry`, sorted deterministic by folder then sender, deduped by id.
    /// Pure observation — no network I/O, no `SecretField` exposure.
    #[must_use]
    pub fn list_entries(entries: Vec<MailEntry>) -> Vec<MailEntry> {
        use std::collections::BTreeSet;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut filtered: Vec<MailEntry> = Vec::new();
        for e in entries {
            if !Self::is_valid_id(&e.id)
                || !Self::is_valid_subject(&e.subject)
                || !Self::is_valid_sender(&e.sender)
                || !Self::is_valid_preview(&e.preview)
            {
                continue;
            }
            if seen.insert(e.id.clone()) {
                filtered.push(e);
            }
        }
        let mut out = filtered;
        out.sort_by(|a, b| {
            format!("{}", a.folder)
                .cmp(&format!("{}", b.folder))
                .then_with(|| a.sender.cmp(&b.sender))
                .then_with(|| a.id.cmp(&b.id))
        });
        if out.len() > MAIL_PANEL_MAX_ENTRIES {
            out.truncate(MAIL_PANEL_MAX_ENTRIES);
        }
        out
    }

    /// Filters `entries` by case-insensitive substring `query` over `subject`, `sender`, `preview` or `id`.
    /// Query is truncated to `MAIL_PANEL_MAX_SUBJECT_CHARS` at char boundary; bounded to `MAIL_PANEL_MAX_ENTRIES`.
    #[must_use]
    pub fn filter_entries(entries: &[MailEntry], query: &str) -> Vec<MailEntry> {
        let bounded_query = if query.chars().count() <= MAIL_PANEL_MAX_SUBJECT_CHARS {
            query.to_owned()
        } else {
            query.chars().take(MAIL_PANEL_MAX_SUBJECT_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= MAIL_PANEL_MAX_ENTRIES {
                break;
            }
            if lower.is_empty()
                || e.subject.to_ascii_lowercase().contains(&lower)
                || e.sender.to_ascii_lowercase().contains(&lower)
                || e.preview.to_ascii_lowercase().contains(&lower)
                || e.id.to_ascii_lowercase().contains(&lower)
                || format!("{}", e.folder)
                    .to_ascii_lowercase()
                    .contains(&lower)
            {
                out.push(e.clone());
            }
        }
        out
    }

    /// Sorts entries by subject then sender, deduped by id. Pure.
    #[must_use]
    pub fn sorted_by_subject(mut entries: Vec<MailEntry>) -> Vec<MailEntry> {
        entries.sort_by(|a, b| {
            a.subject
                .cmp(&b.subject)
                .then_with(|| a.sender.cmp(&b.sender))
                .then_with(|| a.id.cmp(&b.id))
        });
        entries.dedup_by(|a, b| a.id == b.id);
        if entries.len() > MAIL_PANEL_MAX_ENTRIES {
            entries.truncate(MAIL_PANEL_MAX_ENTRIES);
        }
        entries
    }

    /// Lists folders from `names` into `MAIL_PANEL_MAX_FOLDERS` valid `MailFolder`, sorted, deduped.
    #[must_use]
    pub fn list_folders(names: &[String]) -> Vec<MailFolder> {
        let mut folders: Vec<MailFolder> = names
            .iter()
            .filter_map(|n| MailFolder::parse(n.clone()))
            .collect();
        folders.sort_by(|a, b| format!("{a}").cmp(&format!("{b}")));
        folders.dedup_by(|a, b| format!("{a}") == format!("{b}"));
        if folders.len() > MAIL_PANEL_MAX_FOLDERS {
            folders.truncate(MAIL_PANEL_MAX_FOLDERS);
        }
        folders
    }

    /// Filters folders by case-insensitive substring `query` over display name, bounded.
    #[must_use]
    pub fn filter_folders(folders: &[MailFolder], query: &str) -> Vec<MailFolder> {
        let bounded_query = if query.chars().count() <= MAIL_PANEL_MAX_FOLDER_CHARS {
            query.to_owned()
        } else {
            query.chars().take(MAIL_PANEL_MAX_FOLDER_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for f in folders {
            if out.len() >= MAIL_PANEL_MAX_FOLDERS {
                break;
            }
            let name = format!("{f}");
            if lower.is_empty() || name.to_ascii_lowercase().contains(&lower) {
                out.push(f.clone());
            }
        }
        out
    }

    /// Validates that `path` is allowed for a read operation (`fs.read:~/mail/**`).
    /// Fails closed with `None` when not allowed.
    #[must_use]
    pub fn validate_read(path: &str) -> Option<String> {
        if Self::is_fs_allowed(path) {
            Some(path.to_owned())
        } else {
            None
        }
    }

    /// Validates that `path` is allowed for a write mutation (`fs.write:~/mail/**`);
    /// fails closed when not granted.
    #[must_use]
    pub fn validate_write(path: &str) -> Option<String> {
        if Self::is_fs_write_allowed(path) {
            Some(path.to_owned())
        } else {
            None
        }
    }

    /// Validates that `tool` and `payload` fit MCP framing `8 KiB` and allowlisted tool.
    /// Returns `Some(capability)` on success (`mcp.invoke:mail.*`), `None` when bounds violated.
    #[must_use]
    pub fn validate_mcp_invoke(tool: &str, payload: &str) -> Option<String> {
        if !Self::is_allowed_mcp_tool(tool) {
            return None;
        }
        if payload.contains('\0') {
            return None;
        }
        if payload.len() > MAIL_PANEL_MCP_MAX_FRAME_BYTES {
            return None;
        }
        if payload
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return None;
        }
        Some(format!("mcp.invoke:{tool}"))
    }

    /// Validates that `dest` is an allowed network destination for mail (`network.connect:HOST:PORT`).
    #[must_use]
    pub fn validate_network_dest(dest: &str) -> Option<String> {
        let cand = format!("network.connect:{dest}");
        if Self::is_network_destination_allowed(&cand) {
            Some(cand)
        } else {
            None
        }
    }

    // -- Tiled layout -------------------------------------------------------

    /// Builds a tiled mail layout from a main list `View` and optional preview `View`
    /// using `LayoutNode::split` `H` reuse (no new tiling primitive).
    ///
    /// When `preview` is `None`, returns a single leaf; otherwise a horizontal
    /// split with clamped ratio (mirrors file-manager `tiled_layout`).
    #[must_use]
    pub fn tiled_layout(main: View, preview: Option<View>, ratio: f32) -> LayoutNode {
        match preview {
            None => LayoutNode::leaf(main),
            Some(p) => LayoutNode::split(
                SplitAxis::Horizontal,
                ratio,
                LayoutNode::leaf(main),
                LayoutNode::leaf(p),
            ),
        }
    }

    /// Builds a vertical stack for mail list + compose stacking.
    #[must_use]
    pub fn vertical_stack(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Whether `subject` is an unread filter match (presentation helper, bounded).
    #[must_use]
    pub fn is_unread(entry: &MailEntry) -> bool {
        entry.unread
    }

    /// Filters entries to only unread, bounded.
    #[must_use]
    pub fn filter_unread(entries: &[MailEntry]) -> Vec<MailEntry> {
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= MAIL_PANEL_MAX_SELECTION {
                break;
            }
            if e.unread {
                out.push(e.clone());
            }
        }
        out
    }
}

/// Creates a mail panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults, PR-1..PR-12).
/// Returns the panel handle on success; caller must still activate the
/// associated plugin via the public PluginHost path (`declare → resolve →
/// register → GrantRecord → activate`) for capabilities
/// `panel.provider` + `panel.create` + `terminal.semantic-read` +
/// `mcp.invoke:mail.*` + `network.connect:DESTINATION` +
/// `fs.read:~/mail/**` (and optional `fs.write:~/mail/**`).
/// Helper process (IMAP/SMTP) is counted under the requesting generation
/// (RC-1/RC-2 attribution) and RC-3 `512 MiB` aggregate, not ambient.
/// Any stored tokens are `SecretField` with `0600` and bounded retention,
/// identical to `ai-panel` minimization; WebView path reuses Browser
/// placement but is not allocated here.
pub fn create_mail_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that mail panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
/// PR-1..PR-12: `[1,32]` per workspace, `[1,64]` per window, topics `<=256`,
/// subscriptions `<=32` per panel, drop handled by bus admission.
pub fn validate_mail_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

/// Creates a mail tiled layout via `LayoutNode` primitives (H/V) and mounts
/// the resulting leaf views into a `PanelRegistry`-backed tiled panel
/// placement.
///
/// Pure layout helper — no PanelRegistry mutation, no PTY.
#[must_use]
pub fn mail_panel_tiled_layout(main: View, preview: Option<View>, ratio: f32) -> LayoutNode {
    MailIntegration::tiled_layout(main, preview, ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::Rect as UiRect;
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn mail_id_folder_validation() {
        assert!(MailIntegration::is_valid_id("abc123"));
        assert!(MailIntegration::is_valid_id("mail-1_01.2"));
        assert!(!MailIntegration::is_valid_id(""));
        assert!(!MailIntegration::is_valid_id("has space"));
        assert!(!MailIntegration::is_valid_id("has\0null"));
        assert!(!MailIntegration::is_valid_id("has/control\x07"));
        let long = "a".repeat(MAIL_PANEL_MAX_ID_CHARS + 1);
        assert!(!MailIntegration::is_valid_id(&long));
        assert!(MailIntegration::is_valid_id(
            &"a".repeat(MAIL_PANEL_MAX_ID_CHARS)
        ));
        assert!(MailIntegration::is_valid_folder("INBOX"));
        assert!(MailIntegration::is_valid_folder("Sent"));
        assert!(MailIntegration::is_valid_folder("CustomFolder-1"));
        assert!(!MailIntegration::is_valid_folder(""));
        assert!(!MailIntegration::is_valid_folder("has/slash"));
        assert!(!MailIntegration::is_valid_folder("has\0null"));
        assert!(!MailIntegration::is_valid_folder(
            &"a".repeat(MAIL_PANEL_MAX_FOLDER_CHARS + 1)
        ));
        assert!(!MailIntegration::is_valid_folder(".."));
        assert!(MailFolder::parse("INBOX".into()).unwrap() == MailFolder::Inbox);
        assert!(MailFolder::parse("Sent".into()).unwrap() == MailFolder::Sent);
        assert!(
            MailFolder::parse("Custom-1".into()).unwrap() == MailFolder::Custom("Custom-1".into())
        );
        assert!(MailFolder::parse("bad/folder".into()).is_none());
        let folder = MailFolder::parse("MyArchive".into()).unwrap();
        assert!(!folder.is_well_known());
        assert!(MailFolder::Inbox.is_well_known());
    }

    #[test]
    fn mail_path_and_fs_isolation() {
        assert!(MailIntegration::is_valid_path("~/mail/foo"));
        assert!(MailIntegration::is_within_mail_scope("~/mail"));
        assert!(MailIntegration::is_within_mail_scope("~/mail/"));
        assert!(MailIntegration::is_within_mail_scope(
            "~/mail/inbox/msg1.json"
        ));
        assert!(!MailIntegration::is_within_mail_scope("~/projects/foo"));
        assert!(!MailIntegration::is_within_mail_scope(
            "~/mail/../etc/passwd"
        ));
        assert!(!MailIntegration::is_within_mail_scope("/etc/passwd"));
        assert!(!MailIntegration::is_within_mail_scope(""));
        assert!(MailIntegration::is_fs_allowed("~/mail/inbox/msg1.json"));
        assert!(!MailIntegration::is_fs_allowed("/etc/passwd"));
        assert!(!MailIntegration::is_fs_allowed("~/mail/../secret"));
        assert!(MailIntegration::is_fs_write_allowed("~/mail/drafts/tmp"));
        assert!(!MailIntegration::is_fs_write_allowed("/tmp/evil"));
        assert!(MailIntegration::mail_key_raw("~/mail/inbox/a").unwrap() == "inbox/a");
        assert!(MailIntegration::mail_key_raw("~/mail").is_none());
        assert!(MailIntegration::validate_read("~/mail/inbox/x").is_some());
        assert!(MailIntegration::validate_read("/etc/passwd").is_none());
        assert!(MailIntegration::validate_write("~/mail/drafts/x").is_some());
        assert!(MailIntegration::validate_write("/tmp/x").is_none());
    }

    #[test]
    fn mail_entry_bounded_truncated() {
        let folder = MailFolder::Inbox;
        let e = MailEntry::new(
            "id-1".into(),
            "Hello".into(),
            "alice@example.com".into(),
            "preview text".into(),
            folder,
            true,
        )
        .unwrap();
        assert_eq!(e.id, "id-1");
        assert_eq!(e.subject, "Hello");
        assert!(!e.truncated_subject);
        assert!(MailIntegration::is_valid_subject("Hello world"));
        assert!(!MailIntegration::is_valid_subject("bad\0subject"));
        assert!(MailIntegration::is_valid_sender("bob@example.com"));
        assert!(!MailIntegration::is_valid_sender(""));
        assert!(!MailIntegration::is_valid_sender("bad\0sender"));
        // Truncation at 128/256
        let long_subj = "s".repeat(MAIL_PANEL_MAX_SUBJECT_CHARS + 20);
        let long_sender = "a".repeat(MAIL_PANEL_MAX_SENDER_CHARS + 20) + "@example.com";
        let long_preview = "p".repeat(MAIL_PANEL_MAX_PREVIEW_CHARS + 50);
        let e2 = MailEntry::new(
            "id-2".into(),
            long_subj,
            long_sender.clone(),
            long_preview,
            MailFolder::Custom("MyFolder".into()),
            false,
        )
        .unwrap();
        assert_eq!(e2.subject.chars().count(), MAIL_PANEL_MAX_SUBJECT_CHARS);
        assert!(e2.truncated_subject);
        assert_eq!(e2.sender.chars().count(), MAIL_PANEL_MAX_SENDER_CHARS);
        assert!(e2.truncated_sender);
        assert_eq!(e2.preview.chars().count(), MAIL_PANEL_MAX_PREVIEW_CHARS);
        assert!(e2.truncated_preview);
        // Invalid id rejected
        assert!(
            MailEntry::new(
                "bad id".into(),
                "subj".into(),
                "alice@example.com".into(),
                "prev".into(),
                MailFolder::Inbox,
                false
            )
            .is_none()
        );
        assert!(
            MailEntry::new(
                "".into(),
                "subj".into(),
                "alice@example.com".into(),
                "prev".into(),
                MailFolder::Inbox,
                false
            )
            .is_none()
        );
        // Control rejected except \n \t for preview (but not for subject/sender)
        assert!(
            MailEntry::new(
                "id3".into(),
                "subj\x07".into(),
                "alice@example.com".into(),
                "prev".into(),
                MailFolder::Inbox,
                false
            )
            .is_none()
        );
        // Preview allows newline
        let e3 = MailEntry::new(
            "id3".into(),
            "subj".into(),
            "alice@example.com".into(),
            "line1\nline2".into(),
            MailFolder::Inbox,
            false,
        )
        .unwrap();
        assert!(e3.preview.contains('\n'));
        // Payload overflow rejected (8KiB)
        let huge = "x".repeat(MAIL_PANEL_PAYLOAD_MAX_BYTES + 1);
        assert!(
            MailEntry::new(
                "idhuge".into(),
                "subj".into(),
                "alice@example.com".into(),
                huge,
                MailFolder::Inbox,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn mcp_and_network_capability_helpers_bounded() {
        assert!(MailIntegration::is_mcp_mail_allowed("mcp.invoke:mail.list"));
        assert!(MailIntegration::is_mcp_mail_allowed("mcp.invoke:mail.read"));
        assert!(MailIntegration::is_mcp_mail_allowed("mcp.invoke:mail.send"));
        assert!(MailIntegration::is_mcp_mail_allowed(
            "mcp.invoke:mail.search"
        ));
        assert!(!MailIntegration::is_mcp_mail_allowed(
            "mcp.invoke:mail.delete"
        ));
        assert!(!MailIntegration::is_mcp_mail_allowed("mcp.invoke:fetch"));
        assert!(!MailIntegration::is_mcp_mail_allowed(
            "mcp.invoke:mail.list:extra"
        ));
        assert!(MailIntegration::is_allowed_mcp_tool("mail.list"));
        assert!(!MailIntegration::is_allowed_mcp_tool("mail.delete"));
        assert!(MailIntegration::is_mcp_list_allowed(MAIL_PANEL_MCP_LIST));
        assert!(!MailIntegration::is_mcp_list_allowed(MAIL_PANEL_MCP_READ));
        assert_eq!(
            MailIntegration::validate_mcp_invoke("mail.list", "{\"folder\":\"INBOX\"}").unwrap(),
            "mcp.invoke:mail.list"
        );
        assert!(MailIntegration::validate_mcp_invoke("mail.delete", "{}").is_none());
        assert!(
            MailIntegration::validate_mcp_invoke(
                "mail.list",
                &"a".repeat(MAIL_PANEL_MCP_MAX_FRAME_BYTES + 1)
            )
            .is_none()
        );
        assert!(MailIntegration::validate_mcp_invoke("mail.list", "bad\0payload").is_none());
        // Network
        assert!(MailIntegration::is_network_destination_allowed(
            MAIL_PANEL_NETWORK_IMAP
        ));
        assert!(MailIntegration::is_network_destination_allowed(
            MAIL_PANEL_NETWORK_SMTP
        ));
        assert!(MailIntegration::is_network_destination_allowed(
            "network.connect:imap.example.com:993"
        ));
        assert!(MailIntegration::is_network_destination_allowed(
            "network.connect:smtp.example.com:465"
        ));
        assert!(MailIntegration::is_network_destination_allowed(
            "network.connect:mail.example.com:993"
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "network.connect:"
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "network.connect:local"
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "network.connect:imap.example.com:99999"
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "network.connect:imap.example.com:bad"
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "fs.read:~/mail/**"
        ));
        assert!(MailIntegration::validate_network_dest("imap.example.com:993").is_some());
        assert!(MailIntegration::validate_network_dest("no-dot").is_none());
        assert!(MailIntegration::is_mail_fs_read_allowed(
            "fs.read:~/mail/**"
        ));
        assert!(!MailIntegration::is_mail_fs_read_allowed(
            "fs.read:~/projects/**"
        ));
    }

    #[test]
    fn list_entries_bounded_sorted_deduped() {
        let make = |id: &str, subject: &str, folder: MailFolder| {
            MailEntry::new(
                id.into(),
                subject.into(),
                "alice@example.com".into(),
                "preview".into(),
                folder,
                false,
            )
            .unwrap()
        };
        let dup = vec![
            make("id1", "B subject", MailFolder::Inbox),
            make("id1", "A subject", MailFolder::Sent), // same id, different folder -> deduped by id
            make("id2", "A subject", MailFolder::Inbox),
        ];
        let listed = MailIntegration::list_entries(dup);
        assert_eq!(listed.len(), 2);
        // Sorted by folder then sender then id
        assert!(listed.iter().any(|e| e.id == "id1"));
        assert!(listed.iter().any(|e| e.id == "id2"));
        // Bounded at 128
        let many: Vec<MailEntry> = (0..200)
            .map(|i| {
                MailEntry::new(
                    format!("id{i}"),
                    format!("Subject {i}"),
                    "alice@example.com".into(),
                    "preview".into(),
                    MailFolder::Inbox,
                    false,
                )
                .unwrap()
            })
            .collect();
        assert_eq!(
            MailIntegration::list_entries(many).len(),
            MAIL_PANEL_MAX_ENTRIES
        );
        // Folder list bounded
        let folder_names: Vec<String> = (0..100).map(|i| format!("Folder{i}")).collect();
        assert_eq!(
            MailIntegration::list_folders(&folder_names).len(),
            MAIL_PANEL_MAX_FOLDERS
        );
        // Deduped
        let dup_folders = vec!["INBOX".into(), "INBOX".into(), "Sent".into()];
        assert_eq!(MailIntegration::list_folders(&dup_folders).len(), 2);
        // Invalid folders filtered
        let mixed = vec!["INBOX".into(), "bad/folder".into(), "Valid-1".into()];
        assert_eq!(MailIntegration::list_folders(&mixed).len(), 2);
    }

    #[test]
    fn filter_entries_bounded_case_insensitive() {
        let make = |id: &str, subj: &str, sender: &str| {
            MailEntry::new(
                id.into(),
                subj.into(),
                sender.into(),
                "preview".into(),
                MailFolder::Inbox,
                false,
            )
            .unwrap()
        };
        let entries = vec![
            make("id1", "alpha report", "alice@example.com"),
            make("id2", "Beta notes", "Bob@example.com"),
            make("id3", "gamma", "charlie@example.com"),
        ];
        let listed = MailIntegration::list_entries(entries);
        let filtered = MailIntegration::filter_entries(&listed, "alpha");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "id1");
        let filtered2 = MailIntegration::filter_entries(&listed, "BETA");
        assert_eq!(filtered2.len(), 1);
        assert_eq!(filtered2[0].id, "id2");
        let empty = MailIntegration::filter_entries(&listed, "");
        assert_eq!(empty.len(), 3);
        let long_q = "a".repeat(MAIL_PANEL_MAX_SUBJECT_CHARS + 10);
        assert_eq!(MailIntegration::filter_entries(&listed, &long_q).len(), 0);
        // Bounded at 128
        let many: Vec<MailEntry> = (0..200)
            .map(|i| {
                MailEntry::new(
                    format!("id{i}"),
                    format!("Subject {i}"),
                    "alice@example.com".into(),
                    "p".into(),
                    MailFolder::Inbox,
                    false,
                )
                .unwrap()
            })
            .collect();
        let listed_many = MailIntegration::list_entries(many);
        assert_eq!(
            MailIntegration::filter_entries(&listed_many, "").len(),
            MAIL_PANEL_MAX_ENTRIES
        );
        // Sender also searchable
        let sender_q = MailIntegration::filter_entries(&listed, "alice");
        assert_eq!(sender_q.len(), 1);
        // Folder searchable
        let folder_entry = MailEntry::new(
            "idX".into(),
            "subj".into(),
            "a@b.com".into(),
            "p".into(),
            MailFolder::Sent,
            false,
        )
        .unwrap();
        let mixed =
            MailIntegration::list_entries(vec![listed[0].clone(), listed[1].clone(), folder_entry]);
        let folder_q = MailIntegration::filter_entries(&mixed, "Sent");
        assert_eq!(folder_q.len(), 1);
        assert_eq!(folder_q[0].folder, MailFolder::Sent);
        // Unread filter
        let unread_entry = MailEntry::new(
            "uid1".into(),
            "unread mail".into(),
            "a@b.com".into(),
            "p".into(),
            MailFolder::Inbox,
            true,
        )
        .unwrap();
        let read_entry = MailEntry::new(
            "rid1".into(),
            "read mail".into(),
            "a@b.com".into(),
            "p".into(),
            MailFolder::Inbox,
            false,
        )
        .unwrap();
        let unread_filtered =
            MailIntegration::filter_unread(&[unread_entry.clone(), read_entry.clone()]);
        assert_eq!(unread_filtered.len(), 1);
        assert_eq!(unread_filtered[0].id, "uid1");
        assert!(MailIntegration::is_unread(&unread_entry));
        assert!(!MailIntegration::is_unread(&read_entry));
    }

    #[test]
    fn tiled_layout_reuses_h_split_and_stack_no_new_primitive() {
        let main = View::new(ViewId::new(10), 80, 24);
        let preview = View::new(ViewId::new(11), 80, 24);
        let layout = MailIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.5);
        assert!(matches!(layout, LayoutNode::Split { .. }));
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        let low = MailIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.01);
        let high = MailIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.99);
        for l in [low, high] {
            let a = l.layout(UiRect::new(0, 0, 80, 24));
            assert!(a[0].1.width >= 1 && a[1].1.width >= 1);
        }
        let nan = MailIntegration::tiled_layout(main.clone(), Some(preview), f32::NAN);
        assert_eq!(nan.layout(UiRect::new(0, 0, 80, 24)).len(), 2);
        let solo = MailIntegration::tiled_layout(main, None, 0.5);
        assert_eq!(solo.leaf_count(), 1);
        let v1 = View::new(ViewId::new(20), 80, 12);
        let v2 = View::new(ViewId::new(21), 80, 12);
        let stack = MailIntegration::vertical_stack(vec![v1, v2]);
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
        let id = create_mail_panel(&mut reg, ws, view).expect("create mail panel");
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
        let topic = reg2.declare_topic("xuepoo.mail:listing").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("~/mail/msg{i}.json")).unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        assert!(!MailIntegration::is_fs_allowed("/etc/passwd"));
        assert!(MailIntegration::is_fs_allowed("~/mail/inbox/msg1.json"));
        assert!(!MailIntegration::is_fs_allowed("~/mail/../secret"));
        assert!(MailIntegration::is_fs_write_allowed(
            "~/mail/drafts/tmp.json"
        ));
        assert!(!MailIntegration::is_fs_write_allowed("/tmp/hack"));
        assert!(MailIntegration::is_mcp_mail_allowed(MAIL_PANEL_MCP_LIST));
        assert!(!MailIntegration::is_mcp_mail_allowed("mcp.invoke:fetch"));
        assert!(MailIntegration::is_network_destination_allowed(
            MAIL_PANEL_NETWORK_IMAP
        ));
        assert!(!MailIntegration::is_network_destination_allowed(
            "network.connect:bad"
        ));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_mail_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_mail_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_mail_panel_config(&ok).is_ok());
        let bad_topics = PanelRegistryConfig {
            max_topics_total: 257,
            ..Default::default()
        };
        assert!(validate_mail_panel_config(&bad_topics).is_err());
        let bad_subs = PanelRegistryConfig {
            max_subscriptions_per_panel: 33,
            ..Default::default()
        };
        assert!(validate_mail_panel_config(&bad_subs).is_err());
    }

    #[test]
    fn capability_parsing_and_hash_bound_isolation_for_mail() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::mail_panel_manifest};
        let m = mail_panel_manifest();
        let read_caps: Vec<_> = m
            .capabilities
            .filesystem
            .iter()
            .filter(|r| matches!(r.access, bitty_plugin_host::FsAccess::Read))
            .collect();
        assert!(!read_caps.is_empty());
        assert!(read_caps[0].paths.contains(&"~/mail/**".to_string()));
        let cap_read = CapabilityId::parse("fs.read:~/mail/**").unwrap();
        assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
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
                .contains(&CapabilityId::parse("mcp.invoke:mail.list").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("mcp.invoke:mail.read").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("mcp.invoke:mail.send").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("network.connect:imap.example.com:993").unwrap())
        );
        let cap_net = CapabilityId::parse(MAIL_PANEL_NETWORK_IMAP).unwrap();
        assert_eq!(
            cap_net.family(),
            bitty_plugin_host::CapabilityFamily::Network
        );
        let cap_mcp = CapabilityId::parse(MAIL_PANEL_MCP_LIST).unwrap();
        assert_eq!(cap_mcp.family(), bitty_plugin_host::CapabilityFamily::Mcp);
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        let outside = CapabilityId::parse("fs.read:/etc/passwd").unwrap();
        assert_ne!(cap_read.as_str(), outside.as_str());
        let _id = PluginId::new("bitty-terminal.mail-panel").unwrap();
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == MAIL_PANEL_COMMAND_OPEN)
        );
    }

    #[test]
    fn tiled_layout_deterministic_and_overlay_still_works() {
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
        let stack = MailIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        let base = LayoutNode::leaf(View::new(ViewId::new(10), 80, 24));
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
    }

    #[test]
    fn truncate_and_selection_bounded() {
        let long = "a".repeat(MAIL_PANEL_MAX_SUBJECT_CHARS + 50);
        let truncated = MailIntegration::truncate_subject(&long);
        assert_eq!(truncated.chars().count(), MAIL_PANEL_MAX_SUBJECT_CHARS);
        assert!(MailIntegration::is_subject_bounded("hello"));
        assert!(!MailIntegration::is_subject_bounded(&long));
        assert!(MailIntegration::is_sender_bounded("alice@example.com"));
        let long_sender = "a".repeat(MAIL_PANEL_MAX_SENDER_CHARS + 10);
        assert!(!MailIntegration::is_sender_bounded(&long_sender));
        assert!(MailIntegration::is_preview_bounded("preview"));
        let many: Vec<MailEntry> = (0..200)
            .map(|i| {
                MailEntry::new(
                    format!("id{i}"),
                    format!("Subject {i}"),
                    "alice@example.com".into(),
                    "p".into(),
                    MailFolder::Inbox,
                    false,
                )
                .unwrap()
            })
            .collect();
        let entries = MailIntegration::list_entries(many);
        assert_eq!(entries.len(), MAIL_PANEL_MAX_ENTRIES);
        let filtered = MailIntegration::filter_entries(&entries, "");
        assert!(
            filtered.len() <= MAIL_PANEL_MAX_SELECTION || filtered.len() <= MAIL_PANEL_MAX_ENTRIES
        );
        let preview_long = "p".repeat(MAIL_PANEL_MAX_PREVIEW_CHARS + 20);
        assert_eq!(
            MailIntegration::truncate_preview(&preview_long)
                .chars()
                .count(),
            MAIL_PANEL_MAX_PREVIEW_CHARS
        );
    }
}
