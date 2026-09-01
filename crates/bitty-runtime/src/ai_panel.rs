#![forbid(unsafe_code)]
//! AI panel via Panel Runtime — tiled Panel + AgentId/AgentWorkspace 32KiB budget, bounded, via Panel(PanelId).
//!
//! This module is the first-party `bitty-terminal.ai-panel` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). AI panel is a
//! tiled `Panel(PanelId)` agent surface (`PanelType::Helper`) plus optional
//! Browser view snapshot for context capture (CTX-0120 Option A) for chat,
//! tool invocation, memory presentation, and consent surface with four levels
//! `inspect`/`self`/`workspace`/`all` with ephemeral scope. It verifies
//! `PanelId` distinct newtype with no `From` bridge to `ViewId`/`TerminalId`/
//! `BrowserSurfaceId`, `Generation` monotonic with reserve `1024` and
//! fail-closed exhaustion, lifecycle
//! `Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed` with
//! validated transitions, command registry `owner.name:command` qualified
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`, `<=128` chars)
//! duplicates rejected per-type `32` bound, overlay max `4` plus `1` modal
//! with modal exclusivity and text `128`/tooltip `256` bounds, `Palette` kind,
//! EventBus with three levels `64`/`1024`/`256 KiB`/`8192`/`2 MiB` and `8 KiB`
//! payload `DropOldest` default with counted per-queue attribution and
//! coalescing for observation topics (PR-1..PR-12, `PanelRegistry`
//! single-process `winit` one-registry-per-window headless,
//! `ViewContent::Panel`), capability isolation per `(PanelId, generation)`/
//! `(AgentId, generation)` deny-by-default via `CapabilityId` panel family
//! `panel.provider`/`panel.create` plus agent family
//! `agent.context.terminal`/`agent.context.workspace`/`agent.memory:persist`
//! plus `mcp.invoke:TOOL` per-tool and `ai.provider`/`ai.stream`/`ai.model`
//! — no ambient authority, no first-party bypass. `AgentWorkspace` ephemeral
//! `64` files / `2 MiB` aggregate / `256 KiB` per file is pure bounded helpers
//! (`AiPanelIntegration::is_workspace_file_allowed` etc.), Context Budget
//! `32 KiB` per turn is enforced at char boundary with truncation and
//! `remaining_budget` accounting. `AgentMemory` conversational `32` turns /
//! `64 KiB` aggregate is a bounded ring with `DropOldest` and counted drops.
//! Tool Bus via MCP adapter is bounded framing `256 KiB` frame,
//! `512 KiB` in-flight, depth `32`, `RC-9`/`RC-10` strict (validated before
//! publish). Tiled workspace reuses `LayoutNode` `H`/`V` with panel content,
//! not a PTY, bounded `32` leaves. No parser, renderer, or input hot path is
//! entered, and no grid mutation ever occurs here (only `Action` writes
//! `State` per Terminal State RFC). Default is disabled (fresh
//! `EffectiveConfig` has empty `plugins`); `bitty --safe` rejects
//! `bitty-terminal.*` as non-builtin without panic, identical to third-party
//! `xuepoo.*` parity (no private channel). Bounded queues
//! (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload,
//! `32`/`8 KiB` batch) and single-process `winit` `PanelRegistry` per window
//! are verified headlessly. `forbid(unsafe)`, `T-10` `is_untrusted_surface`
//! labeled observations remain untrusted data, never authority.

use bitty_ui::{LayoutNode, SplitAxis, View, ViewId, panel::MAX_OVERLAY_TEXT_LEN};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

// ---------------------------------------------------------------------------
// Capability patterns — each a distinct gate, no ambient
// ---------------------------------------------------------------------------

/// Panel provider capability for ai-panel.
pub const AI_PANEL_CAPABILITY_PANEL_PROVIDER: &str = "panel.provider";
/// Panel create capability for ai-panel.
pub const AI_PANEL_CAPABILITY_PANEL_CREATE: &str = "panel.create";
/// Agent terminal context capability (per Terminal with generation, 32KiB budget).
pub const AI_PANEL_CAPABILITY_AGENT_CONTEXT_TERMINAL: &str = "agent.context.terminal";
/// Agent workspace context capability (per Workspace, 32KiB budget).
pub const AI_PANEL_CAPABILITY_AGENT_CONTEXT_WORKSPACE: &str = "agent.context.workspace";
/// Agent memory persist capability — opt-in only (`0600`, `<=7 days`, no exfiltration), requires param `persist`.
pub const AI_PANEL_CAPABILITY_AGENT_MEMORY_PERSIST: &str = "agent.memory:persist";
/// MCP invoke prefix — per-tool capability `mcp.invoke:TOOL` where TOOL is a bounded tool name.
pub const AI_PANEL_CAPABILITY_MCP_INVOKE_PREFIX: &str = "mcp.invoke:";
/// AI provider capability.
pub const AI_PANEL_CAPABILITY_AI_PROVIDER: &str = "ai.provider";
/// AI stream capability.
pub const AI_PANEL_CAPABILITY_AI_STREAM: &str = "ai.stream";
/// AI model selection capability (alias of stream selection).
pub const AI_PANEL_CAPABILITY_AI_MODEL: &str = "ai.model";

// ---------------------------------------------------------------------------
// Bounded resource table (PR-1..PR-12 reuse + BA-7..BA-10 + AI panel)
// ---------------------------------------------------------------------------

/// Maximum AI panels per workspace — mirrors `MAX_PANELS_PER_WORKSPACE` 32 PR-1.
pub const AI_PANEL_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
/// Maximum AI panels per window — mirrors `MAX_PANELS_PER_WINDOW` 64 PR-2 but BA-7 `4` aggregate for ai type (candidate).
pub const AI_PANEL_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;
/// BA-7 candidate: maximum agents per window (ai panels) — `4` bounded `[1,8]`.
pub const AI_PANEL_MAX_AGENTS_PER_WINDOW: usize = 4;
/// Maximum pending MCP navigations? Not used; BA-8 `1` session per panel (strict).
pub const AI_PANEL_MAX_SESSIONS_PER_PANEL: usize = 1;
/// Maximum AgentWorkspace files per AgentId — `64` ephemeral.
pub const AI_PANEL_MAX_WORKSPACE_FILES: usize = 64;
/// Maximum AgentWorkspace aggregate bytes — `2 MiB`.
pub const AI_PANEL_MAX_WORKSPACE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum per-file bytes in AgentWorkspace — `256 KiB`.
pub const AI_PANEL_MAX_FILE_BYTES: usize = 256 * 1024;
/// Context Budget per turn — `32 KiB` strict per `Stable Id hierarchy`.
pub const AI_PANEL_CONTEXT_BUDGET_BYTES: usize = 32 * 1024;
/// AgentMemory conversational max turns — `32`.
pub const AI_PANEL_MEMORY_MAX_TURNS: usize = 32;
/// AgentMemory aggregate max bytes — `64 KiB`.
pub const AI_PANEL_MEMORY_MAX_BYTES: usize = 64 * 1024;
/// MCP frame max bytes — `256 KiB` (IPC framing `MAX_FRAME_BYTES` reuse).
pub const AI_PANEL_MCP_MAX_FRAME_BYTES: usize = 256 * 1024;
/// MCP in-flight max bytes — `512 KiB`.
pub const AI_PANEL_MCP_MAX_IN_FLIGHT_BYTES: usize = 512 * 1024;
/// MCP max depth — `32` pending tool calls per agent (mirrors `MAX_TOOL_CALLS_PER_TURN*4`).
pub const AI_PANEL_MCP_MAX_DEPTH: usize = 32;
/// Panel payload for AI observations is bounded by `BUS_EVENT_MAX_BYTES` `8 KiB` at bus admission.
pub const AI_PANEL_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;
/// Maximum selection size — bounded `64` mirroring per-subscription bound (PR-7).
pub const AI_PANEL_MAX_SELECTION: usize = crate::registry::BUS_PER_SUBSCRIPTION_LIMIT;
/// Maximum path bytes — mirrors parser `BoundedString::MAX_LEN` `4096` and project path bound.
pub const AI_PANEL_MAX_PATH_BYTES: usize = 4096;
/// Maximum chars per title/overlay — `128` (MAX_OVERLAY_TEXT_LEN).
pub const AI_PANEL_MAX_TITLE_CHARS: usize = MAX_OVERLAY_TEXT_LEN;
/// Message content max bytes — `32 KiB` mirrors `MAX_MESSAGE_BYTES` in bitty-agent.
pub const AI_PANEL_MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// Per-tool args max bytes — `16 KiB` mirrors `MAX_TOOL_ARGS_BYTES`.
pub const AI_PANEL_MAX_TOOL_ARGS_BYTES: usize = 16 * 1024;
/// Per-tool result max bytes — `16 KiB` mirrors `MAX_TOOL_RESULT_BYTES`.
pub const AI_PANEL_MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Canonical ai-panel commands (qualified `owner.name:command`).
pub const AI_PANEL_COMMAND_OPEN: &str = "bitty-terminal.ai-panel:open";
pub const AI_PANEL_COMMAND_SEND: &str = "bitty-terminal.ai-panel:send";
pub const AI_PANEL_COMMAND_CLEAR: &str = "bitty-terminal.ai-panel:clear";
pub const AI_PANEL_COMMAND_NEW_SESSION: &str = "bitty-terminal.ai-panel:new-session";
pub const AI_PANEL_COMMAND_STOP: &str = "bitty-terminal.ai-panel:stop";

fn truncate_bounded(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let truncated: String = s.chars().take(max_chars).collect();
    (truncated, true)
}

// ---------------------------------------------------------------------------
// Pure data types for workspace files and memory entries (bounded, owned)
// ---------------------------------------------------------------------------

/// Single AgentWorkspace file — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentWorkspaceFile {
    /// File name/path relative to ephemeral workspace, bounded `<=4096` bytes, no `..`, no control.
    pub path: String,
    /// Content bounded `<=256 KiB` per file, truncated at char boundary when constructed via helper.
    pub content: String,
    /// Whether content was truncated at the per-file bound.
    pub truncated: bool,
    /// Size in bytes of stored content.
    pub bytes: usize,
}

impl AgentWorkspaceFile {
    /// Creates a file from `path` and `content`, validating both. Returns `None` for invalid paths or oversized content that cannot be truncated deterministically.
    #[must_use]
    pub fn from_parts(path: String, content: String) -> Option<Self> {
        if !AiPanelIntegration::is_valid_workspace_path(&path) {
            return None;
        }
        if content.len() > AI_PANEL_MAX_FILE_BYTES {
            // Truncate at char boundary to 256 KiB
            let truncated_content = truncate_to_bytes(&content, AI_PANEL_MAX_FILE_BYTES);
            let bytes = truncated_content.len();
            return Some(Self {
                path,
                content: truncated_content,
                truncated: true,
                bytes,
            });
        }
        let bytes = content.len();
        // Also reject content containing NUL / control that would be unsafe?
        if content.contains('\0') {
            return None;
        }
        Some(Self {
            path,
            content,
            truncated: false,
            bytes,
        })
    }
}

fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // Truncate at char boundary without splitting UTF-8
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_owned();
    // Ensure we didn't cut in middle of multi-byte that would still be valid but we already aligned.
    // If we truncated, ensure content still bounded.
    if out.len() > max_bytes {
        out.truncate(max_bytes);
    }
    out
}

/// Single conversational memory turn — bounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMemoryEntry {
    /// Role label: `user`, `assistant`, `tool`, `system`.
    pub role: String,
    /// Bounded content `<=32 KiB` (per-turn Context Budget mirrored) but aggregate enforced by memory helper.
    pub content: String,
    /// Whether entry was truncated.
    pub truncated: bool,
}

impl AgentMemoryEntry {
    /// Creates entry with bounded content; truncates at `AI_PANEL_MAX_MESSAGE_BYTES` if needed.
    #[must_use]
    pub fn new(role: impl Into<String>, content: String) -> Option<Self> {
        let role = role.into();
        if role.is_empty() || role.len() > 32 {
            return None;
        }
        if role.contains('\0') || role.chars().any(|c| c.is_control()) {
            return None;
        }
        if content.contains('\0') {
            return None;
        }
        if content.len() > AI_PANEL_MAX_MESSAGE_BYTES {
            let truncated = truncate_to_bytes(&content, AI_PANEL_MAX_MESSAGE_BYTES);
            return Some(Self {
                role,
                content: truncated,
                truncated: true,
            });
        }
        Some(Self {
            role,
            content,
            truncated: false,
        })
    }
}

// ---------------------------------------------------------------------------
// AiPanelIntegration — pure, observation-only helpers over committed state
// ---------------------------------------------------------------------------

/// AiPanelIntegration — pure, observation-only helpers for the `ai-panel` candidate.
///
/// No mutation of `State`, no hot-path, bounded `<=64` files / `<=32` turns,
/// `32 KiB` context budget, `256 KiB` MCP framing, tiled `LayoutNode` `H`/`V`
/// reuse, no new tiling primitive, `forbid(unsafe)`.
#[derive(Debug, Clone, Copy)]
pub struct AiPanelIntegration;

impl AiPanelIntegration {
    // -- AgentId validation -------------------------------------------------

    /// Whether `id` is a valid `AgentId` (`owner.name`, bounded 128, segment grammar).
    #[must_use]
    pub fn is_valid_agent_id(id: &str) -> bool {
        bitty_agent::AgentId::new(id).is_ok()
    }

    /// Whether `candidate` is the exact `agent.context.terminal` capability string.
    #[must_use]
    pub fn is_agent_context_terminal_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AGENT_CONTEXT_TERMINAL
    }

    /// Whether `candidate` is the exact `agent.context.workspace` capability string.
    #[must_use]
    pub fn is_agent_context_workspace_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AGENT_CONTEXT_WORKSPACE
    }

    /// Whether `candidate` is the exact `agent.memory:persist` capability string.
    #[must_use]
    pub fn is_agent_memory_persist_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AGENT_MEMORY_PERSIST
    }

    /// Whether `candidate` is allowed for `ai.provider`.
    #[must_use]
    pub fn is_ai_provider_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AI_PROVIDER
    }

    /// Whether `candidate` is allowed for `ai.stream`.
    #[must_use]
    pub fn is_ai_stream_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AI_STREAM
    }

    /// Whether `candidate` is allowed for `ai.model`.
    #[must_use]
    pub fn is_ai_model_allowed(candidate: &str) -> bool {
        candidate == AI_PANEL_CAPABILITY_AI_MODEL
    }

    /// Whether `candidate` is a valid `mcp.invoke:TOOL` capability where TOOL is bounded tool name.
    #[must_use]
    pub fn is_mcp_invoke_allowed(candidate: &str) -> bool {
        if !candidate.starts_with(AI_PANEL_CAPABILITY_MCP_INVOKE_PREFIX) {
            return false;
        }
        let tool = &candidate[AI_PANEL_CAPABILITY_MCP_INVOKE_PREFIX.len()..];
        Self::is_valid_mcp_tool_name(tool)
    }

    /// Whether `tool` is a valid MCP tool name (`^[a-z][a-z0-9_.-]*$`, `<=64` bytes).
    #[must_use]
    pub fn is_valid_mcp_tool_name(tool: &str) -> bool {
        if tool.is_empty() || tool.len() > 64 {
            return false;
        }
        if tool.contains('\0') || tool.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return false;
        }
        let first = tool.as_bytes()[0];
        if !first.is_ascii_lowercase() {
            return false;
        }
        for b in tool.bytes() {
            if !(b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'_'
                || b == b'.'
                || b == b'-')
            {
                return false;
            }
        }
        true
    }

    // -- Workspace path helpers ---------------------------------------------

    /// Whether `path` is a valid bounded workspace path (no `..`, no null, `<=4096` bytes, no control, non-empty).
    ///
    /// Pure bounded check; symlink/device checks are host-level and deferred, mirroring `FileManagerIntegration`.
    #[must_use]
    pub fn is_valid_workspace_path(path: &str) -> bool {
        if path.is_empty() || path.len() > AI_PANEL_MAX_PATH_BYTES {
            return false;
        }
        if path.contains('\0') {
            return false;
        }
        if path.chars().any(|c| c.is_control()) {
            return false;
        }
        if path.contains("..") {
            return false;
        }
        // Disallow absolute or leading slash — workspace is ephemeral relative
        if path.starts_with('/') || path.starts_with('\\') {
            return false;
        }
        // No leading ~/ either — ai workspace is ephemeral, not ~/projects
        // But allow alphanumeric, dot, dash, slash segments.
        true
    }

    /// Whether `path` is allowed for workspace file read/write (same as valid path for ephemeral 64/2MiB/256KiB).
    #[must_use]
    pub fn is_workspace_file_allowed(path: &str) -> bool {
        Self::is_valid_workspace_path(path)
    }

    /// Whether `content` is bounded at `AI_PANEL_MAX_FILE_BYTES` (256 KiB).
    #[must_use]
    pub fn is_file_content_bounded(content: &str) -> bool {
        content.len() <= AI_PANEL_MAX_FILE_BYTES
    }

    /// Truncates `content` to `AI_PANEL_MAX_FILE_BYTES` at byte/char boundary.
    #[must_use]
    pub fn truncate_file_content(content: &str) -> String {
        truncate_to_bytes(content, AI_PANEL_MAX_FILE_BYTES)
    }

    /// Whether aggregated workspace `files` respect `64` files and `2 MiB` aggregate.
    #[must_use]
    pub fn is_workspace_within_limits(files: &[AgentWorkspaceFile]) -> bool {
        if files.len() > AI_PANEL_MAX_WORKSPACE_FILES {
            return false;
        }
        let total: usize = files.iter().map(|f| f.bytes).sum();
        if total > AI_PANEL_MAX_WORKSPACE_BYTES {
            return false;
        }
        for f in files {
            if f.bytes > AI_PANEL_MAX_FILE_BYTES {
                return false;
            }
            if !Self::is_valid_workspace_path(&f.path) {
                return false;
            }
        }
        true
    }

    /// Filters and bounds a raw `paths` listing to `64` valid workspace files, sorted, deduped. Pure observation.
    #[must_use]
    pub fn list_workspace_files(entries: &[AgentWorkspaceFile]) -> Vec<AgentWorkspaceFile> {
        let mut out: Vec<AgentWorkspaceFile> = entries
            .iter()
            .filter(|e| {
                Self::is_valid_workspace_path(&e.path) && e.bytes <= AI_PANEL_MAX_FILE_BYTES
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        if out.len() > AI_PANEL_MAX_WORKSPACE_FILES {
            out.truncate(AI_PANEL_MAX_WORKSPACE_FILES);
        }
        // Enforce 2 MiB aggregate by truncating largest tail deterministically after sort
        let mut total: usize = out.iter().map(|f| f.bytes).sum();
        while total > AI_PANEL_MAX_WORKSPACE_BYTES && !out.is_empty() {
            // Remove last (lexically largest) to keep determinism
            if let Some(removed) = out.pop() {
                total = total.saturating_sub(removed.bytes);
            } else {
                break;
            }
        }
        out
    }

    /// Remaining workspace bytes given `files`.
    #[must_use]
    pub fn workspace_remaining_bytes(files: &[AgentWorkspaceFile]) -> usize {
        let used: usize = files.iter().map(|f| f.bytes).sum();
        AI_PANEL_MAX_WORKSPACE_BYTES.saturating_sub(used)
    }

    // -- Context budget helpers ---------------------------------------------

    /// Whether `text` fits Context Budget `32 KiB`.
    #[must_use]
    pub fn is_context_bounded(text: &str) -> bool {
        text.len() <= AI_PANEL_CONTEXT_BUDGET_BYTES
    }

    /// Truncates `text` to `32 KiB` at byte/char boundary.
    #[must_use]
    pub fn truncate_context(text: &str) -> String {
        truncate_to_bytes(text, AI_PANEL_CONTEXT_BUDGET_BYTES)
    }

    /// Remaining context budget bytes after accounting for `parts`.
    #[must_use]
    pub fn remaining_context_budget(parts: &[String]) -> usize {
        let used: usize = parts.iter().map(|s| s.len()).sum();
        AI_PANEL_CONTEXT_BUDGET_BYTES.saturating_sub(used)
    }

    /// Assembles a bounded context from `parts` (Stable Id hierarchy `Instance->Window->Workspace->View->Terminal`)
    /// truncated deterministically to `32 KiB` aggregate. Returns `(assembled, truncated)`.
    #[must_use]
    pub fn assemble_context(parts: &[String]) -> (String, bool) {
        let mut out = String::new();
        let mut truncated = false;
        for part in parts {
            if out.len() + part.len() + 1 > AI_PANEL_CONTEXT_BUDGET_BYTES {
                // Need to truncate this part and stop.
                let remaining = AI_PANEL_CONTEXT_BUDGET_BYTES
                    .saturating_sub(out.len())
                    .saturating_sub(1);
                if remaining > 0 {
                    let piece = truncate_to_bytes(part, remaining);
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&piece);
                }
                truncated = true;
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(part);
        }
        // Ensure final is within budget
        if out.len() > AI_PANEL_CONTEXT_BUDGET_BYTES {
            out = truncate_to_bytes(&out, AI_PANEL_CONTEXT_BUDGET_BYTES);
            truncated = true;
        }
        (out, truncated)
    }

    /// Validates that `text` is bounded for context (32KiB) and contains no NUL.
    #[must_use]
    pub fn validate_context(text: &str) -> Option<String> {
        if text.contains('\0') {
            return None;
        }
        if !Self::is_context_bounded(text) {
            return None;
        }
        Some(text.to_owned())
    }

    // -- Memory helpers ------------------------------------------------------

    /// Whether `entries` respects `32` turns / `64 KiB` aggregate.
    #[must_use]
    pub fn is_memory_within_limits(entries: &[AgentMemoryEntry]) -> bool {
        if entries.len() > AI_PANEL_MEMORY_MAX_TURNS {
            return false;
        }
        let total: usize = entries.iter().map(|e| e.content.len()).sum();
        if total > AI_PANEL_MEMORY_MAX_BYTES {
            return false;
        }
        true
    }

    /// Bounded memory push with `DropOldest` when `32`/`64 KiB` exceeded. Pure helper: returns truncated vec and whether oldest was dropped.
    #[must_use]
    pub fn bounded_memory_push(
        mut entries: Vec<AgentMemoryEntry>,
        new: AgentMemoryEntry,
    ) -> (Vec<AgentMemoryEntry>, bool) {
        entries.push(new);
        let mut dropped = false;
        while entries.len() > AI_PANEL_MEMORY_MAX_TURNS {
            entries.remove(0);
            dropped = true;
        }
        let mut total: usize = entries.iter().map(|e| e.content.len()).sum();
        while total > AI_PANEL_MEMORY_MAX_BYTES && !entries.is_empty() {
            let removed = entries.remove(0);
            total = total.saturating_sub(removed.content.len());
            dropped = true;
        }
        (entries, dropped)
    }

    /// Filters memory entries by case-insensitive substring `query` over `role` or `content`, bounded.
    #[must_use]
    pub fn filter_memory(entries: &[AgentMemoryEntry], query: &str) -> Vec<AgentMemoryEntry> {
        let bounded_query = if query.chars().count() <= AI_PANEL_MAX_TITLE_CHARS {
            query.to_owned()
        } else {
            query.chars().take(AI_PANEL_MAX_TITLE_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= AI_PANEL_MEMORY_MAX_TURNS {
                break;
            }
            if lower.is_empty()
                || e.role.to_ascii_lowercase().contains(&lower)
                || e.content.to_ascii_lowercase().contains(&lower)
            {
                out.push(e.clone());
            }
        }
        out
    }

    // -- MCP framing helpers -------------------------------------------------

    /// Whether `payload` fits MCP frame `256 KiB`.
    #[must_use]
    pub fn is_mcp_frame_bounded(payload: &[u8]) -> bool {
        payload.len() <= AI_PANEL_MCP_MAX_FRAME_BYTES
    }

    /// Whether `payload` fits MCP args `16 KiB`.
    #[must_use]
    pub fn is_mcp_args_bounded(args: &str) -> bool {
        args.len() <= AI_PANEL_MAX_TOOL_ARGS_BYTES
    }

    /// Whether `result` fits MCP result `16 KiB`.
    #[must_use]
    pub fn is_mcp_result_bounded(result: &str) -> bool {
        result.len() <= AI_PANEL_MAX_TOOL_RESULT_BYTES
    }

    /// Whether `pending` count is within MCP depth `32`.
    #[must_use]
    pub fn is_mcp_depth_within(pending: usize) -> bool {
        pending <= AI_PANEL_MCP_MAX_DEPTH
    }

    /// Whether `in_flight_bytes` is within `512 KiB` in-flight cap.
    #[must_use]
    pub fn is_mcp_in_flight_within(in_flight_bytes: usize) -> bool {
        in_flight_bytes <= AI_PANEL_MCP_MAX_IN_FLIGHT_BYTES
    }

    /// Validates MCP invoke request for `tool` with `args` against bounded caps.
    /// Returns `Some(tool_capability)` on success (`mcp.invoke:TOOL`), `None` when bounds violated.
    #[must_use]
    pub fn validate_mcp_invoke(tool: &str, args: &str) -> Option<String> {
        if !Self::is_valid_mcp_tool_name(tool) {
            return None;
        }
        if !Self::is_mcp_args_bounded(args) {
            return None;
        }
        if args.contains('\0') {
            return None;
        }
        Some(format!("{}{}", AI_PANEL_CAPABILITY_MCP_INVOKE_PREFIX, tool))
    }

    /// Truncates `text` to overlay/title bound at char boundary (`128`).
    #[must_use]
    pub fn truncate_title(text: &str) -> String {
        let (s, _) = truncate_bounded(text, AI_PANEL_MAX_TITLE_CHARS);
        s
    }

    /// Whether `text` is bounded at title `128`.
    #[must_use]
    pub fn is_title_bounded(text: &str) -> bool {
        text.chars().count() <= AI_PANEL_MAX_TITLE_CHARS
    }

    // -- Tiled layout --------------------------------------------------------

    /// Builds a tiled ai-panel layout from a main `View` and optional `View` (e.g., chat vs context inspector)
    /// using `LayoutNode::split` `H` reuse (no new tiling primitive).
    ///
    /// When `secondary` is `None`, returns single leaf; otherwise a horizontal split with clamped ratio.
    #[must_use]
    pub fn tiled_layout(main: View, secondary: Option<View>, ratio: f32) -> LayoutNode {
        match secondary {
            None => LayoutNode::leaf(main),
            Some(s) => LayoutNode::split(
                SplitAxis::Horizontal,
                ratio,
                LayoutNode::leaf(main),
                LayoutNode::leaf(s),
            ),
        }
    }

    /// Builds a vertical stack for ai-panel details (e.g., chat + tool-output).
    #[must_use]
    pub fn vertical_stack(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Builds a tiled layout for ai-panel with a `Panel(PanelId)` + Browser snapshot for context capture (CTX-0120 Option A).
    /// Pure helper that reuses H split — secondary may represent the Browser `View`.
    #[must_use]
    pub fn tiled_with_browser_snapshot(
        panel_view: View,
        browser_snapshot_view: Option<View>,
        ratio: f32,
    ) -> LayoutNode {
        Self::tiled_layout(panel_view, browser_snapshot_view, ratio)
    }
}

/// Creates an AI panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults, PR-1..PR-12).
/// Returns the panel handle on success; caller must still activate the
/// associated plugin via the public PluginHost path (`declare → resolve →
/// register → GrantRecord → activate`) for capabilities
/// `panel.provider` + `panel.create` + `agent.context.terminal` +
/// `agent.context.workspace` + `agent.memory:persist` + `mcp.invoke:TOOL` +
/// `ai.provider`/`ai.stream`/`ai.model`. The `AgentId` + `AgentWorkspace`
/// pair is ephemeral and generation-scoped; no filesystem `~/projects/**`
/// is implied and no `~` expansion is performed here.
pub fn create_ai_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that AI panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
/// PR-1..PR-12: `[1,32]` per workspace, `[1,64]` per window, topics `<=256`,
/// subscriptions `<=32` per panel, drop handled by bus admission.
/// BA-7 `4` per window for ai type, BA-8 `1` session/panel, BA-9/BA-10 context/memory 32KiB/64KiB.
pub fn validate_ai_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

