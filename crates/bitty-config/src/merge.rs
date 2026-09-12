//! Layer merge, source attribution, and conflict reporting.
//!
//! Implements the candidate contract from RFC section
//! “Layers, merge, and attribution”:
//!
//! 1. Every schema field declares exactly one merge class
//!    (scalar replace, schema-guided deep merge, set-by-identifier, or
//!    explicit list policy); undeclared fields fail validation rather than
//!    merging implicitly.
//! 2. Merge conflicts are computed, reported with both sources' file
//!    locations, and resolved only by declared precedence — never silently
//!    by load order.
//! 3. Source attribution survives merging so every effective value answers
//!    “which file, which layer” (for `config show --source`).
//! 4. System policy entries marked non-overridable reject overriding plans at
//!    validation with a dedicated diagnostic class.
//! 5. Profile `extends` resolves single-parent chains with cycle detection.
//!
//! # Drift note
//!
//! The typed schema's per-field merge class is encoded in the match arms
//! below and mirrored in [`MergeClass`]. If a new field is added, add its
//! class here, update the table in the crate docs, and add a test that
//! proves attribution survives.

use std::collections::{HashMap, HashSet};

use crate::error::ConfigError;
use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
use crate::types::{EffectiveConfig, KeymapEntry, PluginSpec};

/// Declared merge class for a single schema field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeClass {
    /// Later layer replaces the earlier scalar wholesale.
    ScalarReplace,
    /// Structured map uses field-wise deep merge.
    DeepMerge,
    /// Set merges by stable identifier (keymaps by `context+chord`, plugins by `id`).
    SetById,
    /// Generic list uses explicit policy; this crate implements Replace.
    ListReplace,
}

impl std::fmt::Display for MergeClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::ScalarReplace => "scalar-replace",
            Self::DeepMerge => "deep-merge",
            Self::SetById => "set-by-id",
            Self::ListReplace => "list-replace",
        };
        f.write_str(s)
    }
}

/// Per-field merge-class table (kept in code so it cannot drift silently
/// from the behavior below).
#[must_use]
pub fn merge_class_for(field: &str) -> Option<MergeClass> {
    match field {
        "font.family"
        | "font.size"
        | "font.line_height"
        | "font.letter_spacing"
        | "window.opacity"
        | "window.padding"
        | "window.radius_px"
        | "terminal.scrollback"
        | "terminal.shell"
        | "terminal.scroll_lines_per_notch"
        | "terminal.scroll_pixels_per_notch"
        | "selection.auto_copy"
        | "layout.gaps_in"
        | "layout.gaps_out"
        | "decoration.gaps_in"
        | "decoration.gaps_out"
        | "decoration.border"
        | "decoration.radius"
        | "decoration.content_inset"
        | "decoration.border_color"
        | "decoration.border_color_focused"
        | "decoration.border_color_idle"
        | "decoration.border_width"
        | "decoration.border_width_focused"
        | "decoration.border_width_idle"
        | "scrollbar.mode"
        | "scrollbar.width"
        | "mouse.focus_follows_mouse"
        | "mouse.focus_follows_mouse_delay_ms"
        | "appearance.theme"
        | "appearance.animations.enabled"
        | "appearance.animations.reduced_motion"
        | "appearance.animations.duration_ms.open"
        | "appearance.animations.duration_ms.close"
        | "appearance.animations.duration_ms.focus"
        | "appearance.animations.duration_ms.workspace"
        | "appearance.animations.easing.open"
        | "appearance.animations.easing.close"
        | "appearance.animations.easing.focus"
        | "appearance.animations.easing.workspace"
        | "mod_key"
        | "extends"
        | "profile"
        | "schema_version" => Some(MergeClass::ScalarReplace),
        "font"
        | "window"
        | "terminal"
        | "selection"
        | "layout"
        | "decoration"
        | "scrollbar"
        | "mouse"
        | "appearance"
        | "appearance.animations" => Some(MergeClass::DeepMerge),
        "keymaps" | "plugins" => Some(MergeClass::SetById),
        _ => None,
    }
}

/// A single field conflict with source attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    /// Dotted field path.
    pub field: String,
    /// Source that previously owned the field.
    pub previous_source: ConfigSource,
    /// Source that attempted to override it.
    pub new_source: ConfigSource,
    /// Merge class that governed the field.
    pub merge_class: MergeClass,
}

/// Result of merging a layer stack.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedConfig {
    /// The effective configuration.
    pub effective: EffectiveConfig,
    /// Source attribution surviving the merge: every effective value
    /// answers "which file, which layer".
    pub attribution: HashMap<String, ConfigSource>,
    /// Conflicts that were resolved by precedence (reported, not silent).
    pub conflicts: Vec<MergeConflict>,
    /// Non-overridable policy violations that were rejected.
    pub policy_violations: Vec<ConfigError>,
}

impl MergedConfig {
    /// Which source produced the current value of `field`, if any.
    #[must_use]
    pub fn source_of(&self, field: &str) -> Option<&ConfigSource> {
        self.attribution.get(field)
    }
}

fn record_attribution(
    attribution: &mut HashMap<String, ConfigSource>,
    conflicts: &mut Vec<MergeConflict>,
    field: &str,
    previous: Option<ConfigSource>,
    new_src: &ConfigSource,
    merge_class: MergeClass,
) {
    if let Some(prev) = previous {
        conflicts.push(MergeConflict {
            field: field.to_string(),
            previous_source: prev,
            new_source: new_src.clone(),
            merge_class,
        });
    }
    attribution.insert(field.to_string(), new_src.clone());
}

/// Mutable merge accumulators shared with [`merge_animations_overrides`].
///
/// Bundles the four maps/vectors the merge walk threads through every field
/// so the per-leaf helper stays under clippy's argument bound without
/// duplicating the attribution/policy logic.
struct MergeAccumulators<'a> {
    policy_fields: &'a mut HashMap<String, ConfigSource>,
    attribution: &'a mut HashMap<String, ConfigSource>,
    conflicts: &'a mut Vec<MergeConflict>,
    policy_violations: &'a mut Vec<ConfigError>,
}

/// Merges one layer's `appearance.animations` overrides (RFC-0002, CTX-0341).
///
/// Each present leaf is scalar-replace with its own source attribution; an
/// absent leaf is "says nothing" and inherits the lower-precedence value.
/// Mirrors the decoration color handling: policy layers own the leaf, later
/// non-policy overrides are recorded as violations and skipped.
fn merge_animations_overrides(
    effective: &mut EffectiveConfig,
    acc: &mut MergeAccumulators<'_>,
    src: &ConfigSource,
    is_policy: bool,
    over: &crate::types::AnimationsOverride,
) {
    let MergeAccumulators {
        policy_fields,
        attribution,
        conflicts,
        policy_violations,
    } = acc;
    // Field name plus a setter for the concrete effective value. `None`
    // leaves are skipped before any policy/attribution work.
    let leaves: [(&str, bool); 10] = [
        ("appearance.animations.enabled", over.enabled.is_some()),
        (
            "appearance.animations.reduced_motion",
            over.reduced_motion.is_some(),
        ),
        (
            "appearance.animations.duration_ms.open",
            over.duration_open.is_some(),
        ),
        (
            "appearance.animations.duration_ms.close",
            over.duration_close.is_some(),
        ),
        (
            "appearance.animations.duration_ms.focus",
            over.duration_focus.is_some(),
        ),
        (
            "appearance.animations.duration_ms.workspace",
            over.duration_workspace.is_some(),
        ),
        (
            "appearance.animations.easing.open",
            over.easing_open.is_some(),
        ),
        (
            "appearance.animations.easing.close",
            over.easing_close.is_some(),
        ),
        (
            "appearance.animations.easing.focus",
            over.easing_focus.is_some(),
        ),
        (
            "appearance.animations.easing.workspace",
            over.easing_workspace.is_some(),
        ),
    ];
    for (field, present) in leaves {
        if !present {
            continue;
        }
        if is_policy {
            policy_fields.insert(field.to_string(), src.clone());
        } else if let Some(policy_src) = policy_fields.get(field) {
            policy_violations.push(ConfigError::NonOverridable {
                field: field.to_string(),
                policy_source: policy_src.describe(),
                attempted_source: src.describe(),
            });
            conflicts.push(MergeConflict {
                field: field.to_string(),
                previous_source: policy_src.clone(),
                new_source: src.clone(),
                merge_class: MergeClass::ScalarReplace,
            });
            continue;
        }
        // Apply only this leaf (all others inherit).
        let mut one = crate::types::AnimationsOverride::default();
        match field {
            "appearance.animations.enabled" => one.enabled = over.enabled,
            "appearance.animations.reduced_motion" => one.reduced_motion = over.reduced_motion,
            "appearance.animations.duration_ms.open" => one.duration_open = over.duration_open,
            "appearance.animations.duration_ms.close" => one.duration_close = over.duration_close,
            "appearance.animations.duration_ms.focus" => one.duration_focus = over.duration_focus,
            "appearance.animations.duration_ms.workspace" => {
                one.duration_workspace = over.duration_workspace;
            }
            "appearance.animations.easing.open" => one.easing_open = over.easing_open,
            "appearance.animations.easing.close" => one.easing_close = over.easing_close,
            "appearance.animations.easing.focus" => one.easing_focus = over.easing_focus,
            _ => one.easing_workspace = over.easing_workspace,
        }
        effective.animations.apply_overrides(&one);
        let prev = attribution.get(field).cloned();
        record_attribution(
            attribution,
            conflicts,
            field,
            prev,
            src,
            MergeClass::ScalarReplace,
        );
    }
    // Attribute the container to this layer when it declared the table at
    // all, matching the decoration/appearance container convention.
    if leaves.iter().any(|(_, present)| *present) {
        attribution.insert("appearance.animations".to_string(), src.clone());
    }
}

