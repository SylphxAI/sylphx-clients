#!/usr/bin/env python3
"""Regression tests for the package archive smoke test."""
from pathlib import Path
import json
import os
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent
SMOKE = ROOT / "smoke-npm.sh"


def package(directory: Path, manifest: dict, files: dict[str, str]) -> None:
    directory.mkdir(parents=True)
    (directory / "package.json").write_text(json.dumps(manifest) + "\n")
    for name, contents in files.items():
        path = directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)


def run_smoke(directory: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(SMOKE), str(directory)],
        cwd=directory,
        text=True,
        capture_output=True,
        env={**os.environ, "npm_config_cache": str(directory / "npm-cache")},
    )


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="sylphx-smoke-tests-") as temporary:
        root = Path(temporary)
        peer = root / "peer"
        package(peer, {
            "name": "smoke-optional-peer", "version": "1.0.0", "type": "module",
            "exports": {".": "./index.js"}, "files": ["index.js"],
        }, {"index.js": "export const optional = true;\n"})
        packed = subprocess.run(
            ["npm", "pack", "--json", "--pack-destination", str(root)],
            cwd=peer, check=True, capture_output=True, text=True,
        )
        peer_archive = root / json.loads(packed.stdout)[0]["filename"]
        healthy = root / "healthy"
        package(healthy, {
            "name": "smoke-healthy", "version": "1.0.0", "type": "module",
            "exports": {".": "./index.js", "./*": "./lib/*.js", "./style.css": "./style.css"},
            "files": ["index.js", "lib", "style.css"],
            "peerDependencies": {"smoke-optional-peer": "file:" + str(peer_archive)},
            "peerDependenciesMeta": {"smoke-optional-peer": {"optional": True}},
        }, {
            "index.js": "export const server = true;\n",
            "lib/server.js": "export const server = true;\n",
            "lib/ui.js": "import { optional } from 'smoke-optional-peer'; export { optional };\n",
            "style.css": "body { color: black; }\n",
        })
        result = run_smoke(healthy)
        if result.returncode != 0:
            raise SystemExit("healthy package smoke failed:\n" + result.stdout + result.stderr)
        for expected in (
            "esm ok  smoke-healthy", "cjs ok  smoke-healthy/server",
            "expected optional peer miss  smoke-healthy/ui", "asset ok smoke-healthy/style.css",
            "esm ok  smoke-healthy/ui", "cjs ok  smoke-healthy/ui",
        ):
            if expected not in result.stdout:
                raise SystemExit("healthy smoke did not exercise " + expected + "\n" + result.stdout)

        broken = root / "broken"
        package(
            broken,
            {
                "name": "smoke-broken-js",
                "version": "1.0.0",
                "type": "module",
                "exports": {".": "./index.js", "./broken": "./broken.js"},
                "files": ["*.js"],
            },
            {
                "index.js": "export const healthy = true;\n",
                "broken.js": "import 'smoke-missing-runtime';\n",
            },
        )
        result = run_smoke(broken)
        if result.returncode == 0:
            raise SystemExit("smoke unexpectedly accepted a broken JavaScript export")
        if "smoke-broken-js/broken" not in result.stdout + result.stderr:
            raise SystemExit("broken JavaScript export was not exercised")

        for reverse in (False, True):
            mixed = root / ("mixed-reverse" if reverse else "mixed")
            exports = {"import": "./broken.cjs", "require": "./style.css"} if reverse else {
                "import": "./style.css", "require": "./broken.cjs",
            }
            package(mixed, {
                "name": "smoke-mixed", "version": "1.0.0",
                "exports": {".": exports}, "files": ["style.css", "broken.cjs"],
            }, {"style.css": "body {}\n", "broken.cjs": "throw new Error('BROKEN CONDITIONAL ENTRY');\n"})
            result = run_smoke(mixed)
            if result.returncode == 0 or "BROKEN CONDITIONAL ENTRY" not in result.stderr + result.stdout:
                raise SystemExit("mixed asset/JavaScript conditional export was not executed:\n" + result.stdout + result.stderr)

        subpath = root / "subpath"
        package(subpath, {
            "name": "smoke-subpath", "version": "1.0.0",
            "exports": {".": "./index.cjs", "./ui": "./ui.cjs"},
            "files": ["*.cjs"],
            "peerDependencies": {"smoke-optional-peer": "file:" + str(peer_archive)},
            "peerDependenciesMeta": {"smoke-optional-peer": {"optional": True}},
        }, {"index.cjs": "module.exports = true;\n",
            "ui.cjs": "module.exports = require('smoke-optional-peer/runtime');\n"})
        # Supply a peer with a real subpath, then re-pack its exact archive.
        manifest = json.loads((peer / "package.json").read_text())
        manifest["exports"]["./runtime"] = "./index.js"
        (peer / "package.json").write_text(json.dumps(manifest))
        subprocess.run(["npm", "pack", "--json", "--pack-destination", str(root)],
                       cwd=peer, check=True, capture_output=True, text=True)
        result = run_smoke(subpath)
        if result.returncode != 0 or "expected optional peer miss  smoke-subpath/ui" not in result.stdout:
            raise SystemExit("optional peer subpath smoke failed:\n" + result.stdout + result.stderr)
        # A similar prefix is not the declared optional peer.
        (subpath / "ui.cjs").write_text("require('smoke-optional-peer-unrelated/runtime');\n")
        result = run_smoke(subpath)
        if result.returncode == 0 or "expected optional peer miss" in result.stdout:
            raise SystemExit("unrelated package was mistaken for an optional peer")

        missing = root / "missing"
        package(
            missing,
            {
                "name": "smoke-missing-target",
                "version": "1.0.0",
                "type": "module",
                "exports": {".": "./index.js", "./missing": "./missing.js"},
                "files": ["index.js"],
            },
            {"index.js": "export const healthy = true;\n"},
        )
        result = run_smoke(missing)
        if result.returncode == 0:
            raise SystemExit("smoke unexpectedly accepted an export target absent from the tarball")
        if "export target missing or empty in tarball" not in result.stderr:
            raise SystemExit("missing export target was not reported")

    print("smoke-npm regression tests passed")


if __name__ == "__main__":
    main()
