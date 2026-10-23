#!/usr/bin/env bash
# check-distro-recipes.test.sh — verify distribution packaging templates.
#
# Asserts integrity of packaging recipes for:
#   - Alpine Linux (aports APKBUILD)
#   - Gentoo Linux (live ebuild + prebuilt binary ebuild + overlay config)
#   - Void Linux (xbps-src template)
#   - FreeBSD (Ports recipe: Makefile, pkg-plist, pkg-descr, distinfo)
#   - openSUSE (OBS spec + source service)
#
# Version-agnostic on purpose: recipe versions are derived from the files
# themselves (globbed ebuild names, pkgver/version fields) and cross-checked
# against the workspace version in Cargo.toml, so a release bump does not
# require editing this gate. Patterns are anchored so a renamed package,
# recipe, or subpackage fails instead of sliding past a prefix match. The
# FreeBSD checks pin structure instead: the port's PORTVERSION moves with each
# release, so no version string appears in this gate. The openSUSE checks
# compare the spec's Version against the source-service revision and its Rust
# requirement against the workspace MSRV.
#
# Part of CTX-1471 (#2513, #2516, #2517), CTX-1473 (#2512), and CTX-1474
# (#2514).
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

# expect FILE ERE DESCRIPTION — the pattern must match at least one line.
expect() {
  local file=$1 pattern=$2 desc=$3
  if grep -Eq -- "$pattern" "$file"; then
    pass "$desc"
  else
    fail "$desc (no line matching /$pattern/ in ${file#"$REPO_ROOT"/})"
  fi
}

# reject FILE ERE DESCRIPTION — the pattern must match no line.
reject() {
  local file=$1 pattern=$2 desc=$3
  if grep -Eq -- "$pattern" "$file"; then
    fail "$desc (unexpected match for /$pattern/ in ${file#"$REPO_ROOT"/})"
  else
    pass "$desc"
  fi
}

# reject_code FILE ERE DESCRIPTION — like reject, but comment lines are ignored
# so a comment that explains why an atom is wrong does not trip the gate.
reject_code() {
  local file=$1 pattern=$2 desc=$3
  if grep -Ev '^[[:space:]]*#' "$file" | grep -Eq -- "$pattern"; then
    fail "$desc (unexpected match for /$pattern/ in ${file#"$REPO_ROOT"/})"
  else
    pass "$desc"
  fi
}

# expect_block TEXT ERE DESCRIPTION — ERE must match inside a captured block.
expect_block() {
  local text=$1 pattern=$2 desc=$3
  if grep -Eq -- "$pattern" <<<"$text"; then
    pass "$desc"
  else
    fail "$desc (no line matching /$pattern/)"
  fi
}

reject_block() {
  local text=$1 pattern=$2 desc=$3
  if grep -Eq -- "$pattern" <<<"$text"; then
    fail "$desc (unexpected match for /$pattern/)"
  else
    pass "$desc"
  fi
}

