#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import bisect
import re
import subprocess
import sys

LANGUAGES = {
    ".rs": "rust",
    ".swift": "swift",
    ".ts": "js",
    ".tsx": "js",
    ".mts": "js",
    ".cts": "js",
    ".js": "js",
    ".mjs": "js",
    ".cjs": "js",
}
ALLOWED = re.compile(r"SPDX-License-Identifier|SAFETY:|@generated|eslint-|prettier-ignore|swiftlint:|https?://")
MARKER = re.compile(r"\b(TODO|FIXME|XXX|HACK)\b|\bfor now\b", re.IGNORECASE)
INTERNAL = re.compile(r"^(server|apps/macos/Koidrive|cli/tests)/|(^|/)(tests?|Tests)/|(^|/)tests\.rs$|\.test\.[cm]?[jt]sx?$")
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
WORD = re.compile(r"\w+")
IDENT = re.compile(r"[A-Za-z0-9_$]+")
RUST_RAW = re.compile(r'[bc]?r(#*)"')
RUST_CHAR = re.compile(r"'(\\.[^'\n]*|[^\\'\n])'")
SWIFT_RAW = re.compile(r'(#+)("""|")')
REGEX_BEFORE = set("(,=:[!&|?{};+-*%<>~^") | {"", "return", "typeof", "case", "in", "of", "delete", "void", "throw", "yield", "await"}

MESSAGES = {
    "marker": "TODO, FIXME, XXX, HACK and 'for now' belong in an issue, not in the code.",
    "doc": "New doc comment on internal code (tests, server/, the macOS app, cli/tests). Doc comments go only on API that other packages call, so let the name or the type say it.",
    "comment": "New comment. Say it in code: a name, a type, a test or an assertion message. A comment may stay only as a license header, a SAFETY: note, a tool pragma, or a line that links the source of an external constraint.",
}


def comment_spans(src, lang, tokens=None):
    found = []
    n = len(src)

    def block(i):
        depth, j = 1, i + 2
        while j < n and depth:
            if lang != "js" and src.startswith("/*", j):
                depth, j = depth + 1, j + 2
            elif src.startswith("*/", j):
                depth, j = depth - 1, j + 2
            else:
                j += 1
        found.append((i, j))
        return j

    def quoted(i, quote, multiline, interpolation):
        j = i + len(quote)
        while j < n and not src.startswith(quote, j):
            if src[j] == "\n" and not multiline:
                return j
            if interpolation and src.startswith(interpolation, j):
                j = code(j + len(interpolation), interpolation[-1].translate(str.maketrans("({", ")}")))
            elif src[j] == "\\":
                j += 2
            else:
                j += 1
        return j + len(quote)

    def regex(i):
        j, in_class = i + 1, False
        while j < n and src[j] != "\n":
            if src[j] == "\\":
                j += 2
                continue
            if src[j] == "[":
                in_class = True
            elif src[j] == "]":
                in_class = False
            elif src[j] == "/" and not in_class:
                return j + 1
            j += 1
        return i + 1

    def token(i, prev):
        c = src[i]
        if lang == "rust":
            m = RUST_RAW.match(src, i)
            if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
                end = src.find('"' + m.group(1), m.end())
                return n if end < 0 else end + 1 + len(m.group(1))
            if c in "bc" and src.startswith('"', i + 1):
                return quoted(i + 1, '"', True, None)
            if c == '"':
                return quoted(i, '"', True, None)
            if c == "'":
                m = RUST_CHAR.match(src, i)
                return m.end() if m else i + 1
        if lang == "swift":
            m = SWIFT_RAW.match(src, i)
            if m:
                end = src.find(m.group(2) + m.group(1), m.end())
                return n if end < 0 else end + len(m.group(2)) + len(m.group(1))
            if src.startswith('"""', i):
                return quoted(i, '"""', True, "\\(")
            if c == '"':
                return quoted(i, '"', False, "\\(")
        if lang == "js":
            if c in "\"'":
                return quoted(i, c, False, None)
            if c == "\x60":
                return quoted(i, "\x60", True, "\${")
            if c == "/" and prev in REGEX_BEFORE:
                return regex(i)
        m = IDENT.match(src, i)
        return m.end() if m else i + 1

    def code(i, closer):
        depth, prev = 0, ""
        while i < n:
            c = src[i]
            if c.isspace():
                i += 1
            elif src.startswith("//", i):
                j = src.find("\n", i)
                j = n if j < 0 else j
                found.append((i, j))
                i = j
            elif src.startswith("/*", i):
                i = block(i)
            elif closer and c == closer and depth == 0:
                return i + 1
            else:
                if c in "([{":
                    depth += 1
                elif c in ")]}":
                    depth -= 1
                start = i
                i = token(i, prev)
                prev = src[start:i]
                if tokens is not None:
                    tokens.append(prev)
        return i

    code(src.find("\n") if src.startswith("#!") else 0, None)
    return found


