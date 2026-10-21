#!/usr/bin/env bash
# check-release-paths.sh — fail when a built binary embeds a builder path
# (CTX-2402, #2402).
#
# Usage:
#   scripts/check-release-paths.sh --binary PATH [--binary PATH ...]
#                                  [--prefix PREFIX ...]
#
# The release policy (packaging/README.md, "Reproducible release binaries")
# remaps every builder root to a stable `/build/...` shape at compile time
# via `scripts/release-rustflags.sh`. This gate is the fail-closed backstop:
# it scans the built bytes for
#
#   * the current builder roots derived from the environment (`HOME`,
#     `CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TARGET_DIR`) and this checkout,
#   * any `/home/<user>/` or `/Users/<user>/` path (covers other builders'
#     binaries and a bypassed remap), and
#   * Windows user-profile paths (`%USERPROFILE%`),
#
# so a build that skips the flag script cannot ship. Rust keeps panic-location
# paths in runtime data, so stripping does not satisfy this gate by itself.
#
# It reads only the file bytes with `grep -a`, so it works on ELF, Mach-O,
# and PE artifacts from any host (the release workflow runs it on the same
# runner that produced each target).
#
# Options:
#   --binary PATH  Built binary to scan (repeatable, required).
#   --prefix PATH  Extra literal prefix that must not appear (repeatable);
#                  the derived roots above are always added.
#   -h, --help     Show this help.
#
# Exit codes: 0 = clean, 1 = builder path found (or unreadable binary),
# 2 = usage error.
set -euo pipefail
export LC_ALL=C

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BINARIES=()
EXTRA_PREFIXES=()

usage() {
  cat <<'EOF'
Usage: scripts/check-release-paths.sh --binary PATH [--binary PATH ...]
                                      [--prefix PREFIX ...]

Scans built binaries for builder paths (the current HOME, CARGO_HOME,
RUSTUP_HOME, CARGO_TARGET_DIR, this checkout, any /home/<user>/ or
/Users/<user>/ path, and Windows user-profile paths) and fails when one is
present. Release builds must be produced with scripts/release-rustflags.sh
(`just release-build`).

Exit codes: 0 = clean, 1 = builder path found, 2 = usage error.
EOF
}

usage_error() {
  printf 'check-release-paths: %s\n' "$*" >&2
  printf 'run with --help\n' >&2
  exit 2
}

die() {
  printf 'check-release-paths[error]: %s\n' "$*" >&2
  exit 1
}

while (($#)); do
  case "$1" in
  --binary | --prefix)
    (($# >= 2)) || usage_error "$1 needs a value"
    case "$1" in
    --binary) BINARIES+=("$2") ;;
    --prefix) EXTRA_PREFIXES+=("$2") ;;
    esac
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage_error "unknown argument: $1"
    ;;
  esac
done

((${#BINARIES[@]} > 0)) || usage_error "--binary is required"
command -v grep >/dev/null 2>&1 || die "grep not found"

TMPDIR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

PATTERNS="$TMPDIR_ROOT/patterns"
: >"$PATTERNS"

add_pattern() {
  [[ -n "$1" ]] || return 0
  printf '%s\n' "$1" >>"$PATTERNS"
}

# native_forms PATH — the path plus its Windows native spellings (rustc
# records `C:\...` on Windows runners).
native_forms() {
  local p="$1"
  printf '%s\n' "$p"
  case "$(uname -s 2>/dev/null || printf 'unknown')" in
  MSYS* | MINGW* | CYGWIN*)
    if command -v cygpath >/dev/null 2>&1; then
      local w
      w="$(cygpath -w "$p")"
      printf '%s\n' "$w"
      printf '%s\n' "${w//\\//}"
    fi
    ;;
  esac
}

derive_root() {
  local root="$1" form
  [[ -n "$root" ]] || return 0
  while [[ "$root" == */ && "$root" != "/" ]]; do
    root="${root%/}"
  done
  [[ "$root" == "/" ]] && return 0
  while IFS= read -r form; do
    add_pattern "$form"
  done < <(native_forms "$root")
}

HOME_DIR="${HOME:-}"
derive_root "$HOME_DIR"
derive_root "${CARGO_HOME:-${HOME_DIR:+$HOME_DIR/.cargo}}"
derive_root "${RUSTUP_HOME:-${HOME_DIR:+$HOME_DIR/.rustup}}"
derive_root "$(rustc --print sysroot 2>/dev/null || true)"
derive_root "${CARGO_TARGET_DIR:-}"
derive_root "$REPO_ROOT"
for prefix in ${EXTRA_PREFIXES[@]+"${EXTRA_PREFIXES[@]}"}; do
  derive_root "$prefix"
done

FAIL=0

scan_binary() {
  local bin="$1" hits prefix_hits generic_hits count shown
  [[ -f "$bin" ]] || die "binary not found: $bin"
  [[ -r "$bin" ]] || die "binary not readable: $bin"

  # 1. Literal builder roots (fixed strings, one per line).
  prefix_hits=""
  if [[ -s "$PATTERNS" ]]; then
    prefix_hits="$(grep -a -F -o -f "$PATTERNS" "$bin" 2>/dev/null || true)"
  fi

  # 2. Generic builder-home shapes from any host. The leading boundary keeps
  # remapped `/build/...` paths (and URL segments) from matching: a `/home/`
  # that follows a path character is not a fresh absolute root.
  generic_hits="$(
    {
      grep -a -E -o '(^|[^A-Za-z0-9._/-])/home/[A-Za-z0-9._-]+/' "$bin" 2>/dev/null || true
      grep -a -E -o '(^|[^A-Za-z0-9._/-])/Users/[A-Za-z0-9._-]+/' "$bin" 2>/dev/null || true
      grep -a -E -o '[A-Za-z]:\\Users\\[A-Za-z0-9._-]+' "$bin" 2>/dev/null || true
    } | sed -E 's|^[^/A-Za-z]||'
  )"

  hits="$(
    {
      [[ -n "$prefix_hits" ]] && printf '%s\n' "$prefix_hits"
      [[ -n "$generic_hits" ]] && printf '%s\n' "$generic_hits"
    } | sed '/^$/d' | sort -u
  )"

  if [[ -z "$hits" ]]; then
    printf 'check-release-paths: PASS %s (no builder paths)\n' "$bin"
    return 0
  fi

  count="$(printf '%s\n' "$hits" | wc -l | tr -d '[:space:]')"
  FAIL=1
  printf 'check-release-paths[error]: %s embeds %s builder path string(s):\n' "$bin" "$count" >&2
  shown=0
  while IFS= read -r hit; do
    [[ -n "$hit" ]] || continue
    if ((shown >= 8)); then
      break
    fi
    printf '  %s\n' "$hit" >&2
    shown=$((shown + 1))
  done <<<"$hits"
  if ((count > shown)); then
    printf '  ... and %s more\n' "$((count - shown))" >&2
  fi
  printf 'remediation: build with `just release-build` (scripts/release-rustflags.sh);\n' >&2
  printf 'stripping alone does not remove panic-location paths.\n' >&2
  return 1
}

for bin in "${BINARIES[@]}"; do
  scan_binary "$bin" || true
done

if ((FAIL)); then
  exit 1
fi
