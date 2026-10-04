setup:
    cargo fetch
    lefthook install
    just tools

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo clippy -p bitty-terminal --features dev-tools --all-targets --locked -- -D warnings

# Run the Rust test suite. cargo-nextest runs each test in its own process
# (isolation + per-test timeouts + the `live-pty` group cap that keeps
# real-process suites from starving each other on small runners). nextest does
# not execute doctests, cannot list the two `harness = false` platform binaries
# (winit needs the OS main thread), and does not run benches/examples, so those
# run here explicitly -- keeping coverage identical to the previous
# `cargo test --workspace --all-targets` (which executed all three).
# `bitty dev capture|synthesize|dump|overlay` live behind `dev-tools`
# (CTX-0922): the default nextest run proves the compiled-out error paths, the
# last lines rerun the CLI suite (and the `dev::` unit tests) with the feature
# so the success paths stay covered. (`bitty dev trace` measurement lives in
# the external bitty-perf suite since the W-105 relocation and is never linked
# here; its harness-side coverage runs in that repository.)
# The perf bench gate below runs the pinned external bitty-perf checkout
# (BITTY_PERF_DIR, default ../bitty-perf; see validation-pins.env).
test:
    cargo nextest run --workspace --locked
    cargo test --workspace --doc --locked
    cargo test -p bitty-platform --test headless_run --test winit_window --locked
    cargo test --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --benches --locked
    cargo test -p bitty-terminal --features dev-tools --test cli_dev --locked
    cargo test -p bitty-terminal --features dev-tools --bin bitty --locked dev::

typecheck:
    cargo check --workspace --all-targets --locked

actionlint:
    actionlint -color

# Run the GitHub CI 'Quality gates' job locally through act before pushing a PR.
# Builds the bitty-act image from .github/act/Dockerfile (one-time).
# Spends remote CI only on what already passed here.
#
# Performance: the repository is bind-mounted (no 38 GB target/ copy), the
# cargo registry/git caches are persisted under a per-branch
# ../.targets/act-cache-<branch> dir (override with BITTY_ACT_CACHE for
# explicit sharing) and seeded once from the host CARGO_HOME (no
# re-download of ~800 crates), and the build target dir lives in the same
# persistent cache so repeat runs are incremental. The per-branch default
# keeps concurrent ci-local runs on different branches/tasks isolated:
# sharing one act cache caused exit-137 SIGKILL plus cross-task path
# contamination. Cargo uses every local core.
ci-local *args:
    #!/usr/bin/env bash
    set -euo pipefail
    root="$(git rev-parse --show-toplevel)"
    # act derives job container names from the workflow and job, so two
    # ci-local runs on one Docker daemon collide even from different
    # worktrees: the second run removes the first run's container mid-job
    # (exit 137). Serialize runs per repository with a lock in the shared git
    # directory, and fail fast instead of clobbering a running job.
    if command -v flock >/dev/null 2>&1; then
      lock="$(git rev-parse --path-format=absolute --git-common-dir)/ci-local.lock"
      exec 9>"$lock"
      if ! flock -n 9; then
        echo "ci-local: another ci-local run of this repository holds $lock; wait for it to finish" >&2
        exit 1
      fi
    else
      echo "ci-local: flock not found; concurrent runs are not serialized" >&2
    fi
    branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo detached)"
    [ "$branch" = "HEAD" ] && branch="detached-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    tag="$(printf '%s' "$branch" | tr -c 'A-Za-z0-9_.-' '-' | cut -c1-64)"
    cache="${BITTY_ACT_CACHE:-$root/../.targets/act-cache-$tag}"
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
    # Job containers clone actions (checkout/cache) and fetch crates from
    # GitHub and crates.io. On a host behind an egress proxy the container's
    # direct connection dies mid-clone with a transient
    # `Get .../info/refs...: unexpected EOF`, which reads like a flaky gate.
    # Forward the host proxy when one is configured; act runs the container on
    # the host network, so a 127.0.0.1 proxy stays reachable. NO_PROXY keeps
    # loopback (the act server) off the proxy.
    proxy="${HTTPS_PROXY:-${https_proxy:-${HTTP_PROXY:-${http_proxy:-${NETWORK_PROXY:-}}}}}"
    # The job runs checked-out (possibly untrusted PR) code that can read its
    # environment, so a proxy URL carrying credentials (`scheme://user:pass@`)
    # is never forwarded; use a credential-free (for example loopback) proxy.
    case "$proxy" in
      *://*@*)
        echo "ci-local: not forwarding a proxy URL that carries credentials into the job container" >&2
        proxy=""
        ;;
    esac
    proxy_env=()
    if [ -n "$proxy" ]; then
      proxy_env=(
        --env "HTTP_PROXY=$proxy" --env "HTTPS_PROXY=$proxy"
        --env "http_proxy=$proxy" --env "https_proxy=$proxy"
        --env "ALL_PROXY=$proxy" --env "all_proxy=$proxy"
        --env "NO_PROXY=localhost,127.0.0.1,::1"
        --env "no_proxy=localhost,127.0.0.1,::1"
      )
    fi
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
      ${proxy_env[@]+"${proxy_env[@]}"} \
      {{args}}

