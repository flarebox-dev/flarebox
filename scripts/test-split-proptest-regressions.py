# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "split_regressions", Path(__file__).with_name("split-proptest-regressions.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class SplitRegressionsTests(unittest.TestCase):
    def test_moves_each_line_unchanged_and_can_run_again(self):
        with tempfile.TemporaryDirectory() as root:
            legacy = Path(root) / "property.proptest-regressions"
            lines = [f"cc {'1' * 64} # shrinks to ß\n", f"cc {'2' * 64}\n"]
            legacy.write_text("# header\n" + "".join(lines), encoding="utf-8")
            self.assertEqual(module.split(legacy), 2)
            self.assertEqual(legacy.read_text(encoding="utf-8"), "# header\n")
            cases = sorted(legacy.with_suffix(".cases").glob("*.txt"))
            self.assertEqual([path.read_text(encoding="utf-8") for path in cases], lines)
            self.assertEqual(module.split(legacy), 0)

    def test_conflicting_destination_preserves_the_legacy_file(self):
        with tempfile.TemporaryDirectory() as root:
            legacy = Path(root) / "property.txt"
            line = f"cc {'1' * 64}\n"
            legacy.write_text(line, encoding="utf-8")
            destination = legacy.with_suffix(".cases") / f"cc-{'1' * 64}.txt"
            destination.parent.mkdir()
            destination.write_text("other\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                module.split(legacy)
            self.assertEqual(legacy.read_text(encoding="utf-8"), line)
            self.assertEqual(destination.read_text(encoding="utf-8"), "other\n")


if __name__ == "__main__":
    unittest.main()
