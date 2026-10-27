//! Category consent profiles for plugin capabilities (issue #2472, CTX-2467).
//!
//! A profile is a **preselection over individual grants**, never an authority
//! layer. It names capability heads a reviewer commonly wants for one plugin
//! category, and [`ProfileSelection::resolve`] intersects that list with the
//! manifest's requested set. The result is still a plain set of validated
//! [`CapabilityId`] values that the CLI shows, records, and later revokes one
//! by one.
//!
//! # Non-negotiables
//!
//! - **No silent sensitive grants.** Exec (`process.*`), network egress,
//!   agent/MCP/AI, clipboard reads, environment reads, terminal input/raw
//!   read, debug, browser, URL/file open, and device-control heads are never
//!   profile members and are filtered again at resolution time. They remain
//!   explicit per-item consent.
//! - **Fail closed.** [`ConsentProfile::validate`] checks every entry against
//!   the live [`CapabilityCatalog`]; an unknown or removed head rejects the
//!   whole profile instead of being ignored.
//! - **Bounded.** A profile declares at most [`MAX_PROFILE_ENTRIES`] heads and
//!   can only select capabilities the manifest already requests.
//! - **Versioned.** [`CONSENT_PROFILE_VERSION`] identifies the reviewed
//!   profile content. Bump it whenever a profile's declared set changes.
//! - **Never revokes.** Resolution reports additions only; callers keep
//!   existing grants and per-capability denials untouched.
//!
//! ```rust
//! use std::collections::BTreeSet;
//!
//! use bitty_package::CapabilityCatalog;
//! use bitty_plugin_host::capability::CapabilityId;
//! use bitty_plugin_host::consent_profile::{ConsentProfile, ProfileSelection};
//!
//! let requested: BTreeSet<CapabilityId> = ["terminal.semantic-read", "network.connect:example.com:443"]
//!     .into_iter()
//!     .map(|id| CapabilityId::parse(id).expect("known capability"))
//!     .collect();
//! let plan = ProfileSelection::resolve(
//!     ConsentProfile::EditorLike,
//!     &CapabilityCatalog::core(),
//!     &requested,
//!     &BTreeSet::new(),
//!     &BTreeSet::new(),
//! )
//! .expect("core profile validates");
//! assert!(plan.selected.iter().any(|cap| cap.as_str() == "terminal.semantic-read"));
//! // Network authority is never preselected: it needs explicit per-item consent.
//! assert!(plan.excluded_sensitive.iter().any(|cap| cap.as_str().starts_with("network.")));
//! ```

use std::collections::BTreeSet;

pub use bitty_package::catalog::CapabilityCatalog;

use crate::capability::CapabilityId;
use crate::error::PluginError;

/// Version of the reviewed profile contents.
///
/// Bump this whenever a profile's declared head list changes so a review
/// screen, test, or manual page can tell two profile shapes apart.
pub const CONSENT_PROFILE_VERSION: u32 = 1;

/// Maximum heads one profile may declare (bounded, reviewed change).
pub const MAX_PROFILE_ENTRIES: usize = 32;

/// The built-in category profiles, in help order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsentProfile {
    /// No optional capabilities.
    Minimal,
    /// Project-relative filesystem read/edit, clipboard write, semantic/history reads.
    EditorLike,
    /// UI, history read, read-only runtime settings inspection.
    PanelLike,
    /// Broader observational set; sensitive heads stay excluded.
    AgentLike,
}

/// Heads preselected by [`ConsentProfile::EditorLike`].
///
/// `fs.read`/`fs.write` are parameterized heads: the concrete manifest
/// patterns are still reviewed and validated by the existing filesystem
/// bounds, and home-rooted/absolute patterns are treated as sensitive (they
/// need explicit per-item consent). `clipboard.write` is allowed;
/// `clipboard.read` is not.
pub const EDITOR_LIKE_HEADS: &[&str] = &[
    "terminal.semantic-read",
    "ui.rich",
    "clipboard.write",
    "fs.read",
    "fs.write",
    "history.transcript.read",
    "history.commands.read",
    "history.kv.read",
    "workspace.read",
    "runtime.inspect",
    "platform.notify",
];

