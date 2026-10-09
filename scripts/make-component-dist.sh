#!/usr/bin/env bash
# make-component-dist.sh — assemble an R2 prebuilt component distribution.
#
# Packs one target's component binary plus its `bitty-component.toml` into
# `bitty/components/<name>/<version>/<target>.tar.gz` payload form and
# maintains the aggregate `SHA256SUMS` manifest beside it (CTX-1063, issue
# #1792; layout reserved by CTX-1056 in packaging/README.md). The future
# `r2-components` publisher uploads exactly these files with the immutable
# cache policy and read-back verification, mirroring `r2-mirror`; this
# script uploads nothing.
#
# The tarball holds exactly two members at the archive root (no wrapper
# directory): the executable `bitty-<name>` (mode 755) and
# `bitty-component.toml`. The client extracts into the version directory it
# creates (`<root>/<name>/<version>/`), so a wrapper dir would nest one
# level too deep. Member order is fixed (descriptor first, then the
# executable) so the archive layout is stable.
#
# The source descriptor is validated against the installed parser rules
# (`crates/bitty-runtime/src/component/descriptor.rs`): closed
# `[component]` table, `name` matching `[a-z][a-z0-9-]{0,31}`, `version`
# equal to `--version`, `protocol = [min, max]` with `1 <= min <= max`,
# `executable` equal to `bitty-<name>`, and a required lowercase-hex
# `sha256` that must match the binary. Anything else fails closed before
# any output is written.
#
# Pack version policy is intentionally narrower than the installed parser:
# only the strict `X.Y.Z` core (no leading zeros, no prerelease or build
# metadata) is accepted here, while the parser takes full semver. A
# prerelease descriptor the client would install is refused at pack time
# by policy, never silently repackaged.
#
# `--print-url` is a pure mapping (no spawn, no filesystem writes): it
# validates its inputs against the same allowlist and prints the tarball URL
# plus the manifest URL. Only `https://cdn.bitty.run` (or an explicit
# `--cdn-base` override for tests) is ever emitted; names, versions, or
# targets that would escape the template are refused.
#
# Needs only `tar`, `gzip`, and `sha256sum` (or `shasum -a 256`).
#
# Usage:
#   scripts/make-component-dist.sh --source <dir> --version 0.0.23 \
#     --target x86_64-unknown-linux-gnu --binary <path> --output <dir>/<target>.tar.gz
#   scripts/make-component-dist.sh --print-url --name net --version 0.0.23 \
#     --target x86_64-unknown-linux-gnu [--cdn-base https://cdn.bitty.run]
set -euo pipefail

DESCRIPTOR_FILE="bitty-component.toml"
EXECUTABLE_PREFIX="bitty-"
EXE_MAX_BYTES=268435456   # 256 MiB, mirrors COMPONENT_EXECUTABLE_MAX_BYTES
DESCRIPTOR_MAX_BYTES=4096 # mirrors COMPONENT_DESCRIPTOR_MAX_BYTES
CDN_DEFAULT="https://cdn.bitty.run"

SOURCE=""
VERSION=""
TARGET=""
BINARY=""
OUTPUT=""
PRINT_URL=0
NAME=""
CDN_BASE="$CDN_DEFAULT"
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: scripts/make-component-dist.sh --source DIR --version X.Y.Z --target TRIPLE --binary PATH --output DIR/<target>.tar.gz [--dry-run]
       scripts/make-component-dist.sh --print-url --name NAME --version X.Y.Z --target TRIPLE [--cdn-base URL]

  --source DIR     staged component dir holding bitty-component.toml
  --version X.Y.Z  component version (must match the descriptor; strict X.Y.Z)
  --target TRIPLE  rust target triple (e.g. x86_64-unknown-linux-gnu)
  --binary PATH    component executable to pack (non-empty, <= 256 MiB)
  --output PATH    tarball to create; basename must be <target>.tar.gz
  --dry-run        validate inputs and stage the payload only; write nothing
  --print-url      print the tarball URL and SHA256SUMS URL, then exit (pure)
  --name NAME      component name for --print-url ([a-z][a-z0-9-]{0,31})
  --cdn-base URL   CDN base override for --print-url (default https://cdn.bitty.run)
EOF
}

die() {
  echo "make-component-dist: ERROR: $1" >&2
  exit 1
}

valid_name() {
  [[ "$1" =~ ^[a-z][a-z0-9-]{0,31}$ ]]
}

