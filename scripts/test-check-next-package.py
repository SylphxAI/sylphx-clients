#!/usr/bin/env python3
"""Exercise optional SDK peers with packed dependencies and a missing API."""

import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("check_next_package", Path(__file__).with_name("check-next-package.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class NextPackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="next-package-regression-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        sdk, nxt = self.root / "typescript", self.root / "next"
        (sdk / "dist").mkdir(parents=True)
        (nxt / "src").mkdir(parents=True)
        (self.root / "scripts").mkdir()
        shutil.copyfile(Path(__file__).with_name("smoke-npm.sh"), self.root / "scripts/smoke-npm.sh")
        (sdk / "package.json").write_text(json.dumps({
            "name": "@sylphx/sdk", "version": "1.0.0", "type": "module",
            "exports": {".": "./dist/index.js"}, "files": ["dist"],
        }))
        (sdk / "dist/index.js").write_text("export const continueApi = () => 42;\n")
        self.manifest = json.dumps({
            "name": "next-package-regression", "version": "1.0.0", "type": "module",
            "exports": {".": "./dist/index.js"}, "files": ["dist"],
            "peerDependencies": {"@sylphx/sdk": ">=1.0.0"},
            "peerDependenciesMeta": {"@sylphx/sdk": {"optional": True}},
            "devDependencies": {"@sylphx/sdk": "1.0.0"},
            "scripts": {"typecheck": "node src/index.js", "test": "node src/index.js", "build": "node build.js"},
        }, indent=2) + "\n"
        (nxt / "package.json").write_text(self.manifest)
        (nxt / "src/index.js").write_text(
            "import { continueApi } from '@sylphx/sdk';\n"
            "if (continueApi() !== 42) throw new Error('incompatible SDK API');\n"
            "export const verified = true;\n"
        )
        (nxt / "build.js").write_text(
            "import { mkdirSync, copyFileSync } from 'node:fs';\n"
            "mkdirSync('dist', { recursive: true }); copyFileSync('src/index.js', 'dist/index.js');\n"
        )

    def test_optional_peer_installs_archive_and_ships_original_manifest(self):
        artifact = module.verify(self.root, self.root / "packed")
        with tarfile.open(artifact) as packed:
            self.assertEqual(packed.extractfile("package/package.json").read(), self.manifest.encode())
            self.assertIn("package/dist/index.js", packed.getnames())
        self.assertEqual((self.root / "next/package.json").read_text(), self.manifest)
        self.assertFalse((self.root / "next/node_modules").exists())
        self.assertFalse((self.root / "next/dist").exists())

    def test_incompatible_packed_sdk_fails_before_consumer_artifact(self):
        (self.root / "typescript/dist/index.js").write_text("export const oldApi = true;\n")
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            module.verify(self.root, self.root / "packed")
        self.assertEqual(failure.exception.cmd, ["bun", "run", "typecheck"])
        self.assertFalse((self.root / "packed").exists())
        self.assertEqual((self.root / "next/package.json").read_text(), self.manifest)


if __name__ == "__main__":
    unittest.main()
