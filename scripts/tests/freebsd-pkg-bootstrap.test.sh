#!/usr/bin/env bash
# freebsd-pkg-bootstrap.test.sh — hermetic contract test for
# scripts/freebsd-pkg-bootstrap.sh (CTX-1674, bitty#2851).
#
# The FreeBSD guest is not available on a Linux or macOS host, and the failure
# this gate exists for is a pkg that never returns, so every case here drives a
# stub `pkg`/`pkg-static`:
#
#   * repository selection: `FreeBSD-ports*` disabled only when the image
#     defines them, exactly one repo enabled, `${ABI}` left for pkg, and the
#     post-update assertion rejecting a two-repo or off-`latest` set;
#   * the wall-clock bound: a hanging install is cut at the configured bound
#     (both the timeout(1) path and the watchdog fallback);
#   * retry with backoff and the metadata refresh between install attempts;
#   * the fail-closed verdict: installed packages, not exit codes, decide
#     success, and a failure prints the diagnostics needed to triage the next
#     occurrence;
#   * usage errors exit 2 and stay argv-safe.
#
# No network, no root, no FreeBSD.
set -euo pipefail

cd "$(dirname "$0")/../.."

SCRIPT=./scripts/freebsd-pkg-bootstrap.sh
FAIL=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

pass() { printf 'ok   %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*" >&2
  FAIL=1
}

# --- stub pkg clients -------------------------------------------------------

STATE=""
new_state() {
  STATE="$TMP/state.$1"
  mkdir -p "$STATE/installed"
  export PKG_FAKE_STATE="$STATE"
}

write_pkg_stub() {
  local path="$1"
  cat >"$path" <<'STUB'
#!/usr/bin/env bash
# Stub pkg / pkg-static for scripts/tests/freebsd-pkg-bootstrap.test.sh.
set -u
state="${PKG_FAKE_STATE:?PKG_FAKE_STATE is unset}"
kind="${0##*/}"
printf '%s %s\n' "$kind" "$*" >>"$state/calls.log"
printf 'ASSUME_ALWAYS_YES=%s BATCH=%s\n' \
  "${ASSUME_ALWAYS_YES:-unset}" "${BATCH:-unset}" >>"$state/env.log"
sub="${1:-}"
shift || true

case "$sub" in
update)
  echo "Updating FreeBSD repository catalogue..."
  exit 0
  ;;
-vv)
  cat "${PKG_FAKE_VV:?PKG_FAKE_VV is unset}"
  exit 0
  ;;
bootstrap)
  case "${PKG_FAKE_BOOTSTRAP_MODE:-ok}" in
  hang) sleep 120 ;;
  fail) echo "pkg bootstrap: repository unavailable" ; exit 1 ;;
  esac
  echo "Bootstrapping pkg"
  exit 0
  ;;
install)
  case "${PKG_FAKE_INSTALL_MODE:-ok}" in
  hang)
    sleep 120
    ;;
  fail-once)
    if [[ -f "$state/install-failed" ]]; then
      echo "installing"
    else
      : >"$state/install-failed"
      echo "No package(s) matching cmake"
      exit 1
    fi
    ;;
  noop)
    echo "Nothing to do"
    exit 0
    ;;
  ok)
    echo "installing"
    ;;
  esac
  # Record the packages the caller asked for, as `pkg install -y NAME...`.
  for arg in "$@"; do
    [[ "$arg" == -* ]] && continue
    : >"$state/installed/$arg"
  done
  exit 0
  ;;
info)
  # `pkg info -e NAME`
  name="${2:-}"
  echo "$name"
  [[ -f "$state/installed/$name" ]]
  exit $?
  ;;
*)
  echo "stub: unhandled subcommand '$sub'" >&2
  exit 0
  ;;
esac
STUB
  chmod +x "$path"
}

