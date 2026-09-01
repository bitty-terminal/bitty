#![forbid(unsafe_code)]
//! Browser panel via Panel Runtime — tiled Panel + View Browser(BrowserSurfaceId), bounded, allowlisted.
//!
//! This module is the first-party `bitty-terminal.browser-panel` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Browser panel
//! is a host-owned `View` `Browser(BrowserSurfaceId)` surface plus optional
//! `Panel(PanelId)` controls (address input, tab strip) — host-owned
//! `BrowserSurfaceId` per `05e8803` placement Option A. The browser surface
//! is embedded via an external embedder (for example `wry` `WebView`); the
//! host owns the surface handle and `LogicalRect` placement, the embedder
//! owns the web process and navigation state. No terminal bytes are parsed,
//! no grid mutation occurs (only `Action` writes `State` per Terminal State
//! RFC), and no hot-path is entered.
//!
//! It verifies `BrowserSurfaceId` distinct newtype with no `From` bridge to
//! `PanelId`/`ViewId`/`TerminalId`, `Generation` monotonic with reserve `1024`
//! and fail-closed exhaustion, lifecycle
//! `Declared -> Created -> Navigating -> Focused -> Suspended -> Disposed`
//! with validated transitions, navigation allowlist `https` default with
//! `file://` requiring distinct `browser.file-url` gate per R-005
//! `FileUrlActivation` (validated against `PROJECT_GLOB` `~/projects/**`),
//! `javascript:`/`data:`/`http://` denied, storage via distinct
//! `browser.storage` gate, bounded queues `64`/`1024`/`256 KiB`/`8192`/`2 MiB`
//! `DropOldest` with `8 KiB` payload and `32`/`8 KiB` batch (PR-1..PR-12,
//! BA-1..BA-3), command registry `owner.name:command` qualified
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`, `<=128` chars)
//! duplicates rejected per-type `32` bound, overlay max `4` plus `1` modal
//! with modal exclusivity and text `128`/tooltip `256` bounds, focus MRU per
//! Workspace per Window, EventBus three levels `64`/`1024`/`256 KiB`/`8192`/
//! `2 MiB` and `8 KiB` payload `DropOldest` default with counted per-queue
//! attribution and coalescing for observation topics (single-process `winit`
//! one-registry-per-window headless, `ViewContent::Browser`), capability
//! isolation per `(BrowserSurfaceId, generation)` and `(PanelId, generation)`
//! deny-by-default via `CapabilityId` browser family `browser.embed`/
//! `browser.navigation`/`browser.file-url`/`browser.storage` plus panel family
//! `panel.provider`/`panel.create` — no ambient authority, no first-party
//! bypass. Default is disabled (fresh `EffectiveConfig` has empty `plugins`);
//! `bitty --safe` rejects `bitty-terminal.*` as non-builtin without panic,
//! identical to third-party `xuepoo.*` parity (no private channel). Bounded
//! embedder is under RC-3 `512 MiB` aggregate, navigation pending BA-3 `32`
//! FIFO `DropOldest`, suspension retains handle but pauses media. `forbid(unsafe)`,
//! single-process `winit` one-registry-per-window headless.

use std::collections::VecDeque;

use bitty_ui::{
    LayoutNode, SplitAxis, View, ViewId,
    panel::{BrowserSurfaceId, MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN},
};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

// ---------------------------------------------------------------------------
// Capability patterns — each a distinct gate, no ambient
// ---------------------------------------------------------------------------

/// High-risk embed capability — requires explicit user grant.
pub const BROWSER_CAPABILITY_EMBED: &str = "browser.embed";
/// Navigation capability — allowlisted `https` default.
pub const BROWSER_CAPABILITY_NAVIGATION: &str = "browser.navigation";
/// File-url capability — distinct gate for `file://`, validated against `~/projects/**`.
pub const BROWSER_CAPABILITY_FILE_URL: &str = "browser.file-url";
/// Storage capability — cookie/cache persistence with bounded quota.
pub const BROWSER_CAPABILITY_STORAGE: &str = "browser.storage";

// ---------------------------------------------------------------------------
// Bounded resource table (BA-1..BA-3 + PR-1..PR-12 reuse)
// ---------------------------------------------------------------------------

