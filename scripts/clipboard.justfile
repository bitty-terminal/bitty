# Test-only gates: just --justfile scripts/clipboard.justfile <recipe>.
set working-directory := ".."

guard-unit output:
    rustc --edition=2024 --test crates/bitty-platform/tests/clipboard_live_guard.rs -o "{{output}}"
    "{{output}}"

fmt:
    cargo fmt -p bitty-platform

test:
    cargo test -p bitty-platform --locked --test clipboard_live_guard --test clipboard_live --features gui-tests -- --nocapture

clippy:
    cargo clippy -p bitty-platform --locked --test clipboard_live_guard --test clipboard_live --features gui-tests -- -D warnings

isolated binary:
    bash ./scripts/clipboard-isolated.sh "{{binary}}"

runner-check:
    shellcheck -S warning scripts/clipboard-isolated.sh scripts/tests/clipboard-isolated.test.sh
    shfmt -d scripts/clipboard-isolated.sh scripts/tests/clipboard-isolated.test.sh
    bash scripts/tests/clipboard-isolated.test.sh
