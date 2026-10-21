#!/usr/bin/env bash
# check-package-manifests.test.sh — pin the R2/CDN-primary channel contract.
#
# CTX-1413 (#1847): AUR `bitty-bin`, Homebrew, and Scoop prefer the R2-backed
# CDN and keep GitHub Releases as the documented fallback. This test renders
# each generated manifest with fixed inputs (no network) and asserts:
#   - every download URL uses https://cdn.bitty.run/releases/v<version>/
#   - the published sha256 is embedded in the manifest
#   - the GitHub Releases fallback URL is documented
#   - the release workflow renders through scripts/render-package-manifests.sh
#     and waits for r2-mirror before publishing
#   - the renderer rejects a non-CDN base URL and malformed digests
#
# CTX-1433 (#2364) hardens the same contract: the AUR/Homebrew/Scoop publish
# jobs must verify `${BASE_URL}/<asset>.sha256` through
# `scripts/verify-cdn-sidecar.sh`, and this gate runs that verifier against a
# file:// fixture so a digest computed from the local copy (a self-referential
# hash) cannot pass. The docs must scope the sidecar claim to bare binaries and
# archives and name the package/FreeBSD exceptions.
#
# The live tag-time fetch/verify path stays in release.yml; this gate pins the
# generation contract on every PR.
set -euo pipefail

cd "$(dirname "$0")/../.."

RENDER=./scripts/render-package-manifests.sh
WORKFLOW=./.github/workflows/release.yml
PKGBUILD=./packaging/PKGBUILD.bin
README=./packaging/README.md
INSTALL=./packaging/INSTALL.md
TOP_README=./README.md
FAIL=0

TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-package-manifests.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

VERSION=0.0.24
BASE_URL="https://cdn.bitty.run/releases/v${VERSION}"
INTEL_ASSET=bitty-x86_64-apple-darwin
ARM_ASSET=bitty-aarch64-apple-darwin
X64_ASSET=bitty-x86_64-pc-windows-msvc.exe
ARM64_ASSET=bitty-aarch64-pc-windows-msvc.exe
SHA_INTEL=772bd9e7fcf2d3e64fed7c6f1241a3abd9da237e6540a7ede82fd375343ec3aa
SHA_ARM=88ff8a49211f3d9d4cccfd0a5894670a7c668900dc8922e7d982bbf855e3f0e8
SHA_X64=e0bdb50de8429e85e39979a76fb0ea24bcb862c18bf4ff5a2e7c8ecfe6a89642
SHA_ARM64=f718fb6bb769ca553f60803b04875ab3427dc0fb215f483d309ee73768117148

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
    fail "$3"
  fi
}

expect_render_fail() { # <label> <args...>
  local label="$1"
  shift
  if "$RENDER" "$@" >"$TMP/rejected.out" 2>"$TMP/rejected.err"; then
    fail "renderer accepted $label"
  else
    pass "renderer rejects $label"
  fi
}

[[ -x "$RENDER" ]] || fail "renderer is missing or not executable: $RENDER"
[[ -f "$WORKFLOW" ]] || fail "release workflow is missing: $WORKFLOW"

bash -n "$RENDER" || fail "bash -n rejects $RENDER"

# --- Homebrew ---------------------------------------------------------------
if "$RENDER" homebrew \
  --version "$VERSION" \
  --base-url "$BASE_URL" \
  --intel-asset "$INTEL_ASSET" \
  --arm-asset "$ARM_ASSET" \
  --sha-intel "$SHA_INTEL" \
  --sha-arm "$SHA_ARM" >"$TMP/bitty.rb" 2>"$TMP/homebrew.err"; then
  pass "renderer produces a Homebrew formula"
else
  fail "renderer failed for Homebrew: $(cat "$TMP/homebrew.err")"
fi

expect_contains "$TMP/bitty.rb" "url \"${BASE_URL}/${INTEL_ASSET}\"" \
  "Homebrew Intel URL uses the R2/CDN host"
