#!/usr/bin/env python3
"""Exercise the Homebrew workflow's publication block against local Git."""

import json
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).parents[1] / ".github/workflows/promote.yml"


class HomebrewBranches(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / "tap"
        self.repo.mkdir()
        self.remote = self.root / "remote.git"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.env = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
            "FIXTURE": str(self.root), "V": "1.2.3",
            "GITHUB_RUN_ID": "456", "GITHUB_RUN_ATTEMPT": "1",
        }
        gh = self.bin / "gh"
        gh.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
a=sys.argv[1:]
with (Path(os.environ["FIXTURE"])/"gh.jsonl").open("a") as f: f.write(json.dumps(a)+"\\n")
if a[:2]==["pr","list"]: pass
elif a[:2]==["pr","create"]: print("https://example.com/pull/20")
elif a[:2]==["pr","merge"]: pass
else: raise SystemExit("Unexpected gh call: "+str(a))
''')
        gh.chmod(0o755)
        self.git("init", "--bare", str(self.remote))
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.com")
        (self.repo / "Formula").mkdir()
        self.formula = self.repo / "Formula/sylphx.rb"
        self.formula.write_text("old formula\n")
        self.git("add", "Formula/sylphx.rb")
        self.git("commit", "-m", "Initial")
        self.main = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "main")
        self.git("checkout", "-b", "sylphx-1.2.3")
        (self.repo / "review.txt").write_text("Existing review\n")
        self.git("add", "review.txt")
        self.git("commit", "-m", "Review in progress")
        self.legacy = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("push", "origin", "sylphx-1.2.3")
        self.git("checkout", "main")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, env=self.env,
                              check=True, capture_output=True, text=True, timeout=10)

    def invoke(self, attempt):
        self.git("checkout", "main")
        self.formula.write_text("new formula\n")
        # Run the actual workflow block after release download/formula rendering.
        source = WORKFLOW.read_text()
        source = textwrap.dedent(source[source.index('          branch='):])
        return subprocess.run(["bash", "-euo", "pipefail"], input=source, cwd=self.repo,
                              env={**self.env, "GITHUB_RUN_ATTEMPT": str(attempt)},
                              capture_output=True, text=True, timeout=15)

    def calls(self):
        path = self.root / "gh.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_attempts_publish_separate_refs_and_keep_the_prior_review(self):
        for attempt in (1, 2):
            result = self.invoke(attempt)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            branch = f"sylphx-1.2.3-456-{attempt}"
            self.assertEqual(self.git("branch", "--show-current").stdout.strip(), branch)
            self.assertEqual(self.git("rev-parse", "HEAD^").stdout.strip(), self.main)
            self.assertEqual(self.git("ls-remote", "origin", "refs/heads/" + branch).stdout.split()[0],
                             self.git("rev-parse", "HEAD").stdout.strip())
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/sylphx-1.2.3").stdout.split()[0], self.legacy)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").stdout.split()[0], self.main)
        self.assertEqual(len([c for c in self.calls() if c[:2] == ["pr", "create"]]), 2)
        self.assertEqual(len([c for c in self.calls() if c[:2] == ["pr", "merge"] and "--auto" in c]), 2)

    def test_divergent_destination_is_preserved_without_pr_changes(self):
        branch = "sylphx-1.2.3-456-1"
        self.git("push", "origin", f"{self.legacy}:refs/heads/{branch}")
        result = self.invoke(1)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/" + branch).stdout.split()[0], self.legacy)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
