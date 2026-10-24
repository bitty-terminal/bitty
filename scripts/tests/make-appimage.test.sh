#!/usr/bin/env bash
# make-appimage.test.sh — fixture test for scripts/make-appimage.sh.
#
# Validates argument checks, dry-run staging, AppDir hierarchy, desktop file
# placement, icon integration, the AppRun launcher, and checksum generation.
# The AppImage tool is an explicit mock injected through APPIMAGETOOL, so the
# test never resolves a host `appimagetool`, a cached copy, or the network
# (issue #2566).
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

# 3. Mock toolchain. The mock lives outside PATH and is injected through
#    APPIMAGETOOL. A same-named stub inside PATH records any bypass, and the
#    curl stub makes a download fallback fail without touching the network.
STUB_BIN="$TMP_BASE/bin"
MOCK_TOOL="$TMP_BASE/mock-appimagetool"
MOCK_MARKER="$TMP_BASE/mock-appimagetool.calls"
BYPASS_MARKER="$TMP_BASE/path-appimagetool.calls"
CURL_MARKER="$TMP_BASE/curl.calls"
: >"$MOCK_MARKER"
mkdir -p "$STUB_BIN"

cat <<'EOF' >"$MOCK_TOOL"
#!/bin/sh
set -eu
: "${MOCK_MARKER:?MOCK_MARKER must be set}"
printf 'invoked\n' >>"$MOCK_MARKER"

test "$#" -eq 2 || { echo "expected APPDIR and OUTPUT, got $# args" >&2; exit 10; }
test "${APPIMAGE_EXTRACT_AND_RUN:-}" = "1" || { echo "APPIMAGE_EXTRACT_AND_RUN is not 1" >&2; exit 11; }
test "${ARCH:-}" = "x86_64" || { echo "ARCH is not x86_64" >&2; exit 12; }

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

# Emit a mock AppImage with a recognizable payload: the test proves the output
# came from this mock, not from a host, cached, or downloaded appimagetool.
printf '#!/bin/sh\necho "bitty-appimage-mock"\n' >"$OUTPUT"
chmod +x "$OUTPUT"
EOF
chmod +x "$MOCK_TOOL"

cat <<'EOF' >"$STUB_BIN/appimagetool"
#!/bin/sh
set -eu
: >"${BYPASS_MARKER:?BYPASS_MARKER must be set}"
echo "PATH appimagetool stub must not be used" >&2
exit 90
EOF
chmod +x "$STUB_BIN/appimagetool"

cat <<'EOF' >"$STUB_BIN/curl"
#!/bin/sh
set -eu
: >"${CURL_MARKER:?CURL_MARKER must be set}"
echo "appimagetool download is forbidden in this test" >&2
exit 91
EOF
chmod +x "$STUB_BIN/curl"

run_script() {
  local tool="$1"
  shift
  PATH="$STUB_BIN:$PATH" \
    APPIMAGETOOL="$tool" \
    MOCK_MARKER="$MOCK_MARKER" \
    BYPASS_MARKER="$BYPASS_MARKER" \
    CURL_MARKER="$CURL_MARKER" \
    "$SCRIPT" "$@"
}

marker_count() {
  wc -l <"$MOCK_MARKER" | tr -d ' '
}

# 4. Dry-run staging validates without creating output and without the tool
if run_script "$MOCK_TOOL" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" --dry-run >/dev/null 2>&1; then
  if [[ -e "$OUT" ]]; then
    fail "dry-run wrote output"
  else
    pass "dry-run validates and stages without writing bundle"
  fi
else
  fail "dry-run exited non-zero"
fi
if [[ "$(marker_count)" -eq 0 ]]; then
  pass "dry-run did not invoke appimagetool"
else
  fail "dry-run invoked appimagetool"
fi

# 5. Real assembly must invoke the explicit mock
if run_script "$MOCK_TOOL" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$OUT" >/dev/null 2>&1; then
  pass "assembly with the explicit appimagetool mock succeeded"
else
  fail "assembly with the explicit appimagetool mock failed"
fi

if [[ "$(marker_count)" -gt 0 ]]; then
  pass "mock invocation marker recorded"
else
  fail "appimagetool mock was bypassed (no invocation marker)"
fi
if [[ -e "$BYPASS_MARKER" ]]; then
  fail "script resolved the PATH appimagetool instead of the APPIMAGETOOL mock"
else
  pass "script did not resolve a PATH appimagetool"
fi
if [[ -e "$CURL_MARKER" ]]; then
  fail "script attempted to download appimagetool"
else
  pass "script did not attempt a download"
fi

[[ -f "$OUT" ]] || fail "output AppImage missing"
[[ -x "$OUT" ]] || fail "output AppImage not executable"
if grep -q 'bitty-appimage-mock' "$OUT" 2>/dev/null; then
  pass "output payload came from the mock"
else
  fail "output payload did not come from the mock"
fi
[[ -f "$OUT.sha256" ]] || fail "sha256 sidecar missing"
pass "AppImage bundle and sha256 sidecar emitted"

# 6. An unusable explicit tool path fails closed: no PATH, cache, or network fallback
NEGATIVE_OUT="$TMP_BASE/bitty-negative.AppImage"
MARKER_BEFORE="$(marker_count)"
if run_script "$TMP_BASE/absent-appimagetool" --version "$VERSION" --target "$TARGET" \
  --binary "$FAKE_BIN" --output "$NEGATIVE_OUT" >/dev/null 2>&1; then
  fail "unusable APPIMAGETOOL override was accepted"
else
  pass "unusable APPIMAGETOOL override fails closed"
fi
if [[ "$(marker_count)" -eq "$MARKER_BEFORE" ]]; then
  pass "fail-closed run did not invoke appimagetool"
else
  fail "fail-closed run invoked appimagetool"
fi
if [[ -e "$BYPASS_MARKER" ]]; then
  fail "fail-closed run fell back to a PATH appimagetool"
else
  pass "fail-closed run did not fall back to a PATH appimagetool"
fi
if [[ -e "$CURL_MARKER" ]]; then
  fail "fail-closed run attempted a download"
else
  pass "fail-closed run did not attempt a download"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "make-appimage.test: FAIL" >&2
  exit 1
fi

echo "make-appimage.test: PASS"
