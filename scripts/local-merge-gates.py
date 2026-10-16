#!/usr/bin/env python3
"""运行 AI 合入的本地权威门禁；不访问 GitHub，不修改受控源码。"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def command_name(name):
    if os.name == 'nt' and name == 'npm':
        return 'npm.cmd'
    return name


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root)


def python_script(name, *args):
    return [sys.executable, name, *args]


def report_path(report_dir, name):
    return report_dir / (name + '.txt')


def command_matrix(root, base_ref, report_dir):
    """Return the serial contract gate and independent server/frontend gates."""
    base = str(base_ref)
    node_report = report_dir / 'candidate-node.tap'
    rust_report = report_dir / 'candidate-rust.list'
    return {
        'contracts': [
            ('merge-tooling', python_script('-B', '-m', 'unittest', 'discover', '-s', 'scripts',
                                            '-p', 'test_*merge*.py', '-v')),
            ('integration-closure', python_script('scripts/check-merge-closure.py',
                                                  '--base-ref', base,
                                                  '--report', str(report_dir / 'integration-closure.json'))),
            ('version-consistency', python_script('scripts/check-version-consistency.py')),
            ('contract-manifest', python_script('scripts/check-merge-contracts.py', '--base-ref', base)),
            ('baseline-gate', python_script('scripts/replay-merge-baseline.py', 'gate', '--base-ref', base)),
        ],
        'server': [
            ('cargo-check', ['cargo', 'check', '--manifest-path', 'desktop-tauri/src-tauri/Cargo.toml',
                             '-p', 'agent2api-server', '--locked']),
            ('cargo-test', ['cargo', 'test', '--manifest-path', 'desktop-tauri/src-tauri/Cargo.toml',
                            '-p', 'agent2api-server', '--locked']),
            ('rust-list', ['cargo', 'test', '--manifest-path', 'desktop-tauri/src-tauri/Cargo.toml',
                           '-p', 'agent2api-server', '--locked', '--', '--list']),
            ('rust-contracts', python_script('scripts/check-merge-contracts.py', '--base-ref', base,
                                             '--rust-list', str(rust_report))),
            ('baseline-rust', python_script('scripts/replay-merge-baseline.py', 'rust', '--base-ref', base,
                                            '--report', str(report_dir / 'baseline-rust.txt'),
                                            '--candidate-rust-list', str(rust_report))),
        ],
        'frontend': [
            ('candidate-node', python_script('scripts/replay-merge-baseline.py', 'candidate-node',
                                             '--report', str(node_report))),
            ('node-contracts', python_script('scripts/check-merge-contracts.py', '--base-ref', base,
                                             '--node-report', str(node_report))),
            ('ui-kit-typecheck', [command_name('npm'), '--prefix', 'desktop-tauri/ui-kit', 'run', 'typecheck']),
            ('ui-islands-typecheck', [command_name('npm'), '--prefix', 'desktop-tauri/ui-islands', 'run', 'typecheck']),
            ('ui-islands-build', [command_name('npm'), '--prefix', 'desktop-tauri/ui-islands', 'run', 'build']),
            ('generated-diff', ['git', 'diff', '--exit-code', '--', 'desktop-tauri/ui/islands/ui.js',
                                'desktop-tauri/ui/islands/ui.css']),
            ('baseline-frontend', python_script('scripts/replay-merge-baseline.py', 'frontend',
                                                '--base-ref', base,
                                                '--report', str(report_dir / 'baseline-frontend.txt'))),
            ('candidate-browser', python_script('scripts/replay-merge-baseline.py', 'candidate-browser')),
        ],
    }


def run_step(root, report_dir, group, name, argv):
    cwd = root
    if group == 'server' and name == 'rust-list':
        output = report_dir / 'candidate-rust.list'
    else:
        output = report_path(report_dir, f'{group}-{name}')
    print(f'[{group}] {name}: +' + ' '.join(map(str, argv)), flush=True)
    try:
        executable = shutil.which(argv[0]) or argv[0]
        result = subprocess.run([executable, *argv[1:]], cwd=cwd,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    except OSError as error:
        result = subprocess.CompletedProcess(argv, 127, b'', str(error).encode('utf-8'))
    output.write_bytes(result.stdout)
    output.with_name(output.name + '.stderr').write_bytes(result.stderr)
    sys.stdout.buffer.write(result.stdout)
    sys.stderr.buffer.write(result.stderr)
    sys.stdout.flush()
    sys.stderr.flush()
    if result.returncode:
        print(f'[{group}] {name}: FAIL exit={result.returncode}', flush=True)
    else:
        print(f'[{group}] {name}: PASS', flush=True)
    return {'group': group, 'name': name, 'exit': result.returncode,
            'stdout': str(output), 'stderr': str(output.with_name(output.name + '.stderr'))}


def run_group(root, report_dir, group, steps):
    results = []
    for name, argv in steps:
        result = run_step(root, report_dir, group, name, argv)
        results.append(result)
        if result['exit']:
            break
    return results


def validate_base(root, base_ref, candidate):
    if git(root, 'rev-parse', '--is-shallow-repository').strip() != b'false':
        raise ValueError('需要完整 Git 历史；先补齐 shallow clone')
    base = git(root, 'rev-parse', '--verify', '--end-of-options', base_ref + '^{commit}').decode().strip()
    subprocess.run(['git', 'merge-base', '--is-ancestor', base, 'HEAD'], cwd=root, check=True)
    head = git(root, 'rev-parse', 'HEAD').decode().strip()
    head_tree = git(root, 'rev-parse', 'HEAD^{tree}').decode().strip()
    if base == head and head_tree == candidate:
        raise ValueError('本地保护基线不能与候选树相同')
    return base


def candidate_tree(root):
    if subprocess.run(['git', 'diff-files', '--quiet', '--'], cwd=root).returncode != 0:
        raise ValueError('候选存在未暂存修改；先暂存候选源码')
    if git(root, 'diff-files', '--name-only', '--diff-filter=U').strip():
        raise ValueError('候选存在未解决冲突')
    unknown = [item for item in git(root, 'ls-files', '--others', '--exclude-standard', '-z')
               .decode().split('\0') if item and not item.startswith(('.verify/', 'diagnostic-artifacts/'))]
    if unknown:
        raise ValueError('候选存在未纳入的文件: ' + ', '.join(unknown))
    return git(root, 'write-tree').decode().strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--base-ref', required=True, help='合并前本地 H；不能使用候选 HEAD')
    parser.add_argument('--report-dir', type=Path, help='证据目录，默认写入 .verify/local-merge-gates')
    args = parser.parse_args()
    root = args.root.resolve()
    before_tree = candidate_tree(root)
    base = validate_base(root, args.base_ref, before_tree)
    report_dir = (args.report_dir or root / '.verify' / 'local-merge-gates' /
                  datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')).resolve()
    report_dir.mkdir(parents=True, exist_ok=True)
    matrix = command_matrix(root, base, report_dir)
    all_results = []

    contract_results = run_group(root, report_dir, 'contracts', matrix['contracts'])
    all_results.extend(contract_results)
    if any(item['exit'] for item in contract_results):
        print('LOCAL_GATES: contracts failed; server/frontend gates were not started', file=sys.stderr)
        return 1

    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = {pool.submit(run_group, root, report_dir, group, matrix[group]): group
                   for group in ('server', 'frontend')}
        for future in as_completed(futures):
            all_results.extend(future.result())
    all_results.sort(key=lambda item: (item['group'], item['name']))
    after_tree = candidate_tree(root)
    if before_tree != after_tree:
        print(f'LOCAL_GATES: FAIL candidate tree changed {before_tree} -> {after_tree}', file=sys.stderr)
        return 1
    summary = {'base_ref': base, 'candidate_tree': after_tree,
               'report_dir': str(report_dir), 'results': all_results,
               'passed': sum(item['exit'] == 0 for item in all_results),
               'failed': sum(item['exit'] != 0 for item in all_results)}
    (report_dir / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n',
                                              encoding='utf-8')
    if summary['failed']:
        print(f"LOCAL_GATES: FAIL {summary['failed']} failed; evidence={report_dir}", file=sys.stderr)
        return 1
    print(f"LOCAL_GATES: PASS {summary['passed']} gates; base={base}; evidence={report_dir}")
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f'LOCAL_GATES: FAIL {error}', file=sys.stderr)
        sys.exit(1)