is_semver() {
  [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

# version_ge A B — true when strict X.Y.Z A >= B.
version_ge() {
  local IFS=.
  local -a left right
  read -ra left <<<"$1"
  read -ra right <<<"$2"
  local i
  for i in 0 1 2; do
    if ((left[i] > right[i])); then
      return 0
    fi
    if ((left[i] < right[i])); then
      return 1
    fi
  done
  return 0
}

# RDEPEND="..." block, from the opening line to the closing quote line.
rdepend_block() {
  awk '/^RDEPEND="/{inside=1} inside{print} inside && /^"$/{exit}' "$1"
}

# 0. Workspace version, the anchor every recipe is compared against.
CARGO_VERSION="$(
  awk '
    /^\[workspace\.package\]/ { inside = 1; next }
    /^\[/ { inside = 0 }
    inside && /^version[[:space:]]*=/ {
      gsub(/[^0-9.]/, "", $0)
      print
      exit
    }
  ' "$REPO_ROOT/Cargo.toml"
)"
if is_semver "$CARGO_VERSION"; then
  pass "workspace version $CARGO_VERSION read from Cargo.toml"
else
  fail "cannot read a X.Y.Z [workspace.package] version from Cargo.toml"
  CARGO_VERSION="0.0.0"
fi

# 1. Alpine Linux APKBUILD
ALPINE_APKBUILD="$REPO_ROOT/packaging/alpine/APKBUILD"
alpine_pkgver=""
if [[ -f "$ALPINE_APKBUILD" ]]; then
  expect "$ALPINE_APKBUILD" '^pkgname=bitty$' "Alpine pkgname is exactly bitty"
  expect "$ALPINE_APKBUILD" '^options="net"$' 'Alpine APKBUILD sets options="net"'
  expect "$ALPINE_APKBUILD" '^license="MIT OR Apache-2\.0"$' "Alpine license is MIT OR Apache-2.0"
  expect "$ALPINE_APKBUILD" 'cargo auditable build' "Alpine APKBUILD uses cargo auditable build"
  expect "$ALPINE_APKBUILD" 'tic -x' "Alpine APKBUILD compiles terminfo with tic -x"
  expect "$ALPINE_APKBUILD" 'target-feature=-crt-static' "Alpine APKBUILD keeps musl dynamically linked"

  alpine_pkgname="$(sed -n 's/^pkgname=\(.*\)$/\1/p' "$ALPINE_APKBUILD")"
  alpine_pkgver="$(sed -n 's/^pkgver=\(.*\)$/\1/p' "$ALPINE_APKBUILD")"
  if is_semver "$alpine_pkgver"; then
    pass "Alpine pkgver $alpine_pkgver is X.Y.Z"
  else
    fail "Alpine pkgver '$alpine_pkgver' is not X.Y.Z"
  fi

  # every declared subpackage needs its split function, or abuild validate dies
  alpine_subpackages="$(sed -n 's/^subpackages="\(.*\)"$/\1/p' "$ALPINE_APKBUILD")"
  if [[ -n "$alpine_subpackages" ]]; then
    read -ra subpkg_names <<<"${alpine_subpackages//\$pkgname/$alpine_pkgname}"
    for subpkg in "${subpkg_names[@]}"; do
      # abuild names the split function after the subpackage with the
      # "$pkgname-" prefix stripped and dashes turned into underscores.
      split_name="${subpkg#"$alpine_pkgname"-}"
      split_fn="${split_name//-/_}"
      expect "$ALPINE_APKBUILD" "^${split_fn}\(\) \{" \
        "Alpine split function ${split_fn}() exists for subpackage ${subpkg}"
    done
  else
    pass "Alpine APKBUILD declares no subpackages"
  fi
  pass "Alpine APKBUILD integrity"
else
  fail "Alpine APKBUILD not found at ${ALPINE_APKBUILD#"$REPO_ROOT"/}"
fi

# 2. Gentoo overlay metadata
GENTOO_ROOT="$REPO_ROOT/packaging/gentoo"
GENTOO_REPO_NAME="$GENTOO_ROOT/profiles/repo_name"
GENTOO_LAYOUT="$GENTOO_ROOT/metadata/layout.conf"

expect "$GENTOO_REPO_NAME" '^bitty$' "Gentoo profiles/repo_name is exactly bitty"
expect "$GENTOO_LAYOUT" '^masters = gentoo$' "Gentoo metadata/layout.conf masters = gentoo"

# 3. Gentoo ebuilds — discovered by glob so a release bump needs no gate edit.
shopt -s nullglob
src_ebuilds=("$GENTOO_ROOT"/x11-terms/bitty/bitty-*.ebuild)
bin_ebuilds=("$GENTOO_ROOT"/x11-terms/bitty-bin/bitty-bin-*.ebuild)
shopt -u nullglob

ebuild_version() {
  local base=${1##*/}
  base=${base%.ebuild}
  printf '%s\n' "${base##*-}"
}

src_version=""
bin_version=""

if [[ ${#src_ebuilds[@]} -eq 1 ]]; then
  src_ebuild="${src_ebuilds[0]}"
  src_version="$(ebuild_version "$src_ebuild")"

  expect "$src_ebuild" '^inherit .*cargo' "Gentoo source ebuild inherits cargo.eclass"
  expect "$src_ebuild" '^LICENSE="MIT OR Apache-2\.0"$' "Gentoo source ebuild license is MIT OR Apache-2.0"
  expect "$src_ebuild" 'cargo_src_compile -p bitty-terminal --bin bitty' \
    "Gentoo source ebuild builds the bitty binary"
  expect "$src_ebuild" 'tic -x' "Gentoo source ebuild compiles terminfo with tic -x"
  reject_code "$src_ebuild" 'sys-apps/tic' "Gentoo source ebuild does not depend on the nonexistent sys-apps/tic"

  src_rdepend="$(rdepend_block "$src_ebuild")"
  expect_block "$src_rdepend" 'media-libs/fontconfig' "Gentoo source RDEPEND declares fontconfig"
  expect_block "$src_rdepend" 'media-libs/freetype' "Gentoo source RDEPEND declares freetype"
  reject_block "$src_rdepend" 'x11-libs/libX11|x11-libs/libxkbcommon|dev-libs/wayland|sys-libs/zlib' \
    "Gentoo source RDEPEND does not over-declare dlopen-only backends"

  if [[ "$src_version" == "9999" ]]; then
    # live ebuild: git-r3 checkout, no CRATES list, no keywords
    expect "$src_ebuild" '^EGIT_REPO_URI="https://github\.com/bitty-terminal/bitty\.git"$' \
      "Gentoo live ebuild sets EGIT_REPO_URI"
    expect "$src_ebuild" '^inherit .*git-r3' "Gentoo live ebuild inherits git-r3.eclass"
    expect "$src_ebuild" '^KEYWORDS=""$' "Gentoo live ebuild carries no KEYWORDS"
    expect "$src_ebuild" 'git-r3_src_unpack' "Gentoo live ebuild checks the repository out"
    expect "$src_ebuild" 'cargo_live_src_unpack' "Gentoo live ebuild vendors through cargo_live_src_unpack"
    reject "$src_ebuild" '^CRATES=' "Gentoo live ebuild declares no CRATES list"
    reject "$src_ebuild" 'CARGO_CRATE_URIS' "Gentoo live ebuild does not expand CARGO_CRATE_URIS"
    pass "Gentoo source ebuild is a consistent live (9999) ebuild"
  else
    if is_semver "$src_version"; then
      pass "Gentoo source ebuild version $src_version is a release version"
    else
      fail "Gentoo source ebuild version '$src_version' is neither 9999 nor X.Y.Z"
    fi
    expect "$src_ebuild" 'refs/tags/v\$\{PV\}' "Gentoo release ebuild fetches the v\${PV} tag archive"
  fi
else
  fail "expected exactly one Gentoo source ebuild (bitty-*.ebuild), found ${#src_ebuilds[@]}"
fi

if [[ ${#bin_ebuilds[@]} -eq 1 ]]; then
  bin_ebuild="${bin_ebuilds[0]}"
  bin_version="$(ebuild_version "$bin_ebuild")"

  if is_semver "$bin_version"; then
    pass "Gentoo binary ebuild version $bin_version is a release version"
  else
    fail "Gentoo binary ebuild version '$bin_version' is not X.Y.Z"
  fi

  expect "$bin_ebuild" '^inherit desktop xdg' "Gentoo binary ebuild inherits desktop and xdg"
  expect "$bin_ebuild" '^LICENSE="MIT OR Apache-2\.0"$' "Gentoo binary ebuild license is MIT OR Apache-2.0"
  expect "$bin_ebuild" 'https://cdn\.bitty\.run/releases/v\$\{PV\}/' "Gentoo binary ebuild fetches the CDN archive"
  expect "$bin_ebuild" 'tic -x' "Gentoo binary ebuild compiles terminfo with tic -x"
  reject_code "$bin_ebuild" 'sys-apps/tic' "Gentoo binary ebuild does not depend on the nonexistent sys-apps/tic"

  bin_rdepend="$(rdepend_block "$bin_ebuild")"
  expect_block "$bin_rdepend" 'media-libs/fontconfig' "Gentoo binary RDEPEND declares fontconfig"
  expect_block "$bin_rdepend" 'media-libs/freetype' "Gentoo binary RDEPEND declares freetype"
  reject_block "$bin_rdepend" 'x11-libs/libX11|x11-libs/libxkbcommon|dev-libs/wayland|sys-libs/zlib' \
    "Gentoo binary RDEPEND does not over-declare dlopen-only backends"

  pass "Gentoo binary ebuild integrity"
else
  fail "expected exactly one Gentoo binary ebuild (bitty-bin-*.ebuild), found ${#bin_ebuilds[@]}"
fi

# 4. Void Linux xbps-src template
VOID_TEMPLATE="$REPO_ROOT/packaging/void/srcpkgs/bitty/template"
void_version=""
if [[ -f "$VOID_TEMPLATE" ]]; then
  expect "$VOID_TEMPLATE" '^pkgname=bitty$' "Void template pkgname is exactly bitty"
  expect "$VOID_TEMPLATE" '^build_style=cargo$' "Void template uses build_style=cargo"
  expect "$VOID_TEMPLATE" '^make_install_args=.*--path crates/bitty-terminal' \
    "Void template builds crates/bitty-terminal"
  expect "$VOID_TEMPLATE" '^license="MIT OR Apache-2\.0"$' "Void template license is MIT OR Apache-2.0"
  expect "$VOID_TEMPLATE" 'vdesk ' "Void template installs the desktop file with vdesk"
  expect "$VOID_TEMPLATE" 'tic -x' "Void template compiles terminfo with tic -x"

  void_version="$(sed -n 's/^version=\(.*\)$/\1/p' "$VOID_TEMPLATE")"
  if is_semver "$void_version"; then
    pass "Void template version $void_version is X.Y.Z"
  else
    fail "Void template version '$void_version' is not X.Y.Z"
  fi

  void_short_desc="$(sed -n 's/^short_desc="\(.*\)"$/\1/p' "$VOID_TEMPLATE")"
  if [[ -n "$void_short_desc" && ${#void_short_desc} -le 72 ]]; then
    pass "Void short_desc is ${#void_short_desc} characters (<= 72)"
  else
    fail "Void short_desc is missing or ${#void_short_desc} characters (> 72)"
  fi

  void_checksum_line="$(grep -n '^checksum=' "$VOID_TEMPLATE" | head -1 | cut -d: -f1)"
  void_checksum="$(sed -n 's/^checksum="\{0,1\}\([0-9A-Za-z]*\)"\{0,1\}$/\1/p' "$VOID_TEMPLATE")"
  if [[ "$void_checksum" == "SKIP" ]]; then
    if sed -n "1,${void_checksum_line:-1}p" "$VOID_TEMPLATE" | grep -Eq '#.*(xgensum|tag time)'; then
      pass "Void checksum=SKIP is documented with the tag-time replacement step"
    else
      fail "Void checksum=SKIP has no rationale comment naming the tag-time checksum step"
    fi
  elif [[ "$void_checksum" =~ ^[0-9a-f]{64}$ ]]; then
    pass "Void template pins a 64-hex sha256 checksum"
  else
    fail "Void checksum is neither a real sha256 nor a documented SKIP"
  fi

  pass "Void Linux template integrity"
else
  fail "Void Linux template not found at ${VOID_TEMPLATE#"$REPO_ROOT"/}"
fi

# 5. FreeBSD ports recipe (x11/bitty)
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
    bin/bitty) ;;        # produced by do-install, asserted above
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

# 6. Recipe versions must agree with one another, and never lag the workspace.
#    Recipes may lead Cargo.toml by the pending release bump (draft release PR),
#    so a lead is reported, not failed.
if [[ -n "$bin_version" && -n "$void_version" && -n "$alpine_pkgver" ]] &&
  [[ "$alpine_pkgver" == "$bin_version" ]] &&
  [[ "$void_version" == "$bin_version" ]]; then
  pass "Alpine, Gentoo binary, and Void recipes agree on version $bin_version"
else
  fail "recipe versions disagree (Alpine '$alpine_pkgver', Gentoo bin '$bin_version', Void '$void_version')"
fi

if is_semver "$bin_version" && is_semver "$CARGO_VERSION"; then
  if version_ge "$bin_version" "$CARGO_VERSION"; then
    if [[ "$bin_version" == "$CARGO_VERSION" ]]; then
      pass "recipe version $bin_version matches the workspace version"
    else
      echo "NOTE: recipes pin $bin_version while Cargo.toml is $CARGO_VERSION (pending release bump)"
      pass "recipe version $bin_version leads the workspace version $CARGO_VERSION"
    fi
  else
    fail "recipes pin $bin_version but the workspace is already $CARGO_VERSION (recipes lag the bump)"
  fi
fi

# 7. Documentation coverage
for doc in packaging/alpine.md packaging/gentoo.md packaging/void.md packaging/freebsd.md; do
  if [[ -f "$REPO_ROOT/$doc" ]]; then
    pass "Documentation exists: $doc"
    expect "$REPO_ROOT/$doc" 'tag time' "Documentation states tag-time steps: $doc"
  else
    fail "Documentation missing: $doc"
  fi
done

# 8. The Gentoo guide must describe a valid overlay, not the repository root:
#    profiles/repo_name and metadata/layout.conf live only in packaging/gentoo.
GENTOO_DOC="$REPO_ROOT/packaging/gentoo.md"
reject "$GENTOO_DOC" 'eselect repository add bitty git https://github\.com/bitty-terminal/bitty\.git' \
  "Gentoo guide does not add the repository root as an overlay"
reject "$GENTOO_DOC" 'sync-uri[[:space:]]*=[[:space:]]*https://github\.com/bitty-terminal/bitty\.git' \
  "Gentoo guide does not sync the repository root as an overlay"
expect "$GENTOO_DOC" 'location = .*packaging/gentoo' \
  "Gentoo guide points the overlay location at packaging/gentoo"

if grep -q 'packaging/gentoo.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/void.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/freebsd.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/alpine/APKBUILD' "$REPO_ROOT/packaging/INSTALL.md"; then
  pass "INSTALL.md references the Alpine, Gentoo, Void, and FreeBSD recipes"
else
  fail "INSTALL.md missing references to distro recipes"
fi

# --- openSUSE: spec required fields -----------------------------------------
OPENSPEC="$REPO_ROOT/packaging/opensuse/bitty.spec"
OBS_SERVICE="$REPO_ROOT/packaging/opensuse/_service"

if [[ -f "$OPENSPEC" ]]; then
  for field in Name Version Release Summary License Group URL Source0; do
    if grep -qE "^${field}:[[:space:]]+[^[:space:]]+" "$OPENSPEC"; then
      pass "openSUSE spec has $field"
    else
      fail "openSUSE spec missing $field"
    fi
  done

  if grep -qE '^License:[[:space:]]+MIT OR Apache-2\.0' "$OPENSPEC"; then
    pass "openSUSE spec license matches the project (MIT OR Apache-2.0)"
  else
    fail "openSUSE spec license is not 'MIT OR Apache-2.0'"
  fi

  # The release tarball pattern: the tag archive of the packaged Version.
  if grep -qE '^Source0:.*(v%\{version\}|%\{version\}).*\.tar\.(gz|xz|zst)' "$OPENSPEC"; then
    pass "openSUSE spec Source0 points at the release tarball"
  else
    fail "openSUSE spec Source0 is not a versioned release tarball"
  fi

  # %build / %install / %files / %changelog must exist as sections.
  for section in description prep build install files changelog; do
    if grep -qE "^%${section}([[:space:]]|$)" "$OPENSPEC"; then
      pass "openSUSE spec has %$section section"
    else
      fail "openSUSE spec missing %$section section"
    fi
  done

  # Cargo invocation: source build of the binary crate, no prebuilt payload.
  if grep -qE '^cargo build --release (--locked )?-p bitty-terminal --bin bitty$' "$OPENSPEC"; then
    pass "openSUSE spec builds the binary with cargo"
  else
    fail "openSUSE spec missing 'cargo build --release -p bitty-terminal --bin bitty'"
  fi

  # Terminfo is compiled, never installed as the raw source file.
  if grep -qE '^tic -x -o %\{buildroot\}' "$OPENSPEC"; then
    pass "openSUSE spec compiles terminfo with tic -x"
  else
    fail "openSUSE spec missing compiled terminfo install (tic -x)"
  fi

  # BuildRequires: cargo plus the font stack devel packages and the MSRV.
  for br in cargo fontconfig-devel freetype2-devel zlib-devel; do
    if grep -qE "^BuildRequires:[[:space:]]+${br}([[:space:]]|\$)" "$OPENSPEC"; then
      pass "openSUSE spec BuildRequires $br"
    else
      fail "openSUSE spec missing BuildRequires $br"
    fi
  done

  MSRV="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
  SPEC_RUST="$(sed -n 's/^BuildRequires:[[:space:]]*rust[[:space:]]*>=[[:space:]]*\([0-9][0-9.]*\).*/\1/p' "$OPENSPEC" | head -n1)"
  if [[ -n "$MSRV" && "$SPEC_RUST" == "$MSRV" ]]; then
    pass "openSUSE spec BuildRequires rust >= $MSRV matches the workspace MSRV"
  else
    fail "openSUSE spec rust requirement '${SPEC_RUST:-missing}' does not match workspace MSRV '${MSRV:-missing}'"
  fi

  # --- openSUSE: %files assets exist in-tree --------------------------------
  # The spec ships no prebuilt payload, so every share/ path it lists must
  # resolve to a checked-in asset (icons, .desktop, metainfo). Paths the build
  # produces itself (binary, compiled terminfo) and doc paths are skipped.
  if python3 - "$OPENSPEC" "$REPO_ROOT" <<'PY'; then
# Trusted in-tree input: this parses a file from the repository checkout and
# never fetches or interpolates external XML, so stdlib ElementTree is fine
# here (no external entity or DTD processing is enabled).
import glob
import os
import sys

spec, root = sys.argv[1], sys.argv[2]
macros = {"%{_datadir}": "usr/share", "%{_bindir}": "usr/bin"}
generated = ("usr/bin/", "usr/share/terminfo/", "usr/share/licenses/", "usr/share/doc/")

entries = []
in_files = False
directives = (
    "%license",
    "%doc",
    "%dir",
    "%attr",
    "%defattr",
    "%config",
    "%exclude",
    "%ghost",
    "%lang",
    "%verify",
)
with open(spec, encoding="utf-8") as handle:
    for raw in handle:
        line = raw.strip()
        if line.startswith("%files"):
            in_files = True
            continue
        if in_files and line.startswith("%changelog"):
            break
        if not in_files or not line or line.startswith("#"):
            continue
        # Directive lines (%license, %doc, %attr, ...) carry source-tree-relative
        # names, not installed paths, so they are not asset-mapped here. Only
        # plain path lines are: those start with a path macro, not a directive.
        if line.startswith(directives):
            continue
        entries.extend(line.split())

missing = []
for entry in entries:
    path = entry
    for macro, value in macros.items():
        path = path.replace(macro, value)
    if "%{" in path:
        continue
    if path.startswith(generated):
        continue
    rel = path[len("usr/share/"):] if path.startswith("usr/share/") else path
    basename = os.path.basename(rel)
    candidates = [
        os.path.join(root, "packaging", rel),
        os.path.join(root, "packaging", basename),
        os.path.join(root, "terminfo", basename),
    ]
    if any(os.path.exists(c) for c in candidates):
        continue
    if any(glob.glob(c) for c in candidates):
        continue
    missing.append(entry)

for entry in missing:
    print(f"    %files asset not in tree: {entry}", file=sys.stderr)
sys.exit(1 if missing else 0)
PY
    pass "openSUSE spec %files assets all exist in-tree"
  else
    fail "openSUSE spec %files references assets that do not exist in-tree"
  fi
else
  fail "openSUSE spec not found at $OPENSPEC"
fi

# --- openSUSE: OBS source service is well-formed XML ------------------------
if [[ -f "$OBS_SERVICE" ]]; then
  if python3 - "$OBS_SERVICE" <<'PY'; then
# Trusted in-tree input (see the note above): parse only, no entity expansion.
import sys
import xml.etree.ElementTree as ET

root = ET.parse(sys.argv[1]).getroot()
assert root.tag == "services", f"root element is <{root.tag}>, expected <services>"
services = root.findall("service")
assert services, "no <service> elements"
for service in services:
    name = service.get("name")
    assert name, "a <service> element has no name"
    assert service.get("mode") == "disabled", f"service {name} is not mode=\"disabled\""
    for param in service.findall("param"):
        assert param.get("name"), f"service {name} has a <param> without a name"
        assert (param.text or "").strip(), f"service {name} param {param.get('name')} is empty"
PY
    pass "openSUSE _service parses as XML and declares only disabled services"
  else
    fail "openSUSE _service is not valid XML or not in disabled mode"
  fi

  # The service revision and the spec Version must name the same release.
  SPEC_VERSION="$(sed -n 's/^Version:[[:space:]]*\([^[:space:]]*\).*/\1/p' "$OPENSPEC" 2>/dev/null | head -n1)"
  SERVICE_VERSION="$(sed -n 's/.*<param name="revision">v\([^<]*\)<\/param>.*/\1/p' "$OBS_SERVICE" | head -n1)"
  if [[ -n "$SPEC_VERSION" && "$SPEC_VERSION" == "$SERVICE_VERSION" ]]; then
    pass "openSUSE _service revision matches spec Version $SPEC_VERSION"
  else
    fail "openSUSE _service revision '${SERVICE_VERSION:-missing}' does not match spec Version '${SPEC_VERSION:-missing}'"
  fi
else
  fail "openSUSE _service not found at $OBS_SERVICE"
fi

# --- openSUSE: documentation coverage ---------------------------------------
for doc in packaging/opensuse.md packaging/opensuse/README.md; do
  if [[ -f "$REPO_ROOT/$doc" ]]; then
    pass "Documentation exists: $doc"
  else
    fail "Documentation missing: $doc"
  fi
done

if grep -q 'packaging/opensuse.md' "$REPO_ROOT/packaging/INSTALL.md" &&
  grep -q 'packaging/opensuse/bitty.spec' "$REPO_ROOT/packaging/README.md"; then
  pass "INSTALL.md and packaging/README.md reference the openSUSE recipe"
else
  fail "INSTALL.md or packaging/README.md does not reference the openSUSE recipe"
fi

if [[ "$FAIL" -ne 0 ]]; then
  echo "check-distro-recipes.test.sh: FAIL" >&2
  exit 1
fi

echo "check-distro-recipes.test.sh: ALL PASS"
