"""Static merge-closure checks; use temporary fixtures and no project mutation."""
import importlib.util
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location('closure', Path(__file__).with_name('check-merge-closure.py'))
closure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(closure)


class MergeClosureTests(unittest.TestCase):
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
