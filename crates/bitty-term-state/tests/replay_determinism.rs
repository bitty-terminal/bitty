//! Replay determinism (RFC "Deterministic replay guarantees").
//!
//! Applying the same action sequence to two fresh states must produce
//! byte-identical damage streams and identical state hashes — the
//! machine-checkable form of "same input conditions, identical result".

#![forbid(unsafe_code)]

mod common;

use proptest::prelude::*;

use bitty_term_state::State;
use bitty_vt::{Count, Direction, GraphemeCell, Mode, Row, StatusKind, TerminalAction};

/// Applies `actions` twice from scratch; returns both damage streams as
/// byte vectors plus both final hashes.
fn replay_twice(actions: &[TerminalAction]) -> (Vec<u8>, Vec<u8>, u64, u64) {
    let mut first = State::new();
    let mut second = State::new();
    let mut stream_a = Vec::new();
    let mut stream_b = Vec::new();
    for action in actions {
        let a = first.apply(action);
        let b = second.apply(action);
        assert_eq!(a.generation, b.generation);
        stream_a.extend_from_slice(&common::damage_bytes(&a));
        stream_b.extend_from_slice(&common::damage_bytes(&b));
    }
    let hash_a = first.state_hash();
    let hash_b = second.state_hash();
    (stream_a, stream_b, hash_a, hash_b)
}

#[test]
fn fixed_scenarios_replay_byte_identically() {
    fn prints(text: &str) -> Vec<TerminalAction> {
        text.chars()
            .map(|c| TerminalAction::Print(GraphemeCell::from(c)))
            .collect()
    }

    let mut scenario: Vec<TerminalAction> = vec![
        // Title + styled banner across two lines with wide glyphs.
        TerminalAction::OscTitle {
            text: bitty_vt::BoundedString::new("replay"),
        },
        TerminalAction::SetAttributes {
            attrs: bitty_vt::AttributeDiff {
                changes: vec![
                    bitty_vt::AttributeChange::Enable(bitty_vt::Attribute::Bold),
                    bitty_vt::AttributeChange::Foreground(bitty_vt::Color::Indexed(4)),
                ]
                .into_boxed_slice(),
            },
        },
    ];
    scenario.extend(prints("bitty 中 terminal"));
    scenario.push(TerminalAction::PrintControl(bitty_vt::ControlChar(0x0D)));
    scenario.push(TerminalAction::PrintControl(bitty_vt::ControlChar(0x0A)));
    scenario.extend(prints("second line"));
    scenario.extend([
        // Scroll enough times to push lines into scrollback.
        TerminalAction::SetScrollRegion {
            top: Row(1),
            bottom: Row::SENTINEL,
        },
        TerminalAction::ScrollUp { n: Count(3) },
        TerminalAction::PrintControl(bitty_vt::ControlChar(0x0A)),
        TerminalAction::ScrollDown { n: Count(1) },
        // Modes, tabs, charsets, hyperlink, zones, replies.
        TerminalAction::SetMode {
            mode: Mode::AlternateScreenClearAndRestore,
            enabled: true,
        },
    ]);
    scenario.extend(prints("alt"));
    scenario.extend([
        TerminalAction::TabSet,
        TerminalAction::SelectCharset {
            slot: bitty_vt::CharsetSlot::G0,
            table: bitty_vt::CharsetTable::DecSpecialGraphics,
        },
        TerminalAction::RequestDeviceStatus {
            kind: StatusKind::CursorPosition,
        },
        TerminalAction::OscHyperlink {
            link: Some(bitty_vt::Hyperlink {
                id: None,
                uri: bitty_vt::BoundedString::new("https://bitty.dev"),
            }),
        },
        TerminalAction::OscPromptMark {
            kind: bitty_vt::ZoneKind::OutputStart,
            exit_code: None,
        },
        TerminalAction::SetMode {
            mode: Mode::AlternateScreenClearAndRestore,
            enabled: false,
        },
        TerminalAction::EraseInDisplay {
            mode: bitty_vt::EraseDisplayMode::Below,
        },
        TerminalAction::SoftReset,
    ]);
    scenario.extend(std::iter::repeat_n(
        TerminalAction::CursorMove {
            dir: Direction::Right,
            n: Count(2),
        },
        7,
    ));

    let (stream_a, stream_b, hash_a, hash_b) = replay_twice(&scenario);
    assert_eq!(stream_a, stream_b, "damage streams diverged");
    assert_eq!(hash_a, hash_b, "state hashes diverged");
}

#[test]
fn inert_actions_leave_hash_unchanged() {
    // (action, expected unmapped-sequence telemetry delta). `OscClipboard`
    // is policy-inert rather than unmapped, so it counts nothing (RFC
    // replay guarantee 6: effects flow through recorded Env outcomes).
    let inert: Vec<(TerminalAction, u64)> = vec![
        (
            TerminalAction::Unknown(bitty_vt::UnrecognizedSequence {
                kind: bitty_vt::SequenceKind::Csi,
                final_byte: b'z',
                intermediates: *b">9",
            }),
            1,
        ),
        (
            TerminalAction::Unknown(bitty_vt::UnrecognizedSequence {
                kind: bitty_vt::SequenceKind::Dcs,
                final_byte: b'q',
                intermediates: [0, 0],
            }),
            1,
        ),
        (
            TerminalAction::OscUnknown {
                id: 1337,
                data: bitty_vt::BoundedBytes::new([0u8, 1, 2]),
            },
            1,
        ),
        (
            TerminalAction::OscClipboard {
                op: bitty_vt::ClipboardOp::Write,
                data: bitty_vt::BoundedBytes::new("payload"),
            },
            0,
        ),
    ];
    for (action, telemetry_delta) in inert {
        let mut state = State::new();
        state.apply(&TerminalAction::Print(GraphemeCell::from('x')));
        let hash_before = state.state_hash();
        let snapshot_before = state.snapshot();
        let counters_before = state.telemetry();
        let damage = state.apply(&action);
        assert_eq!(
            hash_before,
            state.state_hash(),
            "inert action {action:?} mutated state"
        );
        assert_eq!(snapshot_before.cells, state.snapshot().cells);
        assert!(damage.regions.is_empty(), "inert action damaged output");
        let counted = state.telemetry().unknown_csi
            + state.telemetry().unknown_dcs
            + state.telemetry().unknown_osc
            - (counters_before.unknown_csi
                + counters_before.unknown_dcs
                + counters_before.unknown_osc);
        assert_eq!(
            counted, telemetry_delta,
            "telemetry mismatch for {action:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn random_sequences_replay_with_identical_streams_and_hashes(
        actions in prop::collection::vec(common::arb_action(), 0..120),
    ) {
        let (stream_a, stream_b, hash_a, hash_b) = replay_twice(&actions);
        prop_assert_eq!(stream_a, stream_b);
        prop_assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn damage_since_reconstructs_the_full_stream(
        actions in prop::collection::vec(common::arb_action(), 1..40),
    ) {
        // Within the retained-history window, damage_since(0) must
        // reproduce exactly the concatenation of the per-batch streams the
        // caller already received.
        use bitty_term_state::DamagedRegion;
        let mut state = State::new();
        let mut observed: Vec<DamagedRegion> = Vec::new();
        for action in &actions {
            let damage = state.apply(action);
            observed.extend_from_slice(&damage.regions);
        }
        prop_assert!(actions.len() < bitty_term_state::damage::DAMAGE_HISTORY_BATCHES);
        prop_assert_eq!(observed, state.damage_since(0));
    }
}
