#!/usr/bin/env python3
"""检查唯一能力清单和测试入口；不把静态存在性当成业务验证。仅使用标准库。"""
import argparse
import copy
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath

MANIFEST = 'docs/local-contracts.json'
NODE_STRINGS = r'"(?:\\.|[^"\\])*"' + r"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`"


def read_json(text):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f'重复 JSON 键: {key}')
            result[key] = value
        return result
    return json.loads(text, object_pairs_hook=unique)


def file_at(root, name):
    if not isinstance(name, str) or not name or '\\' in name or ':' in name:
        raise ValueError(f'无效仓库路径: {name!r}')
    relative = PurePosixPath(name)
    if relative.is_absolute() or '..' in relative.parts or relative.as_posix() != name:
        raise ValueError(f'无效仓库路径: {name}')
    if root is None:
        return None  # 历史清单只校验结构；不要求历史文件仍在候选中。
    path = (root / name).resolve()
    if not path.is_relative_to(root) or not path.is_file():
        raise ValueError(f'文件缺失或越过仓库边界: {name}')
    return path


def test_key(test):
    return (test['kind'], test['path'], test.get('name', ''))


def contract_digest(contract):
    """绑定 H 的核心契约；演进说明及对象键顺序不影响摘要。"""
    fields = ('id', 'level', 'status', 'behavior', 'source_commits', 'implementation', 'tests')
    try:
        core = {field: contract[field] for field in fields}
        canonical = json.dumps(core, ensure_ascii=False, sort_keys=True, separators=(',', ':'), allow_nan=False)
    except (KeyError, TypeError) as error:
        raise ValueError('契约核心字段不完整或不是 JSON 数据') from error
    return hashlib.sha256(canonical.encode('utf-8')).hexdigest()


def evolution_moves(contract):
    """声明只证明映射完整；真实运行和独立语义审查由调用方提供。"""
    if 'evolution' not in contract:
        return {}, {}
    evolution = contract['evolution']
    if not isinstance(evolution, dict) or not isinstance(evolution.get('baseline_sha256'), str) or not re.fullmatch(r'[0-9a-f]{64}', evolution['baseline_sha256']):
        raise ValueError(f'{contract["id"]}: evolution 缺少有效 H 摘要')
    if any(not isinstance(evolution.get(field), str) or not evolution[field].strip() for field in ('reason', 'review')):
        raise ValueError(f'{contract["id"]}: evolution 缺少原因或独立审查证据')
    if 'business_change' in evolution:
        change = evolution['business_change']
        if not isinstance(change, dict) or any(not isinstance(change.get(field), str) or not change[field].strip() for field in ('authorization', 'reason', 'previous_behavior')):
            raise ValueError(f'{contract["id"]}: business_change 缺少授权记录、原因或旧行为')
    mappings = []
    for field in ('implementation_moves', 'test_moves'):
        rows = evolution.get(field, [])
        if not isinstance(rows, list):
            raise ValueError(f'{contract["id"]}: {field} 必须为列表')
        moves, targets = {}, set()
        for row in rows:
            if not isinstance(row, dict) or not isinstance(row.get('from'), str) or not row['from'].strip():
                raise ValueError(f'{contract["id"]}: {field} 缺少有效 from')
            destinations = row.get('to')
            if not isinstance(destinations, list) or not destinations or any(not isinstance(name, str) or not name.strip() for name in destinations):
                raise ValueError(f'{contract["id"]}: {field} 的 to 必须为非空 ID/路径列表')
            if row['from'] in moves or len(set(destinations)) != len(destinations) or targets.intersection(destinations):
                raise ValueError(f'{contract["id"]}: {field} 存在重复 from/to')
            if field == 'implementation_moves':
                for name in [row['from'], *destinations]:
                    file_at(None, name)
            elif row.get('mode') not in ('adapted', 'replacement') or not isinstance(row.get('reason'), str) or not row['reason'].strip():
                raise ValueError(f'{contract["id"]}: test_moves 缺少合法 mode 或原因')
            moves[row['from']] = row
            targets.update(destinations)
        mappings.append(moves)
    return tuple(mappings)


