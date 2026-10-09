#!/usr/bin/env bash
# install.sh — first-class Bitty installer for Linux and macOS (CTX-1047, #1847).
#
# Entry points (stable, served from the CDN):
#   curl -fsSL https://cdn.bitty.run/bitty/install/install.sh | bash
#   curl -fsSL https://cdn.bitty.run/bitty/install/install.sh | bash -s -- --version 0.0.23
#
# The script resolves the release through one pointer:
#   https://cdn.bitty.run/bitty/install/latest.txt
# which holds TAG (for example `v0.0.23`) with a trailing newline. VERSION is
# TAG without the leading `v` and is what bundle names embed.
#
# Fetch contract: literal per-OS/arch URL templates from the R2 layout decision
# (packaging/README.md, CTX-1056, closes #1794). Only TAG and VERSION resolve
# at install time; OS and arch segments below are literal:
#   Linux x86_64 glibc:
#     https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-gnu.tar.zst
#   Linux aarch64 glibc:
#     https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-aarch64-unknown-linux-gnu.tar.zst
#   Linux x86_64 musl (Alpine):
#     https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-musl.tar.zst
#   macOS arm64:
#     https://cdn.bitty.run/bitty/releases/<TAG>/bitty-aarch64-apple-darwin
#   macOS x86_64:
#     https://cdn.bitty.run/bitty/releases/<TAG>/bitty-x86_64-apple-darwin
# Each row has a `<artifact>.sha256` sidecar at the same URL with `.sha256`
# appended, verified before installing (fail closed on mismatch). There are no
# signatures in 0.1.0: verification is hash-only (Windows signing deferred
# past 0.2.0 per #1810; Sigstore/cosign stays a follow-up).
#
# Rationale per OS (from the layout decision): Linux fetches the versioned
# `.tar.zst` bundle (bin/bitty plus desktop entry, icons, AppStream metainfo),
# not the bare triple binary; macOS fetches the bare per-arch binary (the DMG
# is an interactive drag-to-install image, unsuitable for headless install,
# and the bare binary is already the Homebrew fetch artifact). FreeBSD Tier 2
# is not a bootstrap target and fails closed with manual-download guidance.
# Windows is not served here: uname reports MINGW/MSYS/CYGWIN under Git-Bash
# and the script fails closed pointing at install.ps1 (irm|iex entry point).
#
# No Rust toolchain is needed: the script needs only curl, sha256sum (or
# shasum), and tar+zstd for Linux bundles. Scoop/Homebrew/AUR manifests are
# generated at tag time into external repos and need no code here; the Windows
# x64 portable ZIP installed by install.ps1 carries the same bitty.exe payload
# the Scoop manifest ships (portable parity is payload parity, not URL parity).
#
# Usage:
#   install.sh [--version VERSION] [--prefix DIR] [--bin-dir DIR] [--dry-run] [--print-url] [--target TRIPLE] [-h|--help]
#
# Options:
#   --version VERSION  release version to install (X.Y.Z or vX.Y.Z); default
#                      resolves https://cdn.bitty.run/bitty/install/latest.txt
#   --prefix DIR       install prefix (binary goes to DIR/bin); default uses
#                      HOME/.local (binary HOME/.local/bin)
#   --bin-dir DIR      binary directory override (wins over --prefix;
#                      desktop integration is skipped when set without --prefix,
#                      so custom binary dirs stay hermetic)
#   --dry-run          resolve and print the fetch/install plan without
#                      downloading or installing
#   --print-url        print the artifact URL and its .sha256 URL, then exit
#   --target TRIPLE    override auto-detected Rust target (test/manual only)
#   -h, --help         show this help
#
# Environment (test/manual overrides only; defaults serve production):
#   BITTY_VERSION      same as --version
#   BITTY_CDN_BASE     CDN base override (default https://cdn.bitty.run)
#   BITTY_TARGET       same as --target
#
# Exit codes: 0 = installed (or plan printed), 1 = install/fetch/verify
# failure or unsupported platform, 2 = usage error.
set -euo pipefail

export LC_ALL=C

