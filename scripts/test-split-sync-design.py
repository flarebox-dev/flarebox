#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
import sys
import unittest


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("split_design", Path(__file__).with_name("split-sync-design.py"))
split = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(split)


class HeadingsTests(unittest.TestCase):
    def test_fence_with_trailing_text_does_not_close_the_block(self):
        source = "```text\n```literal\n## Example\n```\n## Real\n"
        self.assertEqual([title for _, _, title in split.headings(source)], ["Real"])

    def test_closing_fences_allow_trailing_spaces_and_tabs(self):
        for opening, closing in [("```", "```"), ("~~~", "~~~~")]:
            with self.subTest(opening=opening):
                source = f"{opening}text\n## Hidden\n{closing} \t\n## Real\n"
                self.assertEqual([title for _, _, title in split.headings(source)], ["Real"])


if __name__ == "__main__":
    unittest.main()
