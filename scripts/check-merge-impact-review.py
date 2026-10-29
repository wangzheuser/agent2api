#!/usr/bin/env python3
"""Validate that an impact review is complete and bound to one frozen merge candidate."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


ALLOWED_DECISIONS = {'upstream_equivalent', 'local_extension', 'semantic_conflict', 'local_only'}
RESOLVED_FINDING_STATUSES = {'resolved', 'accepted', 'closed'}
REVIEW_BLOCK = re.compile(r'```merge-impact-review\s*\r?\n(.*?)\r?\n```', re.DOTALL)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root)


def stable_digest(document):
    material = dict(document)
    material.pop('digest', None)
    return sha256(json.dumps(material, ensure_ascii=False, sort_keys=True).encode('utf-8'))


def candidate_tree(root):
    subprocess.run(['git', 'update-index', '--refresh'], cwd=root,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if git(root, 'diff-files', '--name-only', '--diff-filter=U').strip():
        raise ValueError('候选存在未解决冲突')
    if subprocess.run(['git', 'diff-files', '--quiet', '--'], cwd=root).returncode != 0:
        raise ValueError('候选存在未暂存修改')
    unknown = git(root, 'ls-files', '--others', '--exclude-standard', '-z').decode().split('\0')
    remaining = [name for name in unknown if name and not name.startswith(('diagnostic-artifacts/', '.verify/'))]
    if remaining:
        raise ValueError('候选存在未纳入文件: ' + ', '.join(remaining))
    return git(root, 'write-tree').decode().strip()


def parse_review(path):
    text = path.read_text(encoding='utf-8')
    blocks = REVIEW_BLOCK.findall(text)
    if len(blocks) != 1:
        raise ValueError('impact review 必须且只能包含一个 ```merge-impact-review 结构块')
    payload = json.loads(blocks[0])
    if not isinstance(payload, dict):
        raise ValueError('impact review 结构块必须是 JSON 对象')
    return payload, text


def validate_digest(document, label, findings):
    actual = document.get('digest') if isinstance(document, dict) else None
    if not actual or stable_digest(document) != actual:
        findings.append(f'{label} digest 无效或已被修改')


def validate_review(root, record_path, review_path):
    root = root.resolve()
    record_path = record_path.resolve()
    review_path = review_path.resolve()
    findings = []
    try:
        record = json.loads(record_path.read_text(encoding='utf-8'))
        payload, review_text = parse_review(review_path)
        tree = candidate_tree(root)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError, subprocess.CalledProcessError) as error:
        return {'passed': False, 'findings': [str(error)]}

    if record.get('schema_version') != 1:
        findings.append('run record schema_version 不是 1')
    inputs = record.get('inputs')
    plan = record.get('plan')
    overlap = record.get('overlap')
    impact = record.get('impact')
    if not isinstance(inputs, dict) or set(inputs) != {'H', 'U', 'B'}:
        findings.append('run record 缺少精确 H/U/B')
        inputs = inputs if isinstance(inputs, dict) else {}
    else:
        for label, commit in inputs.items():
            try:
                resolved = git(root, 'rev-parse', '--verify', '--end-of-options', commit + '^{commit}').decode().strip()
                if resolved != commit:
                    findings.append(f'{label} 不是精确提交 SHA')
            except subprocess.CalledProcessError:
                findings.append(f'{label} 提交不可解析: {commit}')
        try:
            bases = git(root, 'merge-base', '--all', inputs['H'], inputs['U']).decode().splitlines()
            if bases != [inputs['B']]:
                findings.append('记录中的 B 不是 H/U 的唯一共同祖先')
        except (KeyError, subprocess.CalledProcessError):
            findings.append('无法验证 H/U/B 共同祖先')

    if not isinstance(plan, dict):
        findings.append('run record 缺少 plan')
        plan = {}
    if not isinstance(overlap, dict):
        findings.append('run record 缺少 overlap')
        overlap = {}
    if not isinstance(impact, list) or len(impact) != 3:
        findings.append('run record 必须包含三份 impact 差异报告')
        impact = impact if isinstance(impact, list) else []

    validate_digest(plan, 'plan', findings)
    validate_digest(overlap, 'overlap', findings)
    if plan.get('candidate_tree') != tree:
        findings.append('plan 未绑定当前 candidate tree')
    if overlap.get('candidate_tree') != tree:
        findings.append('overlap 未绑定当前 candidate tree')
    if record.get('impact_tree') != tree:
        findings.append('impact 未绑定当前 candidate tree')
    if plan.get('inputs') != inputs or overlap.get('inputs') != inputs:
        findings.append('plan/overlap 的 H/U/B 与 run record 不一致')
    if plan.get('overlap_digest') != overlap.get('digest'):
        findings.append('plan 与 overlap digest 不一致')
    if overlap.get('unresolved'):
        findings.append('overlap 仍有未分类路径')

    for report in impact:
        try:
            path = Path(report['path'])
            if sha256(path.read_bytes()) != report.get('sha256'):
                findings.append(f'impact 报告哈希不匹配: {path}')
        except (KeyError, OSError):
            findings.append('impact 报告缺失或不可读')

    if payload.get('schema_version') != 1:
        findings.append('review schema_version 必须为 1')
    if payload.get('candidate_tree') != tree:
        findings.append('review candidate_tree 与当前候选不一致')
    if payload.get('inputs') != inputs:
        findings.append('review H/U/B 与 run record 不一致')
    if payload.get('plan_digest') != plan.get('digest'):
        findings.append('review plan_digest 不匹配')
    if payload.get('overlap_digest') != overlap.get('digest'):
        findings.append('review overlap_digest 不匹配')
    for key in ('reviewer', 'reviewed_at', 'summary'):
        if not isinstance(payload.get(key), str) or not payload[key].strip():
            findings.append(f'review 缺少非空字段: {key}')

    expected = {item.get('path'): item.get('decision') for item in overlap.get('items', [])
                if isinstance(item, dict) and item.get('path')}
    rows = payload.get('paths')
    if not isinstance(rows, list):
        findings.append('review paths 必须是数组')
        rows = []
    seen = set()
    for row in rows:
        if not isinstance(row, dict):
            findings.append('review paths 元素必须是对象')
            continue
        path = row.get('path')
        decision = row.get('decision')
        rationale = row.get('rationale')
        evidence = row.get('evidence')
        if not isinstance(path, str) or not path:
            findings.append('review path 缺少路径')
            continue
        if path in seen:
            findings.append(f'review path 重复: {path}')
        seen.add(path)
        if path not in expected:
            findings.append(f'review 包含非 overlap 路径: {path}')
            continue
        if decision not in ALLOWED_DECISIONS or decision != expected[path]:
            findings.append(f'review 决策不匹配: {path}')
        if not isinstance(rationale, str) or len(rationale.strip()) < 8:
            findings.append(f'review 缺少充分理由: {path}')
        if not isinstance(evidence, list) or not evidence or any(
                not isinstance(item, str) or not item.strip() for item in evidence):
            findings.append(f'review 缺少证据列表: {path}')
    missing = sorted(set(expected) - seen)
    if missing:
        findings.append('review 未覆盖 overlap 路径: ' + ', '.join(missing))

    review_findings = payload.get('findings')
    if not isinstance(review_findings, list):
        findings.append('review findings 必须是数组')
        review_findings = []
    for item in review_findings:
        if not isinstance(item, dict):
            findings.append('review finding 必须是对象')
            continue
        severity = item.get('severity')
        status = item.get('status')
        summary = item.get('summary')
        if severity not in {'P0', 'P1', 'P2'}:
            findings.append('review finding severity 必须是 P0/P1/P2')
        if not isinstance(summary, str) or not summary.strip():
            findings.append('review finding 缺少 summary')
        if severity in {'P0', 'P1'} and status not in RESOLVED_FINDING_STATUSES:
            findings.append(f'存在未解决的 {severity} finding: {summary or "<missing>"}')

    return {
        'passed': not findings,
        'candidate_tree': tree,
        'inputs': inputs,
        'plan_digest': plan.get('digest'),
        'overlap_digest': overlap.get('digest'),
        'review_sha256': sha256(review_text.encode('utf-8')),
        'reviewed_paths': len(rows),
        'expected_paths': len(expected),
        'findings': findings,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--record', type=Path, required=True)
    parser.add_argument('--review', type=Path, required=True)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    report = validate_review(args.root, args.record, args.review)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    if report['passed']:
        print(f"IMPACT_REVIEW: PASS tree={report['candidate_tree']}; paths={report['reviewed_paths']}")
        return 0
    print('IMPACT_REVIEW: FAIL', file=sys.stderr)
    for finding in report['findings']:
        print(f' - {finding}', file=sys.stderr)
    return 1


if __name__ == '__main__':
    sys.exit(main())
