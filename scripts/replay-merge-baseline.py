#!/usr/bin/env python3
"""冻结事件保护基线；在隔离候选副本中按原路径重放独立测试。仅使用标准库。"""
import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile

MANIFEST = 'docs/local-contracts.json'
RUST_TESTS = 'desktop-tauri/src-tauri/server/tests'
CHECKER = 'scripts/check-merge-contracts.py'
SELF_TEST = 'scripts/test_check_merge_contracts.py'


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root)


def resolve_base(root, event_name, event, explicit=''):
    if git(root, 'rev-parse', '--is-shallow-repository').strip() != b'false':
        raise ValueError('需要完整历史；checkout 必须使用 fetch-depth: 0')
    base = explicit or (event['pull_request']['base']['sha'] if event_name == 'pull_request'
                        else event.get('before', '') if event_name == 'push' else '')
    if not base or set(base) == {'0'}:
        base = 'HEAD^1'
    if base.startswith('-'):
        raise ValueError('无效基线引用')
    sha = git(root, 'rev-parse', '--verify', '--end-of-options', base + '^{commit}').decode().strip()
    if sha == git(root, 'rev-parse', 'HEAD').decode().strip():
        raise ValueError('基线不能等于候选 HEAD')
    subprocess.run(['git', 'merge-base', '--is-ancestor', sha, 'HEAD'], cwd=root, check=True)
    return sha


def paths_at(root, ref, prefix):
    return git(root, 'ls-tree', '-r', '--name-only', '-z', ref, '--', prefix).decode().split('\0')[:-1]


def destination(root, name):
    relative = PurePosixPath(name)
    if relative.is_absolute() or '..' in relative.parts or '\\' in name or ':' in name:
        raise ValueError(f'无效仓库路径: {name}')
    path = root / name
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError(f'路径越过副本边界: {name}')
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def copyable(name):
    parts = PurePosixPath(name).parts
    return not (any(p in ('.git', '.omx', '.verify', 'diagnostic-artifacts', 'runtime', 'target', 'node_modules', 'Acankao', 'backups') for p in parts)
                or any(p.startswith('.env') or p.endswith(('.key', '.pem', '.sqlite', '.sqlite3', '.db')) for p in parts))


def snapshot(root, target, manifest):
    names = set(git(root, 'ls-files', '--cached', '-z').decode().split('\0')[:-1])
    names.update([MANIFEST, CHECKER, SELF_TEST])
    names.update(name for c in manifest['contracts'] for name in c['implementation'])
    names.update(t['path'] for c in manifest['contracts'] for t in c['tests'])
    # 新建立的独立用例和 SQL 样本可能尚未进入 Git。
    names.update(p.relative_to(root).as_posix() for p in (root / RUST_TESTS).rglob('*') if p.is_file())
    for name in sorted(names):
        if not copyable(name):
            continue
        source = root / name
        if source.is_file() and not source.is_symlink():
            shutil.copy2(source, destination(target, name))
    for directory in ('desktop-tauri/ui-kit', 'desktop-tauri/ui-islands'):
        modules = root / directory / 'node_modules'
        if modules.is_dir():
            destination(target, directory + '/node_modules').symlink_to(modules, target_is_directory=True)


def overlay(root, target, ref, names):
    for name in names:
        destination(target, name).write_bytes(git(root, 'show', f'{ref}:{name}'))


