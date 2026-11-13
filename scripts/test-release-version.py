#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("stamp", Path(__file__).with_name("stamp-release-version.py"))
stamp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stamp)


class ReleaseVersion(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "cli").mkdir()
        (self.root / "server").mkdir()
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["cli"]\n[workspace.package]\nversion = "0.0.0-dev"\n')
        (self.root / "cli/Cargo.toml").write_text('[package]\nname = "koidrive-cli"\nversion.workspace = true\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "koidrive-cli"\nversion = "0.0.0-dev"\n[[package]]\nname = "koidrive-cli"\nversion = "0.0.0-dev"\nsource = "registry+https://example.com"\n')
        (self.root / "server/release.json").write_text('{"version":"0.0.0-dev"}\n')

    def test_release_tags_stamp_every_version_source(self):
        for tag in ["v1.2.3", "v1.2.3-alpha.2", "v1.2.3-rc.4", "v0.0.0-rc.0"]:
            with self.subTest(tag=tag):
                self.assertEqual(stamp.stamp(self.root, tag), tag[1:])
                cargo = (self.root / "Cargo.toml").read_text()
                lock = (self.root / "Cargo.lock").read_text()
                self.assertIn(f'version = "{tag[1:]}"', cargo)
                self.assertIn(f'name = "koidrive-cli"\nversion = "{tag[1:]}"', lock)
                self.assertIn('name = "koidrive-cli"\nversion = "0.0.0-dev"\nsource =', lock)
                self.assertEqual(stamp.json.loads((self.root / "server/release.json").read_text())["version"], tag[1:])
                before = [path.read_bytes() for path in [self.root / "Cargo.toml", self.root / "Cargo.lock", self.root / "server/release.json"]]
                stamp.stamp(self.root, tag)
                self.assertEqual([path.read_bytes() for path in [self.root / "Cargo.toml", self.root / "Cargo.lock", self.root / "server/release.json"]], before)

    def test_invalid_tags_do_not_change_manifests(self):
        paths = [self.root / name for name in ["Cargo.toml", "Cargo.lock", "server/release.json"]]
        before = [path.read_bytes() for path in paths]
        for tag in ["1.2.3", "v01.2.3", "v1.2.3-alpha.01", "v1.2.3-beta.1", "v1.2.3+build", "v1.2.3\ninjected"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                stamp.stamp(self.root, tag)
            self.assertEqual([path.read_bytes() for path in paths], before)

    def test_missing_workspace_lock_entry_stops_before_writing(self):
        (self.root / "Cargo.lock").write_text('version = 4\n')
        before = (self.root / "Cargo.toml").read_bytes()
        with self.assertRaisesRegex(ValueError, "missing workspace packages"):
            stamp.stamp(self.root, "v1.2.3")
        self.assertEqual((self.root / "Cargo.toml").read_bytes(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