pty-gate:
    ./scripts/check-pty-gated-tests.sh

# #1519: a `test-support` feature (hermetic entry points such as bitty-ipc's
# authority-less ServeContext constructors) must never be enabled through a
# normal or build dependency edge.
test-support-gate:
    ./scripts/check-test-support-gate.sh

# Run the M1 evidence suites on this host and print the per-platform
# table: `just m1-matrix`. The compat-lab suites run in the pinned external
# checkout (BITTY_COMPAT_LAB_DIR, default $BITTY_WORKSPACE/bitty-compat-lab
# or ../bitty-compat-lab; see validation-pins.env); the runtime suites run
# in this workspace. The aggregated Tier 1 view lives in CI
# (`.github/workflows/ci.yml` job `m1-matrix`); see scripts/m1-matrix.sh.
m1-matrix *args:
    ./scripts/m1-matrix.sh run --platform local {{args}}

m1-matrix-test:
    ./scripts/tests/m1-matrix.test.sh

# Run the 14 surfaces x 4 terminals release-matrix suites on this host and
# print the per-platform table: `just compat-matrix`. The suites run in the
# pinned external bitty-compat-lab checkout (same BITTY_COMPAT_LAB_DIR
# resolution as above). The aggregated Tier 1 view lives in CI
# (`.github/workflows/ci.yml` job `compat-matrix`); see
# scripts/compat-matrix.sh. PERF-11 (#1065), CTX-0692.
compat-matrix *args:
    ./scripts/compat-matrix.sh run --platform local {{args}}

compat-matrix-test:
    ./scripts/tests/compat-matrix.test.sh

# Vendored compat-fixture freshness gate (W-105): the external compat-lab
# suite snapshots product test/scenario names; this fails when the snapshot
# drifts from this workspace. See scripts/check-compat-fixture-freshness.sh.
compat-freshness:
    ./scripts/check-compat-fixture-freshness.sh

compat-freshness-test:
    ./scripts/tests/check-compat-fixture-freshness.test.sh

scratch-paths:
    ./scripts/check-scratch-paths.sh

scratch-paths-test:
    ./scripts/tests/check-scratch-paths.test.sh

status-drift:
    ./scripts/check-status-drift.sh

status-drift-test:
    ./scripts/tests/check-status-drift.test.sh

# Move the docs/ submodule pin to a bitty-terminal-docs commit reachable from
# its origin/main without populating the mount (CTX-0810): `just docs-pin`
# pins origin/main of the workspace checkout, `just docs-pin <rev>` a merged
# rev. Stages the gitlink only; see scripts/docs-pin.sh.
docs-pin rev="origin/main":
    ./scripts/docs-pin.sh {{ quote(rev) }}

docs-pin-test:
    ./scripts/tests/docs-pin.test.sh

runtime-deps-test:
    ./scripts/tests/check-runtime-deps.test.sh

# Compare declared package runtime deps against a binary's ldd/readelf output:
# `just runtime-deps target/debug/bitty deb,rpm,archlinux`
runtime-deps binary packagers:
    ./scripts/check-runtime-deps.sh --binary {{binary}} --packagers {{packagers}}

# Local supply-chain gate mirroring the CI `Supply chain (deny/audit)` job
# (CTX-0634, SEC-16 / R-019): `just supply-chain`
supply-chain:
    ./scripts/check-supply-chain.sh

