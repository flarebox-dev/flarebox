#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import argparse
import re
import shlex
import unittest


WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/ci.yml"


def job(workflow, name):
    body = workflow.split(f"\n  {name}:\n", 1)[1]
    return re.split(r"\n  \S", body, maxsplit=1)[0]


def test_arguments(body, marker):
    commands = [shlex.split(line) for line in re.findall(r"cargo test[^\n]+", body) if marker in line]
    if len(commands) != 2:
        raise ValueError("Each partition must have one listing and one execution command")
    listing, execution = commands
    if "--" not in listing or listing[listing.index("--") + 1:listing.index("--") + 2] != ["--list"]:
        raise ValueError("The inventory must list the executed tests")
    if listing[:listing.index("--")] != execution:
        raise ValueError("Inventory and execution must use identical Cargo arguments")
    return execution


def check_partitions(workflow):
    remainder = test_arguments(job(workflow, "rust"), "--workspace")
    parser = argparse.ArgumentParser(add_help=False, exit_on_error=False)
    parser.add_argument("--workspace", action="store_true")
    parser.add_argument("--exclude", action="append", default=[])
    parser.add_argument("-p", "--package", action="append", default=[])
    parser.add_argument("--features", action="append", default=[])
    parser.add_argument("--locked", action="store_true")
    remaining, unknown = parser.parse_known_args(remainder[2:])
    if unknown:
        raise ValueError("The remainder must not filter default test targets")
    if not remaining.workspace or remaining.package:
        raise ValueError("The remainder must select the workspace")
    excluded = remaining.exclude
    shards = job(workflow, "rust-tests")
    matrix = re.search(r"(?m)^        suite: \[([^\]]+)\]$", shards)
    if matrix is None:
        raise ValueError("Expected a literal Rust suite matrix")
    packages = ["koidrive-" + name.strip(" '\"") for name in matrix[1].split(",")]
    if len(excluded) != len(set(excluded)) or len(packages) != len(set(packages)):
        raise ValueError("Rust partitions must not contain duplicate packages")
    if set(excluded) != set(packages):
        raise ValueError("Every excluded package must run in exactly one Rust shard")
    command = test_arguments(shards, "koidrive-$SUITE")
    selected, unknown = parser.parse_known_args(command[2:])
    if unknown:
        raise ValueError("Rust shards must not filter default test targets")
    if selected.package != ["koidrive-$SUITE"] or selected.workspace or selected.exclude:
        raise ValueError("Each Rust shard must execute its matrix package")
    features = re.findall(r"(?m)^          FEATURES: (.+)$", shards)
    if len(features) != 2 or features[0] != features[1]:
        raise ValueError("Inventory and execution must use identical feature expressions")


class RustPartitions(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_every_excluded_package_runs_once(self):
        check_partitions(self.workflow)

    def test_missing_exclusion_is_rejected(self):
        changed = self.workflow.replace(" --exclude koidrive-sim", "")
        with self.assertRaisesRegex(ValueError, "Every excluded package"):
            check_partitions(changed)

    def test_missing_shard_is_rejected(self):
        changed = self.workflow.replace("suite: [driver, engine, sim]", "suite: [driver, engine]")
        with self.assertRaisesRegex(ValueError, "Every excluded package"):
            check_partitions(changed)

    def test_duplicate_shard_is_rejected(self):
        changed = self.workflow.replace("suite: [driver, engine, sim]", "suite: [driver, engine, sim, sim]")
        with self.assertRaisesRegex(ValueError, "duplicate packages"):
            check_partitions(changed)

    def test_different_listing_selection_is_rejected(self):
        changed = self.workflow.replace(" --exclude koidrive-sim", "", 1)
        with self.assertRaisesRegex(ValueError, "identical Cargo arguments"):
            check_partitions(changed)

    def test_additional_shard_package_is_rejected(self):
        changed = self.workflow.replace('-p "koidrive-$SUITE"', '-p "koidrive-$SUITE" -p koidrive-cli')
        with self.assertRaisesRegex(ValueError, "execute its matrix package"):
            check_partitions(changed)

    def test_shard_target_filter_is_rejected(self):
        changed = self.workflow.replace('-p "koidrive-$SUITE"', '-p "koidrive-$SUITE" --lib')
        with self.assertRaisesRegex(ValueError, "not filter default test targets"):
            check_partitions(changed)

    def test_remainder_target_filter_is_rejected(self):
        changed = self.workflow.replace("cargo test --workspace", "cargo test --workspace --lib")
        with self.assertRaisesRegex(ValueError, "not filter default test targets"):
            check_partitions(changed)

    def test_different_inventory_features_are_rejected(self):
        changed = self.workflow.replace("FEATURES: ${{ matrix.suite", "FEATURES: changed ${{ matrix.suite", 1)
        with self.assertRaisesRegex(ValueError, "identical feature expressions"):
            check_partitions(changed)


if __name__ == "__main__":
    unittest.main()
