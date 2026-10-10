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

# 9. Live fake-CDN install legs (CTX-1061, #1861): latest.txt pointer fetch
# plus real download, sidecar verify, extract/install — file:// only, no
# external network. Extends the offline legs above; pointer resolution is
# never exercised offline, so this duplicates nothing.
FAKE_VER="0.0.99"
FAKE_TAG="v0.0.99"
FAKE_CDN="$TMP/fake-cdn"
FAKE_REL="$FAKE_CDN/bitty/releases/$FAKE_TAG"
mkdir -p "$FAKE_CDN/bitty/install" "$FAKE_REL"
printf '%s\n' "$FAKE_TAG" >"$FAKE_CDN/bitty/install/latest.txt"

write_sidecar() { # <file>: GNU sha256sum format, shasum fallback (macOS).
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" >"$1.sha256"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" >"$1.sha256"
  else
    echo "FAIL: live fake-CDN legs need sha256sum or shasum" >&2
    FAIL=1
  fi
}

make_fake_bitty() { # <path> <version>: executable --version smoke stub.
  printf '#!/bin/sh\necho "bitty %s"\n' "$2" >"$1"
  chmod +x "$1"
}

# macOS bare-binary fixtures (both arches so host-native auto-detect works
# on either runner; no tar needed on this path).
make_fake_bitty "$FAKE_REL/bitty-aarch64-apple-darwin" "$FAKE_VER"
make_fake_bitty "$FAKE_REL/bitty-x86_64-apple-darwin" "$FAKE_VER"
write_sidecar "$FAKE_REL/bitty-aarch64-apple-darwin"
write_sidecar "$FAKE_REL/bitty-x86_64-apple-darwin"

# 9a. macOS live install via the pointer (no --version): fetch, verify,
# extract-free copy, smoke.
mac_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --target aarch64-apple-darwin --bin-dir "$TMP/fake-mac-bin" 2>&1)" && mac_status=0 || mac_status=$?
if ((mac_status != 0)); then
  echo "FAIL: macOS pointer install exited $mac_status" >&2
  printf '%s\n' "$mac_out" >&2
  FAIL=1
else
  for needle in 'checksum OK' 'smoke OK'; do
    if ! grep -qF -- "$needle" <<<"$mac_out"; then
      echo "FAIL: macOS pointer install missing '$needle'" >&2
      printf '%s\n' "$mac_out" >&2
      FAIL=1
    fi
  done
  if ! "$TMP/fake-mac-bin/bitty" --version 2>&1 | grep -qF -- "$FAKE_VER"; then
    echo "FAIL: macOS pointer install binary does not report $FAKE_VER" >&2
    FAIL=1
  fi
fi

# 9b. Explicit --version against the fake CDN agrees with the pointer.
mac_exp_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --version "$FAKE_VER" --target aarch64-apple-darwin --bin-dir "$TMP/fake-mac-exp-bin" 2>&1)" && mac_exp_status=0 || mac_exp_status=$?
if ((mac_exp_status != 0)); then
  echo "FAIL: macOS explicit-version install exited $mac_exp_status" >&2
  printf '%s\n' "$mac_exp_out" >&2
  FAIL=1
elif ! "$TMP/fake-mac-exp-bin/bitty" --version 2>&1 | grep -qF -- "$FAKE_VER"; then
  echo "FAIL: macOS explicit-version install binary does not report $FAKE_VER" >&2
  FAIL=1
fi

# 9c. Corrupted macOS payload fails closed on the sidecar.
cp "$FAKE_REL/bitty-aarch64-apple-darwin" "$TMP/mac-good"
printf 'tamper' >>"$FAKE_REL/bitty-aarch64-apple-darwin"
corrupt_mac_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --version "$FAKE_VER" --target aarch64-apple-darwin --bin-dir "$TMP/fake-mac-bad" 2>&1)" && corrupt_mac_status=0 || corrupt_mac_status=$?
cp "$TMP/mac-good" "$FAKE_REL/bitty-aarch64-apple-darwin"
if ((corrupt_mac_status == 0)); then
  echo "FAIL: corrupted macOS payload installed but failure was expected" >&2
  FAIL=1
elif ! grep -qF -- 'checksum mismatch' <<<"$corrupt_mac_out"; then
  echo "FAIL: corrupted macOS payload missing checksum-mismatch error" >&2
  printf '%s\n' "$corrupt_mac_out" >&2
  FAIL=1
fi

# 9d. Linux bundle fixture (minimal TOPDIR/bin/bitty + share file).
# Linux-only: install.sh extracts with GNU tar -I zstd, which macOS bsdtar
# rejects (its -I is an inclusion pattern), and production never extracts
# bundles on macOS (it installs bare binaries). The macOS legs above plus
# the native Darwin leg cover that runner; quality covers this leg on Linux.
HAVE_BUNDLE=0
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: live Linux bundle legs run on Linux hosts only" >&2
elif ! command -v tar >/dev/null 2>&1 || ! command -v zstd >/dev/null 2>&1; then
  echo "FAIL: live Linux bundle legs need tar and zstd" >&2
  FAIL=1
