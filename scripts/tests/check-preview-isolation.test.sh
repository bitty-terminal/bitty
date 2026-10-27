#!/usr/bin/env bash
# check-preview-isolation.test.sh — fail-closed static isolation proof.
#
# Accepted contract: bitty-terminal-docs/specifications/preview-release-contract.md
# (Isolation proof, Required assertions 1-4). The test is intentionally a
# static workflow check: a grep alone would not prove behavior, so every stable
# publisher is also required to carry all three independent guards (needs
# validate, the stable output, and the dash guard) and no `always()` bypass.
#
# Optional review-time extension: set PREVIEW_ISOLATION_ACT=1 to also run the
# `act -n` job listing for both workflows. This is syntax-only; the predicate
# execution gate (`act push` with a preview-tag event) still needs Docker and is
# run by the reviewer, not by CI.
set -euo pipefail

cd "$(dirname "$0")/../.."

PREVIEW=".github/workflows/preview.yml"
RELEASE=".github/workflows/release.yml"
FAIL=0
TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/check-preview-isolation-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

# Strip comments so an explanatory comment can name a forbidden surface
# without tripping the check, while active YAML/run text still must not.
strip_comments() {
  sed -E 's/(^|[[:space:]])#.*$/\1/' "$1"
}

[[ -f "$PREVIEW" ]] || fail "preview workflow missing: $PREVIEW"
[[ -f "$RELEASE" ]] || fail "stable release workflow missing: $RELEASE"
if ((FAIL)); then
  exit 1
fi

PREVIEW_ACTIVE="$TMP_BASE/preview.active"
RELEASE_ACTIVE="$TMP_BASE/release.active"
strip_comments "$PREVIEW" >"$PREVIEW_ACTIVE"
strip_comments "$RELEASE" >"$RELEASE_ACTIVE"

expect_absent() { # <file> <literal> <label>
  if grep -qF -- "$2" "$1"; then
    fail "$3 (unexpected: '$2')"
  else
    pass "$3"
  fi
}

expect_present() { # <file> <literal> <label>
  if grep -qF -- "$2" "$1"; then
    pass "$3"
  else
    fail "$3 (missing: '$2')"
  fi
}

extract_job() { # <file> <job>
  awk -v job="$2" '
    $0 == "  " job ":" { inside = 1; next }
    inside && /^  [a-z0-9-]+:$/ { exit }
    inside { print }
  ' "$1"
}

# --- 1. Preview workflow has no stable surface or credential ----------------
for token in \
  "environment:" \
  "wrangler" \
  "bitty/install" \
  "bitty/releases" \
  "bitty/apt" \
  "latest.txt" \
  "cdn.bitty.run" \
  "homebrew" \
  "HOMEBREW_" \
  "scoop" \
  "SCOOP_" \
  "aur" \
  "AUR_" \
  "PKGBUILD" \
  "copr" \
  "cachix" \
  "CACHIX" \
  "cargo publish" \
  "CARGO_REGISTRY_TOKEN" \
  "CLOUDFLARE" \
  "secrets." \
  "codesign" \
  "notarytool" \
  "sigstore" \
  "cosign" \
  "gpg" \
  "authenticode" \
  "sbom" \
  "SBOM" \
  "winget" \
  "setup.exe" \
  ".dmg" \
  ".deb" \
  ".rpm" \
  ".apk" \
  ".pkg.tar.zst"; do
  expect_absent "$PREVIEW_ACTIVE" "$token" "preview workflow excludes '$token'"
done

# `aur` is a substring of common English words; make sure the broad loop above
# is backed by an exact job/publisher token check as well.
if grep -Eqi '(^|[^a-z])aur([^a-z]|$)' "$PREVIEW_ACTIVE"; then
  fail "preview workflow still names the AUR publisher"
else
  pass "preview workflow has no AUR publisher token"
fi

# The same-commit marker is release state, not a tag alone, so a failed run
# that left a tag cannot suppress the next legitimate preview.
expect_present "$PREVIEW_ACTIVE" "gh release list --exclude-drafts" \
  "preflight reads successful release state"
expect_present "$PREVIEW_ACTIVE" "--releases-file" \
  "preflight receives the release marker list"

# --- 2. Public pre-release metadata and tag guards --------------------------
if grep -qF -- "--prerelease" "$PREVIEW_ACTIVE" &&
  grep -qF -- "--latest=false" "$PREVIEW_ACTIVE"; then
  pass "release creation is a pre-release and never latest"
