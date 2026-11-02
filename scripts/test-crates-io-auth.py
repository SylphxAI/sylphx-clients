#!/usr/bin/env python3
"""Release identity contract: only the public repository's exact release tag."""

from pathlib import Path
from types import SimpleNamespace
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[1] / '.github/workflows/release.yml'
TEXT = WORKFLOW.read_text()
CRATES = TEXT.split('\n  crates:\n', 1)[1]
CONDITION = re.search(r'^    if: (.+)$', CRATES, re.M).group(1)


def admitted(repository='SylphxAI/sylphx-clients', ref='refs/tags/v1.2.3',
             event='push', tag=''):
    """Evaluate the workflow's own Boolean expression for identity fixtures."""
    return eval(CONDITION.replace('&&', 'and').replace('||', 'or'),
                {'__builtins__': {}}, {
                    'github': SimpleNamespace(repository=repository, ref=ref,
                                              ref_name=ref.rsplit('/', 1)[-1], event_name=event),
                    'inputs': SimpleNamespace(tag=tag),
                    'startsWith': lambda value, prefix: value.startswith(prefix),
                })


class ReleaseIdentity(unittest.TestCase):
    def test_release_tag_and_exact_tag_recovery(self):
        self.assertTrue(admitted())
        self.assertTrue(admitted(event='workflow_dispatch', tag='v1.2.3'))

    def test_foreign_repository_and_fork_are_denied(self):
        for repository in ('SylphxAI/cloud', 'shtse8/sylphx-clients', 'fork/sylphx-clients'):
            with self.subTest(repository=repository):
                self.assertFalse(admitted(repository=repository))

    def test_branches_and_pull_requests_are_denied(self):
        for ref in ('refs/heads/main', 'refs/heads/v1.2.3', 'refs/pull/1/merge'):
            self.assertFalse(admitted(ref=ref))
        for event in ('pull_request', 'pull_request_target', 'schedule'):
            self.assertFalse(admitted(event=event))

    def test_another_tag_cannot_borrow_the_identity(self):
        self.assertFalse(admitted(event='workflow_dispatch', tag='v9.9.9'))
        self.assertFalse(admitted(ref='refs/tags/other'))

    def test_federated_token_only_and_scoped_permissions(self):
        header = CRATES.split('    steps:\n', 1)[0]
        self.assertIn('    environment: crates-io\n', header)
        self.assertIn('    permissions:\n      contents: read\n      id-token: write\n', header)
        self.assertNotIn('secrets.CARGO_REGISTRY_TOKEN', CRATES)
        self.assertIn('CARGO_REGISTRY_TOKEN: ${{ steps.crates-auth.outputs.token }}', CRATES)
        self.assertIn('uses: rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18', CRATES)
        self.assertLess(CRATES.index('id: crates-auth'),
                        CRATES.index('name: Publish SDK crates before their consumers'))
        # The existing package-order authority and immutable-version skip remain.
        self.assertIn('scripts/check-publish-set.py --list', CRATES)
        self.assertIn('if on_registry "$crate" "$version";', CRATES)
        self.assertIn('cargo publish --locked --registry crates-io -p "$crate"', CRATES)


if __name__ == '__main__':
    unittest.main()
