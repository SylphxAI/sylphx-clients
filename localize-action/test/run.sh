#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2015,SC2034  # checks are eval-ed strings; RC/OUT are read by them
# Runs localize.sh against the stub server for each path layout.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT=$HERE/../localize.sh
ROOT=$(mktemp -d)
STUB_PID=
cleanup() { [[ -n $STUB_PID ]] && kill "$STUB_PID" 2>/dev/null || true; rm -rf "$ROOT"; }
trap cleanup EXIT

fails=0
ok() { echo "ok   - $1"; }
bad() { echo "FAIL - $1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

start_stub() { # mode
  [[ -n $STUB_PID ]] && kill "$STUB_PID" 2>/dev/null || true
  rm -f "$ROOT/port" "$ROOT/log"
  : >"$ROOT/log"
  python3 "$HERE/stub_server.py" "$ROOT/port" "$ROOT/log" --mode "$1" &
  STUB_PID=$!
  for _ in $(seq 100); do [[ -s $ROOT/port ]] && break; sleep 0.1; done
  PORT=$(cat "$ROOT/port")
}

# run_case NAME: runs in a fresh repo dir $ROOT/repo; sets RC and OUT.
run_case() {
  rm -rf "$ROOT/repo" "$ROOT/tmp"
  mkdir -p "$ROOT/repo" "$ROOT/tmp"
  : >"$ROOT/log"
  (cd "$ROOT/repo" && setup) || true
}

run_script() { # env assignments are inherited
  set +e
  OUT=$(cd "$ROOT/repo" && env SYLPHX_API_KEY=test-key LOCALIZE_CATALOG=envs/e/catalogs/c \
    LOCALIZE_API_URL="http://127.0.0.1:$PORT" LOCALIZE_POLL_INTERVAL=0 RUNNER_TEMP="$ROOT/tmp" \
    GITHUB_OUTPUT="$ROOT/tmp/output" GITHUB_STEP_SUMMARY="$ROOT/tmp/summary" "$@" "$SCRIPT" 2>&1)
  RC=$?
  set -e
}
last_sync() { jq -s 'map(select(.path|endswith(":sync")))|last.body' "$ROOT/log"; }
output() { sed -n "s/^$1=//p" "$ROOT/tmp/output"; }

# ---- layout 1: i18n/{locale}.json -------------------------------------------
setup() { mkdir i18n; echo '{"a":"Hello"}' >i18n/en.json; echo '{"a":"Bonjour"}' >i18n/fr.json; }
start_stub pass; run_case; run_script LOCALIZE_FILES='i18n/{locale}.json'
check "layout1 exits 0" '[[ $RC -eq 0 ]]'
check "layout1 sync called 3 times" '[[ $(jq -s "map(select(.path|endswith(\":sync\")))|length" "$ROOT/log") -eq 3 ]]'
check "layout1 source path" '[[ $(last_sync | jq -r ".sources[0].path") == "i18n/{locale}.json" ]]'
check "layout1 source locale" '[[ $(last_sync | jq -r ".sources[0].locale") == en ]]'
check "layout1 committed fr sent" '[[ $(last_sync | jq -r "[.translations[].locale]|join(\",\")") == fr ]]'
check "layout1 format" '[[ $(last_sync | jq -r .format) == sylphx_json ]]'
check "layout1 fr written" '[[ $(jq -r .from "$ROOT/repo/i18n/fr.json") == "i18n/{locale}.json" ]]'
check "layout1 zh-Hans written by tag" '[[ -f $ROOT/repo/i18n/zh-Hans.json ]]'
check "layout1 pseudo not written" '[[ ! -e $ROOT/repo/i18n/en-XA.json ]]'
check "export omits source_fallback by default" '[[ $(jq -s "map(select(.path|endswith(\":export\")))|last.body|has(\"source_fallback\")" "$ROOT/log") == false ]]'
check "layout1 qa-passed" '[[ $(output qa-passed) == true ]]'
check "layout1 translated chars summed" '[[ $(output translated-characters) == 250 ]]'
check "layout1 changed files" '[[ $(output changed-files) -eq 2 ]]'
check "layout1 report file" 'jq -e ".qa.passed and .sync.added == 3" "$(output report)" >/dev/null'
check "layout1 summary counts" 'grep -q "| Added | 3 |" "$ROOT/tmp/summary" && grep -q "| Adopted | 5 |" "$ROOT/tmp/summary" && grep -q "| Memory matches | 3 |" "$ROOT/tmp/summary"'
check "layout1 summary warning row" 'grep -q "long \\\\| text" "$ROOT/tmp/summary"'
# second run changes nothing
run_script LOCALIZE_FILES='i18n/{locale}.json'
check "rerun changes nothing" '[[ $(output changed-files | tail -n1) -eq 0 ]]'
# pseudo enabled
run_script LOCALIZE_FILES='i18n/{locale}.json' LOCALIZE_PSEUDO_LOCALES=true
check "pseudo written when enabled" '[[ -f $ROOT/repo/i18n/en-XA.json ]]'

run_script LOCALIZE_FILES='i18n/{locale}.json' LOCALIZE_SOURCE_FALLBACK=true
check "source-fallback sent on export" '[[ $(jq -s "map(select(.path|endswith(\":export\")))|last.body.source_fallback" "$ROOT/log") == true ]]'

# ---- layout 2: directory per locale with glob -------------------------------
setup() {
  mkdir -p src/messages/en src/messages/fr
  echo '{"x":1}' >src/messages/en/common.json; echo '{"y":2}' >src/messages/en/home.json
  echo '{"x":"un"}' >src/messages/fr/common.json
}
run_case; run_script LOCALIZE_FILES='src/messages/{locale}/*.json'
check "layout2 exits 0" '[[ $RC -eq 0 ]]'
check "layout2 two sources" '[[ $(last_sync | jq -r "[.sources[].path]|sort|join(\",\")") == "src/messages/{locale}/common.json,src/messages/{locale}/home.json" ]]'
check "layout2 one committed translation" '[[ $(last_sync | jq -r "[.translations[]|.path+\"@\"+.locale]|join(\",\")") == "src/messages/{locale}/common.json@fr" ]]'
check "layout2 files written" '[[ -f src/messages/fr/home.json || -f $ROOT/repo/src/messages/fr/home.json ]] && [[ -f $ROOT/repo/src/messages/zh-Hans/common.json ]]'

# ---- layout 3: gettext, locale map, format ----------------------------------
setup() {
  mkdir -p locales/en locales/zh
  printf 'msgid "a"\nmsgstr ""\n' >locales/en/messages.po
  printf 'msgid "a"\nmsgstr "x"\n' >locales/zh/messages.po
}
run_case; run_script LOCALIZE_FILES='locales/{locale}/messages.po' LOCALIZE_FORMAT=gettext-po LOCALIZE_LOCALE_MAP='zh-Hans=zh'
check "layout3 exits 0" '[[ $RC -eq 0 ]]'
check "layout3 format" '[[ $(last_sync | jq -r .format) == gettext_po ]]'
check "layout3 mapped dir sent as zh-Hans" '[[ $(last_sync | jq -r "[.translations[]|.path+\"@\"+.locale]|join(\",\")") == "locales/{locale}/messages.po@zh-Hans" ]]'
check "layout3 mapped dir written" '[[ -f $ROOT/repo/locales/zh/messages.po && -f $ROOT/repo/locales/fr/messages.po && ! -e $ROOT/repo/locales/zh-Hans ]]'
check "layout3 exact bytes" '[[ $(cat "$ROOT/repo/locales/fr/messages.po") == "{\"locale\": \"fr\", \"from\": \"locales/{locale}/messages.po\"}" ]]'

# ---- QA error mode ----------------------------------------------------------
start_stub error
setup() { mkdir i18n; echo '{"a":"Hello"}' >i18n/en.json; }
run_case; run_script LOCALIZE_FILES='i18n/{locale}.json'
check "qa error exits 1" '[[ $RC -eq 1 ]]'
check "qa error files still written" '[[ -f $ROOT/repo/i18n/fr.json ]]'
check "qa-passed false" '[[ $(output qa-passed) == false ]]'
check "qa errors listed first" '[[ $(grep -n "| error |" "$ROOT/tmp/summary" | head -n1 | cut -d: -f1) -lt $(grep -n "| warning |" "$ROOT/tmp/summary" | head -n1 | cut -d: -f1) ]]'
run_script LOCALIZE_FILES='i18n/{locale}.json' LOCALIZE_FAIL_ON_QA=false
check "fail-on-qa=false exits 0" '[[ $RC -eq 0 ]]'

# ---- retries, auth, deadline ------------------------------------------------
start_stub flaky
run_case; run_script LOCALIZE_FILES='i18n/{locale}.json'
check "503 is retried" '[[ $RC -eq 0 ]] && grep -q "retrying" <<<"$OUT"'
start_stub pass
run_case; run_script LOCALIZE_FILES='i18n/{locale}.json' SYLPHX_API_KEY=wrong
check "problem details printed on 401" '[[ $RC -eq 1 ]] && grep -q "Unauthorized: bad key \[UNAUTHENTICATED\]" <<<"$OUT"'
start_stub pass
run_case; run_script LOCALIZE_FILES='i18n/{locale}.json' LOCALIZE_MAX_WAIT=0
check "max-wait deadline fails clearly" '[[ $RC -eq 1 ]] && grep -q "max-wait" <<<"$OUT"'
run_case; run_script LOCALIZE_FILES='i18n/*/{locale}.json'
check "bad template rejected" '[[ $RC -eq 1 ]]'
check "--help works" '"$SCRIPT" --help | grep -q Usage'

# ---- open-pr with a fake gh -------------------------------------------------
mkdir -p "$ROOT/bin"
cat >"$ROOT/bin/gh" <<'GH'
#!/usr/bin/env bash
echo "gh $*" >>"$GH_LOG"
case "$1 $2" in
  "repo view") echo main ;;
  "pr list") : ;;
