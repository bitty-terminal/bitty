//! Live plugin-suggested keybindings (issue #2407, ADR-0014 stage 3).
//!
//! `bitty.keymaps.suggest` captures up to
//! [`REGISTRATION_MAX_KEYMAP_SUGGESTIONS`](bitty_lua::REGISTRATION_MAX_KEYMAP_SUGGESTIONS)
//! suggestions per activation, and the commit gate bounds their shape. This
//! module publishes the active generation's suggestions into a Core-owned
//! directory the application consults at the reserved
//! `DispatchPriority::Plugin` slot: after the user keymap table and before
//! terminal encoding.
//!
//! Publication is generation-fenced and lifecycle-driven:
//!
//! - [`SuggestedBindingDirectory::publish`] replaces one owner's suggestions
//!   when its generation activates (and again on reload, under the new
//!   generation).
//! - [`SuggestedBindingDirectory::suspend_owner`] parks them (disable,
//!   unload-to-suspend) and [`SuggestedBindingDirectory::resume_owner`]
//!   restores them in place.
//! - [`SuggestedBindingDirectory::revoke_owner`] drops them (unload, crash,
//!   dispose, failed activation), so a dead generation can never keep a
//!   chord.
//!
//! Only the application's cold path mutates the directory; the input path
//! holds a read borrow for one exact chord comparison and clones nothing
//! unless a suggestion wins. No plugin callback runs while matching: a match
//! resolves to the same host-mediated [`ChromeAction::InvokeCommand`] a user
//! `command:<owner:command>` binding uses, so dispatch keeps every existing
//! registration, schema, liveness, and generation gate.
//!
//! Ordering is deterministic across generations: owners iterate in
//! lexicographic order and each owner's suggestions are sorted by canonical
//! chord then qualified command, so activation order, reloads, and hash
//! iteration can never change which suggestion owns a chord. An explicit user
//! binding always wins because the application only consults this directory
//! when the user table produced no match.
//!
//! # Conflict diagnostics
//!
//! A chord published by more than one suggestion has exactly one dispatch
//! owner (lexicographically first owner, then first registered suggestion),
//! but the losers are never silently invisible: [`SuggestedBindingDirectory::publish`]
//! records one bounded [`SuggestedChordConflict`] row per shadowed
//! suggestion.
//! The application drains those on its cold path and logs them as warnings,
//! so a shipped plugin collision is a visible diagnostic instead of an
//! arbitrary winner (the accepted LUA-OQ-5 conflict-diagnostic rule). The
//! deterministic tiebreak stays as the documented fallback for dispatch.

use std::collections::BTreeMap;

use bitty_config::{Chord, ChromeAction, KeyRef, ModKey};

/// Maximum pending conflict diagnostics retained between cold-path drains.
///
/// Bounded like the notification queue: a hostile or broken plugin set can
/// flood publications, but the application only ever logs this many rows per
/// drain and the overflow is counted (never hidden as memory growth).
pub const MAX_KEYMAP_SUGGESTION_CONFLICTS: usize = 32;

/// One chord claimed by more than one published suggestion.
///
/// `winner` dispatches; `loser` is shadowed. Both are qualified
/// `owner:command` names, and `chord` is the captured (Mod-independent)
/// spelling. Rows are emitted at publication (one per shadowed suggestion)
/// for the LUA-OQ-5 conflict diagnostics and drained by the application's
/// cold path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedChordConflict {
    /// Colliding chord in captured spelling.
    pub chord: Chord,
    /// Qualified command that owns the chord.
    pub winner: String,
    /// Qualified command shadowed by `winner`.
    pub loser: String,
}

/// One published suggestion: a chord bound to a same-generation command.
///
/// `qualified` is the exact `owner:command` name captured from
/// `bitty.keymaps.suggest`. The runtime publishes only suggestions whose
/// command is registered by the same activation capture, so the target is
/// always owner-local; a stale or foreign name never reaches this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedBinding {
    /// Owning plugin id (the suggestion's `generation` owner).
    pub owner: String,
    /// Activation generation that captured the suggestion.
    pub generation: u32,
    /// Normalized chord, stored in the captured spelling (the configured Mod
    /// slot is applied at lookup, never baked in, so a `mod_key` reload
    /// re-spells every `alt`-bearing suggestion without a republish).
    pub chord: Chord,
    /// Qualified command (`owner:command`) registered by this generation.
    pub qualified: String,
}

