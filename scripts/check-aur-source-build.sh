#!/usr/bin/env bash
# check-aur-source-build.sh — build the AUR source package with a real `makepkg`.
#
# Why this exists (bitty#3011): the release `aur` job only ran
# `makepkg --printsrcinfo`, which parses the recipe and never runs prepare(),
# build(), or package(). A `package()` that stopped installing a built binary —
# or a `build()` that no longer selected the workspace member that produces it —
# still produced valid `.SRCINFO` and shipped green (the #2999 failure class).
# This gate runs `makepkg` for real and inspects the binary package it produces.
#
# Two modes, both a REAL makepkg run against the real recipe (never a
# hand-copied `package()`):
#
#   --mode plumbing   (default, PR CI)
#     * copies packaging/PKGBUILD byte-for-byte and `source`s it from a
#       generated wrapper, so `package()`, `pkgname`, `depends`, and the source
#       list are the recipe's own code;
#     * replaces only `prepare()`/`build()` — the cargo compile — with a stub
#       that emits exactly the artifacts the recipe's own `build()` selects
#       (`-p bitty-terminal`, `-p bitty-daemon`). Dropping a workspace member
#       from `build()` therefore still breaks the real `package()` install;
#     * feeds a local fixture tarball (same `$pkgname-$pkgver` layout, contents
#       limited to what `package()` installs), so no network and no cargo run;
#     * runs `makepkg --force --nocheck --nodeps` under a wall-clock timeout and
#       requires the produced package to contain usr/bin/bitty, usr/bin/bittyd,
#       and compiled terminfo.
#     Bounded approximation: the artifact bytes are stubs. This mode proves
#     packaging plumbing (source layout, install paths, tic, dependency names,
#     install-vs-build-selection consistency), not that the Rust tree compiles.
#
#   --mode full       (release / on demand)
#     * runs the unmodified recipe end to end: `makepkg --force --nocheck
#       dependencies installed, so cargo fetch/build really happen and every declared
#       dependency must exist in the pacman repos;
#     * same package-content assertions on the real binary package.
#     Ceiling: `--nocheck` skips the recipe's `check()` smoke test (project CI
#     owns the test matrix); only the Linux x86_64/aarch64 package is built.
#
# Both modes run `makepkg` as an unprivileged user. Inside a container the
# script is typically started as root; it then re-executes itself as a
# throwaway `builder` account, because makepkg refuses to build as root.
#
# Usage:
#   scripts/check-aur-source-build.sh [--mode plumbing|full] [--recipe PATH]
#                                     [--timeout SECONDS] [--expect-version X.Y.Z]
#                                     [--srcdest DIR] [--keep]
#
# --srcdest DIR points makepkg at a directory that already holds the recipe's
# source tarball (makepkg's SRCDEST). It exists for offline/reproducible reruns
# (air-gapped builders, a locally mirrored release tarball) and for testing the
# full path without release-asset network access; the gate still verifies the
# archive through the recipe's own checksum entry.
#
# Exit status: 0 gate passed; 1 gate failed; 2 usage or precondition error.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIG_ARGS=("$@")
RECIPE="$REPO_ROOT/packaging/PKGBUILD"
MODE=plumbing
WALL_TIMEOUT=900
EXPECT_VERSION=""
SRCDEST_OPT=""
KEEP=0

usage() {
  sed -n '/^# Usage:/,/^# Exit status:/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die_usage() {
  echo "check-aur-source-build: $1" >&2
  exit 2
}

die() {
  echo "check-aur-source-build: FAIL: $1" >&2
  exit 1
}

