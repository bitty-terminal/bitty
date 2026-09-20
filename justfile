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

# Run the GitHub CI 'Quality gates' job locally through act before pushing a PR.
# Builds the bitty-act image from .github/act/Dockerfile (one-time).
# Spends remote CI only on what already passed here.
#
# Performance: the repository is bind-mounted (no 38 GB target/ copy), the
# cargo registry/git caches are persisted under ../.targets/act-cache and
# seeded once from the host CARGO_HOME (no re-download of ~800 crates), and
# the build target dir lives in the same persistent cache so repeat runs are
# incremental. Cargo uses every local core.
ci-local *args:
    #!/usr/bin/env bash
    set -euo pipefail
    root="$(git rev-parse --show-toplevel)"
    cache="${BITTY_ACT_CACHE:-$root/../.targets/act-cache}"
    host_cargo="${CARGO_HOME:-$HOME/.cargo}"
    # Fixed in-image toolchain locations baked into .github/act/Dockerfile.
    # scratch-paths-exempt: container paths, not host paths.
    ctr_home=/home/ubuntu ctr_cargo=/usr/local/cargo ctr_rustup=/usr/local/rustup
    # The actionlint CI step runs a dockerized actionlint, so the job user
    # needs the host docker group as a supplementary group (act mounts the
    # docker socket itself; do not mount it again or Docker errors with a
    # duplicate mount point).
    docker_gid="$(getent group docker 2>/dev/null | cut -d: -f3 || true)"
    group_add=()
    [ -n "$docker_gid" ] && group_add=(--group-add "$docker_gid")
    if ! docker image inspect bitty-act:latest >/dev/null 2>&1; then
      echo "building bitty-act:latest from .github/act/Dockerfile (one-time)" >&2
      docker build -t bitty-act:latest "$root/.github/act" >&2
    fi
    mkdir -p "$cache/cargo-registry" "$cache/cargo-git" "$cache/target"
    if [ -z "$(ls -A "$cache/cargo-registry" 2>/dev/null)" ] && [ -d "$host_cargo/registry" ]; then
      cp -a "$host_cargo/registry/." "$cache/cargo-registry/"
    fi
    if [ -z "$(ls -A "$cache/cargo-git" 2>/dev/null)" ] && [ -d "$host_cargo/git" ]; then
      cp -a "$host_cargo/git/." "$cache/cargo-git/"
    fi
    # Bind the persistent target dir at both CARGO_TARGET_DIR and the repo's
    # own target/ so gates that read target/debug/bitty see a container-built
    # binary instead of the host's (the host glibc is newer than the image's).
    exec act -W .github/workflows/ci.yml -j quality \
      --pull=false --bind --container-architecture linux/amd64 \
      -P ubuntu-latest=bitty-act:latest \
      --container-options "-u ubuntu ${group_add[*]:-} -v $cache/cargo-registry:$ctr_cargo/registry -v $cache/cargo-git:$ctr_cargo/git -v $cache/target:/cache/target -v $cache/target:$root/target" \
      --env HOME=$ctr_home --env CARGO_HOME=$ctr_cargo --env RUSTUP_HOME=$ctr_rustup \
      --env CARGO_TARGET_DIR=/cache/target --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(nproc)}" \
      {{args}}

pty-gate:
    ./scripts/check-pty-gated-tests.sh

# Run the four M1 evidence suites on this host and print the per-platform
# table: `just m1-matrix`. The aggregated Tier 1 view lives in CI
# (`.github/workflows/ci.yml` job `m1-matrix`); see scripts/m1-matrix.sh.
m1-matrix *args:
    ./scripts/m1-matrix.sh run --platform local {{args}}

m1-matrix-test:
    ./scripts/tests/m1-matrix.test.sh

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

check: fmt-check clippy test scratch-paths scratch-paths-test pty-gate status-drift status-drift-test runtime-deps-test terminfo-check terminfo-test desktop-check desktop-test install-smoke-test rust-channel-test binary-arch-test workflow-publish-test m1-matrix-test actionlint markdownlint

# Parser-throughput baseline (CTX-0576, M1-11). Runs the deterministic
# headless parser benchmark over the committed VT/escape corpora and verifies
# the ratio gate against crates/bitty-perf/baselines/parser-throughput.json.
# The release `bench` profile is used (optimized); the same gate runs bounded
# in CI via `cargo test -p bitty-perf --test parser_throughput_regression`.
perf-parser:
    cargo bench -p bitty-perf --bench parser_throughput -- --nocapture

# Re-capture the committed parser-throughput baseline artifact. Provenance is
# taken from the environment so no host path or username enters the file:
#   BITTY_PERF_DATE, BITTY_PERF_REVISION, BITTY_PERF_TOOLCHAIN, BITTY_PERF_OS,
#   BITTY_PERF_MACHINE_CLASS (see benches/parser_throughput.rs).
# Defaults leave the artifact placeholders intact, so fill every variable
# before committing a regenerated baseline.
perf-parser-baseline out="crates/bitty-perf/baselines/parser-throughput.json":
    cargo bench -p bitty-perf --bench parser_throughput -- --nocapture --write-baseline {{out}}

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
