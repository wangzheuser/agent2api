#!/usr/bin/env python3
"""Create an isolated worktree from H before an AI agent starts an upstream merge."""
import argparse
import json
from pathlib import Path
import subprocess
import sys


PROTECTED_CONTROL_FILES = {
    'AGENTS.md',
    'docs/local-contracts.json',
    'scripts/check-merge-contracts.py',
    'scripts/local-merge-gates.py',
    'scripts/merge-flow.py',
    'scripts/replay-merge-baseline.py',
}


def git(root, *args, check=True):
    return subprocess.run(['git', *args], cwd=root, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check)


def protected_deletions(root, head, upstream):
    deleted = []
    rows = git(root, 'diff', '--name-status', head, upstream).stdout.splitlines()
    for row in rows:
        fields = row.split('\t')
        if len(fields) >= 2 and fields[0] == 'D' and fields[1] in PROTECTED_CONTROL_FILES:
            deleted.append(fields[1])
    return sorted(deleted)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--upstream', required=True)
    parser.add_argument('--worktree', type=Path, required=True)
    parser.add_argument('--record', type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    worktree = args.worktree.resolve()
    record_path = args.record.resolve()
    if worktree.exists():
        print(f'FAIL: candidate worktree already exists: {worktree}', file=sys.stderr)
        return 1
    if git(root, 'rev-parse', '--is-shallow-repository').stdout.strip() != 'false':
        print('FAIL: complete Git history is required before creating a candidate', file=sys.stderr)
        return 1
    branch = git(root, 'branch', '--show-current').stdout.strip()
    if not branch:
        print('FAIL: current checkout must be on a named branch', file=sys.stderr)
        return 1
    if git(root, 'rev-parse', '-q', '--verify', 'MERGE_HEAD', check=False).returncode == 0:
        print('FAIL: source checkout already has MERGE_HEAD', file=sys.stderr)
        return 1
    status = git(root, 'status', '--porcelain=v1', '-z').stdout.split('\0')
    tracked = [item for item in status if item and not item.startswith('?? ')]
    if tracked:
        print('FAIL: tracked source/index changes must be resolved before candidate creation', file=sys.stderr)
        for item in tracked:
            print(f' - {item}', file=sys.stderr)
        return 1
    head = git(root, 'rev-parse', 'HEAD').stdout.strip()
    upstream = git(root, 'rev-parse', '--verify', '--end-of-options', args.upstream + '^{commit}').stdout.strip()
    bases = git(root, 'merge-base', '--all', head, upstream).stdout.splitlines()
    if len(bases) != 1:
        print('FAIL: upstream and H do not have one unique merge base', file=sys.stderr)
        return 1
    blocked = protected_deletions(root, head, upstream)
    if blocked:
        print('FAIL: upstream deletes protected merge-control files; preserve them before any merge', file=sys.stderr)
        for path in blocked:
            print(f' - {path}', file=sys.stderr)
        return 2
    candidate_branch = f'codex/merge-candidate-{upstream[:12]}'
    if git(root, 'show-ref', '--verify', f'refs/heads/{candidate_branch}', check=False).returncode == 0:
        print(f'FAIL: candidate branch already exists: {candidate_branch}', file=sys.stderr)
        return 1
    worktree.parent.mkdir(parents=True, exist_ok=True)
    added = git(root, 'worktree', 'add', '-b', candidate_branch, str(worktree), head, check=False)
    if added.returncode:
        print(added.stderr, file=sys.stderr, end='')
        return added.returncode
    record = {
        'schema_version': 1,
        'source_root': str(root),
        'source_branch': branch,
        'H': head,
        'U': upstream,
        'B': bases[0],
        'candidate_root': str(worktree),
        'candidate_branch': candidate_branch,
        'source_untracked': sorted(item[3:] for item in status if item.startswith('?? ')),
        'merge_started': False,
    }
    record_path.parent.mkdir(parents=True, exist_ok=True)
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(f'CANDIDATE: root={worktree}; H={head}; U={upstream}; B={bases[0]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