/// Every dotted schema field the merge attributes, in canonical order.
///
/// Shared by [`merge_layers`] (which backfills any field no layer declared as
/// [`LayerKind::CoreDefaults`]) and [`safe_merged`] (which attributes every
/// field to the built-in safe configuration).
const ATTRIBUTED_FIELDS: &[&str] = &[
    "font.family",
    "font.size",
    "font.line_height",
    "font.letter_spacing",
    "font",
    "window.opacity",
    "window.padding",
    "window.radius_px",
    "window",
    "terminal.scrollback",
    "terminal.shell",
    "terminal.scroll_lines_per_notch",
    "terminal.scroll_pixels_per_notch",
    "terminal",
    "selection.auto_copy",
    "selection",
    "layout.gaps_in",
    "layout.gaps_out",
    "layout",
    "decoration.gaps_in",
    "decoration.gaps_out",
    "decoration.border",
    "decoration.radius",
    "decoration.content_inset",
    "decoration.border_color",
    "decoration.border_color_focused",
    "decoration.border_color_idle",
    "decoration.border_width",
    "decoration.border_width_focused",
    "decoration.border_width_idle",
    "decoration",
    "scrollbar.mode",
    "scrollbar.width",
    "scrollbar",
    "mouse.focus_follows_mouse",
    "mouse.focus_follows_mouse_delay_ms",
    "mouse",
    "appearance.theme",
    "appearance.animations.enabled",
    "appearance.animations.reduced_motion",
    "appearance.animations.duration_ms.open",
    "appearance.animations.duration_ms.close",
    "appearance.animations.duration_ms.focus",
    "appearance.animations.duration_ms.workspace",
    "appearance.animations.easing.open",
    "appearance.animations.easing.close",
    "appearance.animations.easing.focus",
    "appearance.animations.easing.workspace",
    "appearance.animations",
    "appearance",
    "keymaps",
    "plugins",
    "schema_version",
];

/// The built-in safe configuration (`bitty --safe`, R-009/P0-AC-019).
///
/// Returns [`crate::reload::fallback_builtin`] with every schema field
/// attributed to [`LayerKind::CoreDefaults`]: no user file, profile, or CLI
/// layer participates, so the safe values (`0/0/1/0/0` decoration geometry
/// and the opaque `#FFFFFF`/`#808080` outline pair) always win. There are no
/// conflicts or policy violations because nothing overrides the core layer.
/// Pure; performs no I/O.
pub fn safe_merged() -> Result<MergedConfig, ConfigError> {
    let effective = crate::reload::fallback_builtin();
    // Defense in depth: the built-in safe config is constructed to be valid.
    effective.validate()?;
    let core_src = ConfigSource::new(LayerKind::CoreDefaults, None::<String>);
    let attribution = ATTRIBUTED_FIELDS
        .iter()
        .map(|field| ((*field).to_string(), core_src.clone()))
        .collect();
    Ok(MergedConfig {
        effective,
        attribution,
        conflicts: Vec::new(),
        policy_violations: Vec::new(),
    })
}