/// Heads preselected by [`ConsentProfile::PanelLike`].
///
/// Presentation and bounded observation only: no filesystem write and no
/// terminal input/raw read, exec, network, clipboard, or environment access.
pub const PANEL_LIKE_HEADS: &[&str] = &[
    "ui.rich",
    "ui.overlay",
    "ui.overlay.focus",
    "panel.provider",
    "panel.create",
    "panel.focus",
    "panel.overlay",
    "history.transcript.read",
    "history.commands.read",
    "history.kv.read",
    "workspace.read",
    "runtime.inspect",
    "runtime.keymaps.read",
    "platform.notify",
    "platform.cpu-read",
    "platform.memory-read",
    "platform.battery-read",
];

/// Heads preselected by [`ConsentProfile::AgentLike`].
///
/// The broadest built-in profile, but still an observational set: exec,
/// network, agent/MCP/AI, clipboard reads, environment reads, terminal
/// input/raw read, debug, browser, URL/file open, and device control stay
/// explicit per-item consent.
pub const AGENT_LIKE_HEADS: &[&str] = &[
    "terminal.semantic-read",
    "ui.rich",
    "ui.overlay",
    "ui.overlay.focus",
    "panel.provider",
    "panel.create",
    "panel.focus",
    "panel.overlay",
    "clipboard.write",
    "fs.read",
    "history.transcript.read",
    "history.commands.read",
    "history.kv.read",
    "workspace.read",
    "runtime.inspect",
    "runtime.keymaps.read",
    "platform.notify",
    "platform.cpu-read",
    "platform.memory-read",
    "platform.battery-read",
    "platform.network.read",
    "platform.backlight.read",
    "platform.idle.read",
    "audio.spectrum",
    "audio.state.read",
];

impl ConsentProfile {
    /// Every profile in help order (single source for parsing and docs).
    pub const ALL: &'static [Self] = &[
        Self::Minimal,
        Self::EditorLike,
        Self::PanelLike,
        Self::AgentLike,
    ];

    /// `(name, profile)` pairs in help order.
    pub const NAMES: &'static [(&'static str, Self)] = &[
        ("minimal", Self::Minimal),
        ("editor-like", Self::EditorLike),
        ("panel-like", Self::PanelLike),
        ("agent-like", Self::AgentLike),
    ];

    /// Parse a profile name (case-sensitive, lowercase).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(name, _)| *name == raw)
            .map(|(_, profile)| *profile)
    }

    /// Canonical profile name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::EditorLike => "editor-like",
            Self::PanelLike => "panel-like",
            Self::AgentLike => "agent-like",
        }
    }

    /// One-line human summary for help and review screens.
    #[must_use]
    pub fn summary(self) -> &'static str {
        match self {
            Self::Minimal => "no optional capabilities",
            Self::EditorLike => {
                "project-relative filesystem read/edit, clipboard write, semantic/history reads"
            }
            Self::PanelLike => "UI, history read, read-only runtime settings inspection",
            Self::AgentLike => "broader observational set; sensitive heads stay explicit",
        }
    }

    /// Declared heads for this profile.
    #[must_use]
    pub fn heads(self) -> &'static [&'static str] {
        match self {
            Self::Minimal => &[],
            Self::EditorLike => EDITOR_LIKE_HEADS,
            Self::PanelLike => PANEL_LIKE_HEADS,
            Self::AgentLike => AGENT_LIKE_HEADS,
        }
    }

    /// Validate the declared heads against a live capability catalog.
    ///
    /// Fails closed on an unknown/removed head, a duplicate, an entry over the
    /// profile bound, or a sensitive head. This runs on every resolution, so a
    /// catalog that drops a head can never silently narrow a profile.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Capability`] for unknown, duplicate, or
    /// sensitive entries, and [`PluginError::LimitExceeded`] when the profile
    /// exceeds [`MAX_PROFILE_ENTRIES`].
    pub fn validate(self, catalog: &CapabilityCatalog) -> Result<(), PluginError> {
        validate_profile_heads(self.name(), self.heads(), catalog)
    }

    /// Whether this profile preselects `capability` by its head.
    ///
    /// Parameterized capabilities (`fs.read:<pattern>`, …) match on the head;
    /// the concrete parameter still travels as an individual grant.
    #[must_use]
    pub fn matches(self, capability: &CapabilityId) -> bool {
        let head = capability_head(capability.as_str());
        self.heads().contains(&head)
    }
}

