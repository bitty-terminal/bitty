//! Per-recipient event payload redaction (CTX-0899).
//!
//! Some event payloads carry fields whose visibility depends on the
//! recipient's grants (`intercept.paste` text requires `clipboard.read`).
//! The knowledge of which fields are gated lives in exactly one place,
//! [`EventKind::payload_policy`] in `bitty-plugin-host`; this module applies
//! it to the bridge-level [`LuaValue`] payload for one recipient.
//!
//! Every recipient-facing copy goes through [`redact_payload_for`] (or the
//! equivalent [`recipient_view`] + [`apply_view`] pair): the Lua subscriber
//! fan-out in [`PluginRuntime::deliver_event`](super::PluginRuntime::deliver_event)
//! and the `debug.trace` recording in [`TraceHub`](super::debug::TraceHub).
//! A `debug.trace` owner therefore never observes more than a subscriber
//! holding the same grants.
//!
//! Fail closed: a kind that `EventKind::parse` does not know has no reviewed
//! policy, so its payload is withheld entirely and replaced by the
//! [`REDACTED_KEY`] marker table. Known kinds are covered by an exhaustive
//! match, so a new kind cannot silently default to "ungated".
//!
//! CTX-0926 (W-100 first slice, W-71 observability boundary): this module is
//! retained Core mechanism — the redaction rules stay in Core under `W-71`
//! and never move to `bitty-observability` with the debug/trace
//! implementation. This slice changes no behavior.

use std::borrow::Cow;

use bitty_lua::LuaValue;
use bitty_plugin_host::{EventKind, PayloadPolicy};

/// Marker key set to `true` on every payload that lost fields to redaction.
pub const REDACTED_KEY: &str = "redacted";

/// What one recipient may see of one event kind's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientView {
    /// The payload unchanged (ungated kind, or the gating grant is held).
    Full,
    /// Only these top-level fields, plus `redacted = true`.
    Public(&'static [&'static str]),
    /// Nothing but the `{ redacted = true }` marker (unknown kind).
    Withheld,
}

/// Decide the view of `kind` for a recipient whose grants answer
/// `has_grant(capability_id)`.
///
/// O(1) policy lookup plus at most one `has_grant` call.
#[must_use]
pub fn recipient_view(kind: &str, has_grant: impl Fn(&str) -> bool) -> RecipientView {
    let Ok(kind) = EventKind::parse(kind) else {
        return RecipientView::Withheld;
    };
    match kind.payload_policy() {
        PayloadPolicy::Ungated => RecipientView::Full,
        PayloadPolicy::Gated {
            capability,
            public_fields,
        } => {
            if has_grant(capability) {
                RecipientView::Full
            } else {
                RecipientView::Public(public_fields)
            }
        }
    }
}

fn marker() -> (LuaValue, LuaValue) {
    (
        LuaValue::String(REDACTED_KEY.to_string()),
        LuaValue::Bool(true),
    )
}

/// Apply a [`RecipientView`] to `payload`.
///
/// `Full` borrows; the other views allocate a fresh table. A `Public` view
/// over a non-table payload keeps no fields (there are no named fields to
/// allowlist). O(n · f) for n payload pairs and f public fields.
#[must_use]
pub fn apply_view(view: RecipientView, payload: &LuaValue) -> Cow<'_, LuaValue> {
    match view {
        RecipientView::Full => Cow::Borrowed(payload),
        RecipientView::Withheld => Cow::Owned(LuaValue::Table(vec![marker()])),
        RecipientView::Public(fields) => {
            let mut kept: Vec<(LuaValue, LuaValue)> = match payload {
                LuaValue::Table(pairs) => pairs
                    .iter()
                    .filter(|(key, _)| {
                        matches!(key, LuaValue::String(name)
                            if name != REDACTED_KEY && fields.contains(&name.as_str()))
                    })
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            };
            kept.push(marker());
            Cow::Owned(LuaValue::Table(kept))
        }
    }
}

