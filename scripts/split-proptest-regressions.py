# SPDX-License-Identifier: Apache-2.0

import argparse
from pathlib import Path
import re


def split(path: Path) -> int:
    lines = path.read_text().splitlines(keepends=True)
    saved = []
    for line in lines:
        if line.startswith("cc "):
            match = re.fullmatch(r"cc ([0-9a-f]{64})(?:\s*#.*)?\s*", line)
            if match is None:
                raise ValueError(f"Invalid cc line in {path}: {line!r}")
            destination = path.with_suffix(".cases") / f"cc-{match[1]}.txt"
            data = line.rstrip("\r\n") + "\n"
            if destination.exists() and destination.read_text() != data:
                raise ValueError(f"Different regression already exists in {destination}")
            saved.append((destination, data))
    for destination, data in saved:
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(data)
    path.write_text("".join(line for line in lines if not line.startswith("cc ")))
    return len(saved)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Move cc lines into one file per saved case.")
    parser.add_argument("paths", type=Path, nargs="+")
    for path in parser.parse_args().paths:
        print(f"{path}: moved {split(path)} saved cases")