expect_contains "$TMP/bitty.rb" "url \"${BASE_URL}/${ARM_ASSET}\"" \
  "Homebrew ARM URL uses the R2/CDN host"
expect_contains "$TMP/bitty.rb" "sha256 \"${SHA_INTEL}\"" \
  "Homebrew Intel sha256 is the expected digest"
expect_contains "$TMP/bitty.rb" "sha256 \"${SHA_ARM}\"" \
  "Homebrew ARM sha256 is the expected digest"
expect_contains "$TMP/bitty.rb" \
  "https://github.com/bitty-terminal/bitty/releases/download/v${VERSION}/${INTEL_ASSET}" \
  "Homebrew documents the GitHub Releases fallback"
if grep -Eq '^[[:space:]]*url "https://github\.com/' "$TMP/bitty.rb"; then
  fail "Homebrew has an active GitHub download URL"
else
  pass "Homebrew has no active GitHub download URL"
fi
if command -v ruby >/dev/null 2>&1; then
  ruby -c "$TMP/bitty.rb" >/dev/null || fail "Homebrew formula is not valid Ruby"
  pass "Homebrew formula parses as Ruby"
else
  echo "SKIP: ruby not available, formula syntax probe skipped"
fi

# --- Scoop ------------------------------------------------------------------
if "$RENDER" scoop \
  --version "$VERSION" \
  --base-url "$BASE_URL" \
  --x64-asset "$X64_ASSET" \
  --arm64-asset "$ARM64_ASSET" \
  --sha-x64 "$SHA_X64" \
  --sha-arm64 "$SHA_ARM64" >"$TMP/bitty.json" 2>"$TMP/scoop.err"; then
  pass "renderer produces a Scoop manifest"
else
  fail "renderer failed for Scoop: $(cat "$TMP/scoop.err")"
fi

python3 - "$TMP/bitty.json" "$BASE_URL" "$VERSION" "$SHA_X64" "$SHA_ARM64" <<'PY' || FAIL=1
import json
import sys

path, base, version, sha_x64, sha_arm64 = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    manifest = json.load(handle)

failures = []


def check(condition, message):
    if condition:
        print(f"PASS: {message}")
    else:
        failures.append(message)
        print(f"FAIL: {message}", file=sys.stderr)


arch = manifest.get("architecture", {})
check(arch.get("64bit", {}).get("url") == f"{base}/bitty-x86_64-pc-windows-msvc.exe",
      "Scoop x64 URL uses the R2/CDN host")
check(arch.get("arm64", {}).get("url") == f"{base}/bitty-aarch64-pc-windows-msvc.exe",
      "Scoop arm64 URL uses the R2/CDN host")
check(arch.get("64bit", {}).get("hash") == sha_x64,
      "Scoop x64 hash is the expected digest")
check(arch.get("arm64", {}).get("hash") == sha_arm64,
      "Scoop arm64 hash is the expected digest")
notes = manifest.get("notes", "")
check(f"https://github.com/bitty-terminal/bitty/releases/tag/v{version}" in notes,
      "Scoop documents the GitHub Releases fallback")
check(manifest.get("autoupdate", {}).get("architecture", {}).get("64bit", {}).get("url")
      == "https://cdn.bitty.run/releases/v$version/bitty-x86_64-pc-windows-msvc.exe",
      "Scoop x64 autoupdate uses the R2/CDN host")
check(manifest.get("autoupdate", {}).get("architecture", {}).get("arm64", {}).get("url")
      == "https://cdn.bitty.run/releases/v$version/bitty-aarch64-pc-windows-msvc.exe",
      "Scoop arm64 autoupdate uses the R2/CDN host")
check(all("github.com" not in str(value) for value in arch.values() if isinstance(value, dict)),
      "Scoop download entries have no GitHub URL")
sys.exit(1 if failures else 0)
PY