BIN="$TMP/bin"
mkdir -p "$BIN"
write_pkg_stub "$BIN/pkg"
write_pkg_stub "$BIN/pkg-static"
export PATH="$BIN:$PATH"

# --- fixtures ---------------------------------------------------------------

REPOS_SRC="$TMP/image-repos"
mkdir -p "$REPOS_SRC"
cat >"$REPOS_SRC/FreeBSD.conf" <<'CONF'
FreeBSD: {
  enabled: yes,
  url: "pkg+https://pkg.FreeBSD.org/${ABI}/quarterly",
}
CONF
cat >"$REPOS_SRC/FreeBSD-ports.conf" <<'CONF'
FreeBSD-ports: {
  enabled: yes,
  url: "pkg+https://pkg.FreeBSD.org/${ABI}/latest",
}
CONF
cat >"$REPOS_SRC/FreeBSD-ports-kmods.conf" <<'CONF'
FreeBSD-ports-kmods: {
  enabled: yes,
  url: "pkg+https://pkg.FreeBSD.org/${ABI}/latest",
}
CONF

VV_SINGLE="$TMP/vv-single.txt"
cat >"$VV_SINGLE" <<'VV'
Configuration file '/usr/local/etc/pkg.conf' not found
PKG_DBDIR = "/var/db/pkg";
ABI = "FreeBSD:15:amd64";
Repositories:
  FreeBSD: {
    url             : "https://pkg.FreeBSD.org/FreeBSD:15:amd64/latest",
    enabled         : yes,
    mirror_type     : "NONE",
    priority        : 0,
  }
VV

VV_TWO="$TMP/vv-two.txt"
cat >"$VV_TWO" <<'VV'
Repositories:
  FreeBSD-ports: {
    url             : "https://pkg.FreeBSD.org/FreeBSD:15:amd64/latest",
    enabled         : yes,
  }
  FreeBSD: {
    url             : "https://pkg.FreeBSD.org/FreeBSD:15:amd64/latest",
    enabled         : yes,
  }
VV

VV_QUARTERLY="$TMP/vv-quarterly.txt"
cat >"$VV_QUARTERLY" <<'VV'
Repositories:
  FreeBSD: {
    url             : "https://pkg.FreeBSD.org/FreeBSD:15:amd64/quarterly",
    enabled         : yes,
  }
VV

# run_case <label> <expected exit> <args...> — runs the helper, captures
# stdout+stderr into $OUT, and asserts the exit status.
OUT=""
STATUS=0
run_case() {
  local want="$1"
  shift
  STATUS=0
  OUT="$("$SCRIPT" "$@" 2>&1)" || STATUS=$?
  if [[ "$STATUS" != "$want" ]]; then
    fail "expected exit $want, got $STATUS: $*"
    printf '%s\n' "$OUT" | sed -e 's/^/       /' >&2
  fi
  return 0
}

contains() {
  local needle="$1" label="$2"
  if ! grep -qF -- "$needle" <<<"$OUT"; then
    fail "$label: missing '$needle'"
    printf '%s\n' "$OUT" | sed -e 's/^/       /' >&2
  fi
  return 0
}

not_contains() {
  local needle="$1" label="$2"
  if grep -qF -- "$needle" <<<"$OUT"; then
    fail "$label: unexpected '$needle'"
    printf '%s\n' "$OUT" | sed -e 's/^/       /' >&2
  fi
  return 0
}

# --- 0. the helper runs on the guest's base shell ---------------------------

if [ "$(head -n 1 "$SCRIPT")" = "#!/bin/sh" ]; then
  pass 'the helper targets /bin/sh (bash is not in the FreeBSD base system)'
else
  fail "the helper must start with #!/bin/sh, found: $(head -n 1 "$SCRIPT")"
fi

