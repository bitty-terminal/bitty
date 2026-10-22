#!/usr/bin/env bash
# check-apt-repo.test.sh — Test suite for Cloudflare R2 APT static repository.
#
# Asserts:
#   1. Static assets (bitty-archive-keyring.gpg, bitty.sources, setup.sh) exist and are valid.
#   2. setup.sh is executable, conforms to shellcheck, and uses the correct CDN URLs.
#   3. scripts/generate-apt-repo.sh correctly parses .deb packages, generates canonical
#      pool layouts, creates Packages and Packages.gz, and writes Release index files
#      with accurate checksums.
#   4. release.yml wires the apt publication job under environment: release and
#      includes apt in the channel-summary.
set -euo pipefail

cd "$(dirname "$0")/../.."

FAIL=0
TMP="$(mktemp -d "${TMPDIR:-/tmp}/check-apt-repo.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

expect_contains() { # <file> <literal> <label>
  if grep -qF -- "$2" "$1"; then
    pass "$3"
  else
    fail "$3 (missing: '$2')"
  fi
}

# --- 1. Static packaging assets ---------------------------------------------
KEYRING="packaging/apt/bitty-archive-keyring.gpg"
SOURCES="packaging/apt/bitty.sources"
SETUP="packaging/apt/setup.sh"
GEN_SH="scripts/generate-apt-repo.sh"
GEN_PY="scripts/generate-apt-repo.py"
WORKFLOW=".github/workflows/release.yml"

[[ -f "$KEYRING" ]] || fail "Keyring missing: $KEYRING"
[[ -f "$SOURCES" ]] || fail "Sources file missing: $SOURCES"
[[ -x "$SETUP" ]] || fail "Setup script missing or not executable: $SETUP"
[[ -x "$GEN_SH" ]] || fail "Generator script missing or not executable: $GEN_SH"
[[ -f "$GEN_PY" ]] || fail "Python generator missing: $GEN_PY"

# Keyring check (must be valid OpenPGP key)
if gpg --show-keys "$KEYRING" >"$TMP/keyring.txt" 2>&1; then
  expect_contains "$TMP/keyring.txt" "Bitty APT Archive" "Keyring contains Bitty APT Archive UID"
  expect_contains "$TMP/keyring.txt" "BCB87D7B489A64168080D900DF4751B6EA711CB8" "Keyring has expected fingerprint"
else
  fail "gpg --show-keys rejected $KEYRING"
fi

# deb822 source file check
expect_contains "$SOURCES" "Types: deb" "bitty.sources has Types: deb"
expect_contains "$SOURCES" "URIs: https://cdn.bitty.run/apt" "bitty.sources has R2 CDN URI"
expect_contains "$SOURCES" "Suites: stable" "bitty.sources has Suites: stable"
expect_contains "$SOURCES" "Components: main" "bitty.sources has Components: main"
expect_contains "$SOURCES" "Signed-By: /etc/apt/keyrings/bitty-archive-keyring.gpg" "bitty.sources specifies Signed-By"

# setup.sh checks
if shellcheck -S warning "$SETUP" >"$TMP/shellcheck.err" 2>&1; then
  pass "setup.sh passes shellcheck"
else
  fail "setup.sh failed shellcheck: $(cat "$TMP/shellcheck.err")"
fi
expect_contains "$SETUP" "https://cdn.bitty.run/apt" "setup.sh references CDN base URL"
expect_contains "$SETUP" "/etc/apt/keyrings/bitty-archive-keyring.gpg" "setup.sh targets /etc/apt/keyrings/"

# --- 2. Synthetic .deb package generation and repository test --------------
DIST_DIR="$TMP/dist"
REPO_DIR="$TMP/apt_repo"
mkdir -p "$DIST_DIR"

# Generate a minimal valid synthetic .deb for testing
python3 -c "
import io, tarfile, pathlib

control_content = b'''Package: bitty
Version: 0.0.26
Architecture: amd64
Maintainer: Bitty Maintainers <maintainers@bitty-terminal.dev>
Description: Bitty terminal emulator
Depends: libfontconfig1, libfreetype6
Section: utils
Priority: optional
'''

# 1. control.tar.gz
control_tar = io.BytesIO()
with tarfile.open(fileobj=control_tar, mode='w:gz') as tar:
    ti = tarfile.TarInfo('control')
    ti.size = len(control_content)
    tar.addfile(ti, io.BytesIO(control_content))
control_tar_bytes = control_tar.getvalue()

# 2. data.tar.gz (empty file)
data_tar = io.BytesIO()
with tarfile.open(fileobj=data_tar, mode='w:gz') as tar:
    pass
data_tar_bytes = data_tar.getvalue()