# --- Renderer negative cases ------------------------------------------------
expect_render_fail "a GitHub Releases base URL" homebrew \
  --version "$VERSION" \
  --base-url "https://github.com/bitty-terminal/bitty/releases/download/v${VERSION}" \
  --intel-asset "$INTEL_ASSET" --arm-asset "$ARM_ASSET" \
  --sha-intel "$SHA_INTEL" --sha-arm "$SHA_ARM"
expect_render_fail "an uppercase digest" homebrew \
  --version "$VERSION" --base-url "$BASE_URL" \
  --intel-asset "$INTEL_ASSET" --arm-asset "$ARM_ASSET" \
  --sha-intel "${SHA_INTEL^^}" --sha-arm "$SHA_ARM"
expect_render_fail "a path-traversing asset name" scoop \
  --version "$VERSION" --base-url "$BASE_URL" \
  --x64-asset "../${X64_ASSET}" --arm64-asset "$ARM64_ASSET" \
  --sha-x64 "$SHA_X64" --sha-arm64 "$SHA_ARM64"

job_block() { # <job>
  sed -n "/^  $1:/,/^  [a-z0-9-]*:/p" "$WORKFLOW"
}

# --- AUR bitty-bin ----------------------------------------------------------
if bash scripts/check-pkgbuild-bin.sh >"$TMP/pkgbuild.log" 2>&1; then
  pass "check-pkgbuild-bin passes (CDN URL + real digest)"
else
  fail "check-pkgbuild-bin failed: $(tail -n 3 "$TMP/pkgbuild.log")"
fi
BIN_SOURCE_LINE="$(awk '/^source_x86_64=/,/\)/' "$PKGBUILD" | grep '_bitty_src' | head -n 1)"
case "$BIN_SOURCE_LINE" in
*"https://cdn.bitty.run/releases/v\${pkgver}/\${_bitty_asset}"*)
  pass "AUR bitty-bin source uses the R2/CDN path"
  ;;
*) fail "AUR bitty-bin source is not the R2/CDN path: ${BIN_SOURCE_LINE:-missing}" ;;
esac
expect_contains "$PKGBUILD" \
  "https://github.com/bitty-terminal/bitty/releases/download/v\${pkgver}/\${_bitty_asset}" \
  "AUR bitty-bin documents the GitHub Releases fallback"

# --- AUR publish-time sidecar verification (CTX-1433, #2364) ----------------
# Pin the *published* sidecar, not a local digest: the publisher must fetch
# `${BASE_URL}/${ASSET}.sha256` and compare that value against the bytes it
# downloaded. String assertions alone let a self-referential local hash pass,
# so the fixture below runs the verifier release.yml actually calls.
VERIFIER=./scripts/verify-cdn-sidecar.sh
AUR_BLOCK="$(job_block aur-bin)"

if [[ -x "$VERIFIER" ]]; then
  pass "shared sidecar verifier is executable"
else
  fail "shared sidecar verifier is missing or not executable: $VERIFIER"
fi
bash -n "$VERIFIER" || fail "bash -n rejects $VERIFIER"
expect_contains "$VERIFIER" '${BASE_URL}/${ASSET}.sha256' \
  "verifier fetches the published <asset>.sha256 sidecar"

if grep -qF 'scripts/verify-cdn-sidecar.sh "$BASE_URL" "$ASSET"' <<<"$AUR_BLOCK"; then
  pass "aur-bin verifies the published sidecar through the shared verifier"
else
  fail "aur-bin does not verify through scripts/verify-cdn-sidecar.sh"
fi
if grep -q 'ACTUAL_SHA' <<<"$AUR_BLOCK"; then
  fail "aur-bin compares a locally computed digest instead of the sidecar digest"
else
  pass "aur-bin has no local-only digest comparison"
fi

