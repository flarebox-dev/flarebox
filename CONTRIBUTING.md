# Contributing to Flarebox

Flarebox is pre-alpha. Issues are open to everyone. Until 1.0, only the project owner and invited maintainers open pull requests.

## Issues

Open an issue for a bug report, a reproduction, feedback on a deployment, or a feature request. Search the existing issues first, and add to a matching one instead of opening another. Remove credentials, such as API tokens, and personal data from logs, screenshots and files before you post them.

Do not report security vulnerabilities in issues. [SECURITY.md](SECURITY.md) explains how to report them privately.

## Pull requests

Each merged change needs review now and maintenance for as long as its code stays in Flarebox. To keep that work within what the maintainers can do, only the project owner and invited maintainers open pull requests until 1.0.

To propose a change, open an issue that describes the problem and the change you have in mind. Changes to the protocol in [spec/](spec/) affect both the server and the clients, so they need an issue too. An accepted issue is not an invitation to open a pull request.

Forks and experiments are welcome under the licenses below. You can link a fork or an experiment in an issue, but a link does not mean a maintainer will review or merge it.

We will revisit this policy after launch.

## AI assistance

You may use AI tools for reports and for invited changes. If you submit one, you are responsible for it:

- Check every claim it makes.
- Understand it well enough to answer questions about it.
- Say which tools you used and how much of the work they did.
- Give the commands that reproduce the problem or test the change.

Do not submit the output of an agent that ran without your supervision. Maintainers follow the same rules.

## Developer Certificate of Origin

Every commit must be signed off under the [Developer Certificate of Origin 1.1](https://developercertificate.org/). By adding a `Signed-off-by` line you certify the following:

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

Sign off with `git commit -s`, which adds a line like this, using your `user.name` and `user.email`:

```
Signed-off-by: Jane Doe <jane@example.com>
```

The name and email must match the commit author. The DCO check on every pull request fails if any commit lacks a matching sign-off. To sign off commits you already made on your branch, run `git rebase --signoff main` and force-push the branch.

## Licenses

Contributions are licensed under the license of the directory they land in: AGPL-3.0-or-later in `server/`, Apache-2.0 everywhere else. New source files start with an `SPDX-License-Identifier` comment for that license.

## Development

| Area | Tools | Checks |
|---|---|---|
| Rust (`core/`, `cli/`) | rustup; the toolchain is pinned in `rust-toolchain.toml` | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` |
| Server (`server/`) | Node.js 24, pnpm (version pinned in `server/package.json`) | `pnpm install`, `pnpm typecheck`, `pnpm test` |
| Schema (`spec/`) | [buf](https://buf.build/docs/cli/installation/) | `buf lint`, `buf generate` (run in `spec/`) |

Generated code is checked in: `server/src/gen/` (TypeScript) and `core/proto/src/gen/` (Rust). After changing a `.proto` file, run `buf generate` in `spec/` and commit the result. CI fails if the generated code is out of date.

CI runs these checks on Linux for every pull request. The macOS run of the Rust checks and the macOS app build, which also packages the unsigned release DMG, run on a pull request or in the merge queue only when the change touches a path they use, as listed in the `changes` job of `.github/workflows/ci.yml`. The server and spec checks run on macOS only on pushes to `main`, because nothing in them depends on macOS. A push to `main` runs every macOS job, and `ci-report` fails the push if one was skipped.

The macOS jobs of CI pick their runners by event. For every pull request, from this repository or a fork, they run on GitHub-hosted `xcode-27` runners. In the merge queue and on pushes to `main`, which run only code a maintainer queued or merged, they run on the project's self-hosted Mac runners (label `flarebox-mini`). To move the merge queue and `main` to GitHub-hosted runners as well without changing code, set the repository variable `MACOS_RUNS_ON` to `["xcode-27"]` in Settings → Secrets and variables → Actions → Variables. Delete the variable to go back to the self-hosted runners. A value that names the `self-hosted` label builds with `/Applications/Xcode.app`; any other value builds with Xcode 27.2 from `/Applications/Xcode_27.2.app`, as the `xcode-27` image provides it. The signed release always runs on GitHub-hosted `xcode-27` runners with Xcode 27.2.

That routing is a convenience, not the security boundary: a pull request runs the `ci.yml` from its own branch, so a fork could change `runs-on` to the self-hosted labels. The boundary is a job-started hook on each Mac runner together with one rule: only trusted code runs as `ghrunner`, the user that runs the jobs. Trusted code is code that a maintainer merged into `main` or put in the merge queue, together with the dependencies it pulls in. Putting a pull request in the merge queue is the trust step: from then on its code, a fork's included, runs on the Mac runners, so queue only a pull request whose changes you have reviewed. The hook, [`.github/runner/job-started.sh`](.github/runner/job-started.sh), runs before every job and reads the event payload. It allows only a push to `refs/heads/main` and a merge queue run for `main` (a `refs/heads/gh-readonly-queue/main/...` ref whose payload names `refs/heads/main` as its base) in `flarebox-dev/flarebox`. It refuses every other job, including pull requests from this repository, other branches and tags, `workflow_dispatch` and `schedule`, and it refuses a job whose event it cannot read. Its message shows under **Set up runner** in the job log, and the `runner hook` job of CI runs its tests and the installer's tests on the Mac runners. The hook does not check where a job's checkout comes from: a future workflow that runs in a trusted context (for example `push`, `workflow_dispatch` or `pull_request_target`) and checks out code from a fork needs a new security review before it may run on these runners. The repository also requires approval before a fork pull request from any outside contributor runs.

To install or update the hook, run `sudo .github/runner/install-hook.sh` from a checkout on the Mac that hosts the runners, then restart each runner while it is idle with `sudo launchctl kickstart -k system/dev.flarebox.runner-N`. The script copies the hook to `/usr/local/libexec/flarebox/job-started.sh`, owned by root. In each runner directory it writes `ACTIONS_RUNNER_HOOK_JOB_STARTED` into `.env` and a `PATH` with the system directories first into `.path`, makes both files owned by root and immutable (`chflags uchg`), and prints their owners and flags, whether `ghrunner` can write them, and the hook path. It fails if any of them is wrong or has unexpected contents. It reads and prepares every file before it changes anything, then updates one runner at a time and restores a runner's previous files if its update fails or the script gets SIGTERM, SIGINT or SIGHUP, so each runner ends up either fully installed or unchanged. It replaces `.path` with a fixed value and prints the old one; check that the old one held nothing the jobs need. Neither file may be writable by `ghrunner`: the runner loads `.env` each time it starts, over the LaunchDaemon's variables, and it looks up `bash` for the hook on that `PATH`. The locks stop a stray edit, not code that already runs as `ghrunner`, which owns the rest of the runner directory, including `runsvc.sh` and the runner binaries; that is why the rule above matters. A runner's self-update replaces only its `bin` and `externals` directories and leaves both files alone. `config.sh` rewrites them, so before you configure a runner again, run `sudo chflags nouchg` on both files and give them back to `ghrunner`, then run the install script again afterwards.

Commit titles follow [Conventional Commits](https://www.conventionalcommits.org/), for example `feat(server): add commit validation`.
