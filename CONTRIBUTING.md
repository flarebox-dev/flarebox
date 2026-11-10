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

The macOS jobs of CI run on the project's self-hosted Mac runners (label `flarebox-mini`), except for pull requests from forks, which run them on GitHub-hosted `macos-26` runners. To move every macOS job of CI to GitHub-hosted runners without changing code, set the repository variable `MACOS_RUNS_ON` to `["macos-26"]` in Settings → Secrets and variables → Actions → Variables. Delete the variable to go back to the self-hosted runners. The signed release always runs on GitHub-hosted runners.

Commit titles follow [Conventional Commits](https://www.conventionalcommits.org/), for example `feat(server): add commit validation`.
