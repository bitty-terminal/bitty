#!/usr/bin/env bash
# check-release-channels.test.sh — static regression guards for the 0.0.27
# release-channel repairs.
#
# Asserts:
#   1. release.yml and repair-release-channels.yml build an SRPM with
#      scripts/build-copr-srpm.sh and submit it with `copr-cli build`; direct
#      `copr-cli uploadrpm` is forbidden. Both install rpm/zstd and use a
#      Python venv for copr-cli.
#   2. release.yml imports the APT secret through scripts/import-apt-gpg-key.sh
#      and verifies the pinned archive fingerprint instead of piping the raw
#      secret into gpg.
#   3. repair-release-channels.yml is a manual, tag-input workflow that checks
#      out the requested tag, verifies published checksums, imports the APT key
#      robustly, and never mutates tags or release assets.
set -euo pipefail

cd "$(dirname "$0")/../.."

RELEASE=".github/workflows/release.yml"
REPAIR=".github/workflows/repair-release-channels.yml"
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

expect_contains() { # <file> <literal> <label>
  if grep -qF -- "$2" "$1"; then
    pass "$3"
  else
    fail "$3 (missing: '$2')"
  fi
}

expect_absent() { # <file> <literal> <label>
  if grep -qF -- "$2" "$1"; then
    fail "$3 (unexpected: '$2')"
  else
    pass "$3"
  fi
}

[[ -f "$REPAIR" ]] || fail "repair workflow missing: $REPAIR"

# --- 0. Pinned archive key fingerprint -------------------------------------
IMPORTER="scripts/import-apt-gpg-key.sh"
KEYRING="packaging/apt/bitty-archive-keyring.gpg"
KEYRING_FP="$(
  gpg --batch --with-colons --show-keys "$KEYRING" 2>/dev/null |
    awk -F: '$1 == "fpr" { print $10; exit }'
)"
if [[ -n "$KEYRING_FP" ]]; then
  expect_contains "$IMPORTER" "PINNED_FINGERPRINT=\"$KEYRING_FP\"" \
    "importer pins the committed keyring fingerprint"
  expect_contains "$RELEASE" "APT_GPG_KEY_FINGERPRINT: $KEYRING_FP" \
    "release.yml pins the committed keyring fingerprint"
  expect_contains "$REPAIR" "APT_GPG_KEY_FINGERPRINT: $KEYRING_FP" \
    "repair workflow pins the committed keyring fingerprint"
else
  fail "could not read the fingerprint from $KEYRING"
fi

# --- 1. Copr SRPM submission path ------------------------------------------
COPR_BLOCK="$(
  awk '/^  copr:/{ inside = 1 } inside && /^  apt:/{ exit } inside' "$RELEASE"
)"
if [[ -z "$COPR_BLOCK" ]]; then
  fail "could not locate the copr job in $RELEASE"
else
  pass "located the copr job"
fi

for workflow in "$RELEASE" "$REPAIR"; do
  expect_absent "$workflow" "uploadrpm" \
    "$(basename "$workflow") does not use direct RPM upload"
  expect_contains "$workflow" "apt-get install -y --no-install-recommends rpm zstd" \
    "$(basename "$workflow") installs rpmbuild and zstd"
  expect_contains "$workflow" "scripts/build-copr-srpm.sh" \
    "$(basename "$workflow") builds the Copr SRPM through the shared helper"
  expect_contains "$workflow" 'copr-cli build --timeout 1800 "${ARGS[@]}" "$COPR_PROJECT" "$SRPM"' \
    "$(basename "$workflow") submits the SRPM to Copr"
  expect_contains "$workflow" "bitty-\${VERSION}-\${arch}-unknown-linux-gnu.tar.zst" \
    "$(basename "$workflow") uses the published release source bundles"
  expect_contains "$workflow" 'python3 -m venv "$RUNNER_TEMP/copr-cli"' \
    "$(basename "$workflow") creates a venv under RUNNER_TEMP"
  expect_contains "$workflow" '"$RUNNER_TEMP/copr-cli/bin/pip" install copr-cli' \
    "$(basename "$workflow") installs copr-cli with pip"
  expect_contains "$workflow" 'echo "$RUNNER_TEMP/copr-cli/bin" >> "$GITHUB_PATH"' \
    "$(basename "$workflow") puts the venv bin directory on PATH"
  expect_contains "$workflow" '"$RUNNER_TEMP/copr-cli/bin/copr-cli" --version' \
    "$(basename "$workflow") prints copr-cli --version"
