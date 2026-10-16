#!/usr/bin/env python3
"""Check registration, migration, and generated-entry closure before expensive tests."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root, text=True)


def changed_paths(root, base):
    return set(git(root, 'diff', '--name-only', base, '--').splitlines())


def api_exports(root):
    directory = root / 'desktop-tauri/src-tauri/server/src/server/api'
    module_file = directory / 'mod.rs'
    if not directory.is_dir() or not module_file.is_file():
        return ['server API directory or api/mod.rs is missing']
    text = module_file.read_text(encoding='utf-8')
    exports = set(re.findall(r'\bpub\s+mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;', text))
    findings = []
    for source in sorted(directory.glob('*.rs')):
        if source.name == 'mod.rs':
            continue
        if source.stem not in exports:
            findings.append(f'api/{source.name} is not exported by api/mod.rs')
    return findings


def schema_versions(root):
    path = root / 'desktop-tauri/src-tauri/server/src/server/db/schema.rs'
    if not path.is_file():
        return ['database schema source is missing']
    text = path.read_text(encoding='utf-8')
    match = re.search(r'SCHEMA_VERSION\s*:\s*i64\s*=\s*(\d+)', text)
    if not match:
        return ['SCHEMA_VERSION declaration is missing']
    latest = int(match.group(1))
    arms = {int(value) for value in re.findall(r'(?m)^\s*(\d+)\s*=>', text)}
    missing = [str(value) for value in range(1, latest + 1) if value not in arms]
    return [f'schema version arms missing: {", ".join(missing)}'] if missing else []


def login_closure(root):
    ui_path = root / 'desktop-tauri/ui-islands/src/islands/add-account-configs.ts'
    login_path = root / 'desktop-tauri/src-tauri/src/login.rs'
    if not ui_path.is_file() or not login_path.is_file():
        return ['desktop login source or provider config is missing']
    ui = ui_path.read_text(encoding='utf-8')
    login = login_path.read_text(encoding='utf-8')
    providers = []
    for match in re.finditer(r"provider:\s*'([^']+)'", ui):
        end = ui.find('provider:', match.end())
        block = ui[match.start(): end if end >= 0 else match.end() + 800]
        if 'webLogin:' in block:
            providers.append(match.group(1))
    return [f'web login provider {provider} is absent from desktop login wiring'
            for provider in sorted(set(providers)) if provider not in login]


def generated_closure(root, base):
    changed = changed_paths(root, base)
    source_changed = any(path.startswith('desktop-tauri/ui-islands/src/') for path in changed)
    if not source_changed:
        return []
    generated = {'desktop-tauri/ui/islands/ui.js', 'desktop-tauri/ui/islands/ui.css'}
    missing = sorted(generated - changed)
    return [f'UI source changed without generated artifact: {path}' for path in missing]


def version_closure(root):
    checker = root / 'scripts/check-version-consistency.py'
    if not checker.is_file():
        return ['version consistency checker is missing']
    result = subprocess.run([sys.executable, str(checker), '--root', str(root)],
                            capture_output=True, text=True)
    return [] if result.returncode == 0 else [line for line in result.stderr.splitlines() if line]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--base-ref', required=True)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    findings = []
    try:
        findings.extend(api_exports(root))
        findings.extend(schema_versions(root))
        findings.extend(login_closure(root))
        findings.extend(generated_closure(root, args.base_ref))
        findings.extend(version_closure(root))
    except (OSError, subprocess.CalledProcessError, UnicodeError) as error:
        findings.append(f'closure checker error: {error}')
    report = {'base_ref': args.base_ref, 'findings': findings, 'passed': not findings}
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    if findings:
        print('CLOSURE: FAIL', file=sys.stderr)
        for finding in findings:
            print(f' - {finding}', file=sys.stderr)
        return 1
    print(f'CLOSURE: PASS base={args.base_ref}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