/// Merge a stack of layered plans into an [`EffectiveConfig`] plus
/// attribution and conflict diagnostics.
///
/// Layers are sorted by [`LayerKind::precedence`] regardless of input order
/// so resolution is never load-order accidental. Policy layers
/// (`SystemPolicy`) that declare non-overridable fields cause later
/// overrides to be recorded as `NonOverridable` policy violations and the
/// policy value is retained.
pub fn merge_layers(mut layers: Vec<LayeredPlan>) -> Result<MergedConfig, ConfigError> {
    for lp in &layers {
        lp.plan.validate()?;
    }
    layers.sort_by_key(|lp| lp.source.layer.precedence());
    let mut policy_fields: HashMap<String, ConfigSource> = HashMap::new();
    let mut attribution: HashMap<String, ConfigSource> = HashMap::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();
    let mut policy_violations: Vec<ConfigError> = Vec::new();
    let mut effective = EffectiveConfig::default();

    for lp in &layers {
        let src = &lp.source;
        let plan = &lp.plan;
        let is_policy = src.layer.is_policy();

        if let Some(font) = &plan.font {
            let field = "font.family";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.font.family.clone_from(&font.family);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.font.family.clone_from(&font.family);
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            let field_sz = "font.size";
            if is_policy {
                policy_fields.insert(field_sz.to_string(), src.clone());
                effective.font.size = font.size;
                let prev = attribution.get(field_sz).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field_sz,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field_sz) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field_sz.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field_sz.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field_sz).cloned();
                effective.font.size = font.size;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field_sz,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            for (field_sp, is_line_height) in
                [("font.line_height", true), ("font.letter_spacing", false)]
            {
                if is_policy {
                    policy_fields.insert(field_sp.to_string(), src.clone());
                    if is_line_height {
                        effective.font.line_height = font.line_height;
                    } else {
                        effective.font.letter_spacing = font.letter_spacing;
                    }
                    let prev = attribution.get(field_sp).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field_sp,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field_sp) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field_sp.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field_sp.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field_sp).cloned();
                    if is_line_height {
                        effective.font.line_height = font.line_height;
                    } else {
                        effective.font.letter_spacing = font.letter_spacing;
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field_sp,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            attribution.insert("font".to_string(), src.clone());
        }

        if let Some(win) = &plan.window {
            for field in ["window.opacity", "window.padding", "window.radius_px"] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "window.opacity" => effective.window.opacity = win.opacity,
                    "window.padding" => effective.window.padding = win.padding,
                    "window.radius_px" => effective.window.radius_px = win.radius_px,
                    _ => {}
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("window".to_string(), src.clone());
        }

        if let Some(term) = &plan.terminal {
            for field in [
                "terminal.scrollback",
                "terminal.shell",
                "terminal.scroll_lines_per_notch",
                "terminal.scroll_pixels_per_notch",
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "terminal.scrollback" => effective.terminal.scrollback = term.scrollback,
                    "terminal.shell" => effective.terminal.shell.clone_from(&term.shell),
                    "terminal.scroll_lines_per_notch" => {
                        effective.terminal.scroll_lines_per_notch = term.scroll_lines_per_notch;
                    }
                    "terminal.scroll_pixels_per_notch" => {
                        effective.terminal.scroll_pixels_per_notch = term.scroll_pixels_per_notch;
                    }
                    _ => {}
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("terminal".to_string(), src.clone());
        }

        // CTX-0191: `selection.auto_copy` is scalar-replace like
        // `terminal.scrollback`; absent table means "says nothing".
        if let Some(sel) = &plan.selection {
            let field = "selection.auto_copy";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.selection.auto_copy = sel.auto_copy;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.selection.auto_copy = sel.auto_copy;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("selection".to_string(), src.clone());
        }

        // CTX-0177: `layout.gaps_in`/`layout.gaps_out` are scalar-replace
        // like `selection.auto_copy`; absent table means "says nothing".
        if let Some(gaps) = &plan.layout {
            for (field, value) in [
                ("layout.gaps_in", gaps.gaps_in),
                ("layout.gaps_out", gaps.gaps_out),
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    match field {
                        "layout.gaps_in" => effective.layout.gaps_in = value,
                        _ => effective.layout.gaps_out = value,
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    match field {
                        "layout.gaps_in" => effective.layout.gaps_in = value,
                        _ => effective.layout.gaps_out = value,
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            attribution.insert("layout".to_string(), src.clone());
        }

        // CTX-0292: Core-owned workspace decoration (`decoration.gaps_in`,
        // `decoration.gaps_out`, `decoration.border`, `decoration.radius`)
        // is scalar-replace like `layout.gaps_in`; absent table means "says
        // nothing".
        if let Some(dec) = &plan.decoration {
            for (field, value) in [
                ("decoration.gaps_in", dec.gaps_in),
                ("decoration.gaps_out", dec.gaps_out),
                ("decoration.border", dec.border),
                ("decoration.radius", dec.radius),
                ("decoration.content_inset", dec.content_inset),
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    match field {
                        "decoration.gaps_in" => effective.decoration.gaps_in = value,
                        "decoration.gaps_out" => effective.decoration.gaps_out = value,
                        "decoration.border" => effective.decoration.border = value,
                        "decoration.radius" => effective.decoration.radius = value,
                        _ => effective.decoration.content_inset = value,
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    match field {
                        "decoration.gaps_in" => effective.decoration.gaps_in = value,
                        "decoration.gaps_out" => effective.decoration.gaps_out = value,
                        "decoration.border" => effective.decoration.border = value,
                        "decoration.radius" => effective.decoration.radius = value,
                        _ => effective.decoration.content_inset = value,
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            // CTX-0340: outline colors are scalar-replace, but an unset
            // (`None`) member means "this layer says nothing about color", so
            // it must not clobber a lower layer's explicit value (the RFC's
            // "an unset pair member inherits the resolved base and never
            // silently shadows it").
            for (field, color) in [
                ("decoration.border_color", dec.border_color),
                ("decoration.border_color_focused", dec.border_color_focused),
                ("decoration.border_color_idle", dec.border_color_idle),
            ] {
                let Some(color) = color else {
                    continue;
                };
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "decoration.border_color" => effective.decoration.border_color = Some(color),
                    "decoration.border_color_focused" => {
                        effective.decoration.border_color_focused = Some(color);
                    }
                    _ => effective.decoration.border_color_idle = Some(color),
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            // CTX-0344: outline widths follow the same "unset says nothing"
            // scalar-replace rule as the colors.
            for (field, width) in [
                ("decoration.border_width", dec.border_width),
                ("decoration.border_width_focused", dec.border_width_focused),
                ("decoration.border_width_idle", dec.border_width_idle),
            ] {
                let Some(width) = width else {
                    continue;
                };
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "decoration.border_width" => effective.decoration.border_width = Some(width),
                    "decoration.border_width_focused" => {
                        effective.decoration.border_width_focused = Some(width);
                    }
                    _ => effective.decoration.border_width_idle = Some(width),
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("decoration".to_string(), src.clone());
        }

        // CTX-0181: `scrollbar.mode`/`scrollbar.width` are scalar-replace
        // like `layout.gaps_in`; absent table means "says nothing".
        if let Some(bar) = &plan.scrollbar {
            for (field, is_mode) in [("scrollbar.mode", true), ("scrollbar.width", false)] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    if is_mode {
                        effective.scrollbar.mode = bar.mode;
                    } else {
                        effective.scrollbar.width = bar.width;
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    if is_mode {
                        effective.scrollbar.mode = bar.mode;
                    } else {
                        effective.scrollbar.width = bar.width;
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            attribution.insert("scrollbar".to_string(), src.clone());
        }

        // CTX-0260/CTX-0334: `mouse.focus_follows_mouse` and its dwell
        // delay are scalar-replace like `selection.auto_copy`; absent table
        // means "says nothing".
        if let Some(mouse) = &plan.mouse {
            let field = "mouse.focus_follows_mouse";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mouse.focus_follows_mouse = mouse.focus_follows_mouse;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mouse.focus_follows_mouse = mouse.focus_follows_mouse;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            let field = "mouse.focus_follows_mouse_delay_ms";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mouse.focus_follows_mouse_delay_ms = mouse.focus_follows_mouse_delay_ms;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mouse.focus_follows_mouse_delay_ms = mouse.focus_follows_mouse_delay_ms;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("mouse".to_string(), src.clone());
        }

        if let Some(app) = &plan.appearance {
            let field = "appearance.theme";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.appearance.theme.clone_from(&app.theme);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
                attribution.insert("appearance".to_string(), src.clone());
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.appearance.theme.clone_from(&app.theme);
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
                attribution.insert("appearance".to_string(), src.clone());
            }
            // RFC-0002: the animations table deep-merges per leaf; every
            // present leaf is scalar-replace with its own attribution.
            if let Some(over) = &app.animations {
                let mut acc = MergeAccumulators {
                    policy_fields: &mut policy_fields,
                    attribution: &mut attribution,
                    conflicts: &mut conflicts,
                    policy_violations: &mut policy_violations,
                };
                merge_animations_overrides(&mut effective, &mut acc, src, is_policy, over);
            }
        }

        // CTX-0236: `mod_key` is scalar-replace like `appearance.theme`;
        // absent means "says nothing" (lower-precedence value wins).
        if let Some(mod_key) = &plan.mod_key {
            let field = "mod_key";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mod_key = *mod_key;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mod_key = *mod_key;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
        }

        if let Some(kms) = &plan.keymaps {
            let field = "keymaps";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.keymaps.clone_from(kms);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::SetById,
                });
            } else {
                let mut merged: HashMap<String, KeymapEntry> = effective
                    .keymaps
                    .iter()
                    .cloned()
                    .map(|e| (e.id(), e))
                    .collect();
                let prev = attribution.get(field).cloned();
                for km in kms {
                    merged.insert(km.id(), km.clone());
                }
                let mut v: Vec<KeymapEntry> = merged.into_values().collect();
                v.sort_by_key(|a| a.id());
                effective.keymaps = v;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            }
        }

        if let Some(ps) = &plan.plugins {
            let field = "plugins";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.plugins.clone_from(ps);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::SetById,
                });
            } else {
                let mut merged: HashMap<String, PluginSpec> = effective
                    .plugins
                    .iter()
                    .cloned()
                    .map(|p| (p.id.trim().to_string(), p))
                    .collect();
                let prev = attribution.get(field).cloned();
                for p in ps {
                    merged.insert(p.id.trim().to_string(), p.clone());
                }
                let mut v: Vec<PluginSpec> = merged.into_values().collect();
                v.sort_by_key(|a| a.id.clone());
                effective.plugins = v;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            }
        }

        if plan.extends.is_some() {
            let field = "extends";
            let prev = attribution.get(field).cloned();
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
        if plan.profile_name.is_some() {
            let field = "profile";
            let prev = attribution.get(field).cloned();
            effective.profile.clone_from(&plan.profile_name);
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
        if plan.schema_version.is_some() {
            let field = "schema_version";
            let prev = attribution.get(field).cloned();
            effective.schema_version = plan.effective_schema_version();
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
    }

    let core_src = ConfigSource::new(LayerKind::CoreDefaults, None::<String>);
    for field in ATTRIBUTED_FIELDS {
        attribution
            .entry((*field).to_string())
            .or_insert_with(|| core_src.clone());
    }

    effective.validate()?;
    if let Some(first) = policy_violations.first().cloned() {
        return Err(first);
    }

    Ok(MergedConfig {
        effective,
        attribution,
        conflicts,
        policy_violations,
    })
}

/// Like [`merge_layers`] but returns the merged state even when policy
/// violations were recorded (for diagnostics testing and `config show`).
pub fn try_merge_layers(layers: Vec<LayeredPlan>) -> Result<MergedConfig, ConfigError> {
    let mut layers_sorted = layers;
    layers_sorted.sort_by_key(|lp| lp.source.layer.precedence());
    merge_layers_allow_policy_violations(layers_sorted)
}

fn merge_layers_allow_policy_violations(
    layers: Vec<LayeredPlan>,
) -> Result<MergedConfig, ConfigError> {
    for lp in &layers {
        lp.plan.validate()?;
    }
    let mut policy_fields: HashMap<String, ConfigSource> = HashMap::new();
    let mut attribution: HashMap<String, ConfigSource> = HashMap::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();
    let mut policy_violations: Vec<ConfigError> = Vec::new();
    let mut effective = EffectiveConfig::default();

    for lp in &layers {
        let src = &lp.source;
        let plan = &lp.plan;
        let is_policy = src.layer.is_policy();

        if let Some(font) = &plan.font {
            for (field, which) in [
                ("font.family", 0u8),
                ("font.size", 1u8),
                ("font.line_height", 2u8),
                ("font.letter_spacing", 3u8),
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    match which {
                        0 => effective.font.family.clone_from(&font.family),
                        1 => effective.font.size = font.size,
                        2 => effective.font.line_height = font.line_height,
                        _ => effective.font.letter_spacing = font.letter_spacing,
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                    attribution.insert("font".to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    match which {
                        0 => effective.font.family.clone_from(&font.family),
                        1 => effective.font.size = font.size,
                        2 => effective.font.line_height = font.line_height,
                        _ => effective.font.letter_spacing = font.letter_spacing,
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                    attribution.insert("font".to_string(), src.clone());
                }
            }
        }
        if let Some(win) = &plan.window {
            for field in ["window.opacity", "window.padding", "window.radius_px"] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "window.opacity" => effective.window.opacity = win.opacity,
                    "window.padding" => effective.window.padding = win.padding,
                    "window.radius_px" => effective.window.radius_px = win.radius_px,
                    _ => {}
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("window".to_string(), src.clone());
        }
        if let Some(term) = &plan.terminal {
            for field in [
                "terminal.scrollback",
                "terminal.shell",
                "terminal.scroll_lines_per_notch",
                "terminal.scroll_pixels_per_notch",
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "terminal.scrollback" => effective.terminal.scrollback = term.scrollback,
                    "terminal.shell" => effective.terminal.shell.clone_from(&term.shell),
                    "terminal.scroll_lines_per_notch" => {
                        effective.terminal.scroll_lines_per_notch = term.scroll_lines_per_notch;
                    }
                    "terminal.scroll_pixels_per_notch" => {
                        effective.terminal.scroll_pixels_per_notch = term.scroll_pixels_per_notch;
                    }
                    _ => {}
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("terminal".to_string(), src.clone());
        }
        // CTX-0191: `selection.auto_copy` is scalar-replace like
        // `terminal.scrollback`; absent table means "says nothing".
        // (Second merge path: allow-policy-violations variant for diagnostics.)
        if let Some(sel) = &plan.selection {
            let field = "selection.auto_copy";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.selection.auto_copy = sel.auto_copy;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.selection.auto_copy = sel.auto_copy;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("selection".to_string(), src.clone());
        }
        // CTX-0177: `layout.gaps_in`/`layout.gaps_out` are scalar-replace
        // like `selection.auto_copy`; absent table means "says nothing".
        // (Second merge path: allow-policy-violations variant for diagnostics.)
        if let Some(gaps) = &plan.layout {
            for (field, value) in [
                ("layout.gaps_in", gaps.gaps_in),
                ("layout.gaps_out", gaps.gaps_out),
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    match field {
                        "layout.gaps_in" => effective.layout.gaps_in = value,
                        _ => effective.layout.gaps_out = value,
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    match field {
                        "layout.gaps_in" => effective.layout.gaps_in = value,
                        _ => effective.layout.gaps_out = value,
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            attribution.insert("layout".to_string(), src.clone());
        }
        // CTX-0292: Core-owned workspace decoration is scalar-replace like
        // `layout.gaps_in`; absent table means "says nothing".
        // (Second merge path: allow-policy-violations variant for diagnostics.)
        if let Some(dec) = &plan.decoration {
            for (field, value) in [
                ("decoration.gaps_in", dec.gaps_in),
                ("decoration.gaps_out", dec.gaps_out),
                ("decoration.border", dec.border),
                ("decoration.radius", dec.radius),
                ("decoration.content_inset", dec.content_inset),
            ] {
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                    match field {
                        "decoration.gaps_in" => effective.decoration.gaps_in = value,
                        "decoration.gaps_out" => effective.decoration.gaps_out = value,
                        "decoration.border" => effective.decoration.border = value,
                        "decoration.radius" => effective.decoration.radius = value,
                        _ => effective.decoration.content_inset = value,
                    }
                    let prev = attribution.get(field).cloned();
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                } else {
                    let prev = attribution.get(field).cloned();
                    match field {
                        "decoration.gaps_in" => effective.decoration.gaps_in = value,
                        "decoration.gaps_out" => effective.decoration.gaps_out = value,
                        "decoration.border" => effective.decoration.border = value,
                        "decoration.radius" => effective.decoration.radius = value,
                        _ => effective.decoration.content_inset = value,
                    }
                    record_attribution(
                        &mut attribution,
                        &mut conflicts,
                        field,
                        prev,
                        src,
                        MergeClass::ScalarReplace,
                    );
                }
            }
            // CTX-0340: outline colors are scalar-replace, and an unset color
            // ("says nothing") never clobbers a lower layer.
            for (field, color) in [
                ("decoration.border_color", dec.border_color),
                ("decoration.border_color_focused", dec.border_color_focused),
                ("decoration.border_color_idle", dec.border_color_idle),
            ] {
                let Some(color) = color else {
                    continue;
                };
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "decoration.border_color" => effective.decoration.border_color = Some(color),
                    "decoration.border_color_focused" => {
                        effective.decoration.border_color_focused = Some(color);
                    }
                    _ => effective.decoration.border_color_idle = Some(color),
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            // CTX-0344: outline widths are scalar-replace; an unset width
            // ("says nothing") never clobbers a lower layer.
            for (field, width) in [
                ("decoration.border_width", dec.border_width),
                ("decoration.border_width_focused", dec.border_width_focused),
                ("decoration.border_width_idle", dec.border_width_idle),
            ] {
                let Some(width) = width else {
                    continue;
                };
                if is_policy {
                    policy_fields.insert(field.to_string(), src.clone());
                } else if let Some(policy_src) = policy_fields.get(field) {
                    policy_violations.push(ConfigError::NonOverridable {
                        field: field.to_string(),
                        policy_source: policy_src.describe(),
                        attempted_source: src.describe(),
                    });
                    conflicts.push(MergeConflict {
                        field: field.to_string(),
                        previous_source: policy_src.clone(),
                        new_source: src.clone(),
                        merge_class: MergeClass::ScalarReplace,
                    });
                    continue;
                }
                let prev = attribution.get(field).cloned();
                match field {
                    "decoration.border_width" => effective.decoration.border_width = Some(width),
                    "decoration.border_width_focused" => {
                        effective.decoration.border_width_focused = Some(width);
                    }
                    _ => effective.decoration.border_width_idle = Some(width),
                }
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("decoration".to_string(), src.clone());
        }
        // CTX-0260/CTX-0334: `mouse.focus_follows_mouse` and its dwell
        // delay are scalar-replace like `selection.auto_copy`; absent table
        // means "says nothing".
        // (Second merge path: allow-policy-violations variant for diagnostics.)
        if let Some(mouse) = &plan.mouse {
            let field = "mouse.focus_follows_mouse";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mouse.focus_follows_mouse = mouse.focus_follows_mouse;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mouse.focus_follows_mouse = mouse.focus_follows_mouse;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            let field = "mouse.focus_follows_mouse_delay_ms";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mouse.focus_follows_mouse_delay_ms = mouse.focus_follows_mouse_delay_ms;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mouse.focus_follows_mouse_delay_ms = mouse.focus_follows_mouse_delay_ms;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
            attribution.insert("mouse".to_string(), src.clone());
        }
        if let Some(app) = &plan.appearance {
            let field = "appearance.theme";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.appearance.theme.clone_from(&app.theme);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
                attribution.insert("appearance".to_string(), src.clone());
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.appearance.theme.clone_from(&app.theme);
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
                attribution.insert("appearance".to_string(), src.clone());
            }
            // RFC-0002: the animations table deep-merges per leaf (second
            // merge path: allow-policy-violations variant for diagnostics).
            if let Some(over) = &app.animations {
                let mut acc = MergeAccumulators {
                    policy_fields: &mut policy_fields,
                    attribution: &mut attribution,
                    conflicts: &mut conflicts,
                    policy_violations: &mut policy_violations,
                };
                merge_animations_overrides(&mut effective, &mut acc, src, is_policy, over);
            }
        }
        // CTX-0236: `mod_key` is scalar-replace like `appearance.theme`;
        // absent means "says nothing" (lower-precedence value wins).
        // (Second merge path: allow-policy-violations variant for diagnostics.)
        if let Some(mod_key) = &plan.mod_key {
            let field = "mod_key";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.mod_key = *mod_key;
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::ScalarReplace,
                });
            } else {
                let prev = attribution.get(field).cloned();
                effective.mod_key = *mod_key;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::ScalarReplace,
                );
            }
        }
        if let Some(kms) = &plan.keymaps {
            let field = "keymaps";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.keymaps.clone_from(kms);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::SetById,
                });
            } else {
                let mut merged: HashMap<String, KeymapEntry> = effective
                    .keymaps
                    .iter()
                    .cloned()
                    .map(|e| (e.id(), e))
                    .collect();
                let prev = attribution.get(field).cloned();
                for km in kms {
                    merged.insert(km.id(), km.clone());
                }
                let mut v: Vec<KeymapEntry> = merged.into_values().collect();
                v.sort_by_key(|a| a.id());
                effective.keymaps = v;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            }
        }
        if let Some(ps) = &plan.plugins {
            let field = "plugins";
            if is_policy {
                policy_fields.insert(field.to_string(), src.clone());
                effective.plugins.clone_from(ps);
                let prev = attribution.get(field).cloned();
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            } else if let Some(policy_src) = policy_fields.get(field) {
                policy_violations.push(ConfigError::NonOverridable {
                    field: field.to_string(),
                    policy_source: policy_src.describe(),
                    attempted_source: src.describe(),
                });
                conflicts.push(MergeConflict {
                    field: field.to_string(),
                    previous_source: policy_src.clone(),
                    new_source: src.clone(),
                    merge_class: MergeClass::SetById,
                });
            } else {
                let mut merged: HashMap<String, PluginSpec> = effective
                    .plugins
                    .iter()
                    .cloned()
                    .map(|p| (p.id.trim().to_string(), p))
                    .collect();
                let prev = attribution.get(field).cloned();
                for p in ps {
                    merged.insert(p.id.trim().to_string(), p.clone());
                }
                let mut v: Vec<PluginSpec> = merged.into_values().collect();
                v.sort_by_key(|a| a.id.clone());
                effective.plugins = v;
                record_attribution(
                    &mut attribution,
                    &mut conflicts,
                    field,
                    prev,
                    src,
                    MergeClass::SetById,
                );
            }
        }
        if plan.extends.is_some() {
            let field = "extends";
            let prev = attribution.get(field).cloned();
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
        if plan.profile_name.is_some() {
            let field = "profile";
            let prev = attribution.get(field).cloned();
            effective.profile.clone_from(&plan.profile_name);
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
        if plan.schema_version.is_some() {
            let field = "schema_version";
            let prev = attribution.get(field).cloned();
            effective.schema_version = plan.effective_schema_version();
            record_attribution(
                &mut attribution,
                &mut conflicts,
                field,
                prev,
                src,
                MergeClass::ScalarReplace,
            );
        }
    }

    let core_src = ConfigSource::new(LayerKind::CoreDefaults, None::<String>);
    for field in ATTRIBUTED_FIELDS {
        attribution
            .entry((*field).to_string())
            .or_insert_with(|| core_src.clone());
    }

    effective.validate()?;

    Ok(MergedConfig {
        effective,
        attribution,
        conflicts,
        policy_violations,
    })
}

/// Resolve a single-parent `extends` chain for profiles with cycle detection.
///
/// `profiles` maps profile name to its plan. `start` is the entry profile
/// name. Multiple inheritance remains open and is not supported here.
pub fn resolve_profile_chain(
    profiles: &HashMap<String, ConfigPlan>,
    start: &str,
) -> Result<Vec<ConfigPlan>, ConfigError> {
    let mut chain: Vec<ConfigPlan> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut current = Some(start.to_string());

    while let Some(name) = current {
        if !visited.insert(name.clone()) {
            order.push(name.clone());
            return Err(ConfigError::CycleDetected { chain: order });
        }
        order.push(name.clone());
        let plan = profiles
            .get(&name)
            .ok_or_else(|| ConfigError::ProfileNotFound { name: name.clone() })?;
        chain.push(plan.clone());
        current = plan.extends.clone();
    }

    chain.reverse();
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
    use crate::types::{FontConfig, WindowConfig};

    fn plan_with_font(family: &str, size: f32) -> ConfigPlan {
        ConfigPlan {
            font: Some(FontConfig {
                family: family.into(),
                size,
                ..Default::default()
            }),
            schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
            ..Default::default()
        }
    }

    #[test]
    fn later_layer_wins_scalar_replace() {
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("a.lua")),
            plan_with_font("Mono", 12.0),
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            plan_with_font("JetBrains", 14.0),
        );
        let merged = merge_layers(vec![b.clone(), a.clone()]).expect("merge");
        assert_eq!(merged.effective.font.family, "JetBrains");
        assert_eq!(merged.effective.font.size, 14.0);
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::Cli
        );
        assert!(!merged.conflicts.is_empty());
    }

    #[test]
    fn mod_key_merges_scalar_replace_with_attribution() {
        // CTX-0236: user layer wins with per-field attribution; absent
        // keeps the lower-precedence value (Alt default).
        use crate::keymap::ModKey;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                mod_key: Some(ModKey::Super),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.mod_key, ModKey::Super);
        assert_eq!(merged.source_of("mod_key").unwrap().layer, LayerKind::User);
        assert_eq!(
            crate::merge::merge_class_for("mod_key"),
            Some(MergeClass::ScalarReplace)
        );
        // Absent rides the Alt default.
        let empty = merge_layers(vec![]).expect("empty merges");
        assert_eq!(empty.effective.mod_key, ModKey::Alt);
        // try_merge_layers agrees (second merge path).
        let user2 = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                mod_key: Some(ModKey::Super),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged2 = try_merge_layers(vec![user2]).expect("try merge");
        assert_eq!(merged2.effective.mod_key, ModKey::Super);
        assert_eq!(merged2.source_of("mod_key").unwrap().layer, LayerKind::User);
    }

    #[test]
    fn scrollbar_merges_scalar_replace_with_attribution() {
        // CTX-0181: user layer wins with per-field attribution; absent
        // table keeps the lower-precedence value (auto default, CTX-0362).
        use crate::types::{ScrollbarConfig, ScrollbarMode};
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                scrollbar: Some(ScrollbarConfig {
                    mode: ScrollbarMode::Auto,
                    width: 12,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.scrollbar.mode, ScrollbarMode::Auto);
        assert_eq!(merged.effective.scrollbar.width, 12);
        assert_eq!(
            merged.source_of("scrollbar.mode").unwrap().layer,
            LayerKind::User
        );
        assert_eq!(
            merged.source_of("scrollbar.width").unwrap().layer,
            LayerKind::User
        );
        // Absent table rides the auto default with core-defaults
        // attribution (like every other field).
        let merged2 = merge_layers(vec![]).expect("merge");
        assert_eq!(merged2.effective.scrollbar.mode, ScrollbarMode::Auto);
        assert_eq!(
            merged2.source_of("scrollbar.mode").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn mouse_focus_follows_mouse_merges_scalar_replace_with_attribution() {
        // CTX-0260: user opt-in wins with per-field attribution; absent
        // table keeps the lower-precedence value (off default).
        use crate::types::MouseConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                mouse: Some(MouseConfig {
                    focus_follows_mouse: true,
                    ..MouseConfig::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert!(merged.effective.mouse.focus_follows_mouse);
        assert_eq!(
            merged.source_of("mouse.focus_follows_mouse").unwrap().layer,
            LayerKind::User
        );
        // Absent table rides the off default with core-defaults attribution.
        let merged2 = merge_layers(vec![]).expect("merge");
        assert!(!merged2.effective.mouse.focus_follows_mouse);
        assert_eq!(
            merged2
                .source_of("mouse.focus_follows_mouse")
                .unwrap()
                .layer,
            LayerKind::CoreDefaults
        );
        // Later layer wins with a reported conflict.
        let cli = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, None::<String>),
            ConfigPlan {
                mouse: Some(MouseConfig {
                    focus_follows_mouse: false,
                    ..MouseConfig::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user2 = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                mouse: Some(MouseConfig {
                    focus_follows_mouse: true,
                    ..MouseConfig::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged3 = merge_layers(vec![user2, cli]).expect("merge");
        assert!(!merged3.effective.mouse.focus_follows_mouse);
        assert!(
            merged3
                .conflicts
                .iter()
                .any(|c| c.field == "mouse.focus_follows_mouse")
        );
    }

    #[test]
    fn mouse_focus_follows_mouse_delay_merges_scalar_replace_with_attribution() {
        // CTX-0334: the dwell delay rides the same scalar-replace path as
        // the enable bool, with its own attribution and core default.
        use crate::types::MouseConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                mouse: Some(MouseConfig {
                    focus_follows_mouse: true,
                    focus_follows_mouse_delay_ms: 250,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.mouse.focus_follows_mouse_delay_ms, 250);
        assert_eq!(
            merged
                .source_of("mouse.focus_follows_mouse_delay_ms")
                .unwrap()
                .layer,
            LayerKind::User
        );
        let merged2 = merge_layers(vec![]).expect("merge");
        assert_eq!(merged2.effective.mouse.focus_follows_mouse_delay_ms, 0);
        assert_eq!(
            merged2
                .source_of("mouse.focus_follows_mouse_delay_ms")
                .unwrap()
                .layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn merge_reports_conflicts() {
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::Distribution, Some("distro.lua")),
            plan_with_font("Mono", 12.0),
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            plan_with_font("Fira", 13.0),
        );
        let merged = merge_layers(vec![a, b]).expect("merge");
        assert!(merged.conflicts.iter().any(|c| c.field == "font.family"));
    }

    #[test]
    fn policy_prevents_override() {
        let policy = LayeredPlan::new(
            ConfigSource::new(LayerKind::SystemPolicy, Some("policy.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 0.9,
                    padding: 4,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 1.0,
                    padding: 8,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let err = merge_layers(vec![policy, user]).expect_err("policy must reject");
        assert!(matches!(err, ConfigError::NonOverridable { .. }));
    }

    #[test]
    fn try_merge_exposes_policy_violations() {
        let policy = LayeredPlan::new(
            ConfigSource::new(LayerKind::SystemPolicy, Some("policy.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 0.9,
                    padding: 4,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 1.0,
                    padding: 8,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = try_merge_layers(vec![policy, user]).expect("try_merge keeps value");
        assert!(!merged.policy_violations.is_empty());
        assert_eq!(merged.effective.window.opacity, 0.9);
    }

    #[test]
    fn set_by_id_merge_keymaps() {
        use crate::types::KeymapEntry;
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("a.lua")),
            ConfigPlan {
                keymaps: Some(vec![KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_next".into(),
                    context: "global".into(),
                }]),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                keymaps: Some(vec![KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_prev".into(),
                    context: "global".into(),
                }]),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![a, b]).expect("merge");
        assert_eq!(merged.effective.keymaps.len(), 1);
        assert_eq!(merged.effective.keymaps[0].action, "focus_prev");
        assert_eq!(merged.source_of("keymaps").unwrap().layer, LayerKind::Cli);
    }

    #[test]
    fn profile_cycle_detected() {
        let mut m = HashMap::new();
        m.insert(
            "a".to_string(),
            ConfigPlan {
                extends: Some("b".into()),
                ..Default::default()
            },
        );
        m.insert(
            "b".to_string(),
            ConfigPlan {
                extends: Some("a".into()),
                ..Default::default()
            },
        );
        assert!(resolve_profile_chain(&m, "a").is_err());
    }

    #[test]
    fn profile_chain_order() {
        let mut m = HashMap::new();
        m.insert(
            "base".to_string(),
            ConfigPlan {
                profile_name: Some("base".into()),
                ..Default::default()
            },
        );
        m.insert(
            "coding".to_string(),
            ConfigPlan {
                profile_name: Some("coding".into()),
                extends: Some("base".into()),
                ..Default::default()
            },
        );
        let chain = resolve_profile_chain(&m, "coding").expect("chain");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].profile_name.as_deref(), Some("base"));
        assert_eq!(chain[1].profile_name.as_deref(), Some("coding"));
    }

    #[test]
    fn attribution_survives_for_core_defaults() {
        let merged = merge_layers(vec![]).expect("empty layers merge to defaults");
        assert!(merged.source_of("font.family").is_some());
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn merge_class_table_coverage() {
        assert_eq!(
            merge_class_for("font.family"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(
            merge_class_for("font.line_height"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(
            merge_class_for("font.letter_spacing"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(merge_class_for("font"), Some(MergeClass::DeepMerge));
        assert_eq!(merge_class_for("keymaps"), Some(MergeClass::SetById));
        // CTX-0185: scroll speed keys are scalar-replace like scrollback.
        assert_eq!(
            merge_class_for("terminal.scroll_lines_per_notch"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(
            merge_class_for("terminal.scroll_pixels_per_notch"),
            Some(MergeClass::ScalarReplace)
        );
        // CTX-0191: selection opt-out is scalar-replace like scrollback.
        assert_eq!(
            merge_class_for("selection.auto_copy"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(merge_class_for("selection"), Some(MergeClass::DeepMerge));
        // CTX-0177: panel gaps are scalar-replace leaves under a deep table.
        assert_eq!(
            merge_class_for("layout.gaps_in"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(
            merge_class_for("layout.gaps_out"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(merge_class_for("layout"), Some(MergeClass::DeepMerge));
        // CTX-0241 S0: window radius is a scalar-replace leaf under `window`.
        assert_eq!(
            merge_class_for("window.radius_px"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(merge_class_for("window"), Some(MergeClass::DeepMerge));
        // CTX-0260/CTX-0334: hover-focus and its dwell delay are
        // scalar-replace leaves under `mouse`.
        assert_eq!(
            merge_class_for("mouse.focus_follows_mouse"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(
            merge_class_for("mouse.focus_follows_mouse_delay_ms"),
            Some(MergeClass::ScalarReplace)
        );
        assert_eq!(merge_class_for("mouse"), Some(MergeClass::DeepMerge));
        assert_eq!(merge_class_for("unknown"), None);
    }

    #[test]
    fn later_layer_wins_terminal_scroll_speed() {
        // CTX-0185: user scroll keys override defaults; CLI wins over file.
        use crate::types::TerminalConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                terminal: Some(TerminalConfig {
                    scrollback: 10_000,
                    shell: None,
                    scroll_lines_per_notch: 5,
                    scroll_pixels_per_notch: 24,
                }),
                ..Default::default()
            },
        );
        let cli = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                terminal: Some(TerminalConfig {
                    scrollback: 10_000,
                    shell: None,
                    scroll_lines_per_notch: 2,
                    scroll_pixels_per_notch: 8,
                }),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user, cli]).expect("merge");
        assert_eq!(merged.effective.terminal.scroll_lines_per_notch, 2);
        assert_eq!(merged.effective.terminal.scroll_pixels_per_notch, 8);
        assert_eq!(
            merged
                .source_of("terminal.scroll_lines_per_notch")
                .unwrap()
                .layer,
            LayerKind::Cli
        );
        assert!(
            merged
                .conflicts
                .iter()
                .any(|c| c.field == "terminal.scroll_lines_per_notch")
        );
    }

    #[test]
    fn later_layer_wins_selection_auto_copy() {
        // CTX-0191: user opt-out overrides the default-on; CLI wins over file.
        // Absent table means "says nothing" so defaults survive.
        use crate::types::SelectionConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                selection: Some(SelectionConfig { auto_copy: false }),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert!(!merged.effective.selection.auto_copy);
        assert_eq!(
            merged.source_of("selection.auto_copy").unwrap().layer,
            LayerKind::User
        );
        // No layers at all -> default-on survives with core-defaults source.
        let merged_default = merge_layers(vec![]).expect("merge");
        assert!(merged_default.effective.selection.auto_copy);
        // CLI opt-out wins over a user opt-in.
        let user_in = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                selection: Some(SelectionConfig { auto_copy: true }),
                ..Default::default()
            },
        );
        let cli_out = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                selection: Some(SelectionConfig { auto_copy: false }),
                ..Default::default()
            },
        );
        let merged2 = merge_layers(vec![user_in, cli_out]).expect("merge");
        assert!(!merged2.effective.selection.auto_copy);
        assert_eq!(
            merged2.source_of("selection.auto_copy").unwrap().layer,
            LayerKind::Cli
        );
        assert!(
            merged2
                .conflicts
                .iter()
                .any(|c| c.field == "selection.auto_copy")
        );
    }

    #[test]
    fn spacing_fields_merge_scalar_replace() {
        use crate::types::{DEFAULT_LETTER_SPACING, DEFAULT_LINE_HEIGHT};
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("a.lua")),
            ConfigPlan {
                font: Some(FontConfig {
                    family: "Mono".into(),
                    size: 12.0,
                    line_height: 1.0,
                    letter_spacing: 0.0,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                font: Some(FontConfig {
                    family: "Mono".into(),
                    size: 12.0,
                    line_height: DEFAULT_LINE_HEIGHT,
                    letter_spacing: DEFAULT_LETTER_SPACING,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![a, b]).expect("merge");
        assert!((merged.effective.font.line_height - DEFAULT_LINE_HEIGHT).abs() < f32::EPSILON);
        assert!(
            (merged.effective.font.letter_spacing - DEFAULT_LETTER_SPACING).abs() < f32::EPSILON
        );
        assert_eq!(
            merged.source_of("font.line_height").unwrap().layer,
            LayerKind::Cli
        );
    }

    #[test]
    fn layout_gaps_merge_scalar_replace_with_attribution() {
        // CTX-0177: user gaps land in effective with user attribution; CLI
        // wins over file; absent layers keep core defaults (edge-to-edge).
        use crate::types::LayoutConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                layout: Some(LayoutConfig {
                    gaps_in: 1,
                    gaps_out: 2,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.layout.gaps_in, 1);
        assert_eq!(merged.effective.layout.gaps_out, 2);
        assert_eq!(
            merged.source_of("layout.gaps_in").unwrap().layer,
            LayerKind::User
        );
        assert_eq!(
            merged.source_of("layout.gaps_out").unwrap().layer,
            LayerKind::User
        );
        let cli = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                layout: Some(LayoutConfig {
                    gaps_in: 3,
                    gaps_out: 0,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user2 = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                layout: Some(LayoutConfig {
                    gaps_in: 1,
                    gaps_out: 2,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged2 = merge_layers(vec![user2, cli]).expect("merge");
        assert_eq!(merged2.effective.layout.gaps_in, 3);
        assert_eq!(
            merged2.source_of("layout.gaps_in").unwrap().layer,
            LayerKind::Cli
        );
        assert!(
            merged2
                .conflicts
                .iter()
                .any(|c| c.field == "layout.gaps_in")
        );
        // Empty stack keeps zero gaps with core-defaults attribution.
        let merged3 = merge_layers(vec![]).expect("empty layers merge");
        assert_eq!(merged3.effective.layout.gaps_in, 0);
        assert_eq!(merged3.effective.layout.gaps_out, 0);
        assert_eq!(
            merged3.source_of("layout.gaps_in").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn decoration_merges_scalar_replace_with_attribution() {
        // CTX-0292/CTX-0333: user decoration lands in effective with user
        // attribution; CLI wins over file; absent layers keep the unified
        // defaults (gaps 6/6, border 2, radius 6, content inset 6) with
        // core-defaults attribution.
        use crate::types::DecorationConfig;
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    gaps_in: 0,
                    gaps_out: 0,
                    border: 1,
                    radius: 0,
                    content_inset: 0,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.decoration.gaps_in, 0);
        assert_eq!(merged.effective.decoration.border, 1);
        assert_eq!(merged.effective.decoration.content_inset, 0);
        assert_eq!(
            merged.source_of("decoration.gaps_in").unwrap().layer,
            LayerKind::User
        );
        assert_eq!(
            merged.source_of("decoration.border").unwrap().layer,
            LayerKind::User
        );
        assert_eq!(
            merged.source_of("decoration.content_inset").unwrap().layer,
            LayerKind::User
        );
        let cli = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    gaps_in: 8,
                    gaps_out: 8,
                    border: 4,
                    radius: 12,
                    content_inset: 3,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user2 = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    gaps_in: 0,
                    gaps_out: 0,
                    border: 1,
                    radius: 0,
                    content_inset: 0,
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged2 = merge_layers(vec![user2, cli]).expect("merge");
        assert_eq!(merged2.effective.decoration.gaps_in, 8);
        assert_eq!(merged2.effective.decoration.radius, 12);
        assert_eq!(merged2.effective.decoration.content_inset, 3);
        assert_eq!(
            merged2.source_of("decoration.gaps_in").unwrap().layer,
            LayerKind::Cli
        );
        assert!(
            merged2
                .conflicts
                .iter()
                .any(|c| c.field == "decoration.gaps_in")
        );
        // Empty stack keeps the accepted defaults with core attribution.
        let merged3 = merge_layers(vec![]).expect("empty layers merge");
        assert_eq!(merged3.effective.decoration, DecorationConfig::default());
        assert_eq!(
            merged3.source_of("decoration.gaps_in").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn decoration_colors_scalar_replace_and_unset_never_shadows() {
        // CTX-0340: only an explicit color overrides the lower layer; an
        // unset (`None`) member says nothing and never clobbers it. The base
        // and the explicit pair are independent scalar-replace fields.
        use crate::types::{DecorationConfig, OutlineColor};
        // Base and focused differ and clear AC-2; the base is a neutral
        // gray the user never overrides.
        let base = OutlineColor([0x59, 0x59, 0x59, 0xFF]);
        let focused = OutlineColor([0x33, 0xCC, 0xFF, 0xFF]);
        let profile = LayeredPlan::new(
            ConfigSource::new(LayerKind::Profile, Some("profile.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    border_color: Some(base),
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        // User sets only the focused member; the base must survive.
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    border_color_focused: Some(focused),
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user, profile]).expect("merge");
        assert_eq!(merged.effective.decoration.border_color, Some(base));
        assert_eq!(
            merged.effective.decoration.border_color_focused,
            Some(focused)
        );
        assert_eq!(merged.effective.decoration.border_color_idle, None);
        assert_eq!(
            merged.source_of("decoration.border_color").unwrap().layer,
            LayerKind::Profile
        );
        assert_eq!(
            merged
                .source_of("decoration.border_color_focused")
                .unwrap()
                .layer,
            LayerKind::User
        );
        // No color set anywhere: the effective stays `None` (theme token).
        let empty = merge_layers(vec![]).expect("empty merge");
        assert_eq!(empty.effective.decoration.border_color, None);
        assert_eq!(
            empty.source_of("decoration.border_color").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn decoration_widths_scalar_replace_and_unset_never_shadows() {
        // CTX-0344: outline widths follow the same scalar-replace +
        // "unset says nothing" rule as the colors.
        use crate::types::DecorationConfig;
        let profile = LayeredPlan::new(
            ConfigSource::new(LayerKind::Profile, Some("profile.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    border_width: Some(3),
                    border_width_idle: Some(1),
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        // User sets only the focused member; the base/idle must survive.
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                decoration: Some(DecorationConfig {
                    border_width_focused: Some(6),
                    ..Default::default()
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user, profile]).expect("merge");
        assert_eq!(merged.effective.decoration.border_width, Some(3));
        assert_eq!(merged.effective.decoration.border_width_focused, Some(6));
        assert_eq!(merged.effective.decoration.border_width_idle, Some(1));
        assert_eq!(
            merged.source_of("decoration.border_width").unwrap().layer,
            LayerKind::Profile
        );
        assert_eq!(
            merged
                .source_of("decoration.border_width_focused")
                .unwrap()
                .layer,
            LayerKind::User
        );
        // No width set anywhere: the effective stays `None` (inherits
        // `border`).
        let empty = merge_layers(vec![]).expect("empty merge");
        assert_eq!(empty.effective.decoration.border_width, None);
        assert_eq!(
            empty.source_of("decoration.border_width").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn safe_merged_forces_safe_decoration_with_core_attribution() {
        // CTX-0346: the safe merged config is the built-in safe effective
        // config with every schema field attributed to core defaults; there
        // are no conflicts because no user/profile/CLI layer participates.
        use crate::types::DecorationConfig;
        let merged = safe_merged().expect("safe merge");
        assert_eq!(merged.effective, crate::reload::fallback_builtin());
        assert_eq!(merged.effective.decoration, DecorationConfig::safe());
        assert_eq!(merged.effective.decoration.gaps_in, 0);
        assert_eq!(merged.effective.decoration.gaps_out, 0);
        assert_eq!(merged.effective.decoration.border, 1);
        assert_eq!(merged.effective.decoration.radius, 0);
        assert_eq!(merged.effective.decoration.content_inset, 0);
        assert!(merged.conflicts.is_empty());
        assert!(merged.policy_violations.is_empty());
        for field in [
            "decoration.gaps_in",
            "decoration.border_color_focused",
            "decoration.border_color_idle",
            "terminal.shell",
            "appearance.theme",
        ] {
            assert_eq!(
                merged.source_of(field).map(|s| s.layer),
                Some(LayerKind::CoreDefaults),
                "field {field} must be core-default attributed"
            );
        }
    }

    #[test]
    fn window_radius_merges_scalar_replace_with_attribution() {
        // CTX-0241 S0: user radius lands in effective with user attribution;
        // later layers win; empty stack keeps 0 with core-defaults source.
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 1.0,
                    padding: 8,
                    radius_px: 12,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![user]).expect("merge");
        assert_eq!(merged.effective.window.radius_px, 12);
        assert_eq!(
            merged.source_of("window.radius_px").unwrap().layer,
            LayerKind::User
        );
        let cli = LayeredPlan::new(
            ConfigSource::new(LayerKind::Cli, Some("cli")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 1.0,
                    padding: 8,
                    radius_px: 6,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user2 = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                window: Some(WindowConfig {
                    opacity: 1.0,
                    padding: 8,
                    radius_px: 12,
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged2 = merge_layers(vec![user2, cli]).expect("merge");
        assert_eq!(merged2.effective.window.radius_px, 6);
        assert_eq!(
            merged2.source_of("window.radius_px").unwrap().layer,
            LayerKind::Cli
        );
        assert!(
            merged2
                .conflicts
                .iter()
                .any(|c| c.field == "window.radius_px")
        );
        let merged3 = merge_layers(vec![]).expect("empty layers merge");
        assert_eq!(merged3.effective.window.radius_px, 0);
        assert_eq!(
            merged3.source_of("window.radius_px").unwrap().layer,
            LayerKind::CoreDefaults
        );
    }

    #[test]
    fn animations_merge_per_field_with_attribution() {
        // RFC-0002 (CTX-0341): each present leaf is scalar-replace with its
        // own attribution; absent leaves inherit the lower layer / defaults.
        use crate::types::{AnimationEasing, AnimationsOverride, ReducedMotion};
        let system = LayeredPlan::new(
            ConfigSource::new(LayerKind::SystemDefaults, Some("system.lua")),
            ConfigPlan {
                appearance: Some(crate::types::AppearanceConfig {
                    theme: None,
                    animations: Some(AnimationsOverride {
                        duration_open: Some(250),
                        easing_open: Some(AnimationEasing::Linear),
                        ..Default::default()
                    }),
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("user.lua")),
            ConfigPlan {
                appearance: Some(crate::types::AppearanceConfig {
                    theme: None,
                    animations: Some(AnimationsOverride {
                        duration_open: Some(500),
                        enabled: Some(false),
                        reduced_motion: Some(ReducedMotion::Always),
                        ..Default::default()
                    }),
                }),
                schema_version: Some(crate::migration::CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let merged = merge_layers(vec![system.clone(), user.clone()]).expect("merge");
        // User wins the leaves it declared.
        assert_eq!(merged.effective.animations.duration_ms.open, 500);
        assert!(!merged.effective.animations.enabled);
        assert_eq!(
            merged.effective.animations.reduced_motion,
            ReducedMotion::Always
        );
        // System's easing survives because user said nothing about it.
        assert_eq!(
            merged.effective.animations.easing.open,
            AnimationEasing::Linear
        );
        // Untouched leaves keep the accepted defaults.
        assert_eq!(
            merged.effective.animations.duration_ms.close,
            crate::types::DEFAULT_ANIMATION_CLOSE_MS
        );
        assert_eq!(
            merged.effective.animations.easing.focus,
            AnimationEasing::EaseInOut
        );
        // Per-leaf attribution answers the declaring layer.
        assert_eq!(
            merged
                .source_of("appearance.animations.duration_ms.open")
                .unwrap()
                .layer,
            LayerKind::User
        );
        assert_eq!(
            merged
                .source_of("appearance.animations.easing.open")
                .unwrap()
                .layer,
            LayerKind::SystemDefaults
        );
        assert_eq!(
            merged
                .source_of("appearance.animations.enabled")
                .unwrap()
                .layer,
            LayerKind::User
        );
        // A leaf nobody declared rides core defaults.
        assert_eq!(
            merged
                .source_of("appearance.animations.duration_ms.close")
                .unwrap()
                .layer,
            LayerKind::CoreDefaults
        );
        assert!(
            merged
                .conflicts
                .iter()
                .any(|c| c.field == "appearance.animations.duration_ms.open")
        );
        // Empty stack keeps the accepted contract defaults.
        let empty = merge_layers(vec![]).expect("empty merge");
        assert!(empty.effective.animations.enabled);
        assert_eq!(empty.effective.animations.duration_ms.open, 150);
        assert_eq!(
            empty.effective.animations.reduced_motion,
            ReducedMotion::Auto
        );
        assert_eq!(
            merge_class_for("appearance.animations"),
            Some(MergeClass::DeepMerge)
        );
        assert_eq!(
            merge_class_for("appearance.animations.duration_ms.open"),
            Some(MergeClass::ScalarReplace)
        );
        // try_merge_layers agrees (second merge path).
        let merged2 = try_merge_layers(vec![system, user]).expect("try merge");
        assert_eq!(merged2.effective.animations.duration_ms.open, 500);
        assert_eq!(
            merged2.effective.animations.easing.open,
            AnimationEasing::Linear
        );
    }
}
