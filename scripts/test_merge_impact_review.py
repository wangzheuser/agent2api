"""Impact-review binding checks; uses an isolated temporary Git repository."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


spec = importlib.util.spec_from_file_location(
    'impact_review', Path(__file__).with_name('check-merge-impact-review.py'))
impact_review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(impact_review)


def digest(document):
    material = dict(document)
    material.pop('digest', None)
    return hashlib.sha256(json.dumps(material, ensure_ascii=False, sort_keys=True).encode()).hexdigest()


class MergeImpactReviewTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'repo'
        self.root.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.name', 'fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        (self.root / 'source.py').write_text('value = 1\n', encoding='utf-8')
        self.git('add', 'source.py')
        self.git('commit', '-qm', 'fixture')
        self.h = self.git('rev-parse', 'HEAD')
        self.tree = self.git('write-tree')
        self.evidence = self.root / 'diagnostic-artifacts' / 'round'
        self.evidence.mkdir(parents=True)
        impact = []
        for name in ('upstream', 'from-local', 'from-upstream'):
            path = self.evidence / f'{name}.diff'
            path.write_text(name + '\n', encoding='utf-8')
            impact.append({'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
                           'argv': ['git', 'diff', self.h, self.h]})
        inputs = {'H': self.h, 'U': self.h, 'B': self.h}
        overlap = {
            'version': 1,
            'candidate_tree': self.tree,
            'inputs': inputs,
            'path_overlap': ['source.py'],
            'items': [{'path': 'source.py', 'decision': 'local_extension'}],
            'unresolved': [],
        }
        overlap['digest'] = digest(overlap)
        plan = {
            'version': 1,
            'candidate_tree': self.tree,
            'inputs': inputs,
            'overlap_digest': overlap['digest'],
            'required_gates': ['impact-review', 'integration-closure'],
        }
        plan['digest'] = digest(plan)
        self.record = {
            'schema_version': 1,
            'inputs': inputs,
            'impact': impact,
            'impact_tree': self.tree,
            'overlap': overlap,
            'plan': plan,
        }
        self.record_path = self.evidence / 'run.json'
        self.record_path.write_text(json.dumps(self.record, indent=2), encoding='utf-8')
        self.review_path = self.evidence / 'impact-review.md'
        self.write_review()

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True).strip()

    def payload(self):
        return {
            'schema_version': 1,
            'candidate_tree': self.tree,
            'inputs': self.record['inputs'],
            'plan_digest': self.record['plan']['digest'],
            'overlap_digest': self.record['overlap']['digest'],
            'reviewer': 'independent-reviewer',
            'reviewed_at': '2026-10-29T18:00:00+08:00',
            'summary': '逐项核对上游与本地语义，未发现阻断问题。',
            'paths': [{
                'path': 'source.py',
                'decision': 'local_extension',
                'rationale': '本地值是上游没有覆盖的产品扩展，候选保持调用入口。',
                'evidence': ['H/U/candidate 三向差异已核对', 'fixture test passed'],
            }],
            'findings': [],
        }

    def write_review(self, payload=None):
        payload = payload or self.payload()
        self.review_path.write_text(
            '# Impact review\n\n```merge-impact-review\n' +
            json.dumps(payload, ensure_ascii=False, indent=2) +
            '\n```\n', encoding='utf-8')

    def test_valid_review_is_bound_to_candidate_and_all_paths(self):
        report = impact_review.validate_review(self.root, self.record_path, self.review_path)
        self.assertTrue(report['passed'], report['findings'])
        self.assertEqual(report['reviewed_paths'], 1)

    def test_missing_path_and_decision_mismatch_are_rejected(self):
        payload = self.payload()
        payload['paths'] = []
        self.write_review(payload)
        report = impact_review.validate_review(self.root, self.record_path, self.review_path)
        self.assertFalse(report['passed'])
        self.assertTrue(any('未覆盖 overlap 路径' in item for item in report['findings']))

        payload = self.payload()
        payload['paths'][0]['decision'] = 'upstream_equivalent'
        self.write_review(payload)
        report = impact_review.validate_review(self.root, self.record_path, self.review_path)
        self.assertTrue(any('决策不匹配' in item for item in report['findings']))

    def test_stale_digest_is_rejected(self):
        payload = self.payload()
        payload['plan_digest'] = '0' * 64
        self.write_review(payload)
        report = impact_review.validate_review(self.root, self.record_path, self.review_path)
        self.assertFalse(report['passed'])
        self.assertTrue(any('plan_digest' in item for item in report['findings']))

    def test_open_p0_or_p1_finding_blocks_review(self):
        payload = self.payload()
        payload['findings'] = [{'severity': 'P0', 'status': 'open', 'summary': '路由语义未核实'}]
        self.write_review(payload)
        report = impact_review.validate_review(self.root, self.record_path, self.review_path)
        self.assertFalse(report['passed'])
        self.assertTrue(any('未解决的 P0' in item for item in report['findings']))


if __name__ == '__main__':
    unittest.main()
