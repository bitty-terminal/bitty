#!/usr/bin/env bash
# check-nix-patch-stub.test.sh — exercise scripts/check-nix-patch-stub.sh.
#
# The guard protects the #2591 fix: crane's dummy source must never compile a
# crate that the workspace `[patch]` table points at an external dependency.
# These cases pin the guard contract without Nix or network access:
#   1. real patched crates plus `cargoArtifacts = null` pass;
#   2. buildDepsOnly in flake.nix fails (the stub-source cache returns);
#   3. an omitted `cargoArtifacts = null` fails (buildPackage creates one);
#   4. mkDummySrc in flake.nix fails;
#   5. a crane stub lib.rs fails;
#   6. a patch path without src/lib.rs fails;
#   7. no patch table allows dependency caching (nothing is stubbed);
#   8. the real repository passes the guard.
#
# Usage: scripts/tests/check-nix-patch-stub.test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT/scripts/check-nix-patch-stub.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-nix-patch-stub-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

real_lib() { # <path> — larger than a crane stub with a public API
  local file="$1"
  mkdir -p "$(dirname "$file")"
  {
    printf '//! Real workspace crate used by the guard fixture.\n'
    for i in $(seq 1 100); do
      printf 'pub struct Fixture%d;\n' "$i"
    done
  } >"$file"
}

stub_lib() { # <path> — the shape crane's mkDummySrc writes
  local file="$1"
  mkdir -p "$(dirname "$file")"
  cat >"$file" <<'STUB'
#![allow(clippy::all)]
#![allow(dead_code)]

#[allow(unused_extern_crates)]
extern crate core;

pub fn main() {}
STUB
}

write_patch_toml() { # <dir>
  cat >"$1/Cargo.toml" <<'TOML'
[workspace]
members = ["crates/bitty-pty", "crates/bitty-plugin-host"]

[patch."https://github.com/bitty-terminal/bitty"]
bitty-pty = { path = "crates/bitty-pty" }
bitty-plugin-host = { path = "crates/bitty-plugin-host" }
TOML
}

write_flake() { # <dir> <cargoArtifacts line>
  cat >"$1/flake.nix" <<NIX
{
  # buildDepsOnly in a comment is documentation, not a call.
  outputs = { ... }: {
    packages.default = craneLib.buildPackage {
      src = ./.;
      cargoExtraArgs = "-p bitty-terminal";
      $2
    };
  };
}
NIX
}

make_project() { # <dir>
  local dir="$1"
  mkdir -p "$dir"
  write_patch_toml "$dir"
  real_lib "$dir/crates/bitty-pty/src/lib.rs"
  real_lib "$dir/crates/bitty-plugin-host/src/lib.rs"
  write_flake "$dir" "cargoArtifacts = null;"
}

run_guard() { # <project-dir>
  bash "$SCRIPT" --root "$1"
}

expect_pass() { # <label> <dir>
  if run_guard "$2" >"$TMP/out" 2>&1; then
    pass "$1"
  else
    fail "$1"
    cat "$TMP/out" >&2
  fi
}

expect_fail() { # <label> <dir> <needle>
  if run_guard "$2" >"$TMP/out" 2>&1; then
    fail "$1"
  elif grep -qF "$3" "$TMP/out"; then
    pass "$1"
  else
    fail "$1 (missing message: $3)"
    cat "$TMP/out" >&2
  fi
}

# --- 1. The intended shape passes -------------------------------------------
P1="$TMP/ok"
make_project "$P1"
expect_pass "real patched crates plus cargoArtifacts = null pass" "$P1"

# --- 2. Dependency-only caching returns -> fail -----------------------------
P2="$TMP/build-deps-only"
make_project "$P2"
write_flake "$P2" "cargoArtifacts = craneLib.buildDepsOnly commonArgs;"
expect_fail "buildDepsOnly fails" "$P2" "calls buildDepsOnly"

# --- 3. Omitting the null override -> fail ----------------------------------
P3="$TMP/implicit-cache"
make_project "$P3"
write_flake "$P3" ""
expect_fail "omitted cargoArtifacts = null fails" "$P3" "cargoArtifacts = null"

# --- 4. Explicit dummy source -> fail ---------------------------------------
P4="$TMP/dummy-src"
make_project "$P4"
write_flake "$P4" "cargoArtifacts = null; dummySrc = craneLib.mkDummySrc {};"
expect_fail "mkDummySrc fails" "$P4" "calls mkDummySrc"

# --- 5. A committed stub crate -> fail --------------------------------------
P5="$TMP/stub-crate"
make_project "$P5"
stub_lib "$P5/crates/bitty-pty/src/lib.rs"
expect_fail "a crane stub crate fails" "$P5" "crane dummy stub"

# --- 6. A patch path without a library -> fail -------------------------------
P6="$TMP/missing-lib"
make_project "$P6"
rm "$P6/crates/bitty-plugin-host/src/lib.rs"
expect_fail "a patch path without src/lib.rs fails" "$P6" "without src/lib.rs"

# --- 7. No patch table -> dependency caching is allowed ---------------------
P7="$TMP/no-patch"
mkdir -p "$P7/crates"
cat >"$P7/Cargo.toml" <<'TOML'
[workspace]
members = ["crates/bitty-pty"]
TOML
cat >"$P7/flake.nix" <<'NIX'
{
  outputs = { ... }: {
    packages.default = craneLib.buildPackage {
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    };
  };
}
NIX
expect_pass "no patch table allows dependency caching" "$P7"

# --- 8. The real repository passes ------------------------------------------
expect_pass "the repository itself passes" "$ROOT"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-nix-patch-stub.test: FAIL" >&2
  exit 1
fi
echo "check-nix-patch-stub.test: PASS"
