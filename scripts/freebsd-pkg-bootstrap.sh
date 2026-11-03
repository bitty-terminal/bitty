#!/bin/sh
# freebsd-pkg-bootstrap.sh — bounded, fail-closed FreeBSD pkg bootstrap
# (CTX-1674, bitty#2851).
#
# Usage:
#   scripts/freebsd-pkg-bootstrap.sh --packages "git cmake pkgconf"
#                                    [--packages ...] [options]
#
# POSIX sh on purpose: the FreeBSD guest's default shell is /bin/sh and bash
# is not part of the base system (release.yml installs `bash` *as* one of the
# prerequisites), so the helper that makes the guest usable cannot depend on a
# package that pkg — possibly wedged — would have to install first.
#
# Why this exists: the FreeBSD release job runs inside an emulated FreeBSD
# guest, and its pkg(8) bootstrap is the only step that has not been
# reproducible there. Two image faults were observed:
#
#   * `pkg update` dies when the metadata is mid-sync, and the following
#     install reports "No package(s) matching cmake";
#   * `pkg install -y pkg` and `pkg-static install -fy pkg` never return,
#     because the image enables several repositories that all carry `pkg`
#     (FreeBSD-ports, FreeBSD-ports-kmods, FreeBSD), so pkg detects a "new
#     version of pkg", re-executes itself, and loops until the job budget is
#     gone.
#
# This helper makes that step bounded and diagnosable:
#
#   * exactly one repository is enabled — the `FreeBSD` repo on the `latest`
#     train — with `FreeBSD-ports` / `FreeBSD-ports-kmods` disabled when the
#     image defines them; after the metadata refresh the script asserts that
#     `pkg -vv` reports exactly one enabled repository on a `/latest` URL, so
#     a repository the image reintroduces later cannot silently restart the
#     loop;
#   * every pkg invocation is non-interactive (ASSUME_ALWAYS_YES=yes,
#     BATCH=yes) and has an explicit wall-clock bound: timeout(1) when the
#     guest has it, otherwise a background watchdog that TERMs then KILLs;
#   * transient mirror states are retried with exponential backoff, and the
#     install retry forces a metadata refresh first (the mid-sync cure);
#   * success means every requested package answers `pkg info -e`, not that
#     some command exited 0;
#   * failure prints `pkg -vv`, every repository configuration file, and the
#     tail of the failing command's output before exiting non-zero.
#
# There is no fixed package list and no fixed repository URL: the caller
# passes both, so release.yml and bsd-tier.yml exercise the same path with
# their own prerequisite sets.
#
# Options:
#   --packages LIST       Whitespace-separated packages to install (required;
#                         repeatable, lists accumulate).
#   --repo-url URL        URL of the one enabled repository (default:
#                         https://pkg.FreeBSD.org/${ABI}/latest — `${ABI}` is
#                         expanded by pkg, not by the shell; the URL stays
#                         plain https because a `pkg+https` scheme makes pkg
#                         infer an SRV mirror and reject the URL).
#   --repo-name NAME      Repository name to enable (default: FreeBSD).
#   --repos-dir DIR       Local repository configuration directory
#                         (default: /usr/local/etc/pkg/repos).
#   --base-confs-dir DIR  Image-provided repository configuration directory,
#                         inspected for `FreeBSD-ports*` definitions and
#                         dumped on failure (default: /etc/pkg).
#   --conf-name NAME      File written into --repos-dir (default:
#                         99-bitty-bootstrap.conf, a name that sorts after the
#                         image's own files so it wins the merge).
#   --attempts N          Attempts per step (default: 2).
#   --backoff SECONDS     First retry delay; doubles per attempt (default: 10).
#   --bootstrap-timeout SECONDS
#                         Bound for the `pkg-static bootstrap -f` repair step
#                         (default: 300).
#   --update-timeout SECONDS
#                         Bound for `pkg update -f` (default: 300).
#   --install-timeout SECONDS
#                         Bound for `pkg install -y` (default: 900).
#   --diagnose-timeout SECONDS
#                         Bound for each `pkg -vv` / `pkg info` probe
#                         (default: 60).
#   --tail-lines N        Lines of captured output shown for a failed step
#                         (default: 200).
#   --pkg BINARY          pkg client to use (default: pkg).
#   --pkg-static BINARY   pkg-static client to use (default: pkg-static).
#   --sudo                Prefix privileged commands with sudo.
#   --no-sudo             Never use sudo (default: sudo when the caller is not
#                         root and sudo exists).
#   --skip-bootstrap      Skip the `pkg-static bootstrap -f` repair step.
#   --log-dir DIR         Keep the per-step logs in DIR instead of a temporary
#                         directory that is removed on success.
#   --dry-run             Print the repository configuration and every command
#                         with its bound; execute nothing and write nothing.
#   -h, --help            Show this help.
#
# Exit codes: 0 = every requested package is installed,
#             1 = bootstrap or install failed (diagnostics on stderr),
#             2 = usage error.
set -eu
LC_ALL=C
export LC_ALL