elif grep -qF "prerelease: true" "$PREVIEW_ACTIVE" &&
  grep -qF "make_latest: false" "$PREVIEW_ACTIVE"; then
  pass "release creation is a pre-release and never latest (action form)"
else
  fail "release creation does not prove prerelease + not-latest"
fi

if grep -qF -- "git ls-remote --exit-code --tags origin" "$PREVIEW_ACTIVE"; then
  pass "release job refuses an existing preview tag"
else
  fail "release job has no tag-existence guard"
fi

if grep -qF "\${{ needs.preflight.outputs.tag }}" "$PREVIEW_ACTIVE" &&
  grep -qF "refs/tags/" "$PREVIEW_ACTIVE"; then
  pass "the preview identity comes from the preflight output"
else
  fail "the preview tag is not threaded from the preflight"
fi

if grep -qF 'PREVIEW_TAG_RE=' scripts/preview-preflight.sh &&
  grep -qF -- '-preview\.([0-9]{8})' scripts/preview-preflight.sh; then
  pass "the preflight enforces the accepted preview tag grammar"
else
  fail "the preflight does not enforce the accepted preview tag grammar"
fi

expect_present "$PREVIEW_ACTIVE" "actions/attest-build-provenance" \
  "archives are attested with build provenance"
expect_present "$PREVIEW_ACTIVE" "gh attestation verify" \
  "attestations are verified in the run"
expect_present "$PREVIEW_ACTIVE" "gh release delete" \
  "retention pruning deletes old preview releases"
expect_present "$PREVIEW_ACTIVE" "--cleanup-tag" \
  "retention pruning removes the matching preview tag"

# Stable readers reject the preview identity. The Unix bootstrap runs in
# --dry-run with a pinned prerelease version: normalize_version rejects the
# dash before any network call, so this is a real hermetic assertion. The
# PowerShell equivalent is asserted statically because pwsh is not available on
# every test host.
if scripts/check-release-version.sh --tag v0.0.30-preview.20261027 >/dev/null 2>&1; then
  fail "check-release-version.sh accepted a preview tag"
else
  pass "check-release-version.sh rejects a preview tag"
fi
INSTALL_LOG="$TMP_BASE/install.log"
if BITTY_VERSION=v0.0.30-preview.20261027 bash scripts/install.sh --dry-run >"$INSTALL_LOG" 2>&1; then
  fail "install.sh accepted a prerelease version"
elif grep -qF "prerelease versions are not served by the bootstrap scripts" "$INSTALL_LOG"; then
  pass "install.sh rejects a prerelease version before any network access"
else
  fail "install.sh failed without the prerelease refusal: $(tr '\n' ' ' <"$INSTALL_LOG")"
fi
if grep -qF "prerelease versions are not served by the bootstrap scripts" scripts/install.ps1; then
  pass "scripts/install.ps1 rejects prerelease versions"
else
  fail "scripts/install.ps1 has no prerelease rejection"
fi

# --- 3. Preview triggers and per-job gating ---------------------------------
if grep -qF 'cron: "30 21 * * *"' "$PREVIEW_ACTIVE" &&
  grep -qF 'cron: "30 22 * * *"' "$PREVIEW_ACTIVE"; then
  pass "both UTC cron slots are present"
else
  fail "the two accepted UTC crons are missing"
fi

if grep -qF "workflow_dispatch:" "$PREVIEW_ACTIVE"; then
  pass "manual dispatch is available"
else
  fail "workflow_dispatch is missing"
fi

if grep -qE '^  push:|^    tags:' "$PREVIEW_ACTIVE"; then
  fail "preview workflow is tag-triggered; it must not run from a pushed tag"
else
  pass "preview workflow is schedule/dispatch only"
fi