esac
GH
chmod +x "$ROOT/bin/gh"
start_stub pass
setup() {
  mkdir i18n; echo '{"a":"Hello"}' >i18n/en.json; echo other >unrelated.txt
  git init -q -b main . && git config user.name t && git config user.email t@t
  git add -A && git commit -qm init
  git init -q --bare "$ROOT/remote.git" && git remote add origin "$ROOT/remote.git"
}
run_case; export GH_LOG=$ROOT/gh.log; : >"$GH_LOG"
echo dirty >"$ROOT/repo/unrelated.txt"
run_script LOCALIZE_FILES='i18n/{locale}.json' LOCALIZE_OPEN_PR=true GITHUB_TOKEN=tok PATH="$ROOT/bin:$PATH"
check "open-pr exits 0" '[[ $RC -eq 0 ]]'
check "open-pr pushed branch" 'git -C "$ROOT/remote.git" rev-parse --verify -q localization/update >/dev/null'
check "open-pr commits only locale files" '[[ $(git -C "$ROOT/remote.git" show --name-only --format= localization/update | sort | tr "\n" " ") == "i18n/fr.json i18n/zh-Hans.json " ]]'
check "open-pr creates PR" 'grep -q "pr create --base main --head localization/update --title Update translations" "$GH_LOG"'

echo
if ((fails > 0)); then echo "$fails check(s) failed"; exit 1; fi
echo "all checks passed"