PACKAGES=""
REPO_NAME="FreeBSD"
REPO_URL="https://pkg.FreeBSD.org/\${ABI}/latest"
REPOS_DIR="/usr/local/etc/pkg/repos"
BASE_CONFS_DIR="/etc/pkg"
CONF_NAME="99-bitty-bootstrap.conf"
ATTEMPTS=2
BACKOFF=10
BOOTSTRAP_TIMEOUT=300
UPDATE_TIMEOUT=300
INSTALL_TIMEOUT=900
DIAGNOSE_TIMEOUT=60
TAIL_LINES=200
PKG_BIN="pkg"
PKG_STATIC_BIN="pkg-static"
SUDO_MODE="auto"
USE_SUDO=0
SKIP_BOOTSTRAP=0
DRY_RUN=0
KEEP_LOGS=0
LOG_DIR=""
LAST_LOG=""
LAST_RC=0

usage() {
  sed -n '2,/^# Exit codes/p' "$0" | sed -e 's/^# \{0,1\}//'
  cat <<'EOF'

Exit codes: 0 = every requested package is installed,
            1 = bootstrap or install failed (diagnostics on stderr),
            2 = usage error.
EOF
}

usage_error() {
  printf 'freebsd-pkg-bootstrap[usage]: %s\n' "$*" >&2
  printf 'run with --help\n' >&2
  exit 2
}

die() {
  printf 'freebsd-pkg-bootstrap[error]: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'freebsd-pkg-bootstrap: %s\n' "$*" >&2
}

# require_value <flag> <argc-left>
require_value() {
  [ "$2" -ge 2 ] || usage_error "$1 needs a value"
}

# positive_int <flag> <value>
positive_int() {
  case "$2" in
  '' | *[!0-9]*) usage_error "$1 must be a positive integer (got '$2')" ;;
  *) [ "$2" -ge 1 ] || usage_error "$1 must be a positive integer (got '$2')" ;;
  esac
}

# parse_packages <list> — validate then append to PACKAGES. These names reach
# `pkg install`, so a malformed entry is a usage error, never argv material.
parse_packages() {
  # shellcheck disable=SC2086 # the list is intentionally word-split
  for pp_name in $1; do
    case "$pp_name" in
    [A-Za-z0-9]*)
      case "$pp_name" in
      *[!A-Za-z0-9+._-]*) usage_error "invalid package name '$pp_name'" ;;
      esac
      ;;
    *) usage_error "invalid package name '$pp_name'" ;;
    esac
    PACKAGES="$PACKAGES $pp_name"
  done
}

while [ "$#" -gt 0 ]; do
  case "$1" in
  --packages)
    require_value "$1" "$#"
    parse_packages "$2"
    shift 2
    ;;
  --repo-url) require_value "$1" "$#"; REPO_URL="$2"; shift 2 ;;
  --repo-name) require_value "$1" "$#"; REPO_NAME="$2"; shift 2 ;;
  --repos-dir) require_value "$1" "$#"; REPOS_DIR="$2"; shift 2 ;;
  --base-confs-dir) require_value "$1" "$#"; BASE_CONFS_DIR="$2"; shift 2 ;;
  --conf-name) require_value "$1" "$#"; CONF_NAME="$2"; shift 2 ;;
  --attempts) require_value "$1" "$#"; ATTEMPTS="$2"; shift 2 ;;
  --backoff) require_value "$1" "$#"; BACKOFF="$2"; shift 2 ;;
  --bootstrap-timeout) require_value "$1" "$#"; BOOTSTRAP_TIMEOUT="$2"; shift 2 ;;
  --update-timeout) require_value "$1" "$#"; UPDATE_TIMEOUT="$2"; shift 2 ;;
  --install-timeout) require_value "$1" "$#"; INSTALL_TIMEOUT="$2"; shift 2 ;;
  --diagnose-timeout) require_value "$1" "$#"; DIAGNOSE_TIMEOUT="$2"; shift 2 ;;
  --tail-lines) require_value "$1" "$#"; TAIL_LINES="$2"; shift 2 ;;
  --pkg) require_value "$1" "$#"; PKG_BIN="$2"; shift 2 ;;
  --pkg-static) require_value "$1" "$#"; PKG_STATIC_BIN="$2"; shift 2 ;;
  --sudo) SUDO_MODE="yes"; shift ;;
  --no-sudo) SUDO_MODE="no"; shift ;;
  --skip-bootstrap) SKIP_BOOTSTRAP=1; shift ;;
  --log-dir) require_value "$1" "$#"; KEEP_LOGS=1; LOG_DIR="$2"; shift 2 ;;
  --dry-run) DRY_RUN=1; shift ;;
  -h | --help)
    usage
    exit 0
    ;;
  *) usage_error "unknown argument '$1'" ;;
  esac
