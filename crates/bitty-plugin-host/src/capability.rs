//! Capability identifier grammar and families (OQ-012, part 2).
//!
//! Proposed grammar (candidate, not normative): `family.resource[.scope]`,
//! lowercase, dot-separated, with optional parameterized form
//! `family.resource:parameter` for path and destination constraints.
//! Identifiers are closed symbols; plugins cannot invent families.

pub use bitty_package::catalog::CapabilityCatalog;

use crate::error::PluginError;

/// All capability families proposed for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapabilityFamily {
    Terminal,
    Ui,
    Clipboard,
    /// Host-mediated environment reads (`env.read:<KEY>`, CTX-0330).
    ///
    /// Keys name one variable each; values are read host-side through the
    /// `bitty.env` boundary, never through ambient `os.getenv` (which stays
    /// denied per ADR-0006). Grants are per-key and fail closed.
    Env,
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
    /// Layout algorithm proposals for the workspace compositor (CW-07).
    Layout,
    // CTX-0916 S4 (DEC-0102, breaking cutover): the Agent/Mcp/Ai families
    // left the Core seed (zero-AI default). Core-only installs reject
    // `agent.*`/`mcp.*`/`ai.*` fail-closed; they validate only through an
    // explicitly extended catalog (see `bitty_package::CapabilityCatalog`).
    /// Workspace L1 domain (ADR-0014, CTX-0889): `workspace.read` observes
    /// workspace identity/order/attention; `workspace.control` mutates
    /// workspaces through the same Core handlers as keybindings. Read never
    /// implies control.
    Workspace,
    /// Read-only history/search/selection family (RFC-0004, CTX-0955, W-139).
    ///
    /// A NEW family beside `terminal.*`, never an extension of it: scoped,
    /// bounded snapshot reads over the three queryable sources (segmented
    /// transcript, command history, own per-plugin KV). Session snapshots
    /// are never a queryable source. Every member is deny-by-default with
    /// explicit per-plugin, per-source scope; exact Lua spellings stay
    /// parked to W-139 (SDK) under a new root, never `bitty.terminal.*`.
    History,
}

impl CapabilityFamily {
    /// Parse a family string (lowercase).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "terminal" => Some(Self::Terminal),
            "ui" => Some(Self::Ui),
            "clipboard" => Some(Self::Clipboard),
            "env" => Some(Self::Env),
            "fs" => Some(Self::Fs),
            "process" => Some(Self::Process),
            "network" => Some(Self::Network),
            "runtime" => Some(Self::Runtime),
            "debug" => Some(Self::Debug),
            "platform" => Some(Self::Platform),
            "protocol" => Some(Self::Protocol),
            "panel" => Some(Self::Panel),
            "browser" => Some(Self::Browser),
            "layout" => Some(Self::Layout),
            "workspace" => Some(Self::Workspace),
            "history" => Some(Self::History),
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
            Self::Env => "env",
            Self::Fs => "fs",
            Self::Process => "process",
            Self::Network => "network",
            Self::Runtime => "runtime",
            Self::Debug => "debug",
            Self::Platform => "platform",
            Self::Protocol => "protocol",
            Self::Panel => "panel",
            Self::Browser => "browser",
            Self::Layout => "layout",
            Self::Workspace => "workspace",
            Self::History => "history",
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
                // W-82 additive v2 (CTX-0929 S-5): bounded PTY submission
                // through the Core paste pipeline.
                "terminal.input.submit",
                "terminal.manage",
            ],
            Self::Ui => &[
                "ui.rich",
                "ui.overlay",
                "ui.overlay.focus",
                "ui.protocol-register",
            ],
            Self::Clipboard => &["clipboard.read", "clipboard.write"],
            Self::Env => &["env.read"],
            Self::Fs => &["fs.read", "fs.write"],
            // W-82 additive v2 (CTX-0929 S-5): `process.editor` is the
            // allowlisted external-editor round trip (no parameter; the
            // editor program is allowlist-resolved by Core). It shares the
            // Process family with the parameterized `process.spawn`.
            Self::Process => &["process.editor", "process.spawn"],
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
            Self::Layout => &["layout.provider"],
            Self::Workspace => &["workspace.read", "workspace.control"],
            // RFC-0004 read-only history/search/selection family (CTX-0955).
            // New family beside `terminal.*`; the `Terminal` table above is
            // frozen and untouched by this family.
            Self::History => &[
                "history.transcript.read",
                "history.commands.read",
                "history.kv.read",
            ],
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
        // CTX-0916 S1 (DEC-0102): the static entry point delegates to the
        // Core catalog seed; behavior is unchanged.
        Self::parse_with(&CapabilityCatalog::core(), raw)
    }

    /// Parse and validate against an explicit catalog (CTX-0916 S1, DEC-0102).
    ///
    /// Shape rules and the [`CapabilityFamily`] vocabulary are Core-owned and
    /// identical to [`Self::parse`]; only closed-set membership and parameter
    /// presence come from `catalog`. S1 limitation: families outside the
    /// closed [`CapabilityFamily`] enum still fail closed here even when the
    /// catalog knows them (package-side `parse_with` already accepts them);
    /// opening the family vocabulary is a later slice. With
    /// [`CapabilityCatalog::core`] the result equals [`Self::parse`].
    pub fn parse_with(catalog: &CapabilityCatalog, raw: &str) -> Result<Self, PluginError> {
        Self::parse_impl(catalog, raw)
    }

    /// Shared parse implementation behind [`Self::parse`] and
    /// [`Self::parse_with`]; only the closed-set source varies.
    fn parse_impl(catalog: &CapabilityCatalog, raw: &str) -> Result<Self, PluginError> {
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
        let known = is_known_capability_with(catalog, head, param.is_some(), raw)?;
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
    /// `debug.control`, `runtime.plugin-manage`, `browser.embed`, and `env.read`.
    /// Consent UI must present them
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
            // Environment variables routinely carry credentials, so every
            // `env.read` grant gets distinct consent presentation.
            | "env.read"
    )
}

