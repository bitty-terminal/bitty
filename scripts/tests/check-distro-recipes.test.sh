#!/usr/bin/env bash
# check-distro-recipes.test.sh — verify Linux distribution packaging templates.
#
# Asserts integrity of packaging recipes for:
#   - Alpine Linux (aports APKBUILD)
#   - Gentoo Linux (live ebuild + prebuilt binary ebuild + overlay config)
#   - Void Linux (xbps-src template)
#
# Version-agnostic on purpose: recipe versions are derived from the files
# themselves (globbed ebuild names, pkgver/version fields) and cross-checked
# against the workspace version in Cargo.toml, so a release bump does not
# require editing this gate. Patterns are anchored so a renamed package,
# recipe, or subpackage fails instead of sliding past a prefix match.
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

# 5. Recipe versions must agree with one another, and never lag the workspace.
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

# 6. Documentation coverage
for doc in packaging/alpine.md packaging/gentoo.md packaging/void.md; do
  if [[ -f "$REPO_ROOT/$doc" ]]; then
    pass "Documentation exists: $doc"
    expect "$REPO_ROOT/$doc" 'tag time' "Documentation states tag-time steps: $doc"
  else
    fail "Documentation missing: $doc"
  fi
done

# 7. The Gentoo guide must describe a valid overlay, not the repository root:
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
