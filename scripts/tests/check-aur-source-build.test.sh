#!/usr/bin/env bash
# check-aur-source-build.test.sh — negative + wiring cases for the AUR source
# build gate (bitty#3011).
#
# `scripts/check-aur-source-build.sh` is only worth its runtime if it fails
# when the recipe is broken, so this test drives it against mutated copies of
# packaging/PKGBUILD and requires exit 1:
#
#   - package() drops the `bittyd` install  -> gate must reject
#     (the #2999 failure class: build() produces a daemon the package never
#     ships, or installs a file the build never produced)
#   - build() drops `-p bitty-daemon`       -> gate must reject
#     (bittyd is a separate workspace member; a build selection that omits it
#     leaves package() copying a file cargo never emitted)
#   - package() drops the `bitty` install   -> gate must reject
#
# An untouched-recipe control run must pass first: without it, a broken
# toolchain (missing fakeroot, unwritable HOME, wrong user) would make every
# "expect reject" case green for the wrong reason.
#
# The runtime cases need Arch tooling (makepkg, fakeroot, tic, bsdtar). They
# run in the `aur-source-build` CI job (archlinux container, PR-reachable).
# Where the tooling is absent (ubuntu quality job) those cases SKIP loudly and
# only the static/workflow-wiring assertions run, mirroring
# check-aur-bin-render.test.sh.
set -euo pipefail

cd "$(dirname "$0")/../.."

GATE=./scripts/check-aur-source-build.sh
RECIPE=./packaging/PKGBUILD
CI_WORKFLOW=./.github/workflows/ci.yml
RELEASE_WORKFLOW=./.github/workflows/release.yml
FAIL=0
# Kept well under the workflow's timeout-minutes so a hung makepkg fails this
# test instead of stalling the job.
GATE_TIMEOUT=600

# makepkg refuses to run as root, so container CI (root) re-executes this test
# as the same throwaway `builder` account the gate uses, keeping the runtime
# cases real instead of skipping them where they matter most.
if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${AUR_GATE_TEST_AS_BUILDER:-}" ]]; then
  id builder >/dev/null 2>&1 || useradd --create-home builder ||
    { echo "check-aur-source-build.test: FAIL: cannot create builder" >&2; exit 1; }
  printf -v cmd 'cd %q && AUR_GATE_TEST_AS_BUILDER=1 bash %q' "$PWD" "$PWD/scripts/tests/check-aur-source-build.test.sh"
  rc=0
  su builder -s /bin/bash -c "$cmd" || rc=$?
  exit "$rc"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-aur-source-build.XXXXXX")"
chmod 755 "$TMP"
trap 'rm -rf "$TMP"' EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

# --- Static assertions ------------------------------------------------------
[[ -x "$GATE" ]] || fail "gate is missing or not executable: $GATE"
[[ -f "$RECIPE" ]] || fail "source recipe is missing: $RECIPE"
bash -n "$GATE" || fail "bash -n rejects $GATE"
bash -n "$RECIPE" || fail "bash -n rejects $RECIPE"
bash -n "$0" || fail "bash -n rejects this test"

# A gate that only parses the recipe is exactly the hole #3011 closed.
if grep -Eq '^[^#]*makepkg[[:space:]]+\\?$|^[^#]*makepkg[[:space:]]+--' "$GATE" &&
  grep -Eq 'timeout --kill-after=[0-9]+s' "$GATE"; then
  pass "gate invokes makepkg itself and bounds it with a wall-clock timeout"
else
  fail "gate does not run makepkg under a wall-clock timeout"
fi

# The gate must source the real recipe, not carry a hand-written install list
# that can drift from packaging/PKGBUILD.
if grep -Eq '^[[:space:]]*install -D' "$GATE"; then
  fail "gate carries its own install list instead of the recipe's package()"
else
  pass "gate has no copied package() install list; it sources the real recipe"
fi
if grep -Eq '^[^#]*source %q|printf .source %q' "$GATE"; then
  pass "gate generates a wrapper that sources the real recipe"
else
  fail "gate never sources the real recipe"
fi

if grep -Eq '(^|[[:space:]"])(/(home|Users|mnt|workspace)/)' "$GATE"; then
  fail "gate hardcodes a host path"
else
  pass "gate derives every path from the repo and environment"
fi

if grep -Fq 'usr/bin/bittyd' "$GATE"; then
  pass "gate asserts the built package contains usr/bin/bittyd"
else
  fail "gate never checks the built package for usr/bin/bittyd"
fi

# Plumbing mode is fast because it stubs the cargo compile; that shortcut is
# only honest if the stub follows the recipe's own build() selection.
if grep -Fq 'declare -f build' "$GATE"; then
  pass "gate's stub derives its artifacts from the recipe's build() selection"
else
  fail "gate's stub build does not follow the recipe's build() selection"
