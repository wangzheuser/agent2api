"""基线解析与隔离断言重放自检；使用临时 Git 样本，不改当前工作区。"""
import importlib.util
import difflib
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('replay', Path(__file__).with_name('replay-merge-baseline.py'))
replay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replay)


class ReplayBaselineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'repo'
        self.root.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.name', 'fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'core.autocrlf', 'false')
        self.write('impl.cjs', 'module.exports = 7;\n')
        self.write('scripts/assert.test.cjs', "const {test}=require('node:test'); const assert=require('node:assert/strict'); const sample=require('./fixtures/value.cjs'); test('original assertion',{skip:require('../impl.cjs')===0},()=>assert.equal(require('../impl.cjs'),sample));\n")
        self.write('scripts/fixtures/value.cjs', 'module.exports = 7;\n')
        self.write(replay.RUST_TESTS + '/first.rs', '#[test]\nfn first() { assert_eq!(1, 1); }\n')
        self.write(replay.RUST_TESTS + '/second.rs', '#[test]\nfn second() { assert_eq!(2, 2); }\n')
        self.write(replay.RUST_TESTS + '/fixtures/one.sql', 'SELECT 1;\n')
        self.write(replay.RUST_TESTS + '/fixtures/nested/two.sql', 'SELECT 2;\n')
        self.manifest = {'contracts': [{'implementation': ['impl.cjs'], 'tests': [
            {'kind': 'node', 'path': 'scripts/assert.test.cjs', 'name': 'original assertion'},
            {'kind': 'rust', 'path': replay.RUST_TESTS + '/first.rs', 'name': 'first'},
            {'kind': 'rust', 'path': replay.RUST_TESTS + '/second.rs', 'name': 'second'},
        ]}]}
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        self.checker = '''import copy,pathlib,sys
def baseline_view(baseline,current):
 result=copy.deepcopy(baseline)
 for old,new in zip(result['contracts'],current['contracts']):
  old['implementation']=new['implementation']
 return result
def protected_report(text):
 return 'ok 1 - original assertion' in text.splitlines()
if __name__=='__main__' and '--node-report' in sys.argv:
 assert protected_report(pathlib.Path(sys.argv[sys.argv.index('--node-report')+1]).read_text())
'''
        self.write(replay.CHECKER, self.checker)
        self.write(replay.SELF_TEST, '''import importlib.util,pathlib,unittest
spec=importlib.util.spec_from_file_location('gate',pathlib.Path(__file__).with_name('check-merge-contracts.py'))
gate=importlib.util.module_from_spec(spec); spec.loader.exec_module(gate)
class FrozenGate(unittest.TestCase):
 def test_original_negative(self): self.assertFalse(gate.protected_report('empty'))
''')
        self.git('add', '.')
        self.git('commit', '-qm', 'baseline fixture')
        self.base = self.git('rev-parse', 'HEAD').strip()
        self.write('impl.cjs', 'module.exports = 8;\n')
        self.git('commit', '-qam', 'candidate fixture')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True)

    def write(self, name, source):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding='utf-8')

    def test_pr_push_manual_and_new_branch_freeze_same_expected_sha(self):
        for name, event, explicit in [('pull_request', {'pull_request': {'base': {'sha': self.base}}}, ''),
                                      ('push', {'before': self.base}, ''),
                                      ('push', {'before': '0' * 40}, ''),
                                      ('workflow_dispatch', {}, self.base),
                                      ('workflow_dispatch', {}, '')]:
            with self.subTest(name=name, event=event):
                self.assertEqual(replay.resolve_base(self.root, name, event, explicit), self.base)

    def test_head_nonancestor_and_shallow_fail_closed(self):
        with self.assertRaisesRegex(ValueError, 'HEAD'):
            replay.resolve_base(self.root, 'workflow_dispatch', {}, 'HEAD')
        other = self.git('commit-tree', 'HEAD^{tree}', '-m', 'unrelated root').strip()
        with self.assertRaises(subprocess.CalledProcessError):
            replay.resolve_base(self.root, 'push', {'before': other})
        with patch.object(replay, 'git', return_value=b'true\n'):
            with self.assertRaisesRegex(ValueError, '完整历史'):
                replay.resolve_base(self.root, 'push', {'before': self.base})

    def test_snapshot_excludes_runtime_and_preserves_untracked_test_inputs(self):
        self.write('.env', 'fixture-secret')
        self.write('diagnostic-artifacts/private.txt', 'fixture-private')
        self.git('add', '.env', 'diagnostic-artifacts/private.txt')
        self.write(replay.RUST_TESTS + '/fixtures/untracked.sql', 'SELECT 3;')
        target = Path(self.temp.name) / 'snapshot'
        replay.snapshot(self.root, target, self.manifest)
        self.assertFalse((target / '.env').exists())
        self.assertFalse((target / 'diagnostic-artifacts/private.txt').exists())
        self.assertEqual((target / 'impl.cjs').read_bytes(), (self.root / 'impl.cjs').read_bytes())
        self.assertTrue((target / replay.RUST_TESTS / 'fixtures/untracked.sql').is_file())
        self.assertFalse((target / '.git').exists())

    def test_all_rust_targets_and_nested_fixtures_keep_original_paths(self):
        self.write(replay.RUST_TESTS + '/fixtures/nested/two.sql', 'SELECT 99;')
        target = Path(self.temp.name) / 'rust'
        calls = []
        with patch.object(replay, 'run', side_effect=lambda cmd, cwd, report=None: calls.append(cmd)):
            replay.replay(self.root, self.base, 'rust', target, Path(self.temp.name) / 'rust.txt')
        self.assertNotIn('--lib', calls[0])
        self.assertIn('first', calls[0])
        self.assertIn('second', calls[0])
        self.assertEqual(calls[0][-2:], ['--', '--list'])
        self.assertIn('first', calls[1])
        self.assertIn('second', calls[1])
        self.assertEqual((target / replay.RUST_TESTS / 'fixtures/nested/two.sql').read_text(), 'SELECT 2;\n')
        self.assertEqual((self.root / replay.RUST_TESTS / 'fixtures/nested/two.sql').read_text(), 'SELECT 99;')

    def test_unmapped_h_rust_target_runs_but_candidate_only_target_is_not_h(self):
        self.write(replay.RUST_TESTS + '/unmapped_h.rs', '#[test]\nfn unmapped_h() { assert_eq!(3, 3); }\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'unmapped H integration target')
        ref = self.git('rev-parse', 'HEAD').strip()
        self.write(replay.RUST_TESTS + '/candidate_only.rs', '#[test]\nfn candidate_only() { assert_eq!(4, 4); }\n')
        target = Path(self.temp.name) / 'all-h-targets'
        calls = []
        with patch.object(replay, 'run', side_effect=lambda cmd, cwd, report=None: calls.append(cmd)):
            replay.replay(self.root, ref, 'rust', target, Path(self.temp.name) / 'all-h.txt')
        for command in calls[:2]:
            self.assertIn('unmapped_h', command)
            self.assertNotIn('candidate_only', command)
        view = json.loads((target / replay.MANIFEST).read_text())
        self.assertEqual(view['baseline_rust_targets'], ['first', 'second', 'unmapped_h'])
        self.assertTrue((target / replay.RUST_TESTS / 'candidate_only.rs').is_file())

    def test_snapshot_links_installed_modules_without_copying_or_mutating_them(self):
        self.write('desktop-tauri/ui-islands/node_modules/sentinel', 'installed-dependency')
        target = Path(self.temp.name) / 'linked'
        replay.snapshot(self.root, target, self.manifest)
        modules = target / 'desktop-tauri/ui-islands/node_modules'
        self.assertTrue(modules.is_symlink())
        self.assertEqual((modules / 'sentinel').read_text(), 'installed-dependency')
        self.assertEqual((target / replay.MANIFEST).read_bytes(), (self.root / replay.MANIFEST).read_bytes())

    def test_original_node_assertion_catches_weakened_candidate_without_mutation(self):
        weakened = "const {test}=require('node:test'); test('original assertion',()=>{});\n"
        self.write('scripts/assert.test.cjs', weakened)
        with self.assertRaises(subprocess.CalledProcessError):
            replay.replay(self.root, self.base, 'frontend', Path(self.temp.name) / 'node', Path(self.temp.name) / 'node.txt')
        self.assertEqual((self.root / 'scripts/assert.test.cjs').read_text(), weakened)
        self.write('impl.cjs', 'module.exports = 7;\n')
        replay.replay(self.root, self.base, 'frontend', Path(self.temp.name) / 'restored', Path(self.temp.name) / 'restored.txt')

    def test_original_skip_cannot_be_masked_by_candidate_pass_report(self):
        self.write('impl.cjs', 'module.exports = 0;\n')
        report = Path(self.temp.name) / 'isolated-report.txt'
        report.write_text('ok 1 - original assertion\n', encoding='utf-8')
        candidate = Path(self.temp.name) / 'skip-candidate'
        with self.assertRaises(subprocess.CalledProcessError):
            replay.replay(self.root, self.base, 'frontend', candidate, report)
        self.assertNotIn('ok 1 - original assertion', report.read_text().splitlines())
        self.assertIn('# SKIP', report.read_text())
        self.assertEqual(json.loads(report.with_name(report.name + '.original.status.json').read_text())['exit'], 1)
        with self.assertRaises(subprocess.CalledProcessError):
            replay.frozen_gate(self.root, self.base, Path(self.temp.name) / 'skip-gate', ['--node-report', str(report)], verification_root=candidate)

    def test_frontend_overlay_does_not_restore_candidate_implementation_in_test_parent(self):
        self.write('scripts/implementation.cjs', 'module.exports = 7;\n')
        self.manifest['contracts'][0]['implementation'].append('scripts/implementation.cjs')
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        self.git('add', '.')
        self.git('commit', '-qm', 'baseline script implementation')
        ref = self.git('rev-parse', 'HEAD').strip()
        self.write('scripts/implementation.cjs', 'module.exports = 8;\n')
        target = Path(self.temp.name) / 'preserved-implementation'
        with patch.object(replay, 'run'):
            replay.replay(self.root, ref, 'frontend', target, Path(self.temp.name) / 'unused.txt')
        self.assertEqual((target / 'scripts/implementation.cjs').read_text(), 'module.exports = 8;\n')

    def test_h_original_negative_runs_against_current_checker_and_reports(self):
        weakened = self.checker.replace("return 'ok 1 - original assertion' in text.splitlines()", 'return True')
        self.write(replay.CHECKER, weakened)
        report = Path(self.temp.name) / 'report.txt'
        report.write_text('ok 1 - original assertion\n', encoding='utf-8')
        with self.assertRaises(subprocess.CalledProcessError):
            replay.frozen_gate(self.root, self.base, Path(self.temp.name) / 'frozen-fail', ['--node-report', str(report)])
        self.assertEqual((self.root / replay.CHECKER).read_text(), weakened)
        self.write(replay.CHECKER, self.checker)
        report.write_text('empty\n', encoding='utf-8')
        with self.assertRaises(subprocess.CalledProcessError):
            replay.frozen_gate(self.root, self.base, Path(self.temp.name) / 'report-fail', ['--node-report', str(report)])
        report.write_text('ok 1 - original assertion\n', encoding='utf-8')
        replay.frozen_gate(self.root, self.base, Path(self.temp.name) / 'frozen-pass', ['--node-report', str(report)], verification_root=self.root)
        self.assertEqual((self.root / replay.CHECKER).read_text(), self.checker)

    def wiring_patch(self, value=7):
        original = (self.root / 'scripts/assert.test.cjs').read_text()
        adapted = original.replace("require('../impl.cjs')", "require('../new-api.cjs')")
        self.write('new-api.cjs', f'module.exports = {value};\n')
        (self.root / 'impl.cjs').unlink()
        self.manifest['contracts'][0]['implementation'] = ['new-api.cjs']
        self.write('docs/replay-api.patch', ''.join(difflib.unified_diff(original.splitlines(True), adapted.splitlines(True), fromfile='a/scripts/assert.test.cjs', tofile='b/scripts/assert.test.cjs')))
        declaration = {'path': 'docs/replay-api.patch', 'files': [{'path': 'scripts/assert.test.cjs',
                       'sha256': hashlib.sha256(replay.git(self.root, 'show', self.base + ':scripts/assert.test.cjs')).hexdigest()}],
                       'reason': 'API import 接线改名，保持原断言', 'review': 'fixture reviewed import-only patch'}
        self.manifest['replay_patches'] = [declaration]
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        return declaration

    def test_declared_api_wiring_patch_retains_original_failure_and_adapted_pass(self):
        self.wiring_patch()
        sources = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file() and '.git' not in p.parts}
        report = Path(self.temp.name) / 'adapted.txt'
        target = Path(self.temp.name) / 'adapted-candidate'
        replay.replay(self.root, self.base, 'frontend', target, report)
        self.assertIn('MODULE_NOT_FOUND', report.with_name(report.name + '.original').read_text())
        self.assertEqual(json.loads(report.with_name(report.name + '.original.status.json').read_text())['exit'], 1)
        self.assertIn('ok 1 - original assertion', report.read_text())
        status = json.loads(report.with_name(report.name + '.adapted.status.json').read_text())
        self.assertEqual((status['label'], status['exit']), ('ADAPTED_REPLAY', 0))
        self.assertEqual(sources, {p: p.read_bytes() for p in sources})
        self.assertIn("require('../new-api.cjs')", (target / 'scripts/assert.test.cjs').read_text())

    def test_api_wiring_patch_still_rejects_underlying_wrong_behavior(self):
        self.wiring_patch(value=8)
        report = Path(self.temp.name) / 'wrong.txt'
        with self.assertRaises(subprocess.CalledProcessError):
            replay.replay(self.root, self.base, 'frontend', Path(self.temp.name) / 'wrong-candidate', report)
        self.assertIn('MODULE_NOT_FOUND', report.with_name(report.name + '.original').read_text())
        self.assertIn('8 !== 7', report.with_name(report.name + '.adapted').read_text())
        self.assertEqual(json.loads(report.with_name(report.name + '.adapted.status.json').read_text())['exit'], 1)

    def test_replay_patch_rejects_production_hash_and_undeclared_paths(self):
        declaration = self.wiring_patch()
        for invalid, message in [
            ({**declaration, 'files': [{'path': 'impl.cjs', 'sha256': '0' * 64}]}, '生产/样本'),
            ({**declaration, 'files': [{'path': 'scripts/assert.test.cjs', 'sha256': '0' * 64}]}, 'SHA256'),
        ]:
            with self.subTest(message=message):
                self.manifest['replay_patches'] = [invalid]
                self.write(replay.MANIFEST, json.dumps(self.manifest))
                report = Path(self.temp.name) / (message.replace('/', '-') + '.txt')
                with self.assertRaisesRegex(ValueError, message):
                    replay.replay(self.root, self.base, 'frontend', Path(self.temp.name) / message.replace('/', '-'), report)
                self.assertEqual(json.loads(report.with_name(report.name + '.original.status.json').read_text())['exit'], 1)
        self.manifest['replay_patches'] = [declaration]
        self.write('docs/replay-api.patch', (self.root / 'docs/replay-api.patch').read_text().replace('scripts/assert.test.cjs', 'scripts/other.test.cjs'))
        with self.assertRaisesRegex(ValueError, '实际路径'):
            replay.replay_patches(self.root, self.base, json.loads(replay.git(self.root, 'show', self.base + ':' + replay.MANIFEST)), self.manifest, 'frontend')

    def test_inline_independent_and_retired_domain_obligations_stay_separate(self):
        view = {'contracts': [{'tests': [
            {'kind': 'rust', 'path': 'inline.rs', 'baseline_inline': True},
            {'kind': 'rust', 'path': 'standalone.rs'},
            {'kind': 'rust', 'path': 'both.rs', 'baseline_inline': True, 'baseline_independent': True},
            {'kind': 'node', 'path': 'node.cjs'},
        ]}], 'audit_retired': ['retired']}
        self.assertEqual([t['path'] for t in replay.domain_view(view, 'rust')['contracts'][0]['tests']], ['standalone.rs', 'both.rs'])
        self.assertEqual([t['path'] for t in replay.domain_view(view, 'rust', inline=True)['contracts'][0]['tests']], ['inline.rs', 'both.rs'])
        empty = replay.domain_view({'contracts': [], 'audit_retired': ['retired']}, 'frontend')
        self.assertEqual(empty, {'contracts': [], 'audit_retired': ['retired']})

    def test_candidate_does_not_require_retired_audit_paths(self):
        self.write('impl.cjs', 'module.exports = 7;\n')
        self.manifest['contracts'].append({'status': '已批准替代/移除', 'implementation': ['retired/missing.cjs'], 'tests': [
            {'kind': 'node', 'path': 'retired/missing-test.cjs', 'name': 'old test'},
            {'kind': 'browser', 'path': 'retired/missing-browser.cjs'},
        ]})
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        replay.candidate_tests(self.root, 'node', Path(self.temp.name) / 'active.txt')
        replay.candidate_tests(self.root, 'browser')
        replay.snapshot(self.root, Path(self.temp.name) / 'retired-snapshot', self.manifest)

    def test_browser_only_domain_does_not_pass_empty_node_report(self):
        self.write('future/browser.cjs', "require('node:assert/strict').equal(7,7);\n")
        self.manifest['contracts'][0]['tests'] = [{'kind': 'browser', 'path': 'future/browser.cjs'}]
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        self.git('add', '.')
        self.git('commit', '-qm', 'browser-only baseline')
        ref = self.git('rev-parse', 'HEAD').strip()
        argv = ['replay', 'frontend', '--root', str(self.root), '--base-ref', ref, '--report', str(Path(self.temp.name) / 'browser.txt')]
        with patch.object(replay.sys, 'argv', argv), patch.object(replay, 'frozen_gate') as gate:
            replay.main()
        self.assertEqual(gate.call_args.args[3], [])
        self.assertTrue((Path(self.temp.name) / 'browser.txt.original.browser').is_file())

    def test_cli_relative_report_resolves_in_caller_not_different_root(self):
        self.write('impl.cjs', 'module.exports = 7;\n')
        caller = Path(self.temp.name) / 'caller'
        caller.mkdir()
        command = [sys.executable, '-B', str(Path(replay.__file__).resolve()), 'frontend', '--root', str(self.root),
                   '--base-ref', self.base, '--report', 'relative.txt']
        result = subprocess.run(command, cwd=caller, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('ok 1 - original assertion', (caller / 'relative.txt').read_text())
        self.assertEqual(json.loads((caller / 'relative.txt.original.status.json').read_text())['exit'], 0)
        self.assertFalse((self.root / 'relative.txt').exists())

    def test_candidate_discovers_future_paths_and_existing_globs_without_source_mutation(self):
        self.write('impl.cjs', 'module.exports = 7;\n')
        self.write('future/provider/new-check.cjs', "const {test}=require('node:test'); test('future path',()=>require('node:assert/strict').equal(1,1));\n")
        self.write('scripts/unmapped.test.cjs', "require('node:test')('legacy script glob',()=>{});\n")
        self.write('desktop-tauri/ui-islands/tests/unmapped.test.mjs', "import {test} from 'node:test'; test('legacy UI glob',()=>{});\n")
        self.write('future/provider/new-browser.cjs', "require('node:fs').writeFileSync(require('node:path').join(__dirname,'browser-ran.txt'),'future browser executed');\n")
        self.manifest['contracts'][0]['tests'].extend([
            {'kind': 'node', 'path': 'future/provider/new-check.cjs', 'name': 'future path'},
            {'kind': 'browser', 'path': 'future/provider/new-browser.cjs'},
        ])
        self.write(replay.MANIFEST, json.dumps(self.manifest))
        sources = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file() and '.git' not in p.parts}
        report = Path(self.temp.name) / 'candidate.txt'
        replay.candidate_tests(self.root, 'node', report)
        self.assertEqual(report.read_text().count('# Subtest: original assertion\n'), 1)
        self.assertRegex(report.read_text(), r'ok \d+ - future path')
        self.assertIn('legacy script glob', report.read_text())
        self.assertIn('legacy UI glob', report.read_text())
        replay.candidate_tests(self.root, 'browser')
        self.assertEqual((self.root / 'future/provider/browser-ran.txt').read_text(), 'future browser executed')
        self.assertEqual(sources, {p: p.read_bytes() for p in sources})


if __name__ == '__main__':
    unittest.main()
