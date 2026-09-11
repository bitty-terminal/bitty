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

check: fmt-check clippy test scratch-paths pty-gate actionlint markdownlint

# Publish a ctxpack snapshot to the bitty-workflow mirror (commander merge
# closeout only; never a git hook). Dry run exports + validates without push.
# Canonical closeout runs from the primary checkout on branch main
# (`cd "$BITTY_WORKSPACE/bitty" && just workflow-publish`) so source.json
# records repo_branch=main; detached worktrees publish but record `detached`.
# Shared parametrized template: WORKFLOW_SOURCE_REPO / WORKFLOW_MIRROR_URL /
# WORKFLOW_MIRROR_DIR override the defaults.
workflow-publish *args:
    bash scripts/publish-ctxpack.sh {{args}}

workflow-publish-dry *args:
    bash scripts/publish-ctxpack.sh --dry-run {{args}}

# Restore the local CarryCtx DB from the bitty-workflow mirror LATEST
# snapshot (fresh-clone recipe). Refuses to replace a non-empty local DB
# without --force, e.g. `just workflow-import --force`.
workflow-import *args:
    bash scripts/fetch-ctxpack.sh {{args}}

workflow-import-dry *args:
    bash scripts/fetch-ctxpack.sh --dry-run {{args}}