/// Human-readable effect statement for consent dialogs (plain language).
///
/// CTX-0916 S3 (DEC-0102): the Core-seeded default delegates to
/// [`effect_statement_with`] with [`CapabilityCatalog::core`], so existing
/// callers keep byte-identical behavior while extension-aware callers pass an
/// explicit catalog.
#[must_use]
pub fn effect_statement(id: &CapabilityId) -> &'static str {
    effect_statement_with(&CapabilityCatalog::core(), id)
}

/// Catalog-scoped effect statement sharing the Core-owned presentation strings
/// with [`effect_statement`] (CTX-0916 S3, DEC-0102).
///
/// Head membership comes from `catalog`: heads unknown to the catalog fall
/// back to `"Requested capability"`. Presentation strings stay Core-owned, so
/// extension heads in known families also fall back until an RFC assigns them
/// wording. With [`CapabilityCatalog::core`] the result equals
/// [`effect_statement`].
#[must_use]
pub fn effect_statement_with(catalog: &CapabilityCatalog, id: &CapabilityId) -> &'static str {
    let head = id.as_str().split(':').next().unwrap_or(id.as_str());
    if !catalog.contains_head(head) {
        return "Requested capability";
    }
    match head {
        "terminal.semantic-read" => "Read structured terminal content (bounded snapshot)",
        "terminal.raw-read" => "Read raw terminal bytes and full cell grid (high-risk)",
        "terminal.input.self" => "Observe input directed to this plugin's own terminals",
        "terminal.input.all" => "Observe all terminal input (high-risk)",
        "terminal.input.submit" => {
            "Submit bounded text to the focused panel through the paste pipeline"
        }
        "terminal.manage" => "Create and manage terminals",
        "ui.rich" => "Render rich blocks in the terminal",
        "ui.overlay" => "Show overlays and popups",
        "ui.overlay.focus" => "Acquire a focusable overlay and hold transient input capture",
        "ui.protocol-register" => "Register custom URL protocols (high-risk)",
        "clipboard.read" => "Read clipboard contents",
        "clipboard.write" => "Write to clipboard",
        "env.read" => "Read the named host environment variable (high-risk)",
        "fs.read" => "Read files matching the declared globs",
        "fs.write" => "Write files matching the declared globs",
        "process.spawn" => "Spawn the allowlisted program",
        "process.editor" => {
            "Edit text in the allowlisted external editor on a Core-owned temp file"
        }
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
        "layout.provider" => "Provide layout algorithms for the workspace",
        "workspace.read" => "List workspaces and observe workspace events (no terminal content)",
        "workspace.control" => "Create, close, rename, and focus workspaces and move panels",
        "history.transcript.read" => {
            "Read bounded redacted snapshots of the opt-in segmented transcript"
        }
        "history.commands.read" => "Read bounded redacted snapshots of command history",
        "history.kv.read" => "Read this plugin's own key-value namespace (bounded)",
        _ => "Requested capability",
    }
}