def source_without_comments(text, kind):
    # ponytail: 这里只识别常见词法形式；完整语法和条件编译仍由实际测试验证。
    strings = r'"(?:\\.|[^"\\])*"'
    if kind == 'rust':
        strings = r'r(?P<hashes>#+)".*?"(?P=hashes)|' + strings + r"|'(?:\\.|[^'\\\r\n])'"
    else:
        strings = NODE_STRINGS
    return re.sub(strings + r'|//[^\n]*|/\*.*?\*/',
                  lambda m: m[0] if kind == 'node' and not m[0].startswith(('//', '/*'))
                  else re.sub(r'[^\n]', ' ', m[0]), text, flags=re.S)


def node_report_names(root, text, protected_names=None):
    reports = re.split(r'^TAP version 13\s*$', text.lstrip('\ufeff'), flags=re.M)
    if reports[0].strip() or len(reports) == 1:
        raise ValueError('Node 报告缺少 TAP 起始标记')
    names = set()
    seen_titles = set()
    for report in reports[1:]:
        results = re.findall(r'^(ok|not ok) (\d+) - (.+)$', report, re.M)
        plans = re.findall(r'^1\.\.(\d+)\s*$', report, re.M)
        # Node 混合原生 test() 与整文件 helper 时，文件编号可能与用例编号重复。
        if len(plans) != 1 or int(plans[0]) == 0 or len(results) != int(plans[0]):
            raise ValueError('Node 报告未完整结束或零用例，禁止通过')
        for field in ('fail', 'cancelled'):
            if re.findall(r'^# ' + field + r' (\d+)\s*$', report, re.M) != ['0']:
                raise ValueError(f'Node 报告存在失败、取消或缺少结束汇总: {field}')
        helpers = set()
        for line in report.splitlines():
            helper = re.fullmatch(r'# PASS (.+)', line)
            if helper:
                helpers.add(helper[1])
            result = re.fullmatch(r'(ok|not ok) \d+ - (.+)', line)
            if not result:
                continue
            title = result[2]
            directive = re.search(r'\s+#\s*(?:SKIP|TODO)\b', title, re.I)
            case_title = title[:directive.start()] if directive else title
            if case_title in seen_titles and (protected_names is None or case_title in protected_names):
                raise ValueError(f'Node 受保护标题重复或 skip/pass 冲突，须按实际文件分别验证: {case_title}')
            seen_titles.add(case_title)
            if directive:
                helpers.clear()
                continue
            if result[1] != 'ok':
                raise ValueError(f'Node 用例执行失败: {title}')
            names.add(title)
            # 既有 check helper 的日志只有所在文件真实通过后才算证据。
            if helpers:
                path = (root / title.replace('\\', '/')).resolve()
                if path.is_relative_to(root) and path.is_file():
                    names.update((path.relative_to(root).as_posix(), name) for name in helpers)
                helpers.clear()
    if not names:
        raise ValueError('Node 报告为空，禁止零用例通过')
    return names


def rust_report_names(text, protected_names):
    names = re.findall(r'^([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*): test$', text.lstrip('\ufeff'), re.M)
    if not names:
        raise ValueError('编译测试列表为空，禁止零用例通过')
    seen = set()
    for name in names:
        if name in seen and name in protected_names:
            raise ValueError(f'Rust 受保护名称出现在多个列表入口，须按实际 target 分别验证: {name}')
        seen.add(name)
    return seen