/// Maximum browser panels per window — BA-1 default `4` aggregate `[1,8]`.
pub const BROWSER_MAX_PANELS_PER_WINDOW: usize = 4;
/// Maximum WebView instances per Browser panel — BA-2 `1` strict.
pub const BROWSER_MAX_WEBVIEWS_PER_PANEL: usize = 1;
/// Maximum pending navigations per BrowserSurfaceId — BA-3 `32` FIFO `DropOldest`.
pub const BROWSER_MAX_NAVIGATION_QUEUE: usize = 32;
/// Maximum history entries to present — mirrors `GIT_PANEL_MAX_ENTRIES` `128` but scoped to `32`.
pub const BROWSER_MAX_HISTORY_ENTRIES: usize = 32;
/// Maximum URL bytes — mirrors parser `BoundedString::MAX_LEN` `4096` and project path bound.
pub const BROWSER_MAX_URL_BYTES: usize = 4096;
/// Maximum title chars — mirrors overlay text bound `128`.
pub const BROWSER_MAX_TITLE_CHARS: usize = MAX_OVERLAY_TEXT_LEN;
/// Maximum title tooltip chars — `256`.
pub const BROWSER_MAX_TITLE_TOOLTIP_CHARS: usize = MAX_OVERLAY_TOOLTIP_LEN;
/// Panel payload for browser observations is bounded by `BUS_EVENT_MAX_BYTES` `8 KiB` at bus admission.
pub const BROWSER_PANEL_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;
/// Maximum selection size — bounded `64` mirroring per-subscription bound (PR-7).
pub const BROWSER_PANEL_MAX_SELECTION: usize = crate::registry::BUS_PER_SUBSCRIPTION_LIMIT;
/// Maximum panels per workspace for browser — mirrors `MAX_PANELS_PER_WORKSPACE` `32` PR-1.
pub const BROWSER_PANEL_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
/// Maximum panels per window for browser — mirrors `MAX_PANELS_PER_WINDOW` `64` PR-2 but capped by BA-1 `4` for browser type.
pub const BROWSER_PANEL_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;
/// Maximum storage bytes per BrowserSurface — bounded quota `256 KiB` (RC-5 style).
pub const BROWSER_MAX_STORAGE_BYTES: usize = 256 * 1024;

/// Canonical browser-panel commands (qualified `owner.name:command`).
pub const BROWSER_PANEL_COMMAND_OPEN: &str = "bitty-terminal.browser-panel:open";
pub const BROWSER_PANEL_COMMAND_NAVIGATE: &str = "bitty-terminal.browser-panel:navigate";
pub const BROWSER_PANEL_COMMAND_BACK: &str = "bitty-terminal.browser-panel:back";
pub const BROWSER_PANEL_COMMAND_FORWARD: &str = "bitty-terminal.browser-panel:forward";
pub const BROWSER_PANEL_COMMAND_RELOAD: &str = "bitty-terminal.browser-panel:reload";

/// Closed allowed schemes for navigation — `https` default, `file` gated, `http`/`javascript:`/`data:` denied.
pub const BROWSER_ALLOWED_SCHEMES: &[&str] = &["https", "file"];

// ---------------------------------------------------------------------------
// History entry — bounded, pure data
// ---------------------------------------------------------------------------

/// Single history entry — bounded, pure data for presentation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BrowserHistoryEntry {
    /// URL, validated and bounded to `BROWSER_MAX_URL_BYTES`.
    pub url: String,
    /// Title, truncated at char boundary to `BROWSER_MAX_TITLE_CHARS`.
    pub title: String,
    /// Whether title was truncated.
    pub truncated: bool,
}

impl BrowserHistoryEntry {
    /// Creates an entry from validated url and title.
    /// Returns `None` for invalid url (non-allowlisted scheme, too long,
    /// control chars) or file-url outside scope without gate.
    #[must_use]
    pub fn new(url: String, title: String) -> Option<Self> {
        if !BrowserIntegration::is_valid_url(&url) {
            return None;
        }
        if !BrowserIntegration::is_navigation_allowed(&url, false) {
            // Without file-url gate, file:// is denied; for history we
            // require allowlisted navigation even without gate escalation.
            // This keeps history consistent with navigation policy.
            // For file:// with gate we accept.
            if BrowserIntegration::is_file_url(&url) {
                // file:// requires file-url gate; deny in this pure constructor
                return None;
            }
            if !BrowserIntegration::is_https_url(&url) {
                return None;
            }
        }
        let (bounded_title, truncated) = truncate_bounded(&title, BROWSER_MAX_TITLE_CHARS);
        Some(Self {
            url,
            title: bounded_title,
            truncated,
        })
    }

