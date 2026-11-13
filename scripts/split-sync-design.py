#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
from collections import Counter
import re
from pathlib import Path
import subprocess
import unicodedata
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parent.parent
SOURCE = Path("docs/design/sync-engine.md")
DIRECTORY = SOURCE.with_suffix("")
SECTIONS = {
    "Summary": "summary.md",
    "Changes from the approved draft": "approved-draft-changes.md",
    "1. Context": "context.md",
    "2. State model": "state-model.md",
    "3. Correctness properties": "correctness.md",
    "4. The server's total order": "server-order.md",
    "5. Local file system edge cases": "filesystem.md",
    "6. Mass-deletion safety": "mass-deletion.md",
    "7. Testing": "testing.md",
    "8. Is a TLA+ model worth it?": "formal-model.md",
    "9. Interfaces with other tracks": "interfaces.md",
    "10. Spec changes and questions": "spec-questions.md",
    "References": "references.md",
}
LINK = re.compile(r"(?<=\]\()([^\s)]+)(?=\))")
EXPLICIT = re.compile(r'<a\s+id="([^"]+)"')


def headings(source):
    found = []
    offset = 0
    fence = None
    for line in source.splitlines(keepends=True):
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            if fence is None:
                fence = marker[1]
            elif (
                marker[1][0] == fence[0]
                and len(marker[1]) >= len(fence)
                and re.fullmatch(r"[ \t]*(?:\r?\n)?", line[marker.end():])
            ):
                fence = None
        elif fence is None and (match := re.match(r"^(#{1,6}) (.+?)\s*$", line)):
            found.append((offset, len(match[1]), match[2]))
        offset += len(line)
    return found


def slug(text):
    return "".join(char for char in text.lower()
                   if char in "-_ " or unicodedata.category(char)[0] in "LMN").replace(" ", "-")


def anchors(source):
    seen = Counter()
    result = {}
    for offset, level, title in headings(source):
        name = slug(title)
        suffix = seen[name]
        seen[name] += 1
        result[name + (f"-{suffix}" if suffix else "")] = offset
    for match in EXPLICIT.finditer(source):
        if match[1] in result:
            raise ValueError(f"duplicate anchor: {match[1]}")
        result[match[1]] = match.start()
    return result


def generate(source):
    if "[Design index](sync-engine/README.md)" in source:
        raise ValueError("source reference is already split; choose the original unsplit revision")
    titles = headings(source)
    starts = [(offset, title) for offset, level, title in titles if level == 2]
    if [title for _, title in starts] != list(SECTIONS):
        raise ValueError("section headings changed; update SECTIONS explicitly before splitting")
    sections = {"README.md": source[:starts[0][0]]}
    for index, (offset, title) in enumerate(starts):
        end = starts[index + 1][0] if index + 1 < len(starts) else len(source)
        sections[SECTIONS[title]] = source[offset:end]
    owners = {}
    for filename, text in sections.items():
        for name in anchors(text):
            if name in owners:
                raise ValueError(f"anchor spans multiple sections: {name}")
            owners[name] = filename
    if set(owners) != set(anchors(source)):
        raise ValueError("splitting changes the anchor inventory")
    rewritten = 0

    def relocate(filename, text):
        def destination(match):
            nonlocal rewritten
            target = match[0]
            if target.startswith("#"):
                owner = owners[unquote(target[1:])]
                if owner != filename:
                    rewritten += 1
                    return owner + target
            elif not urlsplit(target).scheme and not target.startswith("/"):
                raise ValueError(f"relative file link needs an explicit relocation: {target}")
            return target

        return LINK.sub(destination, text)

    planned = {DIRECTORY / filename: relocate(filename, text) for filename, text in sections.items()}
    index = planned[DIRECTORY / "README.md"]
    legacy = source[:source.index("\n") + 1] + "\n[Design index](sync-engine/README.md)\n\n"
    for offset, level, title in titles:
        if level == 1:
            continue
        name = slug(title)
        owner = owners[name]
        index += f'{"  " * (level - 2)}- [{title}]({owner}#{name})\n'
        legacy += f'{"#" * level} {title}\n\n[Read section](sync-engine/{owner}#{name})\n\n'
    for name in EXPLICIT.findall(source):
        legacy += f'<a id="{name}"></a>[{name}](sync-engine/{owners[name]}#{name})\n\n'
    planned[DIRECTORY / "README.md"] = index
    planned[SOURCE] = legacy
    return {path: text.rstrip("\n") + "\n" for path, text in planned.items()}, rewritten


def verify(source, planned, rewritten):
    for path, expected in planned.items():
        if (ROOT / path).read_bytes() != expected.encode():
            raise ValueError(f"file differs from the mechanical split: {path}")
    old_anchors = set(anchors(source))
    if not old_anchors <= set(anchors((ROOT / SOURCE).read_text())):
        raise ValueError("the compatibility index lost an original anchor")
    for path, text in planned.items():
        for target in LINK.findall(text):
            parsed = urlsplit(target)
            if parsed.scheme or target.startswith("/"):
                continue
            resolved = path.parent / unquote(parsed.path) if parsed.path else path
            if resolved not in planned:
                raise ValueError(f"missing local link target in {path}: {target}")
            if parsed.fragment and unquote(parsed.fragment) not in anchors(planned[resolved]):
                raise ValueError(f"missing link anchor in {path}: {target}")
    inbound = subprocess.run(
        ["git", "grep", "-n", "-o", "-E", r"sync-engine\.md#[A-Za-z0-9_-]+", "--", "docs", "core", "spec"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if inbound.returncode not in (0, 1):
        raise ValueError(inbound.stderr)
    for line in inbound.stdout.splitlines():
        if line.rsplit("#", 1)[1] not in old_anchors:
            raise ValueError(f"unresolved inbound anchor: {line}")
    print(f"PASS {len(SECTIONS)} section bodies unchanged except {rewritten} cross-file link destinations and boundary newlines")
    print(f"PASS {len(old_anchors)} original anchors retained at {SOURCE}")
    print("PASS every generated local link resolves to an existing file and anchor")
    print(f"PASS git grep: {len(inbound.stdout.splitlines())} inbound anchored links checked")


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("command", choices=("split", "verify"))
    cli.add_argument("--source-ref", required=True)
    args = cli.parse_args()
    source = subprocess.check_output(["git", "show", f"{args.source_ref}:{SOURCE}"], cwd=ROOT).decode()
    planned, rewritten = generate(source)
    if args.command == "split":
        for path, text in planned.items():
            (ROOT / path).parent.mkdir(parents=True, exist_ok=True)
            (ROOT / path).write_bytes(text.encode())
    verify(source, planned, rewritten)


if __name__ == "__main__":
    main()
