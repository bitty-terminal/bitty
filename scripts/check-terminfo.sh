#!/usr/bin/env bash
# check-terminfo.sh — guard the audited bitty terminfo entry (CTX-1121, #1764).
#
# Canonical source is terminfo/bitty.ti (use=xterm-256color + audited Tc,
# Su/Setulc/Smulx, Sync, Hls, pairs#32767, Ms@ cancel). This gate fails on
# aspirational drift and on packaging regressions:
#   1. the retired placeholder path terminfo/bitty.terminfo must not exist;
#   2. no file under terminfo/ (markdown excepted) may carry the placeholder
#      marker;
#   3. terminfo/bitty.ti must exist, use xterm-256color, cancel inherited Ms
#      with Ms@, contain no aspirational Ms/fullkbd/RGB claims, compile with
#      `tic -x`, and round-trip via `infocmp -x bitty` with the audited flags
#      and without Ms (compiled check catches use-chain inheritance);
#   4. packaging inputs must not reference the retired placeholder path and
#      must not install the raw .ti source to /usr/share/terminfo (only the
#      tic-compiled entry via `tic -x -o .../terminfo`); AUR recipes must run
#      tic -x on terminfo/bitty.ti.
#
# Usage: scripts/check-terminfo.sh [--root DIR]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo "Usage: scripts/check-terminfo.sh [--root DIR]"
}

