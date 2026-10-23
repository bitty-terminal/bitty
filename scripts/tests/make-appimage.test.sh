#!/usr/bin/env bash
# make-appimage.test.sh — fixture test for scripts/make-appimage.sh.
#
# Validates argument checks, dry-run staging, AppDir hierarchy, desktop file
# placement, icon integration, AppRun executable launcher, and checksum
# generation using a mock tool (no network access required).
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT="./scripts/make-appimage.sh"
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/make-appimage-test.XXXXXX")"
cleanup() {
  rm -rf "$TMP_BASE"
}
trap cleanup EXIT

VERSION="0.0.99"
TARGET="x86_64-unknown-linux-gnu"

# 1. Ephemeral fake binary
FAKE_BIN="$TMP_BASE/bitty-fixture"
printf '#!/bin/sh\necho "bitty %s"\n' "$VERSION" >"$FAKE_BIN"
chmod +x "$FAKE_BIN"

OUT="$TMP_BASE/bitty-$VERSION-$TARGET.AppImage"

# 2. Argument validation failures
if "$SCRIPT" >/dev/null 2>&1; then
  fail "empty args accepted"
else
  pass "empty args rejected"
fi

if "$SCRIPT" --version "bad" --target "$TARGET" --binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
  fail "invalid version accepted"
else
  pass "invalid version rejected"
fi

if "$SCRIPT" --version "$VERSION" --target "unknown-os" --binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
  fail "unsupported target accepted"
else
  pass "unsupported target rejected"
fi

if "$SCRIPT" --version "$VERSION" --target "$TARGET" --binary "$TMP_BASE/nonexistent" --output "$OUT" >/dev/null 2>&1; then
  fail "nonexistent binary accepted"
else
  pass "nonexistent binary rejected"
fi

# 3. Dry-run staging validates without creating output
if "$SCRIPT" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" --dry-run >/dev/null 2>&1; then
  if [[ -e "$OUT" ]]; then
    fail "dry-run wrote output"
  else
    pass "dry-run validates and stages without writing bundle"
  fi
else
  fail "dry-run exited non-zero"
fi

# 4. Mock appimagetool to verify packaging invocation and AppDir content
MOCK_TOOL="$TMP_BASE/mock-appimagetool"
cat <<'EOF' >"$MOCK_TOOL"
#!/bin/sh
set -eu
APPDIR="$1"
OUTPUT="$2"

# Assert critical AppDir structure
test -f "$APPDIR/AppRun" || { echo "missing AppRun" >&2; exit 2; }
test -x "$APPDIR/AppRun" || { echo "AppRun not executable" >&2; exit 3; }
test -f "$APPDIR/run.bitty.Bitty.desktop" || { echo "missing root desktop file" >&2; exit 4; }
test -f "$APPDIR/bitty.png" || { echo "missing root bitty.png" >&2; exit 5; }
test -f "$APPDIR/usr/bin/bitty" || { echo "missing usr/bin/bitty" >&2; exit 6; }
test -x "$APPDIR/usr/bin/bitty" || { echo "usr/bin/bitty not executable" >&2; exit 7; }
test -f "$APPDIR/usr/share/metainfo/run.bitty.Bitty.metainfo.xml" || { echo "missing metainfo" >&2; exit 8; }
test -f "$APPDIR/usr/share/applications/run.bitty.Bitty.desktop" || { echo "missing applications desktop" >&2; exit 9; }

# Emit mock AppImage
printf '#!/bin/sh\necho "bitty-appimage"\n' > "$OUTPUT"
chmod +x "$OUTPUT"
EOF
chmod +x "$MOCK_TOOL"

PATH="$TMP_BASE:$PATH"
export PATH

# Run make-appimage with mock tool in PATH
if ! "$SCRIPT" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
  fail "mock assembly exited non-zero"
else
  pass "assembly with appimagetool succeeded"
fi

[[ -f "$OUT" ]] || fail "output AppImage missing"
[[ -x "$OUT" ]] || fail "output AppImage not executable"
[[ -f "$OUT.sha256" ]] || fail "sha256 sidecar missing"
pass "AppImage bundle and sha256 sidecar emitted"

if [[ "$FAIL" -ne 0 ]]; then
  echo "make-appimage.test: FAIL" >&2
  exit 1
fi

echo "make-appimage.test: PASS"
