#!/usr/bin/env bash
# install-smoke.test.sh — CTX-0450 fixture test for the install smoke driver.
#
# Exercises the argument/validation paths directly and, with a docker shim
# capturing the container invocation, asserts that every distro leg installs
# the right package and runs `--version`, `doctor`, and the headless smoke. No
# container is started here; the cross-container run is the release workflow's
# platform gate.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/install-smoke.sh
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

expect_exit() { # <exit> <label> <args...>
	local want="$1" label="$2"
	shift 2
	local status=0 out
	out="$("$SCRIPT" "$@" 2>&1)" || status=$?
	if ((status != want)); then
		echo "FAIL: $label: expected exit $want, got $status" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
}

# Argument and validation paths (no docker required).
expect_exit 0 'help' --help
expect_exit 2 'no arguments'
expect_exit 2 'missing package selector' --distro ubuntu
expect_exit 2 'both selectors' --distro ubuntu --package "$TMP/x.deb" --dir "$TMP"
expect_exit 1 'unknown distro' --distro plan9 --package "$TMP/plan9.pkg"
expect_exit 1 'missing package' --distro ubuntu --package "$TMP/nope.deb"
expect_exit 1 'missing directory' --distro ubuntu --dir "$TMP/nope"
mkdir -p "$TMP/empty"
expect_exit 1 'directory without a package' --distro ubuntu --dir "$TMP/empty"

# Docker shim: capture the container invocation, start nothing.
mkdir -p "$TMP/bin" "$TMP/gnu" "$TMP/musl"
cat >"$TMP/bin/docker" <<'SH'
#!/bin/sh
printf '%s\n' "$@" >"$DOCKER_ARGS_FILE"
SH
chmod +x "$TMP/bin/docker"
export PATH="$TMP/bin:$PATH"

# One glibc artifact directory holds all three packages; each distro must pick
# its own, and the Arch leg must accept the historical `.archlinux` suffix.
: >"$TMP/gnu/bitty-x86_64-unknown-linux-gnu.deb"
: >"$TMP/gnu/bitty-x86_64-unknown-linux-gnu.rpm"
: >"$TMP/gnu/bitty-x86_64-unknown-linux-gnu.archlinux"
: >"$TMP/musl/bitty-x86_64-unknown-linux-musl.apk"

run_leg() { # <label> <distro> <image> <source-basename> <install-fragment> <selector...>
	local label="$1" distro="$2" image="$3" source="$4" fragment="$5"
	shift 5
	local args_file="$TMP/args-$label" out
	out="$(DOCKER_ARGS_FILE="$args_file" "$SCRIPT" --distro "$distro" "$@" 2>&1)" ||
		{
			echo "FAIL: $label exited non-zero" >&2
			printf '%s\n' "$out" >&2
			FAIL=1
			return
		}
	if ! grep -qF -- "$source" <<<"$out"; then
		echo "FAIL: $label did not select '$source'" >&2
		printf '%s\n' "$out" >&2
		FAIL=1
	fi
	local args
	args="$(cat "$args_file")"
	local needle
	for needle in 'run' '--rm' "$image" 'sh' '-euc' "$fragment" \
		'bitty --version' 'bitty doctor' 'bitty --headless'; do
		if ! grep -qF -- "$needle" <<<"$args"; then
			echo "FAIL: $label missing '$needle'" >&2
			printf '%s\n' "$args" >&2
			FAIL=1
		fi
	done
}

run_leg ubuntu-dir ubuntu 'ubuntu:24.04' 'bitty-x86_64-unknown-linux-gnu.deb' \
	'dpkg -i /pkg/bitty.deb' --dir "$TMP/gnu"
run_leg fedora-dir fedora 'fedora:latest' 'bitty-x86_64-unknown-linux-gnu.rpm' \
	'dnf install -y -q /pkg/bitty.rpm' --dir "$TMP/gnu"
run_leg arch-legacy arch 'archlinux:latest' 'bitty-x86_64-unknown-linux-gnu.archlinux' \
	'pacman -U --noconfirm /pkg/bitty.pkg.tar.zst' --dir "$TMP/gnu"
run_leg alpine-dir alpine 'alpine:3.22' 'bitty-x86_64-unknown-linux-musl.apk' \
	'apk add --no-cache --allow-untrusted /pkg/bitty.apk' --dir "$TMP/musl"
run_leg ubuntu-package ubuntu 'ubuntu:24.04' 'bitty-x86_64-unknown-linux-gnu.deb' \
	'dpkg -i /pkg/bitty.deb' --package "$TMP/gnu/bitty-x86_64-unknown-linux-gnu.deb"

# The Arch recipe accepts the standard `.pkg.tar.zst` name too (CTX-0448).
: >"$TMP/gnu/bitty-x86_64-unknown-linux-gnu.pkg.tar.zst"
run_leg arch-standard arch 'archlinux:latest' 'bitty-x86_64-unknown-linux-gnu.pkg.tar.zst' \
	'pacman -U --noconfirm /pkg/bitty.pkg.tar.zst' --dir "$TMP/gnu"

if ((FAIL)); then
	echo "install-smoke-test: FAIL" >&2
	exit 1
fi
echo "install-smoke-test: OK"
