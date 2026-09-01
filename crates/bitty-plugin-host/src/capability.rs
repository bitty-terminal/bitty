//! Capability identifier grammar and families (OQ-012, part 2).
//!
//! Proposed grammar (candidate, not normative): `family.resource[.scope]`,
//! lowercase, dot-separated, with optional parameterized form
//! `family.resource:parameter` for path and destination constraints.
//! Identifiers are closed symbols; plugins cannot invent families.

use crate::error::PluginError;

/// All capability families proposed for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapabilityFamily {
    Terminal,
    Ui,
    Clipboard,
    Fs,
    Process,
    Network,
    Runtime,
    Debug,
    Platform,
    /// Protocol registration and dispatch authority.
    Protocol,
    /// Panel runtime (generic Panel Runtime per OQ-014 pre-study).
    Panel,
    /// Browser embed/navigation/storage for WebView surface (CTX-0110, BA-1..BA-3).
    Browser,
    /// Agent context, memory and workspace (CTX-0111, ai-panel).
    Agent,
    /// MCP tool invocation per-tool (CTX-0111, ai-panel).
    Mcp,
    /// AI provider/stream/model (CTX-0111, ai-panel).
    Ai,
}

impl CapabilityFamily {
    /// Parse a family string (lowercase).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "terminal" => Some(Self::Terminal),
            "ui" => Some(Self::Ui),
            "clipboard" => Some(Self::Clipboard),
            "fs" => Some(Self::Fs),
            "process" => Some(Self::Process),
            "network" => Some(Self::Network),
            "runtime" => Some(Self::Runtime),
            "debug" => Some(Self::Debug),
            "platform" => Some(Self::Platform),
            "protocol" => Some(Self::Protocol),
            "panel" => Some(Self::Panel),
            "browser" => Some(Self::Browser),
            "agent" => Some(Self::Agent),
            "mcp" => Some(Self::Mcp),
            "ai" => Some(Self::Ai),
            _ => None,
        }
    }

    /// Family label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Ui => "ui",
            Self::Clipboard => "clipboard",
            Self::Fs => "fs",
            Self::Process => "process",
            Self::Network => "network",
            Self::Runtime => "runtime",
            Self::Debug => "debug",
            Self::Platform => "platform",
            Self::Protocol => "protocol",
            Self::Panel => "panel",
            Self::Browser => "browser",
            Self::Agent => "agent",
            Self::Mcp => "mcp",
            Self::Ai => "ai",
        }
    }

    /// Whether an absent grant denies every capability in this family.
    ///
    /// This deliberately returns `true` for every family. The host must grant
    /// individual, validated identifiers; family membership is never authority.
    #[must_use]
    pub const fn denied_without_grant(self) -> bool {
        true
    }

    /// The closed, non-parameterized identifiers in this family.
    #[must_use]
    pub const fn closed_identifiers(self) -> &'static [&'static str] {
        match self {
            Self::Terminal => &[
                "terminal.semantic-read",
                "terminal.raw-read",
                "terminal.input.self",
                "terminal.input.all",
                "terminal.manage",
            ],
            Self::Ui => &["ui.rich", "ui.overlay", "ui.protocol-register"],
            Self::Clipboard => &["clipboard.read", "clipboard.write"],
            Self::Fs => &["fs.read", "fs.write"],
            Self::Process => &["process.spawn"],
            Self::Network => &["network.connect"],
            Self::Runtime => &[
                "runtime.inspect",
                "runtime.configure",
                "runtime.plugin-manage",
            ],
            Self::Debug => &["debug.inspect", "debug.trace", "debug.control"],
            Self::Platform => &[
                "platform.notify",
                "platform.open-url",
                "platform.image-file",
            ],
            Self::Protocol => &["protocol.register"],
            Self::Panel => &[
                "panel.provider",
                "panel.create",
                "panel.focus",
                "panel.overlay",
            ],
            Self::Browser => &[
                "browser.embed",
                "browser.navigation",
                "browser.file-url",
                "browser.storage",
            ],
            Self::Agent => &[
                "agent.context.terminal",
                "agent.context.workspace",
                "agent.memory",
            ],
            Self::Mcp => &["mcp.invoke"],
            Self::Ai => &["ai.provider", "ai.stream", "ai.model"],
        }
    }
}

