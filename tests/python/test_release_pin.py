"""Release admission must reject a wrong Git source even when its SHA suffix matches."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

try:
    import tomllib
except ImportError:
    raise unittest.SkipTest('Release admission tests require Python 3.11+')

spec = importlib.util.spec_from_file_location('release_check', Path(__file__).resolve().parents[2] / 'scripts/check-release.py')
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class ReleasePinTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='liquidator-release-pin-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.sha = 'a' * 40
        self.url = 'https://github.com/BarterLab/barterlend-state.git'
        self.manifest = '[dependencies]\nbarterlend-state = {git = "' + self.url + '", rev = "' + self.sha + '"}\n'
        self.lock = '[[package]]\nname = "barterlend-state"\nversion = "0.1.0"\nsource = "git+' + self.url + '?rev=' + self.sha + '#' + self.sha + '"\n'
        (self.root / 'Cargo.toml').write_text(self.manifest)
        (self.root / 'Cargo.lock').write_text(self.lock)

    def test_exact_git_pin_is_accepted(self):
        checker.check(self.root)

    def test_wrong_source_and_duplicate_sdk_cannot_pass_on_the_sha_suffix(self):
        for contents in [self.lock.replace('BarterLab', 'different-owner'), self.lock + self.lock]:
            with self.subTest(contents=contents):
                (self.root / 'Cargo.lock').write_text(contents)
                with self.assertRaises(ValueError): checker.check(self.root)

    def test_local_sdk_and_dependency_overrides_are_rejected(self):
        variants = ['[dependencies]\nbarterlend-state = {path = "../sdk"}\n',
                    self.manifest + '[patch.crates-io]\nsome-crate = {path = "../override"}\n']
        for contents in variants:
            with self.subTest(contents=contents):
                (self.root / 'Cargo.toml').write_text(contents)
                with self.assertRaises(ValueError): checker.check(self.root)

    def test_missing_or_changed_lock_pin_is_rejected(self):
        for contents in [self.lock.replace('a' * 40, 'b' * 40), self.lock.split('source =')[0]]:
            with self.subTest(contents=contents):
                (self.root / 'Cargo.lock').write_text(contents)
                with self.assertRaises(ValueError): checker.check(self.root)


if __name__ == '__main__':
    unittest.main()
