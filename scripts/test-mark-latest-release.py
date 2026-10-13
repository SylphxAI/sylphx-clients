#!/usr/bin/env python3
"""Latest selection stays numeric, stable-only, idempotent and fail-closed."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "mark_latest_release", Path(__file__).with_name("mark-latest-release.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def release(tag, **fields):
    return {"tag_name": tag, "draft": False, "prerelease": False, **fields}


class LatestReleaseTests(unittest.TestCase):
    def run_selection(self, tag, pages, latest=None):
        responses = [subprocess.CompletedProcess([], 0, json.dumps(pages))]
        if latest is not None:
            responses += [subprocess.CompletedProcess([], 0), subprocess.CompletedProcess([], 0, latest)]
        with mock.patch.object(module.subprocess, "run", side_effect=responses) as run, \
                contextlib.redirect_stdout(io.StringIO()):
            module.mark_latest("SylphxAI/sylphx-clients", tag)
            return run

    def test_newest_stable_is_selected_across_pages_in_numeric_order(self):
        run = self.run_selection("v0.29.11", [
            [release("v0.29.9"), release("v0.29.11")],
            [release("v0.25.1"), release("v0.29.10")],
        ], "v0.29.11\n")
        self.assertEqual(run.call_count, 3)
        self.assertEqual(run.call_args_list[1], mock.call(
            ["gh", "release", "edit", "v0.29.11", "--repo", "SylphxAI/sylphx-clients", "--latest"], check=True,
        ))

    def test_old_release_rerun_does_not_downgrade_latest(self):
        run = self.run_selection("v0.29.10", [[release("v0.29.10"), release("v0.29.11")]])
        self.assertEqual(run.call_count, 1)

    def test_prerelease_never_marks_latest(self):
        with mock.patch.object(module.subprocess, "run") as run, \
                contextlib.redirect_stdout(io.StringIO()):
            module.mark_latest("SylphxAI/sylphx-clients", "v0.30.0-rc.1")
        run.assert_not_called()

    def test_repair_suffix_is_never_stable(self):
        # A repair-suffixed tag is not a stable release: as the current tag it
        # must not touch Latest, and in the release list it must not hide the
        # newest stable release.
        with mock.patch.object(module.subprocess, "run") as run, \
                contextlib.redirect_stdout(io.StringIO()):
            module.mark_latest("SylphxAI/sylphx-clients", "v0.29.11-repair1")
        run.assert_not_called()
        run = self.run_selection("v0.29.11", [[
            release("v0.29.11"), release("v0.30.0-repair1"),
        ]], "v0.29.11")
        self.assertEqual(run.call_count, 3)

    def test_drafts_prereleases_and_unrelated_tags_do_not_hide_newest_stable(self):
        self.run_selection("v0.29.11", [[
            release("v0.29.11"), release("v1.0.0", draft=True),
            release("v2.0.0", prerelease=True), release("clients-v9.0.0"),
        ]], "v0.29.11")

    def test_target_must_already_be_a_published_stable_release(self):
        with self.assertRaisesRegex(RuntimeError, "not a published stable Release"):
            self.run_selection("v0.29.11", [[release("v0.29.10")]])

    def test_latest_readback_mismatch_fails(self):
        with self.assertRaisesRegex(RuntimeError, "readback does not name"):
            self.run_selection("v0.29.11", [[release("v0.29.11")]], "v0.25.1")

    def test_read_and_write_failures_are_not_suppressed(self):
        for successful_reads in (0, 1, 2):
            with self.subTest(successful_reads=successful_reads):
                responses = [subprocess.CompletedProcess([], 0, json.dumps([[release("v0.29.11")]]))]
                responses += [subprocess.CompletedProcess([], 0)]
                responses = responses[:successful_reads] + [subprocess.CalledProcessError(1, ["gh"])]
                with mock.patch.object(module.subprocess, "run", side_effect=responses), \
                        self.assertRaises(subprocess.CalledProcessError):
                    module.mark_latest("SylphxAI/sylphx-clients", "v0.29.11")

class WorkflowControlTests(unittest.TestCase):
    """The workflow must queue publications and load control scripts from the
    workflow revision, so historical-tag reruns work and cannot cancel the
    newest tag's pending publication."""

    workflow = (
        Path(__file__).parents[1] / ".github" / "workflows" / "release.yml"
    ).read_text()

    def test_github_release_publications_are_queued_not_replaced(self):
        self.assertIn("queue: max", self.workflow)
        self.assertIn("cancel-in-progress: false", self.workflow)

    def test_control_scripts_come_from_workflow_revision_not_tag(self):
        # Every job that runs release-control scripts (check, github-release
        # and npm) checks the workflow revision out beside the frozen tag
        # sources.
        self.assertEqual(self.workflow.count("ref: ${{ github.sha }}"), 3)
        self.assertEqual(self.workflow.count("path: .release-control"), 3)
        for script in (
            "test-check-publish-set.py",
            "test-mark-latest-release.py",
            "test-wait-for-npm-version.py",
            "check-publish-set.py",
            "mark-latest-release.py",
            "wait-for-npm-version.py",
        ):
            self.assertIn(f".release-control/scripts/{script}", self.workflow)
        # The check, github-release and npm invocations must not fall back to
        # the tag tree. (The crates job deliberately keeps the tag's own
        # check-publish-set.py: it enumerates that tag's workspace.)
        for script in (
            "test-check-publish-set.py",
            "test-mark-latest-release.py",
            "mark-latest-release.py",
            "wait-for-npm-version.py",
        ):
            self.assertNotIn(f"python3 -I scripts/{script}", self.workflow)

if __name__ == "__main__":
    unittest.main()