/// A validated capability identifier.
///
/// Validation enforces:
/// - deny by default (absent means denied),
/// - no wildcards, no `*`, no family-wide wildcard,
/// - `family.resource[.scope]` plus optional `:parameter`,
/// - closed identifier set (unknown resource fails validation instead of being ignored).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityId {
    raw: String,
    family: CapabilityFamily,
    /// Whether this identifier carries a `:` parameter (e.g. `fs.read:~/docs/**`).
    has_param: bool,
    /// Whether this identifier is flagged high-risk per RFC rule 3.
    high_risk: bool,
}

impl CapabilityId {
    /// Parse and validate a capability identifier string.
    pub fn parse(raw: &str) -> Result<Self, PluginError> {
        if raw.is_empty() {
            return Err(PluginError::capability(raw, "capability must not be empty"));
        }
        if raw.len() > 512 {
            return Err(PluginError::capability(
                raw,
                "capability id too long (max 512)",
            ));
        }
        if raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
            return Err(PluginError::capability(
                raw,
                "capability must not contain control characters or whitespace",
            ));
        }

        // Split on ':' to separate parameter (first ':' only — param may contain ':' for network host:port).
        let (head, param) = match raw.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (raw, None),
        };

        // Wildcards are not allowed in the identifier head (no allow-all); param globs for fs are allowed.
        if head.contains('*') {
            return Err(PluginError::capability(
                raw,
                "wildcards are not allowed (deny-by-default, no allow-all)",
            ));
        }

        if let Some(p) = param {
            if p.is_empty() {
                return Err(PluginError::capability(raw, "parameter must not be empty"));
            }
            if p.len() > 1024 {
                return Err(PluginError::capability(
                    raw,
                    "parameter too long (max 1024)",
                ));
            }
            // Param may contain ':', '/', '*', etc. for host:port and globs; controls and whitespace are rejected above.
            // No additional colon check here — network.connect:example.com:443 is valid.
        }

        // Head must be family.resource[.scope] (2 or 3 dot segments).
        let parts: Vec<&str> = head.split('.').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(PluginError::capability(
                raw,
                "capability must be family.resource or family.resource.scope",
            ));
        }

        // All segments must be lowercase alphanumeric, '-' or '_' (and must start with alpha).
        for seg in &parts {
            validate_segment(seg, raw)?;
        }

        let family = CapabilityFamily::parse(parts[0]).ok_or_else(|| {
            PluginError::capability(raw, format!("unknown family '{}'", parts[0]))
        })?;

        // Check closed identifier set: only known combos are accepted.
        let known = is_known_capability(head, param.is_some(), raw)?;
        let high_risk = is_high_risk(head);

        Ok(Self {
            raw: raw.to_string(),
            family,
            has_param: param.is_some(),
            high_risk: high_risk && known,
        })
    }

    /// Raw identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Capability family.
    #[must_use]
    pub fn family(&self) -> CapabilityFamily {
        self.family
    }

    /// Whether this identifier is flagged high-risk per RFC rule 3.
    ///
    /// High-risk: `terminal.input.all`, `terminal.raw-read`, `ui.protocol-register`,
    /// `debug.control`, and `runtime.plugin-manage`. Consent UI must present them
    /// distinctly and they cannot be granted implicitly via workspace config or
    /// service indirection.
    #[must_use]
    pub fn is_high_risk(&self) -> bool {
        self.high_risk
    }

    /// Whether the identifier carries a scoped parameter.
    #[must_use]
    pub fn has_param(&self) -> bool {
        self.has_param
    }
}

impl std::fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

fn validate_segment(seg: &str, raw: &str) -> Result<(), PluginError> {
    if seg.is_empty() {
        return Err(PluginError::capability(raw, "empty capability segment"));
    }
    if seg.len() > 64 {
        return Err(PluginError::capability(
            raw,
            "capability segment too long (max 64)",
        ));
    }
    let first = seg.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(PluginError::capability(
            raw,
            "capability segment must start with lowercase letter",
        ));
    }
    for b in seg.bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return Err(PluginError::capability(
                raw,
                "capability segment must be [a-z0-9_-]",
            ));
        }
    }
    Ok(())
}

fn is_high_risk(head: &str) -> bool {
    matches!(
        head,
        "terminal.input.all"
            | "terminal.raw-read"
            | "ui.protocol-register"
            | "debug.control"
            | "runtime.plugin-manage"
            | "browser.embed"
    )
}