while (($#)); do
  case "$1" in
  --root)
    [[ $# -ge 2 ]] || {
      echo "check-terminfo: --root needs a value" >&2
      exit 2
    }
    ROOT="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "check-terminfo: unknown argument: $1" >&2
    usage >&2
    exit 2
    ;;
  esac
done

fail() {
  echo "check-terminfo: FAIL: $1" >&2
  exit 1
}

# 1. The placeholder path stays retired (CTX-0451).
[[ -e "$ROOT/terminfo/bitty.terminfo" ]] &&
  fail "terminfo/bitty.terminfo exists: the placeholder is retired (CTX-0451); ship the tic-compiled bitty.ti entry or nothing"

# 2. No placeholder marker in terminfo sources (markdown may document it).
while IFS= read -r file; do
  if [[ "$file" != *.md ]] && grep -q 'dummy terminfo' "$file"; then
    fail "$file carries the retired placeholder marker (dummy terminfo)"
  fi
done < <(find "$ROOT/terminfo" -type f 2>/dev/null)

TI="$ROOT/terminfo/bitty.ti"
[[ -f "$TI" ]] || fail "terminfo/bitty.ti missing (CTX-1121 canonical source)"

# 3a. Must base on xterm-256color and cancel inherited Ms (default-deny clipboard).
grep -q 'use=xterm-256color' "$TI" ||
  fail "terminfo/bitty.ti must contain use=xterm-256color"
grep -qE '^[[:space:]]*Ms@[,:]?' "$TI" ||
  fail "terminfo/bitty.ti must cancel inherited Ms with 'Ms@' (xterm-256color advertises OSC 52 clipboard; bitty policy is default-deny)"

# 3b. No aspirational caps in the source (ignore # comments).
# Runs before tic so an explicit Ms=/fullkbd/RGB claim fails here with the
# aspirational message; the compiled Ms= check below catches use-chain
# inheritance even when the source omits Ms.
# Strip comment lines before matching so prose mentioning Ms/RGB does not trip.
SRC_NOCOMMENTS="$(grep -v '^[[:space:]]*#' "$TI")"
aspirational() {
  local pattern="$1" label="$2"
  if grep -qE "$pattern" <<<"$SRC_NOCOMMENTS"; then
    fail "terminfo/bitty.ti claims aspirational capability $label; remove it (see terminfo/README.md exclusion notes)"
  fi
}
aspirational '^[[:space:]]*Ms[=,]' 'Ms (OSC 52 clipboard, PARTIAL default-deny)'
aspirational '^[[:space:]]*fullkbd[=,]?[[:space:]]*$|fullkbd,' 'fullkbd (legacy Kitty DEC 2017, ABSENT)'
aspirational '^[[:space:]]*RGB[=,]?[[:space:]]*$|^[[:space:]]*RGB,' 'RGB (direct geometry; Tc covers the proven path)'

# 3c. tic must exist (ncurses); report clearly when absent.
command -v tic >/dev/null 2>&1 ||
  fail "tic not found (ncurses missing); install ncurses to compile terminfo/bitty.ti"
command -v infocmp >/dev/null 2>&1 ||
  fail "infocmp not found (ncurses missing); install ncurses to verify terminfo/bitty.ti"

# 3d. Must compile with tic -x and round-trip via infocmp -x.
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
tic -x -o "$TMPDIR" "$TI" 2>"$TMPDIR/tic.err" ||
  fail "tic -x failed on terminfo/bitty.ti: $(cat "$TMPDIR/tic.err")"
# `-1` prints one capability per line so each probe can be anchored to a
# capability line. The header comment still embeds the random TERMINFO path,
# so drop it before matching (#2320): a path containing "Tc" must never
# satisfy the audited-capability checks.
INFO="$(TERMINFO="$TMPDIR" infocmp -1 -x bitty 2>"$TMPDIR/infocmp.err")" ||
  fail "infocmp -1 -x bitty failed after tic: $(cat "$TMPDIR/infocmp.err")"
CAPS="$(sed '/^#/d' <<<"$INFO")"

has_cap() {
  local name="$1"
  grep -qE "^[[:space:]]*${name}([=#,]|\$)" <<<"$CAPS"
}

# 3e. Audited flags must survive the round-trip.
for cap in 'Tc' 'Su' 'Smulx' 'Setulc' 'Sync' 'Hls'; do
  has_cap "$cap" ||
    fail "compiled bitty entry missing audited capability '$cap' (tic/infocmp round-trip)"
done
# pairs#32767 compiles as pairs#0x7fff.
has_cap 'pairs#0x7fff' ||
  fail "compiled bitty entry missing pairs#32767 (pairs#0x7fff)"
has_cap 'colors#0x100' ||
  fail "compiled bitty entry missing colors#256 (colors#0x100, inherited from xterm-256color)"
# Inherited Ms must stay cancelled: the compiled entry must not contain Ms=
# (use Ms= with equals so the Ms@ cancellation marker does not trip).
if grep -qE '^[[:space:]]*Ms[=,]' <<<"$CAPS"; then
  fail "compiled bitty entry contains Ms (OSC 52 clipboard, inherited from xterm-256color); cancel it in terminfo/bitty.ti with 'Ms@'"
fi

# 4. Packaging: no retired path, no raw-source install; AUR must run tic -x.
CONFIGS=(
  "$ROOT/packaging/PKGBUILD"
  "$ROOT/packaging/PKGBUILD.bin"
  "$ROOT/nfpm.yaml"
)
while IFS= read -r workflow; do
  CONFIGS+=("$workflow")
done < <(find "$ROOT/.github/workflows" -type f -name '*.yml' 2>/dev/null)

for config in "${CONFIGS[@]}"; do
  [[ -f "$config" ]] || continue
  rel="${config#"$ROOT"/}"
  if grep -q 'terminfo/bitty\.terminfo' "$config"; then
    fail "$rel references the retired placeholder path terminfo/bitty.terminfo"
  fi
  # Raw-source install: an `install ...` line placing the .ti under terminfo.
  if grep -E '^[[:space:]]*install[[:space:]].*terminfo/bitty\.ti.*usr/share/terminfo' "$config" >/dev/null; then
    fail "$rel installs the raw terminfo/bitty.ti source to /usr/share/terminfo; install only the tic-compiled entry (tic -x -o .../terminfo terminfo/bitty.ti)"
  fi
done

for recipe in "$ROOT/packaging/PKGBUILD" "$ROOT/packaging/PKGBUILD.bin"; do
  rel="${recipe#"$ROOT"/}"
  [[ -f "$recipe" ]] || continue
  grep -q 'tic -x' "$recipe" ||
    fail "$rel must compile terminfo with 'tic -x' (CTX-1121 AUR flow)"
  grep -q 'terminfo/bitty\.ti' "$recipe" ||
    fail "$rel must reference terminfo/bitty.ti in its tic -x step"
  if grep -q 'usr/share/terminfo' "$recipe" && ! grep -q 'tic -x.*usr/share/terminfo' "$recipe"; then
    fail "$rel touches /usr/share/terminfo without tic -x; only tic-compiled installs are allowed"
  fi
done

# nFPM must map the staged compiled entry (tic -x -o ./target/terminfo).
if [[ -f "$ROOT/nfpm.yaml" ]]; then
  grep -q 'target/terminfo/b/bitty' "$ROOT/nfpm.yaml" ||
    fail "nfpm.yaml must map the staged compiled entry './target/terminfo/b/bitty' (run tic -x -o ./target/terminfo terminfo/bitty.ti before nfpm package)"
  grep -q '/usr/share/terminfo/b/bitty' "$ROOT/nfpm.yaml" ||
    fail "nfpm.yaml must install the compiled entry to '/usr/share/terminfo/b/bitty'"
fi

echo "check-terminfo: PASS (bitty.ti compiles with tic -x; audited flags Tc/Su/Setulc/Smulx/Sync/Hls present, no aspirational drift)"
