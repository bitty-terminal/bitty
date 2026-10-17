#!/usr/bin/env bash
# ci-local.test.sh — CTX-1308 local Quality dispatch and cache regression tests.
set -euo pipefail

cd "$(dirname "$0")/../.."
scratch="${TMPDIR:-/tmp}/bitty"
mkdir -p "$scratch"
TMP="$(mktemp -d "$scratch/ci-local-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
FAIL=0

fail() {
  echo "FAIL: $*" >&2
  FAIL=1
}

# Run the real recipe with only its external container commands replaced.
# A separate Git repository also isolates the repository-wide act lock.
mkdir -p "$TMP/bin" "$TMP/repo" "$TMP/cargo"
git init --quiet "$TMP/repo"
cat >"$TMP/bin/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == 'image inspect bitty-act:latest' ]]
STUB
cat >"$TMP/bin/act" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" >>"$CI_LOCAL_TEST_ARGS"
if [[ -n "${CI_LOCAL_TEST_FAIL_SHARD:-}" && " $* " == *" shard:$CI_LOCAL_TEST_FAIL_SHARD "* ]]; then
  exit 23
fi
exit "${CI_LOCAL_TEST_ACT_STATUS:-0}"
STUB
chmod +x "$TMP/bin/docker" "$TMP/bin/act"
export CI_LOCAL_TEST_ARGS="$TMP/args"

# Extract the shebang recipe so CI does not need a second task runner just
# to test dispatch. Substitute the variadic argument exactly as just does.
awk '
  /^ci-local \*args:/ {recipe=1; next}
  recipe && /^    / {sub(/^    /, ""); gsub(/\{\{args\}\}/, "--concurrent-jobs 1"); print; next}
  recipe {exit}
' justfile >"$TMP/recipe.sh"
PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo" BITTY_ACT_CACHE="$TMP/cache" \
  bash -c 'cd "$1" && bash "$2"' _ "$TMP/repo" "$TMP/recipe.sh"

runner="$(awk '/^  quality:/{quality=1; next} quality && /^    runs-on:/{print $2; exit}' .github/workflows/ci.yml)"
grep -Fxq "$runner=bitty-act:latest" "$TMP/args" || fail 'Quality runner must map to the local image'
grep -Fq -- "-P $runner=bitty-act:latest" .github/act/Dockerfile || fail 'Dockerfile usage must match the Quality runner'
grep -Fxq 'quality' "$TMP/args" || fail 'dispatch must select the Quality job'
grep -Fxq -- '--concurrent-jobs' "$TMP/args" || fail 'caller arguments must reach act'
grep -Fxq -- '--bind' "$TMP/args" || fail 'local working tree must remain bind-mounted'
grep -Fq -- "$TMP/cache/target:/cache/target" "$TMP/args" || fail 'build/cache directory must persist'
grep -Fxq -- '--cache-server-addr' "$TMP/args" || fail 'local cache server must bind to proxy-exempt loopback'
grep -Fxq '127.0.0.1' "$TMP/args" || fail 'local cache endpoint must match NO_PROXY'
for shard in 1 2; do
  grep -Fxq "shard:$shard" "$TMP/args" || fail "dispatch must explicitly execute shard $shard"
done
[[ "$(grep -Fxc 'quality' "$TMP/args")" == 2 ]] || fail 'both shards must run once in separate act invocations'

# An early shard failure must propagate, without starting the next shard.
rm "$TMP/args"
status=0
PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo" BITTY_ACT_CACHE="$TMP/cache" \
  CI_LOCAL_TEST_ACT_STATUS=23 bash -c 'cd "$1" && bash "$2"' _ "$TMP/repo" "$TMP/recipe.sh" || status=$?
[[ "$status" == 23 ]] || fail 'a shard failure must fail dispatch'
[[ "$(grep -Fxc 'quality' "$TMP/args")" == 1 ]] || fail 'a failed shard must stop dispatch'

# A passing first shard must not hide a failure in the second shard.
rm "$TMP/args"
status=0
PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo" BITTY_ACT_CACHE="$TMP/cache" \
  CI_LOCAL_TEST_FAIL_SHARD=2 bash -c 'cd "$1" && bash "$2"' _ "$TMP/repo" "$TMP/recipe.sh" || status=$?
[[ "$status" == 23 ]] || fail 'a second-shard failure must fail dispatch'
[[ "$(grep -Fxc 'quality' "$TMP/args")" == 2 ]] || fail 'second-shard failure must follow a passing first shard'

# A competing invocation must fail before starting any container command.
(
  flock -x 9
  rm "$TMP/args"
  if PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo" BITTY_ACT_CACHE="$TMP/cache" \
    bash -c 'cd "$1" && bash "$2"' _ "$TMP/repo" "$TMP/recipe.sh" >"$TMP/locked.log" 2>&1; then
    fail 'a held repository lock must reject a second run'
  fi
  [[ ! -e "$TMP/args" ]] || fail 'a rejected run must not invoke act'
  grep -Fq 'another ci-local run' "$TMP/locked.log" || fail 'lock contention must be explained'
  exit "$FAIL"
) 9>"$TMP/repo/.git/ci-local.lock" || FAIL=1

# Execute the local-only workflow setup, verifying the environment seen by
# the unchanged sccache action. Separate shard directories avoid disk-cache
# server races when act runs both matrix entries concurrently.
setup="$(awk '
  /^      - name: Configure local sccache backend$/ {step=1; next}
  step && /^        if:/ {next}
  step && /^        run: \|/ {body=1; next}
  body && /^          / {sub(/^          /, ""); print; next}
  body {exit}
' .github/workflows/ci.yml)"
if [[ -z "$setup" ]]; then
  fail 'local sccache setup step must exist'
else
  awk '/^      - name: Configure local sccache backend$/ {getline; if ($0 != "        if: env.ACT") exit 1; found=1} END {if (!found) exit 1}' \
    .github/workflows/ci.yml || fail 'local backend must be limited to act'
  for shard in 1 2; do
    : >"$TMP/env-$shard"
    printf '%s\n' "$setup" | sed "s/\${{ matrix.shard }}/$shard/g" |
      GITHUB_ENV="$TMP/env-$shard" CARGO_TARGET_DIR="$TMP/target" bash -e
    grep -Fxq 'SCCACHE_GHA_ENABLED=false' "$TMP/env-$shard" || fail 'act must disable only the GHA backend'
    grep -Fxq 'SCCACHE_GHA_VERSION=' "$TMP/env-$shard" || fail 'a GHA namespace must not re-enable the remote backend locally'
    grep -Fxq "SCCACHE_DIR=$TMP/target/sccache-$shard" "$TMP/env-$shard" || fail 'local disk cache must persist per shard'
    grep -Eq '^SCCACHE_CACHE_SIZE=[1-9][0-9]*[MG]$' "$TMP/env-$shard" || fail 'disk cache must have an explicit size bound'
  done
fi
quality="$(awk '/^  quality:/{quality=1; next} quality && /^  [a-z]/{exit} quality {print}' .github/workflows/ci.yml)"
grep -Fq 'SCCACHE_GHA_ENABLED: "true"' <<<"$quality" || fail 'remote Quality GHA caching must remain enabled'
grep -Fq 'RUSTC_WRAPPER: "sccache"' <<<"$quality" || fail 'Quality compiler caching must remain enabled'

if ((FAIL)); then
  echo 'ci-local-test: FAIL' >&2
  exit 1
fi
echo 'ci-local-test: OK'
