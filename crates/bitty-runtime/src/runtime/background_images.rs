//! `Runtime` — per-`View` background-image loading (CTX-0347).
//!
//! RFC-0001/OQ-042 accepted contract: the Core-owned user-configuration path
//! for `decoration.background_image` / `background_fit` /
//! `background_image_roots` plus the per-`View` `background_image` /
//! `background_fit` fields. This module is the whole-reload gate: every
//! configured image is resolved, trust-checked, sniffed, decoded, and
//! admitted **at construction** (off the present hot path), and any failure
//! rejects the entire config with a source-attributed diagnostic. The present
//! path then only looks up a decoded image and a bounded scaled-blit cache.
//!
//! `bitty --safe` never reaches a load: the safe effective config clears the
//! image and the roots, so the store stays deny-by-default and empty and no
//! file is opened.

use super::*;

/// Builds the deny-by-default root policy from
/// `decoration.background_image_roots` (global-only; never widened by a
/// `views` rule).
fn background_policy(config: &RuntimeConfig) -> Result<bitty_rich::ResourcePolicy, RuntimeError> {
    if config.background_image_roots.is_empty() {
        return Ok(bitty_rich::ResourcePolicy::deny_all());
    }
    let roots = config
        .background_image_roots
        .iter()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    bitty_rich::ResourcePolicy::with_max_roots(roots, bitty_rich::BG_MAX_ROOTS).map_err(|err| {
        RuntimeError::BackgroundImage(format!("decoration.background_image_roots: {err}"))
    })
}

/// Rebuilds the full background state (decoded store plus the configured
/// path -> identity map) for `config`, fail-closed.
///
/// Every configured image — the global `decoration.background_image` and
/// every `views[<selector>].background_image` — is loaded in configuration
/// order. The first failure returns [`RuntimeError::BackgroundImage`] naming
/// the owning key; nothing is partially applied because the caller only
/// swaps state on success.
fn build_background_state(
    config: &RuntimeConfig,
) -> Result<
    (
        bitty_rich::BackgroundStore,
        std::collections::HashMap<String, bitty_rich::BackgroundKey>,
    ),
    RuntimeError,
> {
    let mut store = bitty_rich::BackgroundStore::new(background_policy(config)?);
    let mut keys = std::collections::HashMap::new();
    let mut load = |key: &str, path: &str| -> Result<(), RuntimeError> {
        let cache_key = store
            .load(path, None)
            .map_err(|err| RuntimeError::BackgroundImage(format!("{key}: {err}")))?;
        store.pin(&cache_key);
        keys.insert(path.to_string(), cache_key);
        Ok(())
    };
    if let Some(path) = config.background_image.as_deref() {
        load("decoration.background_image", path)?;
    }
    for rule in &config.view_appearance {
        if let Some(path) = rule.background_image.as_deref() {
            let key = format!("views[{}].background_image", rule.selector);
            load(&key, path)?;
        }
    }
    Ok((store, keys))
}

/// Validates every configured background image without constructing a
/// runtime (CTX-0347 `bitty config check`).
///
/// Runs exactly the startup pipeline — approved-root policy, canonicalize,
/// regular file, BG-1..BG-3, format/animation sniff, decode, BG-4/BG-5
/// admission — so `config check` and startup can never disagree.
///
/// # Errors
///
/// [`RuntimeError::BackgroundImage`] naming the owning config key and the
/// rejection reason.
pub fn validate_background_images(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    let _ = build_background_state(config)?;
    Ok(())
}

impl Runtime {
    /// Number of decoded background images resident (headless-observable).
    #[must_use]
    pub fn background_image_count(&self) -> usize {
        self.backgrounds.len()
    }

    /// Aggregate decoded background bytes resident (BG-4 headless proof).
    #[must_use]
    pub fn background_bytes(&self) -> usize {
        self.backgrounds.total_bytes()
    }

    /// Successful background decodes since construction (identity-mismatched
    /// reloads included); `0` proves a `--safe` startup opened no image.
    #[must_use]
    pub fn background_loads(&self) -> u64 {
        self.backgrounds.loads()
    }

    /// Scaled-blit cache counters: hits, misses, resident entries/bytes
    /// (CTX-0347 proof that static frames reuse the per-frame blit and that
    /// geometry changes invalidate it).
    #[must_use]
    pub fn background_raster_stats(&self) -> (u64, u64, usize, usize) {
        (
            self.background_rasters.hits(),
            self.background_rasters.misses(),
            self.background_rasters.len(),
            self.background_rasters.total_bytes(),
        )
    }

    /// Rebuilds the approved-root policy and loads every configured
    /// background image, fail-closed.
    ///
    /// Runs at construction (whole-config gate). Any failure returns
    /// [`RuntimeError::BackgroundImage`] naming the owning config key and
    /// leaves the runtime unconstructed, so no partial appearance can be
    /// composed.
    pub(crate) fn reload_backgrounds(&mut self) -> Result<(), RuntimeError> {
        let (store, keys) = build_background_state(&self.config)?;
        self.backgrounds = store;
        self.background_keys = keys;
        self.background_rasters = bitty_rich::BackgroundRasterCache::new();
        Ok(())
    }
}
