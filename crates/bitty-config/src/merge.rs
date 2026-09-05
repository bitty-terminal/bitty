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
        | "terminal.scrollback"
        | "terminal.shell"
        | "terminal.scroll_lines_per_notch"
        | "terminal.scroll_pixels_per_notch"
        | "selection.auto_copy"
        | "layout.gaps_in"
        | "layout.gaps_out"
        | "appearance.theme"
        | "extends"
        | "profile"
        | "schema_version" => Some(MergeClass::ScalarReplace),
        "font" | "window" | "terminal" | "selection" | "layout" | "appearance" => {
            Some(MergeClass::DeepMerge)
        }
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
            for field in ["window.opacity", "window.padding"] {
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
    for field in [
        "font.family",
        "font.size",
        "font.line_height",
        "font.letter_spacing",
        "font",
        "window.opacity",
        "window.padding",
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
        "appearance.theme",
        "appearance",
        "keymaps",
        "plugins",
        "schema_version",
    ] {
        attribution
            .entry(field.to_string())
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
            for field in ["window.opacity", "window.padding"] {
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
    for field in [
        "font.family",
        "font.size",
        "font.line_height",
        "font.letter_spacing",
        "font",
        "window.opacity",
        "window.padding",
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
        "appearance.theme",
        "appearance",
        "keymaps",
        "plugins",
        "schema_version",
    ] {
        attribution
            .entry(field.to_string())
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
}
