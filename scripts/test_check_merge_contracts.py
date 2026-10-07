"""门禁自身的反例测试：清单/实现/测试同时被删也必须失败。"""
import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name('check-merge-contracts.py')
spec = importlib.util.spec_from_file_location('merge_contracts', SCRIPT)
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class ContractGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        (self.root / 'docs').mkdir()
        (self.root / 'impl.rs').write_text('pub fn run() {}', encoding='utf-8')
        (self.root / 'tests.rs').write_text('#[test]\nfn counts() { assert_eq!(2, 2); }', encoding='utf-8')
        (self.root / 'tests.mjs').write_text("test('counts', () => assert.equal(2, 2))", encoding='utf-8')
        self.document = {'schema_version': 1, 'contracts': [{
            'id': 'token-counts', 'level': 'P0', 'status': '必须保留',
            'behavior': '真实用量不被零值覆盖', 'source_commits': ['bcbbea0'],
            'implementation': ['impl.rs'], 'tests': [
                {'kind': 'rust', 'path': 'tests.rs', 'name': 'counts'},
                {'kind': 'node', 'path': 'tests.mjs', 'name': 'counts'},
            ],
        }]}

    def validate(self, document=None, names=None):
        return gate.validate(self.root, self.document if document is None else document, names)

    def cli(self, *args):
        (self.root / gate.MANIFEST).write_text(json.dumps(self.document), encoding='utf-8')
        return subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root), *args],
                              capture_output=True, encoding='utf-8')

    def stable_baseline(self):
        for test in self.document['contracts'][0]['tests']:
            test['id'] = test['kind'] + '-counts'
        return copy.deepcopy(self.document)

    def declare_evolution(self, baseline, **fields):
        self.document['contracts'][0]['evolution'] = {
            'baseline_sha256': gate.contract_digest(baseline['contracts'][0]),
            'reason': '等价入口适配', 'review': '独立审查记录：断言和样本保持', **fields,
        }

    def moved_case(self):
        baseline = self.stable_baseline()
        (self.root / 'new.rs').write_text('#[test]\nfn renamed() { assert_eq!(2, 2); }', encoding='utf-8')
        self.document['contracts'][0]['implementation'] = ['new.rs']
        self.document['contracts'][0]['tests'][0].update(path='new.rs', name='renamed')
        self.declare_evolution(baseline,
            implementation_moves=[{'from': 'impl.rs', 'to': ['new.rs']}],
            test_moves=[{'from': 'rust-counts', 'to': ['rust-counts'], 'mode': 'adapted', 'reason': 'API 重命名'}])
        return baseline

    def test_valid_manifest_and_compiled_test_list(self):
        current = self.validate(names={'counts'})
        gate.compare_baseline(current, self.document)

    def test_empty_and_invalid_manifest(self):
        for document in [{}, [], {'schema_version': 1, 'contracts': []},
                         {'schema_version': 1, 'contracts': ['invalid']}]:
            with self.subTest(document=document), self.assertRaises(ValueError):
                gate.validate(self.root, document)

    def test_duplicate_id_and_json_keys(self):
        self.document['contracts'].append(copy.deepcopy(self.document['contracts'][0]))
        with self.assertRaisesRegex(ValueError, '重复'):
            self.validate()
        with self.assertRaisesRegex(ValueError, '重复 JSON'):
            gate.read_json('{"schema_version":1,"schema_version":2}')

    def test_missing_implementation_and_path_escape(self):
        for path in ['missing.rs', '../outside.rs', '/outside.rs', 'C:/outside.rs']:
            self.document['contracts'][0]['implementation'] = [path]
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.validate()

    def test_removed_or_ignored_rust_test(self):
        for source in ['fn counts() {}', '#[test]\n#[ignore]\nfn counts() {}']:
            (self.root / 'tests.rs').write_text(source, encoding='utf-8')
            with self.subTest(source=source), self.assertRaises(ValueError):
                self.validate()

    def test_missing_compiled_test_catches_cfg_disabled_cases(self):
        with self.assertRaisesRegex(ValueError, '编译后的测试列表缺少'):
            self.validate(names=set())

    def test_node_report_rejects_commented_or_unexecuted_test(self):
        with self.assertRaisesRegex(ValueError, 'Node 报告缺少真实通过'):
            gate.validate(self.root, self.document, {'counts'}, set())
        gate.validate(self.root, self.document, {'counts'}, {'counts'})

    def test_node_skip_todo_or_rename(self):
        for source in ["test.skip('counts', () => {})", "test.todo('counts')", "test('renamed', () => {})"]:
            (self.root / 'tests.mjs').write_text(source, encoding='utf-8')
            with self.subTest(source=source), self.assertRaises(ValueError):
                self.validate()

    def test_baseline_blocks_deleted_contract_and_tests(self):
        baseline = copy.deepcopy(self.document)
        with self.assertRaisesRegex(ValueError, '保护项被删除'):
            gate.compare_baseline({}, baseline)
        self.document['contracts'][0]['tests'].pop()
        with self.assertRaisesRegex(ValueError, '受保护测试入口'):
            gate.compare_baseline(self.validate(), baseline)

    def test_baseline_blocks_downgrade_and_changed_behavior(self):
        baseline = copy.deepcopy(self.document)
        self.document['contracts'][0]['level'] = 'P1'
        with self.assertRaisesRegex(ValueError, 'P0 被降级'):
            gate.compare_baseline(self.validate(), baseline)
        self.document['contracts'][0]['level'] = 'P0'
        self.document['contracts'][0]['behavior'] = '允许丢失输入'
        with self.assertRaisesRegex(ValueError, '行为基线改变'):
            gate.compare_baseline(self.validate(), baseline)

    def test_baseline_blocks_removed_implementation_and_changed_status(self):
        baseline = copy.deepcopy(self.document)
        (self.root / 'dummy.rs').write_text('pub fn unrelated() {}', encoding='utf-8')
        self.document['contracts'][0]['implementation'] = ['dummy.rs']
        with self.assertRaisesRegex(ValueError, '实现入口'):
            gate.compare_baseline(self.validate(), baseline)
        self.document['contracts'][0]['implementation'] = ['impl.rs']
        self.document['contracts'][0]['status'] = '上游等价实现'
        with self.assertRaisesRegex(ValueError, '状态基线'):
            gate.compare_baseline(self.validate(), baseline)

    def test_baseline_can_resolve_unverified_status_without_losing_mapping(self):
        baseline = copy.deepcopy(self.document)
        baseline['contracts'][0]['status'] = '待验证'
        gate.compare_baseline(self.validate(), baseline)
        baseline['contracts'][0]['source_commits'].append('0123456')
        with self.assertRaisesRegex(ValueError, '来源证据'):
            gate.compare_baseline(self.validate(), baseline)

    def test_invalid_baseline_cannot_disable_protection(self):
        for baseline in [{'schema_version': 1, 'contracts': []},
                         {'schema_version': 2, 'contracts': self.document['contracts']}]:
            with self.subTest(baseline=baseline), self.assertRaises(ValueError):
                gate.compare_baseline(self.validate(), baseline)

    def test_boolean_schema_and_nonstring_fields_fail_cleanly(self):
        for field, value in [('schema_version', True), ('id', 123), ('status', []),
                             ('name', None), ('name', 'counts:::invalid')]:
            document = copy.deepcopy(self.document)
            if field == 'schema_version':
                document[field] = value
            elif field == 'name':
                document['contracts'][0]['tests'][0][field] = value
            else:
                document['contracts'][0][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                self.validate(document)

    def test_rust_comments_strings_and_conditional_ignore_do_not_pass(self):
        for source in ['// #[test]\n// fn counts() {}',
                       '/* #[test]\nfn counts() {} */',
                       'const SOURCE: &str = r#"#[test]\nfn counts() {}"#;',
                       '#[test]\n#[cfg_attr(all(), ignore)]\nfn counts() {}']:
            (self.root / 'tests.rs').write_text(source, encoding='utf-8')
            with self.subTest(source=source), self.assertRaises(ValueError):
                self.validate(names={'counts'})

    def test_node_comments_do_not_pass(self):
        for source in ["// test('counts', () => {})", "/* test('counts', () => {}) */",
                       '''const source = "test('counts', () => {})"''',
                       "const source = `test('counts', () => {})`"]:
            (self.root / 'tests.mjs').write_text(source, encoding='utf-8')
            with self.subTest(source=source), self.assertRaises(ValueError):
                self.validate()

    def test_node_failed_incomplete_or_diagnostic_only_reports_fail(self):
        valid = ('TAP version 13\n# Subtest: counts\nok 1 - counts\n1..1\n'
                 '# tests 1\n# pass 1\n# fail 0\n# cancelled 0\n')
        report = self.root / 'node.txt'
        for text in ['ok 1 - counts\n', '# PASS counts\n',
                     valid.replace('# fail 0', '# fail 1'),
                     valid.replace('# cancelled 0', '# cancelled 1'),
                     valid + 'TAP version 13\nok 1 - unrelated\n',
                     valid.replace('1..1', '1..2')]:
            report.write_text(text, encoding='utf-8')
            with self.subTest(text=text):
                result = self.cli('--node-report', str(report))
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        report.write_text(valid, encoding='utf-8')
        self.assertEqual(self.cli('--node-report', str(report)).returncode, 0)

    def test_node_skip_and_todo_directives_are_case_insensitive(self):
        report = self.root / 'node.txt'
        for directive in ['# SKIP', '# skip', '# ToDo', '#\tSKIP reason']:
            report.write_text('TAP version 13\n# Subtest: counts\n'
                              f'ok 1 - counts {directive}\n1..1\n'
                              '# tests 1\n# pass 0\n# fail 0\n# cancelled 0\n', encoding='utf-8')
            with self.subTest(directive=directive):
                result = self.cli('--node-report', str(report))
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_node_helper_requires_passing_own_file_and_allows_mixed_numbering(self):
        (self.root / 'tests.mjs').write_text("check('counts', () => assert.equal(2, 2))", encoding='utf-8')
        report = ('TAP version 13\n# Subtest: other\nok 1 - other\n'
                  '# PASS counts\n# Subtest: tests.mjs\nok 1 - tests.mjs\n'
                  '1..2\n# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n')
        names = gate.node_report_names(self.root, report)
        gate.validate(self.root, self.document, {'counts'}, names)
        for invalid in [report.replace('ok 1 - tests.mjs', 'ok 1 - impl.rs'),
                        report.replace('ok 1 - tests.mjs', 'ok 1 - tests.mjs # SKIP')]:
            with self.subTest(report=invalid), self.assertRaises(ValueError):
                gate.validate(self.root, self.document, {'counts'}, gate.node_report_names(self.root, invalid))

    def test_unknown_status_and_type_fail_closed(self):
        self.document['contracts'][0]['status'] = '待验证'
        with self.assertRaises(ValueError):
            self.validate()
        self.document['contracts'][0]['status'] = '必须保留'
        self.document['contracts'][0]['tests'][0]['kind'] = 'shell'
        with self.assertRaises(ValueError):
            self.validate()

    def test_cli_missing_manifest_returns_failure(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root)], capture_output=True)
        self.assertEqual(result.returncode, 1)

    def test_baseline_lookup_distinguishes_bootstrap_from_bad_ref(self):
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                        'commit', '--allow-empty', '-qm', 'fixture'], cwd=self.root, check=True)
        self.document['contracts'][0]['source_commits'] = [subprocess.check_output(
            ['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()]
        self.assertIsNone(gate.baseline_at(self.root, 'HEAD'))
        result = self.cli('--base-ref', 'HEAD')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('BOOTSTRAP:', result.stdout)
        self.assertNotIn('BASELINE:', result.stdout)
        (self.root / gate.MANIFEST).write_text(json.dumps(self.document), encoding='utf-8')
        subprocess.run(['git', 'add', gate.MANIFEST], cwd=self.root, check=True)
        subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                        'commit', '-qm', 'baseline'], cwd=self.root, check=True)
        self.assertEqual(gate.baseline_at(self.root, 'HEAD'), self.document)
        with self.assertRaises(subprocess.CalledProcessError):
            gate.baseline_at(self.root, 'missing-ref')
        result = self.cli('--base-ref', 'missing-ref')
        self.assertEqual(result.returncode, 1)
        self.assertNotIn('BOOTSTRAP:', result.stdout)

    def test_contract_digest_is_canonical_and_excludes_evolution(self):
        original = self.document['contracts'][0]
        digest = gate.contract_digest(original)
        reordered = dict(reversed(list(original.items())))
        reordered['evolution'] = {'ignored': True}
        self.assertEqual(digest, gate.contract_digest(reordered))
        reordered['behavior'] += '新增行为'
        self.assertNotEqual(digest, gate.contract_digest(reordered))
        self.assertRegex(digest, r'^[0-9a-f]{64}$')

    def test_stable_ids_are_optional_global_and_unique_within_contract(self):
        self.validate()  # 历史无 ID 清单保持兼容。
        self.stable_baseline()
        duplicate = copy.deepcopy(self.document['contracts'][0])
        duplicate['id'] = 'other-contract'
        self.document['contracts'].append(duplicate)
        self.validate()  # 不同契约可映射同一物理用例和 ID。
        duplicate['tests'][0]['name'] = 'different'
        with self.assertRaisesRegex(ValueError, '全局测试 ID'):
            gate.validate(None, self.document)
        self.document['contracts'].pop()
        for value in ['', ' ', None, 12, 'rust-counts']:
            self.document['contracts'][0]['tests'][1]['id'] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, '测试 ID'):
                self.validate()

    def test_equivalent_move_and_status_transition_pass_with_bound_evidence(self):
        baseline = self.moved_case()
        self.document['contracts'][0]['status'] = '上游等价实现'
        gate.compare_baseline(self.validate(names={'renamed'}), baseline)
        next_baseline = copy.deepcopy(self.document)
        self.document['contracts'][0]['status'] = '必须保留'
        self.declare_evolution(next_baseline)
        gate.compare_baseline(self.validate(), next_baseline)

    def test_case_split_replacement_preserves_every_h_obligation(self):
        baseline = self.stable_baseline()
        (self.root / 'split.rs').write_text('#[test]\nfn first() {}\n#[test]\nfn second() {}', encoding='utf-8')
        self.document['contracts'][0]['tests'][:1] = [
            {'id': 'first-case', 'kind': 'rust', 'path': 'split.rs', 'name': 'first'},
            {'id': 'second-case', 'kind': 'rust', 'path': 'split.rs', 'name': 'second'}]
        self.declare_evolution(baseline, test_moves=[{
            'from': 'rust-counts', 'to': ['first-case', 'second-case'],
            'mode': 'replacement', 'reason': '完整计数义务拆为两个输入场景'}])
        current = self.validate(names={'first', 'second'})
        before = copy.deepcopy((baseline, self.document))
        view = gate.baseline_view(baseline, current)
        self.assertEqual((baseline, self.document), before)
        rust = [t for t in view['contracts'][0]['tests'] if t['kind'] == 'rust']
        self.assertEqual([t['name'] for t in rust], ['first', 'second'])
        self.assertTrue(all(t['baseline_inline'] and t['baseline_case_ids'] == ['rust-counts'] for t in rust))
        gate.validate(self.root, view, {'first', 'second'})

    def test_case_move_cannot_silently_drop_obligation_by_switching_runtime_kind(self):
        baseline = self.stable_baseline()
        self.document['contracts'][0]['tests'].pop(0)
        self.declare_evolution(baseline, test_moves=[{
            'from': 'rust-counts', 'to': ['node-counts'], 'mode': 'replacement', 'reason': '不同验证域'}])
        with self.assertRaisesRegex(ValueError, '保持验证类型'):
            gate.baseline_view(baseline, self.document)

    def test_move_requires_complete_fresh_evidence(self):
        baseline = self.moved_case()
        valid = copy.deepcopy(self.document['contracts'][0]['evolution'])
        bad = [None, {**valid, 'baseline_sha256': '0' * 64},
               {**valid, 'reason': ' '}, {**valid, 'review': ''},
               {**valid, 'implementation_moves': []}, {**valid, 'test_moves': []},
               {**valid, 'implementation_moves': [{'from': 'unknown.rs', 'to': ['new.rs']}]},
               {**valid, 'implementation_moves': [{'from': 'impl.rs', 'to': ['unknown.rs']}]},
               {**valid, 'test_moves': [{'from': 'unknown', 'to': ['rust-counts'], 'mode': 'adapted', 'reason': 'x'}]},
               {**valid, 'test_moves': [{'from': 'rust-counts', 'to': ['unknown'], 'mode': 'adapted', 'reason': 'x'}]}]
        for evolution in bad:
            if evolution is None:
                self.document['contracts'][0].pop('evolution', None)
            else:
                self.document['contracts'][0]['evolution'] = evolution
            with self.subTest(evolution=evolution), self.assertRaises(ValueError):
                gate.compare_baseline(self.validate(), baseline)

    def test_evolution_rejects_empty_duplicate_and_invalid_moves(self):
        self.stable_baseline()
        row = {'from': 'rust-counts', 'to': ['rust-counts'], 'mode': 'adapted', 'reason': 'x'}
        for rows in [None, [{}], [{**row, 'to': []}], [{**row, 'to': ['rust-counts', 'rust-counts']}],
                     [row, row], [{**row, 'mode': 'unchecked'}], [{**row, 'reason': ''}],
                     [row, {**row, 'from': 'node-counts'}]]:
            self.declare_evolution(self.document, test_moves=rows)
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                self.validate()

    def test_evolution_does_not_authorize_p0_or_source_loss(self):
        baseline = self.moved_case()
        for field, value in [('level', 'P1'), ('source_commits', ['0123456'])]:
            original = self.document['contracts'][0][field]
            self.document['contracts'][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                gate.compare_baseline(self.validate(), baseline)
            self.document['contracts'][0][field] = original
        for status in ['缺失', '待验证']:
            self.document['contracts'][0]['status'] = status
            with self.subTest(status=status), self.assertRaises(ValueError):
                gate.compare_baseline(gate.validate(None, self.document), baseline)

    def test_legacy_locator_cannot_be_replaced_by_new_ids(self):
        baseline = copy.deepcopy(self.document)
        self.stable_baseline()
        gate.compare_baseline(self.validate(), baseline)  # 首次给旧 locator 加 ID 不算移动。
        self.document['contracts'][0]['tests'][0]['name'] = 'renamed'
        self.declare_evolution(baseline, test_moves=[{
            'from': 'rust-counts', 'to': ['rust-counts'], 'mode': 'adapted', 'reason': 'x'}])
        with self.assertRaisesRegex(ValueError, '受保护测试入口'):
            gate.compare_baseline(gate.validate(None, self.document), baseline)

    def test_historical_evolution_does_not_block_unchanged_obligations(self):
        self.stable_baseline()
        self.declare_evolution(self.document, test_moves=[{
            'from': 'obsolete-case', 'to': ['obsolete-target'], 'mode': 'replacement', 'reason': '历史变更'}])
        self.document['contracts'][0]['evolution']['baseline_sha256'] = '0' * 64
        baseline = copy.deepcopy(self.document)
        gate.compare_baseline(self.validate(), baseline)
        view = gate.baseline_view(baseline, self.document)
        self.assertEqual(view['contracts'][0]['tests'][0]['path'], 'tests.rs')

    def test_view_keeps_original_independent_locators_and_maps_inline_only(self):
        baseline = self.stable_baseline()
        contract = baseline['contracts'][0]
        contract['tests'].append({'id': 'independent', 'kind': 'rust',
            'path': 'desktop-tauri/src-tauri/server/tests/old.rs', 'name': 'original'})
        self.document = copy.deepcopy(baseline)
        for test in self.document['contracts'][0]['tests']:
            test['path'] = 'new-' + test['path']
        self.declare_evolution(baseline, test_moves=[{'from': t['id'], 'to': [t['id']],
            'mode': 'adapted', 'reason': '接线改变'} for t in contract['tests']])
        view = gate.baseline_view(baseline, self.document)
        tests = {t['id']: t for t in view['contracts'][0]['tests']}
        self.assertEqual(tests['rust-counts']['path'], 'new-tests.rs')
        self.assertTrue(tests['rust-counts']['baseline_inline'])
        for case_id in ['node-counts', 'independent']:
            self.assertEqual(tests[case_id]['path'], next(t['path'] for t in contract['tests'] if t['id'] == case_id))
            self.assertNotIn('baseline_inline', tests[case_id])

    def test_view_shared_inline_independent_locator_keeps_both_origins(self):
        baseline = self.stable_baseline()
        baseline['contracts'][0]['tests'].append({'id': 'independent', 'kind': 'rust',
            'path': 'desktop-tauri/src-tauri/server/tests/old.rs', 'name': 'original'})
        self.document = copy.deepcopy(baseline)
        self.document['contracts'][0]['tests'].pop(0)
        self.declare_evolution(baseline, test_moves=[{
            'from': 'rust-counts', 'to': ['independent'], 'mode': 'replacement', 'reason': '共享测试'}])
        view = gate.baseline_view(baseline, self.document)
        rust = next(t for t in view['contracts'][0]['tests'] if t['kind'] == 'rust')
        self.assertTrue(rust['baseline_inline'] and rust['baseline_independent'])
        self.assertEqual(rust['baseline_case_ids'], ['rust-counts', 'independent'])
        gate.validate(None, view)

    def test_business_change_requires_bound_authorization_and_old_behavior(self):
        baseline = self.stable_baseline()
        self.document['contracts'][0]['behavior'] = '完整保留并扩展新的计数规则'
        valid = {'authorization': '用户消息：允许本轮新增规则', 'reason': '新增业务要求',
                 'previous_behavior': baseline['contracts'][0]['behavior']}
        self.declare_evolution(baseline, business_change=valid)
        gate.compare_baseline(self.validate(), baseline)
        for change in [None, {**valid, 'authorization': ''}, {**valid, 'previous_behavior': '错误旧行为'}]:
            self.declare_evolution(baseline, **({} if change is None else {'business_change': change}))
            with self.subTest(change=change), self.assertRaises(ValueError):
                gate.compare_baseline(self.validate(), baseline)
        self.declare_evolution(baseline, business_change=valid)
        self.document['contracts'][0]['evolution']['baseline_sha256'] = '0' * 64
        with self.assertRaisesRegex(ValueError, '摘要'):
            gate.compare_baseline(self.validate(), baseline)

    def test_authorized_retirement_is_audited_and_never_deletes_shared_test_file(self):
        baseline = self.stable_baseline()
        other = copy.deepcopy(baseline['contracts'][0])
        other['id'] = 'still-active'
        baseline['contracts'].append(other)
        self.document = copy.deepcopy(baseline)
        source = (self.root / 'tests.rs').read_bytes()
        retired = self.document['contracts'][0]
        retired.update(status='已批准替代/移除', implementation=[], tests=[])
        self.declare_evolution(baseline, business_change={
            'authorization': '用户确认：退休该功能', 'reason': '业务停用', 'previous_behavior': retired['behavior']})
        current = self.validate()
        view = gate.baseline_view(baseline, current)
        self.assertEqual([c['id'] for c in view['contracts']], ['still-active'])
        self.assertEqual(view['audit_retired'][0]['id'], 'token-counts')
        self.assertEqual(view['audit_retired'][0]['evolution']['business_change']['authorization'], '用户确认：退休该功能')
        self.assertEqual((self.root / 'tests.rs').read_bytes(), source)
        gate.compare_baseline(current, copy.deepcopy(self.document))  # 以后不重新授权同一退休。
        self.document['contracts'].pop()
        view = gate.baseline_view({'schema_version': 1, 'contracts': [baseline['contracts'][0]]}, self.document)
        self.assertEqual(view['contracts'], [])
        self.assertEqual(len(view['audit_retired']), 1)

    def test_retirement_without_authorization_or_with_wrong_old_behavior_fails(self):
        baseline = self.stable_baseline()
        self.document['contracts'][0]['status'] = '已批准替代/移除'
        with self.assertRaises(ValueError):
            self.validate()
        for previous in ['错误旧行为', baseline['contracts'][0]['behavior']]:
            self.declare_evolution(baseline, business_change={'authorization': '用户确认',
                'reason': '停用', 'previous_behavior': previous})
            if previous != baseline['contracts'][0]['behavior']:
                with self.assertRaises(ValueError):
                    gate.compare_baseline(self.validate(), baseline)
            else:
                self.document['contracts'][0]['evolution']['baseline_sha256'] = '0' * 64
                with self.assertRaises(ValueError):
                    gate.compare_baseline(self.validate(), baseline)

    def test_source_sha_must_resolve_to_commit_in_real_repository(self):
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        with self.assertRaisesRegex(ValueError, '来源提交不存在'):
            self.validate()
        subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                        'commit', '--allow-empty', '-qm', 'fixture'], cwd=self.root, check=True)
        sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        self.document['contracts'][0]['source_commits'] = [sha]
        self.validate()
        blob = subprocess.check_output(['git', 'hash-object', '-w', 'impl.rs'], cwd=self.root, text=True).strip()
        self.document['contracts'][0]['source_commits'] = [blob]
        with self.assertRaisesRegex(ValueError, '不是 commit'):
            self.validate()

    def test_cli_manifest_override_retains_root_file_boundaries(self):
        manifest = self.root / 'view.json'
        manifest.write_text(json.dumps(self.document), encoding='utf-8')
        self.document['schema_version'] = 2
        result = self.cli('--manifest', str(manifest))
        self.assertEqual(result.returncode, 0, result.stderr)
        document = gate.read_json(manifest.read_text(encoding='utf-8'))
        document['contracts'][0]['tests'][0]['path'] = '../outside.rs'
        manifest.write_text(json.dumps(document), encoding='utf-8')
        self.assertEqual(self.cli('--manifest', str(manifest)).returncode, 1)

    def test_runtime_reports_reject_same_kind_name_across_mapped_paths(self):
        for kind, name, source in [('rust', 'other.rs', '#[test]\nfn counts() {}'),
                                    ('node', 'other.mjs', "test('counts', () => {})")]:
            document = copy.deepcopy(self.document)
            (self.root / name).write_text(source, encoding='utf-8')
            document['contracts'][0]['tests'].append({'kind': kind, 'path': name, 'name': 'counts'})
            self.validate(document)  # 静态合法；只有名字集合证据时须拒绝歧义。
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, '报告身份歧义'):
                gate.validate(self.root, document, {'counts'} if kind == 'rust' else None,
                              {'counts'} if kind == 'node' else None)

    def test_node_protected_duplicate_and_skip_pass_conflicts_fail(self):
        def report(title):
            return f'TAP version 13\nok 1 - {title}\n1..1\n# fail 0\n# cancelled 0\n'
        for text in [report('counts') + report('counts'), report('counts # SKIP') + report('counts')]:
            with self.subTest(text=text), self.assertRaisesRegex(ValueError, '标题重复或 skip/pass'):
                gate.node_report_names(self.root, text, {'counts'})
        gate.node_report_names(self.root, report('other') + report('other') + report('counts'), {'counts'})

    def test_rust_list_rejects_duplicate_protected_names(self):
        with self.assertRaisesRegex(ValueError, '多个列表入口'):
            gate.rust_report_names('counts: test\ncounts: test\n', {'counts'})
        self.assertEqual(gate.rust_report_names('other: test\nother: test\ncounts: test\n', {'counts'}), {'other', 'counts'})

    def test_cli_malformed_manifest_fails_without_traceback(self):
        for document in [[], {'schema_version': 1, 'contracts': ['invalid']},
                         {'schema_version': 1, 'contracts': [{'id': 'bad-contract', 'tests': [None]}]}]:
            self.document = document
            with self.subTest(document=document):
                result = self.cli()
                self.assertEqual(result.returncode, 1)
                self.assertIn('FAIL:', result.stderr)
                self.assertNotIn('Traceback', result.stderr)


if __name__ == '__main__':
    unittest.main()
