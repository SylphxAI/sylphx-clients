#!/usr/bin/env python3
"""Release-set selection and registry failure regressions."""

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
from unittest import mock
import urllib.error

spec = importlib.util.spec_from_file_location(
    "check_publish_set", Path(__file__).with_name("check-publish-set.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PublishSetTests(unittest.TestCase):
    def setUp(self):
        self.metadata = {
            "workspace_members": list(module.PUBLISH_ORDER),
            "packages": [
                {"id": name, "name": name, "version": "0.1.0", "publish": None}
                for name in reversed(module.PUBLISH_ORDER)
            ],
        }

    def test_metadata_uses_the_same_complete_order_as_the_publisher(self):
        self.assertEqual(
            module.ordered_packages(self.metadata),
            [(name, "0.1.0") for name in module.PUBLISH_ORDER],
        )
        self.metadata["packages"].append({
            "id": "dependency", "name": "dependency", "version": "1.0.0", "publish": None,
        })
        self.assertEqual(len(module.ordered_packages(self.metadata)), 7)

    def test_missing_or_unlisted_publishable_member_fails(self):
        for name in ("sylphx-cli", "new-client"):
            with self.subTest(name=name):
                metadata = dict(self.metadata)
                metadata["workspace_members"] = [
                    member for member in metadata["workspace_members"] if member != name
                ] + ([name] if name == "new-client" else [])
                if name == "new-client":
                    metadata["packages"] = metadata["packages"] + [{
                        "id": name, "name": name, "version": "0.1.0", "publish": None,
                    }]
                with self.assertRaisesRegex(RuntimeError, "publish order differ"):
                    module.ordered_packages(metadata)

    def run_selection(self, packages, published):
        with mock.patch.object(module, "registry_contains", side_effect=published), \
                mock.patch.object(module.subprocess, "run") as run, \
                contextlib.redirect_stdout(io.StringIO()):
            module.dry_run(packages)
            return run

    def test_stale_registered_lease_is_not_hidden_by_workspace_overlay(self):
        run = self.run_selection(
            [("sylphx-build-lease", "0.1.0"), ("sylphx-cli", "0.29.11")],
            [True, False],
        )
        run.assert_called_once_with(
            ["cargo", "publish", "--dry-run", "--locked", "--package", "sylphx-cli"],
            check=True,
        )

    def test_new_lease_and_consumer_are_verified_together_in_order(self):
        run = self.run_selection(
            [("sylphx-build-lease", "0.1.1"), ("sylphx-cli", "0.29.11")],
            [False, False],
        )
        run.assert_called_once_with([
            "cargo", "publish", "--dry-run", "--locked",
            "--package", "sylphx-build-lease", "--package", "sylphx-cli",
        ], check=True)

    def test_registered_set_does_not_invoke_an_unselected_workspace_publish(self):
        self.run_selection([("sylphx-cli", "0.29.11")], [True]).assert_not_called()

    def test_only_404_is_an_unpublished_version(self):
        for status in (404, 403, 429, 503):
            with self.subTest(status=status):
                error = urllib.error.HTTPError("https://crates.io/fixture", status, "fixture", {}, None)
                with mock.patch.object(module.urllib.request, "urlopen", side_effect=error):
                    if status == 404:
                        self.assertFalse(module.registry_contains("sylphx-build-lease", "0.1.1"))
                    else:
                        with self.assertRaises(urllib.error.HTTPError):
                            module.registry_contains("sylphx-build-lease", "0.1.1")

    def test_failed_package_verification_propagates(self):
        error = module.subprocess.CalledProcessError(101, ["cargo", "publish"])
        with mock.patch.object(module, "registry_contains", return_value=False), \
                mock.patch.object(module.subprocess, "run", side_effect=error):
            with self.assertRaises(module.subprocess.CalledProcessError):
                module.dry_run([("sylphx-cli", "0.29.11")])


if __name__ == "__main__":
    unittest.main()