# A second, stricter POSIX shell catches bash-isms that /bin/sh-as-bash hides.
if command -v dash >/dev/null 2>&1; then
  new_state dash
  STATUS=0
  OUT="$(dash "$SCRIPT" --no-sudo --dry-run --packages "git cmake" \
    --repos-dir "$TMP/repos-dash" --base-confs-dir "$REPOS_SRC" 2>&1)" || STATUS=$?
  if [ "$STATUS" != 0 ]; then
    fail "dash rejected the helper (exit $STATUS)"
    printf '%s\n' "$OUT" | sed -e 's/^/       /' >&2
  else
    pass 'dash (strict POSIX) runs the helper'
  fi
else
  printf 'skip dash: not installed\n'
fi

# --- 1. dry run writes nothing and shows the intended configuration ---------

new_state dryrun
REPOS_DST="$TMP/repos-dryrun"
run_case 0 --no-sudo --dry-run --packages "git cmake" \
  --repos-dir "$REPOS_DST" --base-confs-dir "$REPOS_SRC" &&
  {
    contains 'FreeBSD-ports: { enabled: no }' 'dry run disables FreeBSD-ports'
    contains 'FreeBSD-ports-kmods: { enabled: no }' 'dry run disables ports-kmods'
    # shellcheck disable=SC2016 # ${ABI} must reach pkg unexpanded
    contains 'FreeBSD: { enabled: yes, url: "https://pkg.FreeBSD.org/${ABI}/latest" }' \
      'dry run keeps one latest repo and the pkg ${ABI} placeholder'
    contains 'install -y git cmake' 'dry run prints the install command'
    contains 'update -f' 'dry run prints the metadata refresh'
    contains 'bootstrap -f' 'dry run prints the bootstrap repair'
    if [[ -e "$REPOS_DST/99-bitty-bootstrap.conf" ]]; then
      fail 'dry run wrote the repository configuration'
    else
      pass 'dry run wrote nothing'
    fi
    if [[ -s "$STATE/calls.log" ]]; then
      fail 'dry run invoked pkg'
    else
      pass 'dry run invoked no pkg command'
    fi
  }

# --- 2. only the repositories the image defines are disabled ----------------

new_state noports
NOSRC="$TMP/no-ports"
mkdir -p "$NOSRC"
cat >"$NOSRC/FreeBSD.conf" <<'CONF'
FreeBSD: { enabled: yes, url: "pkg+https://pkg.FreeBSD.org/${ABI}/quarterly" }
CONF
run_case 0 --no-sudo --dry-run --packages git \
  --repos-dir "$TMP/repos-noports" --base-confs-dir "$NOSRC" &&
  {
    not_contains 'FreeBSD-ports' 'an undefined repository is not named'
    contains 'FreeBSD: { enabled: yes' 'the one repository stays enabled'
  }

# --- 3. happy path ----------------------------------------------------------

new_state happy
REPOS_OK="$TMP/repos-ok"
export PKG_FAKE_VV="$VV_SINGLE"
run_case 0 --no-sudo --packages "git cmake bash" \
  --repos-dir "$REPOS_OK" --base-confs-dir "$REPOS_SRC" --log-dir "$TMP/logs-happy" &&
  {
    if grep -q '^FreeBSD-ports: { enabled: no }$' "$REPOS_OK/99-bitty-bootstrap.conf"; then
      pass 'repository configuration written with FreeBSD-ports disabled'
    else
      fail 'repository configuration missing the FreeBSD-ports disable line'
      sed -e 's/^/       /' "$REPOS_OK/99-bitty-bootstrap.conf" >&2 || true
    fi
    # shellcheck disable=SC2016 # ${ABI} is the literal pkg placeholder
    if grep -qxF 'FreeBSD: { enabled: yes, url: "https://pkg.FreeBSD.org/${ABI}/latest" }' \
      "$REPOS_OK/99-bitty-bootstrap.conf"; then
      pass 'exactly one repository enabled on the latest train'
    else
      fail 'the enabled repository is not the latest-train FreeBSD repo'
    fi
    if grep -q 'install -y git cmake bash' "$STATE/calls.log"; then
      pass 'install received the exact requested package set'
    else
      fail 'install did not receive the requested package set'
      sed -e 's/^/       /' "$STATE/calls.log" >&2 || true
    fi
    if grep -q 'ASSUME_ALWAYS_YES=yes BATCH=yes' "$STATE/env.log"; then
      pass 'pkg ran non-interactively (ASSUME_ALWAYS_YES/BATCH)'
    else
      fail 'pkg did not receive the non-interactive environment'
      sed -e 's/^/       /' "$STATE/env.log" >&2 || true
    fi
    if grep -q 'pkg-static bootstrap -f' "$STATE/calls.log"; then
      pass 'the bootstrap repair used pkg-static'
    else
      fail 'pkg-static bootstrap was not attempted'
    fi
    contains 'pkg bootstrap complete' 'a green run reports completion'
  }