/// Creates a tiled layout for AI: panel `View` plus optional context `View` via `LayoutNode` primitives.
#[must_use]
pub fn ai_panel_tiled_layout(main: View, context: Option<View>, ratio: f32) -> LayoutNode {
    AiPanelIntegration::tiled_layout(main, context, ratio)
}

/// Allocates an `AgentId` string helper — validates bounded `owner.name`.
#[must_use]
pub fn validate_agent_id(id: &str) -> Option<String> {
    if bitty_agent::AgentId::new(id).is_ok() {
        Some(id.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::Rect as UiRect;
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn agent_id_distinct_and_bounded() {
        let aid = bitty_agent::AgentId::new("local.assistant").unwrap();
        let pid = PanelId::new(1);
        assert_eq!(aid.as_str(), "local.assistant");
        assert_eq!(aid.owner(), "local");
        assert_eq!(aid.name(), "assistant");
        assert!(AiPanelIntegration::is_valid_agent_id("local.assistant"));
        assert!(!AiPanelIntegration::is_valid_agent_id(""));
        assert!(!AiPanelIntegration::is_valid_agent_id("Local.assistant"));
        assert!(!AiPanelIntegration::is_valid_agent_id(
            "local.assistant.extra"
        ));
        let long = format!("{}.{}", "a".repeat(64), "b".repeat(64));
        assert!(!AiPanelIntegration::is_valid_agent_id(&long));
        // Distinct newtype checks
        assert_ne!(
            std::any::TypeId::of::<bitty_agent::AgentId>(),
            std::any::TypeId::of::<PanelId>()
        );
        assert_ne!(
            std::any::TypeId::of::<bitty_agent::AgentId>(),
            std::any::TypeId::of::<ViewId>()
        );
        assert_eq!(pid.get(), ViewId::new(1).0);
    }

    #[test]
    fn workspace_file_bounded_sorted_deduped() {
        let _f1 =
            AgentWorkspaceFile::from_parts("src/main.rs".into(), "fn main() {}".into()).unwrap();
        let _f2 = AgentWorkspaceFile::from_parts("README.md".into(), "# hi".into()).unwrap();
        let f3 = AgentWorkspaceFile::from_parts("../evil".into(), "hack".into());
        assert!(f3.is_none());
        assert!(AiPanelIntegration::is_valid_workspace_path("src/main.rs"));
        assert!(!AiPanelIntegration::is_valid_workspace_path("../evil"));
        assert!(!AiPanelIntegration::is_valid_workspace_path(
            "/absolute/path"
        ));
        assert!(!AiPanelIntegration::is_valid_workspace_path(""));
        assert!(!AiPanelIntegration::is_valid_workspace_path("a/b\0c"));
        assert!(AiPanelIntegration::is_workspace_file_allowed(
            "notes/todo.txt"
        ));
        assert!(!AiPanelIntegration::is_workspace_file_allowed("../escape"));

        let many_files: Vec<AgentWorkspaceFile> = (0..100)
            .map(|i| {
                AgentWorkspaceFile::from_parts(format!("file{i}.txt"), format!("content {i}"))
                    .unwrap()
            })
            .collect();
        let listed = AiPanelIntegration::list_workspace_files(&many_files);
        assert_eq!(listed.len(), AI_PANEL_MAX_WORKSPACE_FILES);
        // Deduped and sorted
        let dup = vec![
            AgentWorkspaceFile::from_parts("a.txt".into(), "hi".into()).unwrap(),
            AgentWorkspaceFile::from_parts("a.txt".into(), "hi2".into()).unwrap(),
        ];
        assert_eq!(AiPanelIntegration::list_workspace_files(&dup).len(), 1);
        // Aggregate 2MiB: create files each 64KiB -> 32 files fills 2MiB, 33rd would exceed
        let big_content = "x".repeat(64 * 1024);
        let big_files: Vec<AgentWorkspaceFile> = (0..40)
            .map(|i| {
                AgentWorkspaceFile::from_parts(format!("big{i}.bin"), big_content.clone()).unwrap()
            })
            .collect();
        let listed_big = AiPanelIntegration::list_workspace_files(&big_files);
        assert!(listed_big.len() * 64 * 1024 <= AI_PANEL_MAX_WORKSPACE_BYTES);
        assert!(AiPanelIntegration::is_workspace_within_limits(&listed_big));
        let used: usize = listed_big.iter().map(|f| f.bytes).sum();
        assert_eq!(
            AiPanelIntegration::workspace_remaining_bytes(&listed_big),
            AI_PANEL_MAX_WORKSPACE_BYTES.saturating_sub(used)
        );
        // Per-file 256KiB truncate
        let huge = "a".repeat(AI_PANEL_MAX_FILE_BYTES + 100);
        let huge_file = AgentWorkspaceFile::from_parts("huge.bin".into(), huge).unwrap();
        assert_eq!(huge_file.bytes, AI_PANEL_MAX_FILE_BYTES);
        assert!(huge_file.truncated);
        // Control rejected
        assert!(AgentWorkspaceFile::from_parts("bad\0file".into(), "hi".into()).is_none());
    }

    #[test]
    fn context_budget_32kib_enforced() {
        assert_eq!(AI_PANEL_CONTEXT_BUDGET_BYTES, 32 * 1024);
        let small = "hello world".to_string();
        assert!(AiPanelIntegration::is_context_bounded(&small));
        assert_eq!(AiPanelIntegration::truncate_context(&small), small);
        let big = "a".repeat(AI_PANEL_CONTEXT_BUDGET_BYTES + 500);
        assert!(!AiPanelIntegration::is_context_bounded(&big));
        assert_eq!(
            AiPanelIntegration::truncate_context(&big).len(),
            AI_PANEL_CONTEXT_BUDGET_BYTES
        );
        // assemble respects stable order and truncates at 32KiB
        let parts = vec![
            "x".repeat(16 * 1024),
            "y".repeat(16 * 1024),
            "z".repeat(16 * 1024),
        ];
        let (assembled, truncated) = AiPanelIntegration::assemble_context(&parts);
        assert_eq!(assembled.len(), AI_PANEL_CONTEXT_BUDGET_BYTES);
        assert!(truncated);
        // remaining budget
        let used_parts = vec!["a".repeat(1024), "b".repeat(1024)];
        assert_eq!(
            AiPanelIntegration::remaining_context_budget(&used_parts),
            AI_PANEL_CONTEXT_BUDGET_BYTES - 2048
        );
        assert!(AiPanelIntegration::validate_context("valid context").is_some());
        assert!(AiPanelIntegration::validate_context(&big).is_none());
        assert!(AiPanelIntegration::validate_context("bad\0context").is_none());
        // UTF-8 safe truncation
        let unicode = "é".repeat(AI_PANEL_CONTEXT_BUDGET_BYTES);
        // Each é is 2 bytes, so this is 2*32768 = 65536 bytes > 32 KiB, will be truncated at byte boundary respecting char
        let truncated_u = AiPanelIntegration::truncate_context(&unicode);
        assert!(truncated_u.len() <= AI_PANEL_CONTEXT_BUDGET_BYTES);
        assert!(truncated_u.is_char_boundary(truncated_u.len()));
    }

    #[test]
    fn memory_bounded_32turns_64kib() {
        let e1 = AgentMemoryEntry::new("user", "hello".into()).unwrap();
        let _e2 = AgentMemoryEntry::new("assistant", "hi".into()).unwrap();
        assert_eq!(e1.role, "user");
        assert!(!e1.truncated);
        let long_content = "a".repeat(AI_PANEL_MAX_MESSAGE_BYTES + 100);
        let e_long = AgentMemoryEntry::new("user", long_content).unwrap();
        assert!(e_long.truncated);
        assert_eq!(e_long.content.len(), AI_PANEL_MAX_MESSAGE_BYTES);
        // bounded push DropOldest for 32 turns
        let mut entries = Vec::new();
        let mut dropped_any = false;
        for i in 0..40 {
            let e = AgentMemoryEntry::new("user", format!("msg {i}")).unwrap();
            let (next, dropped) = AiPanelIntegration::bounded_memory_push(entries, e);
            entries = next;
            if dropped {
                dropped_any = true;
            }
        }
        assert_eq!(entries.len(), AI_PANEL_MEMORY_MAX_TURNS);
        assert!(dropped_any);
        assert!(entries[0].content.contains("msg 8")); // first 8 dropped
        assert!(AiPanelIntegration::is_memory_within_limits(&entries));
        // 64 KiB aggregate: push large messages to exceed 64 KiB
        let mut entries2 = Vec::new();
        let big = "b".repeat(8 * 1024);
        for _ in 0..10 {
            let e = AgentMemoryEntry::new("assistant", big.clone()).unwrap();
            let (next, _) = AiPanelIntegration::bounded_memory_push(entries2, e);
            entries2 = next;
        }
        let total: usize = entries2.iter().map(|e| e.content.len()).sum();
        assert!(total <= AI_PANEL_MEMORY_MAX_BYTES);
        assert!(entries2.len() <= AI_PANEL_MEMORY_MAX_TURNS);
        // filter bounded
        let filtered = AiPanelIntegration::filter_memory(&entries, "msg 10");
        assert!(filtered.len() <= AI_PANEL_MEMORY_MAX_TURNS);
        // invalid role
        assert!(AgentMemoryEntry::new("", "hi".into()).is_none());
        assert!(AgentMemoryEntry::new("bad\0role", "hi".into()).is_none());
        assert!(AgentMemoryEntry::new("user", "bad\0content".into()).is_none());
    }

    #[test]
    fn mcp_bounded_framing_and_per_tool_capability() {
        assert_eq!(AI_PANEL_MCP_MAX_FRAME_BYTES, 256 * 1024);
        assert_eq!(AI_PANEL_MCP_MAX_IN_FLIGHT_BYTES, 512 * 1024);
        assert_eq!(AI_PANEL_MCP_MAX_DEPTH, 32);
        assert!(AiPanelIntegration::is_valid_mcp_tool_name("read_file"));
        assert!(AiPanelIntegration::is_valid_mcp_tool_name("my-tool_1"));
        assert!(!AiPanelIntegration::is_valid_mcp_tool_name(""));
        assert!(!AiPanelIntegration::is_valid_mcp_tool_name("1tool"));
        assert!(!AiPanelIntegration::is_valid_mcp_tool_name("Tool"));
        assert!(!AiPanelIntegration::is_valid_mcp_tool_name(
            "tool with space"
        ));
        assert!(AiPanelIntegration::is_mcp_invoke_allowed(
            "mcp.invoke:read_file"
        ));
        assert!(!AiPanelIntegration::is_mcp_invoke_allowed(
            "mcp.invoke:BadTool"
        ));
        assert!(!AiPanelIntegration::is_mcp_invoke_allowed("mcp.invoke:"));
        assert!(!AiPanelIntegration::is_mcp_invoke_allowed("mcp.invoke"));
        assert!(AiPanelIntegration::is_mcp_frame_bounded(&[0u8; 100]));
        assert!(!AiPanelIntegration::is_mcp_frame_bounded(&vec![
            0u8;
            AI_PANEL_MCP_MAX_FRAME_BYTES
                + 1
        ]));
        assert!(AiPanelIntegration::is_mcp_args_bounded("{}"));
        assert!(!AiPanelIntegration::is_mcp_args_bounded(
            &"a".repeat(AI_PANEL_MAX_TOOL_ARGS_BYTES + 1)
        ));
        assert!(AiPanelIntegration::is_mcp_result_bounded("ok"));
        assert!(!AiPanelIntegration::is_mcp_result_bounded(
            &"a".repeat(AI_PANEL_MAX_TOOL_RESULT_BYTES + 1)
        ));
        assert!(AiPanelIntegration::is_mcp_depth_within(32));
        assert!(!AiPanelIntegration::is_mcp_depth_within(33));
        assert!(AiPanelIntegration::is_mcp_in_flight_within(512 * 1024));
        assert!(!AiPanelIntegration::is_mcp_in_flight_within(512 * 1024 + 1));
        let cap =
            AiPanelIntegration::validate_mcp_invoke("read_file", r#"{"path":"/tmp/x"}"#).unwrap();
        assert_eq!(cap, "mcp.invoke:read_file");
        assert!(AiPanelIntegration::validate_mcp_invoke("BadTool", "{}").is_none());
        assert!(
            AiPanelIntegration::validate_mcp_invoke(
                "read_file",
                &"a".repeat(AI_PANEL_MAX_TOOL_ARGS_BYTES + 1)
            )
            .is_none()
        );
        assert!(AiPanelIntegration::validate_mcp_invoke("read_file", "bad\0args").is_none());
        // Capability allow helpers
        assert!(AiPanelIntegration::is_agent_context_terminal_allowed(
            "agent.context.terminal"
        ));
        assert!(!AiPanelIntegration::is_agent_context_terminal_allowed(
            "agent.context.workspace"
        ));
        assert!(AiPanelIntegration::is_agent_context_workspace_allowed(
            "agent.context.workspace"
        ));
        assert!(AiPanelIntegration::is_agent_memory_persist_allowed(
            "agent.memory:persist"
        ));
        assert!(!AiPanelIntegration::is_agent_memory_persist_allowed(
            "agent.memory"
        ));
        assert!(AiPanelIntegration::is_ai_provider_allowed("ai.provider"));
        assert!(AiPanelIntegration::is_ai_stream_allowed("ai.stream"));
        assert!(AiPanelIntegration::is_ai_model_allowed("ai.model"));
        assert!(!AiPanelIntegration::is_ai_provider_allowed("ai.stream"));
    }

    #[test]
    fn tiled_layout_reuses_h_split_and_stack_no_new_primitive() {
        let main = View::new(ViewId::new(10), 80, 24);
        let secondary = View::new(ViewId::new(11), 80, 24);
        let layout = AiPanelIntegration::tiled_layout(main.clone(), Some(secondary.clone()), 0.5);
        assert!(matches!(layout, LayoutNode::Split { .. }));
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        let low = AiPanelIntegration::tiled_layout(main.clone(), Some(secondary.clone()), 0.01);
        let high = AiPanelIntegration::tiled_layout(main.clone(), Some(secondary.clone()), 0.99);
        for l in [low, high] {
            let a = l.layout(UiRect::new(0, 0, 80, 24));
            assert!(a[0].1.width >= 1 && a[1].1.width >= 1);
        }
        let nan = AiPanelIntegration::tiled_layout(main.clone(), Some(secondary), f32::NAN);
        assert_eq!(nan.layout(UiRect::new(0, 0, 80, 24)).len(), 2);
        let solo = AiPanelIntegration::tiled_layout(main, None, 0.5);
        assert_eq!(solo.leaf_count(), 1);
        let v1 = View::new(ViewId::new(20), 80, 12);
        let v2 = View::new(ViewId::new(21), 80, 12);
        let stack = AiPanelIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        assert_eq!(stack.leaf_count(), 2);
        let pv = View::new(ViewId::new(30), 80, 24);
        let bv = View::new(ViewId::new(31), 40, 24);
        let with_browser =
            AiPanelIntegration::tiled_with_browser_snapshot(pv.clone(), Some(bv.clone()), 0.6);
        assert_eq!(with_browser.leaf_count(), 2);
        let without_browser = AiPanelIntegration::tiled_with_browser_snapshot(pv, None, 0.6);
        assert_eq!(without_browser.leaf_count(), 1);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_ai_panel(&mut reg, ws, view).expect("create ai panel");
        assert_eq!(reg.panel_count(), 1);
        let handle2 = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel");
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        let _ = id.get();
        // focus lifecycle
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
        // EventBus bounded DropOldest for ai tool-output (non-coalescable topic)
        let topic = reg2.declare_topic("xuepoo.agent:tool-output").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("tool result {i}")).unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        assert!(AiPanelIntegration::is_mcp_frame_bounded(&[0u8; 100]));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_ai_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_ai_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_ai_panel_config(&ok).is_ok());
        let bad_topics = PanelRegistryConfig {
            max_topics_total: 257,
            ..Default::default()
        };
        assert!(validate_ai_panel_config(&bad_topics).is_err());
        let bad_subs = PanelRegistryConfig {
            max_subscriptions_per_panel: 33,
            ..Default::default()
        };
        assert!(validate_ai_panel_config(&bad_subs).is_err());
    }

    #[test]
    fn agent_and_mcp_capability_parsing_and_hash_bound_isolation() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::ai_panel_manifest};
        let m = ai_panel_manifest();
        let cap_tt = CapabilityId::parse("agent.context.terminal").unwrap();
        assert_eq!(cap_tt.family(), bitty_plugin_host::CapabilityFamily::Agent);
        let cap_ws = CapabilityId::parse("agent.context.workspace").unwrap();
        assert_eq!(cap_ws.family(), bitty_plugin_host::CapabilityFamily::Agent);
        let cap_mem = CapabilityId::parse("agent.memory:persist").unwrap();
        assert_eq!(cap_mem.family(), bitty_plugin_host::CapabilityFamily::Agent);
        assert!(cap_mem.has_param());
        let cap_mcp = CapabilityId::parse("mcp.invoke:read_file").unwrap();
        assert_eq!(cap_mcp.family(), bitty_plugin_host::CapabilityFamily::Mcp);
        assert!(cap_mcp.has_param());
        let cap_ai_p = CapabilityId::parse("ai.provider").unwrap();
        assert_eq!(cap_ai_p.family(), bitty_plugin_host::CapabilityFamily::Ai);
        let cap_ai_s = CapabilityId::parse("ai.stream").unwrap();
        assert_eq!(cap_ai_s.family(), bitty_plugin_host::CapabilityFamily::Ai);
        // manifest hash deterministic and panel.* present
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
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
                .contains(&CapabilityId::parse("agent.context.terminal").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("agent.context.workspace").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("agent.memory:persist").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ai.provider").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ai.stream").unwrap())
        );
        // mcp.invoke present
        let has_mcp = m
            .capabilities
            .ids
            .iter()
            .any(|c| c.as_str().starts_with("mcp.invoke:"));
        assert!(has_mcp);
        let _id = PluginId::new("bitty-terminal.ai-panel").unwrap();
        // Hash isolation: bump version breaks grant
        let hash = m.manifest_hash();
        let mut bumped = m.clone();
        bumped.identity.version = "0.2.0".to_string();
        assert_ne!(bumped.manifest_hash(), hash);
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
        let stack = AiPanelIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        let base = LayoutNode::leaf(View::new(ViewId::new(10), 80, 24));
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
    }

    #[test]
    fn truncate_and_title_bounded() {
        let long = "a".repeat(AI_PANEL_MAX_TITLE_CHARS + 50);
        let truncated = AiPanelIntegration::truncate_title(&long);
        assert_eq!(truncated.chars().count(), AI_PANEL_MAX_TITLE_CHARS);
        assert!(AiPanelIntegration::is_title_bounded("hello"));
        assert!(!AiPanelIntegration::is_title_bounded(&long));
        // Payload bound via registry already proven, but check constant
        assert_eq!(AI_PANEL_PAYLOAD_MAX_BYTES, 8192);
        assert!(AiPanelIntegration::is_context_bounded(
            &"a".repeat(32 * 1024)
        ));
        assert!(!AiPanelIntegration::is_context_bounded(
            &"a".repeat(32 * 1024 + 1)
        ));
    }
}