/// One resolved profile plan: additions plus the review-screen buckets.
///
/// All sets are filtered to the manifest's requested capabilities. Existing
/// grants outside the profile are not touched; denials stay denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSelection {
    /// Profile that produced this plan.
    pub profile: ConsentProfile,
    /// Reviewed profile content version.
    pub profile_version: u32,
    /// Requested capabilities this profile would add (not already granted).
    pub selected: BTreeSet<CapabilityId>,
    /// Requested capabilities already granted (unchanged).
    pub already_granted: BTreeSet<CapabilityId>,
    /// Requested sensitive capabilities excluded from the profile.
    pub excluded_sensitive: BTreeSet<CapabilityId>,
    /// Requested non-sensitive capabilities outside this profile.
    pub not_selected: BTreeSet<CapabilityId>,
    /// Requested capabilities skipped because they were explicitly revoked.
    pub denied: BTreeSet<CapabilityId>,
}

impl ProfileSelection {
    /// Resolve a profile against the requested/granted/denied sets.
    ///
    /// `requested` is the manifest's exact expansion (`required`, `optional`,
    /// and filesystem patterns). Nothing outside it can be selected. `granted`
    /// and `denied` are the recorded plugin state; a denied capability is
    /// reported as denied and is never re-selected by a profile.
    ///
    /// # Errors
    ///
    /// Propagates [`ConsentProfile::validate`] failures, so an unknown or
    /// removed profile head fails closed before any selection is shown.
    pub fn resolve(
        profile: ConsentProfile,
        catalog: &CapabilityCatalog,
        requested: &BTreeSet<CapabilityId>,
        granted: &BTreeSet<CapabilityId>,
        denied: &BTreeSet<CapabilityId>,
    ) -> Result<Self, PluginError> {
        profile.validate(catalog)?;
        let mut selection = Self {
            profile,
            profile_version: CONSENT_PROFILE_VERSION,
            selected: BTreeSet::new(),
            already_granted: BTreeSet::new(),
            excluded_sensitive: BTreeSet::new(),
            not_selected: BTreeSet::new(),
            denied: BTreeSet::new(),
        };
        for capability in requested {
            if granted.contains(capability) {
                selection.already_granted.insert(capability.clone());
                continue;
            }
            if denied.contains(capability) {
                selection.denied.insert(capability.clone());
                continue;
            }
            if is_sensitive(capability) {
                selection.excluded_sensitive.insert(capability.clone());
                continue;
            }
            if profile.matches(capability) {
                selection.selected.insert(capability.clone());
            } else {
                selection.not_selected.insert(capability.clone());
            }
        }
        Ok(selection)
    }

    /// Whether the requested set is fully covered after applying this plan.
    ///
    /// A plugin may only be enabled when every requested capability is in
    /// `granted ∪ selected`; otherwise the existing enable gate keeps it
    /// disabled until explicit per-item consent fills the gaps.
    #[must_use]
    pub fn completes_requested(&self) -> bool {
        self.excluded_sensitive.is_empty() && self.not_selected.is_empty() && self.denied.is_empty()
    }

    /// Granted set after applying this plan to `existing`.
    ///
    /// Monotonic by construction: existing grants are preserved and only
    /// profile-selected additions are inserted. Existing grants outside the
    /// manifest's requested set are kept (the install path narrows those
    /// separately, exactly as it did before profiles existed).
    #[must_use]
    pub fn granted_after(&self, existing: &BTreeSet<CapabilityId>) -> BTreeSet<CapabilityId> {
        let mut granted = existing.clone();
        granted.extend(self.selected.iter().cloned());
        granted
    }
}