mapfile -t preview_jobs < <(
  awk '
    /^jobs:$/ { jobs = 1; next }
    jobs && /^  [a-z0-9-]+:$/ { sub(/^  /, ""); sub(/:$/, ""); print }
  ' "$PREVIEW_ACTIVE"
)
if ((${#preview_jobs[@]} >= 2)); then
  pass "located ${#preview_jobs[@]} preview jobs"
else
  fail "could not locate the preview jobs"
fi

for job in "${preview_jobs[@]}"; do
  [[ "$job" == "preflight" ]] && continue
  block="$(extract_job "$PREVIEW_ACTIVE" "$job")"
  if [[ -z "$block" ]]; then
    fail "could not extract preview job '$job'"
    continue
  fi
  if grep -qF "needs.preflight.outputs.run == 'true'" <<<"$block"; then
    pass "preview job '$job' gates on the preflight run output"
  else
    fail "preview job '$job' does not gate on needs.preflight.outputs.run == 'true'"
  fi
  if grep -Eq '^    needs:.*\bpreflight\b|^ +preflight,?$' <<<"$block"; then
    pass "preview job '$job' declares preflight in needs"
  else
    fail "preview job '$job' does not declare needs: preflight"
  fi
done

for job in isolation build package release; do
  block="$(extract_job "$PREVIEW_ACTIVE" "$job")"
  if grep -qF 'ref: ${{ needs.preflight.outputs.sha }}' <<<"$block"; then
    pass "preview job '$job' checks out the pinned preflight commit"
  else
    fail "preview job '$job' does not pin the preflight commit"
  fi
done

# The isolation proof must run before any build leg starts.
isolation_block="$(extract_job "$PREVIEW_ACTIVE" isolation)"
build_block="$(extract_job "$PREVIEW_ACTIVE" build)"
if grep -qF "./scripts/tests/check-preview-isolation.test.sh" <<<"$isolation_block"; then
  pass "the isolation job runs the preview isolation test"
else
  fail "the isolation job does not run the preview isolation test"
fi
if grep -Eq '^    needs:.*\bisolation\b|^ +isolation,?$' <<<"$build_block"; then
  pass "the build job waits for the isolation gate"
else
  fail "the build job does not declare needs: isolation"
fi
expect_absent "$PREVIEW_ACTIVE" "--skip-arch-check" \
  "preview workflow does not bypass the macOS arch assertion"

# --- 4. Stable predicate regression on every publisher ---------------------
if grep -qF 'stable: ${{ steps.stable.outputs.stable }}' "$RELEASE_ACTIVE"; then
  pass "validate publishes an explicit stable output"
else
  fail "validate has no stable output"
fi

publishers=(
  release
  r2-mirror
  r2-stable
  aur
  aur-bin
  homebrew
  scoop
  copr
  apt
  nix
)

for job in "${publishers[@]}"; do
  block="$(extract_job "$RELEASE_ACTIVE" "$job")"
  if [[ -z "$block" ]]; then
    fail "could not extract stable publisher '$job'"
    continue
  fi

  if grep -Eq '^    needs:.*\bvalidate\b|^ +validate,?$' <<<"$block"; then
    pass "publisher '$job' declares needs: validate"
  else
    fail "publisher '$job' does not declare needs: validate"
  fi

  if grep -qF "needs.validate.outputs.stable == 'true'" <<<"$block"; then
    pass "publisher '$job' requires the stable output"
  else
    fail "publisher '$job' does not require needs.validate.outputs.stable == 'true'"
  fi

  if grep -qF "!contains(github.ref_name, '-')" <<<"$block"; then
    pass "publisher '$job' keeps the dash guard"
  else
    fail "publisher '$job' lost the !contains(github.ref_name, '-') guard"
  fi

  if grep -qF "startsWith(github.ref, 'refs/tags/v')" <<<"$block"; then
    pass "publisher '$job' keeps the version-tag trigger"
  else
    fail "publisher '$job' lost the version-tag trigger"
  fi

  if grep -qF "always()" <<<"$block"; then
    fail "publisher '$job' uses always(); the stable predicate can be bypassed"
  else
    pass "publisher '$job' has no always() bypass"
  fi
done

# --- 5. Optional review-time act listing ------------------------------------
if [[ "${PREVIEW_ISOLATION_ACT:-0}" == "1" ]]; then
  if command -v act >/dev/null 2>&1; then
    for workflow in "$PREVIEW" "$RELEASE"; do
      if act -n -W "$workflow" --list >/dev/null 2>&1; then
        pass "act -n lists the jobs in $workflow"
      else
        fail "act -n failed for $workflow"
      fi
    done
  else
    echo "SKIP: PREVIEW_ISOLATION_ACT=1 but act is not installed"
  fi
fi

if ((FAIL)); then
  echo "check-preview-isolation-test: FAIL" >&2
  exit 1
fi
echo "check-preview-isolation-test: OK"
