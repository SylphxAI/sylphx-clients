#!/usr/bin/env python3
"""Verify Next against the exact SDK archive, then pack the unchanged manifest."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile


def run(args, cwd, env):
    subprocess.run(args, cwd=cwd, env=env, check=True, stdout=sys.stderr)


def pack(package, destination, env):
    try:
        result = subprocess.run(
            ["npm", "pack", "--json", "--ignore-scripts", "--pack-destination", str(destination)],
            cwd=package, env=env, check=True, capture_output=True, text=True,
        )
    except subprocess.CalledProcessError as error:
        print(error.stderr, file=sys.stderr)
        raise
    return destination / json.loads(result.stdout)[0]["filename"]


def check_installed_archive(archive, installed):
    # A successful Bun install can leave self-links for a file devDependency
    # that is also an optional peer. Prove the installed package is the archive.
    with tarfile.open(archive) as packed:
        for member in packed.getmembers():
            if not member.isfile():
                continue
            relative = Path(member.name).relative_to("package")
            if ".." in relative.parts:
                raise RuntimeError(f"invalid package member: {member.name}")
            target = installed / relative
            if target.is_symlink() or not target.is_file():
                raise RuntimeError(f"SDK archive file was not installed: {relative}")
            if target.read_bytes() != packed.extractfile(member).read():
                raise RuntimeError(f"installed SDK differs from its archive: {relative}")


def verify(root, destination=None):
    sdk = root / "typescript"
    consumer = root / "next"
    sdk_manifest = json.loads((sdk / "package.json").read_text())
    original = (consumer / "package.json").read_bytes()
    manifest = json.loads(original)
    name, version = sdk_manifest["name"], sdk_manifest["version"]
    if manifest["devDependencies"][name] != version:
        raise RuntimeError(f"Next must test its declared SDK version {version}")
    if not (sdk / "dist/index.js").is_file():
        raise RuntimeError("build the SDK before verifying its Next consumer")

    with tempfile.TemporaryDirectory(prefix="sylphx-next-package-") as temporary:
        work = Path(temporary)
        (work / "tmp").mkdir()
        env = dict(os.environ, BUN_INSTALL_CACHE_DIR=str(work / "bun-cache"),
                   BUN_TMPDIR=str(work / "tmp"), TMPDIR=str(work / "tmp"),
                   npm_config_cache=str(work / "npm-cache"))
        sdk_archive = pack(sdk, work, env)
        nxt = work / "next"
        shutil.copytree(consumer, nxt, ignore=shutil.ignore_patterns("node_modules", "dist", "bun.lock", "bun.lockb"))
        # Only the disposable consumer uses an archive path. Its peer contract
        # and all source/tests remain intact; the shipped manifest stays exact.
        manifest["devDependencies"][name] = str(sdk_archive)
        (nxt / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
        run(["bun", "install", "--no-save"], nxt, env)
        check_installed_archive(sdk_archive, nxt / "node_modules" / name)
        for command in ("typecheck", "test", "build"):
            run(["bun", "run", command], nxt, env)
        (nxt / "package.json").write_bytes(original)
        # Runtime smoke installs the archives plus the consumer's declared
        # framework peers; it never substitutes SDK source paths.
        peers = [
            f"{peer}@{spec}" for peer, spec in manifest["devDependencies"].items()
            if peer != name and peer in manifest.get("peerDependencies", {})
        ]
        run(["bash", str(root / "scripts/smoke-npm.sh"), str(nxt), str(sdk_archive), *peers], work, env)
        packed = pack(nxt, work, env)
        with tarfile.open(packed) as archive:
            if archive.extractfile("package/package.json").read() != original:
                raise RuntimeError("packed Next manifest differs from its source")
        if destination is not None:
            destination.mkdir(parents=True, exist_ok=True)
            artifact = destination / packed.name
            shutil.copyfile(packed, artifact)
            return artifact
    return None


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--pack-destination", type=Path)
    args = parser.parse_args()
    artifact = verify(args.root.resolve(), args.pack_destination.resolve() if args.pack_destination else None)
    print(artifact if artifact else "Next package verified against its SDK archive")