# Intentional pack policy: strict X.Y.Z core only (no prerelease/build),
# narrower than the installed parser's full semver (see header). Leading
# zeros and out-of-u32 components are rejected like Version::parse.
valid_version() {
  [[ "${#1}" -le 64 ]] || return 1
  [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || return 1
  local part
  for part in "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}"; do
    [[ "${#part}" -gt 10 ]] && return 1
    # Ten-digit parts have no leading zero (see the regex above), so -gt
    # is octal-safe here; u32 max is 4294967295.
    [[ "${#part}" -eq 10 && "$part" -gt 4294967295 ]] && return 1
  done
  return 0
}

valid_target() {
  [[ "$1" =~ ^[a-z0-9_]+(-[a-z0-9_]+)+$ ]]
}

valid_cdn_base() {
  local base="$1"
  [[ "$base" =~ ^https://[A-Za-z0-9.-]+$ ]] || return 1
  # Reject lookalike hosts that merely extend the real one as a suffix
  # (for example https://cdn.bitty.run.evil.example).
  case "$base" in
  "$CDN_DEFAULT" | https://*.bitty.run) return 0 ;;
  *) return 1 ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  --source)
    SOURCE="${2:-}"
    shift 2
    ;;
  --version)
    VERSION="${2:-}"
    shift 2
    ;;
  --target)
    TARGET="${2:-}"
    shift 2
    ;;
  --binary)
    BINARY="${2:-}"
    shift 2
    ;;
  --output)
    OUTPUT="${2:-}"
    shift 2
    ;;
  --print-url)
    PRINT_URL=1
    shift
    ;;
  --name)
    NAME="${2:-}"
    shift 2
    ;;
  --cdn-base)
    CDN_BASE="${2:-}"
    shift 2
    ;;
  --dry-run)
    DRY_RUN=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    die "unknown argument: $1"
    ;;
  esac
done

# --- pure URL mapping: validate first, emit second, spawn nothing ---
if [[ "$PRINT_URL" -eq 1 ]]; then
  valid_name "$NAME" || die "--name must match [a-z][a-z0-9-]{0,31}, got: $NAME"
  valid_version "$VERSION" || die "--version must look like X.Y.Z, got: $VERSION"
  valid_target "$TARGET" || die "--target must look like a rust target triple, got: $TARGET"
  valid_cdn_base "$CDN_BASE" || die "--cdn-base must be this CDN host over https, got: $CDN_BASE"
  printf '%s/bitty/components/%s/%s/%s.tar.gz\n' "$CDN_BASE" "$NAME" "$VERSION" "$TARGET"
  printf '%s/bitty/components/%s/%s/SHA256SUMS\n' "$CDN_BASE" "$NAME" "$VERSION"
  exit 0
fi

[[ -n "$SOURCE" ]] || die "--source is required"
[[ -d "$SOURCE" ]] || die "--source not found: $SOURCE"
valid_version "$VERSION" || die "--version must look like X.Y.Z, got: $VERSION"
valid_target "$TARGET" || die "--target must look like a rust target triple, got: $TARGET"
[[ -n "$BINARY" ]] || die "--binary is required"
[[ -f "$BINARY" ]] || die "--binary not found: $BINARY"
[[ -n "$OUTPUT" ]] || die "--output is required"
# The R2 key name is the target triple: enforce it at pack time so the
# layout cannot drift between the script and packaging/README.md.
[[ "$(basename "$OUTPUT")" == "$TARGET.tar.gz" ]] || die "--output basename must be $TARGET.tar.gz, got: $(basename "$OUTPUT")"
command -v tar >/dev/null 2>&1 || die "tar not found"
command -v gzip >/dev/null 2>&1 || die "gzip not found"
if command -v sha256sum >/dev/null 2>&1; then
  SHA256SUM="sha256sum"
else
  command -v shasum >/dev/null 2>&1 || die "no sha256 tool found (install sha256sum or shasum)"
  SHA256SUM="shasum -a 256"
fi