def run(command, cwd, report=None):
    print('+ ' + ' '.join(map(str, command)), flush=True)
    if report is None:
        subprocess.run(command, cwd=cwd, check=True)
    else:
        result = subprocess.run(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        sys.stdout.buffer.write(result.stdout)
        sys.stderr.buffer.write(result.stderr)
        with report.open('ab') as output:
            output.write(result.stdout)
        with report.with_name(report.name + '.stderr').open('ab') as output:
            output.write(result.stderr)
        result.check_returncode()


def candidate_tests(root, kind, report=None):
    report = report.resolve() if report is not None else None
    manifest = json.loads((root / MANIFEST).read_text(encoding='utf-8'))
    names = {t['path'] for c in manifest['contracts'] if c.get('status') != '已批准替代/移除'
             for t in c['tests'] if t['kind'] == kind}
    if kind == 'node':
        names.update(p.relative_to(root).as_posix() for pattern in ('scripts/*.test.cjs', 'desktop-tauri/ui-islands/tests/*.test.*') for p in root.glob(pattern) if p.is_file())
    names = sorted(names)
    for name in names:
        if not (root / name).is_file():
            raise ValueError(f'候选测试文件缺失: {name}')
        destination(root, name)  # 复用仓库边界校验；现有文件不会创建新目录。
    if kind == 'node':
        if not names or report is None:
            raise ValueError('候选 Node 测试集合和报告输出路径必须非空')
        report.write_bytes(b'')
        report.with_name(report.name + '.stderr').write_bytes(b'')
        run(['node', '--test', '--test-reporter=tap', '--', *names], root, report)
    else:
        for name in names:
            run(['node', '--', name], root)


def checker_at(root):
    spec = importlib.util.spec_from_file_location('merge_replay_gate', root / CHECKER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def frozen_gate(root, ref, directory, reports=(), verification_root=None, manifest=None):
    shutil.copy2(root / CHECKER, destination(directory, CHECKER))
    if paths_at(root, ref, SELF_TEST):
        overlay(root, directory, ref, [SELF_TEST])
        print('H_GATE_NEGATIVE_REPLAY: H 原始负例针对当前检查器，不是 H 旧检查器通过。', flush=True)
        run([sys.executable, '-B', '-m', 'unittest', 'discover', '-s', str(directory / 'scripts'), '-p', 'test_check_merge_contracts.py', '-v'], root)
    else:
        print('BOOTSTRAP: H 尚无门禁负例；本轮须审查首次覆盖。', flush=True)
    options = ['--base-ref', ref] if verification_root is None else []
    if manifest is not None:
        options += ['--manifest', str(manifest)]
    run([sys.executable, str(directory / CHECKER), '--root', str(verification_root or root), *options, *reports], root)


def domain_view(view, kind, inline=False):
    result = copy.deepcopy(view)
    for contract in result['contracts']:
        contract['tests'] = [t for t in contract['tests'] if (bool(t.get('baseline_inline')) if inline else (t.get('baseline_independent') or not t.get('baseline_inline')))
                             and t['kind'] in (('rust',) if kind == 'rust' else ('node', 'browser'))]
    result['contracts'] = [c for c in result['contracts'] if c['tests']]
    return result


def replay_patches(root, ref, baseline, candidate, kind):
    implementations = {name for manifest in (baseline, candidate) for c in manifest['contracts'] for name in c['implementation']}
    allowed = {t['path']: 'rust' if t['kind'] == 'rust' else 'frontend'
               for c in baseline['contracts'] for t in c['tests']
               if t['kind'] in ('node', 'browser') or (t['kind'] == 'rust' and t['path'].startswith(RUST_TESTS + '/'))}
    allowed = {name: scope for name, scope in allowed.items() if name not in implementations
               and not {'fixture', 'fixtures'}.intersection(PurePosixPath(name).parts)}
    declarations = candidate.get('replay_patches', [])
    if not isinstance(declarations, list):
        raise ValueError('replay_patches 必须为列表')
    selected = []
    for declaration in declarations:
        if not isinstance(declaration, dict) or any(not isinstance(declaration.get(k), str) or not declaration[k].strip() for k in ('path', 'reason', 'review')):
            raise ValueError('重放补丁须声明 path/reason/review')
        patch = root / declaration['path']
        if not patch.is_file():
            raise ValueError('重放补丁文件缺失')
        destination(root, declaration['path'])
        files = declaration.get('files', [])
        if not isinstance(files, list) or not files:
            raise ValueError('补丁 files 必须非空')
        names = set()
        for item in files:
            name = item.get('path') if isinstance(item, dict) else None
            if name not in allowed or name in names:
                raise ValueError(f'补丁只能修改不重复的 H 独立测试，禁止生产/样本文件: {name}')
            if item.get('sha256') != hashlib.sha256(git(root, 'show', f'{ref}:{name}')).hexdigest():
                raise ValueError(f'H 测试 SHA256 不匹配: {name}')
            names.add(name)
        if git(root, 'apply', '--summary', '--', str(patch)).strip():
            raise ValueError('补丁只能修改现有测试内容，禁止新增/删除/重命名/模式变化')
        records = git(root, 'apply', '--numstat', '-z', '--', str(patch)).split(b'\0')[:-1]
        if any(record.startswith(b'-\t') for record in records):
            raise ValueError('禁止二进制补丁')
        actual = {record.split(b'\t', 2)[2].decode() for record in records}
        if actual != names:
            raise ValueError('补丁实际路径与 files 声明不一致')
        scopes = {allowed[name] for name in names}
        if kind in scopes:
            selected.append((patch, names))
    return selected


def execute_replay(root, kind, baseline, directory, report):
    report.write_bytes(b'')
    report.with_name(report.name + '.stderr').write_bytes(b'')
    for suffix in ('.execution', '.browser', '.gate'):
        report.with_name(report.name + suffix).write_bytes(b'')
        report.with_name(report.name + suffix + '.stderr').write_bytes(b'')
    if kind == 'rust':
        names = {t['path'] for c in baseline['contracts'] for t in c['tests'] if t['kind'] == 'rust'}
        target_files = {p.relative_to(directory).as_posix(): p.stem for p in (directory / RUST_TESTS).glob('*.rs')}
        if not names.issubset(target_files):
            raise ValueError('H 独立 Rust 用例未对应自动发现的 integration target，须明确入口适配: ' + ', '.join(sorted(names - target_files.keys())))
        targets = baseline['baseline_rust_targets']
        workspace = directory / 'desktop-tauri/src-tauri'
        # 共享 job target 时强制候选副本重新编译，不复用其他源码树的测试二进制。
        library = workspace / 'server/src/lib.rs'
        if library.is_file():
            library.touch()
        command = ['cargo', 'test', '-p', 'agent2api-server', '--locked']
        if not targets:
            print('BASELINE: 本域无 H 独立 Rust target；内联测试仅按候选实际证据验证。')
            return
        command += [part for target in targets for part in ('--test', target)]
        run([*command, '--', '--list'], workspace, report)
        run(command, workspace, report.with_name(report.name + '.execution'))
    else:
        tests = sorted({(t['kind'], t['path']) for c in baseline['contracts'] for t in c['tests'] if t['kind'] in ('node', 'browser')})
        node = [name for test_kind, name in tests if test_kind == 'node']
        if node:
            run(['node', '--test', '--test-reporter=tap', '--', *node], directory, report)
        try:
            for test_kind, name in tests:
                if test_kind == 'browser':
                    run(['node', '--', name], directory, report.with_name(report.name + '.browser'))
        finally:
            if (directory / '.verify').is_dir():
                shutil.copytree(directory / '.verify', root / '.verify' / report.name, dirs_exist_ok=True)
    options = []
    if kind == 'rust' or any(t['kind'] == 'node' for c in baseline['contracts'] for t in c['tests']):
        options = ['--rust-list' if kind == 'rust' else '--node-report', str(report)]
    run([sys.executable, str(root / CHECKER), '--root', str(directory), *options], root, report.with_name(report.name + '.gate'))


def replay(root, ref, kind, directory, report):
    report = report.resolve()
    if not paths_at(root, ref, MANIFEST):
        print('BOOTSTRAP: 基线尚无保护清单；不宣称完成原始断言重放。')
        return False
    baseline = json.loads(git(root, 'show', f'{ref}:{MANIFEST}'))
    candidate = json.loads((root / MANIFEST).read_text(encoding='utf-8'))
    view = domain_view(checker_at(root).baseline_view(baseline, candidate), kind)
    if not view['contracts']:
        print(f'BASELINE_DOMAIN_SKIPPED: {kind} 无活跃 H 独立测试义务；退休审计仍由当前门禁保留。', flush=True)
        replay_patches(root, ref, baseline, candidate, kind)
        return False
    snapshot(root, directory, candidate)
    if kind == 'rust':
        names = paths_at(root, ref, RUST_TESTS)
        # 清单映射不是测试筛选器：H 已有的独立 target 全部重放，不纳入 C 新 target。
        view['baseline_rust_targets'] = sorted(PurePosixPath(name).stem for name in names
                                              if PurePosixPath(name).parent.as_posix() == RUST_TESTS and name.endswith('.rs'))
    else:
        parents = {PurePosixPath(t['path']).parent.as_posix() for c in view['contracts'] for t in c['tests'] if t['kind'] in ('node', 'browser')}
        implementations = {name for manifest in (baseline, candidate) for c in manifest['contracts'] for name in c['implementation']}
        names = {name for parent in parents for name in paths_at(root, ref, parent)} - implementations
    overlay(root, directory, ref, sorted(name for name in names if copyable(name)))
    (directory / MANIFEST).write_text(json.dumps(view, ensure_ascii=False), encoding='utf-8')
    original = report.with_name(report.name + '.original')
    failure = None
    print(f'ORIGINAL_REPLAY: base={ref}; candidate-source={directory}', flush=True)
    try:
        execute_replay(root, kind, view, directory, original)
    except subprocess.CalledProcessError as error:
        failure = error
    original.with_name(original.name + '.status.json').write_text(json.dumps({'label': 'ORIGINAL_REPLAY', 'base': ref, 'exit': failure.returncode if failure else 0, 'command': failure.cmd if failure else None}), encoding='utf-8')
    shutil.copyfile(original, report)
    patches = replay_patches(root, ref, baseline, candidate, kind)
    if failure is not None:
        if not patches:
            raise failure
        overlay(root, directory, ref, sorted({name for _, names in patches for name in names}))
        for patch, _ in patches:
            run(['git', 'apply', '--check', '--', str(patch)], directory)
            run(['git', 'apply', '--apply', '--', str(patch)], directory)
        adapted = report.with_name(report.name + '.adapted')
        print(f'ADAPTED_REPLAY: 原样失败已保留；仅应用 {len(patches)} 个声明的测试接线补丁。', flush=True)
        adapted_failure = None
        try:
            execute_replay(root, kind, view, directory, adapted)
        except subprocess.CalledProcessError as error:
            adapted_failure = error
            raise
        finally:
            if adapted.is_file():
                shutil.copyfile(adapted, report)
            adapted.with_name(adapted.name + '.status.json').write_text(json.dumps({
                'label': 'ADAPTED_REPLAY', 'base': ref, 'exit': adapted_failure.returncode if adapted_failure else 0,
                'command': adapted_failure.cmd if adapted_failure else None,
                'patches': [{'path': patch.relative_to(root).as_posix(), 'sha256': hashlib.sha256(patch.read_bytes()).hexdigest(), 'files': sorted(names)} for patch, names in patches],
            }), encoding='utf-8')
    return bool(view['contracts'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('kind', choices=('resolve', 'gate', 'rust', 'frontend', 'candidate-node', 'candidate-browser'))
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--base-ref', default=os.environ.get('CONTRACT_BASE', ''))
    parser.add_argument('--report', type=Path, help='运行报告输出文件，清空后重建；候选与 H 重放必须使用独立文件')
    parser.add_argument('--candidate-rust-list', type=Path, help='已成功执行全量候选 Rust 后的独立编译列表；只用于 H 内联义务映射验证')
    args = parser.parse_args()
    if args.report is not None:
        args.report = args.report.resolve()
    root = args.root.resolve()
    if args.kind.startswith('candidate-'):
        candidate_tests(root, args.kind.removeprefix('candidate-'), args.report)
        return
    if args.kind == 'resolve':
        event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text(encoding='utf-8'))
        sha = resolve_base(root, os.environ['GITHUB_EVENT_NAME'], event, args.base_ref)
        print(f'保护基线（实际本地合并 H 由本地另行冻结）: {sha}')
        with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as output:
            output.write(f'base={sha}\n')
        return
    with tempfile.TemporaryDirectory(prefix='merge-baseline-') as temp:
        directory = Path(temp)
        candidate = None
        if args.kind != 'gate':
            if args.report is None:
                parser.error('重放须提供独立 --report 输出，以便当前检查器校验 H 独立运行证据')
            if replay(root, args.base_ref, args.kind, directory / 'candidate', args.report):
                candidate = directory / 'candidate'
        reports = []
        if candidate is not None:
            view = json.loads((candidate / MANIFEST).read_text(encoding='utf-8'))
            if args.kind == 'rust' or any(t['kind'] == 'node' for c in view['contracts'] for t in c['tests']):
                reports = ['--rust-list' if args.kind == 'rust' else '--node-report', str(args.report.resolve())]
        frozen_gate(root, args.base_ref, directory / 'gate', reports, candidate)
        if args.kind == 'rust' and paths_at(root, args.base_ref, MANIFEST):
            baseline = json.loads(git(root, 'show', f'{args.base_ref}:{MANIFEST}'))
            view = checker_at(root).baseline_view(baseline, json.loads((root / MANIFEST).read_text(encoding='utf-8')))
            inline = domain_view(view, 'rust', inline=True)
            if inline['contracts']:
                if args.candidate_rust_list is None:
                    parser.error('H 内联义务须提供已成功执行候选的 --candidate-rust-list；不是原始内联断言重放')
                manifest = directory / 'inline-view.json'
                manifest.write_text(json.dumps(inline, ensure_ascii=False), encoding='utf-8')
                print('CANDIDATE_INLINE_VALIDATION: H 内联义务使用声明后的候选入口和独立实际列表，不是原始断言重放。', flush=True)
                run([sys.executable, str(root / CHECKER), '--root', str(root), '--manifest', str(manifest), '--rust-list', str(args.candidate_rust_list.resolve())], root)


if __name__ == '__main__':
    main()