/// One owner's published suggestions plus its lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerBindings {
    generation: u32,
    suspended: bool,
    /// Sorted by canonical chord then qualified command; one binding per
    /// chord (first registered wins).
    bindings: Vec<SuggestedBinding>,
}

impl OwnerBindings {
    fn new(generation: u32, bindings: Vec<SuggestedBinding>) -> Self {
        Self {
            generation,
            suspended: false,
            bindings,
        }
    }
}

/// Live Core-side directory of plugin-suggested keybindings.
///
/// Owned by [`PluginRuntime`](super::PluginRuntime), shared with the
/// application by reference, and mutated only on the cold path
/// (activation, suspend, resume, dispose, reload, failed activation).
#[derive(Debug, Default)]
pub struct SuggestedBindingDirectory {
    owners: BTreeMap<String, OwnerBindings>,
    /// Undrained conflict diagnostics, bounded by
    /// [`MAX_KEYMAP_SUGGESTION_CONFLICTS`].
    conflicts: Vec<SuggestedChordConflict>,
    /// Conflict rows dropped because the pending buffer was full (bounded
    /// overflow accounting; never silent memory growth).
    conflicts_dropped: u64,
}

impl SuggestedBindingDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish one generation's suggestions, replacing the owner's previous
    /// set.
    ///
    /// The caller has already validated owner-local command registration and
    /// chord grammar; this method additionally pins every row to the calling
    /// owner and generation, normalizes ordering, and keeps the first
    /// registered suggestion when one owner repeats a chord.
    ///
    /// Collisions produce one bounded [`SuggestedChordConflict`] row per
    /// shadowed suggestion: a repeated chord inside this publication (first
    /// registered wins) and a chord already claimed by another owner
    /// (lexicographically smaller owner wins). The winner is deterministic,
    /// but it is never silent — the application drains and logs the rows on
    /// its cold path.
    pub fn publish(&mut self, owner: &str, generation: u32, bindings: Vec<SuggestedBinding>) {
        let mut kept: BTreeMap<String, SuggestedBinding> = BTreeMap::new();
        let mut internal: Vec<SuggestedChordConflict> = Vec::new();
        for mut binding in bindings {
            // Pin the row to this publication: a caller can never publish a
            // row under another owner or a stale generation.
            binding.owner = owner.to_string();
            binding.generation = generation;
            let canonical = binding.chord.canonical();
            if let Some(existing) = kept.get(&canonical) {
                internal.push(SuggestedChordConflict {
                    chord: binding.chord,
                    winner: existing.qualified.clone(),
                    loser: binding.qualified.clone(),
                });
                continue;
            }
            kept.insert(canonical, binding);
        }
        let mut normalized: Vec<SuggestedBinding> = kept.into_values().collect();
        normalized.sort_by(|a, b| {
            a.chord
                .canonical()
                .cmp(&b.chord.canonical())
                .then_with(|| a.qualified.cmp(&b.qualified))
        });
        // Cross-owner collisions: one diagnostic per colliding chord at the
        // publication that introduces the loser. The owner map already
        // iterates lexicographically, so the first owner to claim a chord is
        // also the dispatch winner (`action_for` uses the same order).
        let mut cross: Vec<SuggestedChordConflict> = Vec::new();
        for binding in &normalized {
            let canonical = binding.chord.canonical();
            for (other_owner, other) in &self.owners {
                if other_owner == owner {
                    continue;
                }
                for existing in &other.bindings {
                    if existing.chord.canonical() != canonical {
                        continue;
                    }
                    let (winner, loser) = if other_owner.as_str() < owner {
                        (existing.qualified.clone(), binding.qualified.clone())
                    } else {
                        (binding.qualified.clone(), existing.qualified.clone())
                    };
                    cross.push(SuggestedChordConflict {
                        chord: binding.chord,
                        winner,
                        loser,
                    });
                }
            }
        }
        self.owners.insert(
            owner.to_string(),
            OwnerBindings::new(generation, normalized),
        );
        for conflict in internal.into_iter().chain(cross) {
            self.record_conflict(conflict);
        }
    }

    /// Take every pending conflict diagnostic, oldest first.
    ///
    /// Called by the application's cold path, which logs each row once.
    /// Draining is bounded by [`MAX_KEYMAP_SUGGESTION_CONFLICTS`]; the
    /// overflow counter ([`Self::conflicts_dropped`]) is not reset.
    pub fn drain_conflicts(&mut self) -> Vec<SuggestedChordConflict> {
        std::mem::take(&mut self.conflicts)
    }

    /// Pending (not yet drained) conflict diagnostics, oldest first.
    ///
    /// Read-only view for diagnostics and tests; the application drains the
    /// buffer instead of copying it on the input path.
    #[must_use]
    pub fn pending_conflicts(&self) -> &[SuggestedChordConflict] {
        &self.conflicts
    }

    /// Conflict rows dropped since startup because the pending buffer was
    /// full (diagnostics only; never a dispatch input).
    #[must_use]
    pub fn conflicts_dropped(&self) -> u64 {
        self.conflicts_dropped
    }

    fn record_conflict(&mut self, conflict: SuggestedChordConflict) {
        if self.conflicts.len() >= MAX_KEYMAP_SUGGESTION_CONFLICTS {
            self.conflicts_dropped = self.conflicts_dropped.saturating_add(1);
            return;
        }
        self.conflicts.push(conflict);
    }

    /// Park one owner's suggestions (suspend / disable). They stay keyed for
    /// resume but match nothing while parked.
    pub fn suspend_owner(&mut self, owner: &str) {
        if let Some(entry) = self.owners.get_mut(owner) {
            entry.suspended = true;
        }
    }

    /// Restore one owner's parked suggestions (resume).
    pub fn resume_owner(&mut self, owner: &str) {
        if let Some(entry) = self.owners.get_mut(owner) {
            entry.suspended = false;
        }
    }

    /// Drop one owner's suggestions (unload, crash, dispose, failed
    /// activation).
    pub fn revoke_owner(&mut self, owner: &str) {
        self.owners.remove(owner);
    }

    /// Whether the owner currently has parked suggestions.
    #[must_use]
    pub fn is_suspended(&self, owner: &str) -> bool {
        self.owners.get(owner).is_some_and(|entry| entry.suspended)
    }

    /// Published generation for `owner`, if any (diagnostics and tests).
    #[must_use]
    pub fn generation_of(&self, owner: &str) -> Option<u32> {
        self.owners.get(owner).map(|entry| entry.generation)
    }

    /// Active suggestions in deterministic order (diagnostics and tests).
    #[must_use]
    pub fn active(&self) -> Vec<SuggestedBinding> {
        self.owners
            .values()
            .filter(|entry| !entry.suspended)
            .flat_map(|entry| entry.bindings.iter().cloned())
            .collect()
    }

    /// Number of owners with published (suspended or live) suggestions.
    #[must_use]
    pub fn owner_count(&self) -> usize {
        self.owners.len()
    }

    /// True when no generation has published suggestions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// Resolve one keypress to the winning suggestion's host-mediated action.
    ///
    /// Consultation order is the deterministic directory order; the first
    /// match wins. The configured [`ModKey`] is applied to every stored chord
    /// at lookup time, so `alt`-bearing suggestions follow a `mod_key` flip
    /// while mod-independent chords pass through unchanged. Returns `None`
    /// when no live generation suggests the chord (terminal fall-through) and
    /// clones nothing in that case.
    #[must_use]
    pub fn action_for(&self, keyref: KeyRef, mod_key: ModKey) -> Option<ChromeAction> {
        let unshifted = keyref.unshifted_base();
        for entry in self.owners.values() {
            if entry.suspended {
                continue;
            }
            for binding in &entry.bindings {
                // Generation fence: a row whose generation no longer matches
                // its owner entry is stale and can never dispatch.
                if binding.generation != entry.generation {
                    continue;
                }
                let chord = mod_key.apply_to(binding.chord);
                if keyref.matches(&chord) || unshifted.is_some_and(|base| base.matches(&chord)) {
                    return Some(ChromeAction::InvokeCommand(binding.qualified.clone()));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::KeyName;

    fn binding(owner: &str, generation: u32, chord: &str, qualified: &str) -> SuggestedBinding {
        SuggestedBinding {
            owner: owner.to_string(),
            generation,
            chord: Chord::parse(chord).expect("test chord parses"),
            qualified: qualified.to_string(),
        }
    }

    fn alt_question() -> KeyRef {
        KeyRef {
            key: KeyName::Char('?'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        }
    }

    fn super_question() -> KeyRef {
        KeyRef {
            key: KeyName::Char('?'),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: true,
        }
    }

    #[test]
    fn publish_then_lookup_resolves_owner_action() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish("help", 3, vec![binding("help", 3, "alt+?", "help:toggle")]);
        assert_eq!(
            directory.action_for(alt_question(), ModKey::Alt),
            Some(ChromeAction::InvokeCommand("help:toggle".to_string()))
        );
        assert!(
            directory
                .action_for(super_question(), ModKey::Alt)
                .is_none()
        );
    }

    #[test]
    fn mod_key_rewrites_alt_bearing_suggestions_only() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish(
            "help",
            1,
            vec![
                binding("help", 1, "alt+?", "help:toggle"),
                binding("help", 1, "ctrl+k", "help:open"),
            ],
        );
        // Alt spelling under the default Mod.
        assert!(directory.action_for(alt_question(), ModKey::Alt).is_some());
        // Mod flip: the alt slot moves to Super, the fixed chord stays put.
        assert!(
            directory
                .action_for(alt_question(), ModKey::Super)
                .is_none()
        );
        assert!(
            directory
                .action_for(super_question(), ModKey::Super)
                .is_some()
        );
        let ctrl_k = KeyRef {
            key: KeyName::Char('k'),
            ctrl: true,
            alt: false,
            shift: false,
            super_held: false,
        };
        assert!(directory.action_for(ctrl_k, ModKey::Super).is_some());
    }

    #[test]
    fn suspend_parks_and_resume_restores_without_republishing() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish("help", 2, vec![binding("help", 2, "alt+?", "help:toggle")]);
        directory.suspend_owner("help");
        assert!(directory.is_suspended("help"));
        assert!(directory.action_for(alt_question(), ModKey::Alt).is_none());
        directory.resume_owner("help");
        assert!(!directory.is_suspended("help"));
        assert!(directory.action_for(alt_question(), ModKey::Alt).is_some());
        assert_eq!(directory.generation_of("help"), Some(2));
    }

    #[test]
    fn revoke_drops_the_generation() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish("help", 4, vec![binding("help", 4, "alt+?", "help:toggle")]);
        directory.revoke_owner("help");
        assert!(directory.is_empty());
        assert!(directory.action_for(alt_question(), ModKey::Alt).is_none());
    }

    #[test]
    fn republish_replaces_the_previous_generation() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish("help", 1, vec![binding("help", 1, "alt+?", "help:toggle")]);
        directory.publish("help", 2, vec![binding("help", 2, "alt+/", "help:toggle")]);
        assert_eq!(directory.generation_of("help"), Some(2));
        assert!(directory.action_for(alt_question(), ModKey::Alt).is_none());
        let alt_slash = KeyRef {
            key: KeyName::Char('/'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        assert_eq!(
            directory.action_for(alt_slash, ModKey::Alt),
            Some(ChromeAction::InvokeCommand("help:toggle".to_string()))
        );
    }

    #[test]
    fn repeated_chords_keep_the_first_registered_suggestion() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish(
            "help",
            1,
            vec![
                binding("help", 1, "alt+?", "help:toggle"),
                binding("help", 1, "alt+?", "help:other"),
            ],
        );
        assert_eq!(
            directory.action_for(alt_question(), ModKey::Alt),
            Some(ChromeAction::InvokeCommand("help:toggle".to_string()))
        );
        // The shadowed suggestion is reported, never silently dropped.
        let conflicts = directory.drain_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].chord.canonical(), "alt+?");
        assert_eq!(conflicts[0].winner, "help:toggle");
        assert_eq!(conflicts[0].loser, "help:other");
        assert!(directory.drain_conflicts().is_empty(), "drain is one-shot");
    }

    #[test]
    fn cross_owner_conflicts_resolve_in_owner_order() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish(
            "zebra",
            1,
            vec![binding("zebra", 1, "alt+?", "zebra:toggle")],
        );
        directory.publish(
            "alpha",
            1,
            vec![binding("alpha", 1, "alt+?", "alpha:toggle")],
        );
        // Deterministic lexical owner order, independent of activation order.
        assert_eq!(
            directory.action_for(alt_question(), ModKey::Alt),
            Some(ChromeAction::InvokeCommand("alpha:toggle".to_string()))
        );
        assert_eq!(directory.active().len(), 2);
        // One diagnostic for the colliding chord, naming the real winner and
        // the shadowed owner. Activation order does not decide either.
        let conflicts = directory.drain_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].chord.canonical(), "alt+?");
        assert_eq!(conflicts[0].winner, "alpha:toggle");
        assert_eq!(conflicts[0].loser, "zebra:toggle");
    }

    #[test]
    fn conflict_diagnostics_are_bounded_and_count_overflow() {
        let mut directory = SuggestedBindingDirectory::new();
        // Repeating one chord `MAX + 8` times keeps the first row and turns
        // every other row into a conflict: `MAX + 7` diagnostics, of which
        // the bounded buffer retains `MAX` and counts 7 as overflow.
        let repeater: Vec<SuggestedBinding> = (0..(MAX_KEYMAP_SUGGESTION_CONFLICTS + 8))
            .map(|index| binding("repeater", 1, "alt+?", &format!("repeater:cmd{index}")))
            .collect();
        directory.publish("repeater", 1, repeater);
        let drained = directory.drain_conflicts();
        assert_eq!(drained.len(), MAX_KEYMAP_SUGGESTION_CONFLICTS);
        assert_eq!(directory.conflicts_dropped(), 7);
    }

    #[test]
    fn stale_rows_inside_a_republished_owner_never_dispatch() {
        // The generation fence: a hand-built (or corrupted) row whose
        // generation no longer matches its owner entry can never dispatch,
        // even before the next lifecycle transition removes it.
        let mut directory = SuggestedBindingDirectory::new();
        directory.owners.insert(
            "help".to_string(),
            OwnerBindings {
                generation: 2,
                suspended: false,
                bindings: vec![binding("help", 1, "alt+?", "help:toggle")],
            },
        );
        assert_eq!(directory.action_for(alt_question(), ModKey::Alt), None);
        assert_eq!(directory.generation_of("help"), Some(2));
    }

    #[test]
    fn publish_pins_rows_to_the_publication_owner_and_generation() {
        // A caller cannot publish a row under another owner or an older
        // generation: `publish` rewrites both identity fields.
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish(
            "help",
            7,
            vec![binding("someone-else", 3, "alt+?", "help:toggle")],
        );
        let active = directory.active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].owner, "help");
        assert_eq!(active[0].generation, 7);
    }

    #[test]
    fn unbound_keys_resolve_to_nothing() {
        let mut directory = SuggestedBindingDirectory::new();
        directory.publish("help", 1, vec![binding("help", 1, "alt+?", "help:toggle")]);
        let plain = KeyRef {
            key: KeyName::Char('x'),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
        };
        assert!(directory.action_for(plain, ModKey::Alt).is_none());
    }
}
