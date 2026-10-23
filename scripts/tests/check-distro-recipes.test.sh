#!/usr/bin/env bash
# check-distro-recipes.test.sh — verify distribution packaging templates.
#
# Asserts integrity of packaging recipes for:
#   - Alpine Linux (aports APKBUILD)
#   - Gentoo Linux (ebuilds for source & prebuilt binary + overlay config)
#   - Void Linux (xbps-src template)
#   - FreeBSD (Ports recipe: Makefile, pkg-plist, pkg-descr, distinfo)
#
# FreeBSD checks are version-agnostic on purpose: the port's PORTVERSION moves
# with each release, so the gate pins structure, not a version string.
#
# Part of CTX-1471 (#2513, #2516, #2517) and CTX-1473 (#2512).
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

# 4. FreeBSD ports recipe (x11/bitty)
FREEBSD_PORT_DIR="$REPO_ROOT/packaging/freebsd/x11/bitty"
FREEBSD_MAKEFILE="$FREEBSD_PORT_DIR/Makefile"
FREEBSD_PLIST="$FREEBSD_PORT_DIR/pkg-plist"
FREEBSD_DESCR="$FREEBSD_PORT_DIR/pkg-descr"
FREEBSD_DISTINFO="$FREEBSD_PORT_DIR/distinfo"

# plist_source_for <plist entry> — print the in-tree file a pkg-plist entry is
# installed from, so a renamed or moved asset fails the gate instead of shipping
# a stale plist.
plist_source_for() {
  local entry="$1"
  case "$entry" in
  share/applications/*) printf '%s\n' "$REPO_ROOT/packaging/${entry#share/applications/}" ;;
  share/metainfo/*) printf '%s\n' "$REPO_ROOT/packaging/${entry#share/metainfo/}" ;;
  share/icons/*) printf '%s\n' "$REPO_ROOT/packaging/${entry#share/}" ;;
  share/doc/bitty/*) printf '%s\n' "$REPO_ROOT/${entry#share/doc/bitty/}" ;;
  *) printf '%s\n' "$REPO_ROOT/$entry" ;;
  esac
}

if [[ -f "$FREEBSD_MAKEFILE" ]]; then
  freebsd_makefile_failures=$FAIL
  grep -Eq '^PORTNAME=[[:space:]]+bitty[[:space:]]*$' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile missing PORTNAME=bitty"
  # Version-agnostic: any numeric PORTVERSION/DISTVERSION passes, so a release
  # bump never needs a test edit.
  grep -Eq '^(PORTVERSION|DISTVERSION)=[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile missing a numeric PORTVERSION/DISTVERSION"
  freebsd_categories="$(sed -n 's/^CATEGORIES=[[:space:]]*//p' "$FREEBSD_MAKEFILE")"
  case " ${freebsd_categories} " in
  *" x11 "* | *" x11-servers "*) ;;
  *) fail "FreeBSD Makefile CATEGORIES lacks x11: '${freebsd_categories}'" ;;
  esac
  grep -Eq '^MASTER_SITES=[[:space:]]+[^[:space:]]+' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile missing MASTER_SITES"
  grep -Eq '^LICENSE=[[:space:]]*MIT[[:space:]]+APACHE20[[:space:]]*$' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile LICENSE is not 'MIT APACHE20' (Cargo.toml/nfpm/PKGBUILD declare both)"
  grep -Eq '^USES=.*cargo' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile does not build with USES=cargo"
  grep -Eq '^[[:space:]]*CARGO_CRATES=' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile has no CARGO_CRATES list (make cargo-crates output)"
  grep -Eq '^do-install:' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD Makefile has no do-install target"
  grep -q 'PREFIX}/bin/bitty' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD do-install does not install bin/bitty"
  grep -q 'tic -x' "$FREEBSD_MAKEFILE" ||
    fail "FreeBSD do-install does not compile terminfo with tic -x"
  if [[ "$FAIL" -eq "$freebsd_makefile_failures" ]]; then
    pass "FreeBSD ports Makefile integrity"
  fi
else
  fail "FreeBSD ports Makefile not found at $FREEBSD_MAKEFILE"