/// Human-readable effect statement for consent dialogs (plain language).
#[must_use]
pub fn effect_statement(id: &CapabilityId) -> &'static str {
    match id.as_str().split(':').next().unwrap_or(id.as_str()) {
        "terminal.semantic-read" => "Read structured terminal content (bounded snapshot)",
        "terminal.raw-read" => "Read raw terminal bytes and full cell grid (high-risk)",
        "terminal.input.self" => "Observe input directed to this plugin's own terminals",
        "terminal.input.all" => "Observe all terminal input (high-risk)",
        "terminal.manage" => "Create and manage terminals",
        "ui.rich" => "Render rich blocks in the terminal",
        "ui.overlay" => "Show overlays and popups",
        "ui.protocol-register" => "Register custom URL protocols (high-risk)",
        "clipboard.read" => "Read clipboard contents",
        "clipboard.write" => "Write to clipboard",
        "fs.read" => "Read files matching the declared globs",
        "fs.write" => "Write files matching the declared globs",
        "process.spawn" => "Spawn the allowlisted program",
        "network.connect" => "Connect to the declared destination",
        "runtime.inspect" => "Inspect runtime state",
        "runtime.configure" => "Change runtime configuration",
        "runtime.plugin-manage" => "Manage other plugins (high-risk)",
        "debug.inspect" => "Inspect debug state (read-only)",
        "debug.trace" => "Enable tracing",
        "debug.control" => "Control the debugger (high-risk)",
        "platform.notify" => "Show system notifications",
        "platform.open-url" => "Open URLs in the default handler",
        "platform.image-file" => "Access image files at approved locations",
        "protocol.register" => "Register a protocol handler",
        "panel.provider" => "Provide panel types for the workspace",
        "panel.create" => "Create and manage panels",
        "panel.focus" => "Focus panels and control workspace focus",
        "panel.overlay" => "Show panel overlays and modals",
        "browser.embed" => "Embed browser surface via embedder (high-risk)",
        "browser.navigation" => "Navigate browser surface to allowlisted URLs",
        "browser.file-url" => "Allow file:// navigation validated against project scope",
        "browser.storage" => "Persist browser cookies/cache with bounded quota",
        "agent.context.terminal" => "Observe terminal context for this agent (bounded 32KiB)",
        "agent.context.workspace" => "Observe workspace context for this agent (bounded 32KiB)",
        "agent.memory" => "Persist agent conversational memory (opt-in, 0600, <=7 days)",
        "mcp.invoke" => "Invoke allowlisted MCP tool (per-tool, bounded frame 256KiB)",
        "ai.provider" => "Use allowlisted AI provider",
        "ai.stream" => "Stream AI responses for this agent",
        "ai.model" => "Select AI model for this agent (bounded)",
        _ => "Requested capability",
    }
}