# --- 4. a second enabled repository fails closed ----------------------------

new_state tworepos
export PKG_FAKE_VV="$VV_TWO"
run_case 1 --no-sudo --packages git \
  --repos-dir "$TMP/repos-two" --base-confs-dir "$REPOS_SRC" &&
  {
    contains 'expected exactly one enabled repository, found 2' \
      'two enabled repositories are rejected'
    contains '--- repository configuration ---' 'the failure dumps the repo config'
    contains '99-bitty-bootstrap.conf' 'the dump names the file this script wrote'
    pass 'a two-repository set fails closed with a diagnosis'
  }

# --- 5. an off-latest repository fails closed ------------------------------

new_state quarterly
export PKG_FAKE_VV="$VV_QUARTERLY"
run_case 1 --no-sudo --packages git \
  --repos-dir "$TMP/repos-quarterly" --base-confs-dir "$REPOS_SRC" &&
  {
    contains 'is not on a /latest URL' 'a quarterly URL is rejected'
    pass 'an off-latest repository fails closed'
  }

# --- 6. a hanging install is cut at the bound ------------------------------

new_state hang
export PKG_FAKE_VV="$VV_SINGLE"
export PKG_FAKE_INSTALL_MODE="hang"
START=$SECONDS
run_case 1 --no-sudo --packages git --attempts 1 --install-timeout 2 \
  --repos-dir "$TMP/repos-hang" --base-confs-dir "$REPOS_SRC" &&
  {
    if ((SECONDS - START < 30)); then
      pass "a hanging install was cut in $((SECONDS - START))s"
    else
      fail "the bound did not cut the hanging install ($((SECONDS - START))s)"
    fi
    contains 'hit its 2s bound without returning' 'the bound is reported'
    contains 'FAILED' 'the failure is announced'
    contains 'repository configuration' 'the failure dumps the configuration'
  }
export PKG_FAKE_INSTALL_MODE="ok"

# --- 7. retry with backoff, refreshing metadata between attempts ------------

new_state retry
export PKG_FAKE_VV="$VV_SINGLE"
export PKG_FAKE_INSTALL_MODE="fail-once"
run_case 0 --no-sudo --packages git --attempts 2 --backoff 0 \
  --repos-dir "$TMP/repos-retry" --base-confs-dir "$REPOS_SRC" &&
  {
    if (($(grep -c '^pkg update' "$STATE/calls.log" || true) >= 2)); then
      pass 'the install retry refreshed the metadata first'
    else
      fail 'the retry did not refresh metadata'
      sed -e 's/^/       /' "$STATE/calls.log" >&2 || true
    fi
    contains "step 'pkg-install' exited 1" 'the first install failure is reported'
    contains 'pkg bootstrap complete' 'the retried install ends green'
  }
export PKG_FAKE_INSTALL_MODE="ok"

# --- 8. an install that exits 0 without the package is a failure -----------