supply-chain-test:
    ./scripts/tests/check-supply-chain.test.sh

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

unix-bundle-test:
    ./scripts/tests/make-unix-bundle.test.sh

# Exercise the release bundle verifier's per-target decision: the gnu bundle is
# executed, while musl and aarch64 are inspected but not run. The musl case is
# the regression — a dynamically linked musl binary exits 127 on a glibc runner
# even when the archive is valid, because the musl loader is absent.
verify-unix-bundle-dispatch-test:
    ./scripts/tests/verify-unix-bundle-dispatch.test.sh

# Exercise the DMG hdiutil retry loop and the Universal 2 guard on any host
# (stubs lipo/hdiutil), so a transient macOS-runner `Resource busy` cannot
# silently become a red release again.
macos-dmg-test:
    ./scripts/tests/make-macos-dmg.test.sh

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

check: fmt-check clippy test supply-chain supply-chain-test scratch-paths scratch-paths-test pty-gate test-support-gate status-drift status-drift-test docs-pin-test runtime-deps-test terminfo-check terminfo-test desktop-check desktop-test install-smoke-test unix-bundle-test macos-dmg-test verify-unix-bundle-dispatch-test rust-channel-test binary-arch-test workflow-publish-test m1-matrix-test compat-matrix-test compat-freshness-test real-render-soak-test dogfood-session-test actionlint markdownlint

# Parser-throughput baseline (CTX-0576, M1-11). Runs the deterministic
# headless parser benchmark over the committed VT/escape corpora and verifies
# the ratio gate against the pinned bitty-perf checkout's
# crates/bitty-perf/baselines/parser-throughput.json (W-105 relocation).
# The release `bench` profile is used (optimized); the same gate runs bounded
# in CI via the external parser-throughput regression test.
# BITTY_PERF_DIR locates the checkout (default ../bitty-perf).
perf-parser:
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench parser_throughput -- --nocapture

# Re-capture the committed parser-throughput baseline artifact. Provenance is
# taken from the environment so no host path or username enters the file:
#   BITTY_PERF_DATE, BITTY_PERF_REVISION, BITTY_PERF_TOOLCHAIN, BITTY_PERF_OS,
#   BITTY_PERF_MACHINE_CLASS (see benches/parser_throughput.rs in the bitty-perf checkout).
# Defaults leave the artifact placeholders intact, so fill every variable
# before committing a regenerated baseline. Writes into the bitty-perf
# checkout: baseline promotion stays an owned, reviewed change there.
perf-parser-baseline out="crates/bitty-perf/baselines/parser-throughput.json":
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench parser_throughput -- --nocapture --write-baseline "${BITTY_PERF_DIR:-../bitty-perf}/{{out}}"

# Real-window PB-1 startup + PB-2 idle-memory evidence (CTX-0592). Opt-in: a
# run requires BITTY_PERF_REAL_WINDOW=1 and a built `bitty` binary (release
# preferred). On headless CI the bench reports UNMEASURED and exits 0, so it
# never fabricates numbers. Bounds: BITTY_PERF_STARTUP_SAMPLES (<=50),
# BITTY_PERF_IDLE_SECS (<=300), BITTY_PERF_STARTUP_TIMEOUT_SECS (<=120),
# BITTY_PERF_BIN (explicit binary path). Runbook:
# bitty-perf checkout: crates/bitty-perf/baselines/real-window-evidence.md.
perf-real-window:
    BITTY_PERF_REAL_WINDOW=1 cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench real_window -- --nocapture

# Regenerate the committed real-window evidence artifact. Provenance comes
# from the environment so no host path or username enters the file:
#   BITTY_PERF_DATE, BITTY_PERF_REVISION, BITTY_PERF_TOOLCHAIN,
#   BITTY_PERF_COMMAND, BITTY_PERF_PROFILE (see benches/real_window.rs in the bitty-perf checkout).
# Both PB-1 and PB-2 must be measured or the bench refuses to write (exit 2).
perf-real-window-baseline out="crates/bitty-perf/baselines/pb-real-window.json":
    BITTY_PERF_REAL_WINDOW=1 cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench real_window -- --nocapture --write-baseline "${BITTY_PERF_DIR:-../bitty-perf}/{{out}}"

