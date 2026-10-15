#!/usr/bin/env python3
"""Create an isolated worktree from H before an AI agent starts an upstream merge."""
import argparse
import json
from pathlib import Path
import subprocess
import sys


def git(root, *args, check=True):
    return subprocess.run(['git', *args], cwd=root, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check)


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
    worktree.parent.mkdir(parents=True, exist_ok=True)
    added = git(root, 'worktree', 'add', '--detach', str(worktree), head, check=False)
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
        'source_untracked': sorted(item[3:] for item in status if item.startswith('?? ')),
        'merge_started': False,
    }
    record_path.parent.mkdir(parents=True, exist_ok=True)
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(f'CANDIDATE: root={worktree}; H={head}; U={upstream}; B={bases[0]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