SIDE_CDN="$TMP/cdn-ok/releases/v${VERSION}"
mkdir -p "$SIDE_CDN"
printf 'published release payload\n' >"$SIDE_CDN/$INTEL_ASSET"
SIDE_SHA="$(sha256sum "$SIDE_CDN/$INTEL_ASSET" | awk '{ print $1 }')"
# GNU sha256sum records the build-time path, so the sidecar names dist/<asset>.
printf '%s  dist/%s\n' "$SIDE_SHA" "$INTEL_ASSET" >"$SIDE_CDN/$INTEL_ASSET.sha256"

if GOT="$("$VERIFIER" "file://$SIDE_CDN" "$INTEL_ASSET" "$TMP/dl-ok" 2>"$TMP/ok.err")"; then
  if [[ "$GOT" == "$SIDE_SHA" ]]; then
    pass "verifier accepts a sidecar that describes the published bytes"
  else
    fail "verifier printed '${GOT}', expected '${SIDE_SHA}'"
  fi
else
  fail "verifier rejected a matching sidecar: $(tail -n 1 "$TMP/ok.err")"
fi

# A digest of some other local file (the classic self-referential hash) must
# not satisfy the check.
STALE_CDN="$TMP/cdn-stale"
mkdir -p "$STALE_CDN"
cp "$SIDE_CDN/$INTEL_ASSET" "$STALE_CDN/$INTEL_ASSET"
printf '%s  dist/%s\n' \
  "$(printf 'local rebuild\n' | sha256sum | awk '{ print $1 }')" "$INTEL_ASSET" \
  >"$STALE_CDN/$INTEL_ASSET.sha256"
if "$VERIFIER" "file://$STALE_CDN" "$INTEL_ASSET" "$TMP/dl-stale" >/dev/null 2>&1; then
  fail "verifier accepted a self-referential local hash"
else
  pass "verifier rejects a self-referential local hash"
fi

BAD_HEX_CDN="$TMP/cdn-bad-hex"
mkdir -p "$BAD_HEX_CDN"
cp "$SIDE_CDN/$INTEL_ASSET" "$BAD_HEX_CDN/$INTEL_ASSET"
printf '%s  dist/%s\n' "${SIDE_SHA^^}" "$INTEL_ASSET" >"$BAD_HEX_CDN/$INTEL_ASSET.sha256"
if "$VERIFIER" "file://$BAD_HEX_CDN" "$INTEL_ASSET" "$TMP/dl-hex" >/dev/null 2>&1; then
  fail "verifier accepted an uppercase digest"
else
  pass "verifier rejects an uppercase digest"
fi

MISSING_CDN="$TMP/cdn-missing"
mkdir -p "$MISSING_CDN"
cp "$SIDE_CDN/$INTEL_ASSET" "$MISSING_CDN/$INTEL_ASSET"
if "$VERIFIER" "file://$MISSING_CDN" "$INTEL_ASSET" "$TMP/dl-missing" >/dev/null 2>&1; then
  fail "verifier accepted an asset with no published sidecar"
else
  pass "verifier rejects an asset with no published sidecar"
fi

RENAMED_CDN="$TMP/cdn-renamed"
mkdir -p "$RENAMED_CDN"
cp "$SIDE_CDN/$INTEL_ASSET" "$RENAMED_CDN/$INTEL_ASSET"
printf '%s  dist/bitty-other-asset\n' "$SIDE_SHA" >"$RENAMED_CDN/$INTEL_ASSET.sha256"
if "$VERIFIER" "file://$RENAMED_CDN" "$INTEL_ASSET" "$TMP/dl-renamed" >/dev/null 2>&1; then
  fail "verifier accepted a sidecar naming another file"
else
  pass "verifier rejects a sidecar naming another file"
fi

# --- Workflow wiring --------------------------------------------------------

