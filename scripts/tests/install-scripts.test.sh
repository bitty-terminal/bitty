#!/usr/bin/env bash
# install-scripts.test.sh — CTX-1047 fixture test for the bootstrap installers.
#
# Drives scripts/install.sh URL mapping without network (explicit --version
# plus --target/--print-url, uname/ldd stubs on PATH for auto-detect legs)
# and asserts scripts/install.ps1 carries the literal Windows URL templates
# statically (pwsh cannot run on this host; per-OS execution is remote CI).
# The live latest.txt pointer fetch is never exercised here.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/install.sh
PS1_SCRIPT=./scripts/install.ps1
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

expect_exit() { # <exit> <label> <args...>
  local want="$1" label="$2"
  shift 2
  local status=0 out
  out="$(BITTY_CDN_BASE='https://cdn.bitty.run' "$SCRIPT" "$@" 2>&1)" || status=$?
  if ((status != want)); then
    echo "FAIL: $label: expected exit $want, got $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
  fi
}

expect_url() { # <label> <target> <artifact-url>
  local label="$1" target="$2" want="$3"
  local out status=0
  out="$(BITTY_CDN_BASE='https://cdn.bitty.run' "$SCRIPT" --version 0.0.23 --target "$target" --print-url 2>&1)" || status=$?
  if ((status != 0)); then
    echo "FAIL: $label: --print-url exited $status" >&2
    printf '%s\n' "$out" >&2
    FAIL=1
    return
  fi
  local got brittle
  got="$(printf '%s\n' "$out" | head -n 1)"
  brittle="$(printf '%s\n' "$out" | sed -n '2p')"
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: $label: URL mismatch" >&2
    echo "  want: $want" >&2
    echo "  got:  $got" >&2
    FAIL=1
  fi
  if [[ "$brittle" != "${want}.sha256" ]]; then
    echo "FAIL: $label: sidecar URL mismatch" >&2
    echo "  want: ${want}.sha256" >&2
    echo "  got:  $brittle" >&2
    FAIL=1
  fi
}

# 1. Literal layout-table conformance at TAG=v0.0.23 (packaging/README.md).
expect_url 'linux x86_64 glibc' x86_64-unknown-linux-gnu \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst'
expect_url 'linux aarch64 glibc' aarch64-unknown-linux-gnu \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-aarch64-unknown-linux-gnu.tar.zst'
expect_url 'linux x86_64 musl' x86_64-unknown-linux-musl \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-musl.tar.zst'
expect_url 'macos x86_64' x86_64-apple-darwin \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-x86_64-apple-darwin'
expect_url 'macos arm64' aarch64-apple-darwin \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-aarch64-apple-darwin'

# 2. v-prefixed explicit versions normalize to the same TAG.
out="$(BITTY_CDN_BASE='https://cdn.bitty.run' "$SCRIPT" --version v0.0.23 --target x86_64-apple-darwin --print-url 2>&1 | head -n 1)"
if [[ "$out" != 'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-x86_64-apple-darwin' ]]; then
  echo "FAIL: v-prefixed --version did not normalize" >&2
  printf '%s\n' "$out" >&2
  FAIL=1
fi

# 3. Argument and validation paths (no network: explicit version everywhere).
expect_exit 0 'help' --help
expect_exit 2 'unknown argument' --version 0.0.23 --target x86_64-apple-darwin --bogus
expect_exit 1 'bad version' --version nope --target x86_64-apple-darwin --print-url
expect_exit 1 'prerelease rejected' --version 0.0.23-rc1 --target x86_64-apple-darwin --print-url
expect_exit 1 'unknown target' --version 0.0.23 --target riscv64-unknown-linux-gnu --print-url

# 4. --dry-run prints the plan without downloading.
dry="$(BITTY_CDN_BASE='https://cdn.bitty.run' "$SCRIPT" --version 0.0.23 --target x86_64-unknown-linux-gnu --dry-run 2>&1)" || {
  echo "FAIL: --dry-run exited non-zero" >&2
  printf '%s\n' "$dry" >&2
  FAIL=1
}
for needle in 'TAG=v0.0.23' 'TARGET=x86_64-unknown-linux-gnu' \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst' \
  'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst.sha256'; do
  if ! grep -qF -- "$needle" <<<"$dry"; then
    echo "FAIL: --dry-run missing '$needle'" >&2
    printf '%s\n' "$dry" >&2
    FAIL=1
  fi
done

# 4b. A custom --bin-dir without --prefix stays binary-only (hermetic).
dry_bin="$(BITTY_CDN_BASE='https://cdn.bitty.run' "$SCRIPT" --version 0.0.23 --target x86_64-unknown-linux-gnu --bin-dir "$TMP/bin" --dry-run 2>&1)" || {
  echo "FAIL: --dry-run --bin-dir exited non-zero" >&2
  printf '%s\n' "$dry_bin" >&2
  FAIL=1
}
if ! grep -qF -- 'desktop integration skipped' <<<"$dry_bin"; then
  echo "FAIL: --dry-run --bin-dir missing desktop-integration skip note" >&2
  printf '%s\n' "$dry_bin" >&2
  FAIL=1
fi

