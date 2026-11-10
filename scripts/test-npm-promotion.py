#!/usr/bin/env python3
"""Regression coverage for promotion, lag, and the actual consumer smoke."""
from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
import zipfile
from unittest.mock import patch

HERE = Path(__file__).parent


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), HERE / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


promotion = load('promote-npm')
monitor = load('check-npm-promotion-lag')


class Promotion(unittest.TestCase):
    def test_published_source_receipt_not_dispatch_control_head(self):
        workflow = (HERE.parent / '.github/workflows/release.yml').read_text()
        producer = textwrap.dedent(workflow.split("python3 - <<'PYTHON'\n", 1)[1].split('          PYTHON', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path, version in [('cli/npm/cli/package.json', '1.2.3'), ('typescript/package.json', '2.3.4')]:
                dest = root / path
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(json.dumps({'version': version}))
            for args in [('init', '-b', 'main'), ('add', '.'), ('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.com', 'commit', '-m', 'Fixture')]:
                subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)
            source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
            env = {**os.environ, 'TAG': 'v1.2.3', 'GITHUB_REPOSITORY': 'SylphxAI/sylphx-clients',
                   'GITHUB_RUN_ID': '123', 'GITHUB_RUN_ATTEMPT': '2', 'RUNNER_TEMP': directory,
                   'GITHUB_SHA': 'b' * 40, 'RELEASE_RUN_ID': '123', 'RELEASE_RUN_ATTEMPT': '2',
                   'GITHUB_OUTPUT': str(root / 'outputs')}
            subprocess.run(['python3', '-I', '-c', producer], cwd=root, env=env, check=True)
            receipt = root / 'production-candidate.json'
            self.assertEqual(json.loads(receipt.read_text())['source'], source)
            with patch.dict(os.environ, env), patch.object(promotion, 'eligible', return_value=True), \
                 patch.object(promotion, 'newest_successful_candidate', return_value=promotion.validate_candidate(json.loads(receipt.read_text()), 123, 2)):
                promotion.select()
                self.assertIn(f'source={source}', (root / 'outputs').read_text())
            data = json.loads(receipt.read_text())
            data['run_attempt'] = '1'
            receipt.write_text(json.dumps(data))
            with patch.dict(os.environ, env), patch.object(promotion, 'eligible') as eligibility:
                with self.assertRaisesRegex(ValueError, 'successful release attempt'):
                    promotion.validate_candidate(data, 123, 2)
                eligibility.assert_not_called()

    def test_workflow_preserves_pending_events_and_smoke_gate(self):
        workflow = (HERE.parent / '.github/workflows/promote.yml').read_text()
        self.assertIn('queue: max', workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("github.event.workflow_run.event == 'workflow_dispatch'", workflow)
        self.assertIn('RELEASE_WORKFLOW_ID: ${{ github.event.workflow_run.workflow_id }}', workflow)
        self.assertIn('GH_TOKEN: ${{ github.token }}', workflow)
        self.assertIn('needs: smoke', workflow)
        self.assertIn("if: needs.smoke.outputs.eligible == 'true'", workflow)
        self.assertIn('needs: [smoke, promote]', workflow)

    def registry_fixture(self, *, next_version='1.10.0', new_status='success', mismatch=False):
        repo = 'SylphxAI/sylphx-clients'
        receipts = {}
        artifacts = []
        runs = {}
        for run_id, cli, event in [(100, '1.9.0', 'push'), (101, '1.10.0', 'workflow_dispatch')]:
            candidate = {'repository': repo, 'run_id': str(run_id), 'run_attempt': '2',
                         'cli': cli, 'sdk': '2.3.4', 'tag': f'v{cli}', 'source': str(run_id % 10) * 40}
            if mismatch and run_id == 101:
                candidate['run_attempt'] = '1'
            content = io.BytesIO()
            with zipfile.ZipFile(content, 'w') as archive:
                archive.writestr('production-candidate.json', json.dumps(candidate))
            receipts[f'repos/{repo}/actions/artifacts/{run_id}/zip'] = content.getvalue()
            artifacts.append({'id': run_id, 'name': 'production-candidate-2', 'expired': False,
                              'size_in_bytes': len(content.getvalue()), 'workflow_run': {'id': run_id}})
            runs[f'repos/{repo}/actions/runs/{run_id}/attempts/2'] = {
                'id': run_id, 'run_attempt': 2, 'workflow_id': 77, 'path': '.github/workflows/release.yml',
                'event': event, 'status': 'completed', 'conclusion': new_status if run_id == 101 else 'success',
                'head_sha': 'f' * 40, 'head_repository': {'full_name': repo}}
        # The newer receipt lives on page two. Unrelated archive names are not
        # Release evidence and legitimately have no workflow_run field here.
        def github(path, *, pages=False, binary=False):
            if path.endswith('artifacts?per_page=100'):
                self.assertTrue(pages)
                return [{'artifacts': [artifacts[0], {'name': 'binary'}]}, {'artifacts': [artifacts[1]]}]
            if binary:
                return receipts[path]
            return runs[path]
        return ({'GITHUB_REPOSITORY': repo, 'RELEASE_WORKFLOW_ID': '77'}, github,
                {'next': next_version, 'latest': '1.8.0'})

    def test_old_completion_selects_newest_successful_receipt_across_pages(self):
        env, github, tags = self.registry_fixture()
        with patch.dict(os.environ, env), patch.object(promotion, 'github', side_effect=github), \
             patch.object(promotion, 'view', return_value=tags):
            candidate = promotion.newest_successful_candidate()
        self.assertEqual((candidate['cli'], candidate['source']), ('1.10.0', '1' * 40))
        # The dispatch control head is deliberately ffff, never the receipt source.
        self.assertNotEqual(candidate['source'], 'f' * 40)

    def test_incomplete_or_unproven_newest_never_falls_back_to_old(self):
        for options in ({'new_status': 'failure'}, {'new_status': None}, {'next_version': '1.11.0'}):
            env, github, tags = self.registry_fixture(**options)
            with patch.dict(os.environ, env), patch.object(promotion, 'github', side_effect=github), \
                 patch.object(promotion, 'view', return_value=tags):
                with self.assertRaisesRegex(ValueError, 'lacks a matching successful'):
                    promotion.newest_successful_candidate()
        env, github, tags = self.registry_fixture(mismatch=True)
        with patch.dict(os.environ, env), patch.object(promotion, 'github', side_effect=github), \
             patch.object(promotion, 'view', return_value=tags):
            with self.assertRaisesRegex(ValueError, 'successful release attempt'):
                promotion.newest_successful_candidate()

    def test_fresh_prewrite_never_overwrites_current_or_newer_latest(self):
        for candidate, current, refuses in [('1.0.1', '1.0.2', True), ('1.0.1', '1.0.1', False),
                                            ('1.9.0', '1.10.0', True)]:
            answers = [{'latest': '1.0.0', 'next': candidate}, {'latest': current, 'next': current}]
            with patch.object(promotion, 'targets', return_value=[('p', candidate)]), \
                 patch.object(promotion, 'view', side_effect=answers), \
                 patch.object(promotion.subprocess, 'run') as write:
                if refuses:
                    with self.assertRaisesRegex(ValueError, 'already current'):
                        promotion.promote(candidate, '2.0.1')
                else:
                    promotion.promote(candidate, '2.0.1')
                write.assert_not_called()

    def test_readback_disagreement_fails_after_single_write(self):
        answers = [{'latest': '1.0.0', 'next': '1.0.1'}, {'latest': '1.0.0', 'next': '1.0.1'},
                   {'latest': '1.0.2', 'next': '1.0.2'}]
        with patch.object(promotion, 'targets', return_value=[('p', '1.0.1')]), \
             patch.object(promotion, 'view', side_effect=answers), \
             patch.object(promotion.subprocess, 'run') as write:
            with self.assertRaisesRegex(RuntimeError, 'readback disagrees'):
                promotion.promote('1.0.1', '2.0.1')
            write.assert_called_once_with(['npm', 'dist-tag', 'add', 'p@1.0.1', 'latest'], check=True)

    def test_monotonic_selection_and_unknown(self):
        for tags, expected in [({'latest': '1.0.0', 'next': '1.0.1'}, True),
                               ({'latest': '1.0.2', 'next': '1.0.2'}, False)]:
            with patch.object(promotion, 'view', return_value=tags):
                self.assertEqual(promotion.eligible([('p', '1.0.1')]), expected)
        with patch.object(promotion, 'view', return_value={'next': '1.0.1'}):
            with self.assertRaises(KeyError):
                promotion.eligible([('p', '1.0.1')])

    def test_superseded_before_write(self):
        with patch.object(promotion, 'eligible', return_value=False), patch.object(promotion.subprocess, 'run') as write:
            with self.assertRaisesRegex(ValueError, 'no writes'):
                promotion.promote('1.0.1', '2.0.1')
            write.assert_not_called()

    def test_cached_readback_then_success_and_timeout(self):
        for converges in (True, False):
            answers = [{'latest': '1.0.0', 'next': '1.0.1'}] * (2 if converges else 14)
            if converges:
                answers += [{'latest': '1.0.1'}]
            with patch.object(promotion, 'targets', return_value=[('p', '1.0.1')]), \
                 patch.object(promotion, 'eligible', return_value=True), \
                 patch.object(promotion, 'view', side_effect=answers), \
                 patch.object(promotion.subprocess, 'run') as write, \
                 patch.object(promotion.time, 'sleep'):
                if converges:
                    promotion.promote('1.0.1', '2.0.1')
                else:
                    with self.assertRaisesRegex(RuntimeError, 'after 60 s'):
                        promotion.promote('1.0.1', '2.0.1')
                write.assert_called_once_with(['npm', 'dist-tag', 'add', 'p@1.0.1', 'latest'], check=True)


class Lag(unittest.TestCase):
    def data(self):
        return {'dist-tags': {'latest': '1.0.0', 'next': '1.0.1'},
                'versions': {'1.0.0': {}, '1.0.1': {}, '1.1.0-rc.1': {}},
                'time': {'1.0.1': '2026-11-01T00:00:00Z'}}

    def test_one_release_and_seven_day_boundaries(self):
        self.assertEqual(monitor.lag(self.data(), datetime(2026, 11, 8, tzinfo=timezone.utc)), (1, 7))
        self.assertGreater(monitor.lag(self.data(), datetime(2026, 11, 9, tzinfo=timezone.utc))[1], 7)

    def test_counts_all_stable_releases_and_oldest_gap(self):
        data = self.data()
        data['dist-tags']['next'] = '1.0.2'
        data['versions']['1.0.2'] = {}
        data['time']['1.0.2'] = '2026-11-07T00:00:00Z'
        self.assertEqual(monitor.lag(data, datetime(2026, 11, 9, tzinfo=timezone.utc)), (2, 8))

    def test_backwards_channel_is_unknown(self):
        data = self.data()
        data['dist-tags'] = {'latest': '1.0.1', 'next': '1.0.0'}
        with self.assertRaisesRegex(ValueError, 'inconsistent'):
            monitor.lag(data, datetime.now(timezone.utc))

    def test_unknown_is_not_zero(self):
        for field in ('dist-tags', 'versions', 'time'):
            data = self.data()
            del data[field]
            with self.assertRaises(KeyError):
                monitor.lag(data, datetime.now(timezone.utc))


class Smoke(unittest.TestCase):
    def test_actual_script_positive_and_negative_consumers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'package.json').write_text('{"private":true}')
            binary = root / 'node_modules/.bin/sylphx'
            binary.parent.mkdir(parents=True)
            binary.write_text('''#!/usr/bin/env python3
import json, os, sys
if sys.argv[1:] == ['--version']: print('sylphx ' + os.environ.get('ACTUAL_VERSION', '1.2.3'))
elif 'whoami' in sys.argv:
 print(os.environ.get('CLI_ERROR', 'error: UNAUTHENTICATED (401): missing_authorization'), file=sys.stderr)
 sys.exit(1)
else: print(json.dumps({'info': {'title': 'Sylphx API'}, 'paths': {'/v1/whoami': {}}, 'description': 'x' * (2 * 1024 * 1024)}))
''')
            binary.chmod(0o755)
            sdk = root / 'node_modules/@sylphx/sdk'
            sdk.mkdir(parents=True)
            (sdk / 'package.json').write_text(json.dumps({'version': '2.3.4', 'exports': {'.': './index.cjs'}}))
            (sdk / 'index.cjs').write_text('''class SylphxError extends Error { constructor(){super(); this.status=401; this.code='UNAUTHENTICATED'} }
module.exports={SylphxError,Sylphx: class { access={whoami:async()=>{if(!process.env.ACCEPT_ANONYMOUS) throw new SylphxError()}} }}
''')
            env = {**os.environ, 'CLI_VERSION': '1.2.3', 'SDK_VERSION': '2.3.4'}
            for extra, passes in [({}, True), ({'ACTUAL_VERSION': '1.2.2'}, False),
                                   ({'CLI_ERROR': 'not signed in'}, False), ({'ACCEPT_ANONYMOUS': '1'}, False)]:
                result = subprocess.run(['node', str(HERE.resolve() / 'production-smoke.mjs')], cwd=root,
                                        env={**env, **extra}, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode == 0, passes, result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
