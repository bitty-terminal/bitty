#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // CTX-0824: Parser-to-state fuzz target with invariant checks
    // Bounded: max 16 KiB input, max 4096 actions
    const MAX_INPUT: usize = 16 * 1024;
    const MAX_ACTIONS: usize = 4096;
    
    if data.len() > MAX_INPUT {
        return;
    }
    
    // Initialize parser and state
    let mut parser = bitty_vt::Parser::new();
    let mut state = bitty_term_state::State::new();
    
    // Collect actions with bound
    let mut actions = Vec::with_capacity(256);
    parser.advance(data, |action| {
        if actions.len() < MAX_ACTIONS {
            actions.push(action);
        }
    });
    
    // Apply actions with invariant checks
    // Note: Debug builds already assert invariants after each apply()
    for action in actions {
        state.apply(&action);
    }
});
