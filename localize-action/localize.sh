#!/usr/bin/env bash
# Sylphx Localization CI step: send source and committed translation files to
# the Localization API, wait for translation to finish, and write the exported
# locale files back. It never translates by itself.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: localize.sh [--help]

Reads its settings from environment variables:

  SYLPHX_API_KEY            API key (required)
  LOCALIZE_CATALOG          catalog resource name, e.g. orgs/ORG/projects/PROJECT/envs/ENV/catalogs/web (required)
  LOCALIZE_FILES            newline-separated path templates containing {locale},
                            with an optional * glob in the last segment (required)
  LOCALIZE_FORMAT           sylphx-json | icu-json | gettext-po | xliff2 (default sylphx-json)
  LOCALIZE_LOCALE_MAP       newline-separated bcp47=dirname, e.g. zh-Hans=zh (default identity)
  LOCALIZE_PSEUDO_LOCALES   true|false: also write pseudo-locale files (default false)
  LOCALIZE_SOURCE_FALLBACK  true|false: export fills keys without a passing translation
                            with the source text (default false)
  LOCALIZE_API_URL          default https://api.sylphx.com
  LOCALIZE_FAIL_ON_QA       true|false: exit 1 when QA fails (default true)
  LOCALIZE_MAX_WAIT         seconds to keep syncing until nothing is pending (default 600)
  LOCALIZE_POLL_INTERVAL    seconds between syncs (default 2)
  LOCALIZE_OPEN_PR          true|false: commit changed files and open a PR (default false)
  LOCALIZE_PR_BRANCH        branch for the PR (default localization/update)
  GITHUB_TOKEN              token for `gh`, used only when LOCALIZE_OPEN_PR=true

Outputs are printed as key=value lines, and appended to $GITHUB_OUTPUT when set:
qa-passed, translated-characters, changed-files, report.
USAGE
}

case "${1:-}" in
  -h | --help) usage; exit 0 ;;
  "") ;;
  *) usage >&2; exit 2 ;;
esac

die() { echo "error: $*" >&2; exit 1; }

command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null || die "jq is required"

API_KEY=${SYLPHX_API_KEY:-}
[[ -n $API_KEY ]] || die "SYLPHX_API_KEY is not set"
CATALOG=${LOCALIZE_CATALOG:-}
[[ -n $CATALOG ]] || die "LOCALIZE_CATALOG is not set"
FILES=${LOCALIZE_FILES:-}
[[ -n $FILES ]] || die "LOCALIZE_FILES is not set"
FORMAT_NAME=${LOCALIZE_FORMAT:-sylphx-json}
LOCALE_MAP=${LOCALIZE_LOCALE_MAP:-}
PSEUDO=${LOCALIZE_PSEUDO_LOCALES:-false}
SOURCE_FALLBACK=${LOCALIZE_SOURCE_FALLBACK:-false}
API_URL=${LOCALIZE_API_URL:-https://api.sylphx.com}
API_URL=${API_URL%/}
FAIL_ON_QA=${LOCALIZE_FAIL_ON_QA:-true}
MAX_WAIT=${LOCALIZE_MAX_WAIT:-600}
POLL=${LOCALIZE_POLL_INTERVAL:-2}
OPEN_PR=${LOCALIZE_OPEN_PR:-false}
PR_BRANCH=${LOCALIZE_PR_BRANCH:-localization/update}
CATALOG=${CATALOG#/}

case $FORMAT_NAME in
  sylphx-json) FORMAT=sylphx_json ;;
  icu-json) FORMAT=icu_json ;;
  gettext-po) FORMAT=gettext_po ;;
  xliff2) FORMAT=xliff2 ;;
  *) die "unknown format '$FORMAT_NAME' (use sylphx-json, icu-json, gettext-po or xliff2)" ;;
esac
[[ $MAX_WAIT =~ ^[0-9]+$ ]] || die "max-wait must be a whole number of seconds"

if [[ -n ${GITHUB_ACTIONS:-} ]]; then echo "::add-mask::$API_KEY"; fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
TMP_ROOT=${RUNNER_TEMP:-${TMPDIR:-/tmp}}

# ---- HTTP ------------------------------------------------------------------