/// Canonical closed-identifier validation (CR-PKG-03 export).
///
/// The closed tables are Core-owned in `bitty_package` (the leaf crate; a
/// package-to-host dependency would be a dependency cycle). CTX-0916 S3
/// (DEC-0102): the Core-seeded default delegates to
/// [`validate_closed_capability_with`] with [`CapabilityCatalog::core`], so
/// existing callers keep byte-identical behavior while extension-aware callers
/// pass an explicit catalog. `bitty-package` manifest validation calls the
/// same Core seed, so an identifier accepted at manifest (lock) time is
/// accepted at install (grant) time and vice versa.
pub fn validate_closed_capability(
    head: &str,
    has_param: bool,
    raw: &str,
) -> Result<(), PluginError> {
    validate_closed_capability_with(&CapabilityCatalog::core(), head, has_param, raw)
}

/// Catalog-scoped closed-identifier validation sharing the host error
/// vocabulary with [`validate_closed_capability`] (CTX-0916 S3, DEC-0102).
///
/// Only closed-set membership and parameter presence come from `catalog`.
/// With [`CapabilityCatalog::core`] the result equals
/// [`validate_closed_capability`].
pub fn validate_closed_capability_with(
    catalog: &CapabilityCatalog,
    head: &str,
    has_param: bool,
    raw: &str,
) -> Result<(), PluginError> {
    catalog.check(head, has_param).map_err(|violation| {
        match violation {
            bitty_package::manifest::ClosedCapabilityViolation::UnknownHead => {
                PluginError::capability(
                    raw,
                    format!(
                        "unknown capability '{head}' (closed set; forward compat requires explicit RFC)"
                    ),
                )
            }
            bitty_package::manifest::ClosedCapabilityViolation::ParamRequired => {
                PluginError::capability(
                    raw,
                    format!("capability '{head}' requires a ':PARAMETER'"),
                )
            }
            bitty_package::manifest::ClosedCapabilityViolation::ParamForbidden => {
                PluginError::capability(
                    raw,
                    format!("capability '{head}' must not have a ':PARAMETER'"),
                )
            }
        }
    })
}

