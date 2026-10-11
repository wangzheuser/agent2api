#!/usr/bin/env python3
"""AI 合入的薄记录入口：冻结输入、导出差异、执行验收并校验提交树；不自动合并或提交。"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
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


def ensure_state(record):
    """Fill state fields while keeping schema_version=1 records readable."""
    record.setdefault('phase', 'frozen')
    record.setdefault('next_action', 'review impact')
    record.setdefault('failures', [])
    record.setdefault('decisions', [])
    record.setdefault('events', [])
    record.setdefault('plan', None)
    record.setdefault('last_successful_command', None)
    record.setdefault('last_successful_receipt', None)
    return record


def event(record, kind, **fields):
    record.setdefault('events', []).append({
        'kind': kind,
        'at': time.time(),
        **fields,
    })


def set_phase(record, phase, next_action, **fields):
    record['phase'] = phase
    record['next_action'] = next_action
    record.update(fields)
    event(record, 'phase', phase=phase, next_action=next_action)


def record_failure(record, code, message, retryable, next_action):
    failure = {
        'code': code,
        'message': message,
        'retryable': retryable,
        'next_action': next_action,
        'at': time.time(),
    }
    record.setdefault('failures', []).append(failure)
    record['last_failure'] = failure
    record['next_action'] = next_action
    event(record, 'failure', **failure)


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
              'initial_status': [item for item in git(root, 'status', '--porcelain=v1', '-z').decode().split('\0') if item],
              'receipts': [], 'seal': None}
    ensure_state(record)
    set_phase(record, 'frozen', 'generate plan')
    event(record, 'freeze', upstream=upstream, tracking=tracking_ref(root))
    write_record(path, record)
    print(f'FROZEN: H={h}; U={u}; B={bases[0]}; branch={record["branch"]}')
    return record


def read_record(root, path):
    record = json.loads(path.read_text(encoding='utf-8'))
    if record.get('schema_version') != 1 or Path(record['root']).resolve() != root:
        raise ValueError('记录版本或仓库不匹配')
    if branch(root) != record['branch']:
        raise ValueError('当前分支已改变')
    ensure_state(record)
    for sha in record['inputs'].values():
        if commit(root, sha) != sha:
            raise ValueError('冻结输入失效')
    subprocess.run(['git', 'merge-base', '--is-ancestor', record['inputs']['H'], 'HEAD'],
                   cwd=root, check=True)
    return record


def tracking_ref(root):
    value = git(root, 'for-each-ref', '--format=%(upstream:short)', 'refs/heads/' + branch(root)).decode().strip()
    return value or None


def changed_paths(root, left, right):
    data = git(root, 'diff', '--name-status', left, right, '--').decode().splitlines()
    rows = []
    for line in data:
        fields = line.split('\t')
        if len(fields) >= 2:
            rows.append({'status': fields[0], 'path': fields[-1],
                         'from': fields[1] if len(fields) > 2 else None})
    return rows


def load_manifest(root):
    manifest = root / 'docs' / 'local-contracts.json'
    if not manifest.is_file():
        return []
    document = json.loads(manifest.read_text(encoding='utf-8'))
    return document.get('contracts', []) if isinstance(document, dict) else []


def contract_matches(contract, paths):
    names = set(paths)
    implementation = set(contract.get('implementation', []))
    tests = {item.get('path') for item in contract.get('tests', []) if isinstance(item, dict)}
    return bool(names.intersection(implementation | tests))


def domain_for(path):
    if path.startswith('desktop-tauri/src-tauri/'):
        return 'server'
    if path.startswith('desktop-tauri/ui-islands/') or path.startswith('desktop-tauri/ui-kit/'):
        return 'frontend-source'
    if path.startswith('desktop-tauri/ui/'):
        return 'generated-artifact'
    if path.startswith('docs/') or path.endswith('.md'):
        return 'docs'
    if path.startswith('scripts/'):
        return 'merge-tooling'
    return 'other'


def plan(root, path, record):
    tree = candidate_tree(root)
    inputs = record['inputs']
    upstream_rows = changed_paths(root, inputs['B'], inputs['U'])
    local_rows = changed_paths(root, inputs['H'], tree)
    candidate_rows = changed_paths(root, inputs['U'], tree)
    all_paths = sorted({path
                        for row in upstream_rows + local_rows + candidate_rows
                        for path in (row['path'], row.get('from'))
                        if path})
    contracts = load_manifest(root)
    impacted = [
        {'id': item.get('id'), 'level': item.get('level'), 'status': item.get('status'),
         'tests': [test.get('id') for test in item.get('tests', []) if isinstance(test, dict)]}
        for item in contracts if contract_matches(item, all_paths)
    ]
    domains = {}
    for item in upstream_rows + local_rows + candidate_rows:
        for changed_path in (item['path'], item.get('from')):
            if changed_path:
                domains.setdefault(domain_for(changed_path), set()).add(changed_path)
    domain_rows = [{'id': ident, 'paths': sorted(paths)} for ident, paths in sorted(domains.items())]
    required = ['impact-review']
    if any(item['level'] == 'P0' for item in impacted):
        required.append('contracts-p0')
    kinds = {test.get('kind') for item in contracts if contract_matches(item, all_paths)
             for test in item.get('tests', []) if isinstance(test, dict)}
    if 'rust' in kinds or 'server' in domains:
        required.append('server')
    if 'node' in kinds or 'browser' in kinds or 'frontend-source' in domains or 'generated-artifact' in domains:
        required.append('frontend')
    if 'generated-artifact' in domains:
        required.append('generated-artifacts')
    plan_data = {
        'version': 1,
        'candidate_tree': tree,
        'inputs': dict(inputs),
        'upstream_paths': upstream_rows,
        'local_paths': local_rows,
        'candidate_paths': candidate_rows,
        'domains': domain_rows,
        'impacted_contracts': impacted,
        'required_gates': list(dict.fromkeys(required)),
        'generated_at': time.time(),
    }
    plan_data['digest'] = digest(json.dumps(plan_data, ensure_ascii=False, sort_keys=True).encode('utf-8'))
    record['plan'] = plan_data
    set_phase(record, 'planned', 'resolve first impacted domain', plan_digest=plan_data['digest'])
    output = path.parent / 'plan.json'
    output.write_text(json.dumps(plan_data, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    event(record, 'plan', path=str(output), required_gates=plan_data['required_gates'])
    write_record(path, record)
    print(f'PLAN: tree={tree}; domains={len(domain_rows)}; contracts={len(impacted)}; gates={" ".join(plan_data["required_gates"])}')


def status(root, path, record=None):
    if record is None:
        record = read_record(root, path)
    tree = None
    tree_error = None
    try:
        tree = candidate_tree(root)
    except (ValueError, subprocess.CalledProcessError) as error:
        tree_error = str(error)
    pending = [receipt.get('name') for receipt in record.get('receipts', []) if receipt.get('exit') is None]
    result = {
        'phase': record.get('phase', 'unknown'),
        'next_action': record.get('next_action'),
        'branch': branch(root),
        'merge_head': commit(root, 'MERGE_HEAD') if subprocess.run(
            ['git', 'rev-parse', '-q', '--verify', 'MERGE_HEAD'], cwd=root,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0 else None,
        'candidate_tree': tree,
        'tree_error': tree_error,
        'pending_receipts': pending,
        'receipt_count': len(record.get('receipts', [])),
        'required_gates': (record.get('plan') or {}).get('required_gates', record.get('seal', {}).get('required', []) if record.get('seal') else []),
        'last_successful_command': record.get('last_successful_command'),
        'last_successful_receipt': record.get('last_successful_receipt'),
        'last_failure': record.get('last_failure'),
        'tracking': tracking_ref(root),
        'inputs': record.get('inputs'),
    }
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return result


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
    set_phase(record, 'impact', 'generate plan')
    record['last_successful_command'] = 'git diff --binary B..U; H..candidate; U..candidate'
    event(record, 'impact', tree=tree, reports=[item['path'] for item in reports])
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
    ensure_state(record)
    tree = candidate_tree(root)
    sequence = len(record['receipts']) + 1
    prefix = path.parent / f'{sequence:03d}-{name}'
    start = time.time()
    receipt = {'name': name, 'candidate_tree': tree, 'argv': argv, 'cwd': str(directory),
               'started_at': start, 'exit': None, 'source_unchanged': False, 'status': 'running'}
    record['receipts'].append(receipt)
    record['seal'] = None
    set_phase(record, 'verifying', f'wait for {name}', last_successful_receipt=record.get('last_successful_receipt'))
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
               'stdout_sha256': digest(result.stdout), 'stderr_sha256': digest(result.stderr),
               'status': 'passed' if result.returncode == 0 and unchanged else 'failed'})
    if result.returncode == 0 and unchanged:
        record['last_successful_command'] = ' '.join(map(str, argv))
        record['last_successful_receipt'] = name
        record['next_action'] = 'run next required gate or seal'
        event(record, 'receipt_passed', name=name, tree=tree)
    else:
        record_failure(record, 'GATE_FAILED', f'{name} exit={result.returncode} source_unchanged={unchanged}',
                       result.returncode != 0, f'fix or rerun {name}')
    write_record(path, record)
    print(f'RECEIPT: {name}; tree={tree}; exit={result.returncode}; source_unchanged={unchanged}')
    return result.returncode if unchanged else 1


def execute_batch(root, path, record, tasks):
    """Run independent gates concurrently while keeping one record writer."""
    ensure_state(record)
    if not tasks or not isinstance(tasks, list):
        raise ValueError('批量验收任务不能为空')
    if any(not isinstance(task, dict) for task in tasks):
        raise ValueError('批量验收任务必须是对象')
    names = [task.get('name') for task in tasks if isinstance(task, dict)]
    if len(names) != len(set(names)) or any(not isinstance(name, str) or not re.fullmatch(r'[a-z][a-z0-9-]*', name) for name in names):
        raise ValueError('批量验收名称必须唯一且符合格式')
    tree = candidate_tree(root)
    start_sequence = len(record['receipts']) + 1
    prepared = []
    for offset, task in enumerate(tasks):
        argv = task.get('argv')
        cwd = task.get('cwd', '.')
        if not isinstance(cwd, str):
            raise ValueError(f'批量验收 cwd 无效: {task.get("name")}')
        if not isinstance(argv, list) or not argv or any(not isinstance(item, str) for item in argv):
            raise ValueError(f'批量验收 argv 无效: {task.get("name")}')
        directory = (root / cwd).resolve()
        if not directory.is_relative_to(root) or not directory.is_dir():
            raise ValueError(f'执行目录越过仓库或不存在: {cwd}')
        prefix = path.parent / f'{start_sequence + offset:03d}-{task["name"]}'
        receipt = {'name': task['name'], 'candidate_tree': tree, 'argv': argv,
                   'cwd': str(directory), 'started_at': time.time(), 'exit': None,
                   'source_unchanged': False, 'status': 'running'}
        record['receipts'].append(receipt)
        prepared.append((task, receipt, prefix, directory))
    record['seal'] = None
    set_phase(record, 'verifying', 'wait for batch gates')
    write_record(path, record)

    def run_one(item):
        task, receipt, prefix, directory = item
        executable = shutil.which(task['argv'][0]) or task['argv'][0]
        try:
            result = subprocess.run([executable, *task['argv'][1:]], cwd=directory, capture_output=True)
        except OSError as error:
            result = subprocess.CompletedProcess(task['argv'], -1, b'', str(error).encode('utf-8'))
        stdout, stderr = prefix.with_suffix('.stdout'), prefix.with_suffix('.stderr')
        stdout.write_bytes(result.stdout)
        stderr.write_bytes(result.stderr)
        return task['name'], receipt, result, executable, stdout, stderr

    results = []
    with ThreadPoolExecutor(max_workers=min(8, len(prepared))) as pool:
        futures = [pool.submit(run_one, item) for item in prepared]
        for future in as_completed(futures):
            results.append(future.result())
    order = {name: index for index, name in enumerate(names)}
    results.sort(key=lambda item: order[item[0]])
    try:
        read_record(root, path)
        unchanged = candidate_tree(root) == tree
    except (ValueError, subprocess.CalledProcessError):
        unchanged = False
    passed = 0
    for name, receipt, result, executable, stdout, stderr in results:
        ok = result.returncode == 0 and unchanged
        receipt.update({'finished_at': time.time(), 'exit': result.returncode,
                        'source_unchanged': unchanged, 'executable': executable,
                        'stdout': str(stdout), 'stderr': str(stderr),
                        'stdout_sha256': digest(result.stdout), 'stderr_sha256': digest(result.stderr),
                        'status': 'passed' if ok else 'failed'})
        if ok:
            passed += 1
            record['last_successful_receipt'] = name
            event(record, 'receipt_passed', name=name, tree=tree, batch=True)
        else:
            record_failure(record, 'GATE_FAILED', f'{name} exit={result.returncode} source_unchanged={unchanged}',
                           result.returncode != 0, f'fix or rerun {name}')
    if passed == len(results):
        record['last_successful_command'] = 'run-batch'
        record['next_action'] = 'run next required gate or seal'
    write_record(path, record)
    print(f'BATCH: tree={tree}; passed={passed}/{len(results)}; source_unchanged={unchanged}')
    return 0 if passed == len(results) else 1


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


def validate_plan(record, tree):
    plan_data = record.get('plan')
    if plan_data is None:
        return
    if plan_data.get('candidate_tree') != tree:
        raise ValueError('合并计划不是当前候选树；重新运行 plan')
    expected = dict(plan_data)
    actual_digest = expected.pop('digest', None)
    if actual_digest:
        recalculated = digest(json.dumps(expected, ensure_ascii=False, sort_keys=True).encode('utf-8'))
        if recalculated != actual_digest:
            raise ValueError('合并计划摘要已改变；重新运行 plan')


def seal(root, path, record, required):
    ensure_state(record)
    if not required:
        required = (record.get('plan') or {}).get('required_gates', [])
    tree = candidate_tree(root)
    validate_impact(record, tree)
    validate_plan(record, tree)
    validate_receipts(record, tree, required)
    record['seal'] = {'candidate_tree': tree, 'required': required, 'sealed_at': time.time()}
    set_phase(record, 'sealed', 'check candidate or commit', sealed_tree=tree)
    event(record, 'sealed', tree=tree, required=required)
    write_record(path, record)
    print(f'SEALED: tree={tree}; receipts={len(required)}')


def check(root, record, ref=None):
    if not record['seal']:
        raise ValueError('尚无封存验收')
    actual = git(root, 'rev-parse', '--verify', '--end-of-options', ref + '^{tree}').decode().strip() if ref else candidate_tree(root)
    if actual != record['seal']['candidate_tree']:
        raise ValueError('候选或提交树与已验收输入不一致；重跑受影响验收')
    validate_impact(record, actual)
    validate_plan(record, actual)
    validate_receipts(record, actual, record['seal']['required'])
    if ref:
        inputs = record['inputs']
        for label in ('H', 'U'):
            subprocess.run(['git', 'merge-base', '--is-ancestor', inputs[label], ref], cwd=root, check=True)
        if subprocess.run(['git', 'rev-parse', '-q', '--verify', 'MERGE_HEAD'], cwd=root,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
            raise ValueError('提交检查时仍存在 MERGE_HEAD')
        record['checked_commit'] = commit(root, ref)
        record['checked_tree'] = actual
        record['last_successful_command'] = f'check --commit {ref}'
        set_phase(record, 'checked', 'delivery authorized')
    print(f'MATCH: tree={actual}' + (f'; commit={commit(root, ref)}' if ref else ''))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--record', type=Path, required=True)
    actions = parser.add_subparsers(dest='action', required=True)
    actions.add_parser('freeze').add_argument('--upstream', required=True)
    actions.add_parser('impact')
    actions.add_parser('status')
    actions.add_parser('plan')
    run = actions.add_parser('run')
    run.add_argument('--name', required=True)
    run.add_argument('--cwd', default='.')
    run.add_argument('argv', nargs=argparse.REMAINDER)
    batch = actions.add_parser('run-batch')
    batch.add_argument('--tasks', type=Path, required=True, help='JSON 数组：[{"name","cwd","argv"}]')
    actions.add_parser('seal').add_argument('--required', nargs='*')
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
            elif args.action == 'status':
                status(root, path, record)
            elif args.action == 'plan':
                plan(root, path, record)
            elif args.action == 'run':
                argv = args.argv[1:] if args.argv[:1] == ['--'] else args.argv
                return execute(root, path, record, args.name, args.cwd, argv)
            elif args.action == 'run-batch':
                tasks = json.loads(args.tasks.resolve().read_text(encoding='utf-8'))
                return execute_batch(root, path, record, tasks)
            elif args.action == 'seal':
                seal(root, path, record, args.required)
            else:
                check(root, record, args.commit)
                write_record(path, record)
            return 0
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        if path.exists() and args.action != 'freeze':
            try:
                record = json.loads(path.read_text(encoding='utf-8'))
                ensure_state(record)
                code = 'REF_VALIDATION' if isinstance(error, subprocess.CalledProcessError) else 'FLOW_ERROR'
                record_failure(record, code, str(error), False, 'inspect status and resolve the reported condition')
                write_record(path, record)
            except (OSError, ValueError, json.JSONDecodeError):
                pass
        print(f'FAIL: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