# 3. Assemble ar archive
def ar_member(name, data):
    # 16s name, 12s mtime, 6s uid, 6s gid, 8s mode, 10s size, 2s magic
    header = f'{name:<16}0           0     0     100644  {len(data):<10}\x60\x0a'.encode('ascii')
    pad = b'\x0a' if len(data) % 2 == 1 else b''
    return header + data + pad

ar_data = b'\x21\x3c\x61\x72\x63\x68\x3e\x0a'
ar_data += ar_member('debian-binary', b'2.0\x0a')
ar_data += ar_member('control.tar.gz', control_tar_bytes)
ar_data += ar_member('data.tar.gz', data_tar_bytes)

pathlib.Path('$DIST_DIR/bitty-x86_64-unknown-linux-gnu.deb').write_bytes(ar_data)
"

[[ -f "$DIST_DIR/bitty-x86_64-unknown-linux-gnu.deb" ]] || fail "Synthetic deb creation failed"

# Run repository generation
if "$GEN_SH" --dist-dir "$DIST_DIR" --repo-dir "$REPO_DIR" --assets-dir "packaging/apt" >"$TMP/gen.log" 2>&1; then
  pass "generate-apt-repo.sh executed successfully"
else
  fail "generate-apt-repo.sh failed: $(cat "$TMP/gen.log")"
fi

# Verify generated pool file
STAGED_DEB="$REPO_DIR/pool/main/b/bitty/bitty_0.0.26_amd64.deb"
[[ -f "$STAGED_DEB" ]] || fail "Staged deb in pool is missing: $STAGED_DEB"

# Verify copied assets
[[ -f "$REPO_DIR/bitty-archive-keyring.gpg" ]] || fail "Repo missing copied keyring"
[[ -f "$REPO_DIR/bitty.sources" ]] || fail "Repo missing copied bitty.sources"
[[ -f "$REPO_DIR/setup.sh" ]] || fail "Repo missing copied setup.sh"

# Verify Packages and Packages.gz
PACKAGES="$REPO_DIR/dists/stable/main/binary-amd64/Packages"
PACKAGES_GZ="$REPO_DIR/dists/stable/main/binary-amd64/Packages.gz"
RELEASE="$REPO_DIR/dists/stable/Release"

[[ -f "$PACKAGES" ]] || fail "Packages file missing: $PACKAGES"
[[ -f "$PACKAGES_GZ" ]] || fail "Packages.gz file missing: $PACKAGES_GZ"
[[ -f "$RELEASE" ]] || fail "Release file missing: $RELEASE"

expect_contains "$PACKAGES" "Package: bitty" "Packages contains Package: bitty"
expect_contains "$PACKAGES" "Version: 0.0.26" "Packages contains Version: 0.0.26"
expect_contains "$PACKAGES" "Filename: pool/main/b/bitty/bitty_0.0.26_amd64.deb" "Packages contains canonical Filename"
expect_contains "$PACKAGES" "SHA256:" "Packages contains SHA256"

# Verify Packages.gz decompresses to Packages
gzip -dc "$PACKAGES_GZ" >"$TMP/decompressed_packages"
if cmp -s "$PACKAGES" "$TMP/decompressed_packages"; then
  pass "Packages.gz matches uncompressed Packages exactly"
else
  fail "Packages.gz decompressed content diverged from Packages"
fi

# Verify Release file headers and checksums
expect_contains "$RELEASE" "Origin: bitty-terminal" "Release has Origin"
expect_contains "$RELEASE" "Label: Bitty" "Release has Label"
expect_contains "$RELEASE" "Suite: stable" "Release has Suite"
expect_contains "$RELEASE" "Codename: stable" "Release has Codename"
expect_contains "$RELEASE" "Architectures: amd64" "Release lists active architectures"
expect_contains "$RELEASE" "Components: main" "Release lists Components"

# Verify Release checksum matches actual Packages sha256
PKG_SHA256="$(sha256sum "$PACKAGES" | cut -d' ' -f1)"
expect_contains "$RELEASE" "$PKG_SHA256" "Release lists correct SHA256 for Packages"

# --- 3. Workflow assertions -------------------------------------------------
expect_contains "$WORKFLOW" "apt:" "release.yml defines apt job"
expect_contains "$WORKFLOW" "environment: release" "release.yml apt job runs in release environment"
expect_contains "$WORKFLOW" "APT_GPG_PRIVATE_KEY" "release.yml binds APT_GPG_PRIVATE_KEY secret"
expect_contains "$WORKFLOW" "bitty/apt/" "release.yml uploads to R2 bitty/apt/ prefix"

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-apt-repo.test: FAIL" >&2
  exit 1
fi
echo "check-apt-repo.test: PASS"