# api METHOD PATH [BODY_FILE]: response body lands in $WORK/resp.json.
api() {
  local method=$1 path=$2 body=${3:-} attempt=1 code delay ra
  local url="$API_URL/v1/$path"
  while :; do
    local args=(-sS -X "$method" -H "Authorization: Bearer $API_KEY"
      -H "Content-Type: application/json" -o "$WORK/resp.json" -D "$WORK/headers" -w '%{http_code}')
    [[ -n $body ]] && args+=(--data-binary "@$body")
    if ! code=$(curl "${args[@]}" "$url"); then code=000; fi
    case $code in
      2??) return 0 ;;
      000 | 429 | 502 | 503 | 504)
        if ((attempt < 5)); then
          ra=$(sed -n 's/^[Rr]etry-[Aa]fter:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$WORK/headers" 2>/dev/null | head -n1 || true)
          delay=${ra:-$((1 << (attempt - 1)))}
          ((delay > 60)) && delay=60
          echo "warning: $method $path returned $code, retrying in ${delay}s ($attempt/5)" >&2
          sleep "$delay"
          attempt=$((attempt + 1))
          continue
        fi
        ;;
    esac
    if jq -e . "$WORK/resp.json" >/dev/null 2>&1; then
      echo "error: $method $path failed with HTTP $code: $(jq -r '"\(.title // "error"): \(.detail // "") [\(.code // "")]"' "$WORK/resp.json")" >&2
    else
      echo "error: $method $path failed with HTTP $code: $(head -c 500 "$WORK/resp.json" 2>/dev/null)" >&2
    fi
    exit 1
  done
}

# ---- locales and paths -----------------------------------------------------

declare -A DIR_OF=()
while IFS= read -r line; do
  line=${line//[[:space:]]/}
  [[ -z $line || $line == \#* ]] && continue
  [[ $line == *=* ]] || die "locale-map line '$line' is not bcp47=dirname"
  DIR_OF[${line%%=*}]=${line#*=}
done <<<"$LOCALE_MAP"

dir_for() { printf '%s' "${DIR_OF[$1]:-$1}"; }

TEMPLATES=()
while IFS= read -r line; do
  line=${line#"${line%%[![:space:]]*}"}
  line=${line%"${line##*[![:space:]]}"}
  [[ -z $line ]] && continue
  [[ $line == *"{locale}"* ]] || die "template '$line' has no {locale}"
  rest=${line#*"{locale}"}
  [[ $rest != *"{locale}"* ]] || die "template '$line' has more than one {locale}"
  pre=${line%%"{locale}"*}
  [[ $pre != *"*"* ]] || die "template '$line': a * may only follow {locale}"
  if [[ $line == */* ]]; then
    [[ ${line%/*} != *"*"* ]] || die "template '$line': * is only allowed in the last path segment"
  fi
  TEMPLATES+=("$line")
done <<<"$FILES"
((${#TEMPLATES[@]} > 0)) || die "no path templates given"

# collect KIND LOCALE: append every existing file for LOCALE to $WORK/KIND.ndjson.
collect() {
  local kind=$1 locale=$2 d tpl pat pre f p count=0 IFS=
  d=$(dir_for "$locale")
  for tpl in "${TEMPLATES[@]}"; do
    pat=${tpl//"{locale}"/$d}
    pre=${tpl%%"{locale}"*}
    while IFS= read -r f; do
      [[ -n $f && -f $f ]] || continue
      [[ ${f:0:${#pre}+${#d}} == "$pre$d" ]] || continue
      p="$pre{locale}${f:${#pre}+${#d}}"
      jq -cn --arg path "$p" --arg locale "$locale" --rawfile content "$f" \
        '{path: $path, locale: $locale, content: $content}' >>"$WORK/$kind.ndjson"
      count=$((count + 1))
    done < <(
      shopt -s nullglob
      for f in $pat; do printf '%s\n' "$f"; done | LC_ALL=C sort
    )
  done
  echo "$kind: $count file(s) for $locale"
}

# ---- catalog ---------------------------------------------------------------

api GET "$CATALOG"
SOURCE_LOCALE=$(jq -r '.spec.source_locale // empty' "$WORK/resp.json")
[[ -n $SOURCE_LOCALE ]] || die "catalog $CATALOG has no source_locale"
mapfile -t TARGETS < <(jq -r '.spec.target_locales[]? // empty' "$WORK/resp.json")

: >"$WORK/sources.ndjson"
: >"$WORK/translations.ndjson"
collect sources "$SOURCE_LOCALE"
[[ -s $WORK/sources.ndjson ]] || die "no source files found for locale $SOURCE_LOCALE with: ${TEMPLATES[*]}"
for t in "${TARGETS[@]}"; do
  [[ $t == "$SOURCE_LOCALE" ]] && continue
  collect translations "$t"
done

jq -n --arg format "$FORMAT" --slurpfile s "$WORK/sources.ndjson" --slurpfile t "$WORK/translations.ndjson" \
  '{format: $format, sources: $s, translations: $t}' >"$WORK/sync-body.json"

# ---- sync until nothing is pending -----------------------------------------

: >"$WORK/syncs.ndjson"
deadline=$((SECONDS + MAX_WAIT))
while :; do
  api POST "$CATALOG:sync" "$WORK/sync-body.json"
  jq -c . "$WORK/resp.json" >>"$WORK/syncs.ndjson"
  pending=$(jq -r '(.pending // 0) | tonumber' "$WORK/resp.json")
  echo "sync: $pending pending"
  ((pending == 0)) && break
  ((SECONDS < deadline)) || die "still $pending pending after ${MAX_WAIT}s (max-wait); run again to continue"
  sleep "$POLL"
done

jq -s '
  def n: (. // 0) | tonumber;
  def total(f): map(f | n) | add;
  { added: total(.added), changed: total(.changed), obsoleted: total(.obsoleted),
    adopted: total(.adopted), pinned: total(.pinned), translated: total(.translated),
    failed: (last.failed | n), pending: (last.pending | n),
    translated_characters: total(.usage.translated_characters),
    memory_matches: total(.usage.memory_matches) }' "$WORK/syncs.ndjson" >"$WORK/totals.json"

# ---- export and write files ------------------------------------------------

jq -n --arg format "$FORMAT" --arg fallback "$SOURCE_FALLBACK" \
  '{format: $format} + (if $fallback == "true" then {source_fallback: true} else {} end)' >"$WORK/export-body.json"
api POST "$CATALOG:export" "$WORK/export-body.json"
cp "$WORK/resp.json" "$WORK/export.json"

CHANGED=()
while IFS=$'\t' read -r idx locale path; do
  case $locale in
    en-XA | ar-XB | zh-XW) [[ $PSEUDO == true ]] || continue ;;
  esac
  out=${path//"{locale}"/$(dir_for "$locale")}
  if [[ $out == /* || $out == .. || $out == ../* || $out == */../* || $out == */.. ]]; then
    die "refusing to write outside the repository: $out"
  fi
  jq -j --argjson i "$idx" '.files[$i].content // ""' "$WORK/export.json" >"$WORK/out.tmp"
  if [[ ! -f $out ]] || ! cmp -s "$WORK/out.tmp" "$out"; then
    mkdir -p "$(dirname "$out")"
    cp "$WORK/out.tmp" "$out"
    CHANGED+=("$out")
    echo "wrote $out"
  fi
done < <(jq -r '.files // [] | to_entries[] | [.key, .value.locale, .value.path] | @tsv' "$WORK/export.json")

# ---- report, summary, outputs ----------------------------------------------

REPORT=$TMP_ROOT/localize-report.json
CHANGED_JSON=$(if ((${#CHANGED[@]} > 0)); then printf '%s\n' "${CHANGED[@]}" | jq -R . | jq -s .; else echo '[]'; fi)
jq -n --slurpfile totals "$WORK/totals.json" --slurpfile export "$WORK/export.json" --argjson changed "$CHANGED_JSON" '
  ($export[0]) as $e
  | { catalog: $ARGS.named.catalog, sync: $totals[0],
      qa: { passed: (if $e.report.passed != null then $e.report.passed else ($e.report != null and (($e.report.errors // 0 | tonumber) == 0)) end),
            errors: ($e.report.errors // 0 | tonumber), warnings: ($e.report.warnings // 0 | tonumber),
            findings: ($e.report.findings // []) },
      pending: ($e.pending // 0 | tonumber), glyph_sets: ($e.glyph_sets // []), changed_files: $changed }' \
  --arg catalog "$CATALOG" >"$REPORT"

QA_PASSED=$(jq -r '.qa.passed' "$REPORT")
CHARS=$(jq -r '.sync.translated_characters' "$REPORT")

SUMMARY=$WORK/summary.md
{
  echo "## Sylphx Localization"
  echo
  echo "Catalog \`$CATALOG\`, source locale \`$SOURCE_LOCALE\`."
  echo
  jq -r '.sync | "| Count | |\n|---|---|\n| Added | \(.added) |\n| Changed | \(.changed) |\n| Obsoleted | \(.obsoleted) |\n| Adopted | \(.adopted) |\n| Pinned | \(.pinned) |\n| Translated | \(.translated) |\n| Memory matches | \(.memory_matches) |\n| Failed | \(.failed) |"' "$REPORT"
  echo
  echo "Translated characters (billed for this change): **$CHARS**"
  echo
  echo "Files written: ${#CHANGED[@]}"
  echo
  jq -r '.qa | "### QA: \(if .passed then "passed" else "failed" end)\n\n\(.errors) error(s), \(.warnings) warning(s)."' "$REPORT"
  if [[ $(jq '.qa.findings | length' "$REPORT") -gt 0 ]]; then
    echo
    echo "| Severity | Locale | Key | Check | Message |"
    echo "|---|---|---|---|---|"
    jq -r '
      def cell: tostring | gsub("[\r\n]+"; " ") | gsub("\\|"; "\\|");
      def isError: (.severity // "" | tostring | ascii_downcase | test("error"));
      (.qa.findings | sort_by(if isError then 0 else 1 end)) as $f
      | ($f[:200][] | "| \((.severity // "") | tostring | cell) | \(.locale // "" | cell) | \(.key // "" | cell) | \(.check // "" | cell) | \(.message // "" | cell) |"),
        (if ($f | length) > 200 then "\nand \(($f | length) - 200) more" else empty end)' "$REPORT"
  fi
  if [[ $(jq '.pending' "$REPORT") -gt 0 ]]; then
    echo
    echo "Note: the export still has $(jq '.pending' "$REPORT") pending translation(s); they fall back to the source text."
  fi
} >"$SUMMARY"

if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then cat "$SUMMARY" >>"$GITHUB_STEP_SUMMARY"; else cat "$SUMMARY"; fi

emit() {
  echo "$1=$2"
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then echo "$1=$2" >>"$GITHUB_OUTPUT"; fi
}
emit qa-passed "$QA_PASSED"
emit translated-characters "$CHARS"
emit changed-files "${#CHANGED[@]}"
emit report "$REPORT"

# ---- optional pull request -------------------------------------------------

if [[ $OPEN_PR == true ]]; then
  if ((${#CHANGED[@]} == 0)); then
    echo "no locale files changed; not opening a pull request"
  else
    command -v gh >/dev/null || die "gh is required for open-pr"
    export GH_TOKEN=${GITHUB_TOKEN:-${GH_TOKEN:-}}
    [[ -n $GH_TOKEN ]] || die "open-pr needs github-token"
    base=$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name)
    [[ -n $PR_BRANCH && $PR_BRANCH != "$base" ]] ||
      die "pr-branch must name a branch of its own, not '${PR_BRANCH:-}' (the default branch is '$base')"
    git config user.name >/dev/null || git config user.name "github-actions[bot]"
    git config user.email >/dev/null || git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
    git checkout -B "$PR_BRANCH"
    git add -- "${CHANGED[@]}"
    git commit -m "Update translations" -- "${CHANGED[@]}"
    # The branch belongs to this action: it is rewritten on every run, so the
    # lease only guards against the ref moving between this fetch and the push.
    git fetch -q origin "+refs/heads/$PR_BRANCH:refs/remotes/origin/$PR_BRANCH" 2>/dev/null || true
    git push --force-with-lease origin "$PR_BRANCH"
    number=$(gh pr list --head "$PR_BRANCH" --state open --json number --jq '.[0].number // empty')
    if [[ -n $number ]]; then
      gh pr edit "$number" --title "Update translations" --body-file "$SUMMARY"
    else
      gh pr create --base "$base" --head "$PR_BRANCH" --title "Update translations" --body-file "$SUMMARY"
    fi
  fi
fi

if [[ $QA_PASSED != true && $FAIL_ON_QA == true ]]; then
  echo "error: localization QA failed; see the report at $REPORT" >&2
  exit 1
fi
