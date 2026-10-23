#!/usr/bin/env bash
# check-distro-recipes.test.sh — verify Linux distribution packaging templates.
#
# Asserts integrity of packaging recipes for:
#   - Alpine Linux (aports APKBUILD)
#   - Gentoo Linux (ebuilds for source & prebuilt binary + overlay config)
#   - Void Linux (xbps-src template)
#
# Part of CTX-1471 (#2513, #2516, #2517).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FAIL=0

pass() {
  echo "PASS: $1"
}

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

# 1. Alpine Linux APKBUILD
ALPINE_APKBUILD="$REPO_ROOT/packaging/alpine/APKBUILD"
if [[ -f "$ALPINE_APKBUILD" ]]; then
  grep -q '^pkgname=bitty' "$ALPINE_APKBUILD" || fail "Alpine APKBUILD missing pkgname=bitty"
  grep -q '^options="net"' "$ALPINE_APKBUILD" || fail "Alpine APKBUILD missing options=\"net\""
  grep -q 'cargo auditable build' "$ALPINE_APKBUILD" || fail "Alpine APKBUILD missing cargo auditable build"
  grep -q 'tic -x' "$ALPINE_APKBUILD" || fail "Alpine APKBUILD missing tic -x"
  grep -q 'target-feature=-crt-static' "$ALPINE_APKBUILD" || fail "Alpine APKBUILD missing dynamic musl flag"
  pass "Alpine APKBUILD integrity"
else
  fail "Alpine APKBUILD not found at $ALPINE_APKBUILD"
fi

# 2. Gentoo ebuilds & overlay metadata
GENTOO_REPO_NAME="$REPO_ROOT/packaging/gentoo/profiles/repo_name"
GENTOO_LAYOUT="$REPO_ROOT/packaging/gentoo/metadata/layout.conf"
GENTOO_SRC_EBUILD="$REPO_ROOT/packaging/gentoo/x11-terms/bitty/bitty-0.0.27.ebuild"
GENTOO_BIN_EBUILD="$REPO_ROOT/packaging/gentoo/x11-terms/bitty-bin/bitty-bin-0.0.27.ebuild"

if [[ -f "$GENTOO_REPO_NAME" ]] && grep -q '^bitty' "$GENTOO_REPO_NAME"; then
  pass "Gentoo repo_name exists and contains 'bitty'"
else
  fail "Gentoo repo_name invalid or missing"
fi

if [[ -f "$GENTOO_LAYOUT" ]] && grep -q 'masters = gentoo' "$GENTOO_LAYOUT"; then
  pass "Gentoo layout.conf valid"
else
  fail "Gentoo layout.conf invalid or missing"
fi

if [[ -f "$GENTOO_SRC_EBUILD" ]]; then
  grep -q 'inherit cargo desktop xdg' "$GENTOO_SRC_EBUILD" || fail "Gentoo src ebuild missing eclass inherits"
  grep -q 'cargo_src_compile -p bitty-terminal --bin bitty' "$GENTOO_SRC_EBUILD" || fail "Gentoo src ebuild missing cargo_src_compile"
  grep -q 'tic -x' "$GENTOO_SRC_EBUILD" || fail "Gentoo src ebuild missing tic -x"
  pass "Gentoo source ebuild integrity"
else
  fail "Gentoo source ebuild not found"
fi

if [[ -f "$GENTOO_BIN_EBUILD" ]]; then
  grep -q 'inherit desktop xdg' "$GENTOO_BIN_EBUILD" || fail "Gentoo bin ebuild missing eclass inherits"
  grep -q 'https://cdn.bitty.run/releases/v\${PV}/' "$GENTOO_BIN_EBUILD" || fail "Gentoo bin ebuild missing CDN SRC_URI"
  grep -q 'tic -x' "$GENTOO_BIN_EBUILD" || fail "Gentoo bin ebuild missing tic -x"
  pass "Gentoo binary ebuild integrity"
else
  fail "Gentoo binary ebuild not found"
fi

# 3. Void Linux xbps-src template
VOID_TEMPLATE="$REPO_ROOT/packaging/void/srcpkgs/bitty/template"
if [[ -f "$VOID_TEMPLATE" ]]; then
  grep -q '^pkgname=bitty' "$VOID_TEMPLATE" || fail "Void template missing pkgname=bitty"
  grep -q '^build_style=cargo' "$VOID_TEMPLATE" || fail "Void template missing build_style=cargo"
  grep -q 'make_install_args=.*--path crates/bitty-terminal' "$VOID_TEMPLATE" || fail "Void template missing path to crates/bitty-terminal"
  grep -q 'vdesk ' "$VOID_TEMPLATE" || fail "Void template missing vdesk helper"
  grep -q 'tic -x' "$VOID_TEMPLATE" || fail "Void template missing tic -x"
  pass "Void Linux template integrity"
else
  fail "Void Linux template not found at $VOID_TEMPLATE"
fi

# 4. Documentation coverage
for doc in packaging/alpine.md packaging/gentoo.md packaging/void.md; do
  if [[ -f "$REPO_ROOT/$doc" ]]; then
    pass "Documentation exists: $doc"
  else
    fail "Documentation missing: $doc"
  fi
done

if grep -q 'packaging/gentoo.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/void.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/alpine/APKBUILD' "$REPO_ROOT/packaging/INSTALL.md"; then
  pass "INSTALL.md references all three distro packaging guides"
else
  fail "INSTALL.md missing references to distro recipes"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-distro-recipes.test.sh: FAIL" >&2
  exit 1
fi

echo "check-distro-recipes.test.sh: ALL PASS"
