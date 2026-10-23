#!/usr/bin/env bash
# check-nix-lock.test.sh — exercise scripts/check-nix-lock.sh with a stub nix.
#
# The guard itself needs a real Nix installation (CI installs one through
# cachix/install-nix-action); this test pins its contract instead of its
# network behavior:
#   1. a well-formed lock plus a successful resolve/eval passes;
#   2. a placeholder narHash fails before nix is invoked at all;
#   3. a failing `nix flake metadata` (hash mismatch) fails the guard;
#   4. a failing `nix eval` (stale rev/lastModified metadata) fails the guard;
#   5. a missing nix binary fails the guard instead of silently passing;
#   6. a lock file rewritten while resolving the flake fails the guard.
#
# Usage: scripts/tests/check-nix-lock.test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT/scripts/check-nix-lock.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-nix-lock-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

# --- stub nix ----------------------------------------------------------------
BIN="$TMP/bin"
mkdir -p "$BIN"
cat >"$BIN/nix" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${STUB_LOG:?}"
# Drop the global experimental-features flags; the guard always passes them.
while [[ ${1:-} == --* ]]; do
	shift
	[[ ${1:-} == "nix-command flakes" ]] && shift
done
case "${1:-}" in
flake)
	if [[ -n "${STUB_METADATA_EXIT:-}" ]]; then
		echo "error: NAR hash mismatch in input 'github:NixOS/nixpkgs/...'" >&2
		exit "$STUB_METADATA_EXIT"
	fi
	if [[ -n "${STUB_METADATA_REWRITE:-}" ]]; then
		printf '\n' >>"${STUB_PROJECT:?}/flake.lock"
	fi
	cat <<'JSON'
{"locks":{"nodes":{"nixpkgs":{"locked":{"rev":"5adc50831c259f87faeb09ddf930d43597197473"}}},"root":"root"},"version":7}
JSON
	;;
eval)
	if [[ -n "${STUB_EVAL_EXIT:-}" ]]; then
		echo "error: mismatch in field 'lastModified' of input '{...}'" >&2
		exit "$STUB_EVAL_EXIT"
	fi
	printf '/nix/store/stub-bitty.drv'
	;;
*)
	echo "stub nix: unexpected invocation: $*" >&2
	exit 64
	;;
esac
STUB
chmod +x "$BIN/nix"

# A minimal project: the stub never reads flake.nix, but the guard requires it
# to be a flake root and the real command would.
make_project() { # <dir> <narHash>
  local dir="$1" hash="$2"
  mkdir -p "$dir"
  printf '{ description = "stub"; }\n' >"$dir/flake.nix"
  cat >"$dir/flake.lock" <<JSON
{
  "nodes": {
    "nixpkgs": {
      "locked": {
        "lastModified": 1792506237,
        "narHash": "$hash",
        "owner": "NixOS",
        "repo": "nixpkgs",
        "rev": "5adc50831c259f87faeb09ddf930d43597197473",
        "type": "github"
      },
      "original": {
        "owner": "NixOS",
        "ref": "nixpkgs-unstable",
        "repo": "nixpkgs",
        "type": "github"
      }
    },
    "root": { "inputs": { "nixpkgs": "nixpkgs" } }
  },
  "root": "root",
  "version": 7
}
JSON
}

run_guard() { # <project-dir> [PATH]
  local dir="$1"
  PATH="${2:-$BIN:/usr/bin:/bin}" STUB_LOG="$TMP/stub.log" \
    bash "$SCRIPT" --root "$dir"
}

GOOD_HASH="sha256-Q/8KLB6jNfpaUN1HAedE/u/Et9feE/zYSsutBLKrCFk="

# --- 1. Resolvable, well-formed lock passes ---------------------------------
P1="$TMP/ok"
make_project "$P1" "$GOOD_HASH"
: >"$TMP/stub.log"
if run_guard "$P1" >"$TMP/out1" 2>&1 && grep -q "check-nix-lock: PASS" "$TMP/out1"; then
  pass "well-formed lock resolves and evaluates"
