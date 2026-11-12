#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import os
import subprocess
import sys
import tempfile
import unittest

CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-new-comments.py")


class NewComments(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.repo = self.dir.name
        subprocess.run(["git", "init", "-q", self.repo], check=True)

    def tearDown(self):
        self.dir.cleanup()

    def tree(self, files):
        for path, text in files.items():
            full = os.path.join(self.repo, path)
            if text is None:
                os.remove(full)
                continue
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as f:
                f.write(text)
        subprocess.run(["git", "-C", self.repo, "add", "-A"], check=True)
        return subprocess.run(["git", "-C", self.repo, "write-tree"], check=True, capture_output=True, text=True).stdout.strip()

    def run_check(self, before, after):
        base = self.tree(before)
        head = self.tree(after)
        result = subprocess.run([sys.executable, CHECK, base, head], cwd=self.repo, capture_output=True, text=True)
        flagged = [line.split("::", 2)[1] for line in result.stdout.splitlines() if line.startswith("::error")]
        self.assertEqual(result.returncode, 1 if flagged else 0, result.stdout + result.stderr)
        return flagged

    def assertFlags(self, before, after, *expected):
        self.assertEqual(self.run_check(before, after), [f"error file={path},line={line}" for path, line in expected])

    def test_new_line_comment_fails(self):
        self.assertFlags({"core/a/src/lib.rs": "fn a() {}\n"}, {"core/a/src/lib.rs": "// explains a\nfn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_trailing_comment_fails(self):
        self.assertFlags({"server/src/a.ts": "const a = 1;\n"}, {"server/src/a.ts": "const a = 2; // bumped\n"}, ("server/src/a.ts", 1))

    def test_every_line_of_a_new_block_comment_fails(self):
        self.assertFlags({"apps/macos/Koidrive/A.swift": "let a = 1\n"}, {"apps/macos/Koidrive/A.swift": "/* one\n two */\nlet a = 1\n"}, ("apps/macos/Koidrive/A.swift", 1), ("apps/macos/Koidrive/A.swift", 2))

    def test_deleting_comments_passes(self):
        self.assertFlags({"core/a/src/lib.rs": "/// Doc.\n// note\nfn a() {} // why\n"}, {"core/a/src/lib.rs": "fn a() {}\n"})

    def test_trimming_a_comment_passes(self):
        self.assertFlags(
            {"server/src/a.ts": "// Workers cap subrequests at 1000 per invocation, see spec v0 section 4.\nconst MAX = 1000;\n"},
            {"server/src/a.ts": "// Workers cap subrequests at 1000 per invocation.\nconst MAX = 1000;\n"},
        )

    def test_rewording_a_comment_fails(self):
        self.assertFlags(
            {"server/src/a.ts": "// Workers cap subrequests.\nconst MAX = 1000;\n"},
            {"server/src/a.ts": "// Workers limit subrequests.\nconst MAX = 1000;\n"},
            ("server/src/a.ts", 1),
        )

    def test_moving_a_doc_between_files_passes(self):
        self.assertFlags(
            {"server/src/a.ts": "/** Opens the space. */\nexport function open() {}\n", "server/src/b.ts": "export function close() {}\n"},
            {"server/src/a.ts": "export function open() {}\n", "server/src/b.ts": "/** Opens the space. */\nexport function close() {}\n"},
        )

    def test_doc_comment_on_a_library_passes(self):
        self.assertFlags({"core/a/src/lib.rs": "pub fn a() {}\n"}, {"core/a/src/lib.rs": "//! The a crate.\n\n/// Returns nothing.\npub fn a() {}\n"})

    def test_doc_comment_on_internal_code_fails(self):
        self.assertFlags(
            {"core/a/tests/t.rs": "fn t() {}\n", "server/src/a.ts": "export function a() {}\n", "apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift": "func t() {}\n"},
            {"core/a/tests/t.rs": "/// Helper.\nfn t() {}\n", "server/src/a.ts": "/** Helper. */\nexport function a() {}\n", "apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift": "/// Helper.\nfunc t() {}\n"},
            ("apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift", 1),
            ("core/a/tests/t.rs", 1),
            ("server/src/a.ts", 1),
        )

    def test_markers_fail_in_any_comment(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "pub fn a() {}\n"},
            {"core/a/src/lib.rs": "/// Returns nothing. TODO: return something.\npub fn a() {}\n/// Fine for now.\npub fn b() {}\n"},
            ("core/a/src/lib.rs", 1),
            ("core/a/src/lib.rs", 3),
        )

    def test_a_kept_marker_fails_even_when_trimmed(self):
        self.assertFlags({"core/a/src/lib.rs": "// TODO: drop this hack once v2 ships\nfn a() {}\n"}, {"core/a/src/lib.rs": "// TODO: drop this hack\nfn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_allowed_comments_pass(self):
        self.assertFlags(
            {"cli/src/a.rs": "fn a() {}\n", "server/src/a.ts": "export const a = 1;\n", "apps/macos/Koidrive/A.swift": "let a = 1\n"},
            {
                "cli/src/a.rs": "// SPDX-License-Identifier: Apache-2.0\nfn a() {\n    // SAFETY: fd is open for the whole call.\n    unsafe {}\n}\n",
                "server/src/a.ts": "// Durable Objects drop the alarm on eviction: https://developers.cloudflare.com/durable-objects/api/alarms/\n// eslint-disable-next-line no-console\nexport const a = 1;\n",
                "apps/macos/Koidrive/A.swift": "// swiftlint:disable:next identifier_name\nlet a = 1\n",
            },
        )

    def test_comment_markers_inside_strings_pass(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "fn a() {}\n", "server/src/a.ts": "export const a = 1;\n", "apps/macos/Koidrive/A.swift": "let a = 1\n"},
            {
                "core/a/src/lib.rs": 'fn a() {\n    let u = "http://x // y";\n    let r = r#"a // "b" /* c"#;\n    let c = \'/\';\n    let s = "line\n// still a string";\n}\n',
                "server/src/a.ts": "export const a = '//';\nconst re = /\\/\\//g;\nconst t = \x60x \${'// y'} \${\x60//\x60} //\x60;\nconst d = 4 / 2 / 1;\n",
                "apps/macos/Koidrive/A.swift": 'let a = "x \\("//") y"\nlet b = #"a // "b""#\nlet c = """\n// text\n"""\n',
            },
        )

    def test_comments_after_strings_still_fail(self):
        self.assertFlags(
            {"server/src/a.ts": "export const a = 1;\n", "core/a/src/lib.rs": "fn a() {}\n"},
            {"server/src/a.ts": "const t = \x60\${'x'}\x60; // after a template\nconst re = /a/; // after a regex\n", "core/a/src/lib.rs": 'fn a<\'a>(x: &\'a str) -> char { \'"\' } // after a lifetime and a char\n'},
            ("core/a/src/lib.rs", 1),
            ("server/src/a.ts", 1),
            ("server/src/a.ts", 2),
        )

    def test_nested_rust_block_comment_is_one_comment(self):
        self.assertFlags({"core/a/src/lib.rs": "fn a() {}\n"}, {"core/a/src/lib.rs": "/* outer /* inner */ still outer */ fn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_generated_code_and_other_languages_are_skipped(self):
        self.assertFlags(
            {"server/src/gen/a.ts": "export const a = 1;\n", "scripts/a.sh": "true\n"},
            {"server/src/gen/a.ts": "// Code generated by protoc-gen-es.\nexport const a = 1;\n", "scripts/a.sh": "# a note\ntrue\n"},
        )

    def test_new_and_deleted_files(self):
        self.assertFlags({"server/src/old.ts": "// old note\nexport const a = 1;\n"}, {"server/src/old.ts": None, "server/src/new.ts": "// new note\nexport const b = 2;\n"}, ("server/src/new.ts", 1))


    def comments_only(self, before, after):
        base = self.tree(before)
        head = self.tree(after)
        result = subprocess.run([sys.executable, CHECK, "--comments-only", base, head], cwd=self.repo, capture_output=True, text=True)
        return result.returncode, result.stdout

    def test_comments_only_passes_when_only_comments_change(self):
        code, out = self.comments_only(
            {"core/a/src/lib.rs": '/// Doc.\nfn a() {\n    let s = "// kept"; // gone\n}\n', "server/src/a.ts": '/** Doc. */\nexport const a = "x  y";\n'},
            {"core/a/src/lib.rs": 'fn a() {\n    let s = "// kept";\n}\n', "server/src/a.ts": 'export const a = "x  y";\n'},
        )
        self.assertEqual(code, 0, out)
        self.assertIn("All 2 changed files", out)

    def test_comments_only_fails_on_a_code_change(self):
        code, out = self.comments_only({"server/src/a.ts": '// note\nexport const a = "x  y";\n'}, {"server/src/a.ts": 'export const a = "x y";\n'})
        self.assertEqual((code, out.splitlines()[0]), (1, "server/src/a.ts: code differs once comments are stripped"))

    def test_comments_only_fails_on_added_deleted_or_other_files(self):
        code, out = self.comments_only({"server/src/a.ts": "export const a = 1;\n", "notes.md": "a\n"}, {"server/src/a.ts": None, "server/src/b.ts": "export const a = 1;\n", "notes.md": "b\n"})
        self.assertEqual(code, 1, out)
        self.assertEqual(out.splitlines()[:3], ["notes.md: M on a file that is not Rust, Swift, TypeScript or JavaScript", "server/src/a.ts: status D", "server/src/b.ts: status A"])

if __name__ == "__main__":
    unittest.main()