new_state noop
export PKG_FAKE_VV="$VV_SINGLE"
export PKG_FAKE_INSTALL_MODE="noop"
run_case 1 --no-sudo --packages git \
  --repos-dir "$TMP/repos-noop" --base-confs-dir "$REPOS_SRC" &&
  {
    contains 'packages not installed after a successful install: git' \
      'a zero-exit install that installed nothing fails'
    pass 'the installed-package check is the verdict, not the exit code'
  }
export PKG_FAKE_INSTALL_MODE="ok"

# --- 9. a hanging bootstrap is a repair, not the contract ------------------

new_state boot-hang
export PKG_FAKE_VV="$VV_SINGLE"
export PKG_FAKE_BOOTSTRAP_MODE="hang"
run_case 0 --no-sudo --packages git --attempts 1 --bootstrap-timeout 2 \
  --repos-dir "$TMP/repos-boothang" --base-confs-dir "$REPOS_SRC" &&
  {
    contains 'pkg-static bootstrap did not complete' 'a hung bootstrap is reported'
    contains 'pkg bootstrap complete' 'the install still decides the outcome'
  }
export PKG_FAKE_BOOTSTRAP_MODE="ok"

# --- 10. usage errors -------------------------------------------------------

new_state usage
run_case 2 --no-sudo &&
  contains '--packages is required' 'a missing package list is a usage error'
run_case 2 --no-sudo --packages "-oops" &&
  contains "invalid package name '-oops'" 'a package name cannot be an option'
run_case 2 --no-sudo --packages git --attempts 0 &&
  contains '--attempts must be a positive integer' 'attempts must be positive'
run_case 2 --no-sudo --packages git --install-timeout 0 &&
  contains '--install-timeout must be a positive integer' 'bounds must be positive'
run_case 2 --no-sudo --packages git --bogus &&
  contains "unknown argument '--bogus'" 'an unknown flag is a usage error'
run_case 0 --help &&
  contains 'bounded, fail-closed FreeBSD pkg bootstrap' '--help prints the contract'

# --- 11. the watchdog fallback keeps the bound without timeout(1) ----------

new_state watchdog
SHIM="$TMP/shim"
mkdir -p "$SHIM"
missing=""
for tool in id env grep awk sed tail tee mkdir mktemp rm sleep cat; do
  if resolved="$(command -v "$tool")"; then
    ln -sf "$resolved" "$SHIM/$tool"
  else
    missing="$missing $tool"
  fi
done
for stub in pkg pkg-static; do
  ln -sf "$BIN/$stub" "$SHIM/$stub"
done
if [[ -n "$missing" ]]; then
  printf 'skip watchdog fallback: no%s in PATH\n' "$missing"
else
  export PKG_FAKE_VV="$VV_SINGLE"
  export PKG_FAKE_INSTALL_MODE="hang"
  START=$SECONDS
  STATUS=0
  OUT="$(PATH="$SHIM" "$SCRIPT" --no-sudo --packages git \
    --attempts 1 --install-timeout 2 \
    --repos-dir "$TMP/repos-watchdog" --base-confs-dir "$REPOS_SRC" 2>&1)" || STATUS=$?
  if [[ "$STATUS" != 1 ]]; then
    fail "watchdog fallback: expected exit 1, got $STATUS"
    printf '%s\n' "$OUT" | sed -e 's/^/       /' >&2
  elif ((SECONDS - START >= 40)); then
    fail "watchdog fallback did not bound the hang ($((SECONDS - START))s)"
  else
    pass "watchdog fallback bounded the hang in $((SECONDS - START))s"
    contains 'no timeout(1) on PATH' 'the fallback announces itself'
  fi
  export PKG_FAKE_INSTALL_MODE="ok"
fi

# --- verdict ----------------------------------------------------------------

if ((FAIL != 0)); then
  printf '\nfreebsd-pkg-bootstrap.test: FAILED\n' >&2
  exit 1
fi
printf '\nfreebsd-pkg-bootstrap.test: all checks passed\n'
