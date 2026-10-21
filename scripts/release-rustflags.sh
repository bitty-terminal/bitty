#!/usr/bin/env bash
# release-rustflags.sh — emit the `--remap-path-prefix` flags that keep
# builder roots out of release binaries (CTX-2402, #2402).
#
# Usage:
#   scripts/release-rustflags.sh
#
# Prints one space-separated RUSTFLAGS fragment on stdout (nothing when no
# root can be derived). Consumers prepend it to RUSTFLAGS:
#
#   RUSTFLAGS="$(scripts/release-rustflags.sh)${RUSTFLAGS:+ $RUSTFLAGS}" \
#     cargo build --release --locked -p bitty-terminal
#
# `just release-build` and the `build`/`build-alpine`/`build-freebsd` jobs in
# `.github/workflows/release.yml` all call this script, so local and CI
# release binaries derive the same remapped shape (`/build/...`) from
# whatever host runs the build. No builder path is committed to the
# repository.
#
# Why remap instead of strip: Rust embeds the absolute path of every panic
# location (`#[track_caller]`/`panic!` `Location` statics) in allocated
# read-only data, which is runtime data that `strip` cannot remove (verified
# on the 0.0.25 artifact: `strip --strip-all` still leaves every builder
# path). The mappings cover the workspace checkout, `CARGO_TARGET_DIR`, and
# the cargo registry/git-checkout plus rustup-toolchain roots.
#
# Mappings are emitted non-overlapping: a root nested inside an already
# mapped root is skipped, and `$HOME` is intentionally not mapped as a
# catch-all. rustc's precedence between overlapping `--remap-path-prefix`
# entries is an implementation detail (a broad parent mapping can win and
# turn `$HOME/.cargo` into the less readable `/build/home/.cargo`), so the
# script removes the ambiguity instead of depending on ordering. A builder
# path outside these roots fails `scripts/check-release-paths.sh`, which
# then needs one more derived root instead of shipping silently.
#
# Portability: POSIX-ish bash (macOS bash 3.2, git-bash on Windows). A root
# containing whitespace cannot be represented in RUSTFLAGS (which is split
# on whitespace), so the script fails closed instead of emitting a flag that
# silently does nothing.
#
# Exit codes: 0 = flags printed (possibly none), 1 = a derived root cannot
# be expressed safely, 2 = usage error.
set -euo pipefail
export LC_ALL=C

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

IS_WINDOWS=0
case "$(uname -s 2>/dev/null || printf 'unknown')" in
MSYS* | MINGW* | CYGWIN*) IS_WINDOWS=1 ;;
esac

usage() {
  cat <<'EOF'
Usage: scripts/release-rustflags.sh

Prints the RUSTFLAGS fragment that remaps builder roots to /build/... for a
release build. No arguments; roots are derived from the environment
(HOME, CARGO_HOME, RUSTUP_HOME, CARGO_TARGET_DIR) and this checkout.

Example:
  RUSTFLAGS="$(scripts/release-rustflags.sh)" cargo build --release -p bitty-terminal
EOF
}

die() {
  printf 'release-rustflags[error]: %s\n' "$*" >&2
  exit 1
}

if (($# > 0)); then
  case "$1" in
  -h | --help)
    usage
    exit 0
    ;;
  *)
    printf 'release-rustflags: unknown argument: %s\n' "$1" >&2
    printf 'run with --help\n' >&2
    exit 2
    ;;
  esac
fi

TMPDIR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

PAIRS="$TMPDIR_ROOT/pairs"
SORTED="$TMPDIR_ROOT/sorted"
: >"$PAIRS"

SEQ=0

# native_forms PATH — print PATH, plus on Windows the native backslash form
# (rustc records Windows paths with backslashes) and its forward-slash twin.
native_forms() {
  local p="$1"
  printf '%s\n' "$p"
  if ((IS_WINDOWS)) && command -v cygpath >/dev/null 2>&1; then
    local w
    w="$(cygpath -w "$p")"
    printf '%s\n' "$w"
    printf '%s\n' "${w//\\//}"
  fi
}