# 5. uname stub legs: FreeBSD and Windows Git-Bash fail closed with guidance.
mkdir -p "$TMP/freebsd-bin" "$TMP/mingw-bin" "$TMP/linux-bin" "$TMP/ldd-musl" "$TMP/ldd-gnu"
cat >"$TMP/freebsd-bin/uname" <<'STUB'
#!/bin/sh
if [ "$1" = "-s" ]; then echo FreeBSD; else echo x86_64; fi
STUB
cat >"$TMP/mingw-bin/uname" <<'STUB'
#!/bin/sh
if [ "$1" = "-s" ]; then echo MINGW64_NT-10.0; else echo x86_64; fi
STUB
cat >"$TMP/linux-bin/uname" <<'STUB'
#!/bin/sh
if [ "$1" = "-s" ]; then echo Linux; else echo x86_64; fi
STUB
cat >"$TMP/ldd-musl/ldd" <<'STUB'
#!/bin/sh
echo "musl libc (x86_64)"
STUB
cat >"$TMP/ldd-gnu/ldd" <<'STUB'
#!/bin/sh
echo "ldd (GNU libc) 2.39"
STUB
chmod +x "$TMP/freebsd-bin/uname" "$TMP/mingw-bin/uname" "$TMP/linux-bin/uname" \
  "$TMP/ldd-musl/ldd" "$TMP/ldd-gnu/ldd"

freebsd_out="$(PATH="$TMP/freebsd-bin:$PATH" BITTY_CDN_BASE='https://cdn.bitty.run' \
  "$SCRIPT" --version 0.0.23 --dry-run 2>&1)" && freebsd_status=0 || freebsd_status=$?
if ((freebsd_status == 0)); then
  echo "FAIL: FreeBSD leg passed but failure was expected" >&2
  FAIL=1
elif ! grep -qF -- 'FreeBSD is not a bootstrap target' <<<"$freebsd_out"; then
  echo "FAIL: FreeBSD leg missing guidance" >&2
  printf '%s\n' "$freebsd_out" >&2
  FAIL=1
fi

mingw_out="$(PATH="$TMP/mingw-bin:$PATH" BITTY_CDN_BASE='https://cdn.bitty.run' \
  "$SCRIPT" --version 0.0.23 --dry-run 2>&1)" && mingw_status=0 || mingw_status=$?
if ((mingw_status == 0)); then
  echo "FAIL: Windows Git-Bash leg passed but failure was expected" >&2
  FAIL=1
elif ! grep -qF -- 'install.ps1' <<<"$mingw_out"; then
  echo "FAIL: Windows Git-Bash leg missing install.ps1 guidance" >&2
  printf '%s\n' "$mingw_out" >&2
  FAIL=1
fi

# 6. musl/glibc auto-detect via ldd stub (Linux x86_64 uname stub).
musl_url="$(PATH="$TMP/linux-bin:$TMP/ldd-musl:/usr/bin:/bin" BITTY_CDN_BASE='https://cdn.bitty.run' \
  "$SCRIPT" --version 0.0.23 --print-url 2>&1 | head -n 1)"
if [[ "$musl_url" != 'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-musl.tar.zst' ]]; then
  echo "FAIL: musl auto-detect URL mismatch: $musl_url" >&2
  FAIL=1
fi
gnu_url="$(PATH="$TMP/linux-bin:$TMP/ldd-gnu:/usr/bin:/bin" BITTY_CDN_BASE='https://cdn.bitty.run' \
  "$SCRIPT" --version 0.0.23 --print-url 2>&1 | head -n 1)"
if [[ "$gnu_url" != 'https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst' ]]; then
  echo "FAIL: glibc auto-detect URL mismatch: $gnu_url" >&2
  FAIL=1
fi

# 7. install.ps1 static conformance (pwsh unavailable here; remote CI runs it).
if [[ ! -f "$PS1_SCRIPT" ]]; then
  echo "FAIL: $PS1_SCRIPT missing (PowerShell has no in-repo precedent; placed alongside install.sh)" >&2
  FAIL=1
else
  for needle in 'bitty/install/latest.txt' 'bitty/releases/' \
    'bitty-$VersionBare-windows-x86_64.zip' 'bitty-aarch64-pc-windows-msvc.exe' \
    'irm https://cdn.bitty.run/bitty/install/install.ps1 | iex' \
    'Expand-Archive' 'Get-FileHash'; do
    if ! grep -qF -- "$needle" "$PS1_SCRIPT"; then
      echo "FAIL: install.ps1 missing '$needle'" >&2
      FAIL=1
    fi
  done
fi

# 8. macOS /bin/bash is 3.2: pin install.sh free of bash4-isms.
# Only portable constructs allowed ([[ ]], local, trap, mktemp -d, pipefail
# are 3.2-safe). Case-insensitive comparison must use tr, not ${var,,}.
if grep -Eq -- '\$\{[^}]*,,|\$\{[^}]*\^\^}' "$SCRIPT"; then
  echo "FAIL: install.sh uses \${,,}/\${^^} (bash 4+, breaks macOS /bin/bash 3.2)" >&2
  FAIL=1
fi
if grep -Eq -- '(^|[^A-Za-z0-9_])(mapfile|readarray)([^A-Za-z0-9_]|$)' "$SCRIPT"; then
  echo "FAIL: install.sh uses mapfile/readarray (bash 4+, breaks macOS /bin/bash 3.2)" >&2
  FAIL=1
fi
if grep -Eq -- 'declare[[:space:]]+-[A-Za-z]*A' "$SCRIPT"; then
  echo "FAIL: install.sh uses declare -A (bash 4+, breaks macOS /bin/bash 3.2)" >&2
  FAIL=1
fi

if ((FAIL)); then
  echo "install-scripts-test: FAIL" >&2
  exit 1
fi
echo "install-scripts-test: OK"
