#!/usr/bin/env python3
"""AI 合入的薄记录入口：冻结输入、导出差异、执行验收并校验提交树；不自动合并或提交。"""
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_record(path, record):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(record, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    temporary.replace(path)


@contextmanager
def locked_record(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    lock = path.with_suffix(path.suffix + '.lock')
    marker = lock.open('x', encoding='utf-8')  # 单轮记录只有一个写者；不抢占失联记录。
    try:
        with marker:
            marker.write(str(os.getpid()))
            marker.flush()
            yield
    finally:
        lock.unlink()


def commit(root, ref):
    return git(root, 'rev-parse', '--verify', '--end-of-options', ref + '^{commit}').decode().strip()


def branch(root):
    return git(root, 'branch', '--show-current').decode().strip()


def freeze(root, path, upstream):
    if path.exists():
        raise ValueError('记录已存在；续作复用它，不覆盖冻结输入')
    if git(root, 'rev-parse', '--is-shallow-repository').strip() != b'false':
        raise ValueError('先补齐历史，再冻结共同祖先')
    if not branch(root):
        raise ValueError('需要明确的当前工作分支')
    if subprocess.run(['git', 'rev-parse', '-q', '--verify', 'MERGE_HEAD'], cwd=root,
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
        raise ValueError('已有合并状态；先复用该轮冻结记录，不另建基线')
    h, u = commit(root, 'HEAD'), commit(root, upstream)
    bases = git(root, 'merge-base', '--all', h, u).decode().splitlines()
    if len(bases) != 1:
        raise ValueError('共同祖先不是唯一值；先分析历史，不自行选基线')
    record = {'schema_version': 1, 'root': str(root), 'branch': branch(root),
              'inputs': {'H': h, 'U': u, 'B': bases[0]},
              'initial_status': git(root, 'status', '--porcelain=v1', '-z').decode().split('\0'),
              'receipts': [], 'seal': None}
    write_record(path, record)
    print(f'FROZEN: H={h}; U={u}; B={bases[0]}; branch={record["branch"]}')
    return record


def read_record(root, path):
    record = json.loads(path.read_text(encoding='utf-8'))
    if record.get('schema_version') != 1 or Path(record['root']).resolve() != root:
        raise ValueError('记录版本或仓库不匹配')
    if branch(root) != record['branch']:
        raise ValueError('当前分支已改变')
    for sha in record['inputs'].values():
        if commit(root, sha) != sha:
            raise ValueError('冻结输入失效')
    subprocess.run(['git', 'merge-base', '--is-ancestor', record['inputs']['H'], 'HEAD'],
                   cwd=root, check=True)
    return record


def impact(root, path, record):
    inputs = record['inputs']
    tree = candidate_tree(root)
    pairs = [('upstream', [inputs['B'], inputs['U']]),
             ('from-local', [inputs['H'], tree]), ('from-upstream', [inputs['U'], tree])]
    reports = []
    for name, refs in pairs:
        data = git(root, 'diff', '--binary', *refs, '--')
        output = path.parent / (name + '.diff')
        output.write_bytes(data)
        reports.append({'path': str(output), 'sha256': digest(data),
                        'argv': ['git', 'diff', '--binary', *refs, '--']})
    record['impact'] = reports
    record['impact_tree'] = tree
    write_record(path, record)
    print(f'IMPACT: B→U / H→candidate / U→candidate 已导出；tree={tree}')


def candidate_tree(root):
    hidden = [row[2:] for row in git(root, 'ls-files', '-v', '-z').decode().split('\0')
              if row and (row[0].islower() or row[0] == 'S')]
    if hidden:
        raise ValueError('候选含 assume-unchanged/skip-worktree 文件；先显式核对并恢复索引标志: ' + ', '.join(hidden))
    if git(root, 'diff-files', '--name-only', '--diff-filter=U').strip():
        raise ValueError('仍有未解决冲突')
    subprocess.run(['git', 'update-index', '--refresh'], cwd=root,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if subprocess.run(['git', 'diff-files', '--quiet', '--'], cwd=root).returncode != 0:
        raise ValueError('候选有未暂存修改；先生成产物并按范围暂存，或使用隔离候选')
    unknown = git(root, 'ls-files', '--others', '--exclude-standard', '-z').decode().split('\0')
    remaining = [name for name in unknown if name and not name.startswith(('diagnostic-artifacts/', '.verify/'))]
    if remaining:
        raise ValueError('有未纳入候选的新文件: ' + ', '.join(remaining))
    return git(root, 'write-tree').decode().strip()


def execute(root, path, record, name, cwd, argv):
    if not re.fullmatch(r'[a-z][a-z0-9-]*', name) or not argv:
        raise ValueError('验收名称或 argv 无效')
    directory = (root / cwd).resolve()
    if not directory.is_relative_to(root) or not directory.is_dir():
        raise ValueError('执行目录越过仓库或不存在')
    tree = candidate_tree(root)
    sequence = len(record['receipts']) + 1
    prefix = path.parent / f'{sequence:03d}-{name}'
    start = time.time()
    receipt = {'name': name, 'candidate_tree': tree, 'argv': argv, 'cwd': str(directory),
               'started_at': start, 'exit': None, 'source_unchanged': False}
    record['receipts'].append(receipt)
    record['seal'] = None
    write_record(path, record)
    executable = shutil.which(argv[0]) or argv[0]
    try:
        result = subprocess.run([executable, *argv[1:]], cwd=directory, capture_output=True)
    except OSError as error:
        result = subprocess.CompletedProcess(argv, -1, b'', str(error).encode('utf-8'))
    stdout, stderr = prefix.with_suffix('.stdout'), prefix.with_suffix('.stderr')
    stdout.write_bytes(result.stdout)
    stderr.write_bytes(result.stderr)
    try:
        read_record(root, path)
        unchanged = candidate_tree(root) == tree
    except (ValueError, subprocess.CalledProcessError):
        unchanged = False
    receipt.update({'finished_at': time.time(), 'exit': result.returncode,
               'source_unchanged': unchanged, 'executable': executable,
               'stdout': str(stdout), 'stderr': str(stderr),
               'stdout_sha256': digest(result.stdout), 'stderr_sha256': digest(result.stderr)})
    write_record(path, record)
    print(f'RECEIPT: {name}; tree={tree}; exit={result.returncode}; source_unchanged={unchanged}')
    return result.returncode if unchanged else 1


def validate_receipts(record, tree, required):
    if not required or len(set(required)) != len(required):
        raise ValueError('须明确本轮必需验收项且不得重复')
    latest = {receipt['name']: receipt for receipt in record['receipts']}
    for name in required:
        receipt = latest.get(name)
        if not receipt or receipt['exit'] != 0 or not receipt['source_unchanged'] or receipt['candidate_tree'] != tree:
            raise ValueError(f'缺少同一候选的成功回执: {name}')
        for kind in ('stdout', 'stderr'):
            if digest(Path(receipt[kind]).read_bytes()) != receipt[kind + '_sha256']:
                raise ValueError(f'验收输出已改变: {name}/{kind}')


def validate_impact(record, tree):
    if record.get('impact_tree') != tree:
        raise ValueError('差异分析不是当前候选树；重新导出并审阅 impact')
    for report in record['impact']:
        if digest(Path(report['path']).read_bytes()) != report['sha256']:
            raise ValueError('差异分析报告已改变；重新导出并审阅 impact')


def seal(root, path, record, required):
    tree = candidate_tree(root)
    validate_impact(record, tree)
    validate_receipts(record, tree, required)
    record['seal'] = {'candidate_tree': tree, 'required': required, 'sealed_at': time.time()}
    write_record(path, record)
    print(f'SEALED: tree={tree}; receipts={len(required)}')


def check(root, record, ref=None):
    if not record['seal']:
        raise ValueError('尚无封存验收')
    actual = git(root, 'rev-parse', '--verify', '--end-of-options', ref + '^{tree}').decode().strip() if ref else candidate_tree(root)
    if actual != record['seal']['candidate_tree']:
        raise ValueError('候选或提交树与已验收输入不一致；重跑受影响验收')
    validate_impact(record, actual)
    validate_receipts(record, actual, record['seal']['required'])
    print(f'MATCH: tree={actual}' + (f'; commit={commit(root, ref)}' if ref else ''))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--record', type=Path, required=True)
    actions = parser.add_subparsers(dest='action', required=True)
    actions.add_parser('freeze').add_argument('--upstream', required=True)
    actions.add_parser('impact')
    run = actions.add_parser('run')
    run.add_argument('--name', required=True)
    run.add_argument('--cwd', default='.')
    run.add_argument('argv', nargs=argparse.REMAINDER)
    actions.add_parser('seal').add_argument('--required', nargs='+', required=True)
    actions.add_parser('check').add_argument('--commit')
    args = parser.parse_args()
    root, path = args.root.resolve(), args.record.resolve()
    try:
        with locked_record(path):
            if args.action == 'freeze':
                freeze(root, path, args.upstream)
                return 0
            record = read_record(root, path)
            if args.action == 'impact':
                impact(root, path, record)
            elif args.action == 'run':
                argv = args.argv[1:] if args.argv[:1] == ['--'] else args.argv
                return execute(root, path, record, args.name, args.cwd, argv)
            elif args.action == 'seal':
                seal(root, path, record, args.required)
            else:
                check(root, record, args.commit)
            return 0
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
