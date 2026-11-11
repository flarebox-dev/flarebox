#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Renames Flarebox to Koidrive in the working tree (GUN-391).

Run it from the root of a checkout, on main or on any branch that started before the rename:

    python3 scripts/rename-to-koidrive.py

It renames tracked paths with git mv, rewrites the text of tracked files, updates Cargo.lock for
the renamed crates, formats the Rust code again, regenerates the code in server/src/gen and
core/proto/src/gen, and then lists every line that still holds an old name, with the reason it
stays. It exits 1 when such a line has no reason. Running it again changes nothing.

The command is koi. Everything else takes Koidrive: crates, paths, environment variables,
service names, bundle IDs, the proto package, docs and the site.
"""

import re
import subprocess
import sys
from pathlib import Path

SELF = "scripts/rename-to-koidrive.py"

# Strings that keep the old name, with the reason. They name things outside the repository.
# The self-hosted runner names move together later, because that takes sudo on the runner Mac and
# registering the runners again.
KEEP = {
    "flarebox-mini": "the label the self-hosted Mac runners are registered with; it moves when they are registered again",
    "dev.flarebox.runner-": "the launchd label of the installed Mac runners; it moves when they are installed again",
    "libexec/flarebox": "where install-hook.sh puts the runner hook; it moves with the runner labels, because it needs sudo on the runner Mac",
}

SUBCOMMANDS = "setup|daemon|join|link|unlink|status|questions|lift|restart|restore|restores|trash|versions|usage|space|server|device|pair"

# Uses of the lowercase name that mean the command, or that a plain replacement would get wrong.
# Applied in order, before the plain replacements below.
SPECIAL = [
    # The fl ligature made a full-folding alias of the reserved .flarebox. Nothing full-folds into
    # "koidrive", so the alias becomes the Kelvin sign, which folds to k.
    (r'"name": "\.\\ufb02arebox",\n(\s*)"nfc": "\.\\ufb02arebox",', r'"name": ".\\u212aoidrive",\n\1"nfc": ".Koidrive",'),
    (r"reserved at the root through full folding", "reserved at the root through the Kelvin sign"),
    (r"\\u\{FB02\}arebox", r"\\u{212A}oidrive"),
    (r"\\uFB02arebox", r"\\u212Aoidrive"),
    (r"/\^\\\.\\uFB02AREBOX ", r"/^\\.KOIDRIVE "),
    (r"\\uFB02AREBOX", r"\\u212AOIDRIVE"),
    (r"a full-casefold alias of", "a Kelvin-sign alias of"),
    (r"`\.\uFB02arebox`", "`.\u212Aoidrive` (with the Kelvin sign)"),
    ("\uFB02arebox", "\u212Aoidrive"),
    (r"The name was free on 2026-11-10: `brew info flarebox` found no formula and no cask\.",
     "The name was free on 2026-11-11: `brew info koidrive` found no formula and no cask."),
    (r"flarebox-dev/flarebox", "koidrive/koidrive"),
    (r"flarebox-dev/", "koidrive/"),
    (r"flarebox-dev\b", "koidrive"),
    (r"(?<![\w.-])flarebox\.dev\b", "koidrive.com"),
    (r"(?<![\w./-])flarebox(?= (?:" + SUBCOMMANDS + r")\b| --(?:version|help)\b)", "koi"),
    (r"`flarebox`(?= command)", "`koi`"),
    (r"\b(your|a|puts|newer) `flarebox`(?! formula)", r"\1 `koi`"),
    (r"\bthe flarebox command\b", "the koi command"),
    (r"(?<=#with-)flarebox(?=-setup\))", "koi"),
    (r"\b(this|newer|the|no|destination|Installed) flarebox\b(?![\w-])", r"\1 koi"),
    (r"(?<=bin/)flarebox(?![\w.-])", "koi"),
    (r"(?<=release/)flarebox(?![\w.-])", "koi"),
    (r"(?<=package/)flarebox(?![\w.-])", "koi"),
    (r"(?:(?<=package )|(?<=package\" ))flarebox(?= )", "koi"),
    (r'(?<=\$archive" )flarebox(?= >)', "koi"),
    (r"(?<=\$tmp/)flarebox(?![\w.-])", "koi"),
    (r"(?<=%s/)flarebox(?![\w.-])", "koi"),
    (r"(?<=install_dir/)flarebox(?![\w.-])", "koi"),
    (r"CARGO_BIN_EXE_flarebox\b", "CARGO_BIN_EXE_koi"),
    (r'(\[\[bin\]\]\nname = )"flarebox"', r'\1"koi"'),
    (r'"flarebox(: | \{)', r'"koi\1'),
    (r'\["flarebox", ', '["koi", '),
    (r'"flarebox"(?=,\s+"(?:' + SUBCOMMANDS + r')")', '"koi"'),
    (r'(#\[command\(\s*name = )"flarebox"', r'\1"koi"'),
    (r'"run flarebox"', '"run koi"'),
    (r"'flarebox install: ", "'koi install: "),
]

PLAIN = [
    ("FLAREBOX", "KOIDRIVE"),
    ("FlareBox", "KoiDrive"),
    ("Flarebox", "Koidrive"),
    ("flarebox", "koidrive"),
    ("FBIGNORE", "KOIIGNORE"),
    ("Fbignore", "Koiignore"),
    ("fbignore", "koiignore"),
]

OLD = re.compile(r"flarebox|fbignore|fb02arebox|\ufb02arebox", re.IGNORECASE)


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def same_case(match):
    return "".join(new.upper() if old.isupper() else new for old, new in zip(match.group(0), "koidrive"))


def plain(text):
    for old, new in PLAIN:
        text = text.replace(old, new)
    return re.sub("flarebox", same_case, text, flags=re.IGNORECASE)


def convert(text):
    kept = {}
    for n, token in enumerate(KEEP):
        mark = f"\0KEEP{n}\0"
        kept[mark] = token
        text = text.replace(token, mark)
    for pattern, replacement in SPECIAL:
        text = re.sub(pattern, replacement, text)
    text = plain(text)
    for mark, token in kept.items():
        text = text.replace(mark, token)
    return text


def rename_paths():
    moved = 0
    for path in git("ls-files", "-z").split("\0"):
        if not path or path == SELF or not OLD.search(path):
            continue
        target = plain(path)
        Path(target).parent.mkdir(parents=True, exist_ok=True)
        git("mv", path, target)
        moved += 1
    for directory in sorted({p for p in Path(".").rglob("*") if p.is_dir() and ".git" not in p.parts}, key=lambda p: -len(p.parts)):
        if OLD.search(str(directory)) and not any(directory.iterdir()):
            directory.rmdir()
    return moved


def rewrite_files():
    changed = 0
    for path in git("ls-files", "-z").split("\0"):
        if not path or path == SELF:
            continue
        file = Path(path)
        if file.is_symlink() or not file.is_file():
            continue
        try:
            text = file.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        new = convert(text)
        if new != text:
            file.write_text(new, encoding="utf-8")
            changed += 1
    return changed


def update_cargo():
    try:
        subprocess.run(["cargo", "update", "--workspace", "--offline", "--quiet"], check=True)
        subprocess.run(["cargo", "fmt", "--all"], check=True)
    except FileNotFoundError:
        print("cargo is not on PATH: run cargo update --workspace and cargo fmt --all", file=sys.stderr)


def regenerate_spec():
    try:
        subprocess.run(["buf", "generate"], cwd="spec", check=True)
    except FileNotFoundError:
        print("buf is not on PATH: run buf generate in spec/, because the generated descriptors hold the old package name", file=sys.stderr)


def report():
    left = subprocess.run(["git", "grep", "-n", "-I", "-i", "-e", "flarebox", "-e", "fbignore", "-e", "arebox", "--", ".", f":!{SELF}"],
                          capture_output=True, text=True).stdout.splitlines()
    unexplained = 0
    print(f"{SELF}: holds the old names it replaces")
    for line in left:
        reason = next((why for token, why in KEEP.items() if token in line), None)
        if reason is None:
            unexplained += 1
            reason = "NO REASON: convert this line or add it to KEEP"
        print(f"{line}\n    -> {reason}")
    binary = subprocess.run(["git", "grep", "-l", "-i", "--", "flarebox", ".", f":!{SELF}"],
                            capture_output=True, text=True).stdout.split()
    for path in sorted(set(binary) - {line.split(":", 1)[0] for line in left}):
        unexplained += 1
        print(f"{path}: a binary file holds an old name -> NO REASON")
    return unexplained


def main():
    if not Path(".git").exists() or not Path(SELF).exists():
        sys.exit(f"run {SELF} from the root of the repository")
    moved = rename_paths()
    changed = rewrite_files()
    update_cargo()
    regenerate_spec()
    print(f"Renamed {moved} paths and rewrote {changed} files.\n")
    print("Lines that keep an old name:")
    sys.exit(1 if report() else 0)


if __name__ == "__main__":
    main()
