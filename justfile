setup:
    cargo fetch
    lefthook install
    just tools

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --all-targets --locked

typecheck:
    cargo check --workspace --all-targets --locked

actionlint:
    actionlint -color

pty-gate:
    ./scripts/check-pty-gated-tests.sh

scratch-paths:
    ./scripts/check-scratch-paths.sh

scratch-paths-test:
    ./scripts/tests/check-scratch-paths.test.sh

status-drift:
    ./scripts/check-status-drift.sh

status-drift-test:
    ./scripts/tests/check-status-drift.test.sh

runtime-deps-test:
    ./scripts/tests/check-runtime-deps.test.sh

# Compare declared package runtime deps against a binary's ldd/readelf output:
# `just runtime-deps target/debug/bitty deb,rpm,archlinux`
runtime-deps binary packagers:
    ./scripts/check-runtime-deps.sh --binary {{binary}} --packagers {{packagers}}

# Print the Rust channel pinned in rust-toolchain.toml (single source for CI):
# `just rust-channel`
rust-channel:
    ./scripts/rust-channel.sh

rust-channel-test:
    ./scripts/tests/rust-channel.test.sh

# Assert a built binary carries the expected ELF architecture:
# `just binary-arch target/release/bitty aarch64`
binary-arch binary arch:
    ./scripts/check-binary-arch.sh --binary {{binary}} --arch {{arch}}

binary-arch-test:
    ./scripts/tests/check-binary-arch.test.sh

terminfo-check:
    ./scripts/check-terminfo.sh

terminfo-test:
    ./scripts/tests/check-terminfo.test.sh

desktop-check:
    ./scripts/check-desktop-integration.sh

desktop-test:
    ./scripts/tests/check-desktop-integration.test.sh

install-smoke-test:
    ./scripts/tests/install-smoke.test.sh

# Install a package in a clean container and run the version/doctor/headless
# smoke: `just install-smoke ubuntu dist/bitty-x86_64-unknown-linux-gnu.deb`
install-smoke distro package:
    ./scripts/install-smoke.sh --distro {{distro}} --package {{package}}

workflow-publish-test:
    ./scripts/tests/workflow-publish.test.sh

markdownlint *args:
    bunx --bun markdownlint-cli2@0.23.1 {{args}}

tools:
    #!/usr/bin/env bash
    set -euo pipefail
    pins="commitlint@21.2.2 @commitlint/config-conventional@21.2.2"
    dir="target/dev-tools"
    stamp="$dir/node_modules/.pins"
    if [[ "$(cat "$stamp" 2>/dev/null)" == "$pins" ]]; then
        exit 0
    fi
    mkdir -p "$dir"
    cd "$dir"
    if [[ ! -f package.json ]]; then
        printf '{"name":"bitty-dev-tools","private":true}\n' > package.json
    fi
    bun add $pins
    printf '%s\n' "$pins" > node_modules/.pins

commit-check message:
    @just tools
    @cp commitlint.config.ts target/dev-tools/commitlint.config.ts
    @msg="$(realpath "{{message}}")" && cd target/dev-tools && bunx --bun commitlint --edit "$msg"

check: fmt-check clippy test scratch-paths scratch-paths-test pty-gate status-drift status-drift-test runtime-deps-test terminfo-check terminfo-test desktop-check desktop-test install-smoke-test rust-channel-test binary-arch-test workflow-publish-test actionlint markdownlint

# Publish a redacted CarryCtx snapshot inside this repo (commander merge
# closeout only; never a git hook). `carryctx export --publication` redacts the
# bundle, stamps manifest.redacted, and commits one snapshot to the fixed ref
# `refs/heads/carryctx-snapshots`; the target pushes that branch and fails
# loudly when the local ref does not advance (native carryctx commits one
# snapshot per export, so a re-run publishes again rather than no-opping; a
# ref that did not advance means a stale source or a broken export).
# Canonical closeout runs through the workspace publish-snapshots helper from
# a fresh origin/main worktree whose basename matches the repository name
# (see scripts/tests/workflow-publish.test.sh); a manual run from the primary
# checkout on main is allowed only when that checkout is current with
# origin/main. Dry run validates the export and writes neither the ref nor
# the remote.
workflow-publish *args:
    bash scripts/workflow-publish.sh {{args}}

workflow-publish-dry *args:
    bash scripts/workflow-publish.sh --dry-run {{args}}

# Restore the local CarryCtx DB from the in-repo snapshot branch
# `refs/heads/carryctx-snapshots` (fresh-clone recipe). Refuses to replace a
# non-empty local DB without --force, e.g. `just workflow-import --force`.
workflow-import *args:
    bash scripts/workflow-import.sh {{args}}

workflow-import-dry *args:
    bash scripts/workflow-import.sh --dry-run {{args}}