fi

# --- Workflow wiring -------------------------------------------------------
job_block() { # <workflow> <job id>
  sed -n "/^  $2:/,/^  [a-z0-9-]*:\$/p" "$1"
}

ARCH_JOB="$(job_block "$CI_WORKFLOW" aur-source-build)"
if [[ -z "$ARCH_JOB" ]]; then
  fail "ci.yml has no aur-source-build job"
else
  if grep -qx '    container: archlinux:latest' <<<"$ARCH_JOB"; then
    pass "ci.yml aur-source-build runs in an archlinux container"
  else
    fail "ci.yml aur-source-build does not run in an archlinux container"
  fi
  if grep -qF 'bash scripts/check-aur-source-build.sh --mode plumbing' <<<"$ARCH_JOB"; then
    pass "ci.yml aur-source-build runs the gate in plumbing mode"
  else
    fail "ci.yml aur-source-build does not run scripts/check-aur-source-build.sh"
  fi
  if grep -qF 'bash scripts/tests/check-aur-source-build.test.sh' <<<"$ARCH_JOB"; then
    pass "ci.yml aur-source-build runs this negative test next to the gate"
  else
    fail "ci.yml aur-source-build does not run this test"
  fi
  if grep -Eq '^    timeout-minutes: [0-9]+$' <<<"$ARCH_JOB"; then
    pass "ci.yml aur-source-build declares a job timeout"
  else
    fail "ci.yml aur-source-build has no timeout-minutes"
  fi
  if grep -Eq '^  +# .*[Rr]untime: .*[0-9]' <<<"$ARCH_JOB"; then
    pass "ci.yml aur-source-build records its measured runtime"
  else
    fail "ci.yml aur-source-build does not record a measured runtime"
  fi
  # PR CI has no `paths:` filter (the whole workflow runs on every pull
  # request), and the job must not add a condition that could skip it: a gate
  # that silently does not run is the #3011 failure class.
  if grep -Eq '^    if:.*(pull_request|github\.event)' <<<"$ARCH_JOB"; then
    fail "ci.yml aur-source-build conditions itself on the event; it must run on every PR"
  else
    pass "ci.yml aur-source-build has no event condition that could skip a PR"
  fi
fi

# The release path must build the real package, not just render .SRCINFO.
AUR_JOB="$(job_block "$RELEASE_WORKFLOW" aur)"
if [[ -z "$AUR_JOB" ]]; then
  fail "release.yml has no aur job"
else
  if grep -qF 'scripts/check-aur-source-build.sh --mode full' <<<"$AUR_JOB"; then
    pass "release.yml aur builds the real source package before publishing"
  else
    fail "release.yml aur never builds the real source package"
  fi
  if grep -Eq '^[[:space:]]+run:.*makepkg' <<<"$AUR_JOB"; then
    fail "release.yml aur runs makepkg itself instead of letting the gate own the recipe build"
  else
    pass "release.yml aur delegates the build (and its dependency install) to the gate"
  fi
fi
if grep -qF -- '--syncdeps' "$GATE"; then
  pass "gate's full mode installs the declared dependencies with --syncdeps"
else
  fail "gate's full mode never resolves dependencies"
fi

if ! command -v makepkg >/dev/null 2>&1 || ! command -v tic >/dev/null 2>&1 ||
  ! command -v fakeroot >/dev/null 2>&1; then
  echo "SKIP: makepkg/tic/fakeroot missing; mutation cases run in the archlinux CI job"
  if [[ "$FAIL" -ne 0 ]]; then
    echo "check-aur-source-build.test: FAIL" >&2
    exit 1
  fi
  echo "check-aur-source-build.test: PASS (static only)"
  exit 0
fi

# --- Runtime cases ---------------------------------------------------------
run_gate() { # <recipe> -> exit status; output in $TMP/gate.{out,err}
  local recipe="$1"
  local rc=0
  timeout "$GATE_TIMEOUT" bash "$GATE" --mode plumbing --recipe "$recipe" \
    --timeout "$GATE_TIMEOUT" >"$TMP/gate.out" 2>"$TMP/gate.err" || rc=$?
  return "$rc"
}

expect_reject() { # <label> <recipe> <expected failure text>
  local label="$1" recipe="$2" want="$3"
  if run_gate "$recipe"; then
    fail "gate accepted $label"
    return
  fi
  if grep -Fq -- "$want" "$TMP/gate.err"; then
    pass "gate rejects $label (exit 1; reported: $want)"
  else
    fail "gate rejected $label for an unexpected reason: $(tail -n 2 "$TMP/gate.err" | tr '\n' ' ')"
  fi
}

CONTROL="$TMP/PKGBUILD.control"
cp "$RECIPE" "$CONTROL"
chmod 644 "$CONTROL"
if run_gate "$CONTROL"; then
  pass "gate accepts the untouched recipe (control run)"
