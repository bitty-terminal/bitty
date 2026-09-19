# Local act runner image for bitty CI pre-validation.
#
# Extends the standard catthehacker act image with a pre-installed Rust
# toolchain (pinned channel, rustfmt + clippy) so `act` can actually execute
# the CI workflow jobs (Quality gates, MSRV, platform legs) locally instead of
# failing on `rustup: command not found`.
#
# Build:
#   docker build -t bitty-act:latest .github/act
#
# Usage (dry-run syntax validation):
#   act -n -W .github/workflows/ci.yml
#
# Usage (real local CI, e.g. the Quality gates job):
#   act -W .github/workflows/ci.yml -j quality \
#     --container-architecture linux/amd64 \
#     -P ubuntu-latest=bitty-act:latest
#
# Keep the Rust channel in sync with rust-toolchain.toml when it changes.
FROM catthehacker/ubuntu:act-latest

ARG RUST_CHANNEL=1.98.1
ARG MSRV_CHANNEL=1.85

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTUP_PERMIT_COPY_RENAME=1

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "${RUST_CHANNEL}" \
    && rustup toolchain install "${MSRV_CHANNEL}" --profile minimal --component rustfmt --component clippy \
    && rustup component add rustfmt clippy \
    && rustup target add x86_64-pc-windows-gnu \
    && rustc --version && cargo --version

# GitHub-hosted runners execute jobs as a non-root user; some tests (e.g.
# chmod-000 unreadable-file checks) depend on that. The catthehacker image
# already ships `ubuntu` (uid/gid 1000), which matches the host user so
# bind-mounted repository files keep their ownership. Grant it passwordless
# sudo and make the toolchain installs writable for it.
RUN echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu \
    && chmod 755 /etc/sudoers.d/ubuntu \
    && chown -R ubuntu:ubuntu /usr/local/cargo /usr/local/rustup
