#!/usr/bin/env bash
# check-release-channels.test.sh — static regression guards for the 0.0.27
# release-channel repairs.
#
# Asserts:
#   1. release.yml no longer apt-installs copr-cli; the Copr job creates a
#      Python venv, installs copr-cli with pip, puts its bin on PATH, and
#      prints the version.
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

# --- 1. Copr CLI installation path -----------------------------------------
COPR_BLOCK="$(
  awk '/^  copr:/{ inside = 1 } inside && /^  apt:/{ exit } inside' "$RELEASE"
)"
if [[ -z "$COPR_BLOCK" ]]; then
  fail "could not locate the copr job in $RELEASE"
else
  pass "located the copr job"
fi

if [[ "$COPR_BLOCK" == *"apt-get install"*"copr-cli"* ]]; then
  fail "copr job still apt-installs copr-cli"
else
  pass "copr job no longer apt-installs copr-cli"
fi
if [[ "$COPR_BLOCK" == *'python3 -m venv "$RUNNER_TEMP/copr-cli"'* ]]; then
  pass "copr job creates a venv under RUNNER_TEMP"
else
  fail "copr job does not create a venv under RUNNER_TEMP"
fi
if [[ "$COPR_BLOCK" == *'"$RUNNER_TEMP/copr-cli/bin/pip" install copr-cli'* ]]; then
  pass "copr job installs copr-cli with pip"
else
  fail "copr job does not install copr-cli with pip"
fi
if [[ "$COPR_BLOCK" == *'export PATH="$RUNNER_TEMP/copr-cli/bin:$PATH"'* ]]; then
  pass "copr job puts the venv bin directory on PATH"
else
  fail "copr job does not put the venv bin directory on PATH"
fi
if [[ "$COPR_BLOCK" == *"copr-cli --version"* ]]; then
  pass "copr job prints copr-cli --version"
else
  fail "copr job does not print copr-cli --version"
fi

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
