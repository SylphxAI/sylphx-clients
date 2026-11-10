#!/usr/bin/env python3
"""Select/publish the smoke-verified registry versions, with monotonic readback."""
import io
import json
import os
import re
import subprocess
import sys
import time
import zipfile

PACKAGES = [f"@sylphx/cli-{p}" for p in ("linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64")] + ["@sylphx/cli", "@sylphx/mcp"]


def stable(version):
    if not isinstance(version, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError(f"not a stable release: {version!r}")
    return tuple(map(int, version.split(".")))


def view(package):
    return json.loads(subprocess.check_output(["npm", "view", package, "dist-tags", "--json", "--prefer-online"], text=True))


def targets(cli, sdk):
    stable(cli)
    stable(sdk)
    return [(p, cli) for p in PACKAGES] + [("@sylphx/sdk", sdk)]


def eligible(items):
    # Read the full population before any write. Unknown or partially published
    # state fails; a newer candidate safely retires this older workflow run.
    superseded = False
    for package, version in items:
        tags = view(package)
        next_version = tags["next"]
        latest = tags["latest"]
        if stable(latest) > stable(version) or stable(next_version) > stable(version):
            superseded = True
        elif next_version != version:
            raise ValueError(f"{package} next={next_version}, expected {version}")
    return not superseded


def validate_candidate(candidate, run_id, attempt):
    cli, sdk, source = candidate['cli'], candidate['sdk'], candidate['source']
    stable(cli)
    stable(sdk)
    if candidate['tag'] != f"v{cli}" or not re.fullmatch(r"[0-9a-f]{40}", source):
        raise ValueError("invalid immutable production candidate")
    if (candidate['repository'] != os.environ['GITHUB_REPOSITORY']
            or candidate['run_id'] != str(run_id) or candidate['run_attempt'] != str(attempt)):
        raise ValueError("candidate receipt does not belong to the successful release attempt")
    return candidate


def github(path, *, pages=False, binary=False):
    args = ['gh', 'api', path]
    if pages:
        args += ['--paginate', '--slurp']
    data = subprocess.check_output(args, timeout=60)
    return data if binary else json.loads(data)


def newest_successful_candidate():
    # Events are wakeups, never the desired version. Read the complete retained
    # artifact population and admit only receipts from successful Release attempts.
    repo = os.environ['GITHUB_REPOSITORY']
    workflow = int(os.environ['RELEASE_WORKFLOW_ID'])
    candidates = []
    for page in github(f'repos/{repo}/actions/artifacts?per_page=100', pages=True):
        for artifact in page['artifacts']:
            match = re.fullmatch(r'production-candidate-([0-9]+)', artifact['name'])
            if not match or artifact['expired']:
                continue
            run_id, attempt = artifact['workflow_run']['id'], int(match[1])
            run = github(f'repos/{repo}/actions/runs/{run_id}/attempts/{attempt}')
            if (run['workflow_id'] != workflow or run['path'] != '.github/workflows/release.yml'
                    or run['event'] not in ('push', 'workflow_dispatch')
                    or run['status'] != 'completed' or run['conclusion'] != 'success'
                    or run['head_repository']['full_name'] != repo):
                continue
            if run['id'] != run_id or run['run_attempt'] != attempt:
                raise ValueError('Release attempt identity disagrees with artifact')
            if artifact['size_in_bytes'] > 65536:
                raise ValueError('production candidate receipt exceeds 64 KiB')
            raw = github(f"repos/{repo}/actions/artifacts/{artifact['id']}/zip", binary=True)
            with zipfile.ZipFile(io.BytesIO(raw)) as archive:
                if archive.namelist() != ['production-candidate.json']:
                    raise ValueError('invalid production candidate archive inventory')
                if archive.getinfo('production-candidate.json').file_size > 16384:
                    raise ValueError('production candidate JSON exceeds 16 KiB')
                candidate = json.loads(archive.read('production-candidate.json'))
            candidates.append(validate_candidate(candidate, run_id, attempt))
    if not candidates:
        raise ValueError('no retained successful Release receipt; production state UNKNOWN')
    candidate = max(candidates, key=lambda c: stable(c['cli']))
    newest = [c for c in candidates if c['cli'] == candidate['cli']]
    if any((c['sdk'], c['source']) != (candidate['sdk'], candidate['source']) for c in newest):
        raise ValueError('successful receipts disagree on immutable release source')
    # Retention is three days. Never fall back to an older receipt if the newest
    # published next has no current successful proof (including a still-running
    # release); its eventual completion wakes this same reconciliation again.
    if view('@sylphx/cli')['next'] != candidate['cli']:
        raise ValueError('newest npm next lacks a matching successful Release receipt')
    return candidate


def select():
    candidate = newest_successful_candidate()
    cli, sdk, source = candidate['cli'], candidate['sdk'], candidate['source']
    ok = eligible(targets(cli, sdk))
    with open(os.environ["GITHUB_OUTPUT"], "a") as out:
        out.write(f"version={cli}\nsdk={sdk}\nsource={source}\neligible={str(ok).lower()}\n")
    print(f"newest successful production candidate CLI={cli} SDK={sdk} source={source} eligible={ok}")


def promote(cli, sdk):
    items = targets(cli, sdk)
    # Recheck after smoke: next may have advanced while the consumer ran.
    if not eligible(items):
        raise ValueError("candidate superseded during production smoke; no writes")
    for package, version in items:
        observed = view(package)
        if stable(observed['latest']) > stable(version) or stable(observed['next']) > stable(version):
            raise ValueError(f"{package} already current at newer version; refusing stale promotion")
        if observed['latest'] == version:
            print(f"readback: {package} latest = {version} (already current)", flush=True)
            continue
        if observed['next'] != version:
            raise ValueError(f"{package} next changed before write; refusing promotion")
        subprocess.run(["npm", "dist-tag", "add", f"{package}@{version}", "latest"], check=True)
        for attempt in range(13):
            readback = view(package)['latest']
            if stable(readback) > stable(version):
                raise RuntimeError(f"{package} readback disagrees: latest={readback}, expected {version}")
            if readback == version:
                print(f"readback: {package} latest = {version}", flush=True)
                break
            if attempt < 12:
                time.sleep(5)
        else:
            raise RuntimeError(f"{package} latest is not {version} after 60 s")


if __name__ == "__main__":
    if sys.argv[1:] == ["select"]:
        select()
    elif sys.argv[1:] == ["promote"]:
        promote(os.environ["V"], os.environ["S"])
    else:
        sys.exit("usage: promote-npm.py select | promote")