fi

if [[ -s "$FREEBSD_DESCR" ]] && grep -q '^WWW:' "$FREEBSD_DESCR"; then
  pass "FreeBSD pkg-descr is non-empty and carries a WWW line"
else
  fail "FreeBSD pkg-descr missing, empty, or without a WWW line"
fi

if [[ -f "$FREEBSD_DISTINFO" ]] && grep -q 'make makesum' "$FREEBSD_DISTINFO"; then
  pass "FreeBSD distinfo defers checksums to make makesum"
else
  fail "FreeBSD distinfo missing or does not document make makesum"
fi

if [[ -s "$FREEBSD_PLIST" ]]; then
  freebsd_plist_entries=0
  freebsd_plist_failures=$FAIL
  while IFS= read -r raw_line; do
    entry="${raw_line%%[[:space:]]*}"
    [[ -z "$entry" ]] && continue
    case "$entry" in
    \#* | @*) continue ;;
    esac
    entry="${entry#%%PORTDOCS%%}"
    freebsd_plist_entries=$((freebsd_plist_entries + 1))
    case "$entry" in
    bin/bitty) ;; # produced by do-install, asserted above
    share/terminfo/*) ;; # produced by tic from terminfo/bitty.ti
    *)
      source_path="$(plist_source_for "$entry")"
      [[ -f "$source_path" ]] ||
        fail "pkg-plist entry has no in-tree source: $entry (looked for $source_path)"
      ;;
    esac
  done <"$FREEBSD_PLIST"
  if [[ "$freebsd_plist_entries" -eq 0 ]]; then
    fail "FreeBSD pkg-plist lists no files"
  fi
  # The icon sizes do-install copies must be exactly the sizes pkg-plist ships.
  # Both sides are normalised to numeric order: pkg-plist sorts lexicographically
  # (128 before 16), do-install lists sizes in numeric order.
  makefile_sizes="$(sed -n 's/^\.for size in //p' "$FREEBSD_MAKEFILE" |
    tr ' ' '\n' | grep -E '^[0-9]+$' | sort -n | tr '\n' ' ')"
  plist_sizes="$(sed -n 's|^share/icons/hicolor/\([0-9]*\)x[0-9]*/apps/bitty\.png$|\1|p' \
    "$FREEBSD_PLIST" | sort -n | tr '\n' ' ')"
  if [[ "$makefile_sizes" != "$plist_sizes" ]]; then
    fail "FreeBSD icon sizes drift: do-install '${makefile_sizes}' vs pkg-plist '${plist_sizes}'"
  fi
  if [[ "$FAIL" -eq "$freebsd_plist_failures" ]]; then
    pass "FreeBSD pkg-plist (${freebsd_plist_entries} entries) matches in-tree sources"
  fi
else
  fail "FreeBSD pkg-plist missing or empty"
fi

freebsd_host_paths="$(grep -nE '/(home|Users|Volumes|mnt)/' \
  "$FREEBSD_MAKEFILE" "$FREEBSD_PLIST" "$FREEBSD_DESCR" "$FREEBSD_DISTINFO" \
  "$REPO_ROOT/packaging/freebsd.md" 2>/dev/null || true)"
if [[ -n "$freebsd_host_paths" ]]; then
  fail "FreeBSD recipe carries a hardcoded host path: ${freebsd_host_paths}"
else
  pass "FreeBSD recipe has no hardcoded host paths"
fi

# 5. Documentation coverage
for doc in packaging/alpine.md packaging/gentoo.md packaging/void.md packaging/freebsd.md; do
  if [[ -f "$REPO_ROOT/$doc" ]]; then
    pass "Documentation exists: $doc"
  else
    fail "Documentation missing: $doc"
  fi
done

if grep -q 'packaging/gentoo.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/void.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/freebsd.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/alpine/APKBUILD' "$REPO_ROOT/packaging/INSTALL.md"; then
  pass "INSTALL.md references all four distro packaging guides"
else
  fail "INSTALL.md missing references to distro recipes"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-distro-recipes.test.sh: FAIL" >&2
  exit 1
fi

echo "check-distro-recipes.test.sh: ALL PASS"
