#!/usr/bin/env bash
# check-terminfo.test.sh — CTX-1121 fixture test for the audited terminfo gate.
#
# Drives scripts/check-terminfo.sh against synthetic repository roots so the
# tic-syntax + audited-flag + no-aspirational-cap decisions are exercised
# without touching the checkout. Requires tic/infocmp (ncurses) like the gate.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-terminfo.sh
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# make_root <dir>: minimal tree with the paths the gate inspects, seeded with
# a valid bitty.ti (copied from the real entry) and tic-based AUR recipes.
make_root() {
  local root="$1"
  mkdir -p "$root/terminfo" "$root/packaging" "$root/.github/workflows"
  cp ./terminfo/bitty.ti "$root/terminfo/bitty.ti"
  : >"$root/terminfo/README.md"
  for f in "$root/packaging/PKGBUILD" "$root/packaging/PKGBUILD.bin"; do
    printf 'tic -x -o "$pkgdir/usr/share/terminfo" terminfo/bitty.ti\n' >"$f"
  done
  : >"$root/nfpm.yaml"
  : >"$root/.github/workflows/release.yml"
}

# expect_exit <exit> <label> <gate args...>
expect_exit() {
  local want="$1" label="$2"
  shift 2
  local out status=0
  out="$("$GATE" "$@" 2>&1)" || status=$?
  if ((status != want)); then
    echo "FAIL: $label: expected exit $want, got $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# expect_fail_tag <tag> <label> <gate args...>
expect_fail_tag() {
  local tag="$1" label="$2"
  shift 2
  local out status=0
  out="$("$GATE" "$@" 2>&1)" || status=$?
  if ((status == 0)); then
    echo "FAIL: $label: gate passed but exit $tag was expected" >&2
    FAIL=1
  elif ! grep -qF "$tag" <<<"$out"; then
    echo "FAIL: $label: missing tag '$tag'" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

# 1. The real repository passes.
expect_exit 0 'real repository' --root .

# 2. Valid synthetic root passes.
root="$TMP/valid"
make_root "$root"
expect_exit 0 'valid synthetic root' --root "$root"

# 3. The retired placeholder file fails.
root="$TMP/placeholder"
make_root "$root"
printf '# dummy terminfo for nfpm validation\n' >"$root/terminfo/bitty.terminfo"
expect_fail_tag 'placeholder is retired' 'retired placeholder file' --root "$root"

# 4. A non-markdown file carrying the marker fails.
root="$TMP/marker"
make_root "$root"
printf '# dummy terminfo\n' >>"$root/terminfo/bitty.ti"
expect_fail_tag 'retired placeholder marker' 'placeholder marker in source' --root "$root"

# 5. Missing bitty.ti fails.
root="$TMP/missing"
make_root "$root"
rm "$root/terminfo/bitty.ti"
expect_fail_tag 'bitty.ti missing' 'missing bitty.ti' --root "$root"

# 6. Missing use=xterm-256color fails.
root="$TMP/nouse"
make_root "$root"
grep -v 'use=xterm-256color' "$root/terminfo/bitty.ti" >"$root/terminfo/bitty.ti.tmp"
mv "$root/terminfo/bitty.ti.tmp" "$root/terminfo/bitty.ti"
expect_fail_tag 'use=xterm-256color' 'missing use line' --root "$root"

# 7. Missing audited cap (drop Tc) fails the round-trip.
root="$TMP/notc"
make_root "$root"
grep -v '^[[:space:]]*Tc,' "$root/terminfo/bitty.ti" >"$root/terminfo/bitty.ti.tmp"
mv "$root/terminfo/bitty.ti.tmp" "$root/terminfo/bitty.ti"
expect_fail_tag "audited capability 'Tc'" 'missing Tc' --root "$root"

# 8. Aspirational Ms fails.
root="$TMP/ms"
make_root "$root"
printf '\tMs=\\E]52;%%p1%%s;%%p2%%s\\007,\n' >>"$root/terminfo/bitty.ti"
expect_fail_tag 'aspirational capability Ms' 'aspirational Ms' --root "$root"

# 9. Aspirational fullkbd fails.
root="$TMP/fullkbd"
make_root "$root"
printf '\tfullkbd,\n' >>"$root/terminfo/bitty.ti"
expect_fail_tag 'aspirational capability fullkbd' 'aspirational fullkbd' --root "$root"

# 10. A recipe installing the raw .ti source fails.
root="$TMP/rawinstall"
make_root "$root"
printf 'install -Dm644 terminfo/bitty.ti "$pkgdir/usr/share/terminfo/b/bitty"\n' \
  >>"$root/packaging/PKGBUILD"
expect_fail_tag 'raw terminfo/bitty.ti source' 'raw-source install' --root "$root"

# 11. A recipe without tic -x fails.
root="$TMP/notic"
make_root "$root"
: >"$root/packaging/PKGBUILD"
expect_fail_tag "must compile terminfo with 'tic -x'" 'missing tic step' --root "$root"

# 12. A workflow recreating the retired path fails.
root="$TMP/workflow"
make_root "$root"
printf 'echo "# dummy" > terminfo/bitty.terminfo\n' >"$root/.github/workflows/release.yml"
expect_fail_tag 'retired placeholder path' 'workflow recreates dummy' --root "$root"

# 13. Markdown may name the marker to document the decision.
root="$TMP/readme"
make_root "$root"
printf 'The retired "dummy terminfo" placeholder was removed.\n' >"$root/terminfo/README.md"
expect_exit 0 'markdown may document the marker' --root "$root"

if ((FAIL)); then
  echo "check-terminfo-test: FAIL" >&2
  exit 1
fi
echo "check-terminfo-test: OK"
