#!/usr/bin/env python3
"""Verify packaged clients against the exact crate versions the release uploads."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.error
import urllib.request

# The public release consumes this same list; Cargo stages and verifies selected
# packages in dependency order without uploading during the dry run.
PUBLISH_ORDER = (
    "sylphx",
    "sylphx-resource-codec",
    "sylphx-workload",
    "sylphx-apply",
    "sylphx-mcp",
    "sylphx-build-lease",
    "sylphx-cli",
)


def registry_contains(name, version):
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{name}/{version}",
        headers={"User-Agent": "SylphxAI-clients-release/1.0"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30):
            return True
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise


def ordered_packages(metadata):
    members = set(metadata["workspace_members"])
    packages = {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in members and package["publish"] != []
    }
    if set(packages) != set(PUBLISH_ORDER):
        raise RuntimeError("public workspace and release publish order differ")
    return [(name, packages[name]["version"]) for name in PUBLISH_ORDER]


def dry_run(packages):
    selected = []
    for name, version in packages:
        if registry_contains(name, version):
            print(f"registry dependency: {name}@{version}", flush=True)
        else:
            selected.extend(["--package", name])
    if not selected:
        print("all manifest versions are already on crates.io", flush=True)
        return
    # Do not overlay already-published packages with workspace sources: doing
    # so would hide an unchanged version whose registry API is stale. Cargo's
    # native multi-package dry run overlays only the actual new publish set.
    # Cargo's temporary registry overlays crates.io with the selected archives.
    # A source replacement would hide that overlay behind the published mirror.
    # Verify from a fresh Cargo home and directory, leaving the caller's source
    # configuration untouched. The manifest and lockfile still come from here.
    manifest = Path("Cargo.toml").resolve()
    with tempfile.TemporaryDirectory(prefix="sylphx-publish-") as directory:
        subprocess.run([
            "cargo", "publish", "--dry-run", "--locked", "--registry", "crates-io",
            "--manifest-path", str(manifest), *selected,
        ], check=True, cwd=directory,
            env=dict(os.environ, CARGO_HOME=str(Path(directory) / "cargo-home")))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="list the release order and versions")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        text=True,
    ))
    packages = ordered_packages(metadata)
    if args.list:
        for name, version in packages:
            print(name, version)
    else:
        dry_run(packages)


if __name__ == "__main__":
    main()