def is_doc(text, lang):
    if lang == "rust":
        return (text.startswith("///") and not text.startswith("////")) or text.startswith(("//!", "/*!")) or (
            text.startswith("/**") and not text.startswith(("/**/", "/***"))
        )
    if lang == "swift":
        return text.startswith(("///", "/**"))
    return text.startswith("/**") and not text.startswith("/**/")


def comment_lines(src, lang):
    newlines = [k for k, ch in enumerate(src) if ch == "\n"]
    lines = {}
    for start, end in comment_spans(src, lang):
        text = src[start:end]
        doc = is_doc(text, lang)
        first = bisect.bisect_left(newlines, start) + 1
        for offset, piece in enumerate(text.split("\n")):
            lines.setdefault(first + offset, []).append((doc, piece))
    return lines


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True).stdout.decode("utf-8", "replace")


def is_subsequence(words, run):
    it = iter(run)
    return all(w in it for w in words)


def code_tokens(src, lang):
    tokens = []
    comment_spans(src, lang, tokens)
    return tokens


def differences_beyond_comments(base, head):
    changed = git("-c", "core.quotePath=false", "diff", "--name-status", "--no-renames", base, head).splitlines()
    problems = []
    for entry in changed:
        status, path = entry.split("	", 1)
        lang = LANGUAGES.get("." + path.rsplit(".", 1)[-1])
        if status != "M" or not lang:
            problems.append(f"{path}: {status} on a file that is not Rust, Swift, TypeScript or JavaScript" if status == "M" else f"{path}: status {status}")
        elif code_tokens(git("show", f"{base}:{path}"), lang) != code_tokens(git("show", f"{head}:{path}"), lang):
            problems.append(f"{path}: code differs once comments are stripped")
    return len(changed), problems


def check(base, head):
    diff = git("-c", "core.quotePath=false", "diff", "-U0", "-M", "--no-color", "--no-ext-diff", base, head, "--", *("*" + s for s in LANGUAGES))
    files, current = [], None
    for line in diff.split("\n"):
        if line.startswith("--- "):
            current = {"old": None if line == "--- /dev/null" else line[6:], "removed": [], "added": []}
            files.append(current)
        elif line.startswith("+++ "):
            current["new"] = None if line == "+++ /dev/null" else line[6:]
        elif current and (m := HUNK.match(line)):
            old, old_count, new, new_count = (int(g) if g is not None else 1 for g in m.groups())
            current["removed"].extend(range(old, old + old_count))
            current["added"].extend(range(new, new + new_count))

    runs = []
    for f in files:
        if not f["old"] or not f["removed"]:
            continue
        lang = LANGUAGES["." + f["old"].rsplit(".", 1)[-1]]
        old_lines = comment_lines(git("show", f"{base}:{f['old']}"), lang)
        previous = None
        for number in f["removed"]:
            for _, piece in old_lines.get(number, []):
                if previous is None or number > previous + 1:
                    runs.append([])
                runs[-1].extend(w.lower() for w in WORD.findall(piece))
                previous = number

    problems = []
    for f in files:
        path = f.get("new")
        if not path or not f["added"] or "/gen/" in path:
            continue
        lang = LANGUAGES["." + path.rsplit(".", 1)[-1]]
        new_lines = comment_lines(git("show", f"{head}:{path}"), lang)
        for number in f["added"]:
            for doc, piece in new_lines.get(number, []):
                words = [w.lower() for w in WORD.findall(piece)]
                if not words:
                    continue
                if MARKER.search(piece):
                    problems.append((path, number, "marker", piece))
                elif any(is_subsequence(words, run) for run in runs):
                    continue
                elif ALLOWED.search(piece):
                    continue
                elif not doc:
                    problems.append((path, number, "comment", piece))
                elif INTERNAL.search(path):
                    problems.append((path, number, "doc", piece))
    return problems


def main():
    parser = argparse.ArgumentParser(
        description="Fail on comment lines that the diff from BASE to HEAD adds to Rust, Swift, TypeScript and JavaScript files. A comment line that keeps only words of a comment the same diff removes counts as a trim, not as new."
    )
    parser.add_argument("base", metavar="BASE", help="commit or tree to compare against, such as the merge base with main")
    parser.add_argument("head", metavar="HEAD", nargs="?", default="HEAD", help="commit or tree to check (default: HEAD)")
    parser.add_argument("--comments-only", action="store_true", help="instead, fail unless every changed file is a modified source file whose code is identical once comments are stripped")
    args = parser.parse_args()
    if args.comments_only:
        count, problems = differences_beyond_comments(args.base, args.head)
        for problem in problems:
            print(problem)
        if problems:
            print(f"{len(problems)} of {count} changed files change more than comments between {args.base} and {args.head}.")
            return 1
        print(f"All {count} changed files between {args.base} and {args.head} have identical code once comments are stripped.")
        return 0
    problems = check(args.base, args.head)
    for path, number, kind, piece in problems:
        print(f"::error file={path},line={number}::{MESSAGES[kind]}")
        print(f"    {piece.strip()}")
    if problems:
        print(f"{len(problems)} comment lines added between {args.base} and {args.head}. Remove them, or reshape the code so it reads without them.")
        return 1
    print(f"No new comments between {args.base} and {args.head}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
