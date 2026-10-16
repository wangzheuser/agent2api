#!/usr/bin/env python3
"""检查应用版本文件和 Cargo.lock 的版本是否一致。"""
import argparse
import json
import re
import sys
from pathlib import Path

FILES = (
    "package.json",
    "desktop-tauri/package.json",
    "desktop-tauri/src-tauri/Cargo.toml",
    "desktop-tauri/src-tauri/tauri.conf.json",
    "desktop-tauri/src-tauri/server/Cargo.toml",
)


def read_version(root: Path, relative: str) -> str:
    text = (root / relative).read_text(encoding="utf-8")
    if relative.endswith(".json"):
        value = json.loads(text).get("version")
    else:
        match = re.search(r"(?m)^version\s*=\s*\"([^\"]+)\"", text)
        value = match.group(1) if match else None
    if not isinstance(value, str) or not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError(f"{relative}: version is missing or invalid")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    root = args.root.resolve()
    try:
        versions = {path: read_version(root, path) for path in FILES}
        lock = (root / "desktop-tauri/src-tauri/Cargo.lock").read_text(encoding="utf-8")
        lock_versions = re.findall(r"(?ms)^name = \"(?:agent2api-server|workbuddy-proxy-desktop)\"\nversion = \"([^\"]+)\"", lock)
        if len(lock_versions) != 2:
            raise ValueError("Cargo.lock: expected agent2api-server and workbuddy-proxy-desktop packages")
        versions["Cargo.lock agent2api-server"] = lock_versions[0]
        versions["Cargo.lock workbuddy-proxy-desktop"] = lock_versions[1]
        expected = next(iter(versions.values()))
        bad = {path: value for path, value in versions.items() if value != expected}
        if bad:
            print("VERSION: FAIL", file=sys.stderr)
            for path, value in versions.items():
                print(f" - {path}={value}", file=sys.stderr)
            return 1
        print(f"VERSION: PASS {expected}")
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"VERSION: FAIL {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