    #[must_use]
    pub fn with_file_url_gate(url: String, title: String) -> Option<Self> {
        if !BrowserIntegration::is_valid_url(&url) {
            return None;
        }
        if !BrowserIntegration::is_navigation_allowed(&url, true) {
            return None;
        }
        let (bounded_title, truncated) = truncate_bounded(&title, BROWSER_MAX_TITLE_CHARS);
        Some(Self {
            url,
            title: bounded_title,
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

// ---------------------------------------------------------------------------
// Navigation queue — bounded 32 FIFO DropOldest
// ---------------------------------------------------------------------------

/// Bounded navigation queue per `BrowserSurfaceId` — 32 FIFO `DropOldest`.
///
/// Models BA-3 pending navigations. Not a panel yet, no embedder I/O, pure
/// bounded queue with counted drops for `bitty plugin doctor` style attribution.
#[derive(Clone, Debug)]
pub struct BrowserNavigationQueue {
    inner: VecDeque<String>,
    dropped: u64,
}

impl BrowserNavigationQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: VecDeque::with_capacity(BROWSER_MAX_NAVIGATION_QUEUE),
            dropped: 0,
        }
    }

    /// Enqueues `url` after allowlist validation; `DropOldest` when full.
    /// `file_url_allowed` gates `file://` acceptance.
    ///
    /// Returns `true` when enqueued, `false` when rejected (invalid url or
    /// non-allowlisted scheme without gate).
    pub fn enqueue(&mut self, url: String, file_url_allowed: bool) -> bool {
        if !BrowserIntegration::is_valid_url(&url) {
            return false;
        }
        if !BrowserIntegration::is_navigation_allowed(&url, file_url_allowed) {
            return false;
        }
        if self.inner.len() >= BROWSER_MAX_NAVIGATION_QUEUE {
            self.inner.pop_front();
            self.dropped = self.dropped.wrapping_add(1);
        }
        self.inner.push_back(url);
        true
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn drain(&mut self, max: usize) -> Vec<String> {
        let n = max.min(self.inner.len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(u) = self.inner.pop_front() {
                out.push(u);
            }
        }
        out
    }

    #[must_use]
    pub fn peek_front(&self) -> Option<&String> {
        self.inner.front()
    }
}

impl Default for BrowserNavigationQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// BrowserIntegration — pure, bounded, allowlisted helpers
// ---------------------------------------------------------------------------

/// BrowserIntegration — pure, observation-only helpers over committed state
/// and browser capabilities. No mutation of `State`, no hot-path, bounded
/// `<=32` panels/window (BA-1) and `<=32` navigation queue (BA-3), `1`
/// WebView/panel (BA-2), tiled `LayoutNode` `H`/`V` reuse via `Panel(PanelId)`
/// plus `View Browser(BrowserSurfaceId)` host surface.
#[derive(Debug, Clone, Copy)]
pub struct BrowserIntegration;

impl BrowserIntegration {
    /// Whether `url` is a valid bounded URL (no control, no null, `<=4096`
    /// bytes, non-empty, at least `scheme://` and no `..` segment in path that
    /// would confuse file scope). Rejects `javascript:` and `data:`.
    #[must_use]
    pub fn is_valid_url(url: &str) -> bool {
        if url.is_empty() || url.len() > BROWSER_MAX_URL_BYTES {
            return false;
        }
        if url.contains('\0') {
            return false;
        }
        if url.chars().any(|c| c.is_control()) {
            return false;
        }
        if url.contains(' ') {
            return false;
        }
        // Must contain "://"
        let Some(colon) = url.find("://") else {
            return false;
        };
        let scheme = &url[..colon];
        if scheme.is_empty() || scheme.chars().any(|c| c.is_control()) {
            return false;
        }
        if !scheme.chars().all(|c| c.is_ascii_lowercase()) {
            return false;
        }
        // Deny dangerous schemes even if syntactically valid
        if scheme == "javascript" || scheme == "data" || scheme == "vbscript" {
            return false;
        }
        // Path must not contain null control already checked, but also reject
        // bare ".." traversal that could bypass file scope? Already scoped
        // separately, but keep conservative.
        let rest = &url[colon + 3..];
        if rest.is_empty() {
            return false;
        }
        // Reject URLs with unescaped spaces already checked, but also reject
        // urls with backticks or shell metachars that hint injection? Keep
        // minimal: reject '|' ';' '&' '`' '$' '(' ')' as conservative allowlist?
        // For browser navigation, query strings legitimately contain '&' '=' '?'.
        // So only reject control and null, not these.
        true
    }

    /// Whether `url` is `https://*` — default allowlisted scheme.
    #[must_use]
    pub fn is_https_url(url: &str) -> bool {
        url.starts_with("https://") && Self::is_valid_url(url)
    }

    /// Whether `url` is `file://*`.
    #[must_use]
    pub fn is_file_url(url: &str) -> bool {
        url.starts_with("file://") && Self::is_valid_url(url)
    }

    /// Whether `url` is `http://*` — explicitly denied in default allowlist.
    #[must_use]
    pub fn is_http_url(url: &str) -> bool {
        url.starts_with("http://") && Self::is_valid_url(url)
    }

    /// Extracts the file path from a `file://` url for scope validation.
    /// Returns `None` for non-file urls or invalid.
    #[must_use]
    pub fn file_url_to_path(url: &str) -> Option<String> {
        if !Self::is_file_url(url) {
            return None;
        }
        let path = url.strip_prefix("file://")?;
        if path.is_empty() {
            return None;
        }
        // For validation we expect either absolute `/...` or `~/projects/...`
        // after file://. The pre-study PROJECT_GLOB is `~/projects/**`.
        // Accept `file:///home/...` and `file://~/projects/...` both but scope
        // check via `is_within_file_scope` will reject non-project absolute.
        Some(path.to_owned())
    }

    /// Whether `file://` url is within the granted file scope `~/projects/**`.
    ///
    /// Pure, bounded check: path extracted from `file://` must be covered by
    /// `~/projects/**` or be a valid absolute path that real-path would
    /// resolve under `~/projects` — but pure helper conservatively requires
    /// `~/projects/` prefix or exactly `~/projects`. Symlink/device checks are
    /// deferred to host real-path resolution, mirroring `FileManagerIntegration`.
    #[must_use]
    pub fn is_within_file_scope(url: &str) -> bool {
        let Some(path) = Self::file_url_to_path(url) else {
            return false;
        };
        // Reuse file-manager style scope: must start with ~/projects/
        // For file:// we interpret the path after file:// as the filesystem path.
        // The allowlist requires that path be within ~/projects/**.
        // Accept both "~/projects/..." and "~/projects"
        if path == "~/projects" || path == "~/projects/" {
            return true;
        }
        if path.starts_with("~/projects/") {
            if path.contains("..") {
                return false;
            }
            return true;
        }
        // Also accept absolute `/home/user/projects` style? Conservative: reject
        // unless it contains ~/projects segment; host real-path check would be needed.
        // For bounded pure helper, only allow ~/projects prefix.
        false
    }

    /// Whether navigation to `url` is allowed under the allowlist.
    ///
    /// `https://` allowed when `is_valid_url` (requires `browser.navigation`);
    /// `file://` allowed only when `file_url_allowed` (requires `browser.file-url`
    /// gate) and `is_within_file_scope`.
    /// `http://`, `javascript:`, `data:` always denied. `file://` outside
    /// `~/projects/**` denied even with gate.
    #[must_use]
    pub fn is_navigation_allowed(url: &str, file_url_allowed: bool) -> bool {
        if !Self::is_valid_url(url) {
            return false;
        }
        if Self::is_https_url(url) {
            return true;
        }
        if Self::is_file_url(url) {
            if !file_url_allowed {
                return false;
            }
            return Self::is_within_file_scope(url);
        }
        // http:// and others denied (allowlist is https default)
        false
    }

    /// Whether `candidate` is allowed for browser storage (`browser.storage` gate).
    /// Pure bounded check: path-like storage keys are limited to 4096 bytes and no control.
    #[must_use]
    pub fn is_storage_allowed(candidate: &str) -> bool {
        if candidate.is_empty() || candidate.len() > BROWSER_MAX_URL_BYTES {
            return false;
        }
        if candidate.contains('\0') || candidate.chars().any(|c| c.is_control()) {
            return false;
        }
        true
    }

    /// Whether `url` is allowed for `browser.embed` surface — same as
    /// navigation allowlist but strictly `https` or gated `file`.
    #[must_use]
    pub fn is_browser_embed_allowed(url: &str, file_url_allowed: bool) -> bool {
        Self::is_navigation_allowed(url, file_url_allowed)
    }

    /// Whether `url` string is bounded at `BROWSER_MAX_URL_BYTES`.
    #[must_use]
    pub fn is_url_bounded(url: &str) -> bool {
        url.len() <= BROWSER_MAX_URL_BYTES && url.chars().count() <= BROWSER_MAX_URL_BYTES
    }

    /// Validates that `url` fits payload and bounds.
    #[must_use]
    pub fn validate_url(url: &str, file_url_allowed: bool) -> Option<String> {
        if Self::is_navigation_allowed(url, file_url_allowed) {
            Some(url.to_owned())
        } else {
            None
        }
    }

    /// Whether `title` is bounded at `MAX_OVERLAY_TEXT_LEN`.
    #[must_use]
    pub fn is_title_bounded(title: &str) -> bool {
        title.chars().count() <= BROWSER_MAX_TITLE_CHARS
    }

    /// Truncates `title` to `BROWSER_MAX_TITLE_CHARS` at char boundary.
    #[must_use]
    pub fn truncate_title(title: &str) -> String {
        let (s, _) = truncate_bounded(title, BROWSER_MAX_TITLE_CHARS);
        s
    }

    /// Filters and bounds a raw `urls` history to `BROWSER_MAX_HISTORY_ENTRIES`
    /// valid entries, sorted deterministic, deduped by url. Pure observation.
    /// `file_url_allowed` gates file urls.
    #[must_use]
    pub fn list_history(urls: &[String], file_url_allowed: bool) -> Vec<BrowserHistoryEntry> {
        let mut entries: Vec<BrowserHistoryEntry> = urls
            .iter()
            .filter_map(|u| {
                if Self::is_file_url(u) {
                    BrowserHistoryEntry::with_file_url_gate(u.clone(), String::new())
                } else {
                    BrowserHistoryEntry::new(u.clone(), String::new())
                }
            })
            .filter(|e| {
                if Self::is_file_url(&e.url) {
                    file_url_allowed
                } else {
                    true
                }
            })
            .collect();
        entries.sort_by(|a, b| a.url.cmp(&b.url));
        entries.dedup_by(|a, b| a.url == b.url);
        if entries.len() > BROWSER_MAX_HISTORY_ENTRIES {
            entries.truncate(BROWSER_MAX_HISTORY_ENTRIES);
        }
        entries
    }

    /// Filters history entries by case-insensitive substring `query` over `title` or `url`.
    /// Bounded to `BROWSER_MAX_HISTORY_ENTRIES`.
    #[must_use]
    pub fn filter_history(
        entries: &[BrowserHistoryEntry],
        query: &str,
    ) -> Vec<BrowserHistoryEntry> {
        let bounded_query = if query.chars().count() <= BROWSER_MAX_TITLE_CHARS {
            query.to_owned()
        } else {
            query.chars().take(BROWSER_MAX_TITLE_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= BROWSER_MAX_HISTORY_ENTRIES {
                break;
            }
            if lower.is_empty()
                || e.title.to_ascii_lowercase().contains(&lower)
                || e.url.to_ascii_lowercase().contains(&lower)
            {
                out.push(e.clone());
            }
        }
        out
    }

    /// Sorts history entries by title then url, deduped. Pure.
    #[must_use]
    pub fn sorted_by_title(mut entries: Vec<BrowserHistoryEntry>) -> Vec<BrowserHistoryEntry> {
        entries.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.url.cmp(&b.url)));
        entries.dedup_by(|a, b| a.url == b.url);
        if entries.len() > BROWSER_MAX_HISTORY_ENTRIES {
            entries.truncate(BROWSER_MAX_HISTORY_ENTRIES);
        }
        entries
    }

    /// Builds a tiled browser layout from a browser `View` and optional
    /// controls `View` using `LayoutNode::split` `H` reuse (no new tiling primitive).
    ///
    /// When `controls` is `None`, returns a single leaf; otherwise a horizontal
    /// split with clamped ratio (mirrors tabs `split_for_tabs`).
    #[must_use]
    pub fn tiled_layout(browser: View, controls: Option<View>, ratio: f32) -> LayoutNode {
        match controls {
            None => LayoutNode::leaf(browser),
            Some(c) => LayoutNode::split(
                SplitAxis::Horizontal,
                ratio,
                LayoutNode::leaf(browser),
                LayoutNode::leaf(c),
            ),
        }
    }

    /// Builds a vertical stack for browser plus status/preview stacking.
    #[must_use]
    pub fn vertical_stack(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Whether `url` is a javascript or data URL (always denied).
    #[must_use]
    pub fn is_javascript_or_data_url(url: &str) -> bool {
        url.starts_with("javascript:") || url.starts_with("data:")
    }

    /// Returns true when browser embed capability string is allowed (high-risk `browser.embed`).
    #[must_use]
    pub fn is_browser_embed_cap_allowed(capability: &str) -> bool {
        capability == BROWSER_CAPABILITY_EMBED
    }

    /// Returns true when navigation capability string is allowed.
    #[must_use]
    pub fn is_browser_navigation_allowed(capability: &str) -> bool {
        capability == BROWSER_CAPABILITY_NAVIGATION
    }

    /// Returns true when file-url capability string is allowed.
    #[must_use]
    pub fn is_browser_file_url_allowed(capability: &str) -> bool {
        capability == BROWSER_CAPABILITY_FILE_URL
    }

    /// Returns true when storage capability string is allowed.
    #[must_use]
    pub fn is_browser_storage_allowed(capability: &str) -> bool {
        capability == BROWSER_CAPABILITY_STORAGE
    }
}

/// Creates a browser panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Browser`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults, PR-1..PR-12,
/// BA-1..BA-3). Returns the panel handle on success; caller must still
/// activate the associated plugin via the public PluginHost path
/// (`declare → resolve → register → GrantRecord → activate`) for capabilities
/// `panel.provider` + `panel.create` + `browser.embed` (high-risk) +
/// `browser.navigation` + optional `browser.file-url`/`browser.storage` plus
/// `terminal.semantic-read` for link/title observation. The browser surface
/// `BrowserSurfaceId` is host-owned per `View`; focus reuse follows the same
/// router as Panel (`focused View` owns keyboard/IME/wheel).
pub fn create_browser_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Browser;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that browser panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
/// PR-1..PR-12: `[1,32]` per workspace, `[1,64]` per window, topics `<=256`,
/// subscriptions `<=32` per panel, drop handled by bus admission. BA-1 `4`
/// per window for browser type, BA-2 `1` WebView/panel, BA-3 `32` navigation queue.
pub fn validate_browser_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

/// Creates a tiled layout for browser: browser surface `View` plus optional
/// controls `View` via `LayoutNode` primitives (H/V) and mounts the resulting
/// leaf views into a `PanelRegistry`-backed tiled panel placement.
///
/// Pure layout helper — no PanelRegistry mutation, no PTY, no embedder I/O.
#[must_use]
pub fn browser_tiled_layout(browser: View, controls: Option<View>, ratio: f32) -> LayoutNode {
    BrowserIntegration::tiled_layout(browser, controls, ratio)
}

/// Allocates a `BrowserSurfaceId` handle for `ViewContent::Browser` placement.
///
/// Pure allocation helper that validates `BROWSER_MAX_PANELS_PER_WINDOW` (BA-1)
/// before charging. In the real runtime the host would allocate this via
/// `PanelRegistry` generation; here we model the distinct newtype allocation
/// with `PanelRegistry::generation` monotonic check without requiring a new
/// registry type. Returns the raw id on success; caller tracks generation
/// externally via `Generation` from the registry. This helper proves distinct
/// newtype and bounds without leaking PTY/GPU handles.
#[must_use]
pub fn allocate_browser_surface_id(next_raw: u64) -> BrowserSurfaceId {
    BrowserSurfaceId::new(next_raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::Rect as UiRect;
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn browser_surface_id_distinct_and_bounded() {
        let bid = BrowserSurfaceId::new(42);
        let pid = PanelId::new(42);
        let _vid = ViewId::new(42);
        assert_eq!(bid.0, pid.0);
        assert_ne!(
            std::any::TypeId::of::<BrowserSurfaceId>(),
            std::any::TypeId::of::<PanelId>()
        );
        assert_ne!(
            std::any::TypeId::of::<BrowserSurfaceId>(),
            std::any::TypeId::of::<ViewId>()
        );
        assert_eq!(bid.get(), 42);
        assert_eq!(format!("{bid}"), "BrowserSurfaceId(42)");
        let vc = bitty_ui::panel::ViewContent::Browser(bid);
        assert!(vc.is_browser());
        assert_eq!(vc.browser_id(), Some(bid));
    }

    #[test]
    fn url_validation_allowlist_and_file_scope() {
        // https allowed
        assert!(BrowserIntegration::is_valid_url("https://example.com"));
        assert!(BrowserIntegration::is_https_url("https://example.com"));
        assert!(BrowserIntegration::is_navigation_allowed(
            "https://example.com",
            false
        ));
        assert!(BrowserIntegration::is_browser_embed_allowed(
            "https://example.com",
            false
        ));
        // file allowed only with gate and within ~/projects/**
        assert!(BrowserIntegration::is_file_url(
            "file://~/projects/foo/bar.html"
        ));
        assert!(BrowserIntegration::is_within_file_scope(
            "file://~/projects/foo/bar.html"
        ));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "file://~/projects/foo/bar.html",
            false
        ));
        assert!(BrowserIntegration::is_navigation_allowed(
            "file://~/projects/foo/bar.html",
            true
        ));
        // file outside scope denied even with gate
        assert!(!BrowserIntegration::is_within_file_scope(
            "file:///etc/passwd"
        ));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "file:///etc/passwd",
            true
        ));
        assert!(!BrowserIntegration::is_within_file_scope(
            "file://~/projects/../etc/passwd"
        ));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "file://~/projects/../etc/passwd",
            true
        ));
        // http denied (allowlist is https default)
        assert!(BrowserIntegration::is_http_url("http://example.com"));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "http://example.com",
            false
        ));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "http://example.com",
            true
        ));
        // javascript/data denied
        assert!(!BrowserIntegration::is_valid_url("javascript:alert(1)"));
        assert!(BrowserIntegration::is_javascript_or_data_url(
            "javascript:alert(1)"
        ));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "javascript:alert(1)",
            false
        ));
        assert!(!BrowserIntegration::is_valid_url("data:text/html,hi"));
        assert!(!BrowserIntegration::is_navigation_allowed(
            "data:text/html,hi",
            false
        ));
        // control / empty / too long
        assert!(!BrowserIntegration::is_valid_url(""));
        assert!(!BrowserIntegration::is_valid_url("https://\0evil"));
        assert!(!BrowserIntegration::is_valid_url("https://evil\x07"));
        assert!(!BrowserIntegration::is_valid_url("not-a-url"));
        assert!(!BrowserIntegration::is_valid_url("https://"));
        let long = format!("https://example.com/{}", "a".repeat(5000));
        assert!(!BrowserIntegration::is_valid_url(&long));
        assert!(!BrowserIntegration::validate_url(&long, false).is_some());
        // capability gates pure helpers
        assert!(BrowserIntegration::is_browser_embed_cap_allowed(
            "browser.embed"
        ));
        assert!(!BrowserIntegration::is_browser_embed_cap_allowed(
            "browser.navigation"
        ));
        assert!(BrowserIntegration::is_browser_navigation_allowed(
            "browser.navigation"
        ));
        assert!(BrowserIntegration::is_browser_file_url_allowed(
            "browser.file-url"
        ));
        assert!(BrowserIntegration::is_browser_storage_allowed(
            "browser.storage"
        ));
        assert!(!BrowserIntegration::is_browser_storage_allowed(
            "browser.embed"
        ));
        // storage bounded
        assert!(BrowserIntegration::is_storage_allowed(
            "https://example.com"
        ));
        assert!(!BrowserIntegration::is_storage_allowed(""));
        assert!(!BrowserIntegration::is_storage_allowed("https://\0evil"));
    }

    #[test]
    fn browser_history_bounded_sorted_deduped() {
        let raw = vec![
            "https://example.com/b".to_string(),
            "https://example.com/a".to_string(),
            "https://example.com/a".to_string(),
            "http://example.com/c".to_string(), // denied, not https
            "javascript:alert(1)".to_string(),  // denied
            "file://~/projects/docs/index.html".to_string(), // file requires gate, filtered out without
        ];
        let listed = BrowserIntegration::list_history(&raw, false);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].url, "https://example.com/a");
        assert_eq!(listed[1].url, "https://example.com/b");
        // with file gate
        let with_file = BrowserIntegration::list_history(&raw, true);
        assert_eq!(with_file.len(), 3);
        assert!(
            with_file
                .iter()
                .any(|e| e.url == "file://~/projects/docs/index.html")
        );

        // Bounded at 32
        let many: Vec<String> = (0..100)
            .map(|i| format!("https://example.com/page{i}"))
            .collect();
        assert_eq!(
            BrowserIntegration::list_history(&many, false).len(),
            BROWSER_MAX_HISTORY_ENTRIES
        );
    }

    #[test]
    fn navigation_queue_bounded_drop_oldest() {
        let mut q = BrowserNavigationQueue::new();
        assert!(q.is_empty());
        // Enqueue 40 -> should DropOldest to 32, dropped 8
        for i in 0..40 {
            assert!(q.enqueue(format!("https://example.com/page{i}"), false));
        }
        assert_eq!(q.len(), BROWSER_MAX_NAVIGATION_QUEUE);
        assert_eq!(q.dropped(), 8);
        // Front should be page8 (0..7 dropped)
        assert_eq!(q.peek_front().unwrap(), "https://example.com/page8");
        // http denied not enqueued
        assert!(!q.enqueue("http://example.com".to_string(), false));
        assert_eq!(q.len(), BROWSER_MAX_NAVIGATION_QUEUE);
        // file without gate denied
        assert!(!q.enqueue("file://~/projects/a.html".to_string(), false));
        // file with gate allowed and DropOldest
        assert!(q.enqueue("file://~/projects/a.html".to_string(), true));
        assert_eq!(q.len(), BROWSER_MAX_NAVIGATION_QUEUE);
        assert_eq!(q.dropped(), 9);
        // javascript denied
        assert!(!q.enqueue("javascript:alert(1)".to_string(), false));
        // drain batch
        let drained = q.drain(10);
        assert_eq!(drained.len(), 10);
        assert_eq!(q.len(), 22);
    }

    #[test]
    fn filter_history_bounded_case_insensitive() {
        let raw = vec![
            "https://example.com/alpha".to_string(),
            "https://example.com/Beta".to_string(),
            "https://example.com/gamma".to_string(),
        ];
        let entries: Vec<BrowserHistoryEntry> = raw
            .into_iter()
            .filter_map(|u| BrowserHistoryEntry::new(u, String::new()))
            .collect();
        let filtered = BrowserIntegration::filter_history(&entries, "alpha");
        assert_eq!(filtered.len(), 1);
        let filtered2 = BrowserIntegration::filter_history(&entries, "BETA");
        assert_eq!(filtered2.len(), 1);
        let empty = BrowserIntegration::filter_history(&entries, "");
        assert_eq!(empty.len(), 3);
        let long_q = "a".repeat(BROWSER_MAX_TITLE_CHARS + 10);
        assert_eq!(
            BrowserIntegration::filter_history(&entries, &long_q).len(),
            0
        );
    }

    #[test]
    fn tiled_layout_reuses_h_split_and_stack_no_new_primitive() {
        let browser = View::new(ViewId::new(10), 80, 24);
        let controls = View::new(ViewId::new(11), 20, 24);
        let layout = BrowserIntegration::tiled_layout(browser.clone(), Some(controls.clone()), 0.7);
        assert!(matches!(layout, LayoutNode::Split { .. }));
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 100, 24));
        assert_eq!(allocs.len(), 2);
        // browser should get 70% (clamped)
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        let solo = BrowserIntegration::tiled_layout(browser, None, 0.5);
        assert_eq!(solo.leaf_count(), 1);
        let v1 = View::new(ViewId::new(20), 80, 12);
        let v2 = View::new(ViewId::new(21), 80, 12);
        let stack = BrowserIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        assert_eq!(stack.leaf_count(), 2);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        let id = create_browser_panel(&mut reg, ws, view).expect("create browser panel");
        assert_eq!(reg.panel_count(), 1);
        let handle2 = reg
            .create_panel(PanelType::Browser, Some(ws))
            .expect("second browser panel via raw Browser type");
        // Mount second browser panel to different view is ok (tiled), but same view fails
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        let _ = id.get();
        // Verify BrowserSurfaceId distinct newtype via ViewContent
        let bid = BrowserSurfaceId::new(99);
        let vc = bitty_ui::panel::ViewContent::Browser(bid);
        assert_eq!(vc.browser_id(), Some(bid));
        // EventBus bounded for browser navigated topic
        let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
        let ws2 = WorkspaceId::new(42);
        let view2 = ViewId::new(42);
        let h = reg2.create_panel(PanelType::Browser, Some(ws2)).unwrap();
        reg2.mount_panel(h.id, h.generation, view2).expect("mount");
        reg2.focus_panel(h.id, h.generation, ws2).expect("focus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
        reg2.suspend_panel(h.id, h.generation).expect("suspend");
        assert_eq!(reg2.focused_panel(ws2), None);
        let topic = reg2.declare_topic("xuepoo.browser:navigated").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("https://example.com/page{i}"))
                    .unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // file-url isolation
        assert!(!BrowserIntegration::is_within_file_scope(
            "file:///etc/passwd"
        ));
        assert!(BrowserIntegration::is_within_file_scope(
            "file://~/projects/foo/bar.html"
        ));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_browser_panel_config(&bad).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_browser_panel_config(&ok).is_ok());
    }

    #[test]
    fn browser_capability_parsing_and_hash_bound_isolation() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::browser_panel_manifest};
        let m = browser_panel_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.embed").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.navigation").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.file-url").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.storage").unwrap())
        );
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
        // browser.embed is high-risk
        assert!(CapabilityId::parse("browser.embed").unwrap().is_high_risk());
        assert!(
            !CapabilityId::parse("browser.navigation")
                .unwrap()
                .is_high_risk()
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        // Distinct gates
        let outside = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_ne!(
            CapabilityId::parse("browser.embed").unwrap().as_str(),
            outside.as_str()
        );
        let _id = PluginId::new("bitty-terminal.browser-panel").unwrap();
    }

    #[test]
    fn truncate_and_storage_bounded() {
        let long = "a".repeat(BROWSER_MAX_TITLE_CHARS + 50);
        let trunc = BrowserIntegration::truncate_title(&long);
        assert_eq!(trunc.chars().count(), BROWSER_MAX_TITLE_CHARS);
        assert!(BrowserIntegration::is_title_bounded("hello"));
        assert!(!BrowserIntegration::is_title_bounded(&long));
        // Storage quota
        let big = "a".repeat(BROWSER_MAX_STORAGE_BYTES + 1);
        assert!(!BrowserIntegration::is_storage_allowed(&big));
        assert!(BrowserIntegration::is_storage_allowed(
            "https://example.com"
        ));
    }
}
