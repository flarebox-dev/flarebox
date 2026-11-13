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
            lines = [f"cc {'1' * 64} # shrinks to a\n", f"cc {'2' * 64}\n"]
            legacy.write_text("# header\n" + "".join(lines))
            self.assertEqual(module.split(legacy), 2)
            self.assertEqual(legacy.read_text(), "# header\n")
            cases = sorted(legacy.with_suffix(".cases").glob("*.txt"))
            self.assertEqual([path.read_text() for path in cases], lines)
            self.assertEqual(module.split(legacy), 0)

    def test_conflicting_destination_preserves_the_legacy_file(self):
        with tempfile.TemporaryDirectory() as root:
            legacy = Path(root) / "property.txt"
            line = f"cc {'1' * 64}\n"
            legacy.write_text(line)
            destination = legacy.with_suffix(".cases") / f"cc-{'1' * 64}.txt"
            destination.parent.mkdir()
            destination.write_text("other\n")
            with self.assertRaises(ValueError):
                module.split(legacy)
            self.assertEqual(legacy.read_text(), line)
            self.assertEqual(destination.read_text(), "other\n")


if __name__ == "__main__":
    unittest.main()