/// Closed identifier validation.
///
/// Returns error if the head is not a known capability; otherwise returns true
/// for known identifiers (used to avoid silent escalation).
fn is_known_capability(head: &str, has_param: bool, raw: &str) -> Result<bool, PluginError> {
    // Families that require a parameter.
    let param_required = matches!(
        head,
        "fs.read"
            | "fs.write"
            | "process.spawn"
            | "network.connect"
            | "mcp.invoke"
            | "agent.memory"
    );

    // Validate param presence rules.
    if param_required && !has_param {
        return Err(PluginError::capability(
            raw,
            format!("capability '{head}' requires a ':PARAMETER'"),
        ));
    }
    if !param_required && has_param {
        return Err(PluginError::capability(
            raw,
            format!("capability '{head}' must not have a ':PARAMETER'"),
        ));
    }

    // Closed set: every accepted head must be one of the known identifiers.
    let is_known = matches!(
        head,
        "terminal.semantic-read"
            | "terminal.raw-read"
            | "terminal.input.self"
            | "terminal.input.all"
            | "terminal.manage"
            | "ui.rich"
            | "ui.overlay"
            | "ui.protocol-register"
            | "clipboard.read"
            | "clipboard.write"
            | "fs.read"
            | "fs.write"
            | "process.spawn"
            | "network.connect"
            | "runtime.inspect"
            | "runtime.configure"
            | "runtime.plugin-manage"
            | "debug.inspect"
            | "debug.trace"
            | "debug.control"
            | "platform.notify"
            | "platform.open-url"
            | "platform.image-file"
            | "protocol.register"
            | "panel.provider"
            | "panel.create"
            | "panel.focus"
            | "panel.overlay"
            | "browser.embed"
            | "browser.navigation"
            | "browser.file-url"
            | "browser.storage"
            | "agent.context.terminal"
            | "agent.context.workspace"
            | "agent.memory"
            | "mcp.invoke"
            | "ai.provider"
            | "ai.stream"
            | "ai.model"
    );

    if !is_known {
        return Err(PluginError::capability(
            raw,
            format!(
                "unknown capability '{head}' (closed set; forward compat requires explicit RFC)"
            ),
        ));
    }

    // For fs/process/network, param content could be further validated (glob / host / program)
    // but the draft keeps it as bounded opaque string validated above (length, no colon/space).
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_identifiers_parse() {
        for id in [
            "terminal.semantic-read",
            "ui.rich",
            "clipboard.read",
            "fs.read:~/Documents/**/*.md",
            "process.spawn:git",
            "network.connect:example.com:443",
            "runtime.inspect",
            "debug.trace",
            "platform.notify",
            "protocol.register",
        ] {
            assert!(CapabilityId::parse(id).is_ok(), "should parse {id}");
        }
    }

    #[test]
    fn wildcard_rejected() {
        assert!(CapabilityId::parse("fs.*").is_err());
        assert!(CapabilityId::parse("terminal.*").is_err());
    }

    #[test]
    fn every_family_is_closed_and_denied_without_grant() {
        let families = [
            CapabilityFamily::Fs,
            CapabilityFamily::Process,
            CapabilityFamily::Network,
            CapabilityFamily::Terminal,
            CapabilityFamily::Clipboard,
            CapabilityFamily::Ui,
            CapabilityFamily::Protocol,
            CapabilityFamily::Runtime,
            CapabilityFamily::Debug,
            CapabilityFamily::Panel,
            CapabilityFamily::Browser,
            CapabilityFamily::Agent,
            CapabilityFamily::Mcp,
            CapabilityFamily::Ai,
        ];
        for family in families {
            assert!(family.denied_without_grant());
            for raw in family.closed_identifiers() {
                let parsed = CapabilityId::parse(raw);
                if matches!(
                    family,
                    CapabilityFamily::Fs
                        | CapabilityFamily::Process
                        | CapabilityFamily::Network
                        | CapabilityFamily::Mcp
                ) || *raw == "agent.memory"
                {
                    assert!(
                        parsed.is_err(),
                        "scoped family must require a parameter: {raw}"
                    );
                } else {
                    assert!(parsed.is_ok(), "closed identifier must parse: {raw}");
                }
            }
        }
    }

    #[test]
    fn unknown_capability_rejected() {
        assert!(CapabilityId::parse("terminal.future-thing").is_err());
        assert!(CapabilityId::parse("ui.unknown").is_err());
    }

    #[test]
    fn param_required_for_fs() {
        assert!(CapabilityId::parse("fs.read").is_err());
        assert!(CapabilityId::parse("fs.write").is_err());
        assert!(CapabilityId::parse("fs.read:~/docs/*.md").is_ok());
    }

    #[test]
    fn param_forbidden_for_non_param() {
        assert!(CapabilityId::parse("terminal.semantic-read:extra").is_err());
        assert!(CapabilityId::parse("ui.rich:something").is_err());
    }

    #[test]
    fn high_risk_flags() {
        assert!(
            CapabilityId::parse("terminal.raw-read")
                .unwrap()
                .is_high_risk()
        );
        assert!(
            CapabilityId::parse("terminal.input.all")
                .unwrap()
                .is_high_risk()
        );
        assert!(
            CapabilityId::parse("ui.protocol-register")
                .unwrap()
                .is_high_risk()
        );
        assert!(CapabilityId::parse("debug.control").unwrap().is_high_risk());
        assert!(
            CapabilityId::parse("runtime.plugin-manage")
                .unwrap()
                .is_high_risk()
        );
        assert!(
            !CapabilityId::parse("terminal.semantic-read")
                .unwrap()
                .is_high_risk()
        );
    }

    #[test]
    fn invalid_segments() {
        assert!(CapabilityId::parse("Terminal.semantic-read").is_err());
        assert!(CapabilityId::parse("terminal.Semantic-read").is_err());
        assert!(CapabilityId::parse("terminal.").is_err());
        assert!(CapabilityId::parse(".terminal").is_err());
    }

    #[test]
    fn parameters_reject_controls_and_unicode_whitespace() {
        for parameter in ["path\0name", "path\u{0007}name", "path\u{2003}name"] {
            assert!(CapabilityId::parse(&format!("fs.read:{parameter}")).is_err());
        }
    }
}