else
  BUNDLE_TOP="bitty-$FAKE_VER-x86_64-unknown-linux-gnu"
  BUNDLE_STAGE="$TMP/bundle-stage"
  mkdir -p "$BUNDLE_STAGE/$BUNDLE_TOP/bin" "$BUNDLE_STAGE/$BUNDLE_TOP/share/applications"
  make_fake_bitty "$BUNDLE_STAGE/$BUNDLE_TOP/bin/bitty" "$FAKE_VER"
  chmod 755 "$BUNDLE_STAGE/$BUNDLE_TOP/bin/bitty"
  printf 'fake desktop entry\n' >"$BUNDLE_STAGE/$BUNDLE_TOP/share/applications/fake.desktop"
  BUNDLE="$FAKE_REL/$BUNDLE_TOP.tar.zst"
  if ! (cd "$BUNDLE_STAGE" && tar -c "$BUNDLE_TOP" | zstd -19 -o "$BUNDLE" 2>/dev/null); then
    echo "FAIL: could not assemble the fake Linux bundle" >&2
    FAIL=1
  else
    HAVE_BUNDLE=1
    write_sidecar "$BUNDLE"

    # 9e. Linux pointer install hermetic (--bin-dir: binary-only, no share).
    linux_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --target x86_64-unknown-linux-gnu --bin-dir "$TMP/fake-linux-bin" 2>&1)" && linux_status=0 || linux_status=$?
    if ((linux_status != 0)); then
      echo "FAIL: Linux pointer install exited $linux_status" >&2
      printf '%s\n' "$linux_out" >&2
      FAIL=1
    else
      for needle in 'checksum OK' 'smoke OK' 'skipping desktop integration'; do
        if ! grep -qF -- "$needle" <<<"$linux_out"; then
          echo "FAIL: Linux pointer install missing '$needle'" >&2
          printf '%s\n' "$linux_out" >&2
          FAIL=1
        fi
      done
      if ! "$TMP/fake-linux-bin/bitty" --version 2>&1 | grep -qF -- "$FAKE_VER"; then
        echo "FAIL: Linux pointer install binary does not report $FAKE_VER" >&2
        FAIL=1
      fi
    fi

    # 9f. Linux pointer install with --prefix carries share/ integration.
    prefix_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --target x86_64-unknown-linux-gnu --prefix "$TMP/fake-prefix" 2>&1)" && prefix_status=0 || prefix_status=$?
    if ((prefix_status != 0)); then
      echo "FAIL: Linux --prefix install exited $prefix_status" >&2
      printf '%s\n' "$prefix_out" >&2
      FAIL=1
    else
      if ! "$TMP/fake-prefix/bin/bitty" --version 2>&1 | grep -qF -- "$FAKE_VER"; then
        echo "FAIL: Linux --prefix install binary does not report $FAKE_VER" >&2
        FAIL=1
      fi
      if [[ ! -f "$TMP/fake-prefix/share/applications/fake.desktop" ]]; then
        echo "FAIL: Linux --prefix install missing share/applications/fake.desktop" >&2
        FAIL=1
      fi
      if ! grep -qF -- 'installed desktop integration' <<<"$prefix_out"; then
        echo "FAIL: Linux --prefix install missing desktop-integration note" >&2
        printf '%s\n' "$prefix_out" >&2
        FAIL=1
      fi
    fi

    # 9g. Corrupted Linux bundle fails closed on the sidecar.
    cp "$BUNDLE" "$TMP/bundle-good.tar.zst"
    printf 'tamper' >>"$BUNDLE"
    corrupt_linux_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --version "$FAKE_VER" --target x86_64-unknown-linux-gnu --bin-dir "$TMP/fake-linux-bad" 2>&1)" && corrupt_linux_status=0 || corrupt_linux_status=$?
    cp "$TMP/bundle-good.tar.zst" "$BUNDLE"
    if ((corrupt_linux_status == 0)); then
      echo "FAIL: corrupted Linux bundle installed but failure was expected" >&2
      FAIL=1
    elif ! grep -qF -- 'checksum mismatch' <<<"$corrupt_linux_out"; then
      echo "FAIL: corrupted Linux bundle missing checksum-mismatch error" >&2
      printf '%s\n' "$corrupt_linux_out" >&2
      FAIL=1
    fi
  fi
fi

# 9h. Host-native auto-detect live install (no --target): the uname path the
# per-OS CI runners actually take. Darwin (either arch) resolves a fake bare
# binary; glibc x86_64 Linux resolves the fake bundle. Other hosts select
# targets this fixture does not carry (musl, non-x86_64 Linux), so they skip.
host_os="$(uname -s)"
host_arch="$(uname -m)"
run_native=0
native_skip=""
if [[ "$host_os" == "Darwin" ]]; then
  run_native=1
elif [[ "$host_os" == "Linux" ]] && [[ "$host_arch" == "x86_64" || "$host_arch" == "amd64" ]]; then
  # Mirror install.sh is_musl(): the fixture carries only the glibc bundle.
  if [[ -f /etc/alpine-release ]]; then
    native_skip="musl host selects a bundle this fixture does not carry"
  elif command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    native_skip="musl host selects a bundle this fixture does not carry"
  elif ((HAVE_BUNDLE == 0)); then
    native_skip="no bundle fixture was assembled"
  else
    run_native=1
  fi
else
  native_skip="have $host_os/$host_arch, need Darwin or glibc x86_64 Linux"
fi
if [[ -n "$native_skip" ]]; then
  echo "SKIP: host-native leg ($native_skip)" >&2
fi
if ((run_native)); then
  native_out="$(BITTY_CDN_BASE="file://$FAKE_CDN" "$SCRIPT" --bin-dir "$TMP/fake-native-bin" 2>&1)" && native_status=0 || native_status=$?
  if ((native_status != 0)); then
    echo "FAIL: host-native pointer install exited $native_status ($host_os)" >&2
    printf '%s\n' "$native_out" >&2
    FAIL=1
  elif ! "$TMP/fake-native-bin/bitty" --version 2>&1 | grep -qF -- "$FAKE_VER"; then
    echo "FAIL: host-native install binary does not report $FAKE_VER ($host_os)" >&2
    FAIL=1
  fi
fi

if ((FAIL)); then
  echo "install-scripts-test: FAIL" >&2
  exit 1
fi
echo "install-scripts-test: OK"