/// Closed identifier validation against a catalog.
///
/// Returns error if the head is not a known capability; otherwise returns true
/// for known identifiers (used to avoid silent escalation).
fn is_known_capability_with(
    catalog: &CapabilityCatalog,
    head: &str,
    has_param: bool,
    raw: &str,
) -> Result<bool, PluginError> {
    validate_closed_capability_with(catalog, head, has_param, raw)?;
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
            "env.read:HOME",
            "fs.read:~/Documents/**/*.md",
            "process.spawn:git",
            "network.connect:example.com:443",
            "runtime.inspect",
            "debug.trace",
            "platform.notify",
            "protocol.register",
            "layout.provider",
            "workspace.read",
            "workspace.control",
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
            CapabilityFamily::Env,
            CapabilityFamily::Ui,
            CapabilityFamily::Protocol,
            CapabilityFamily::Runtime,
            CapabilityFamily::Debug,
            CapabilityFamily::Panel,
            CapabilityFamily::Browser,
            CapabilityFamily::Layout,
            CapabilityFamily::Workspace,
            CapabilityFamily::History,
        ];
        for family in families {
            assert!(family.denied_without_grant());
            for raw in family.closed_identifiers() {
                // Bare heads parse exactly when no parameter is required;
                // parameterized heads (for example `process.spawn`) fail
                // bare and parse with a parameter. The Process family holds
                // both shapes since W-82 (`process.editor` takes no
                // parameter; the temp path never leaves Core).
                if bitty_package::manifest::capability_requires_param(raw) {
                    assert!(
                        CapabilityId::parse(raw).is_err(),
                        "scoped capability must require a parameter: {raw}"
                    );
                } else {
                    assert!(
                        CapabilityId::parse(raw).is_ok(),
                        "closed identifier must parse: {raw}"
                    );
                }
            }
        }
    }

    #[test]
    fn core_seed_rejects_ai_families_fail_closed() {
        // CTX-0916 S4 (DEC-0102): the Agent/Mcp/Ai families left the Core
        // seed (zero-AI default). Every former `agent.*`/`mcp.*`/`ai.*`
        // acceptance path now fails closed through the Core-seeded defaults:
        // unknown family at parse time, unknown head at validation time.
        // They validate only through an explicitly extended catalog (see
        // `ai_families_validate_only_via_extended_catalog`).
        use bitty_package::manifest as package_manifest;

        let core = CapabilityCatalog::core();
        assert!(!core.contains_family("agent"));
        assert!(!core.contains_family("mcp"));
        assert!(!core.contains_family("ai"));
        for raw in [
            "agent.context.terminal",
            "agent.context.workspace",
            "agent.memory:record-1",
            "mcp.invoke:mail.list",
            "ai.provider",
            "ai.stream",
            "ai.model",
        ] {
            assert!(
                CapabilityId::parse(raw).is_err(),
                "'{raw}' must fail closed on the Core seed"
            );
            assert!(
                CapabilityId::parse_with(&core, raw).is_err(),
                "'{raw}' must fail closed on the Core catalog"
            );
            assert!(
                bitty_package::CapabilityId::parse_with(&core, raw).is_err(),
                "'{raw}' must fail package validation on the Core catalog"
            );
            assert!(
                package_manifest::validate_capability_with(&core, raw).is_err(),
                "'{raw}' must fail manifest validation on the Core catalog"
            );
            let (head, has_param) = match raw.split_once(':') {
                Some((h, _)) => (h, true),
                None => (raw, false),
            };
            assert!(
                validate_closed_capability(head, has_param, raw).is_err(),
                "'{raw}' must fail host validation on the Core seed"
            );
            assert!(
                validate_closed_capability_with(&core, head, has_param, raw).is_err(),
                "'{raw}' must fail host validation on the Core catalog"
            );
        }
        // The family vocabulary itself no longer names them.
        assert_eq!(CapabilityFamily::parse("agent"), None);
        assert_eq!(CapabilityFamily::parse("mcp"), None);
        assert_eq!(CapabilityFamily::parse("ai"), None);
    }

    #[test]
    fn unknown_capability_rejected() {
        assert!(CapabilityId::parse("terminal.future-thing").is_err());
        assert!(CapabilityId::parse("ui.unknown").is_err());
    }

    #[test]
    fn composer_v2_capabilities_parse_without_parameter() {
        // W-82 additive v2 (CTX-0929 S-5): the composer capabilities are
        // closed, parameter-free, family-routed, and not high-risk (both
        // are bounded, attributed operations: lease-gated submit through
        // the inspected paste pipeline, allowlisted editor on a
        // Core-owned temp file).
        for (raw, family, statement) in [
            (
                "terminal.input.submit",
                CapabilityFamily::Terminal,
                "Submit bounded text to the focused panel through the paste pipeline",
            ),
            (
                "process.editor",
                CapabilityFamily::Process,
                "Edit text in the allowlisted external editor on a Core-owned temp file",
            ),
        ] {
            let granted = CapabilityId::parse(raw).expect("v2 composer capability parses");
            assert_eq!(granted.family(), family);
            assert!(!granted.has_param());
            assert!(!granted.is_high_risk());
            assert_eq!(effect_statement(&granted), statement);
            assert!(CapabilityId::parse(&format!("{raw}:param")).is_err());
        }
    }

    #[test]
    fn history_family_parses_without_parameter() {
        // RFC-0004 (CTX-0955): the history-read family is new and read-only
        // beside `terminal.*`. Scope travels with the host call, never as a
        // grant parameter (same precedent as `terminal.input.submit`).
        for raw in [
            "history.transcript.read",
            "history.commands.read",
            "history.kv.read",
        ] {
            let granted = CapabilityId::parse(raw).expect("history capability parses");
            assert_eq!(granted.family(), CapabilityFamily::History);
            assert_eq!(granted.family().as_str(), "history");
            assert!(!granted.has_param());
            assert!(CapabilityId::parse(&format!("{raw}:param")).is_err());
        }
        // Nothing reads this family's sources under a `terminal.*` spelling.
        assert!(CapabilityId::parse("terminal.history.read").is_err());
        assert!(CapabilityId::parse("terminal.transcript.read").is_err());
    }

    #[test]
    fn terminal_family_stays_frozen() {
        // RFC-0004 non-goal: no `terminal.*` member is widened,
        // reinterpreted, or given a read sub-scope. The v1 set is exactly
        // the six accepted identifiers.
        assert_eq!(
            CapabilityFamily::Terminal.closed_identifiers(),
            &[
                "terminal.semantic-read",
                "terminal.raw-read",
                "terminal.input.self",
                "terminal.input.all",
                "terminal.input.submit",
                "terminal.manage",
            ]
        );
    }

    #[test]
    fn param_required_for_fs() {
        assert!(CapabilityId::parse("fs.read").is_err());
        assert!(CapabilityId::parse("fs.write").is_err());
        assert!(CapabilityId::parse("fs.read:~/docs/*.md").is_ok());
    }

    #[test]
    fn env_read_requires_key_param() {
        assert!(CapabilityId::parse("env.read").is_err());
        assert!(CapabilityId::parse("env.read:HOME").is_ok());
        let granted = CapabilityId::parse("env.read:HOME").expect("env.read:HOME parses");
        assert_eq!(granted.family(), CapabilityFamily::Env);
        assert!(granted.has_param());
        assert!(granted.is_high_risk());
        assert_eq!(
            effect_statement(&granted),
            "Read the named host environment variable (high-risk)"
        );
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

    #[test]
    fn core_seed_equals_host_closed_set() {
        // CTX-0916 S2 (DEC-0102) (a) seed-equality: the package Core seed and
        // the host Core seed accept exactly the same set. CR-PKG-03 keeps the
        // package manifest validator and the host enforcing one shared closed
        // set; the host delegates to the canonical package tables.
        use bitty_package::manifest as package_manifest;

        let core = CapabilityCatalog::core();
        let mut host_heads: Vec<&str> = Vec::new();
        for family in [
            CapabilityFamily::Terminal,
            CapabilityFamily::Ui,
            CapabilityFamily::Clipboard,
            CapabilityFamily::Env,
            CapabilityFamily::Fs,
            CapabilityFamily::Process,
            CapabilityFamily::Network,
            CapabilityFamily::Runtime,
            CapabilityFamily::Debug,
            CapabilityFamily::Platform,
            CapabilityFamily::Protocol,
            CapabilityFamily::Panel,
            CapabilityFamily::Browser,
            CapabilityFamily::Layout,
            CapabilityFamily::Workspace,
            CapabilityFamily::History,
        ] {
            host_heads.extend(family.closed_identifiers().iter().copied());
        }

        // Three-way head equality: catalog Core seed, package static tables,
        // and host enum tables stay byte-identical.
        let mut host_sorted = host_heads.clone();
        host_sorted.sort_unstable();
        let mut package_sorted = package_manifest::CLOSED_CAPABILITY_HEADS.to_vec();
        package_sorted.sort_unstable();
        let mut catalog_sorted = core.heads();
        catalog_sorted.sort_unstable();
        assert_eq!(catalog_sorted, package_sorted);
        assert_eq!(host_sorted, package_sorted);

        // Parameter rules agree on every head.
        for head in package_manifest::CLOSED_CAPABILITY_HEADS {
            assert_eq!(
                core.requires_param(head),
                Some(package_manifest::capability_requires_param(head)),
                "param rule diverged for '{head}'"
            );
        }

        // Every closed head validates on both sides through the Core-seeded
        // defaults and through the explicit Core catalog.
        for head in package_manifest::CLOSED_CAPABILITY_HEADS {
            let raw = if package_manifest::capability_requires_param(head) {
                format!("{head}:param")
            } else {
                (*head).to_string()
            };
            assert!(
                CapabilityId::parse(&raw).is_ok(),
                "package-accepted '{raw}' must parse on the host"
            );
            assert!(
                bitty_package::CapabilityId::new(&raw).is_ok(),
                "host-accepted '{raw}' must validate in the package manifest"
            );
            assert!(
                CapabilityId::parse_with(&core, &raw).is_ok(),
                "catalog-accepted '{raw}' must parse on the host"
            );
            assert!(
                bitty_package::CapabilityId::parse_with(&core, &raw).is_ok(),
                "catalog-accepted '{raw}' must validate in the package manifest"
            );
            assert!(
                package_manifest::validate_capability_with(&core, &raw).is_ok(),
                "catalog-accepted '{raw}' must validate via manifest helper"
            );
        }

        // Divergent identifiers are rejected on both sides, on both paths.
        // CTX-0916 S4: removed AI heads fail closed here too (zero-AI
        // default; they validate only via an explicitly extended catalog).
        for raw in [
            "terminal.unknown-thing",
            "ui.unknown",
            "fs.read",
            "agent.context.terminal",
            "agent.memory:record-1",
            "mcp.invoke:mail.list",
            "ai.provider",
            "ai.stream",
            "ai.model",
        ] {
            assert!(CapabilityId::parse(raw).is_err());
            assert!(bitty_package::CapabilityId::new(raw).is_err());
            assert!(CapabilityId::parse_with(&core, raw).is_err());
            assert!(bitty_package::CapabilityId::parse_with(&core, raw).is_err());
        }
    }

    #[test]
    fn extension_contribution_validates_identically_on_both_sides() {
        // CTX-0916 S2 (DEC-0102) (b) additive-contribution: an extension head
        // registered in a known family validates identically on both sides
        // through the extended catalog, while the Core-seeded defaults on
        // both sides still fail closed and a fresh Core seed is unaffected.
        use bitty_package::manifest as package_manifest;

        let mut extended = CapabilityCatalog::core();
        extended
            .register(
                "panel",
                &[("panel.custom-view", false), ("panel.custom-search", false)],
            )
            .expect("panel extension registers");
        extended
            .register("fs", &[("fs.archive-read", true)])
            .expect("fs extension registers");

        // Parameter-free head: accepted bare, rejected with a parameter.
        for raw in ["panel.custom-view", "panel.custom-search"] {
            assert!(
                bitty_package::CapabilityId::parse_with(&extended, raw).is_ok(),
                "package must accept '{raw}' via extended catalog"
            );
            assert!(
                CapabilityId::parse_with(&extended, raw).is_ok(),
                "host must accept '{raw}' via extended catalog"
            );
            assert!(
                package_manifest::validate_capability_with(&extended, raw).is_ok(),
                "manifest helper must accept '{raw}' via extended catalog"
            );
            let with_param = format!("{raw}:param");
            assert!(
                bitty_package::CapabilityId::parse_with(&extended, &with_param).is_err(),
                "package must reject param on '{with_param}'"
            );
            assert!(
                CapabilityId::parse_with(&extended, &with_param).is_err(),
                "host must reject param on '{with_param}'"
            );
        }

        // Parameter-requiring head: accepted with a parameter, rejected bare.
        assert!(
            bitty_package::CapabilityId::parse_with(&extended, "fs.archive-read:scope-1").is_ok()
        );
        assert!(CapabilityId::parse_with(&extended, "fs.archive-read:scope-1").is_ok());
        assert!(
            package_manifest::validate_capability_with(&extended, "fs.archive-read:scope-1")
                .is_ok()
        );
        assert!(bitty_package::CapabilityId::parse_with(&extended, "fs.archive-read").is_err());
        assert!(CapabilityId::parse_with(&extended, "fs.archive-read").is_err());

        // Core-seeded defaults on both sides still fail closed.
        for raw in [
            "panel.custom-view",
            "panel.custom-search",
            "fs.archive-read:scope-1",
        ] {
            assert!(bitty_package::CapabilityId::new(raw).is_err());
            assert!(CapabilityId::parse(raw).is_err());
        }

        // A fresh Core seed is unaffected by the extension.
        let fresh = CapabilityCatalog::core();
        assert!(bitty_package::CapabilityId::parse_with(&fresh, "panel.custom-view").is_err());
        assert!(CapabilityId::parse_with(&fresh, "panel.custom-view").is_err());
    }

    #[test]
    fn ai_families_validate_only_via_extended_catalog() {
        // CTX-0916 S4 (DEC-0102): the AI families are the canonical additive
        // example. `ai.*`/`mcp.*`/`agent.*` validate ONLY through an
        // explicitly extended catalog; every Core-seeded default fails closed
        // (see `core_seed_rejects_ai_families_fail_closed`).
        use bitty_package::manifest as package_manifest;

        let mut extended = CapabilityCatalog::core();
        extended
            .register(
                "ai",
                &[
                    ("ai.provider", false),
                    ("ai.stream", false),
                    ("ai.model", false),
                ],
            )
            .expect("ai extension registers");
        extended
            .register("mcp", &[("mcp.invoke", true)])
            .expect("mcp extension registers");
        extended
            .register(
                "agent",
                &[
                    ("agent.context.terminal", false),
                    ("agent.context.workspace", false),
                    ("agent.memory", true),
                ],
            )
            .expect("agent extension registers");

        // Package side (catalog-open vocabulary): the extended catalog
        // accepts every contributed head with its parameter polarity ...
        for raw in [
            "ai.provider",
            "ai.stream",
            "ai.model",
            "mcp.invoke:mail.list",
            "agent.context.terminal",
            "agent.context.workspace",
            "agent.memory:record-1",
        ] {
            assert!(
                bitty_package::CapabilityId::parse_with(&extended, raw).is_ok(),
                "package must accept '{raw}' via extended catalog"
            );
            assert!(
                package_manifest::validate_capability_with(&extended, raw).is_ok(),
                "manifest helper must accept '{raw}' via extended catalog"
            );
        }
        assert!(bitty_package::CapabilityId::parse_with(&extended, "ai.provider:extra").is_err());
        assert!(bitty_package::CapabilityId::parse_with(&extended, "mcp.invoke").is_err());
        assert!(bitty_package::CapabilityId::parse_with(&extended, "agent.memory").is_err());

        // ... and the host closed-validation path (catalog-routed, enum-free)
        // agrees identically through the same extended catalog ...
        for (head, has_param, raw) in [
            ("ai.provider", false, "ai.provider"),
            ("ai.stream", false, "ai.stream"),
            ("ai.model", false, "ai.model"),
            ("mcp.invoke", true, "mcp.invoke:mail.list"),
            ("agent.context.terminal", false, "agent.context.terminal"),
            ("agent.context.workspace", false, "agent.context.workspace"),
            ("agent.memory", true, "agent.memory:record-1"),
        ] {
            assert!(
                validate_closed_capability_with(&extended, head, has_param, raw).is_ok(),
                "host validate path must accept '{raw}' via extended catalog"
            );
        }
        assert!(
            validate_closed_capability_with(&extended, "ai.provider", true, "ai.provider:extra")
                .is_err()
        );
        assert!(
            validate_closed_capability_with(&extended, "mcp.invoke", false, "mcp.invoke").is_err()
        );

        // ... while the host identifier parse stays enum-closed (S1
        // limitation, still pinned): even the extended catalog cannot mint a
        // `CapabilityFamily` the Core vocabulary no longer names. This is the
        // intended zero-AI default for every Core-only install path (grant,
        // CLI, runtime), all of which parse through the Core seed.
        for raw in [
            "ai.provider",
            "mcp.invoke:mail.list",
            "agent.memory:record-1",
        ] {
            assert!(
                CapabilityId::parse_with(&extended, raw).is_err(),
                "host parse must still fail closed on '{raw}' (closed enum vocabulary)"
            );
            assert!(
                CapabilityId::parse(raw).is_err(),
                "host static parse must still fail closed on '{raw}'"
            );
        }

        // A fresh Core seed is unaffected by the extension on every path.
        let fresh = CapabilityCatalog::core();
        assert!(
            validate_closed_capability_with(&fresh, "ai.provider", false, "ai.provider").is_err()
        );
        assert!(bitty_package::CapabilityId::parse_with(&fresh, "ai.provider").is_err());
        assert!(package_manifest::validate_capability_with(&fresh, "ai.provider").is_err());
    }

    #[test]
    fn parse_matches_parse_with_on_core_seed() {
        // CTX-0916 S1 (DEC-0102): the static entry point delegates to the
        // Core catalog seed, so both paths must agree on every identifier.
        use bitty_package::manifest as package_manifest;

        let core = CapabilityCatalog::core();
        for head in package_manifest::CLOSED_CAPABILITY_HEADS {
            let raw = if package_manifest::capability_requires_param(head) {
                format!("{head}:param")
            } else {
                (*head).to_string()
            };
            let old = CapabilityId::parse(&raw);
            let new = CapabilityId::parse_with(&core, &raw);
            assert_eq!(
                old.is_ok(),
                new.is_ok(),
                "old/new parse disagree on '{raw}'"
            );
            if let (Ok(a), Ok(b)) = (old, new) {
                assert_eq!(a, b, "old/new parse differ on '{raw}'");
            }
        }

        for raw in [
            "terminal.unknown-thing",
            "ui.unknown",
            "agent.evil",
            "fs.read",
            "env.read",
            "network.connect",
            "mcp.invoke",
            "terminal.semantic-read:extra",
            "ui.rich:something",
            "ai.provider:extra",
        ] {
            assert!(
                CapabilityId::parse(raw).is_err(),
                "static path must reject '{raw}'"
            );
            assert!(
                CapabilityId::parse_with(&core, raw).is_err(),
                "catalog path must reject '{raw}'"
            );
        }
    }

    #[test]
    fn parse_with_accepts_catalog_extension_heads_in_known_families() {
        // Extensions contribute heads through the catalog; the static path
        // still fails closed on them.
        let mut catalog = CapabilityCatalog::core();
        catalog
            .register("panel", &[("panel.custom-view", false)])
            .expect("extension head registers");
        let granted = CapabilityId::parse_with(&catalog, "panel.custom-view")
            .expect("catalog-registered head parses");
        assert_eq!(granted.family(), CapabilityFamily::Panel);
        assert!(!granted.has_param());
        assert!(CapabilityId::parse("panel.custom-view").is_err());

        // S1 limitation, pinned: families outside the closed
        // `CapabilityFamily` enum still fail closed host-side even when the
        // catalog knows them (package-side `parse_with` already accepts
        // them); opening the family vocabulary is a later slice.
        let mut catalog = CapabilityCatalog::core();
        catalog
            .register("acme", &[("acme.widget", false)])
            .expect("extension family registers");
        assert!(CapabilityId::parse_with(&catalog, "acme.widget").is_err());
        assert!(bitty_package::CapabilityId::parse_with(&catalog, "acme.widget").is_ok());
    }

    #[test]
    fn static_paths_delegate_to_core_catalog() {
        // CTX-0916 S3 (DEC-0102): the static entry points delegate to the
        // Core catalog seed, so both paths must agree byte-identically on
        // every identifier. No family additions or removals: the closed
        // `CapabilityFamily` vocabulary is untouched.
        use bitty_package::manifest as package_manifest;

        let core = CapabilityCatalog::core();
        for head in package_manifest::CLOSED_CAPABILITY_HEADS {
            let requires_param = package_manifest::capability_requires_param(head);
            let raw = if requires_param {
                format!("{head}:param")
            } else {
                (*head).to_string()
            };
            // Closed-set validation: full Result equality (shared helper and
            // shared host error vocabulary, not just is_ok).
            assert_eq!(
                validate_closed_capability(head, requires_param, &raw),
                validate_closed_capability_with(&core, head, requires_param, &raw),
                "validate diverged for '{raw}'"
            );
            // Effect statements: identical wording through the Core seed.
            let id = CapabilityId::parse(&raw).expect("closed head must parse");
            assert_eq!(
                effect_statement(&id),
                effect_statement_with(&core, &id),
                "effect diverged for '{raw}'"
            );
        }

        // Rejections agree with identical messages on both paths.
        for (head, has_param, raw) in [
            ("terminal.unknown-thing", false, "terminal.unknown-thing"),
            ("ui.unknown", false, "ui.unknown"),
            ("fs.read", false, "fs.read"),
            ("env.read", false, "env.read"),
            ("network.connect", false, "network.connect"),
            ("mcp.invoke", false, "mcp.invoke"),
            (
                "terminal.semantic-read",
                true,
                "terminal.semantic-read:extra",
            ),
            ("ui.rich", true, "ui.rich:something"),
            ("ai.provider", true, "ai.provider:extra"),
        ] {
            assert_eq!(
                validate_closed_capability(head, has_param, raw),
                validate_closed_capability_with(&core, head, has_param, raw),
                "validate rejection diverged for '{raw}'"
            );
            assert!(validate_closed_capability(head, has_param, raw).is_err());
        }
    }

    #[test]
    fn extension_heads_route_through_catalog() {
        // CTX-0916 S3 (DEC-0102): extension heads registered in known
        // families validate identically through the extended catalog on the
        // validate path, while the Core-seeded statics still fail closed and
        // a fresh Core seed is unaffected. Effect wording for extension heads
        // falls back until an RFC assigns it; the Core seed is byte-identical.
        let mut extended = CapabilityCatalog::core();
        extended
            .register("panel", &[("panel.custom-view", false)])
            .expect("panel extension registers");
        extended
            .register("fs", &[("fs.archive-read", true)])
            .expect("fs extension registers");

        // Parameter-free extension head: accepted bare, rejected with param.
        assert!(
            validate_closed_capability("panel.custom-view", false, "panel.custom-view").is_err()
        );
        assert!(
            validate_closed_capability_with(
                &extended,
                "panel.custom-view",
                false,
                "panel.custom-view"
            )
            .is_ok()
        );
        assert!(
            validate_closed_capability_with(
                &extended,
                "panel.custom-view",
                true,
                "panel.custom-view:param"
            )
            .is_err()
        );
        let granted =
            CapabilityId::parse_with(&extended, "panel.custom-view").expect("extended head parses");
        assert_eq!(
            effect_statement_with(&extended, &granted),
            "Requested capability"
        );

        // Parameter-requiring extension head: accepted with param, rejected bare.
        assert!(
            validate_closed_capability_with(
                &extended,
                "fs.archive-read",
                true,
                "fs.archive-read:scope-1"
            )
            .is_ok()
        );
        assert!(
            validate_closed_capability_with(&extended, "fs.archive-read", false, "fs.archive-read")
                .is_err()
        );
        assert!(CapabilityId::parse_with(&extended, "fs.archive-read:scope-1").is_ok());
        assert!(CapabilityId::parse_with(&extended, "fs.archive-read").is_err());

        // Core-seeded statics still fail closed on extension heads.
        assert!(CapabilityId::parse("panel.custom-view").is_err());
        assert!(CapabilityId::parse("fs.archive-read:scope-1").is_err());

        // A fresh Core seed is unaffected by the extension.
        let fresh = CapabilityCatalog::core();
        assert!(
            validate_closed_capability_with(
                &fresh,
                "panel.custom-view",
                false,
                "panel.custom-view"
            )
            .is_err()
        );
        assert!(CapabilityId::parse_with(&fresh, "panel.custom-view").is_err());
        assert_eq!(
            effect_statement_with(&fresh, &granted),
            "Requested capability"
        );

        // Core heads keep their wording through both paths.
        let core_id = CapabilityId::parse("fs.read:~/docs/*.md").expect("core head must parse");
        assert_eq!(
            effect_statement(&core_id),
            effect_statement_with(&extended, &core_id)
        );
    }
}
