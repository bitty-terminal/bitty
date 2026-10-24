#!/usr/bin/env bash
# build-copr-srpm.sh — build the Copr SRPM for a published release version.
#
# Copr uploads are source builds on this instance (direct RPM upload is
# disabled), so the release workflow and the emergency repair workflow build a
# source RPM from packaging/bitty.spec and the published .tar.zst bundles.
#
# The spec keeps a default Version for direct local builds; release tooling
# overrides it with `--define "version X.Y.Z"`.
#
# Usage:
#   scripts/build-copr-srpm.sh \
#     --version 0.0.27 \
#     --sourcedir /tmp/copr-sources \
#     --output-dir /tmp/copr-srpm
#
# The sourcedir must contain the spec's Source0/Source1 files and may contain a
# SHA256SUMS file to verify them. The script prints only the resulting SRPM
# path on stdout; diagnostics go to stderr.
set -euo pipefail

SPEC="packaging/bitty.spec"
VERSION=""
SOURCEDIR=""
OUTPUT_DIR=""

usage() {
  cat <<'EOF'
Usage: build-copr-srpm.sh --version X.Y.Z --sourcedir DIR --output-dir DIR [--spec PATH]

Builds an SRPM from the spec and the release .tar.zst sources. The sourcedir
must hold every SourceN file; an optional SHA256SUMS file there is enforced.
Prints the SRPM path on stdout.
EOF
}

die() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    echo "::error title=Copr SRPM build failed::$*" >&2
  else
    echo "error: $*" >&2
  fi
  exit 1
}

while (($# > 0)); do
  case "$1" in
  --version)
    (($# >= 2)) || die "--version requires a value"
    VERSION="$2"
    shift 2
    ;;
  --sourcedir)
    (($# >= 2)) || die "--sourcedir requires a value"
    SOURCEDIR="$2"
    shift 2
    ;;
  --output-dir)
    (($# >= 2)) || die "--output-dir requires a value"
    OUTPUT_DIR="$2"
    shift 2
    ;;
  --spec)
    (($# >= 2)) || die "--spec requires a value"
    SPEC="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    die "unknown argument: $1"
    ;;
  esac
done

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  die "--version must be a stable X.Y.Z version"
[[ -f "$SPEC" ]] || die "spec not found: $SPEC"
[[ -d "$SOURCEDIR" ]] || die "sourcedir not found: $SOURCEDIR"
[[ "$OUTPUT_DIR" != *" "* ]] || die "output-dir must not contain spaces"
mkdir -p "$OUTPUT_DIR"

for tool in rpmbuild rpm zstd; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

mapfile -t SOURCE_URLS < <(sed -n 's/^Source[0-9][0-9]*:[[:space:]]*//p' "$SPEC")
((${#SOURCE_URLS[@]} > 0)) || die "no SourceN entries found in $SPEC"

SOURCE_FILES=()
for url in "${SOURCE_URLS[@]}"; do
  [[ "$url" == https://* ]] || die "SourceN must use https: $url"
  url="${url//%\{version\}/$VERSION}"
  file="${url##*/}"
  [[ -n "$file" && -f "$SOURCEDIR/$file" && -s "$SOURCEDIR/$file" ]] ||
    die "missing or empty source: $SOURCEDIR/$file"
  if [[ "$file" == *.zst ]]; then
    zstd -t "$SOURCEDIR/$file" >/dev/null 2>&1 ||
      die "corrupt zstd source: $file"
  fi
  SOURCE_FILES+=("$file")
done

if [[ -f "$SOURCEDIR/SHA256SUMS" ]]; then
  CHECK_FILE="$OUTPUT_DIR/source-sha256"
  : >"$CHECK_FILE"
  for file in "${SOURCE_FILES[@]}"; do
    sum_line="$(awk -v f="$file" '$2 == f || $2 == "*" f { print }' "$SOURCEDIR/SHA256SUMS")"
    [[ -n "$sum_line" ]] || die "$file is missing from SHA256SUMS"
    printf '%s\n' "$sum_line" >>"$CHECK_FILE"
  done
  # Keep verification output off stdout: callers capture stdout as the SRPM
  # path (and write it to $GITHUB_OUTPUT), so a checksum "OK" line would
  # corrupt the single-line contract.
  (cd "$SOURCEDIR" && sha256sum -c "$CHECK_FILE" >&2) ||
    die "source checksum verification failed"
fi

# An SRPM stores its spec verbatim, and `rpmbuild --rebuild` re-evaluates the
# spec without our --define. Materialize the release version into a temporary
# spec so Copr rebuilds the intended version, not the spec's default.
SPEC_COPY="$OUTPUT_DIR/bitty-${VERSION}.spec"
awk -v version="$VERSION" '
  /^%\{!\?version:%global version / { next }
  /^Version:[[:space:]]/ { print "Version:        " version; next }
  { print }
' "$SPEC" >"$SPEC_COPY"
grep -q "^Version:[[:space:]]*${VERSION//./\\.}$" "$SPEC_COPY" ||
  die "could not materialize version $VERSION into the temporary spec"

rm -f "$OUTPUT_DIR"/*.src.rpm
LOG="$OUTPUT_DIR/rpmbuild-bs.log"
if ! rpmbuild -bs "$SPEC_COPY" \
  --define "_sourcedir $SOURCEDIR" \
  --define "_srcrpmdir $OUTPUT_DIR" \
  --define "version $VERSION" >"$LOG" 2>&1; then
  cat "$LOG" >&2
  die "rpmbuild -bs failed for version $VERSION"
fi

mapfile -t SRPMS < <(
  find "$OUTPUT_DIR" -maxdepth 1 -type f -name "bitty-${VERSION}-*.src.rpm" -print | sort
)
((${#SRPMS[@]} == 1)) ||
  die "expected exactly one SRPM for $VERSION, found ${#SRPMS[@]}"

SRPM="${SRPMS[0]}"
NVR="$(rpm -qp --qf '%{NAME} %{VERSION}' "$SRPM")"
[[ "$NVR" == "bitty $VERSION" ]] ||
  die "SRPM metadata mismatch: expected 'bitty $VERSION', got '$NVR'"

printf '%s\n' "$SRPM"
