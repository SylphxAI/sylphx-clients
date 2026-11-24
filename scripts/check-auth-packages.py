#!/usr/bin/env python3
"""Build and smoke-test the exact Auth and Auth Solid archives without publishing."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

AUTH = "@sylphx/auth"
SOLID = "@sylphx/auth-solid"
FRAMEWORK = "solid-js"
REGISTRY = "https://registry.npmjs.org"
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?")
FIXTURES = (
    "auth-package-smoke-fetch.mjs",
    "auth-package-smoke-solid.tsx",
    "auth-package-smoke-vite.mjs",
)


def run(args, cwd, env):
    subprocess.run(args, cwd=cwd, env=env, check=True, stdout=sys.stderr)


def read_json(path):
    return json.loads(path.read_bytes())


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def validate_manifest(manifest, name, dependencies):
    if manifest["name"] != name or not VERSION.fullmatch(manifest["version"]):
        raise ValueError(f"invalid package name or version: {name}")
    if manifest.get("private") or manifest.get("workspaces") or manifest.get("type") != "module":
        raise ValueError(f"expected an independent public ESM package: {name}")
    if manifest.get("dependencies", {}) != dependencies or manifest.get("optionalDependencies"):
        raise ValueError(f"{name} must use exactly these runtime dependencies: {dependencies}")
    peers = manifest.get("peerDependencies", {})
    if set(peers) != ({FRAMEWORK} if name == SOLID else set()):
        raise ValueError(f"unexpected peer dependencies: {name}")
    if any(not VERSION.fullmatch(spec) for spec in manifest["devDependencies"].values()):
        raise ValueError(f"expected pinned registry development dependencies: {name}")


def export_paths(manifest, name):
    keys = {".": {"types": ".d.ts", "default": ".js"}}
    if name == AUTH:
        keys["./client"] = keys["."]
    else:
        keys["."]["browser"] = ".js"
        keys["./sign-in.css"] = ".css"
    exports = manifest["exports"]
    for key, expected in keys.items():
        if key not in exports:
            raise ValueError(f"{name} is missing export {key}")
        if isinstance(expected, dict):
            if not isinstance(exports[key], dict) or not expected.keys() <= exports[key].keys():
                raise ValueError(f"{name} {key} must export {', '.join(expected)}")
            # Conditional resolution is ordered; default would shadow a later browser/types entry.
            if list(exports[key])[-1] != "default":
                raise ValueError(f"{name} {key}: default must be the fallback condition")
            targets = [(exports[key][condition], suffix) for condition, suffix in expected.items()]
        else:
            targets = [(exports[key], expected)]
        for target, suffix in targets:
            if not isinstance(target, str) or not target.startswith("./dist/") or not target.endswith(suffix):
                raise ValueError(f"{name} export {key} is not a built {suffix}: {target!r}")
            relative = Path(target)
            if ".." in relative.parts:
                raise ValueError(f"{name} export {key} may not traverse: {target!r}")
            yield relative
    if name == SOLID and exports["."]["browser"] == exports["."]["default"]:
        raise ValueError("Solid must export separate browser and SSR builds")


def archive_files(archive):
    with tarfile.open(archive, "r:gz") as packed:
        files = {}
        for member in packed:
            relative = Path(member.name).relative_to("package")
            if ".." in relative.parts or relative in files or not member.isfile():
                raise ValueError(f"invalid or duplicate file in {archive.name}: {member.name}")
            files[relative] = packed.extractfile(member).read()
        return files


def check_installed(archive, node_modules, name):
    installed = node_modules / name
    # Compare all packed files (including the manifest), not just a resolution or version.
    for relative, content in archive_files(archive).items():
        path = installed / relative
        if path.resolve() != path or not path.is_file() or path.read_bytes() != content:
            raise ValueError(f"{name}: expected a regular installed copy of {archive.name}:{relative}")


def pack(source, staging, destination, env, manifest, dependency=None):
    shutil.copytree(source, staging, ignore=shutil.ignore_patterns(
        "node_modules", "dist", ".git", "*.tgz", ".npmrc", "bunfig.toml",
    ))
    staged_manifest = staging / "package.json"
    original = staged_manifest.read_bytes()
    if dependency is not None:
        # Only the disposable build copy points to the not-yet-published archive.
        write_json(staged_manifest, {**manifest, "dependencies": {AUTH: dependency.as_uri()}})
    run(["bun", "install", "--no-save", "--ignore-scripts", "--linker", "hoisted",
         "--registry", REGISTRY], staging, env)
    if dependency is not None:
        check_installed(dependency, staging / "node_modules", AUTH)
    run(["bun", "run", "typecheck"], staging, env)
    run(["bun", "run", "test"], staging, env)
    run(["bun", "run", "build"], staging, env)
    if dependency is not None:
        staged_manifest.write_bytes(original)
    # npm reads the restored manifest; do not extract or repack these bytes later.
    result = subprocess.run(
        ["npm", "pack", "--json", "--ignore-scripts", "--pack-destination", str(destination)],
        cwd=staging, env=env, check=True, stdout=subprocess.PIPE, text=True,
    )
    name, version = manifest["name"], manifest["version"]
    filename = f"{name.removeprefix('@').replace('/', '-')}-{version}.tgz"
    if json.loads(result.stdout)[0]["filename"] != filename:
        raise ValueError(f"expected npm to pack {filename}")
    archive = destination / filename
    files = archive_files(archive)
    if files.get(Path("package.json")) != original:
        raise ValueError(f"{name}: packed manifest differs from its source")
    for path in export_paths(manifest, name):
        if not files.get(path):
            raise ValueError(f"{name}: packed export missing or empty: {path}")
    return archive


def smoke(consumer, archives, tools, env, browser):
    consumer.mkdir()
    for filename in FIXTURES:
        shutil.copyfile(Path(__file__).with_name(filename), consumer / filename)
    write_json(consumer / "package.json", {
        "name": "auth-package-smoke", "private": True, "type": "module",
        "dependencies": {name: archive.as_uri() for name, archive in archives.items()},
        "devDependencies": {key: tools[key] for key in (
            FRAMEWORK, "typescript", "vite", "vite-plugin-solid",
        )},
    })
    run(["npm", "install", "--registry", REGISTRY, "--ignore-scripts", "--strict-peer-deps",
         "--no-audit", "--no-fund"], consumer, env)
    for name, archive in archives.items():
        check_installed(archive, consumer / "node_modules", name)
    run(["node", "auth-package-smoke-fetch.mjs"], consumer, env)
    run(["node", "node_modules/typescript/bin/tsc", "--noEmit", "--strict", "--module", "NodeNext",
         "--target", "ES2022", "--jsx", "preserve", "--jsxImportSource", "solid-js", "--types",
         "vite/client", "auth-package-smoke-solid.tsx"], consumer, env)
    run(["node", "auth-package-smoke-vite.mjs", *([str(browser)] if browser else [])], consumer, env)


def verify(root, destination, browser, *, allow_absent=False):
    if not root.is_dir():
        raise ValueError(f"client root is not a directory: {root}")
    sdk = root / "auth"
    solid = root / "auth-solid"
    if not any(path.exists() or path.is_symlink() for path in (sdk, solid)):
        if not allow_absent:
            raise ValueError("missing Auth packages: expected auth and auth-solid")
        print("Auth packages did not exist at this revision; skipping", file=sys.stderr)
        return []
    if not all((path / "package.json").is_file() for path in (sdk, solid)):
        raise ValueError("partial Auth package set: both auth and auth-solid must have package.json")
    auth_manifest = read_json(sdk / "package.json")
    solid_manifest = read_json(solid / "package.json")
    validate_manifest(auth_manifest, AUTH, {})
    validate_manifest(solid_manifest, SOLID, {AUTH: auth_manifest["version"]})
    for name, manifest in ((AUTH, auth_manifest), (SOLID, solid_manifest)):
        list(export_paths(manifest, name))

    with tempfile.TemporaryDirectory(prefix="sylphx-auth-packages-") as temp:
        work = Path(temp).resolve()
        for directory in ("home", "tmp", "archives"):
            (work / directory).mkdir()
        # Read public build dependencies only; do not pass inherited tokens, npmrc,
        # node loader flags, workspace configuration or the caller's caches to children.
        env = {
            "PATH": os.pathsep.join(str(Path(part).resolve()) for part in os.get_exec_path()),
            "HOME": str(work / "home"),
            "BUN_INSTALL_CACHE_DIR": str(work / "bun-cache"),
            "BUN_TMPDIR": str(work / "tmp"), "TMPDIR": str(work / "tmp"),
            "npm_config_userconfig": str(work / "home" / ".npmrc"),
            "npm_config_globalconfig": str(work / "home" / "npmrc-global"),
            "npm_config_cache": str(work / "npm-cache"),
        }
        sdk_archive = pack(sdk, work / "build-auth", work / "archives", env, auth_manifest)
        solid_archive = pack(solid, work / "build-solid", work / "archives", env, solid_manifest, sdk_archive)
        archives = {AUTH: sdk_archive, SOLID: solid_archive}
        smoke(work / "consumer", archives, solid_manifest["devDependencies"], env, browser)

        artifacts = []
        if destination is not None:
            destination.mkdir(parents=True, exist_ok=True)
        for archive in archives.values():
            print(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}", file=sys.stderr)
            if destination is not None:
                artifact = destination / archive.name
                shutil.copyfile(archive, artifact)
                artifacts.append(artifact)
        return artifacts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, help="verify a selected clients root; allow a pre-Auth revision")
    parser.add_argument("--pack-destination", type=Path, help="write verified archives here, Auth then Solid")
    parser.add_argument("--browser-executable", type=Path, help="also exercise the build in local Chromium")
    args = parser.parse_args()
    try:
        for artifact in verify(
            args.root.resolve() if args.root else Path(__file__).resolve().parent.parent,
            args.pack_destination.resolve() if args.pack_destination else None,
            args.browser_executable.absolute() if args.browser_executable else None,
            allow_absent=args.root is not None,
        ):
            print(artifact)
    except (ValueError, KeyError, OSError, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit(f"Auth package verification failed: {error}")


if __name__ == "__main__":
    main()