def validate(root, document, rust_names=None, node_names=None):
    if not isinstance(document, dict) or type(document.get('schema_version')) is not int or document.get('schema_version') != 1 or not isinstance(document.get('contracts'), list) or not document['contracts']:
        raise ValueError('schema_version 必须为 1，contracts 必须非空')
    contracts = {}
    stable_ids = {}
    report_paths = {}
    for contract in document['contracts']:
        if not isinstance(contract, dict):
            raise ValueError('契约必须为对象')
        ident = contract.get('id', '')
        if not isinstance(ident, str) or not re.fullmatch(r'[a-z][a-z0-9-]+', ident) or ident in contracts:
            raise ValueError(f'无效或重复契约 ID: {ident}')
        contracts[ident] = contract
        if contract.get('level') not in ('P0', 'P1'):
            raise ValueError(f'{ident}: 缺少 P0/P1 等级')
        evolution_moves(contract)
        retired = contract.get('status') == '已批准替代/移除'
        statuses = ('必须保留', '上游等价实现', '已批准替代/移除') if root is not None else ('必须保留', '上游等价实现', '已批准替代/移除', '缺失', '待验证')
        if contract.get('status') not in statuses:
            raise ValueError(f'{ident}: 缺失、待验证或移除项须先处理，不能进入验收')
        if root is not None and retired and not contract.get('evolution', {}).get('business_change'):
            raise ValueError(f'{ident}: 退休项缺少 business_change 授权审计')
        if not isinstance(contract.get('behavior'), str) or not contract['behavior'].strip():
            raise ValueError(f'{ident}: 缺少行为契约')
        sources = contract.get('source_commits')
        if not isinstance(sources, list) or not sources or any(
            not isinstance(s, str) or not re.fullmatch(r'[0-9a-f]{7,40}', s) for s in sources
        ):
            raise ValueError(f'{ident}: 缺少有效来源提交')
        for field in ('implementation', 'tests'):
            if not isinstance(contract.get(field), list) or (not retired and not contract[field]):
                raise ValueError(f'{ident}: {field} 必须非空')
        for name in contract['implementation']:
            file_at(None if retired else root, name)
        seen = set()
        seen_ids = set()
        for test in contract['tests']:
            if not isinstance(test, dict):
                raise ValueError(f'{ident}: 测试映射必须为对象')
            kind = test.get('kind')
            if kind not in ('rust', 'node', 'browser'):
                raise ValueError(f'{ident}: 未知测试类型: {kind}')
            path = file_at(None if retired else root, test.get('path'))
            name = test.get('name', '')
            if not isinstance(name, str) or (kind == 'rust' and not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*', name)) or (kind == 'node' and not name) or (kind == 'browser' and name):
                raise ValueError(f'{ident}: 无效 {kind} 测试名')
            key = test_key(test)
            if key in seen:
                raise ValueError(f'{ident}: 重复测试 {key}')
            seen.add(key)
            if not retired and ((kind == 'rust' and rust_names is not None) or (kind == 'node' and node_names is not None)):
                paths = report_paths.setdefault((kind, name), set())
                paths.add(test['path'])
                if len(paths) > 1:
                    raise ValueError(f'{ident}: {kind} 同名测试跨路径，报告身份歧义，须按实际 target/文件分别验证: {name}')
            if 'id' in test:
                case_id = test['id']
                if not isinstance(case_id, str) or not case_id.strip() or case_id in seen_ids:
                    raise ValueError(f'{ident}: 无效或同契约重复的测试 ID')
                if case_id in stable_ids and stable_ids[case_id] != key:
                    raise ValueError(f'{ident}: 全局测试 ID 指向不同入口: {case_id}')
                stable_ids[case_id] = key
                seen_ids.add(case_id)
            if root is None or retired:
                continue
            text = source_without_comments(path.read_text(encoding='utf-8'), kind)
            if kind == 'rust':
                match = re.search(
                    r'((?:\s*#\[[^\]]+\]\s*)+)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+'
                    + re.escape(name.split('::')[-1]) + r'\s*\(', text
                )
                if not match or not re.search(r'#\[(?:tokio::)?test\b', match[1]):
                    raise ValueError(f'{ident}: Rust 测试缺失: {name}')
                if re.search(r'#\[ignore\b|#\[cfg_attr\([^\]]*\bignore\b', match[1]):
                    raise ValueError(f'{ident}: Rust 测试被忽略: {name}')
                if rust_names is not None and name not in rust_names:
                    raise ValueError(f'{ident}: 编译后的测试列表缺少: {name}')
            elif kind == 'node':
                matches = [m.groups() for m in re.finditer(
                    r'\b(test|check)\s*(?:\.\s*(skip|todo))?\s*\(\s*([\'"])(.*?)\3|'
                    + NODE_STRINGS, text, re.S
                ) if m[1]]
                active = [call for call, skip, _, title in matches if not skip and title == name]
                if not active:
                    raise ValueError(f'{ident}: Node 用例缺失或被跳过: {name}')
                if node_names is not None and not any((name if call == 'test' else (test['path'], name)) in node_names for call in active):
                    raise ValueError(f'{ident}: Node 报告缺少真实通过的用例: {name}')
            elif kind == 'browser':
                if name or not re.search(r'\bassert[.(]', text) or 'chromium.launch(' not in text:
                    raise ValueError(f'{ident}: 浏览器脚本缺少真实断言或启动入口')
    if root is not None and (root / '.git').exists():
        sources = sorted({source for c in contracts.values() for source in c['source_commits']})
        objects = subprocess.check_output(['git', 'cat-file', '--batch-check=%(objectname) %(objecttype)'],
                                         input='\n'.join(sources) + '\n', cwd=root, text=True, stderr=subprocess.PIPE).splitlines()
        if len(objects) != len(sources):
            raise ValueError('来源提交对象报告不完整')
        for source, result in zip(sources, objects):
            if not result.endswith(' commit') or not result.split(' ', 1)[0].startswith(source):
                raise ValueError(f'来源提交不存在或不是 commit: {source}')
    return contracts


def protected_changes(old, new):
    old_ids = {test['id']: test for test in old['tests'] if 'id' in test}
    new_ids = {test['id']: test for test in new['tests'] if 'id' in test}
    changed_tests = {case_id for case_id, test in old_ids.items() if case_id not in new_ids or test_key(test) != test_key(new_ids[case_id])}
    removed_paths = set(old['implementation']) - set(new['implementation'])
    changed_status = old['status'] != new['status'] and (old['status'] in ('必须保留', '上游等价实现', '已批准替代/移除') or new['status'] == '已批准替代/移除')
    changed_behavior = old['behavior'] != new['behavior']
    return removed_paths, changed_tests, changed_status, changed_behavior


def compare_baseline(current, baseline):
    for old in validate(None, baseline).values():
        ident = old['id']
        if ident not in current:
            raise ValueError(f'保护项被删除: {ident}')
        new = current[ident]
        if old['level'] == 'P0' and new['level'] != 'P0':
            raise ValueError(f'P0 被降级: {ident}')
        if new['status'] not in ('必须保留', '上游等价实现', '已批准替代/移除'):
            raise ValueError(f'{ident}: 缺失、待验证或移除项须先处理，不能进入验收')
        if not set(old['source_commits']).issubset(new['source_commits']):
            raise ValueError(f'来源证据被删除: {ident}')
        retired = new['status'] == '已批准替代/移除'
        if retired and not new.get('evolution', {}).get('business_change'):
            raise ValueError(f'{ident}: 退休项缺少 business_change 授权审计')
        removed_paths, changed_tests, changed_status, changed_behavior = protected_changes(old, new)
        if retired:
            removed_paths, changed_tests = set(), set()  # 退休停止执行，但不删除清单审计。
        keys = {test_key(test) for test in new['tests']}
        if not retired and any('id' not in test and test_key(test) not in keys for test in old['tests']):
            raise ValueError(f'受保护测试入口被删除或替换，须独立审查: {ident}')
        old_ids = {test['id']: test for test in old['tests'] if 'id' in test}
        new_ids = {test['id']: test for test in new['tests'] if 'id' in test}
        if not (changed_tests or removed_paths or changed_status or changed_behavior):
            continue  # 历史声明不是本轮放行依据，不让后续纯文档提交因旧摘要阻塞。
        implementation_moves, test_moves = evolution_moves(new)
        if not new.get('evolution'):
            field = '行为基线' if changed_behavior else '状态基线' if changed_status else '受保护实现入口' if removed_paths else '受保护测试入口'
            raise ValueError(f'{field}改变，须提供本轮 evolution: {ident}')
        if new['evolution']['baseline_sha256'] != contract_digest(old):
            raise ValueError(f'{ident}: evolution 的 H 摘要过期或不匹配')
        if changed_behavior or (retired and old['status'] != new['status']):
            change = new['evolution'].get('business_change')
            if not change or change['previous_behavior'] != old['behavior']:
                raise ValueError(f'{ident}: 行为基线改变或退休须有准确旧行为及 business_change 授权审计')
        if retired:
            continue
        for path, move in implementation_moves.items():
            if path not in old['implementation'] or not set(move['to']).issubset(new['implementation']):
                raise ValueError(f'{ident}: 实现迁移 from/to 不在 H/候选契约中')
        for case_id, move in test_moves.items():
            if case_id not in old_ids or not set(move['to']).issubset(new_ids):
                raise ValueError(f'{ident}: 测试迁移 from/to 不在 H/候选契约中')
            if any(new_ids[target]['kind'] != old_ids[case_id]['kind'] for target in move['to']):
                raise ValueError(f'{ident}: 测试迁移须保持验证类型，避免 H 义务脱离运行域')
        if not removed_paths.issubset(implementation_moves):
            raise ValueError(f'{ident}: evolution 未覆盖移除的实现入口')
        if not changed_tests.issubset(test_moves):
            raise ValueError(f'{ident}: evolution 未覆盖改变或缺失的测试入口')


def baseline_view(baseline, current):
    """输出 H 义务视图；独立原始测试仍由运行器在 H 路径重放。"""
    if isinstance(current.get('contracts'), list):
        current = validate(None, current)
    compare_baseline(current, baseline)
    view = copy.deepcopy(baseline)
    view['audit_retired'] = []
    active = []
    for contract in view['contracts']:
        old = copy.deepcopy(contract)
        new = current[contract['id']]
        if new['status'] == '已批准替代/移除':
            audit = copy.deepcopy(new)
            audit['baseline_sha256'] = contract_digest(old)
            view['audit_retired'].append(audit)
            continue
        implementation_moves, test_moves = evolution_moves(new) if any(protected_changes(old, new)) else ({}, {})
        contract.pop('evolution', None)
        contract['baseline_sha256'] = contract_digest(old)
        contract['status'] = new['status']
        contract['behavior'] = new['behavior']
        contract['implementation'] = list(dict.fromkeys(path for old_path in old['implementation'] for path in implementation_moves.get(old_path, {}).get('to', [old_path])))
        candidates = {test['id']: test for test in new['tests'] if 'id' in test}
        tests = {}
        for test in old['tests']:
            case_id = test.get('id')
            inline = test['kind'] == 'rust' and not test['path'].startswith('desktop-tauri/src-tauri/server/tests/')
            replacements = [candidates[name] for name in test_moves[case_id]['to']] if inline and case_id in test_moves else [test]
            for replacement in replacements:
                key = test_key(replacement)
                if key not in tests:
                    item = copy.deepcopy(replacement)
                    for field in ('baseline_inline', 'baseline_independent', 'baseline_case_ids'):
                        item.pop(field, None)
                    tests[key] = item
                item = tests[key]
                item['baseline_inline' if inline else 'baseline_independent'] = True
                if case_id is not None:
                    ids = item.setdefault('baseline_case_ids', [])
                    if case_id not in ids:
                        ids.append(case_id)
        contract['tests'] = list(tests.values())
        active.append(contract)
    view['contracts'] = active
    return view


def baseline_at(root, ref):
    ref = subprocess.check_output(['git', 'rev-parse', '--verify', '--end-of-options', ref + '^{commit}'],
                                  cwd=root, text=True, stderr=subprocess.PIPE).strip()
    exists = subprocess.check_output(['git', 'ls-tree', '--name-only', ref, '--', MANIFEST],
                                     cwd=root, text=True).strip()
    if not exists:
        return None
    return read_json(subprocess.check_output(['git', 'show', f'{ref}:{MANIFEST}'],
                                            cwd=root, encoding='utf-8'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--manifest', type=Path, help='验证目录 JSON；默认仓库 docs/local-contracts.json，入口仍以 --root 为边界')
    parser.add_argument('--base-ref', help='合并前本地提交/PR base/push before；不使用上游替代本地基线')
    parser.add_argument('--rust-list', type=Path, help='cargo test 所有相关 target 的实际 --list 合集')
    parser.add_argument('--node-report', type=Path, help='既有 Node 套件实际执行的 TAP 报告合集')
    args = parser.parse_args()
    root = args.root.resolve()
    try:
        manifest = args.manifest if args.manifest else file_at(root, MANIFEST)
        document = read_json(manifest.read_text(encoding='utf-8'))
        catalog = validate(None, document)
        protected = {kind: {t['name'] for c in catalog.values() for t in c['tests']
                            if c['status'] != '已批准替代/移除' and t['kind'] == kind}
                     for kind in ('rust', 'node')}
        names = None
        if args.rust_list:
            names = rust_report_names(args.rust_list.read_text(encoding='utf-8-sig'), protected['rust'])
        node_names = None
        if args.node_report:
            node_names = node_report_names(root, args.node_report.read_text(encoding='utf-8-sig'), protected['node'])
        contracts = validate(root, document, names, node_names)
        if args.base_ref:
            baseline = baseline_at(root, args.base_ref)
            if baseline is None:
                print('BOOTSTRAP: 合并前提交尚无清单；本次建立首份基线，须审查来源与覆盖。')
            else:
                compare_baseline(contracts, baseline)
                print(f'BASELINE: {args.base_ref}; {len(baseline["contracts"])} 个保护项保留。')
        count = sum(len(c['tests']) for c in contracts.values())
        print(f'PASS: {len(contracts)} 个契约，{count} 个测试映射；静态检查不替代业务测试。')
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