/// Redact `payload` of event `kind` for a recipient whose grants answer
/// `has_grant(capability_id)`.
#[must_use]
pub fn redact_payload_for<'a>(
    kind: &str,
    payload: &'a LuaValue,
    has_grant: impl Fn(&str) -> bool,
) -> Cow<'a, LuaValue> {
    apply_view(recipient_view(kind, has_grant), payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paste() -> LuaValue {
        LuaValue::table([
            ("action", LuaValue::String("paste".into())),
            ("origin", LuaValue::String("user".into())),
            ("preview", LuaValue::String("hunter2".into())),
        ])
    }

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn paste_without_clipboard_read_keeps_only_classification() {
        let payload = paste();
        let redacted = redact_payload_for("intercept.paste", &payload, none);
        assert_eq!(
            redacted.get("action"),
            Some(&LuaValue::String("paste".into()))
        );
        assert_eq!(
            redacted.get("origin"),
            Some(&LuaValue::String("user".into()))
        );
        assert_eq!(redacted.get("preview"), None);
        assert_eq!(redacted.get(REDACTED_KEY), Some(&LuaValue::Bool(true)));
        assert!(!format!("{redacted:?}").contains("hunter2"));
    }

    #[test]
    fn paste_with_clipboard_read_is_unchanged() {
        let payload = paste();
        let full = redact_payload_for("intercept.paste", &payload, |c| c == "clipboard.read");
        assert!(matches!(full, Cow::Borrowed(_)));
        assert_eq!(*full, payload);
        // An unrelated grant does not unlock the payload.
        let other = redact_payload_for("intercept.paste", &payload, |c| c == "clipboard.write");
        assert_eq!(other.get("preview"), None);
    }

    #[test]
    fn ungated_kinds_pass_through_without_grants() {
        let payload = LuaValue::table([("title", LuaValue::String("t".into()))]);
        for kind in [
            "terminal.title-changed",
            "terminal.opened",
            "intercept.open-url",
        ] {
            let view = redact_payload_for(kind, &payload, none);
            assert!(matches!(view, Cow::Borrowed(_)), "{kind}");
        }
    }

    #[test]
    fn unknown_kinds_fail_closed() {
        let payload = paste();
        for kind in ["t", "intercept.paste2", "", "clipboard.changed"] {
            let view = redact_payload_for(kind, &payload, |_| true);
            assert_eq!(
                *view,
                LuaValue::Table(vec![marker()]),
                "{kind} must be withheld even with every grant"
            );
        }
    }

    #[test]
    fn public_view_drops_non_table_payloads_and_spoofed_markers() {
        let text = LuaValue::String("hunter2".into());
        let view = redact_payload_for("intercept.paste", &text, none);
        assert_eq!(*view, LuaValue::Table(vec![marker()]));
        // A producer-supplied `redacted` key never survives; the host sets it.
        let spoof = LuaValue::table([
            ("action", LuaValue::String("paste".into())),
            (REDACTED_KEY, LuaValue::Bool(false)),
            ("extra", LuaValue::String("x".into())),
        ]);
        let view = redact_payload_for("intercept.paste", &spoof, none);
        assert_eq!(
            *view,
            LuaValue::Table(vec![
                (
                    LuaValue::String("action".into()),
                    LuaValue::String("paste".into())
                ),
                marker(),
            ])
        );
    }

    #[test]
    fn every_known_kind_has_an_explicit_policy() {
        // Parse round-trip over the closed set: each known kind yields a
        // non-`Withheld` view, so `Withheld` means "unknown" only.
        for kind in [
            "plugin.activated",
            "plugin.suspended",
            "plugin.disposed",
            "handler.violation",
            "terminal.opened",
            "terminal.closed",
            "terminal.title-changed",
            "terminal.cwd-changed",
            "terminal.bell",
            "focus.changed",
            "selection.changed",
            "process.exited",
            "config.reloaded",
            "intercept.command-dispatch",
            "intercept.terminal-spawn",
            "intercept.paste",
            "intercept.open-url",
        ] {
            assert_ne!(
                recipient_view(kind, none),
                RecipientView::Withheld,
                "{kind}"
            );
        }
        assert_eq!(
            recipient_view("intercept.paste", none),
            RecipientView::Public(&["action", "origin"])
        );
    }
}
