# Sylphx Localization CI step

A composite GitHub Action and a standalone script that keep your locale files
in step with a Sylphx Localization catalog. It never translates by itself: it
sends your source files and committed translations to the API, waits until
nothing is pending, and writes the exported files back.

Translation happens only here and in explicit API calls, never at build,
compile or runtime. Builds read the committed files.

## Workflow

```yaml
name: Localize
on:
  push:
    branches: [main]
    paths:
      - "i18n/en.json"
  workflow_dispatch:

permissions:
  contents: write
  pull-requests: write

jobs:
  localize:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4   # pin by full commit SHA in your repository
      - uses: SylphxAI/cloud/clients/localize-action@main
        with:
          catalog: orgs/ORG/projects/PROJECT/envs/ENV/catalogs/web
          api-key: ${{ secrets.SYLPHX_API_KEY }}
          files: i18n/{locale}.json
          open-pr: "true"
```

With `open-pr: "true"` the step commits only the changed locale files to
`localization/update`, force-pushes that branch, and creates or updates one PR
titled "Update translations" whose body is the job summary. Without it, the
files are written into the working tree for a later step to commit.

## Inputs

| Input | Default | Meaning |
|---|---|---|
| `catalog` | required | Catalog resource name, for example `orgs/ORG/projects/PROJECT/envs/ENV/catalogs/web`. |
| `api-key` | required | Sylphx key with `localization:read` and `localization:write`. |
| `files` | required | Newline-separated path templates (see Paths). |
| `format` | `sylphx-json` | `sylphx-json`, `icu-json`, `gettext-po` or `xliff2`. |
| `locale-map` | empty | Newline-separated `bcp47=dirname`, for example `zh-Hans=zh`. |
| `pseudo-locales` | `false` | Also write pseudo-locale files (`en-XA`, `ar-XB`, `zh-XW`). |
| `source-fallback` | `false` | The export fills keys with no passing translation with the source text, for runtimes (such as nested-JSON loaders) that need every key present. |
| `api-url` | `https://api.sylphx.com` | API base URL. |
| `fail-on-qa` | `true` | Exit 1 after writing files when QA did not pass. |
| `max-wait` | `600` | Seconds to keep syncing until nothing is pending; then fail. |
| `open-pr` | `false` | Commit changed files and create or update the PR. |
| `pr-branch` | `localization/update` | Branch for the PR. |
| `github-token` | `github.token` | Used by `gh`, only when `open-pr` is true. GitHub does not run workflows on a pull request opened with the default `github.token`; pass a GitHub App or fine-grained token when the translation PR must run your CI. |

## Outputs

| Output | Meaning |
|---|---|
| `qa-passed` | `true` or `false`. |
| `translated-characters` | Source characters translated by this change (the billed quantity), summed over the syncs. |
| `changed-files` | Number of locale files written that differ from before. |
| `report` | Path to a JSON report under `$RUNNER_TEMP`: sync totals, QA findings, pending count, glyph sets per locale and changed files. |

The job summary shows the sync counts (added, changed, obsoleted, adopted,
pinned, translated, memory matches), the translated characters, and a table of
QA findings (errors first, at most 200 rows, then "and N more").

## Paths

Each line of `files` is a path template with one `{locale}` and an optional `*`
glob in the last segment, after `{locale}`:

- `i18n/{locale}.json`
- `src/messages/{locale}/*.json`
- `locales/{locale}/messages.po`

Sources are the files for the catalog's source locale. Committed translations
are the same expansion for each target locale; missing files are simply left
out. Each file is sent with its path written with a literal `{locale}` (for
example `src/messages/{locale}/common.json`). Exported files are written to
their path with `{locale}` replaced by the locale's directory name, exactly as
returned. Paths that would leave the repository are refused. Directory names
equal the BCP 47 tag unless `locale-map` says otherwise.

Translations that fail QA are never in the exported files, so the runtime falls
back to the source text and writing the files is safe even when the step then
fails.

## Run it locally

```sh
export SYLPHX_API_KEY=...
export LOCALIZE_CATALOG=orgs/ORG/projects/PROJECT/envs/ENV/catalogs/web
export LOCALIZE_FILES='i18n/{locale}.json'
clients/localize-action/localize.sh
```

Settings are the environment variables `SYLPHX_API_KEY`, `LOCALIZE_CATALOG`,
`LOCALIZE_FILES`, `LOCALIZE_FORMAT`, `LOCALIZE_LOCALE_MAP`,
`LOCALIZE_PSEUDO_LOCALES`, `LOCALIZE_SOURCE_FALLBACK`, `LOCALIZE_API_URL`,
`LOCALIZE_FAIL_ON_QA`, `LOCALIZE_MAX_WAIT`, `LOCALIZE_OPEN_PR`,
`LOCALIZE_PR_BRANCH` and `GITHUB_TOKEN`. `localize.sh --help` prints them.
Needs bash, curl and jq; `gh` only for `open-pr`.

## Behaviour

- Errors are RFC 9457 problem details; the title, detail and code are printed
  and the step fails. Only 429, 502, 503 and 504 (and connection failures) are
  retried, up to 5 tries with backoff, honouring `Retry-After`.
- `:sync` is idempotent and is called with the same body until `pending` is 0.

## Tests

```sh
shellcheck localize.sh test/run.sh
test/run.sh
```

`test/stub_server.py` stubs the three endpoints (python 3 standard library
only); `test/run.sh` runs the script against it for each path layout.
