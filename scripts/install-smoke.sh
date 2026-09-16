#!/usr/bin/env bash
# install-smoke.sh — install a freshly built package in a clean container of
# its distro and run the --version / doctor / headless smoke (034 item 5).
#
# The host must have docker and network access to the distro repositories; the
# container is discarded with `--rm`. Runtime libraries are installed
# explicitly so the smoke stays independent of how each distro resolves the
# package's declared runtime dependencies (034 item 4 / CTX-0449).
#
# Usage:
#   scripts/install-smoke.sh --distro ubuntu|fedora|arch|alpine --dir DIR
#   scripts/install-smoke.sh --distro ubuntu|fedora|arch|alpine --package PATH
#
# `--dir` picks the package for the distro from a downloaded-artifact
# directory (the Arch recipe accepts both the historical `.archlinux` suffix
# and `.pkg.tar.zst`); `--package` names the file explicitly.
#
# Exit codes: 0 = install and all three smoke steps passed, 1 = install/smoke
# failure or environment problem, 2 = usage error.
set -euo pipefail

export LC_ALL=C

DISTRO=""
PACKAGE=""
DIR=""
IMAGE=""
CANONICAL=""

usage() {
	cat <<'EOF'
Usage: scripts/install-smoke.sh --distro ubuntu|fedora|arch|alpine --dir DIR
       scripts/install-smoke.sh --distro ubuntu|fedora|arch|alpine --package PATH

Options:
  --distro NAME    Container distro to smoke test (required).
  --dir DIR        Directory of downloaded artifacts; the distro's package is
                   picked from it.
  --package PATH   Package file to install (exclusive with --dir).
  -h, --help       Show this help.

Installs the package in a clean container (ubuntu:24.04, fedora:latest,
archlinux:latest, or alpine:3.22) and runs `bitty --version`, `bitty doctor`,
and `bitty --headless`; any failing step fails the script.
EOF
}

die() {
	printf 'install-smoke: %s\n' "$*" >&2
	exit 1
}

usage_error() {
	printf 'install-smoke: %s\n' "$*" >&2
	printf 'run with --help\n' >&2
	exit 2
}

while (($#)); do
	case "$1" in
	--distro | --package | --dir)
		[[ $# -ge 2 ]] || usage_error "$1 needs a value"
		case "$1" in
		--distro) DISTRO="$2" ;;
		--package) PACKAGE="$2" ;;
		--dir) DIR="$2" ;;
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

[[ -n "$DISTRO" ]] || usage_error "--distro is required"

case "$DISTRO" in
ubuntu)
	IMAGE="ubuntu:24.04"
	CANONICAL="bitty.deb"
	PRIMARY_GLOB='*.deb'
	FALLBACK_GLOB=""
	;;
fedora)
	IMAGE="fedora:latest"
	CANONICAL="bitty.rpm"
	PRIMARY_GLOB='*.rpm'
	FALLBACK_GLOB=""
	;;
arch)
	IMAGE="archlinux:latest"
	CANONICAL="bitty.pkg.tar.zst"
	# Prefer the standard suffix; accept the historical one when it is the only
	# package present (CTX-0448).
	PRIMARY_GLOB='*.pkg.tar.zst'
	FALLBACK_GLOB='*.archlinux'
	;;
alpine)
	IMAGE="alpine:3.22"
	CANONICAL="bitty.apk"
	PRIMARY_GLOB='*.apk'
	FALLBACK_GLOB=""
	;;
*)
	die "unknown distro '$DISTRO' (expected ubuntu, fedora, arch or alpine)"
	;;
esac

if [[ -n "$PACKAGE" && -n "$DIR" ]]; then
	usage_error "--package and --dir are mutually exclusive"
elif [[ -z "$PACKAGE" && -z "$DIR" ]]; then
	usage_error "one of --package or --dir is required"
fi

if [[ -n "$DIR" ]]; then
	[[ -d "$DIR" ]] || die "package directory not found: $DIR"
	PACKAGE="$(find "$DIR" -maxdepth 1 -type f -name "$PRIMARY_GLOB" -print -quit)"
	if [[ -z "$PACKAGE" && -n "$FALLBACK_GLOB" ]]; then
		PACKAGE="$(find "$DIR" -maxdepth 1 -type f -name "$FALLBACK_GLOB" -print -quit)"
	fi
	[[ -n "$PACKAGE" ]] || die "no $DISTRO package found in $DIR"
fi
[[ -f "$PACKAGE" ]] || die "package not found: $PACKAGE"
command -v docker >/dev/null 2>&1 || die "docker not found (needed to run the clean-container smoke)"

smoke='bitty --version
env -u TERM -u DISPLAY -u WAYLAND_DISPLAY bitty doctor
env -u TERM -u DISPLAY -u WAYLAND_DISPLAY bitty --headless'

case "$DISTRO" in
ubuntu)
	install='export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends libfontconfig1 libfreetype6 libgcc-s1
dpkg -i /pkg/bitty.deb'
	;;
fedora)
	install='dnf install -y -q fontconfig freetype libgcc
dnf install -y -q /pkg/bitty.rpm'
	;;
arch)
	install='pacman -Sy --noconfirm
pacman -S --noconfirm --needed fontconfig freetype2 gcc-libs
pacman -U --noconfirm /pkg/bitty.pkg.tar.zst'
	;;
alpine)
	install='apk add --no-cache fontconfig freetype libgcc
apk add --no-cache --allow-untrusted /pkg/bitty.apk'
	;;
esac

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$PACKAGE" "$stage/$CANONICAL"

printf 'install-smoke: %s: installing %s in %s\n' "$DISTRO" "$(basename "$PACKAGE")" "$IMAGE"
docker run --rm -v "$stage:/pkg:ro" "$IMAGE" sh -euc "$install
$smoke"
printf 'install-smoke: %s: PASS\n' "$DISTRO"
