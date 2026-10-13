#!/usr/bin/env python3
"""npm readback tolerates observed propagation but keeps one hard deadline."""

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "wait_for_npm_version", Path(__file__).with_name("wait-for-npm-version.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Clock:
    def __init__(self):
        self.seconds = 0
        self.sleeps = []

    def sleep(self, seconds):
        self.seconds += seconds
        self.sleeps.append(seconds)


class NpmReadbackTests(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.monotonic = mock.patch.object(module.time, "monotonic", side_effect=lambda: self.clock.seconds)
        self.sleep = mock.patch.object(module.time, "sleep", side_effect=self.clock.sleep)
        self.monotonic.start()
        self.sleep.start()
        self.addCleanup(self.monotonic.stop)
        self.addCleanup(self.sleep.stop)

    def test_visible_version_returns_without_sleep(self):
        with mock.patch.object(module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "0.29.11\n")) as run:
            module.wait_for_version("@sylphx/cli-darwin-x64", "0.29.11")
        run.assert_called_once_with(
            ["npm", "view", "@sylphx/cli-darwin-x64@0.29.11", "version", "--prefer-online"],
            capture_output=True, text=True, timeout=900,
        )
        self.assertEqual(self.clock.sleeps, [])

    def test_observed_310_second_propagation_does_not_fail_at_five_minutes(self):
        def view(*args, **kwargs):
            if self.clock.seconds >= 310:
                return subprocess.CompletedProcess([], 0, "0.29.11\n")
            return subprocess.CompletedProcess([], 1, "")
        with mock.patch.object(module.subprocess, "run", side_effect=view) as run:
            module.wait_for_version("@sylphx/cli-darwin-x64", "0.29.11")
        self.assertEqual(self.clock.seconds, 310)
        self.assertEqual(run.call_count, 32)
        self.assertEqual(run.call_args.kwargs["timeout"], 590)

    def test_unavailable_version_fails_at_fifteen_minutes(self):
        with mock.patch.object(module.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "")) as run, \
                self.assertRaisesRegex(RuntimeError, "within 900 seconds"):
            module.wait_for_version("@sylphx/cli", "0.29.11")
        self.assertEqual(self.clock.seconds, 900)
        self.assertEqual(run.call_count, 90)

    def test_request_time_is_charged_to_the_same_deadline(self):
        def view(*args, **kwargs):
            self.clock.seconds += 895
            return subprocess.CompletedProcess([], 1, "")
        with mock.patch.object(module.subprocess, "run", side_effect=view) as run, \
                self.assertRaises(RuntimeError):
            module.wait_for_version("@sylphx/cli", "0.29.11")
        self.assertEqual(run.call_count, 1)
        self.assertEqual(self.clock.sleeps, [5])
        self.assertEqual(self.clock.seconds, 900)

    def test_hung_registry_request_cannot_extend_the_bound(self):
        with mock.patch.object(module.subprocess, "run", side_effect=subprocess.TimeoutExpired(["npm"], 900)), \
                self.assertRaisesRegex(RuntimeError, "within 900 seconds"):
            module.wait_for_version("@sylphx/cli", "0.29.11")
        self.assertEqual(self.clock.sleeps, [])

    def test_wrong_version_or_failed_response_is_not_a_readback(self):
        responses = [subprocess.CompletedProcess([], 0, "0.29.10"),
                     subprocess.CompletedProcess([], 1, "0.29.11"),
                     subprocess.CompletedProcess([], 0, "0.29.11")]
        with mock.patch.object(module.subprocess, "run", side_effect=responses):
            module.wait_for_version("@sylphx/cli", "0.29.11")
        self.assertEqual(self.clock.seconds, 20)


if __name__ == "__main__":
    unittest.main()