for job in aur-bin homebrew scoop; do
  if grep -q 'needs: r2-mirror' <<<"$(job_block "$job")"; then
    pass "release.yml $job waits for r2-mirror"
  else
    fail "release.yml $job does not wait for r2-mirror"
  fi
  if grep -q 'cdn\.bitty\.run' <<<"$(job_block "$job")"; then
    pass "release.yml $job publishes R2/CDN URLs"
  else
    fail "release.yml $job has no R2/CDN URL"
  fi
done

for job in homebrew scoop; do
  if grep -q 'render-package-manifests\.sh' <<<"$(job_block "$job")"; then
    pass "release.yml $job renders through the shared renderer"
  else
    fail "release.yml $job does not use the shared renderer"
  fi
done

BASE_URL_COUNT="$(grep -cF 'BASE_URL="https://cdn.bitty.run/releases/${TAG}"' "$WORKFLOW" || true)"
if [[ "$BASE_URL_COUNT" -eq 3 ]]; then
  pass "release.yml sets the R2/CDN base URL in all three channel jobs"
else
  fail "release.yml has $BASE_URL_COUNT/3 R2/CDN base URLs"
fi
if grep -qF 'BASE_URL="https://github.com/bitty-terminal/bitty/releases/download/' "$WORKFLOW"; then
  fail "release.yml still downloads channel payloads from GitHub"
else
  pass "release.yml no longer uses GitHub as a channel download source"
fi
for needle in 'rendered formula lost the R2/CDN' 'rendered manifest lost the R2/CDN' \
  'scripts/assert-aur-bin-render.sh' \
  'scripts/verify-cdn-sidecar.sh "$BASE_URL" "$ASSET"' \
  'scripts/verify-cdn-sidecar.sh "$BASE_URL" "$INTEL_ASSET"' \
  'scripts/verify-cdn-sidecar.sh "$BASE_URL" "$X64_ASSET"'; do
  expect_contains "$WORKFLOW" "$needle" "release.yml has a publish-time assertion: $needle"
done

for job in homebrew scoop; do
  if grep -qF 'verify-cdn-sidecar.sh' <<<"$(job_block "$job")"; then
    pass "release.yml $job verifies the published sidecar"
  else
    fail "release.yml $job does not verify through the shared verifier"
  fi
done

# --- Documentation ----------------------------------------------------------
expect_contains "$README" 'R2/CDN' "packaging/README.md names the R2/CDN primary source"
expect_contains "$README" 'GitHub Releases' "packaging/README.md documents the GitHub fallback"
expect_contains "$INSTALL" 'R2-backed CDN' "packaging/INSTALL.md names the R2-backed CDN"
expect_contains "$INSTALL" 'GitHub Releases' "packaging/INSTALL.md documents the GitHub fallback"
expect_contains "$TOP_README" 'https://cdn.bitty.run/releases/' \
  "README.md names the CDN release path"

# CTX-1433 (#2364): the sidecar claim must stay scoped to bare binaries and
# archives, with the package/FreeBSD exceptions spelled out.
for doc in "$README" "$INSTALL"; do
  expect_contains "$doc" 'Every bare binary and archive' \
    "$doc scopes the sidecar claim to binaries and archives"
  if grep -qF 'Each artifact ships a `<artifact>.sha256` sidecar' "$doc" ||
    grep -qF 'Each artifact has a `.sha256` sidecar' "$doc"; then
    fail "$doc still claims every artifact has a sidecar"
  else
    pass "$doc no longer claims every artifact has a sidecar"
  fi
done
for needle in '.deb' '.rpm' '.apk' '.pkg.tar.zst' 'bitty-x86_64-unknown-freebsd.sha256'; do
  expect_contains "$README" "$needle" \
    "packaging/README.md names the sidecar exception: $needle"
done
expect_contains "$README" 'scripts/verify-cdn-sidecar.sh' \
  "packaging/README.md names the shared publish-time verifier"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-package-manifests.test: FAIL" >&2
  exit 1
fi
echo "check-package-manifests.test: PASS"