# add_root ROOT REPLACEMENT — queue every spelling of ROOT -> REPLACEMENT.
add_root() {
  local root="$1" replacement="$2" form
  [[ -n "$root" ]] || return 0
  while [[ "$root" == */ && "$root" != "/" ]]; do
    root="${root%/}"
  done
  [[ "$root" == "/" ]] && return 0
  while IFS= read -r form; do
    [[ -n "$form" ]] || continue
    case "$form" in
    *[[:space:]]*)
      die "build root contains whitespace and cannot be expressed in RUSTFLAGS: '$form' (relocate CARGO_HOME/RUSTUP_HOME/CARGO_TARGET_DIR or the checkout)"
      ;;
    esac
    printf '%s\t%s\t%s\t%s\n' "${#form}" "$SEQ" "$form" "$replacement" >>"$PAIRS"
    SEQ=$((SEQ + 1))
  done < <(native_forms "$root")
}

HOME_DIR="${HOME:-}"
CARGO_HOME_DIR="${CARGO_HOME:-${HOME_DIR:+$HOME_DIR/.cargo}}"
RUSTUP_HOME_DIR="${RUSTUP_HOME:-${HOME_DIR:+$HOME_DIR/.rustup}}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
[[ "$TARGET_DIR" == /* ]] || TARGET_DIR="$PWD/$TARGET_DIR"
# A system toolchain (distro rustc, `rustup toolchain link`) lives outside
# RUSTUP_HOME; remap its sysroot too so std/core panic locations stay stable.
SYSROOT_DIR="$(rustc --print sysroot 2>/dev/null || true)"

# is_nested PATH PARENT — true when PATH lives inside PARENT (or equals it).
is_nested() {
  local path="$1" parent="$2"
  [[ -n "$path" && -n "$parent" ]] || return 1
  while [[ "$path" == */ && "$path" != "/" ]]; do path="${path%/}"; done
  while [[ "$parent" == */ && "$parent" != "/" ]]; do parent="${parent%/}"; done
  [[ "$path" == "$parent" ]] && return 0
  case "$path/" in
  "$parent"/*) return 0 ;;
  esac
  return 1
}

MAPPED=()

# queue_root ROOT REPLACEMENT — add ROOT unless an already queued root covers
# it. Overlapping `--remap-path-prefix` entries resolve by rustc behavior, not
# by intent, so keep the set disjoint.
queue_root() {
  local root="$1" mapped
  [[ -n "$root" ]] || return 0
  for mapped in ${MAPPED[@]+"${MAPPED[@]}"}; do
    if is_nested "$root" "$mapped"; then
      return 0
    fi
  done
  MAPPED+=("$root")
  add_root "$root" "$2"
}

# Specific roots first; a nested root (for example the default `$repo/target`
# under the workspace) is already covered by its parent.
queue_root "$REPO_ROOT" /build/workspace
queue_root "$TARGET_DIR" /build/target
queue_root "$CARGO_HOME_DIR" /build/cargo
queue_root "$RUSTUP_HOME_DIR" /build/rustup
queue_root "$SYSROOT_DIR" /build/rustup

# Longest prefix first, stable by insertion order, first mapping per prefix
# wins (a later, broader mapping must never shadow a specific one).
sort -t "$(printf '\t')" -k1,1nr -k2,2n "$PAIRS" |
  awk -F '\t' '!seen[$3]++ { print $3 "\t" $4 }' >"$SORTED"

FLAGS=()
while IFS="$(printf '\t')" read -r from to; do
  [[ -n "$from" ]] || continue
  FLAGS+=("--remap-path-prefix=$from=$to")
done <"$SORTED"

((${#FLAGS[@]} == 0)) && exit 0
printf '%s\n' "${FLAGS[*]}"