CDN_DEFAULT="https://cdn.bitty.run"
LATEST_SUFFIX="bitty/install/latest.txt"

VERSION_ARG=""
PREFIX=""
BIN_DIR_ARG=""
DRY_RUN=0
PRINT_URL=0
TARGET_OVERRIDE=""

usage() {
  cat <<'EOF'
Usage: install.sh [--version VERSION] [--prefix DIR] [--bin-dir DIR] [--dry-run] [--print-url] [--target TRIPLE] [-h|--help]

  --version VERSION  release version to install (X.Y.Z or vX.Y.Z); default
                     resolves https://cdn.bitty.run/bitty/install/latest.txt
  --prefix DIR       install prefix (binary goes to DIR/bin)
  --bin-dir DIR      binary directory override (wins over --prefix;
                       desktop integration is skipped when set without --prefix)
  --dry-run          resolve and print the fetch/install plan only
  --print-url        print the artifact URL and its .sha256 URL, then exit
  --target TRIPLE    override auto-detected Rust target (test/manual only)
  -h, --help         show this help

Installs bitty with no Rust toolchain. Linux fetches the versioned .tar.zst
bundle; macOS fetches the bare per-arch binary. Checksums are verified and
fail closed. FreeBSD and Windows fail closed with guidance.
EOF
}

die() {
  echo "install.sh: ERROR: $1" >&2
  exit 1
}

usage_error() {
  echo "install.sh: ERROR: $1" >&2
  echo "run with --help" >&2
  exit 2
}

log() {
  echo "install.sh: $1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  --version)
    [[ $# -ge 2 ]] || usage_error "--version needs a value"
    VERSION_ARG="$2"
    shift 2
    ;;
  --prefix)
    [[ $# -ge 2 ]] || usage_error "--prefix needs a value"
    PREFIX="$2"
    shift 2
    ;;
  --bin-dir)
    [[ $# -ge 2 ]] || usage_error "--bin-dir needs a value"
    BIN_DIR_ARG="$2"
    shift 2
    ;;
  --dry-run)
    DRY_RUN=1
    shift
    ;;
  --print-url)
    PRINT_URL=1
    shift
    ;;
  --target)
    [[ $# -ge 2 ]] || usage_error "--target needs a value"
    TARGET_OVERRIDE="$2"
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

CDN_BASE="${BITTY_CDN_BASE:-$CDN_DEFAULT}"
# A trailing slash would double up path separators in fetch URLs.
CDN_BASE="${CDN_BASE%/}"
[[ -n "$TARGET_OVERRIDE" ]] || TARGET_OVERRIDE="${BITTY_TARGET:-}"
VERSION_REQ="${VERSION_ARG:-${BITTY_VERSION:-}}"

command -v curl >/dev/null 2>&1 || die "curl not found (install curl first)"
command -v uname >/dev/null 2>&1 || die "uname not found"

# normalize_version <raw> — accept X.Y.Z or vX.Y.Z, reject prereleases.
normalize_version() {
  local raw="$1"
  # Strip carriage returns and surrounding whitespace from pinned inputs.
  raw="$(printf '%s' "$raw" | tr -d '\r' | tr -d ' \t\n')"
  if [[ "$raw" == *"-"* ]]; then
    die "prerelease versions are not served by the bootstrap scripts ($raw); download the release asset manually"
  fi
  raw="${raw#v}"
  [[ "$raw" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must look like X.Y.Z, got: $1"
  printf 'v%s' "$raw"
}

resolve_tag() {
  if [[ -n "$VERSION_REQ" ]]; then
    normalize_version "$VERSION_REQ"
    return
  fi
  local ptr_url="${CDN_BASE}/${LATEST_SUFFIX}"
  local tag
  tag="$(curl -fsSL "$ptr_url" 2>/dev/null)" || die "could not fetch version pointer $ptr_url"
  tag="$(printf '%s' "$tag" | tr -d '\r' | tr -d ' \t\n')"
  [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version pointer $ptr_url returned unexpected content: $tag"
  printf '%s' "$tag"
}

is_musl() {
  if [[ -f /etc/alpine-release ]]; then
    return 0
  fi
  if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    return 0
  fi
  return 1
}

detect_target() {
  if [[ -n "$TARGET_OVERRIDE" ]]; then
    case "$TARGET_OVERRIDE" in
    x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-unknown-linux-musl | x86_64-apple-darwin | aarch64-apple-darwin)
      printf '%s' "$TARGET_OVERRIDE"
      return
      ;;
    *)
      die "unsupported --target override: $TARGET_OVERRIDE"
      ;;
    esac
  fi
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
  Linux)
    case "$arch" in
    x86_64 | amd64)
      if is_musl; then
        printf 'x86_64-unknown-linux-musl'
      else
        printf 'x86_64-unknown-linux-gnu'
      fi
      ;;
    aarch64 | arm64)
      if is_musl; then
        die "musl aarch64 has no bootstrap bundle; install the glibc bundle manually or use a package manager"
      fi
      printf 'aarch64-unknown-linux-gnu'
      ;;
    *)
      die "unsupported Linux arch: $arch (bootstrap serves x86_64 and aarch64)"
      ;;
    esac
    ;;
  Darwin)
    case "$arch" in
    x86_64)
      printf 'x86_64-apple-darwin'
      ;;
    arm64 | aarch64)
      printf 'aarch64-apple-darwin'
      ;;
    *)
      die "unsupported macOS arch: $arch (bootstrap serves x86_64 and arm64)"
      ;;
    esac
    ;;
  FreeBSD)
    die "FreeBSD is not a bootstrap target; download bitty-<version>-x86_64-unknown-freebsd.tar.xz from the release page manually"
    ;;
  MINGW* | MSYS* | CYGWIN*)
    die "Windows Git-Bash detected; run install.ps1 instead: irm https://cdn.bitty.run/bitty/install/install.ps1 | iex"
    ;;
  *)
    die "unsupported OS: $os (bootstrap serves Linux and macOS; Windows uses install.ps1)"
    ;;
  esac
}

