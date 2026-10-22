#!/usr/bin/env bash
# check-aur-bin-render.test.sh — #2417 fixture test for the AUR `bitty-bin`
# publish assertion.
#
# The v0.0.25 release failed only the `bitty-bin` channel because `release.yml`
# grepped the rendered PKGBUILD for a literal `.../v${pkgver}/<asset>` needle
# while `packaging/PKGBUILD.bin` renders `.../v${pkgver}/${_bitty_asset}`. This
# test renders the template with a fixed version and digest and proves:
#   - the rendered `source_x86_64` binary entry is the expected exact line
#   - the pre-fix literal needle does NOT match that recipe, so the old
#     assertion would reject a correct file (the #2417 regression)
#   - scripts/assert-aur-bin-render.sh accepts the same recipe
#   - the generated `.SRCINFO` carries the resolved CDN URL and digest
#   - the checker rejects the GitHub fallback host, SKIP/placeholder digests,
#     a stale pkgver, and an `.SRCINFO` that disagrees with the PKGBUILD
#   - release.yml publishes through the shared checker and summarizes every
#     package channel in the run summary (#2417 observability)
#
# `makepkg` is optional: the `.SRCINFO` cases are skipped where Arch tooling is
# absent (the ubuntu `validate` job) and run on Arch hosts and release
# containers that ship it.
set -euo pipefail

cd "$(dirname "$0")/../.."

ASSERT=./scripts/assert-aur-bin-render.sh
TEMPLATE=./packaging/PKGBUILD.bin
WORKFLOW=./.github/workflows/release.yml
FAIL=0
VERSION=0.0.26
SHA=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855

TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-aur-bin-render.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

expect_reject() { # <label> <command...>
  local label="$1"
  shift
  if "$@" >"$TMP/reject.out" 2>"$TMP/reject.err"; then
    fail "checker accepted $label"
  else
    pass "checker rejects $label"
  fi
}

[[ -x "$ASSERT" ]] || fail "checker is missing or not executable: $ASSERT"
[[ -f "$TEMPLATE" ]] || fail "PKGBUILD template is missing: $TEMPLATE"
[[ -f "$WORKFLOW" ]] || fail "release workflow is missing: $WORKFLOW"
bash -n "$ASSERT" || fail "bash -n rejects $ASSERT"
bash -n "$TEMPLATE" || fail "bash -n rejects $TEMPLATE"

# --- Render the template exactly like the `aur-bin` publish job -------------
RENDERED="$TMP/PKGBUILD"
sed -e "s/^pkgver=.*/pkgver=$VERSION/" \
  -e "s/REPLACE_WITH_RELEASE_SHA256/$SHA/" \
  "$TEMPLATE" >"$RENDERED"
bash -n "$RENDERED" || fail "bash -n rejects the rendered PKGBUILD"

ASSET="$(awk -F'"' '/^_bitty_asset=/{print $2; exit}' "$RENDERED")"
[[ -n "$ASSET" ]] || fail "rendered PKGBUILD does not declare _bitty_asset"

# The exact rendered binary source entry. `${pkgver}` and `${_bitty_asset}`
# stay unexpanded here: the template owns them, and the checker resolves them.
EXPECTED_SOURCE_LINE='  "${_bitty_src}::https://cdn.bitty.run/releases/v${pkgver}/${_bitty_asset}"'
if grep -qxF -- "$EXPECTED_SOURCE_LINE" "$RENDERED"; then
  pass "rendered source_x86_64 binary entry is the exact CDN template line"
else
  fail "rendered source_x86_64 binary entry drifted from: $EXPECTED_SOURCE_LINE"
fi

ASSET_RESOLVED="${ASSET//\$\{pkgver\}/${VERSION}}"
RESOLVED_URL="https://cdn.bitty.run/releases/v${VERSION}/${ASSET_RESOLVED}"

# --- #2417 regression: the pre-fix literal needle cannot match --------------
# This block is the assertion `release.yml` used before the fix, byte for
# byte. It must stay false on a correct recipe; otherwise the fixture no
# longer reproduces v0.0.25 and this test proves nothing.
OLD_NEEDLE="https://cdn.bitty.run/releases/v\${pkgver}/${ASSET}"
if grep -qF "$OLD_NEEDLE" "$RENDERED"; then
  fail "#2417 fixture matches the pre-fix literal needle; the regression case is not faithful"
else
  pass "#2417 pre-fix literal asset needle does not match the rendered recipe (old assertion fails here)"
fi

if "$ASSERT" "$RENDERED" "$VERSION" "$SHA" >"$TMP/assert.log" 2>&1; then
  pass "#2417 fixed checker accepts the rendered recipe ($RESOLVED_URL)"
else
  fail "fixed checker rejected the rendered recipe: $(tail -n 2 "$TMP/assert.log")"
fi