else
  fail "gate rejects the untouched recipe, so the reject cases below prove nothing: $(tail -n 3 "$TMP/gate.err" | tr '\n' ' ')"
  echo "check-aur-source-build.test: FAIL" >&2
  exit 1
fi

# Mutation 1: package() stops installing the daemon. The install line must be
# unique, otherwise this case would not prove what it claims. The assignment is
# a verbatim recipe line, so `$pkgdir` must not expand here.
# shellcheck disable=SC2016
BITTYD_INSTALL='install -Dm755 "target/release/bittyd" "$pkgdir/usr/bin/bittyd"'
if [[ "$(grep -cF -- "$BITTYD_INSTALL" "$RECIPE")" -ne 1 ]]; then
  fail "cannot isolate the bittyd install line in package() (found $(grep -cF -- "$BITTYD_INSTALL" "$RECIPE"))"
else
  MUTATED="$TMP/PKGBUILD.no-bittyd-install"
  grep -vF -- "$BITTYD_INSTALL" "$CONTROL" >"$MUTATED"
  chmod 644 "$MUTATED"
  if cmp -s "$CONTROL" "$MUTATED"; then
    fail "the bittyd-install mutation changed nothing"
  else
    expect_reject "a package() that does not install usr/bin/bittyd" "$MUTATED" \
      "omits usr/bin/bittyd"
  fi
fi

# Mutation 2: build() stops selecting the workspace member that produces the
# daemon, so package() copies an artifact cargo would never emit.
BUILD_SELECTION='cargo build --release --locked -p bitty-terminal -p bitty-daemon'
if [[ "$(grep -cF -- "$BUILD_SELECTION" "$RECIPE")" -ne 1 ]]; then
  fail "cannot isolate the build() selection line in the recipe"
else
  MUTATED="$TMP/PKGBUILD.no-daemon-build"
  awk -v from="$BUILD_SELECTION" -v to='cargo build --release --locked -p bitty-terminal' \
    '{ if (index($0, from) > 0) { sub(from, to) } print }' "$CONTROL" >"$MUTATED"
  chmod 644 "$MUTATED"
  if cmp -s "$CONTROL" "$MUTATED"; then
    fail "the build-selection mutation changed nothing"
  else
    expect_reject "a build() that drops -p bitty-daemon" "$MUTATED" \
      "target/release/bittyd"
  fi
fi

# Mutation 3: the same hole on the client binary — the package would ship a
# daemon and no terminal.
# shellcheck disable=SC2016 # recipe line, `$pkgdir` must not expand here
BITTY_INSTALL='install -Dm755 "target/release/bitty" "$pkgdir/usr/bin/bitty"'
if [[ "$(grep -cF -- "$BITTY_INSTALL" "$RECIPE")" -ne 1 ]]; then
  fail "cannot isolate the bitty install line in package()"
else
  MUTATED="$TMP/PKGBUILD.no-bitty-install"
  grep -vF -- "$BITTY_INSTALL" "$CONTROL" >"$MUTATED"
  chmod 644 "$MUTATED"
  if cmp -s "$CONTROL" "$MUTATED"; then
    fail "the bitty-install mutation changed nothing"
  else
    expect_reject "a package() that does not install usr/bin/bitty" "$MUTATED" \
      "omits usr/bin/bitty ("
  fi
fi

# Mutation 4: the recipe declares a makedepend that does not exist. This is the
# other half of #3011 ("a missing makedepends never fails CI"): plumbing mode
# cannot install deps, so it resolves every declared name against the pacman
# repos and must fail on a name the repos do not carry.
if command -v pacman >/dev/null 2>&1 &&
  pacman -Sp --noconfirm --print-format '%n' ncurses >/dev/null 2>&1; then
  MAKEDEPENDS="$(grep -E '^makedepends=\(' "$RECIPE" | head -n 1)"
  if [[ -z "$MAKEDEPENDS" ]]; then
    fail "cannot find the recipe's makedepends line"
  else
    MUTATED="$TMP/PKGBUILD.bad-makedepends"
    awk -v from="$MAKEDEPENDS" '{
      if ($0 == from) {
        line = from
        gsub(/ncurses/, "ncurses-does-not-exist", line)
        print line
      } else {
        print
      }
    }' "$CONTROL" >"$MUTATED"
    chmod 644 "$MUTATED"
    if cmp -s "$CONTROL" "$MUTATED"; then
      fail "the makedepends mutation changed nothing"
    else
      expect_reject "a makedepends entry the pacman repos do not carry" "$MUTATED" \
        "does not resolve in the pacman repos"
    fi
  fi
else
  echo "SKIP: pacman repo database unavailable, makedepends resolution case skipped"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-aur-source-build.test: FAIL" >&2
  exit 1
fi
echo "check-aur-source-build.test: PASS"