else
  fail "well-formed lock resolves and evaluates"
  cat "$TMP/out1" >&2
fi
if grep -q "flake metadata" "$TMP/stub.log" && grep -q "eval" "$TMP/stub.log"; then
  pass "guard invokes flake metadata and eval"
else
  fail "guard invokes flake metadata and eval"
  cat "$TMP/stub.log" >&2
fi

# --- 2. Placeholder hash fails before nix runs ------------------------------
P2="$TMP/placeholder"
make_project "$P2" "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
: >"$TMP/stub.log"
if run_guard "$P2" >"$TMP/out2" 2>&1; then
  fail "placeholder narHash is rejected"
elif grep -q "placeholder narHash" "$TMP/out2" && [[ ! -s "$TMP/stub.log" ]]; then
  pass "placeholder narHash is rejected before invoking nix"
else
  fail "placeholder narHash is rejected before invoking nix"
  cat "$TMP/out2" >&2
fi

# --- 3. Hash mismatch reported by metadata fails ----------------------------
P3="$TMP/metadata-fail"
make_project "$P3" "$GOOD_HASH"
if STUB_METADATA_EXIT=102 run_guard "$P3" >"$TMP/out3" 2>&1; then
  fail "metadata hash mismatch fails the guard"
elif grep -q "does not resolve" "$TMP/out3"; then
  pass "metadata hash mismatch fails the guard"
else
  fail "metadata hash mismatch fails the guard"
  cat "$TMP/out3" >&2
fi

# --- 4. Stale rev/lastModified metadata reported by eval fails --------------
P4="$TMP/eval-fail"
make_project "$P4" "$GOOD_HASH"
if STUB_EVAL_EXIT=1 run_guard "$P4" >"$TMP/out4" 2>&1; then
  fail "evaluation metadata mismatch fails the guard"
elif grep -q "flake evaluation rejects flake.lock" "$TMP/out4"; then
  pass "evaluation metadata mismatch fails the guard"
else
  fail "evaluation metadata mismatch fails the guard"
  cat "$TMP/out4" >&2
fi

# --- 5. Missing nix fails closed --------------------------------------------
P5="$TMP/no-nix"
make_project "$P5" "$GOOD_HASH"
# /usr/bin and /bin on the runners have no nix; keep bash/coreutils reachable.
NO_NIX_PATH="$(dirname "$(command -v bash)"):/usr/bin:/bin"
if PATH="$NO_NIX_PATH" command -v nix >/dev/null 2>&1; then
  pass "missing-nix case skipped (nix is installed outside the stub path)"
elif run_guard "$P5" "$NO_NIX_PATH" >"$TMP/out5" 2>&1; then
  fail "missing nix fails closed"
elif grep -q "nix is required" "$TMP/out5"; then
  pass "missing nix fails closed"
else
  fail "missing nix fails closed"
  cat "$TMP/out5" >&2
fi

# --- 6. A rewritten flake.lock is rejected, not silently accepted -----------
P6="$TMP/rewrite"
make_project "$P6" "$GOOD_HASH"
git -C "$P6" init -q
git -C "$P6" add flake.nix flake.lock
git -C "$P6" -c user.email=guard@example.invalid -c user.name=guard commit -qm init
if STUB_METADATA_REWRITE=1 STUB_PROJECT="$P6" run_guard "$P6" >"$TMP/out6" 2>&1; then
  fail "a rewritten flake.lock fails the guard"
elif grep -q "rewrote flake.lock" "$TMP/out6"; then
  pass "a rewritten flake.lock fails the guard"
else
  fail "a rewritten flake.lock fails the guard"
  cat "$TMP/out6" >&2
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-nix-lock.test: FAIL" >&2
  exit 1
fi
echo "check-nix-lock.test: PASS"