done

[ -n "$PACKAGES" ] || usage_error "--packages is required"
positive_int "--attempts" "$ATTEMPTS"
positive_int "--bootstrap-timeout" "$BOOTSTRAP_TIMEOUT"
positive_int "--update-timeout" "$UPDATE_TIMEOUT"
positive_int "--install-timeout" "$INSTALL_TIMEOUT"
positive_int "--diagnose-timeout" "$DIAGNOSE_TIMEOUT"
case "$BACKOFF" in
'' | *[!0-9]*) usage_error "--backoff must be a non-negative integer" ;;
esac

# --- runner selection -------------------------------------------------------

# Every pkg invocation needs the non-interactive environment, and sudo drops
# it unless the assignment rides along with `env`, so the privileged form is
# spelled out once, in bounded_cmd.
if [ "$SUDO_MODE" != "no" ] && [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
  USE_SUDO=1
  ROOT_DESC="sudo env ASSUME_ALWAYS_YES=yes BATCH=yes"
else
  USE_SUDO=0
  ROOT_DESC="env ASSUME_ALWAYS_YES=yes BATCH=yes"
  if [ "$SUDO_MODE" = "yes" ]; then
    usage_error "--sudo was requested but sudo is unavailable or unneeded"
  fi
fi

# FreeBSD base ships timeout(1); the fallback keeps the bound when a
# stripped-down image or a test harness does not.
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"

# run_bounded <seconds> <logfile> <command...>
# Runs the command with its output captured to <logfile> and returns its exit
# status; 124 means the wall-clock bound was hit.
run_bounded() {
  rb_limit="$1"
  rb_log="$2"
  shift 2
  if [ -n "$TIMEOUT_BIN" ]; then
    rb_rc=0
    "$TIMEOUT_BIN" -k 15 "$rb_limit" "$@" >"$rb_log" 2>&1 || rb_rc=$?
    return "$rb_rc"
  fi
  "$@" >"$rb_log" 2>&1 &
  rb_pid=$!
  rb_waited=0
  while kill -0 "$rb_pid" 2>/dev/null; do
    if [ "$rb_waited" -ge "$rb_limit" ]; then
      kill -TERM "$rb_pid" 2>/dev/null || true
      sleep 5
      kill -KILL "$rb_pid" 2>/dev/null || true
      wait "$rb_pid" 2>/dev/null || true
      printf 'bounded wait of %ss exceeded; process killed\n' "$rb_limit" >>"$rb_log"
      return 124
    fi
    sleep 1
    rb_waited=$((rb_waited + 1))
  done
  rb_rc=0
  wait "$rb_pid" || rb_rc=$?
  return "$rb_rc"
}

# bounded_cmd <seconds> <logfile> <command...> — run_bounded with the
# non-interactive (and, when needed, sudo) prefix.
bounded_cmd() {
  bc_bound="$1"
  bc_log="$2"
  shift 2
  if [ "$USE_SUDO" -eq 1 ]; then
    run_bounded "$bc_bound" "$bc_log" \
      sudo env ASSUME_ALWAYS_YES=yes BATCH=yes "$@"
  else
    run_bounded "$bc_bound" "$bc_log" \
      env ASSUME_ALWAYS_YES=yes BATCH=yes "$@"
  fi
}

root_mkdir() {
  if [ "$USE_SUDO" -eq 1 ]; then
    sudo mkdir -p "$1"
  else
    mkdir -p "$1"
  fi
}

root_tee() {
  if [ "$USE_SUDO" -eq 1 ]; then
    sudo tee "$1"
  else
    tee "$1"
  fi
}

# --- repository selection ---------------------------------------------------

# repo_present <name> — true when the image defines a repository called <name>
# in either the local or the base configuration directory.
repo_present() {
  for rp_dir in "$REPOS_DIR" "$BASE_CONFS_DIR"; do
    [ -d "$rp_dir" ] || continue
    # The file this script wrote is excluded: on a re-run its own
    # `FreeBSD-ports: { enabled: no }` lines would otherwise read as an
    # image-provided repository and get copied forward.
    if grep -rlE "^[[:space:]]*${1}[[:space:]]*:" "$rp_dir" 2>/dev/null |
      grep -vxF "$REPOS_DIR/$CONF_NAME" | grep -q .; then
      return 0
    fi
  done
  return 1
}

# repo_conf_text — the configuration this script installs: every repository
# that also carries `pkg` disabled, and exactly one enabled on `latest`.
repo_conf_text() {
  for rct_repo in FreeBSD-ports FreeBSD-ports-kmods; do
    if repo_present "$rct_repo"; then
      printf '%s: { enabled: no }\n' "$rct_repo"
    fi
  done
  printf '%s: { enabled: yes, url: "%s" }\n' "$REPO_NAME" "$REPO_URL"
}

write_repo_conf() {
  root_mkdir "$REPOS_DIR" >/dev/null 2>&1 ||
    die "cannot create $REPOS_DIR (needs root: pass --sudo or run as root)"
  repo_conf_text | root_tee "$REPOS_DIR/$CONF_NAME" >/dev/null
  log "wrote $REPOS_DIR/$CONF_NAME"
}

# pkg_vv_repos <file> — print "<name>\t<enabled>\t<url>" per repository block
# in `pkg -vv` output.
pkg_vv_repos() {
  awk '
    {
      line = $0
      if (match(line, /^[[:space:]]*[A-Za-z0-9_.+-]+:[[:space:]]*\{[[:space:]]*$/)) {
        name = line
        sub(/^[[:space:]]*/, "", name)
        sub(/[[:space:]]*:.*/, "", name)
        if (prev != "") printf "%s\t%s\t%s\n", prev, en, url
        prev = name; en = "no"; url = ""
        next
      }
      if (match(line, /^[[:space:]]*enabled[[:space:]]*:/)) {
        en = line
        sub(/^[^:]*:[[:space:]]*/, "", en)
        sub(/[^A-Za-z].*$/, "", en)
        next
      }
      if (match(line, /^[[:space:]]*url[[:space:]]*:/)) {
        url = line
        sub(/^[^:]*:[[:space:]]*/, "", url)
        gsub(/["[:space:]]/, "", url)
        next
      }
    }
    END { if (prev != "") printf "%s\t%s\t%s\n", prev, en, url }
  ' "$1"
}

# assert_single_repo — fail unless exactly one repository is enabled and its
# URL is on the latest train. A second enabled repository is what turns the
# pkg self-upgrade into an unbounded re-exec loop.
assert_single_repo() {
  if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$LOG_DIR/pkg-vv-check.log" "$PKG_BIN" -vv; then
    log "'pkg -vv' failed while checking the repository set"
    return 1
  fi
  asr_enabled="$(pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" | awk -F'\t' '$2 == "yes" { print $1 }')"
  asr_count="$(printf '%s\n' "$asr_enabled" | grep -c . || true)"
  if [ "$asr_count" -ne 1 ]; then
    log "expected exactly one enabled repository, found $asr_count: ${asr_enabled:-<none>}"
    return 1
  fi
  if [ "$asr_enabled" != "$REPO_NAME" ]; then
    log "enabled repository is '$asr_enabled', expected '$REPO_NAME'"
    return 1
  fi
  if ! pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" | awk -F'\t' -v n="$REPO_NAME" \
    '$1 == n && $2 == "yes" && $3 ~ /\/latest/ { found = 1 } END { exit !found }'; then
    log "repository '$REPO_NAME' is not on a /latest URL"
    pkg_vv_repos "$LOG_DIR/pkg-vv-check.log" |
      awk -F'\t' -v n="$REPO_NAME" '$1 == n { print "  url: " $3 }' >&2
    return 1
  fi
  log "repository set is clean: $REPO_NAME on the latest train"
}

# --- diagnostics ------------------------------------------------------------

dump_tail() {
  if [ ! -s "$1" ]; then
    printf '    (no output captured)\n' >&2
    return 0
  fi
  tail -n "$2" "$1" | sed -e 's/^/    /' >&2 || true
}

# diagnose <reason> — print what is needed to tell the next occurrence apart:
# the effective repository set, the configuration that produced it, and the
# output of the command that failed. Every probe is bounded, so the dump itself
# cannot hang the job.
diagnose() {
  {
    printf '\nfreebsd-pkg-bootstrap: FAILED: %s\n' "$1"
    printf '  attempts=%s backoff=%ss bounds: bootstrap=%ss update=%ss install=%ss\n' \
      "$ATTEMPTS" "$BACKOFF" "$BOOTSTRAP_TIMEOUT" "$UPDATE_TIMEOUT" "$INSTALL_TIMEOUT"
  } >&2

  printf '\n--- repositories (pkg -vv, bound %ss) ---\n' "$DIAGNOSE_TIMEOUT" >&2
  if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$LOG_DIR/pkg-vv.log" "$PKG_BIN" -vv; then
    printf '  (pkg -vv exited non-zero; raw output follows)\n' >&2
  fi
  dump_tail "$LOG_DIR/pkg-vv.log" "$TAIL_LINES"

  printf '\n--- repository configuration ---\n' >&2
  for dg_conf in "$REPOS_DIR"/*.conf "$BASE_CONFS_DIR"/*.conf; do
    [ -f "$dg_conf" ] || continue
    printf '  %s:\n' "$dg_conf" >&2
    sed -e 's/^/    /' "$dg_conf" >&2 || true
  done

  if [ -n "$LAST_LOG" ]; then
    printf '\n--- last %s lines of %s (exit %s) ---\n' \
      "$TAIL_LINES" "$LAST_LOG" "$LAST_RC" >&2
    dump_tail "$LAST_LOG" "$TAIL_LINES"
  fi
  printf '\n' >&2
}

fail() {
  diagnose "$1"
  exit 1
}

# --- steps ------------------------------------------------------------------

# refresh_metadata — best-effort metadata refresh between install attempts;
# a failure here is not fatal because the retry owns the verdict.
refresh_metadata() {
  log "refreshing repository metadata before the next attempt"
  if ! bounded_cmd "$UPDATE_TIMEOUT" "$LOG_DIR/pkg-update-retry.log" "$PKG_BIN" update -f; then
    log "metadata refresh failed; retrying the install anyway"
  fi
  return 0
}

# retry_step <name> <bound> <refresh-on-retry:0|1> <command...>
retry_step() {
  rs_name="$1"
  rs_bound="$2"
  rs_refresh="$3"
  shift 3
  rs_log="$LOG_DIR/$rs_name.log"
  rs_attempt=1
  rs_delay="$BACKOFF"
  while [ "$rs_attempt" -le "$ATTEMPTS" ]; do
    log "step '$rs_name': attempt $rs_attempt/$ATTEMPTS, ${rs_bound}s bound"
    rs_rc=0
    bounded_cmd "$rs_bound" "$rs_log" "$@" || rs_rc=$?
    LAST_LOG="$rs_log"
    LAST_RC="$rs_rc"
    if [ "$rs_rc" -eq 0 ]; then
      return 0
    fi
    if [ "$rs_rc" -eq 124 ]; then
      log "step '$rs_name' hit its ${rs_bound}s bound without returning"
    else
      log "step '$rs_name' exited $rs_rc"
    fi
    dump_tail "$rs_log" 40
    if [ "$rs_attempt" -ge "$ATTEMPTS" ]; then
      break
    fi
    sleep "$rs_delay"
    rs_delay=$((rs_delay * 2))
    if [ "$rs_refresh" -eq 1 ]; then
      refresh_metadata
    fi
    rs_attempt=$((rs_attempt + 1))
  done
  return "$rs_rc"
}

verify_packages() {
  vp_missing=""
  vp_first_log=""
  # shellcheck disable=SC2086 # PACKAGES is a validated, space-separated list
  for vp_pkg in $PACKAGES; do
    vp_log="$LOG_DIR/verify-$vp_pkg.log"
    if ! bounded_cmd "$DIAGNOSE_TIMEOUT" "$vp_log" "$PKG_BIN" info -e "$vp_pkg"; then
      vp_missing="$vp_missing $vp_pkg"
      if [ -z "$vp_first_log" ]; then vp_first_log="$vp_log"; fi
    fi
  done
  if [ -n "$vp_missing" ]; then
    LAST_LOG="$vp_first_log"
    LAST_RC=1
    fail "packages not installed after a successful install:$vp_missing"
  fi
  log "verified installed packages:$PACKAGES"
}

# --- main -------------------------------------------------------------------

if [ "$KEEP_LOGS" -eq 1 ]; then
  mkdir -p "$LOG_DIR"
else
  # BSD mktemp(1) requires a template; GNU mktemp accepts one too.
  LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bitty-pkg-bootstrap.XXXXXX")"
  # shellcheck disable=SC2064 # expand now: this LOG_DIR is what to remove
  trap "rm -rf '$LOG_DIR'" EXIT
fi

WORST_CASE=$((ATTEMPTS * (UPDATE_TIMEOUT + INSTALL_TIMEOUT)))
if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
  WORST_CASE=$((WORST_CASE + ATTEMPTS * BOOTSTRAP_TIMEOUT))
fi
log "packages:$PACKAGES"
log "worst-case pkg time: ${WORST_CASE}s ($ATTEMPTS attempt(s) per step)"
if [ -z "$TIMEOUT_BIN" ]; then
  log "no timeout(1) on PATH; bounding each step with a background watchdog"
fi
if [ "$USE_SUDO" -eq 1 ]; then
  log "privileged commands run through sudo"
fi

if [ "$DRY_RUN" -eq 1 ]; then
  log "dry run: nothing is written or executed"
  printf '\n--- %s ---\n' "$REPOS_DIR/$CONF_NAME" >&2
  repo_conf_text | sed -e 's/^/    /' >&2
  printf '\n--- commands ---\n' >&2
  if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
    printf '    [%ss, repair, non-fatal] %s %s bootstrap -f\n' \
      "$BOOTSTRAP_TIMEOUT" "$ROOT_DESC" "$PKG_STATIC_BIN" >&2
  fi
  printf '    [%ss] %s %s update -f\n' "$UPDATE_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" >&2
  printf '    [%ss] %s %s install -y%s\n' \
    "$INSTALL_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" "$PACKAGES" >&2
  # shellcheck disable=SC2086 # validated, space-separated package list
  for dr_pkg in $PACKAGES; do
    printf '    [verify, %ss] %s %s info -e %s\n' \
      "$DIAGNOSE_TIMEOUT" "$ROOT_DESC" "$PKG_BIN" "$dr_pkg" >&2
  done
  printf '\n' >&2
  exit 0
fi

command -v "$PKG_BIN" >/dev/null 2>&1 ||
  fail "pkg client '$PKG_BIN' is not on PATH; the image has no usable pkg"

write_repo_conf

# 1. Repair a pkg that cannot read the repository metadata. `pkg-static` is the
#    base-system copy, so it works when /usr/local/sbin/pkg is wedged. This is
#    a repair, not the contract: a healthy image does not need it, and a
#    bootstrap that hits its bound must not consume the budget while the
#    shipped pkg is still usable. The hard gate is the verification at the end.
if [ "$SKIP_BOOTSTRAP" -eq 0 ]; then
  if command -v "$PKG_STATIC_BIN" >/dev/null 2>&1; then
    if retry_step pkg-bootstrap "$BOOTSTRAP_TIMEOUT" 0 "$PKG_STATIC_BIN" bootstrap -f; then
      log "pkg-static bootstrap succeeded"
    else
      log "pkg-static bootstrap did not complete (exit $LAST_RC, bound ${BOOTSTRAP_TIMEOUT}s per attempt)"
      log "continuing with the installed pkg; install and verification decide the outcome"
    fi
  else
    log "pkg-static is not on PATH; skipping the bootstrap repair step"
  fi
fi

# 2. Refresh metadata from the single repository. `-f` forces the fetch even
#    when the local metadata looks current, which is what cures the stale /
#    mid-sync metadata class of failure.
if ! retry_step pkg-update "$UPDATE_TIMEOUT" 0 "$PKG_BIN" update -f; then
  fail "'pkg update -f' failed with exit $LAST_RC after $ATTEMPTS attempt(s)"
fi

# 3. Prove the repository set is the one this script configured. A second
#    enabled repository means the self-upgrade loop can form again.
assert_single_repo || fail "repository selection is not the single latest-train repo"

# 4. Install the prerequisite set.
# shellcheck disable=SC2086 # PACKAGES is a validated, space-separated name list
if ! retry_step pkg-install "$INSTALL_TIMEOUT" 1 "$PKG_BIN" install -y $PACKAGES; then
  fail "'pkg install -y' failed with exit $LAST_RC after $ATTEMPTS attempt(s)"
fi

# 5. Success means the packages are present, not that a command exited 0.
verify_packages

log "pkg bootstrap complete"
