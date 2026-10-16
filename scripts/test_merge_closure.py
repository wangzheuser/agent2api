"""Static merge-closure checks; use temporary fixtures and no project mutation."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import json
import subprocess
import sys


spec = importlib.util.spec_from_file_location('closure', Path(__file__).with_name('check-merge-closure.py'))
closure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(closure)


class MergeClosureTests(unittest.TestCase):
    def test_version_consistency_rejects_missing_and_mismatch(self):
        checker = Path(__file__).with_name('check-version-consistency.py')
        root = Path(tempfile.mkdtemp())
        (root / 'desktop-tauri/src-tauri/server').mkdir(parents=True)
        values = {
            'package.json': {'version': '3.0.7'},
            'desktop-tauri/package.json': {'version': '3.0.7'},
            'desktop-tauri/src-tauri/tauri.conf.json': {'version': '3.0.7'},
        }
        for path, value in values.items():
            target = root / path; target.parent.mkdir(parents=True, exist_ok=True); target.write_text(json.dumps(value), encoding='utf-8')
        (root / 'desktop-tauri/src-tauri/Cargo.toml').write_text('version = "3.0.7"\n', encoding='utf-8')
        (root / 'desktop-tauri/src-tauri/server/Cargo.toml').write_text('version = "3.0.6"\n', encoding='utf-8')
        (root / 'desktop-tauri/src-tauri/Cargo.lock').write_text('', encoding='utf-8')
        result = subprocess.run([sys.executable, str(checker), '--root', str(root)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)

    def test_api_orphan_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            api = root / 'desktop-tauri/src-tauri/server/src/server/api'
            api.mkdir(parents=True)
            (api / 'mod.rs').write_text('pub mod known;\n', encoding='utf-8')
            (api / 'known.rs').write_text('', encoding='utf-8')
            (api / 'orphan.rs').write_text('', encoding='utf-8')
            self.assertEqual(closure.api_exports(root), ['api/orphan.rs is not exported by api/mod.rs'])

    def test_schema_gap_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            schema = root / 'desktop-tauri/src-tauri/server/src/server/db/schema.rs'
            schema.parent.mkdir(parents=True)
            schema.write_text('pub const SCHEMA_VERSION: i64 = 3;\n1 => ok,\n3 => ok,\n', encoding='utf-8')
            self.assertEqual(closure.schema_versions(root), ['schema version arms missing: 2'])

    def test_login_and_generated_closure_are_checked(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            ui = root / 'desktop-tauri/ui-islands/src/islands'
            login = root / 'desktop-tauri/src-tauri/src'
            ui.mkdir(parents=True)
            login.mkdir(parents=True)
            (ui / 'add-account-configs.ts').write_text(
                "provider: 'demo', webLogin: {}\n", encoding='utf-8')
            (login / 'login.rs').write_text('', encoding='utf-8')
            self.assertEqual(closure.login_closure(root),
                             ['web login provider demo is absent from desktop login wiring'])


if __name__ == '__main__':
    unittest.main()
