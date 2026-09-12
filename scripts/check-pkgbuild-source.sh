#!/usr/bin/env bash
# check-pkgbuild-source.sh — consistency guard for the AUR source recipe.
#
# The source package (AUR `bitty`) once installed `target/release/bitty-app`
# long after the workspace renamed the artifact to `bitty`
# (crates/bitty-app/Cargo.toml `[[bin]] name = "bitty"`), so `makepkg` failed
# at package() with `install: cannot stat 'target/release/bitty-app'`
# (CTX-0352). This script fails fast when the declared artifact and the recipe
# install path drift apart again, before a release tag propagates it to AUR.
#
# It asserts:
#   - `PKGBUILD` and `packaging/PKGBUILD` are byte-identical (README contract)
#   - both parse as shell and produce `.SRCINFO` via `makepkg --printsrcinfo`
#   - `package()` installs the exact artifact declared by `[[bin]]` in
#     `crates/bitty-app/Cargo.toml`, at `/usr/bin/bitty`
#   - the recipe no longer references the retired `bitty-app` artifact path
#   - every desktop/icon/terminfo source referenced by `package()` exists
#
# Usage: scripts/check-pkgbuild-source.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_RECIPE="$REPO_ROOT/PKGBUILD"
PACKAGING_RECIPE="$REPO_ROOT/packaging/PKGBUILD"
APP_MANIFEST="$REPO_ROOT/crates/bitty-app/Cargo.toml"

fail() {
	echo "check-pkgbuild-source: FAIL: $1" >&2
	exit 1
}

[[ -f "$ROOT_RECIPE" ]] || fail "missing $ROOT_RECIPE"
[[ -f "$PACKAGING_RECIPE" ]] || fail "missing $PACKAGING_RECIPE"

# README: the root and packaging recipes are identical.
cmp -s "$ROOT_RECIPE" "$PACKAGING_RECIPE" ||
	fail "PKGBUILD and packaging/PKGBUILD differ; keep them byte-identical"

for recipe in "$ROOT_RECIPE" "$PACKAGING_RECIPE"; do
	bash -n "$recipe" || fail "bash -n rejects $recipe"
done

# The artifact name is owned by crates/bitty-app/Cargo.toml: derive it from
# the `[[bin]]` name so the check tracks the manifest instead of a literal.
declare -a BIN_NAMES=()
while IFS= read -r name; do
	[[ -n "$name" ]] && BIN_NAMES+=("$name")
done < <(awk '
	/^\[\[bin\]\]/ { inbin = 1; next }
	inbin && /^name[[:space:]]*=/ {
		sub(/^name[[:space:]]*=[[:space:]]*"/, ""); sub(/".*/, ""); print; inbin = 0
	}
	inbin && /^\[/ { inbin = 0 }
' "$APP_MANIFEST")
[[ "${#BIN_NAMES[@]}" -eq 1 ]] ||
	fail "expected exactly one [[bin]] name in crates/bitty-app/Cargo.toml, got ${#BIN_NAMES[@]}"

EXPECTED_BIN="${BIN_NAMES[0]}"
INSTALL_SRC='target/release/'"$EXPECTED_BIN"''

grep -Fq "install -Dm755 \"$INSTALL_SRC\" \"\$pkgdir/usr/bin/bitty\"" "$ROOT_RECIPE" ||
	fail "package() must install \"$INSTALL_SRC\" to /usr/bin/bitty (declared artifact: $EXPECTED_BIN)"

# Guard against the retired artifact name returning anywhere in package().
if awk '/^package\(\)/,/^}/' "$ROOT_RECIPE" | grep -Fq "target/release/bitty-app"; then
	fail "package() references the retired target/release/bitty-app artifact"
fi

# `test "$(./target/release/<bin> --version)" = "$pkgver"` must reference the
# declared artifact, not a stale one, so check() exercises what package() ships.
grep -Fq "./$INSTALL_SRC --version" "$ROOT_RECIPE" ||
	fail "check() smoke must invoke ./$INSTALL_SRC --version"

# Every packaged source referenced with install -Dm644 must exist in-tree
# (terminfo is optional and guarded by `[ -f ... ]` in the recipe).
missing=0
while IFS= read -r rel; do
	rel="${rel#./}"
	case "$rel" in
	terminfo/bitty.terminfo) continue ;; # optional, guarded in package()
	target/*) continue ;;                # built artifact, asserted above
	*'$'*) continue ;;                   # variable-derived path, resolved at build time
	esac
	if [[ ! -e "$REPO_ROOT/$rel" ]]; then
		echo "check-pkgbuild-source: missing packaged source: $rel" >&2
		missing=1
	fi
done < <(awk '/^package\(\)/,/^}/' "$ROOT_RECIPE" |
	grep -oE 'install -Dm[0-9]+ "[^"]+"' | sed -E 's/.*"([^"]+)".*/\1/' |
	sed 's#\${size}#16#g')
[[ "$missing" -eq 0 ]] || fail "package() references files absent from the tree"

if command -v makepkg >/dev/null 2>&1; then
	WORK="$(mktemp -d)"
	trap 'rm -rf "$WORK"' EXIT
	cp "$ROOT_RECIPE" "$WORK/PKGBUILD"
	(cd "$WORK" && makepkg --printsrcinfo >.SRCINFO) ||
		fail "makepkg --printsrcinfo rejects the source recipe"
	grep -q "^pkgname = bitty$" "$WORK/.SRCINFO" || fail ".SRCINFO missing pkgname=bitty"
	echo "check-pkgbuild-source: makepkg --printsrcinfo ok"
else
	echo "check-pkgbuild-source: makepkg not available, skipped .SRCINFO probe"
fi

echo "check-pkgbuild-source: PASS (artifact=$EXPECTED_BIN)"