# artifact_name <version-without-v> <target> — literal layout-table mapping.
artifact_name() {
  local version="$1" target="$2"
  case "$target" in
  x86_64-unknown-linux-gnu)
    printf 'bitty-%s-x86_64-unknown-linux-gnu.tar.zst' "$version"
    ;;
  aarch64-unknown-linux-gnu)
    printf 'bitty-%s-aarch64-unknown-linux-gnu.tar.zst' "$version"
    ;;
  x86_64-unknown-linux-musl)
    printf 'bitty-%s-x86_64-unknown-linux-musl.tar.zst' "$version"
    ;;
  x86_64-apple-darwin)
    printf 'bitty-x86_64-apple-darwin'
    ;;
  aarch64-apple-darwin)
    printf 'bitty-aarch64-apple-darwin'
    ;;
  *)
    die "no bootstrap artifact for target: $target"
    ;;
  esac
}

sha256_of_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | cut -d' ' -f1
  else
    die "no sha256 tool found (install sha256sum or shasum)"
  fi
}

verify_sidecar() {
  local file="$1" sidecar="$2"
  local want got
  want="$(cut -d' ' -f1 "$sidecar" | tr -d '\r \t\n')"
  [[ "$want" =~ ^[0-9a-fA-F]{64}$ ]] || die "sidecar $sidecar did not yield a 64-hex digest"
  got="$(sha256_of_file "$file")"
  if [[ "${want,,}" != "${got,,}" ]]; then
    die "checksum mismatch for $(basename "$file") (expected $want, got $got)"
  fi
}

TAG="$(resolve_tag)"
VERSION="${TAG#v}"
TARGET="$(detect_target)"
ARTIFACT="$(artifact_name "$VERSION" "$TARGET")"
URL="${CDN_BASE}/bitty/releases/${TAG}/${ARTIFACT}"
SHA_URL="${URL}.sha256"

if [[ "$PRINT_URL" -eq 1 ]]; then
  printf '%s\n%s\n' "$URL" "$SHA_URL"
  exit 0
fi

# Resolve install directories (derived from HOME or --prefix, never hardcoded).
if [[ -n "$BIN_DIR_ARG" ]]; then
  BIN_DIR="$BIN_DIR_ARG"