done

if grep -Eq 'apt-get install.*copr-cli' <<<"$COPR_BLOCK"; then
  fail "copr job still apt-installs copr-cli"
else
  pass "copr job does not apt-install copr-cli"
fi
expect_contains "$RELEASE" 'BASE_URL="https://cdn.bitty.run/releases/${TAG}"' \
  "release copr job reads source bundles from the CDN"
expect_contains "$REPAIR" '--pattern "bitty-${VERSION}-x86_64-unknown-linux-gnu.tar.zst"' \
  "repair copr job downloads the x86_64 source bundle"
expect_contains "$REPAIR" '--pattern "bitty-${VERSION}-aarch64-unknown-linux-gnu.tar.zst"' \
  "repair copr job downloads the aarch64 source bundle"
expect_contains "$REPAIR" "tooling/scripts/build-copr-srpm.sh" \
  "repair copr job builds the SRPM from default-branch tooling"
expect_contains "$REPAIR" "--spec tooling/packaging/bitty.spec" \
  "repair copr job uses the corrected default-branch spec"
if [[ "$COPR_BLOCK" == *'copr-cli build --timeout 1800 "${ARGS[@]}" "$COPR_PROJECT" "$SRPM"'* ]]; then
  pass "release copr job submits one SRPM to all discovered chroots"
else
  fail "release copr job does not submit the SRPM to the discovered chroots"
fi
if grep -qF 'CHROOTS="$(printf' <<<"$COPR_BLOCK" &&
  grep -qF -- 'x86_64|aarch64' <<<"$COPR_BLOCK"; then
  pass "release copr job discovers x86_64/aarch64 chroots"
else
  fail "release copr job does not discover x86_64/aarch64 chroots"
fi

BUILD_SRPM="scripts/build-copr-srpm.sh"
[[ -x "$BUILD_SRPM" ]] || fail "SRPM helper missing or not executable: $BUILD_SRPM"
expect_contains "$BUILD_SRPM" 'rpmbuild -bs "$SPEC_COPY"' \
  "SRPM helper runs rpmbuild -bs"
expect_contains "$BUILD_SRPM" 'Version:        " version' \
  "SRPM helper materializes the release version into the spec copy"
expect_contains "$BUILD_SRPM" '--define "version $VERSION"' \
  "SRPM helper overrides the spec version"
expect_contains "$BUILD_SRPM" '--define "_sourcedir $SOURCEDIR"' \
  "SRPM helper scopes the source directory"
expect_contains "$BUILD_SRPM" '--define "_srcrpmdir $OUTPUT_DIR"' \
  "SRPM helper scopes the SRPM output directory"
expect_contains "$BUILD_SRPM" "zstd -t" \
  "SRPM helper validates zstd sources"
expect_contains "$BUILD_SRPM" "sha256sum -c" \
  "SRPM helper verifies source checksums when SHA256SUMS is present"
expect_contains "$BUILD_SRPM" 'sha256sum -c "$CHECK_FILE" >&2' \
  "SRPM helper keeps checksum verification output off stdout"
expect_contains "$BUILD_SRPM" 'rpm -qp --qf' \
  "SRPM helper verifies SRPM metadata"

expect_contains "packaging/bitty.spec" 'Version:        %{version}' \
  "spec version is overridable"
expect_contains "packaging/bitty.spec" '%{!?version:%global version 0.0.27}' \
  "spec keeps a default version"
expect_contains "packaging/bitty.spec" "BuildRequires:  zstd" \
  "spec declares zstd for .tar.zst extraction"
expect_contains "packaging/bitty.spec" "%global debug_package %{nil}" \
  "spec disables debuginfo for the prebuilt payload"

# --- 2. APT signing key import ---------------------------------------------
expect_contains "$RELEASE" "scripts/import-apt-gpg-key.sh --fingerprint" \
  "release.yml imports the APT key through the robust helper"
