# Split test scenarios

`../split-tests.py` moves Rust items according to `mapping.json`. Each source file has an explicit assignment for every test, helper, import, constant, type, and implementation. Unmapped items stop the command before it writes files.

Scenario files use `include!` in the original module. This preserves the fully qualified test names and private helper access. `common.rs` holds shared helpers once. The simulation's `proptest!` blocks stay in the original file so their regression-file lookup keeps the same source path.

The parser uses `syn` spans to copy source bytes, not pretty-printed syntax. `verify` compares every original item with its destination, including attributes, signatures, and bodies. It also checks the include list and rejects missing or duplicated items. `compare-lists` compares the full Cargo test-name multiset, not only a count.

## Regenerate and verify

Set a separate `CARGO_TARGET_DIR` for this checkout. The script runs its parser build at nice level 10.

```sh
export CARGO_TARGET_DIR="$(mktemp -d)"
python3 scripts/test-split/test_split.py
python3 scripts/split-tests.py split --source-ref <unsplit-revision>
python3 scripts/split-tests.py verify --source-ref <unsplit-revision>
```

`split` without `--source-ref` reads the working tree, including files already split. Repeating either form with the same inputs produces identical files. Edit `mapping.json` to assign a new item or change its scenario. Ordinary test changes can edit their scenario files directly.

Capture the test inventory before and after the split with the same command and environment:

```sh
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > before.txt
python3 scripts/split-tests.py split
nice -n 10 cargo test -p koidrive-driver -p koidrive-engine -p koidrive-sim -p koidrive-fs -- --list > after.txt
python3 scripts/split-tests.py compare-lists before.txt after.txt
```

## Port an open branch

Keep the old branch as a snapshot. Start a new local branch on the split main. Apply the old branch's non-test diff normally, excluding the four sources named in `mapping.json`. Then port their item-level changes:

```sh
python3 scripts/split-tests.py port --base-ref <old-merge-base> --branch-ref <old-head> \
  --assign 'core/engine/tests/commit.rs::fn:new_test=commit_moves_and_conflicts'
```

Supply one `--assign PATH::ITEM=SCENARIO` for each added test or helper. Helpers belong in `common`. The error lists unassigned items. `port` applies a three-way merge to each changed item, preserves main's unrelated items, and refuses overlapping edits before writing files. It does not switch branches, commit, or push. Run the crate tests after porting.