# A relative --output is anchored before staging (the publisher passes
# dist/<target>.tar.gz from the repository root).
if [[ "$OUTPUT" != /* ]]; then
  OUTPUT="$PWD/${OUTPUT#./}"
fi
OUT_DIR="$(dirname "$OUTPUT")"

DESCRIPTOR_PATH="$SOURCE/$DESCRIPTOR_FILE"
[[ -f "$DESCRIPTOR_PATH" ]] || die "source descriptor not found: $DESCRIPTOR_PATH"
DESC_SIZE="$(wc -c <"$DESCRIPTOR_PATH" | tr -d ' ')"
[[ "$DESC_SIZE" -le "$DESCRIPTOR_MAX_BYTES" ]] || die "source descriptor exceeds $DESCRIPTOR_MAX_BYTES bytes"

# --- closed descriptor subset: exactly [component] + the five known keys ---
DESC_NAME=""
DESC_VERSION=""
DESC_PROTOCOL_MIN=""
DESC_PROTOCOL_MAX=""
DESC_EXECUTABLE=""
DESC_SHA256=""
DESC_SECTION=""
DESC_LINENO=0
while IFS= read -r line || [[ -n "$line" ]]; do
  DESC_LINENO=$((DESC_LINENO + 1))
  # Tolerate one trailing carriage return (CRLF checkout); an embedded
  # CR matches no grammar rule below and fails closed as malformed.
  line=${line%$'\r'}
  # Skip blanks and full-line comments.
  [[ "$line" =~ ^[[:space:]]*$ ]] && continue
  [[ "$line" =~ ^[[:space:]]*# ]] && continue
  if [[ "$line" =~ ^\[(.*)\]$ ]]; then
    [[ "${BASH_REMATCH[1]}" == "component" ]] || die "source descriptor line $DESC_LINENO: unknown table [${BASH_REMATCH[1]}]"
    [[ -z "$DESC_SECTION" ]] || die "source descriptor line $DESC_LINENO: duplicate [component] table"
    DESC_SECTION="component"
    continue
  fi
  [[ -n "$DESC_SECTION" ]] || die "source descriptor line $DESC_LINENO: key outside [component]"
  if [[ "$line" =~ ^([A-Za-z0-9_]+)[[:space:]]*=[[:space:]]*(.*)$ ]]; then
    key="${BASH_REMATCH[1]}"
    raw="${BASH_REMATCH[2]}"
    case "$key" in
    name | version | executable | sha256)
      [[ "$raw" =~ ^\"([^\"]*)\"[[:space:]]*(#.*)?$ ]] || die "source descriptor line $DESC_LINENO: '$key' must be a double-quoted string"
      value="${BASH_REMATCH[1]}"
      ;;
    protocol)
      [[ "$raw" =~ ^\[[[:space:]]*([0-9]+)[[:space:]]*,[[:space:]]*([0-9]+)[[:space:]]*\][[:space:]]*(#.*)?$ ]] || die "source descriptor line $DESC_LINENO: 'protocol' must look like [min, max]"
      [[ -z "$DESC_PROTOCOL_MIN" ]] || die "source descriptor line $DESC_LINENO: duplicate 'protocol'"
      DESC_PROTOCOL_MIN="${BASH_REMATCH[1]}"
      DESC_PROTOCOL_MAX="${BASH_REMATCH[2]}"
      continue
      ;;
    *)
      die "source descriptor line $DESC_LINENO: unknown key '$key'"
      ;;
    esac
    case "$key" in
    name)
      [[ -z "$DESC_NAME" ]] || die "source descriptor line $DESC_LINENO: duplicate 'name'"
      DESC_NAME="$value"
      ;;
    version)
      [[ -z "$DESC_VERSION" ]] || die "source descriptor line $DESC_LINENO: duplicate 'version'"
      DESC_VERSION="$value"
      ;;
    executable)
      [[ -z "$DESC_EXECUTABLE" ]] || die "source descriptor line $DESC_LINENO: duplicate 'executable'"
      DESC_EXECUTABLE="$value"
      ;;
    sha256)
      [[ -z "$DESC_SHA256" ]] || die "source descriptor line $DESC_LINENO: duplicate 'sha256'"
      DESC_SHA256="$value"
      ;;
    esac
  else
    die "source descriptor line $DESC_LINENO: malformed line"
  fi
done <"$DESCRIPTOR_PATH"

[[ -n "$DESC_NAME" ]] || die "source descriptor is missing 'name'"
[[ -n "$DESC_VERSION" ]] || die "source descriptor is missing 'version'"
[[ -n "$DESC_PROTOCOL_MIN" ]] || die "source descriptor is missing 'protocol'"
[[ -n "$DESC_EXECUTABLE" ]] || die "source descriptor is missing 'executable'"
valid_name "$DESC_NAME" || die "source descriptor name must match [a-z][a-z0-9-]{0,31}, got: $DESC_NAME"
valid_version "$DESC_VERSION" || die "source descriptor version must look like X.Y.Z, got: $DESC_VERSION"
[[ "$DESC_VERSION" == "$VERSION" ]] || die "descriptor version $DESC_VERSION does not match --version $VERSION"
if [[ "$DESC_PROTOCOL_MIN" -lt 1 || "$DESC_PROTOCOL_MIN" -gt 65535 || "$DESC_PROTOCOL_MAX" -lt 1 || "$DESC_PROTOCOL_MAX" -gt 65535 ]]; then
  die "source descriptor protocol out of u16 range: [$DESC_PROTOCOL_MIN, $DESC_PROTOCOL_MAX]"
fi
[[ "$DESC_PROTOCOL_MIN" -le "$DESC_PROTOCOL_MAX" ]] || die "source descriptor protocol min exceeds max: [$DESC_PROTOCOL_MIN, $DESC_PROTOCOL_MAX]"
[[ "$DESC_EXECUTABLE" == "$EXECUTABLE_PREFIX$DESC_NAME" ]] || die "source descriptor executable must be $EXECUTABLE_PREFIX$DESC_NAME, got: $DESC_EXECUTABLE"
[[ -n "$DESC_SHA256" ]] || die "source descriptor is missing 'sha256'"
[[ "$DESC_SHA256" =~ ^[0-9a-f]{64}$ ]] || die "source descriptor sha256 must be 64 lowercase hex characters"

BIN_SIZE="$(wc -c <"$BINARY" | tr -d ' ')"
[[ "$BIN_SIZE" -gt 0 ]] || die "--binary is empty: $BINARY"
[[ "$BIN_SIZE" -le "$EXE_MAX_BYTES" ]] || die "--binary exceeds $EXE_MAX_BYTES bytes: $BINARY"
BIN_DIGEST="$($SHA256SUM "$BINARY" | cut -d' ' -f1)"
if [[ "$DESC_SHA256" != "$BIN_DIGEST" ]]; then
  die "source descriptor sha256 $DESC_SHA256 does not match computed digest $BIN_DIGEST for $BINARY"
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-component-dist.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

cp "$DESCRIPTOR_PATH" "$STAGE/$DESCRIPTOR_FILE"
cp "$BINARY" "$STAGE/$DESC_EXECUTABLE"
chmod 755 "$STAGE/$DESC_EXECUTABLE"

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "make-component-dist: dry-run payload:"
  (cd "$STAGE" && ls -l "$DESCRIPTOR_FILE" "$DESC_EXECUTABLE")
  echo "make-component-dist: dry-run would run:"
  echo "  tar -czf $OUTPUT -C \$STAGE $DESCRIPTOR_FILE $DESC_EXECUTABLE"
  echo "make-component-dist: dry-run PASS (name=$DESC_NAME, version=$VERSION, target=$TARGET)"
  exit 0
fi

mkdir -p "$OUT_DIR"
# tar refuses to compress over an existing archive in place, so start from
# a clean path (same reason the bundle scripts rm -f first).
rm -f "$OUTPUT"
tar -czf "$OUTPUT" -C "$STAGE" "$DESCRIPTOR_FILE" "$DESC_EXECUTABLE"

# The member list is fixed: assert the archive holds exactly the staged
# payload at the archive root, so no absolute path, traversal, or stray
# file can slip into a component release.
EXPECTED="$(printf '%s\n%s\n' "$DESCRIPTOR_FILE" "$DESC_EXECUTABLE" | LC_ALL=C sort)"
ACTUAL="$(tar -tzf "$OUTPUT" | LC_ALL=C sort)"
if [[ "$ACTUAL" != "$EXPECTED" ]]; then
  rm -f "$OUTPUT"
  die "unexpected archive entries:
$ACTUAL"
fi

# Per-artifact sidecar (one line, GNU sha256sum format) for single-file
# verification, mirroring the release <.sha256> sidecars.
(cd "$OUT_DIR" && $SHA256SUM "$(basename "$OUTPUT")" >"$(basename "$OUTPUT").sha256")

# Aggregate manifest over every target tarball in the output dir (the file
# the publisher uploads as bitty/components/<name>/<version>/SHA256SUMS).
shopt -s nullglob
TARBALLS=()
for candidate in "$OUT_DIR"/*.tar.gz; do
  base="$(basename "$candidate")"
  base_noext="${base%.tar.gz}"
  if valid_target "$base_noext"; then
    TARBALLS+=("$base")
  fi
done
shopt -u nullglob
[[ "${#TARBALLS[@]}" -gt 0 ]] || die "no target tarballs in $OUT_DIR"
(cd "$OUT_DIR" && $SHA256SUM "${TARBALLS[@]}" | LC_ALL=C sort -k2 >SHA256SUMS)
(cd "$OUT_DIR" && $SHA256SUM -c SHA256SUMS >/dev/null) || die "fresh SHA256SUMS failed to verify"

echo "make-component-dist: PASS (name=$DESC_NAME, version=$VERSION, target=$TARGET, output=$OUTPUT)"
