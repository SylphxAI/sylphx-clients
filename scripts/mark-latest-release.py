#!/usr/bin/env python3
"""Mark the newest published stable client Release as GitHub's Latest."""

import json
import os
import re
import subprocess


def stable_version(tag):
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
    return tuple(map(int, match.groups())) if match else None


def mark_latest(repository, tag):
    version = stable_version(tag)
    if version is None:
        print(f"{tag} is not a stable client release; Latest is unchanged.")
        return
    response = subprocess.run(
        ["gh", "api", f"repos/{repository}/releases?per_page=100", "--paginate", "--slurp"],
        check=True, capture_output=True, text=True,
    )
    releases = [release for page in json.loads(response.stdout) for release in page]
    stable = {
        release["tag_name"]: stable_version(release["tag_name"])
        for release in releases
        if not release["draft"] and not release["prerelease"]
        and stable_version(release["tag_name"]) is not None
    }
    if tag not in stable:
        raise RuntimeError(f"{tag} is not a published stable Release")
    newest = max(stable, key=stable.get)
    if newest != tag:
        print(f"{tag} is older than {newest}; Latest is unchanged.")
        return
    subprocess.run(["gh", "release", "edit", tag, "--repo", repository, "--latest"], check=True)
    response = subprocess.run(
        ["gh", "api", f"repos/{repository}/releases/latest", "--jq", ".tag_name"],
        check=True, capture_output=True, text=True,
    )
    if response.stdout.strip() != tag:
        raise RuntimeError(f"GitHub Latest readback does not name {tag}")
    print(f"readback: {repository} Latest = {tag}")


if __name__ == "__main__":
    mark_latest(os.environ["GITHUB_REPOSITORY"], os.environ["TAG"])
