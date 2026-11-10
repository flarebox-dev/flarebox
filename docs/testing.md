# Testing

The Rust checks that run on every pull request are listed in [CONTRIBUTING.md](../CONTRIBUTING.md). This page covers the property tests and the heavier runs that check them at scale.

## Property tests

The property tests use [proptest](https://docs.rs/proptest/1.12.0/proptest/). They live in the integration test files of `core/chunk`, `core/engine` and `core/sim` that contain a `proptest!` block; the largest is the interleaving oracle in `core/sim/tests/engine_sync.rs`. Each file sets its own case count for `cargo test`. Two environment variables override every file's settings:

- `PROPTEST_CASES`: the number of cases each property test runs.
- `PROPTEST_RNG_SEED`: a fixed seed (a whole number). The same seed, case count and commit produce the same cases. Without it, each run picks a random seed.

When a property fails, proptest prints the shrunk input and saves a line to a `.proptest-regressions` file next to the test file, for example `core/sim/tests/engine_sync.proptest-regressions`. Later runs replay the saved cases first. Commit the file when the failure is fixed so the case keeps being checked.

## The property battery

The battery runs the property tests at high case counts on GitHub Actions instead of a local machine (`.github/workflows/property-battery.yml`). One job builds the test binaries; then many shards run every property test target with the same case count and a different seed each. Shard `k` uses `PROPTEST_RNG_SEED` = run ID × 1000 + `k`, so every run covers new cases and every failure can be reproduced. A summary job lists each shard's seed, result and log, and the number of cases run.

It runs every night on `main` with the defaults: 24 shards of 8192 cases. Start it for a branch with `scripts/battery.sh`, which needs [gh](https://cli.github.com/) signed in to an account that can run workflows on the repository:

```sh
scripts/battery.sh                         # the current branch, as pushed
scripts/battery.sh --cases 16384 --shards 48 feat/my-branch
scripts/battery.sh --filter a_device --wait
```

It prints the run URL and returns; with `--wait` it follows the run and exits with its result. The options:

| Option | Workflow input | Default |
|---|---|---|
| `REF` | `ref`: the branch, tag or commit to test | the current branch |
| `--cases N` | `cases`: `PROPTEST_CASES` on each shard | 8192 |
| `--shards N` | `shards`: the number of shards, at most 256 | 24 |
| `--filter TEXT` | `filter`: a test name filter, as for `cargo test` | none |
| `--mutations` | `mutations`: also run the mutation set | off |

The workflow file is read from `main`. To try a changed workflow from another branch, set `BATTERY_WORKFLOW_REF` to that branch.

### When a shard fails

The shard's job summary prints one command per failing test target, for example:

```sh
git checkout <commit> && PROPTEST_CASES=8192 PROPTEST_RNG_SEED=<seed> cargo test --locked -p flarebox-sim --test engine_sync
```

Running it reproduces the failure locally. The shard's `failure-shard-k` artifact holds the seed, the commands, the test logs and any new `.proptest-regressions` files. Copying a regression file into the checkout replays the shrunk case directly, without the full case count.

## The mutation set

`scripts/engine-mutations.sh` lists mutations of the engine's commit and effects code that the tests must catch, each a perl substitution on one file. `scripts/mutations.sh` applies them one at a time, runs the unit tests (`cargo test -p flarebox-engine --test commit`), the interleaving property (`cargo test -p flarebox-sim --test engine_sync`, up to three runs of 4096 cases), or both, and restores the file. It first checks that the tests pass without mutations. A mutation fails the run when:

- no test catches it (`SURVIVED`),
- its pattern no longer matches the source (`STALE`), or
- the mutated code does not compile (`BROKEN`).

Update a stale mutation when the code it targets changes. When the code is removed, remove the mutation as well.

The battery runs the set with `--mutations`, spread over up to as many jobs as there are shards. To run it locally on a clean checkout (it edits files and restores them with git):

```sh
scripts/mutations.sh               # every mutation
scripts/mutations.sh --shard 0/4   # every fourth mutation, starting with the first
```

`MUTATION_CASES` changes the property case count and `MUTATION_SEED` fixes the seeds.