/// Whether a capability is too sensitive for any profile preselection.
///
/// These heads remain explicit per-item consent. Head-level matching keeps the
/// rule stable for parameterized capabilities (`process.spawn:<tool>`,
/// `network.connect:<host>`, `env.read:<KEY>`). Filesystem heads are
/// project-relative by profile: a home-rooted or absolute `fs.read`/`fs.write`
/// parameter is classified sensitive and needs explicit `--cap` consent.
#[must_use]
pub fn is_sensitive(capability: &CapabilityId) -> bool {
    let raw = capability.as_str();
    let head = capability_head(raw);
    if is_sensitive_head(head) {
        return true;
    }
    if matches!(head, "fs.read" | "fs.write") {
        if let Some((_, parameter)) = raw.split_once(':') {
            return !is_project_relative(parameter);
        }
    }
    false
}

/// Whether a filesystem parameter stays project-relative.
///
/// Home-rooted (`~`), absolute POSIX (`/`), UNC (`\\`), and Windows
/// drive-qualified (`C:`) parameters cannot be preselected by a profile; the
/// manifest may still request them and the user may still grant them one at a
/// time with `bitty plugin grant --cap`.
fn is_project_relative(parameter: &str) -> bool {
    if parameter.starts_with('~') || parameter.starts_with('/') || parameter.starts_with('\\') {
        return false;
    }
    let bytes = parameter.as_bytes();
    !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Head-level sensitive classification shared by profiles and resolution.
#[must_use]
pub fn is_sensitive_head(head: &str) -> bool {
    let family = head.split('.').next().unwrap_or(head);
    if matches!(
        family,
        "process" | "network" | "env" | "agent" | "mcp" | "ai"
    ) {
        return true;
    }
    matches!(
        head,
        "clipboard.read"
            | "terminal.raw-read"
            | "terminal.input.self"
            | "terminal.input.all"
            | "terminal.input.submit"
            | "terminal.manage"
            | "ui.protocol-register"
            | "runtime.configure"
            | "runtime.plugin-manage"
            // CTX-1613 (bitty#2393): configuration writes must never be
            // preselected by a consent profile; every plugin requests and the
            // user grants `config.manage` explicitly.
            | "config.manage"
            | "debug.inspect"
            | "debug.trace"
            | "debug.control"
            | "browser.embed"
            | "browser.navigation"
            | "browser.file-url"
            | "browser.storage"
            | "platform.open-url"
            | "platform.image-file"
            | "platform.essid.read"
            | "platform.bluetooth.control"
            | "platform.backlight.control"
            | "platform.idle.control"
            | "protocol.register"
            | "workspace.control"
            | "audio.state.control"
    )
}

/// Capability head (`family.resource[.scope]`) without a `:PARAMETER`.
#[must_use]
pub fn capability_head(raw: &str) -> &str {
    raw.split_once(':').map_or(raw, |(head, _)| head)
}

/// Validate an arbitrary declared head list against a live catalog.
///
/// Public for tests and future profile sources; the built-in profiles call it
/// through [`ConsentProfile::validate`].
///
/// # Errors
///
/// Returns [`PluginError::LimitExceeded`] when `heads` exceeds
/// [`MAX_PROFILE_ENTRIES`], [`PluginError::Duplicate`] on a repeated head,
/// and [`PluginError::Capability`] for an unknown/removed or sensitive head.
pub fn validate_profile_heads(
    profile: &str,
    heads: &[&str],
    catalog: &CapabilityCatalog,
) -> Result<(), PluginError> {
    if heads.len() > MAX_PROFILE_ENTRIES {
        return Err(PluginError::LimitExceeded {
            field: format!("consent profile '{profile}'"),
            limit: MAX_PROFILE_ENTRIES,
            actual: heads.len(),
        });
    }
    let mut seen = BTreeSet::new();
    for head in heads {
        if !seen.insert(*head) {
            return Err(PluginError::Duplicate {
                kind: "consent profile head".to_string(),
                value: (*head).to_string(),
            });
        }
        let Some(requires_param) = catalog.requires_param(head) else {
            return Err(PluginError::capability(
                *head,
                format!(
                    "consent profile '{profile}' names a capability head that is not in the live catalog"
                ),
            ));
        };
        // Presence in the catalog is not enough: run the catalog's own
        // parameter rule so a shape change in a future catalog is caught.
        catalog
            .check(head, requires_param)
            .map_err(|violation| PluginError::capability(*head, format!("{violation:?}")))?;
        if is_sensitive_head(head) {
            return Err(PluginError::capability(
                *head,
                format!("consent profile '{profile}' may not preselect a sensitive capability"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(ids: &[&str]) -> BTreeSet<CapabilityId> {
        ids.iter()
            .map(|id| CapabilityId::parse(id).expect("test capability parses"))
            .collect()
    }

    fn names(set: &BTreeSet<CapabilityId>) -> Vec<String> {
        set.iter().map(|cap| cap.as_str().to_string()).collect()
    }

    #[test]
    fn profile_names_versions_and_sizes_are_stable() {
        assert_eq!(CONSENT_PROFILE_VERSION, 1);
        assert_eq!(MAX_PROFILE_ENTRIES, 32);
        let names: Vec<&str> = ConsentProfile::NAMES
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            names,
            vec!["minimal", "editor-like", "panel-like", "agent-like"]
        );
        assert_eq!(ConsentProfile::ALL.len(), ConsentProfile::NAMES.len());
        assert!(ConsentProfile::Minimal.heads().is_empty());
        for profile in ConsentProfile::ALL {
            assert!(
                profile.heads().len() <= MAX_PROFILE_ENTRIES,
                "{} exceeds the profile bound",
                profile.name()
            );
        }
    }

    #[test]
    fn profile_contents_are_pinned_for_version_one() {
        // FNV-1a over the newline-joined head list. These pins are the
        // reviewed v1 content: changing a profile requires updating the pin
        // and bumping CONSENT_PROFILE_VERSION (manual/review sync).
        fn signature(heads: &[&str]) -> u64 {
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for byte in heads.join("\n").bytes().chain(std::iter::once(b'\n')) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash
        }
        assert_eq!(
            signature(ConsentProfile::Minimal.heads()),
            0xaf63_c74c_8601_c8dd
        );
        assert_eq!(
            signature(ConsentProfile::EditorLike.heads()),
            0xffc9_2974_da0d_8c76
        );
        assert_eq!(
            signature(ConsentProfile::PanelLike.heads()),
            0x08c2_4a9c_394c_da30
        );
        assert_eq!(
            signature(ConsentProfile::AgentLike.heads()),
            0x4289_652c_8bc9_2e2f
        );
    }

    #[test]
    fn every_declared_head_validates_against_the_core_catalog() {
        let catalog = CapabilityCatalog::core();
        for profile in ConsentProfile::ALL {
            profile
                .validate(&catalog)
                .unwrap_or_else(|error| panic!("{}: {error}", profile.name()));
        }
    }

    #[test]
    fn every_high_risk_head_is_sensitive() {
        // CTX-1613 review B1: the consent-profile preselection gate must
        // never lag the high-risk presentation set. Every head the UI must
        // present distinctly is also too sensitive for any profile to
        // preselect; a new high-risk head without a sensitivity entry fails
        // here instead of silently becoming preselectable.
        use bitty_package::manifest::{CLOSED_CAPABILITY_HEADS, capability_requires_param};
        for head in CLOSED_CAPABILITY_HEADS {
            let raw = if capability_requires_param(head) {
                format!("{head}:param")
            } else {
                (*head).to_string()
            };
            let id = CapabilityId::parse(&raw).expect("closed head parses");
            if id.is_high_risk() {
                assert!(
                    is_sensitive_head(head),
                    "high-risk head '{head}' must also be profile-sensitive"
                );
                assert!(
                    is_sensitive(&id),
                    "high-risk capability '{raw}' must be profile-sensitive"
                );
            }
        }
        // The new write head is both high-risk and explicitly sensitive.
        let config = CapabilityId::parse("config.manage").expect("config.manage");
        assert!(config.is_high_risk());
        assert!(is_sensitive_head("config.manage"));
        assert!(is_sensitive(&config));
    }

    #[test]
    fn profiles_never_declare_sensitive_heads() {
        let forbidden_heads = [
            "process.editor",
            "process.spawn",
            "network.connect",
            "env.read",
            "clipboard.read",
            "terminal.raw-read",
            "terminal.input.self",
            "terminal.input.all",
            "terminal.input.submit",
            "terminal.manage",
            "runtime.plugin-manage",
            "config.manage",
            "debug.control",
            "ui.protocol-register",
            "browser.embed",
            "platform.open-url",
            "platform.essid.read",
        ];
        for profile in ConsentProfile::ALL {
            for head in profile.heads() {
                assert!(
                    !is_sensitive_head(head),
                    "{} preselects sensitive head {head}",
                    profile.name()
                );
                assert!(
                    !forbidden_heads.contains(head),
                    "{} declares forbidden head {head}",
                    profile.name()
                );
            }
        }
        for family in ["agent", "mcp", "ai"] {
            for profile in ConsentProfile::ALL {
                assert!(
                    profile
                        .heads()
                        .iter()
                        .all(|head| !head.starts_with(&format!("{family}."))),
                    "{} must not preselect {family}.*",
                    profile.name()
                );
            }
        }
    }

    #[test]
    fn each_profile_resolves_exactly_its_declared_intersection() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&[
            "terminal.semantic-read",
            "ui.rich",
            "fs.read:src/**",
            "fs.write:src/**",
            "clipboard.write",
            "clipboard.read",
            "network.connect:example.com:443",
            "process.spawn:git",
            "env.read:HOME",
            "history.transcript.read",
            "audio.spectrum",
        ]);
        for profile in ConsentProfile::ALL {
            let plan = ProfileSelection::resolve(
                *profile,
                &catalog,
                &requested,
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .expect("profile resolves");
            let mut expected = BTreeSet::new();
            for capability in &requested {
                if !is_sensitive(capability) && profile.matches(capability) {
                    expected.insert(capability.clone());
                }
            }
            assert_eq!(
                plan.selected,
                expected,
                "{} resolved a different set",
                profile.name()
            );
            // Sensitive requested heads are always reported separately.
            assert_eq!(
                plan.excluded_sensitive,
                caps(&[
                    "clipboard.read",
                    "network.connect:example.com:443",
                    "process.spawn:git",
                    "env.read:HOME",
                ])
            );
        }
        assert!(ConsentProfile::Minimal.heads().is_empty());
        assert_eq!(
            ProfileSelection::resolve(
                ConsentProfile::Minimal,
                &catalog,
                &requested,
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .expect("resolves")
            .selected,
            BTreeSet::new()
        );
    }

    #[test]
    fn sensitive_capabilities_need_explicit_consent_even_with_agent_like() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&[
            "process.spawn:git",
            "network.connect:example.com:443",
            "clipboard.read",
            "env.read:HOME",
            "terminal.raw-read",
        ]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::AgentLike,
            &catalog,
            &requested,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert!(plan.selected.is_empty(), "{:?}", names(&plan.selected));
        assert_eq!(plan.excluded_sensitive.len(), 5);
        assert!(!plan.completes_requested());
    }

    #[test]
    fn parameterized_heads_match_concrete_grants() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&["fs.read:src/**", "fs.write:docs/**", "fs.read:~/notes/**"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::EditorLike,
            &catalog,
            &requested,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert_eq!(
            names(&plan.selected),
            vec!["fs.read:src/**", "fs.write:docs/**"]
        );
        assert_eq!(
            names(&plan.excluded_sensitive),
            vec!["fs.read:~/notes/**"],
            "home-rooted filesystem patterns need explicit per-item consent"
        );
        assert!(!plan.completes_requested());
    }

    #[test]
    fn explicit_denials_are_never_reselected() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&["terminal.semantic-read", "ui.rich"]);
        let denied = caps(&["terminal.semantic-read"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::EditorLike,
            &catalog,
            &requested,
            &BTreeSet::new(),
            &denied,
        )
        .expect("resolves");
        assert_eq!(names(&plan.selected), vec!["ui.rich"]);
        assert_eq!(names(&plan.denied), vec!["terminal.semantic-read"]);
        assert!(!plan.completes_requested());
        assert_eq!(
            plan.granted_after(&BTreeSet::new()),
            caps(&["ui.rich"]),
            "a profile never resurrects a denial"
        );
    }

    #[test]
    fn already_granted_capabilities_are_not_reapplied() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&["terminal.semantic-read", "ui.rich"]);
        let granted = caps(&["terminal.semantic-read"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::EditorLike,
            &catalog,
            &requested,
            &granted,
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert_eq!(names(&plan.selected), vec!["ui.rich"]);
        assert_eq!(names(&plan.already_granted), vec!["terminal.semantic-read"]);
        assert_eq!(
            plan.granted_after(&granted),
            caps(&["terminal.semantic-read", "ui.rich"]),
            "existing grants are preserved and only additions are inserted"
        );
    }

    #[test]
    fn applying_a_profile_to_a_partially_granted_plugin_cannot_escalate() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&[
            "terminal.semantic-read",
            "ui.rich",
            "network.connect:example.com:443",
        ]);
        let granted = caps(&["ui.rich"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::EditorLike,
            &catalog,
            &requested,
            &granted,
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert_eq!(names(&plan.selected), vec!["terminal.semantic-read"]);
        let after = plan.granted_after(&granted);
        let added: BTreeSet<CapabilityId> = after.difference(&granted).cloned().collect();
        assert_eq!(added, plan.selected, "only the preselected addition lands");
        assert!(added.is_subset(&requested), "never exceeds the manifest");
        assert!(
            added.iter().all(|capability| !is_sensitive(capability)),
            "a profile never adds a sensitive capability"
        );
        assert!(
            !after
                .iter()
                .any(|capability| capability.as_str().starts_with("network.")),
            "network authority is never added by a profile: {after:?}"
        );

        // Re-applying the same profile is an idempotent no-op.
        let again = ProfileSelection::resolve(
            ConsentProfile::EditorLike,
            &catalog,
            &requested,
            &after,
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert!(again.selected.is_empty());
        assert_eq!(again.granted_after(&after), after);
    }

    #[test]
    fn profile_selection_never_exceeds_requested() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&["terminal.semantic-read"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::AgentLike,
            &catalog,
            &requested,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert_eq!(names(&plan.selected), vec!["terminal.semantic-read"]);
        assert!(plan.selected.is_subset(&requested));
    }

    #[test]
    fn unknown_or_removed_heads_fail_closed() {
        let catalog = CapabilityCatalog::core();
        let error = validate_profile_heads("test", &["made.up"], &catalog)
            .expect_err("unknown head must fail");
        assert!(matches!(error, PluginError::Capability { .. }), "{error:?}");

        // A parameter-rule change is also caught by the catalog check.
        let error = validate_profile_heads("test", &["fs.read"], &catalog);
        assert!(
            error.is_ok(),
            "parameterized heads are valid profile entries"
        );

        let error = validate_profile_heads("test", &["process.spawn"], &catalog)
            .expect_err("sensitive head must fail");
        assert!(matches!(error, PluginError::Capability { .. }), "{error:?}");
    }

    #[test]
    fn duplicate_and_over_limit_profiles_fail_closed() {
        let catalog = CapabilityCatalog::core();
        let duplicate = validate_profile_heads("test", &["ui.rich", "ui.rich"], &catalog)
            .expect_err("duplicate must fail");
        assert!(
            matches!(duplicate, PluginError::Duplicate { .. }),
            "{duplicate:?}"
        );

        let heads: Vec<&str> = catalog
            .heads()
            .into_iter()
            .take(MAX_PROFILE_ENTRIES + 1)
            .collect();
        assert!(heads.len() > MAX_PROFILE_ENTRIES);
        let over =
            validate_profile_heads("test", &heads, &catalog).expect_err("over-limit must fail");
        assert!(
            matches!(over, PluginError::LimitExceeded { .. }),
            "{over:?}"
        );
    }

    #[test]
    fn minimal_selects_nothing_even_for_safe_heads() {
        let catalog = CapabilityCatalog::core();
        let requested = caps(&["ui.rich", "platform.notify"]);
        let plan = ProfileSelection::resolve(
            ConsentProfile::Minimal,
            &catalog,
            &requested,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("resolves");
        assert!(plan.selected.is_empty());
        assert_eq!(plan.not_selected, requested);
        assert!(!plan.completes_requested());
    }
}