# PB-7 idle CPU/wakeup evidence (CTX-0636, PERF-08). Fast path: the
# frame-on-demand invariant plus cost means (no extended window). Extended
# path: `--idle-window` parks a proven-idle Runtime child and samples its
# /proc CPU and wakeup counters (Linux-only; Unmeasured elsewhere).
# Runbook (bitty-perf checkout): crates/bitty-perf/baselines/idle-evidence.md.
perf-idle:
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench idle_real -- --nocapture

# Regenerate the committed PB-7 idle evidence artifact. Provenance comes
# from the environment so no host path or username enters the file:
#   BITTY_PERF_TASK, BITTY_PERF_DATE, BITTY_PERF_REVISION,
#   BITTY_PERF_TOOLCHAIN, BITTY_PERF_COMMAND, BITTY_PERF_PROFILE
# (see benches/idle_real.rs in the bitty-perf checkout). The window comes from BITTY_PERF_IDLE_SECS
# (default 60, max 600 = the PB-7 10-minute acceptance window); frame-on-demand must pass and the window must be
# measured or the bench refuses to write (exit 2).
perf-idle-baseline out="crates/bitty-perf/baselines/pb-idle.json":
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench idle_real -- --nocapture --write-baseline "${BITTY_PERF_DIR:-../bitty-perf}/{{out}}"
# Long-duration real-render soak planner (CTX-0642, PERF-09). Headless-safe:
# loads the clamped soak config, prints the bounded capture plan and the
# hyprctl+grim leg status, and reports UNMEASURED without
# BITTY_PERF_REAL_SOAK=1. Bounds: BITTY_PERF_SOAK_DURATION_SECS (60..86400),
# BITTY_PERF_SOAK_INTERVAL_SECS (30..3600), BITTY_PERF_SOAK_WORKSPACE (1..10),
# BITTY_PERF_SOAK_WORKLOAD (idle|mixed|input-spam). Runbook:
# bitty-perf checkout: crates/bitty-perf/baselines/real-soak-evidence.md.
perf-real-soak *args:
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench real_soak -- --nocapture {{args}}

# Full automated soak chain (Tier 1: Hyprland + hyprctl/grim/jq required).
# Evidence lands in timestamped run dirs under {{out}} (gitignored scratch):
# `BITTY_PERF_REAL_SOAK=1 just perf-real-soak-run recording/real-soak`.
perf-real-soak-run out *args:
    BITTY_PERF_REAL_SOAK=1 bash scripts/real-render-soak.sh --out-dir {{out}} {{args}}

real-render-soak-test:
    bash scripts/tests/real-render-soak.test.sh

# Daily-driver dogfood session planner (CTX-0643, PERF-10). Headless-safe:
# loads the clamped session config, prints the bounded cycle plan and the
# hyprctl+grim leg status, and reports UNMEASURED without
# BITTY_PERF_DOGFOOD_SESSION=1. Bounds: BITTY_PERF_SESSION_DURATION_SECS
# (60..86400), BITTY_PERF_SESSION_CYCLE_SECS (60..3600),
# BITTY_PERF_SESSION_WORKSPACE (1..10), BITTY_PERF_SESSION_APPS
# (csv subset of shell,cargo,git,nvim,tmux,ssh). Runbook:
# bitty-perf checkout: crates/bitty-perf/baselines/dogfood-session-evidence.md.
perf-dogfood-session *args:
    cargo bench --manifest-path "${BITTY_PERF_DIR:-../bitty-perf}/Cargo.toml" --bench dogfood_session -- --nocapture {{args}}

# Full dogfood session chain (Tier 1: Hyprland + hyprctl/grim/jq required).
# Evidence lands in timestamped run dirs under {{out}} (gitignored scratch):
# `BITTY_PERF_DOGFOOD_SESSION=1 just perf-dogfood-session-run recording/dogfood-session`.
perf-dogfood-session-run out *args:
    BITTY_PERF_DOGFOOD_SESSION=1 bash scripts/dogfood-session.sh --out-dir {{out}} {{args}}

dogfood-session-test:
    bash scripts/tests/dogfood-session.test.sh

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