while (($#)); do
  case "$1" in
  --mode)
    [[ $# -ge 2 ]] || die_usage "--mode needs a value"
    MODE="$2"
    shift 2
    ;;
  --recipe)
    [[ $# -ge 2 ]] || die_usage "--recipe needs a path"
    RECIPE="$2"
    shift 2
    ;;
  --timeout)
    [[ $# -ge 2 ]] || die_usage "--timeout needs seconds"
    WALL_TIMEOUT="$2"
    shift 2
    ;;
  --expect-version)
    [[ $# -ge 2 ]] || die_usage "--expect-version needs a version"
    EXPECT_VERSION="${2#v}"
    shift 2
    ;;
  --srcdest)
    [[ $# -ge 2 ]] || die_usage "--srcdest needs a directory"
    SRCDEST_OPT="$2"
    shift 2
    ;;
  --keep)
    KEEP=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    die_usage "unknown argument: $1"
    ;;
  esac
done

case "$MODE" in
plumbing | full) ;;
*) die_usage "--mode must be plumbing or full, got '$MODE'" ;;
esac
[[ "$WALL_TIMEOUT" =~ ^[0-9]+$ ]] || die_usage "--timeout must be an integer number of seconds"
[[ -f "$RECIPE" ]] || die "recipe not found: $RECIPE"
if [[ -n "$SRCDEST_OPT" ]]; then
  [[ -d "$SRCDEST_OPT" ]] || die "--srcdest '$SRCDEST_OPT' is not a directory"
  SRCDEST_OPT="$(cd "$SRCDEST_OPT" && pwd)"
fi

# --- Preconditions ---------------------------------------------------------
for tool in makepkg bsdtar timeout; do
  command -v "$tool" >/dev/null 2>&1 ||
    die "missing '$tool' (needs Arch tooling: pacman -S --needed base-devel libarchive)"
done
command -v tic >/dev/null 2>&1 ||
  die "missing 'tic' (package() compiles terminfo/bitty.ti; pacman -S --needed ncurses)"

# --- Run makepkg as an unprivileged user when we are root ------------------
# Container CI (archlinux:latest job container) starts as root, and makepkg
# hard-fails as root ("Running makepkg as root is not allowed"). Re-exec the
# whole gate once as `builder` so every makepkg step below runs unprivileged.
if [[ "$EUID" -eq 0 && -z "${AUR_GATE_AS_BUILDER:-}" ]]; then
  id builder >/dev/null 2>&1 || useradd --create-home builder ||
    die "cannot create the unprivileged 'builder' account"
  WORKROOT="$(mktemp -d "${TMPDIR:-/tmp}/aur-source-build.XXXXXX")"
  chown -R builder:builder "$WORKROOT"
  # Full mode installs the recipe's declared dependencies here, as root, and
  # then runs makepkg with --nodeps. The build account never gets a privilege
  # escalation: in the release job the same container later holds the publish
  # key, so a dependency build script must not be able to reach root or plant
  # state the publish step would execute (bitty#3027).
  #
  # The declarations are enumerated as `builder`, never as root: a PKGBUILD is
  # untrusted code, so sourcing it may not run with root's authority. The
  # resulting names are validated before pacman sees them.
  if [[ "$MODE" == full ]]; then
    deps_file="$WORKROOT/declared-deps.txt"
    su builder -s /bin/bash -c "bash -c 'source \"\$1\" >/dev/null 2>&1; printf \"%s\\n\" \"\${depends[@]}\" \"\${makedepends[@]}\"' _ '$RECIPE'" >"$deps_file" ||
      die "cannot enumerate the recipe's declared dependencies"
    mapfile -t root_deps < <(grep -E '^[A-Za-z0-9@._+<>=:-]+$' "$deps_file" | sort -u)
    ((${#root_deps[@]})) || die "full mode found no declared dependencies to install"
    echo "gate: installing ${#root_deps[@]} declared dependenc(y/ies) as root before the unprivileged build"
    pacman -S --needed --noconfirm "${root_deps[@]}" ||
      die "a declared dependency could not be installed (a wrong or missing makedepends must fail CI, not the next release)"
  fi
  reexec=("$REPO_ROOT/scripts/check-aur-source-build.sh")
  [[ -x "$REPO_ROOT/scripts/check-aur-source-build.sh" ]] ||
    reexec=(bash "$REPO_ROOT/scripts/check-aur-source-build.sh")
  printf -v cmd 'cd %q && AUR_GATE_AS_BUILDER=1 AUR_GATE_WORKROOT=%q' "$PWD" "$WORKROOT"
  for arg in "${reexec[@]}" "${ORIG_ARGS[@]}"; do
    printf -v quoted ' %q' "$arg"
    cmd+="$quoted"
  done
  rc=0
  su builder -s /bin/bash -c "$cmd" || rc=$?
  [[ "$KEEP" -eq 1 ]] || rm -rf "$WORKROOT"
  exit "$rc"
fi

WORKROOT="${AUR_GATE_WORKROOT:-$(mktemp -d "${TMPDIR:-/tmp}/aur-source-build.XXXXXX")}"
[[ "$KEEP" -eq 1 ]] || trap 'rm -rf "$WORKROOT"' EXIT

RECIPE_COPY="$WORKROOT/recipe/PKGBUILD"
mkdir -p "$WORKROOT/recipe" "$WORKROOT/build" "$WORKROOT/src"

# Byte-identical copy: the wrapper below must `source` the real recipe, and a
# copy keeps the path readable for the unprivileged builder. Identity is
# asserted so a doctored copy can never be what the gate builds.
cp "$RECIPE" "$RECIPE_COPY"
if command -v sha256sum >/dev/null 2>&1; then
  want="$(sha256sum "$RECIPE" | awk '{print $1}')"
  got="$(sha256sum "$RECIPE_COPY" | awk '{print $1}')"
  [[ "$want" == "$got" ]] || die "recipe copy is not byte-identical to $RECIPE"
fi

# --- Local source mirror (--srcdest) ---------------------------------------
# makepkg insists on a writable SRCDEST, so a read-only mirror (mounted release
# assets, air-gapped media) is copied into the work dir and SRCDEST is pointed
# there. The recipe's own checksum entry still validates the archive.
if [[ -n "$SRCDEST_OPT" ]]; then
  mkdir -p "$WORKROOT/srcdest"
  cp -a "$SRCDEST_OPT/." "$WORKROOT/srcdest/"
  SRCDEST_OPT="$WORKROOT/srcdest"
fi

# pkgname/pkgver are read by executing the recipe's assignments (the recipe has
# no top-level side effects; all work happens in functions).
read -r PKGNAME PKGVER < <(bash -c 'source "$1" >/dev/null 2>&1; printf "%s %s\n" "$pkgname" "$pkgver"' _ "$RECIPE_COPY") ||
  die "cannot read pkgname/pkgver from $RECIPE"
[[ -n "$PKGNAME" && -n "$PKGVER" ]] || die "recipe does not declare pkgname/pkgver"

if [[ -n "$EXPECT_VERSION" && "$PKGVER" != "$EXPECT_VERSION" ]]; then
  die "recipe pkgver is $PKGVER but the release expects $EXPECT_VERSION (the tagged tree must carry its own version)"
fi

echo "gate: mode=$MODE recipe=${RECIPE#"$REPO_ROOT"/} pkgname=$PKGNAME pkgver=$PKGVER timeout=${WALL_TIMEOUT}s"
echo "gate: workdir=$WORKROOT"

# --- Declared dependencies must resolve ------------------------------------
# #3011 names "a missing makedepends never fails CI" as part of the hole. Full
# mode proves it by installing them as root before the unprivileged build.
# Plumbing mode does not install, so it resolves every declared dependency name
# against the pacman repos instead: a typo'd or deleted package still fails
# here, cheaply.
check_dep_names() {
  command -v pacman >/dev/null 2>&1 || {
    echo "gate: pacman unavailable, dependency-name resolution skipped (full mode installs deps instead)"
    return 0
  }
  local recipe="$1" dep name
  local -a deps=()
  while IFS= read -r dep; do
    [[ -n "$dep" ]] && deps+=("$dep")
  done < <(bash -c 'source "$1" >/dev/null 2>&1; printf "%s\n" "${depends[@]}" "${makedepends[@]}"' _ "$recipe")
  ((${#deps[@]})) || return 0
  for dep in "${deps[@]}"; do
    name="${dep%%[<>=]*}"
    if ! pacman -Sp --noconfirm --print-format '%n' "$name" >/dev/null 2>&1; then
      die "declared dependency '$dep' does not resolve in the pacman repos (run 'pacman -Sy' first); a wrong or missing makedepends must fail CI, not the next release"
    fi
  done
  echo "gate: ${#deps[@]} declared dependenc(y/ies) resolve in the pacman repos"
}

# --- Fixture source tree (plumbing mode) -----------------------------------
# Same archive layout the real source tarball has ($PKGNAME-$PKGVER at the
# top), carrying exactly the in-tree files package() installs. No crates: the
# cargo compile is what this mode cannot afford.
make_fixture() {
  local stage="$WORKROOT/fixture/$PKGNAME-$PKGVER"
  mkdir -p "$stage"
  local rel
  for rel in LICENSE README.md CHANGELOG.md packaging terminfo; do
    [[ -e "$REPO_ROOT/$rel" ]] || die "recipe installs from '$rel' but it is missing in the tree"
    cp -a "$REPO_ROOT/$rel" "$stage/"
  done
  tar -czf "$WORKROOT/src/$PKGNAME-$PKGVER.tar.gz" -C "$WORKROOT/fixture" "$PKGNAME-$PKGVER"
}

# --- Wrapper PKGBUILD ------------------------------------------------------
# `source` the real recipe, then override only prepare()/build(). package(),
# check(), and every variable stay the recipe's own code: no copied or sed'd
# package() exists in this repository (bitty#3011 review requirement).
write_wrapper() {
  local dest="$1" fixture="$2" recipe="$3"
  {
    printf '# Generated by scripts/check-aur-source-build.sh — do not edit.\n'
    printf '# Everything except prepare()/build() below is the real recipe.\n'
    printf 'source %q\n' "$recipe"
    cat <<'WRAPPER'

# The recipe's own build() selection decides which cargo artifacts a real
# build would emit. The stub below honors that selection, so dropping
# `-p bitty-daemon` still makes the real package() fail on a missing file.
_gate_real_build="$(declare -f build)"
_gate_selects() {
  grep -Eq -- "-p[[:space:]]+$1([[:space:]]|$)" <<<"$_gate_real_build"
}

prepare() { :; }

build() {
  cd "$pkgname-$pkgver"
  mkdir -p target/release
  if _gate_selects bitty-terminal; then
    printf '#!/bin/sh\necho "%s"\n' "$pkgver" >target/release/bitty
    chmod 755 target/release/bitty
  fi
  if _gate_selects bitty-daemon; then
    printf '#!/bin/sh\necho "bittyd %s (service API %s)"\n' "$pkgver" "$pkgver" >target/release/bittyd
    chmod 755 target/release/bittyd
  fi
}
WRAPPER
    printf '# Local fixture source: same archive layout, no network.\n'
    printf 'source=("%s::file://%s")\n' "$(basename "$fixture")" "$fixture"
    printf "sha256sums=('SKIP')\n"
  } >"$dest"
}

# --- Run makepkg -----------------------------------------------------------
run_makepkg() {
  local builddir="$1" log="$WORKROOT/makepkg.log"
  shift
  local -a extra=("$@")
  local rc=0
  set +e
  (
    cd "$builddir"
    if [[ -n "$SRCDEST_OPT" ]]; then
      export SRCDEST="$SRCDEST_OPT"
    fi
    timeout --kill-after=15s "$WALL_TIMEOUT" makepkg \
      --force --nocheck --noconfirm --noprogressbar "${extra[@]}" 2>&1
  ) | tee "$log"
  rc="${PIPESTATUS[0]}"
  set -e
  if [[ "$rc" -ne 0 ]]; then
    if [[ "$rc" -eq 124 || "$rc" -eq 137 ]]; then
      die "makepkg exceeded the ${WALL_TIMEOUT}s wall-clock timeout (mode=$MODE); raise --timeout and the workflow timeout-minutes together"
    fi
    die "makepkg failed (exit $rc, mode=$MODE); last lines: $(tail -n 5 "$log" | tr '\n' ' ')"
  fi
}

# --- Package content assertions -------------------------------------------
inspect_package() {
  local builddir="$1" pkgfile
  # `makepkg --packagelist` also lists the split debug package; the first entry
  # is the package itself. Read the whole list (no `head`, which can SIGPIPE
  # makepkg under pipefail and silently drop the path).
  local -a pkglist=()
  mapfile -t pkglist < <(cd "$builddir" && makepkg --packagelist)
  pkgfile="${pkglist[0]:-}"
  [[ -n "$pkgfile" ]] || die "makepkg --packagelist produced no package path"
  [[ -f "$pkgfile" ]] || die "makepkg reported '$pkgfile' but no package file exists"

  local -a contents=()
  mapfile -t contents < <(bsdtar -tf "$pkgfile")
  ((${#contents[@]})) || die "built package is empty: $pkgfile"

  local listing
  listing="$(bsdtar -tvf "$pkgfile")"

  local entry
  for entry in usr/bin/bitty usr/bin/bittyd; do
    if ! printf '%s\n' "${contents[@]}" | grep -Fxq -- "$entry"; then
      die "built package omits $entry (package() must install every binary build() produces; bitty#3011)"
    fi
    if ! printf '%s\n' "$listing" | grep -E -- "[[:space:]]${entry}$" | grep -q '^-rwxr-xr-x'; then
      die "$entry is not installed mode 0755 in the built package"
    fi
  done

  # tic must have compiled the canonical terminfo source into the package.
  if ! printf '%s\n' "${contents[@]}" | grep -Eq '^usr/share/terminfo/[^/]+/'; then
    die "built package carries no usr/share/terminfo entry (package() must run tic -x)"
  fi

  # No unexpanded build-time paths leaked into the package.
  if printf '%s\n' "${contents[@]}" | grep -Eq '(^|/)(\.\.|target|build)/'; then
    die "built package contains build-tree paths: $(printf '%s\n' "${contents[@]}" | grep -E '(^|/)(\.\.|target|build)/' | head -n 3 | tr '\n' ' ')"
  fi

  echo "gate: package=$pkgfile"
  echo "gate: entries=${#contents[@]} binary-package=$(basename "$pkgfile")"
  printf '%s\n' "${contents[@]}" | grep -E '^usr/bin/' | sed 's/^/gate:   /'
}

# --- Modes -----------------------------------------------------------------
BUILD_DIR="$WORKROOT/build"

if [[ "$MODE" == plumbing ]]; then
  check_dep_names "$RECIPE_COPY"
  make_fixture
  FIXTURE="$WORKROOT/src/$PKGNAME-$PKGVER.tar.gz"
  write_wrapper "$BUILD_DIR/PKGBUILD" "$FIXTURE" "$RECIPE_COPY"
  echo "gate: plumbing build() stub emits only the artifacts the recipe's build() selects"
  run_makepkg "$BUILD_DIR" --nodeps
  inspect_package "$BUILD_DIR"
else
  # Full mode builds the recipe as written: real source download, real cargo,
  # real dependency install. The copy is still used so makepkg runs in a
  # writable dir owned by builder and never dirties the checkout.
  cp "$RECIPE_COPY" "$BUILD_DIR/PKGBUILD"
  echo "gate: full build of the unmodified recipe (--nodeps --nocheck); this compiles the workspace"
  run_makepkg "$BUILD_DIR" --nodeps --cleanbuild
  inspect_package "$BUILD_DIR"
fi

echo "gate: PASS (mode=$MODE, real makepkg, package contains usr/bin/bitty and usr/bin/bittyd)"
