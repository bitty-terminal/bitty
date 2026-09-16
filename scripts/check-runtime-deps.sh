#!/usr/bin/env bash
# check-runtime-deps.sh — fail when declared package runtime dependencies
# diverge from the shared libraries the release binary actually links
# (034 item 4).
#
# What it does:
#   1. reads the direct `NEEDED` sonames of an ELF binary with `readelf -d`;
#   2. resolves them with `ldd` and fails when a library is missing
#      (cross-architecture binaries, where the host loader cannot run them, are
#      noted and checked with readelf alone);
#   3. maps every soname to the package that provides it per nfpm format via
#      `packaging/linux/runtime-deps.toml` (`base` = distro base system, never
#      declared);
#   4. reads the declared names from `overrides.<format>.depends` in
#      `nfpm.yaml`;
#   5. fails when the sets differ: an unmapped soname, a linked library without
#      a declaration, a declared package without link evidence, or a format
#      with no declarations at all.
#
# Usage:
#   scripts/check-runtime-deps.sh --binary PATH --packagers deb,rpm,archlinux
#
# Options:
#   --binary PATH       ELF binary to inspect (required).
#   --packagers LIST    Comma-separated nfpm packager names; each of deb, rpm,
#                       apk, archlinux (required).
#   --nfpm-config PATH  nfpm config with `overrides.<format>.depends`
#                       (default: <repo>/nfpm.yaml).
#   --map PATH          soname->package mapping TOML
#                       (default: <repo>/packaging/linux/runtime-deps.toml).
#   -h, --help          Show this help.
#
# Exit codes: 0 = all requested formats match, 1 = divergence or inspection
# failure, 2 = usage error.
set -euo pipefail

export LC_ALL=C

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY=""
PACKAGERS=""
NFPM_CONFIG="$REPO_ROOT/nfpm.yaml"
MAP_FILE="$REPO_ROOT/packaging/linux/runtime-deps.toml"

usage() {
	cat <<'EOF'
Usage: scripts/check-runtime-deps.sh --binary PATH --packagers deb,rpm,archlinux

Options:
  --binary PATH       ELF binary to inspect (required).
  --packagers LIST    Comma-separated nfpm packager names; each of deb, rpm,
                      apk, archlinux (required).
  --nfpm-config PATH  nfpm config with `overrides.<format>.depends`
                      (default: <repo>/nfpm.yaml).
  --map PATH          soname->package mapping TOML
                      (default: <repo>/packaging/linux/runtime-deps.toml).
  -h, --help          Show this help.

The gate maps every `readelf -d` NEEDED soname to the package that provides it
per format, compares that set with the `overrides.<format>.depends` lists in
the nfpm config, and fails on any divergence. See
packaging/linux/README.md and 034 item 4 (CTX-0449).
EOF
}

report() {
	local tag="$1"
	shift
	printf 'check-runtime-deps[%s]: %s\n' "$tag" "$*" >&2
}

die() {
	report "error" "$*"
	exit 1
}

die_tag() {
	local tag="$1"
	shift
	report "$tag" "$*"
	exit 1
}

usage_error() {
	printf 'check-runtime-deps: %s\n' "$*" >&2
	printf 'run with --help\n' >&2
	exit 2
}

while (($#)); do
	case "$1" in
	--binary | --packagers | --nfpm-config | --map)
		[[ $# -ge 2 ]] || usage_error "$1 needs a value"
		case "$1" in
		--binary) BINARY="$2" ;;
		--packagers) PACKAGERS="$2" ;;
		--nfpm-config) NFPM_CONFIG="$2" ;;
		--map) MAP_FILE="$2" ;;
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

[[ -n "$BINARY" ]] || usage_error "--binary is required"
[[ -n "$PACKAGERS" ]] || usage_error "--packagers is required"
[[ -f "$BINARY" ]] || die "binary not found: $BINARY"
[[ -f "$NFPM_CONFIG" ]] || die "nfpm config not found: $NFPM_CONFIG"
[[ -f "$MAP_FILE" ]] || die "runtime-deps mapping not found: $MAP_FILE"
command -v readelf >/dev/null 2>&1 || die "readelf not found (install binutils)"

IFS=',' read -r -a FORMATS <<<"$PACKAGERS"
for fmt in "${FORMATS[@]}"; do
	case "$fmt" in
	deb | rpm | apk | archlinux) ;;
	*) die "unknown packager '$fmt' (expected deb, rpm, apk or archlinux)" ;;
	esac
done

# --- 1. direct NEEDED sonames -------------------------------------------------

if ! readelf_out="$(readelf -d "$BINARY" 2>&1)"; then
	die "readelf -d failed on $BINARY (not an ELF binary?): $readelf_out"
fi
needed="$(printf '%s\n' "$readelf_out" | awk '
	/\(NEEDED\)/ && /Shared library:/ {
		line = $0
		sub(/.*\[/, "", line)
		sub(/\].*/, "", line)
		if (line != "") print line
	}
' | sort -u)"
[[ -n "$needed" ]] || die_tag "no-needed" "no NEEDED sonames found in $BINARY"

