#![forbid(unsafe_code)]
//! CTX-0757 (bitty#1360): kitty clipboard extension (OSC 5522) is a
//! permanent non-goal for M1/v0.1.0, locked by this regression test.
//!
//! Rationale: a typed implementation would need a new `TerminalAction`
//! variant (which requires an RFC revision first), terminal-state and
//! runtime reply plumbing, and a clipboard security review for a stateful
//! multi-packet binary-MIME protocol with session reassembly and password
//! grants — a strictly larger privileged surface than OSC 52 while P0
//! clipboard controls stay deny-by-default. OSC 52 gated write already
//! covers M1 plain-text needs; Ghostty parity is not an M1 requirement.
//!
//! Locked behavior: the checked-in corpus
//! (`tests/compat/osc/corpus/04-kitty-clipboard-5522.bin`) parses to
//! bounded inert `OscUnknown { id: 5522 }` actions only, deterministically,
//! and every `ClipboardState` policy answers them with `Ignored` — no
//! capture, no denial counters, no grant consumed. Revisit only through a
//! future RFC with security review.
//!
//! Referenced as `ci` negative evidence by the compat-lab report row for
//! `kitty clipboard extension (OSC 5522)`. Headless, bounded, no window,
//! no GPU, no network.

use std::path::PathBuf;

use bitty_rich::clipboard::{
    ClipboardGrantScope, ClipboardOutcome, ClipboardPolicy, ClipboardState,
};
use bitty_vt::{BoundedBytes, ClipboardOp, TerminalAction};

/// OSC code of the kitty clipboard extension.
const KITTY_CLIPBOARD_OSC: u32 = 5522;

fn corpus_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/compat")
        .join("osc")
        .join("corpus")
        .join("04-kitty-clipboard-5522.bin");
    std::fs::read(&path).unwrap_or_else(|err| panic!("read {path:?}: {err}"))
}

fn parse(bytes: &[u8]) -> Vec<TerminalAction> {
    let mut parser = bitty_vt::Parser::new();
    let mut actions = Vec::new();
    parser.advance(bytes, |action| actions.push(action));
    actions
}

#[test]
fn kitty_5522_locked_as_inert_nongoal() {
    let bytes = corpus_bytes();
    assert!(!bytes.is_empty(), "5522 lock corpus must not be empty");
    assert!(
        bytes.len() <= 8 * 1024,
        "5522 lock corpus {} bytes exceeds headless bound",
        bytes.len()
    );

    // Deterministic: two fresh parses agree action-for-action.
    let first = parse(&bytes);
    let second = parse(&bytes);
    assert_eq!(first, second, "5522 replay diverged between identical runs");

    // Shape lock: exactly the two recorded packets, both bounded inert
    // unknown OSC 5522, nothing else (no clipboard action, no reply).
    let expected = [b"type=read".as_slice(), b"type=write;SGVsbG8=".as_slice()];
    assert_eq!(
        first.len(),
        expected.len(),
        "5522 lock corpus must replay to exactly two actions: {first:?}"
    );
    for (action, want) in first.iter().zip(expected.iter()) {
        match action {
            TerminalAction::OscUnknown { id, data } => {
                assert_eq!(*id, KITTY_CLIPBOARD_OSC, "expected OSC 5522, saw {id}");
                assert_eq!(
                    data.as_bytes(),
                    *want,
                    "OSC 5522 payload drifted: {:?}",
                    String::from_utf8_lossy(data.as_bytes())
                );
                assert!(
                    data.len() <= BoundedBytes::MAX_LEN,
                    "OSC 5522 payload exceeds bound"
                );
            }
            other => panic!("OSC 5522 must stay inert unknown, saw {other:?}"),
        }
    }

    // Policy neutrality: every policy ignores both actions with zero
    // observable effect — no capture, no counters, no grant touched.
    for policy in [
        ClipboardPolicy::Gated,
        ClipboardPolicy::Denied,
        ClipboardPolicy::Allow,
    ] {
        let mut state = ClipboardState::with_policy(policy);
        let scope = ClipboardGrantScope(7);
        let token = state
            .grant_read(scope)
            .expect("OS entropy available in tests");
        assert_eq!(state.outstanding_grants(), 1);
        for action in &first {
            assert_eq!(
                state.handle_action(action),
                ClipboardOutcome::Ignored,
                "OSC 5522 must be ignored under {policy:?}"
            );
        }
        assert!(state.is_empty(), "OSC 5522 retained state under {policy:?}");
        assert_eq!(state.captured_writes(), 0);
        assert_eq!(state.denied_writes(), 0);
        assert_eq!(state.denied_reads(), 0);
        assert_eq!(
            state.outstanding_grants(),
            1,
            "OSC 5522 consumed a read grant under {policy:?}"
        );
        // The grant is still live: redeeming it with a real OSC 52 read
        // proves the 5522 traffic neither seeded nor spent anything.
        let read = TerminalAction::OscClipboard {
            op: ClipboardOp::Read,
            data: BoundedBytes::new(b"?".to_vec()),
        };
        match state.handle_action_with_token(&read, Some(&token), scope) {
            ClipboardOutcome::ReadGranted { data } => {
                assert!(data.is_empty(), "grant read exposed seeded data");
            }
            other => panic!("live grant must redeem after 5522 traffic: {other:?}"),
        }
        assert_eq!(state.outstanding_grants(), 0);
    }
}
