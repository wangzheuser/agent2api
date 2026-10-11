"""本地权威门禁的命令矩阵测试；不启动 Rust、Node 或浏览器。"""
import importlib.util
from pathlib import Path
import unittest


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
        self.assertIn('rust-list', server_names)
        self.assertIn('candidate-node', frontend_names)
        self.assertIn('baseline-rust', server_names)
        self.assertIn('baseline-frontend', frontend_names)


if __name__ == '__main__':
    unittest.main()