# --- .SRCINFO derived by makepkg -------------------------------------------
if command -v makepkg >/dev/null 2>&1 && [[ "$EUID" -ne 0 ]]; then
  if (cd "$TMP" && makepkg --printsrcinfo >.SRCINFO) 2>"$TMP/makepkg.err"; then
    SRCINFO="$TMP/.SRCINFO"
    srcinfo_has() { # <exact line, ignoring leading indentation>
      awk -v want="$1" '
        { line = $0; sub(/^[[:space:]]+/, "", line) }
        line == want { found = 1; exit }
        END { exit(found ? 0 : 1) }
      ' "$SRCINFO"
    }
    srcinfo_has "pkgname = bitty-bin" ||
      fail ".SRCINFO does not declare pkgname = bitty-bin"
    srcinfo_has "pkgver = $VERSION" ||
      fail ".SRCINFO pkgver is not $VERSION"
    srcinfo_has "source_x86_64 = bitty-${VERSION}-x86_64-unknown-linux-gnu.tar.zst::${RESOLVED_URL}" ||
      fail ".SRCINFO binary source is not the resolved R2/CDN URL"
    srcinfo_has "sha256sums_x86_64 = $SHA" ||
      fail ".SRCINFO does not carry the verified binary digest"
    pass ".SRCINFO carries the resolved R2/CDN source and verified digest"

    if "$ASSERT" "$RENDERED" "$VERSION" "$SHA" "$SRCINFO" >"$TMP/assert-srcinfo.log" 2>&1; then
      pass "checker accepts the PKGBUILD plus the .SRCINFO that ships to AUR"
    else
      fail "checker rejected a matching .SRCINFO: $(tail -n 2 "$TMP/assert-srcinfo.log")"
    fi

    sed "s#/v${VERSION}/#/v0.0.25/#" "$SRCINFO" >"$TMP/.SRCINFO.stale"
    expect_reject "an .SRCINFO pinned to another release" \
      "$ASSERT" "$RENDERED" "$VERSION" "$SHA" "$TMP/.SRCINFO.stale"
  else
    fail "makepkg --printsrcinfo rejected the rendered recipe: $(tail -n 2 "$TMP/makepkg.err")"
  fi
else
  echo "SKIP: makepkg unavailable or running as root, .SRCINFO cases skipped"
fi

# --- Checker negative cases ------------------------------------------------
sed 's#https://cdn\.bitty\.run/#https://github.com/#g' "$RENDERED" >"$TMP/PKGBUILD.github"
expect_reject "a recipe downloading from the GitHub fallback host" \
  "$ASSERT" "$TMP/PKGBUILD.github" "$VERSION" "$SHA"

sed "s/$SHA/SKIP/" "$RENDERED" >"$TMP/PKGBUILD.skip"
expect_reject "a binary sha256 slot of SKIP" \
  "$ASSERT" "$TMP/PKGBUILD.skip" "$VERSION" "$SHA"

sed "s/^pkgver=.*/pkgver=0.0.25/" "$RENDERED" >"$TMP/PKGBUILD.stale"
expect_reject "a stale pkgver with the new version's digest" \
  "$ASSERT" "$TMP/PKGBUILD.stale" "$VERSION" "$SHA"

# Use a version deliberately different from the template's current pkgver:
# at release time the template is bumped in the same commit, so passing the
# live VERSION would make this case a legitimate bump, not an unbumped one.
UNBUMPED_VERSION=9.9.9
sed "s/REPLACE_WITH_RELEASE_SHA256/$SHA/" "$TEMPLATE" >"$TMP/PKGBUILD.unrendered"
expect_reject "a template copy whose pkgver was never bumped" \
  "$ASSERT" "$TMP/PKGBUILD.unrendered" "$UNBUMPED_VERSION" "$SHA"

expect_reject "a version carrying the leading v" \
  "$ASSERT" "$RENDERED" "v$VERSION" "$SHA"

# --- Workflow wiring -------------------------------------------------------
job_block() { # <job id>
  sed -n "/^  $1:/,/^  [a-z0-9-]*:/p" "$WORKFLOW"
}

AUR_BLOCK="$(job_block aur-bin)"
if grep -qF 'scripts/assert-aur-bin-render.sh' <<<"$AUR_BLOCK"; then
  pass "release.yml aur-bin asserts through the shared checker"
else
  fail "release.yml aur-bin does not call scripts/assert-aur-bin-render.sh"
fi
if grep -qF '"$AUR_DIR/PKGBUILD" "$VERSION" "$SHA" "$AUR_DIR/.SRCINFO"' <<<"$AUR_BLOCK"; then
  pass "release.yml aur-bin checks the pushed .SRCINFO with the rendered recipe"
else
  fail "release.yml aur-bin does not check the pushed .SRCINFO"
fi
if grep -qF 'v\${pkgver}/${ASSET}' <<<"$AUR_BLOCK"; then
  fail "#2417 literal asset-name needle returned to release.yml aur-bin"
else
  pass "#2417 literal asset-name needle is gone from release.yml aur-bin"
fi

SUMMARY_BLOCK="$(job_block channel-summary)"
if [[ -n "$SUMMARY_BLOCK" ]] &&
  grep -qE 'needs: \[r2-mirror, aur, aur-bin, homebrew, scoop, copr, apt\]' <<<"$SUMMARY_BLOCK" &&
  grep -qF 'always()' <<<"$SUMMARY_BLOCK" &&
  grep -qF 'GITHUB_STEP_SUMMARY' <<<"$SUMMARY_BLOCK"; then
  pass "release.yml summarizes every package channel even when a job fails"
else
  fail "release.yml has no always-on package-channel summary"
fi
for channel in r2-mirror aur aur-bin homebrew scoop copr apt; do
  if grep -qF "needs.${channel}.result" <<<"$SUMMARY_BLOCK"; then
    pass "channel summary reports ${channel}"
  else
    fail "channel summary does not report ${channel}"
  fi
done

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-aur-bin-render.test: FAIL" >&2
  exit 1
fi
echo "check-aur-bin-render.test: PASS"