# --- 2. ldd resolution check --------------------------------------------------

ldd_status=0
ldd_out="$(ldd "$BINARY" 2>&1)" || ldd_status=$?
if [[ "$ldd_out" == *"not found"* ]]; then
	unresolved="$(printf '%s\n' "$ldd_out" | grep 'not found' || true)"
	die_tag "unresolved" "unresolved shared libraries in $BINARY:"$'\n'"$unresolved"
fi
case "$ldd_out" in
*"not a dynamic executable"* | *"cannot execute binary file"* | *"Exec format error"*)
	printf 'check-runtime-deps: note: ldd cannot resolve %s on this host (cross-architecture or foreign interpreter); resolving with readelf -d only\n' "$BINARY"
	;;
*)
	if ((ldd_status != 0)); then
		die_tag "ldd-failed" "ldd exited $ldd_status for $BINARY: $ldd_out"
	fi
	;;
esac

# --- 3./4. mapping and declarations ------------------------------------------

map_rows="$(awk '
	/^\[sonames\."/ {
		soname = $0
		sub(/^\[sonames\."/, "", soname)
		sub(/"\][[:space:]]*$/, "", soname)
		next
	}
	/^[A-Za-z]+[[:space:]]*=/ {
		if (soname == "") next
		key = $1
		val = $3
		gsub(/"/, "", val)
		if (key ~ /^(deb|rpm|apk|archlinux)$/ && val != "") print soname "|" key "|" val
		next
	}
' "$MAP_FILE")"
[[ -n "$map_rows" ]] || die "no soname mappings parsed from $MAP_FILE"

declared_rows="$(awk '
	/^overrides:/ { in_over = 1; fmt = ""; in_dep = 0; next }
	in_over && /^[^[:space:]#]/ { in_over = 0; fmt = ""; in_dep = 0 }
	in_over && /^  [A-Za-z]+:/ {
		fmt = $1
		sub(/:$/, "", fmt)
		in_dep = 0
		next
	}
	in_over && /^    depends:/ { in_dep = 1; next }
	in_over && /^    [A-Za-z]+:/ { in_dep = 0; next }
	in_over && in_dep && /^      - / {
		if (fmt == "") next
		dep = $0
		sub(/^      - /, "", dep)
		sub(/[[:space:]]+#.*$/, "", dep)
		gsub(/^"|"$/, "", dep)
		if (dep != "") print fmt "|" dep
		next
	}
' "$NFPM_CONFIG")"

# --- 5. compare per requested format -----------------------------------------

printf 'check-runtime-deps: binary %s\n' "$BINARY"
printf 'check-runtime-deps: linked NEEDED: %s\n' "$(printf '%s' "$needed" | paste -sd, -)"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
expected_file="$tmp_dir/expected"
declared_file="$tmp_dir/declared"
unmapped_file="$tmp_dir/unmapped"
failed_formats=0

for fmt in "${FORMATS[@]}"; do
	fmt_failed=0
	: >"$expected_file"
	: >"$unmapped_file"
	while IFS= read -r soname; do
		[[ -n "$soname" ]] || continue
		pkg="$(printf '%s\n' "$map_rows" | awk -F'|' -v s="$soname" -v f="$fmt" '$1 == s && $2 == f { print $3; exit }')"
		case "$pkg" in
		"") printf '%s\n' "$soname" >>"$unmapped_file" ;;
		base) ;;
		*) printf '%s\n' "$pkg" >>"$expected_file" ;;
		esac
	done <<<"$needed"
	sort -u -o "$expected_file" "$expected_file"

	printf '%s\n' "$declared_rows" | awk -F'|' -v f="$fmt" '$1 == f { print $2 }' | sort -u >"$declared_file"

	if [[ ! -s "$declared_file" ]]; then
		report "declared-absent" "$fmt: no overrides.$fmt.depends declarations in $NFPM_CONFIG"
		fmt_failed=1
	fi

	if [[ -s "$unmapped_file" ]]; then
		report "unmapped" "$fmt: linked soname(s) with no mapping in $MAP_FILE: $(paste -sd, "$unmapped_file")"
		fmt_failed=1
	fi

	missing="$(comm -23 "$expected_file" "$declared_file" | paste -sd, -)"
	extra="$(comm -13 "$expected_file" "$declared_file" | paste -sd, -)"
	if [[ -n "$missing" ]]; then
		report "declared-missing" "$fmt: linked but not declared: $missing"
		fmt_failed=1
	fi
	if [[ -n "$extra" ]]; then
		report "declared-extra" "$fmt: declared without link evidence: $extra"
		fmt_failed=1
	fi

	if ((fmt_failed)); then
		failed_formats=$((failed_formats + 1))
	else
		printf 'check-runtime-deps[%s]: PASS declared=%s\n' "$fmt" "$(paste -sd, "$declared_file")"
	fi
done

if ((failed_formats)); then
	printf 'check-runtime-deps: FAIL (%d of %d formats)\n' "$failed_formats" "${#FORMATS[@]}" >&2
	exit 1
fi
printf 'check-runtime-deps: PASS (%d formats)\n' "${#FORMATS[@]}"
