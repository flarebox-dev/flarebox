# Testing

The Rust checks that run on each pull request are the `rust` job of [CI](../.github/workflows/ci.yml), which skips a pull request whose changed files are all under `site/`. This page covers the property tests and the heavier runs that check them at scale.

## Property tests

The property tests use [proptest](https://docs.rs/proptest/1.12.0/proptest/). They live in the integration test files of `core/chunk`, `core/engine` and `core/sim` that contain a `proptest!` block; the largest is the interleaving oracle in `core/sim/tests/engine_sync.rs`. Each file sets its own case count for `cargo test`. Two environment variables override every file's settings:

- `PROPTEST_CASES`: the number of cases each property test runs.
- `PROPTEST_RNG_SEED`: a fixed seed (a whole number). The same seed, case count and commit produce the same cases. Without it, each run picks a random seed.

When a property fails, proptest prints the shrunk input and saves a line to a `.proptest-regressions` file next to the test file, for example `core/sim/tests/engine_sync.proptest-regressions`. Later runs replay the saved cases first. Commit the file when the failure is fixed so the case keeps being checked.

### Several devices

`core/sim/tests/devices.rs` runs two or three devices against one reference server while effects fail, messages are lost, repeated or reordered, commits are refused, and devices crash between store transactions or in the middle of an effect (`core/sim/src/world`). It has two properties:

- `devices_lose_no_write` fails when a write leaves a disk or the server's tree and no user overwrote or deleted it, or when the engine, the store or the server refuses a state. It runs on every pull request and in the battery.
- `devices_converge_once_the_faults_stop` also fails when the devices do not reach the server's tree, or when a commit makes a change that the user of its device never made.

The second can fail on `main`, because the reference server puts files into the trash that no user deleted (GUN-333). It is marked `#[ignore]`, together with the two fixed schedules of that failure. Neither `cargo test` nor the battery runs these tests. Until they pass, nothing checks convergence under every interleaving.

To run the ignored tests, and to print the state after every step of one of them:

```sh
cargo test -p koidrive-sim --test devices -- --ignored
KOIDRIVE_SIM_TRACE=1 cargo test -p koidrive-sim --test devices <test name> -- --ignored --nocapture
```

A pull request that fixes one of the issues removes the `#[ignore]` of its schedules.

## The property battery

The battery runs the property tests at high case counts on GitHub Actions instead of a local machine (`.github/workflows/property-battery.yml`). One job builds the test binaries of every package that depends on proptest and keeps each binary that compiles a file with a `proptest!` block, including files it pulls in with `mod` or `include!` and library unit tests. The build fails if a file with a `proptest!` block is in no test binary, or if the filter selects no test. Then many shards run those binaries with the same case count and a different seed each, and a shard fails if a binary runs a different number of tests than it lists for the filter. Shard `k` uses `PROPTEST_RNG_SEED` = run ID × 1000 + `k`, so every run covers new cases and every failure can be reproduced. A summary job lists each shard's seed, result and log, and the number of cases run.

It runs every night on `main` with the defaults: 24 shards of 8192 cases. Start it for a branch with `scripts/battery.sh`, which needs [gh](https://cli.github.com/) signed in to an account that can run workflows on the repository:

```sh
scripts/battery.sh                         # the current branch, as pushed
scripts/battery.sh --cases 16384 --shards 48 feat/my-branch
scripts/battery.sh --filter a_device --wait
```

It prints the run URL and returns; with `--wait` it follows the run and exits with its result. The options:

| Option | Workflow input | Default |
|---|---|---|
| `REF` | the branch or tag the workflow runs on, which is the code it tests | the current branch |
| `--cases N` | `cases`: `PROPTEST_CASES` on each shard | 8192 |
| `--shards N` | `shards`: the number of shards, at most 256 | 24 |
| `--filter TEXT` | `filter`: a test name filter, as for `cargo test` | none |
| `--mutations` | `mutations`: also run the mutation set | off |

The workflow always runs on the branch or tag it tests, using that ref's own copy of the workflow file, so the ref must contain it. This keeps untrusted code out of other branches' caches: any job can write the Actions cache of the ref its run belongs to, so a branch's battery can only write that branch's cache, never `main`'s.

### When a shard fails

The shard's job summary prints one command per failing test target, for example:

```sh
git checkout <commit> && PROPTEST_CASES=8192 PROPTEST_RNG_SEED=<seed> cargo test --locked -p koidrive-sim --test engine_sync
```

Running it reproduces the failure locally. The shard's `failure-shard-k` artifact holds the seed, the commands, the test logs and any new `.proptest-regressions` files. Copying a regression file into the checkout replays the shrunk case directly, without the full case count.

## The mutation set

`scripts/engine-mutations.sh` lists mutations of the engine's commit and effects code that the tests must catch, each a perl substitution on one file. `scripts/mutations.sh` applies them one at a time, runs the unit tests (`cargo test -p koidrive-engine --test commit`), the interleaving property (`cargo test -p koidrive-sim --test engine_sync`, up to three runs of 4096 cases), both, or the tests of several devices (`cargo test -p koidrive-sim --test devices`, up to three runs of 4096 cases), and restores the file. It first checks that the tests pass without mutations. A mutation fails the run when:

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
