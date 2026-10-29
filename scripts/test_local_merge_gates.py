"""本地权威门禁的命令矩阵与分组编排测试；不启动 Rust、Node 或浏览器。"""
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location('local_gates', Path(__file__).with_name('local-merge-gates.py'))
local_gates = importlib.util.module_from_spec(spec)
spec.loader.exec_module(local_gates)


class LocalMergeGatesTests(unittest.TestCase):
    def test_matrix_is_local_and_covers_independent_domains(self):
        matrix = local_gates.command_matrix(Path('.'), 'H', Path('.verify'))
        self.assertEqual(set(matrix), {'contracts', 'server', 'frontend'})
        commands = [argv for steps in matrix.values() for _, argv in steps]
        flattened = ' '.join(' '.join(map(str, argv)) for argv in commands)
        self.assertNotIn('github', flattened.lower())
        self.assertNotIn('gh ', flattened.lower())
        self.assertIn('cargo test', flattened)
        self.assertRegex(flattened, r'(^| )npm(?:\.cmd)? ')

    def test_server_and_frontend_have_independent_reports(self):
        matrix = local_gates.command_matrix(Path('.'), 'H', Path('.verify'))
        server_names = {name for name, _ in matrix['server']}
        frontend_names = {name for name, _ in matrix['frontend']}
        contract_names = {name for name, _ in matrix['contracts']}
        self.assertIn('integration-closure', contract_names)
        self.assertIn('rust-list', server_names)
        self.assertIn('candidate-node', frontend_names)
        self.assertIn('baseline-rust', server_names)
        self.assertIn('baseline-frontend', frontend_names)

    def test_group_selection_is_explicit_and_all_is_stable(self):
        self.assertEqual(local_gates.selected_groups('contracts'), ('contracts',))
        self.assertEqual(local_gates.selected_groups('server'), ('server',))
        self.assertEqual(local_gates.selected_groups('frontend'), ('frontend',))
        self.assertEqual(local_gates.selected_groups('all'), ('contracts', 'server', 'frontend'))
        with self.assertRaisesRegex(ValueError, '未知门禁组'):
            local_gates.selected_groups('unknown')

    def test_single_group_does_not_execute_other_groups(self):
        matrix = {group: [(group + '-step', [sys.executable, '-c', 'pass'])]
                  for group in local_gates.GROUPS}
        calls = []

        def fake_run_group(_root, _report, group, _steps):
            calls.append(group)
            return [{'group': group, 'name': group + '-step', 'exit': 0,
                     'stdout': 'out', 'stderr': 'err'}]

        with patch.object(local_gates, 'run_group', side_effect=fake_run_group):
            results = local_gates.run_selected_groups(Path('.'), Path('.verify'), matrix, 'server')
        self.assertEqual(calls, ['server'])
        self.assertEqual([item['group'] for item in results], ['server'])

    def test_all_stops_before_expensive_groups_when_contracts_fail(self):
        matrix = {group: [(group + '-step', [sys.executable, '-c', 'pass'])]
                  for group in local_gates.GROUPS}
        calls = []

        def fake_run_group(_root, _report, group, _steps):
            calls.append(group)
            return [{'group': group, 'name': group + '-step',
                     'exit': 7 if group == 'contracts' else 0,
                     'stdout': 'out', 'stderr': 'err'}]

        with patch.object(local_gates, 'run_group', side_effect=fake_run_group):
            results = local_gates.run_selected_groups(Path('.'), Path('.verify'), matrix, 'all')
        self.assertEqual(calls, ['contracts'])
        self.assertEqual(results[0]['exit'], 7)


if __name__ == '__main__':
    unittest.main()