elif [[ -n "$PREFIX" ]]; then
  BIN_DIR="${PREFIX%/}/bin"
else
  [[ -n "${HOME:-}" ]] || die "HOME is not set and no --prefix/--bin-dir was given"
  BIN_DIR="${HOME}/.local/bin"
fi
if [[ -n "$PREFIX" ]]; then
  SHARE_BASE="${PREFIX%/}/share"
else
  [[ -n "${HOME:-}" ]] || die "HOME is not set and no --prefix was given"
  SHARE_BASE="${HOME}/.local/share"
fi

case "$TARGET" in
*-unknown-linux-*)
  KIND="bundle"
  ;;
*-apple-darwin)
  KIND="binary"
  ;;
*)
  die "no bootstrap artifact for target: $TARGET"
  ;;
esac

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "dry-run: version pointer: ${CDN_BASE}/${LATEST_SUFFIX}"
  log "dry-run: TAG=$TAG VERSION=$VERSION TARGET=$TARGET KIND=$KIND"
  log "dry-run: artifact URL: $URL"
  log "dry-run: sidecar URL: $SHA_URL"
  log "dry-run: binary dir: $BIN_DIR"
  if [[ "$KIND" == "bundle" ]]; then
    if [[ -n "$BIN_DIR_ARG" && -z "$PREFIX" ]]; then
      log "dry-run: desktop integration skipped (custom --bin-dir without --prefix)"
    else
      log "dry-run: share dir: $SHARE_BASE"
    fi
  fi
  log "dry-run: would download, verify sha256, and install bitty (no changes made)"
  exit 0
fi

if [[ "$KIND" == "bundle" ]]; then
  command -v tar >/dev/null 2>&1 || die "tar not found (needed to extract the Linux bundle)"
  command -v zstd >/dev/null 2>&1 || die "zstd not found (needed to extract the Linux .tar.zst bundle)"
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-install.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

log "resolving bitty $TAG for $TARGET"
log "fetching $URL"
curl -fsSL -o "$STAGE/$ARTIFACT" "$URL" || die "download failed: $URL"
log "fetching $SHA_URL"
curl -fsSL -o "$STAGE/$ARTIFACT.sha256" "$SHA_URL" || die "download failed: $SHA_URL"
verify_sidecar "$STAGE/$ARTIFACT" "$STAGE/$ARTIFACT.sha256"
log "checksum OK: $ARTIFACT"

mkdir -p "$BIN_DIR"
if [[ "$KIND" == "binary" ]]; then
  cp "$STAGE/$ARTIFACT" "$BIN_DIR/bitty"
  chmod 755 "$BIN_DIR/bitty"
else
  log "extracting Linux bundle"
  tar -I zstd -xf "$STAGE/$ARTIFACT" -C "$STAGE" || die "bundle extraction failed"
  TOPDIR="bitty-$VERSION-$TARGET"
  [[ -f "$STAGE/$TOPDIR/bin/bitty" ]] || die "bundle missing $TOPDIR/bin/bitty"
  cp "$STAGE/$TOPDIR/bin/bitty" "$BIN_DIR/bitty"
  chmod 755 "$BIN_DIR/bitty"
  # A custom --bin-dir without --prefix is binary-only on purpose, so test
  # and manual binary drops never touch the shared HOME prefix.
  if [[ -n "$BIN_DIR_ARG" && -z "$PREFIX" ]]; then
    log "skipping desktop integration (custom --bin-dir without --prefix)"
  elif [[ -d "$STAGE/$TOPDIR/share" ]]; then
    mkdir -p "$SHARE_BASE"
    cp -r "$STAGE/$TOPDIR/share/." "$SHARE_BASE/" || die "desktop integration install failed"
    log "installed desktop integration under $SHARE_BASE"
  fi
fi

log "installed bitty to $BIN_DIR/bitty"
if "$BIN_DIR/bitty" --version >/dev/null 2>&1; then
  log "smoke OK: $("$BIN_DIR/bitty" --version)"
else
  die "install smoke failed: $BIN_DIR/bitty --version did not run"
fi
log "ensure $BIN_DIR is on PATH"