expect_contains "$RELEASE" "APT_GPG_KEY_FINGERPRINT: BCB87D7B489A64168080D900DF4751B6EA711CB8" \
  "release.yml pins the archive key fingerprint"
expect_contains "$RELEASE" 'install -d -m 0700 "$GNUPGHOME"' \
  "release.yml imports into a private ephemeral GnuPG home"
expect_absent "$RELEASE" 'echo "$APT_GPG_PRIVATE_KEY" | gpg --batch --import' \
  "release.yml no longer pipes the raw secret into gpg"
expect_contains "$RELEASE" "bash scripts/tests/import-apt-gpg-key.test.sh" \
  "release.yml validates the APT key importer"
expect_contains "$RELEASE" "bash scripts/tests/check-release-channels.test.sh" \
  "release.yml validates the release-channel wiring"
expect_contains "$RELEASE" "gh workflow run repair-release-channels.yml" \
  "release.yml points failed channels at the emergency repair workflow"

# --- 3. Emergency repair workflow ------------------------------------------
expect_contains "$REPAIR" "workflow_dispatch:" \
  "repair workflow is manually dispatched"
expect_contains "$REPAIR" 'default: "v0.0.27"' \
  "repair workflow defaults to v0.0.27"
expect_contains "$REPAIR" "type: choice" \
  "repair workflow exposes a channel choice"
for channel in apt copr all; do
  expect_contains "$REPAIR" "- $channel" "repair workflow offers the $channel channel"
done
expect_contains "$REPAIR" "inputs.channels == 'all' || inputs.channels == 'apt'" \
  "repair workflow gates the APT job on the channels input"
expect_contains "$REPAIR" 'TAG: ${{ inputs.tag }}' \
  "repair workflow binds the tag input to an environment variable"
expect_contains "$REPAIR" 'if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]' \
  "repair workflow validates the tag shape before use"
expect_contains "$REPAIR" "ref: \${{ needs.validate.outputs.tag }}" \
  "repair jobs check out the requested published tag"
expect_contains "$REPAIR" 'ref: ${{ github.event.repository.default_branch }}' \
  "repair tooling comes from the default branch"
expect_contains "$REPAIR" "environment: release" \
  "repair jobs run in the release environment"
expect_contains "$REPAIR" "contents: read" \
  "repair workflow has read-only repository permissions"
expect_absent "$REPAIR" "contents: write" \
  "repair workflow cannot mutate repository contents"
expect_contains "$REPAIR" "gh release download" \
  "repair workflow downloads published release assets"
expect_contains "$REPAIR" "sha256sum -c" \
  "repair workflow verifies published asset checksums"
expect_contains "$REPAIR" "tooling/scripts/import-apt-gpg-key.sh" \
  "repair workflow reuses the robust APT key importer"
expect_contains "$REPAIR" "bitty/apt/" \
  "repair workflow republishes the R2 APT prefix"
expect_contains "$REPAIR" 'wrangler r2 object get "bitty/apt/dists/stable/Release"' \
  "repair workflow reads back the APT index"
expect_contains "$REPAIR" "python3 -m venv" \
  "repair workflow installs copr-cli in a venv"
expect_contains "$REPAIR" '"$RUNNER_TEMP/copr-cli/bin/pip" install copr-cli' \
  "repair workflow installs copr-cli with pip"
expect_contains "$REPAIR" 'git fetch --no-tags --depth=1 origin "$DEFAULT_BRANCH"' \
  "repair workflow fetches the default branch for the flake.lock comparison"
expect_contains "$REPAIR" "nix flake metadata --json ." \
  "repair workflow validates a newer flake.lock before adopting it"
expect_contains "$REPAIR" "nix build .#bitty" \
  "repair workflow builds the Nix package"

for forbidden in "git tag" "git push" "gh release create" "gh release upload" "gh release edit" "gh release delete"; do
  if grep -qF -- "$forbidden" "$REPAIR"; then
    fail "repair workflow must not mutate tags or release assets (found: '$forbidden')"
  else
    pass "repair workflow does not run '$forbidden'"
  fi
done

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-release-channels.test: FAIL" >&2
  exit 1
fi
echo "check-release-channels.test: PASS"
