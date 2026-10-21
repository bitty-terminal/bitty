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

# --- Workflow wiring --------------------------------------------------------
job_block() { # <job>
  sed -n "/^  $1:/,/^  [a-z0-9-]*:/p" "$WORKFLOW"
}

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
  'rendered PKGBUILD does not use the R2/CDN' \
  'rendered PKGBUILD does not carry the verified sha256' \
  'bitty-bin-asset' 'ACTUAL_SHA="$(sha256sum' \
  'fetch_verified "$INTEL_ASSET"' 'fetch_verified "$X64_ASSET"'; do
  expect_contains "$WORKFLOW" "$needle" "release.yml has a publish-time assertion: $needle"
done

# --- Documentation ----------------------------------------------------------
expect_contains "$README" 'R2/CDN' "packaging/README.md names the R2/CDN primary source"
expect_contains "$README" 'GitHub Releases' "packaging/README.md documents the GitHub fallback"
expect_contains "$INSTALL" 'R2-backed CDN' "packaging/INSTALL.md names the R2-backed CDN"
expect_contains "$INSTALL" 'GitHub Releases' "packaging/INSTALL.md documents the GitHub fallback"
expect_contains "$TOP_README" 'https://cdn.bitty.run/releases/' \
  "README.md names the CDN release path"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-package-manifests.test: FAIL" >&2
  exit 1
fi
echo "check-package-manifests.test: PASS"
