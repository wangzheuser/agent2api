"""Worktree preparation tests for the isolated merge flow."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location('prepare', Path(__file__).with_name('prepare-merge-candidate.py'))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class PrepareCandidateTests(unittest.TestCase):
    def git(self, root, *args):
        return subprocess.check_output(['git', *args], cwd=root, text=True).strip()

    def test_creates_candidate_from_clean_tracked_checkout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / 'repo'
            candidate = Path(temp) / 'candidate'
            record = Path(temp) / 'candidate.json'
            root.mkdir()
            self.git(root, 'init', '-q')
            self.git(root, 'config', 'user.name', 'fixture')
            self.git(root, 'config', 'user.email', 'fixture@example.invalid')
            (root / 'source.txt').write_text('base\n', encoding='utf-8')
            self.git(root, 'add', '.')
            self.git(root, 'commit', '-qm', 'H')
            args = ['prepare-merge-candidate.py', '--root', str(root), '--upstream', 'HEAD',
                    '--worktree', str(candidate), '--record', str(record)]
            with patch.object(sys, 'argv', args):
                self.assertEqual(prepare.main(), 0)
            self.assertTrue((candidate / 'source.txt').is_file())
            self.assertEqual(self.git(candidate, 'rev-parse', 'HEAD'), self.git(root, 'rev-parse', 'HEAD'))
            self.git(root, 'worktree', 'remove', '--force', str(candidate))

    def test_tracked_dirty_checkout_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.git(root, 'init', '-q')
            self.git(root, 'config', 'user.name', 'fixture')
            self.git(root, 'config', 'user.email', 'fixture@example.invalid')
            (root / 'source.txt').write_text('base\n', encoding='utf-8')
            self.git(root, 'add', '.')
            self.git(root, 'commit', '-qm', 'H')
            (root / 'source.txt').write_text('dirty\n', encoding='utf-8')
            args = ['prepare-merge-candidate.py', '--root', str(root), '--upstream', 'HEAD',
                    '--worktree', str(Path(temp) / 'candidate'), '--record', str(Path(temp) / 'record.json')]
            with patch.object(sys, 'argv', args):
                self.assertEqual(prepare.main(), 1)

    def test_protected_control_file_deletion_is_rejected_before_worktree(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.git(root, 'init', '-q')
            self.git(root, 'config', 'user.name', 'fixture')
            self.git(root, 'config', 'user.email', 'fixture@example.invalid')
            (root / 'AGENTS.md').write_text('rules\n', encoding='utf-8')
            (root / 'source.txt').write_text('base\n', encoding='utf-8')
            self.git(root, 'add', '.')
            self.git(root, 'commit', '-qm', 'H')
            self.git(root, 'branch', 'fixture-upstream')
            self.git(root, 'checkout', '-q', 'fixture-upstream')
            (root / 'AGENTS.md').unlink()
            self.git(root, 'add', '-u')
            self.git(root, 'commit', '-qm', 'delete governance')
            self.git(root, 'checkout', '-q', 'master')
            self.assertEqual(
                prepare.protected_deletions(root, self.git(root, 'rev-parse', 'HEAD'), 'fixture-upstream'),
                ['AGENTS.md'],
            )


if __name__ == '__main__':
    unittest.main()
