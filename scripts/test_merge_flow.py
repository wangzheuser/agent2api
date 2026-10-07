"""合入记录的真实 Git 反例；不操作项目分支或远端。"""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('flow', Path(__file__).with_name('merge-flow.py'))
flow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(flow)


class MergeFlowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'repo'
        self.root.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.name', 'fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'core.autocrlf', 'false')
        (self.root / 'source.py').write_text('value = 7\n', encoding='utf-8')
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture H')
        self.path = Path(self.temp.name) / 'evidence/run.json'
        self.record = flow.freeze(self.root, self.path, 'HEAD')
        flow.impact(self.root, self.path, self.record)

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True).strip()

    def run_check(self, code='assert True'):
        return flow.execute(self.root, self.path, self.record, 'business', '.', [sys.executable, '-c', code])

    def test_frozen_inputs_cannot_be_overwritten_and_impact_uses_sha(self):
        with self.assertRaises(ValueError):
            flow.freeze(self.root, self.path, 'HEAD')
        flow.impact(self.root, self.path, self.record)
        self.assertEqual(len(self.record['impact']), 3)
        self.assertTrue(all(Path(x['path']).is_file() for x in self.record['impact']))

    def test_record_has_single_writer(self):
        with flow.locked_record(self.path):
            with self.assertRaises(FileExistsError):
                with flow.locked_record(self.path):
                    self.fail('并发写者被放行')
        with flow.locked_record(self.path):
            pass

    def test_upstream_ref_advances_but_impact_keeps_frozen_commit(self):
        (self.root / 'source.py').write_text('value = 8\n', encoding='utf-8')
        self.git('commit', '-qam', 'fixture U1')
        self.git('branch', 'fixture-upstream')
        path = self.path.parent / 'frozen-u.json'
        self.git('checkout', '-q', self.record['inputs']['H'])
        self.git('checkout', '-qb', 'fixture-local')
        (self.root / 'source.py').write_text('value = 70\n', encoding='utf-8')
        self.git('commit', '-qam', 'fixture local H')
        record = flow.freeze(self.root, path, 'fixture-upstream')
        u1 = record['inputs']['U']
        self.git('update-ref', 'refs/heads/fixture-upstream', 'HEAD')
        flow.impact(self.root, path, record)
        self.assertIn(u1, record['impact'][0]['argv'])
        self.assertNotIn(self.git('rev-parse', 'fixture-upstream'), record['impact'][0]['argv'])
        self.assertIn('+value = 8', Path(record['impact'][0]['path']).read_text())
        self.assertIn('+value = 70', Path(record['impact'][2]['path']).read_text())
        self.assertNotEqual(record['inputs']['B'], record['inputs']['H'])
        self.assertNotEqual(record['inputs']['B'], record['inputs']['U'])

    def test_success_seals_exact_tree_and_checks_commit(self):
        self.assertEqual(self.run_check(), 0)
        flow.seal(self.root, self.path, self.record, ['business'])
        flow.check(self.root, self.record, 'HEAD')
        (self.root / 'source.py').write_text('value = 8\n', encoding='utf-8')
        self.git('add', 'source.py')
        with self.assertRaisesRegex(ValueError, '不一致'):
            flow.check(self.root, self.record)
        with self.assertRaisesRegex(ValueError, '差异分析'):
            flow.seal(self.root, self.path, self.record, ['business'])
        flow.impact(self.root, self.path, self.record)
        with self.assertRaisesRegex(ValueError, '同一候选'):
            flow.seal(self.root, self.path, self.record, ['business'])

    def test_failure_empty_or_missing_receipts_do_not_seal(self):
        self.assertNotEqual(self.run_check('raise SystemExit(7)'), 0)
        for names in [[], ['business'], ['missing']]:
            with self.subTest(names=names), self.assertRaises(ValueError):
                flow.seal(self.root, self.path, self.record, names)

    def test_new_and_unstaged_source_rejected(self):
        (self.root / 'new.py').write_text('new = True', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, '新文件'):
            flow.candidate_tree(self.root)
        self.git('add', 'new.py')
        (self.root / 'source.py').write_text('changed = True', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, '未暂存'):
            flow.candidate_tree(self.root)

    def test_hidden_index_flags_rejected(self):
        for flag in ['assume-unchanged', 'skip-worktree']:
            with self.subTest(flag=flag):
                self.git('update-index', '--' + flag, 'source.py')
                with self.assertRaisesRegex(ValueError, '索引标志'):
                    flow.candidate_tree(self.root)
                self.git('update-index', '--no-' + flag, 'source.py')

    def test_launch_error_supersedes_success_and_clears_seal(self):
        self.assertEqual(self.run_check(), 0)
        flow.seal(self.root, self.path, self.record, ['business'])
        self.assertNotEqual(flow.execute(self.root, self.path, self.record, 'business', '.', ['nonexistent-merge-command-5432']), 0)
        self.assertIsNone(self.record['seal'])
        with self.assertRaisesRegex(ValueError, '同一候选'):
            flow.seal(self.root, self.path, self.record, ['business'])

    def test_interrupted_run_leaves_pending_receipt_not_old_success(self):
        self.assertEqual(self.run_check(), 0)
        flow.seal(self.root, self.path, self.record, ['business'])
        real_run = subprocess.run
        def interrupt(args, **kwargs):
            if 'capture_output' in kwargs:
                raise KeyboardInterrupt()
            return real_run(args, **kwargs)
        with patch.object(flow.subprocess, 'run', side_effect=interrupt), self.assertRaises(KeyboardInterrupt):
            self.run_check()
        loaded = flow.read_record(self.root, self.path)
        self.assertIsNone(loaded['receipts'][-1]['exit'])
        with self.assertRaisesRegex(ValueError, '同一候选'):
            flow.seal(self.root, self.path, loaded, ['business'])

    def test_test_mutation_invalidates_zero_exit_receipt(self):
        command = "from pathlib import Path; Path('source.py').write_text('changed = True')"
        self.assertEqual(self.run_check(command), 1)
        self.assertEqual(self.record['receipts'][-1]['exit'], 0)
        self.assertFalse(self.record['receipts'][-1]['source_unchanged'])

    def test_output_tampering_and_branch_switch_rejected(self):
        self.assertEqual(self.run_check(), 0)
        flow.seal(self.root, self.path, self.record, ['business'])
        Path(self.record['receipts'][-1]['stdout']).write_text('fake PASS')
        with self.assertRaisesRegex(ValueError, '输出已改变'):
            flow.seal(self.root, self.path, self.record, ['business'])
        with self.assertRaisesRegex(ValueError, '输出已改变'):
            flow.check(self.root, self.record, 'HEAD')
        self.git('checkout', '-qb', 'fixture-other')
        with self.assertRaisesRegex(ValueError, '分支'):
            flow.read_record(self.root, self.path)

    def test_branch_changed_during_test_invalidates_receipt(self):
        code = "import subprocess; subprocess.run(['git', 'checkout', '-qb', 'fixture-moved'], check=True)"
        self.assertEqual(self.run_check(code), 1)
        self.assertFalse(self.record['receipts'][-1]['source_unchanged'])


if __name__ == '__main__':
    unittest.main()
