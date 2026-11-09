# Contributing to Flarebox

Flarebox is pre-alpha. Before working on anything large, open an issue so we can agree on the approach. Changes to the protocol in [spec/](spec/) affect both the server and the clients, so discuss them in an issue first.

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

Commit titles follow [Conventional Commits](https://www.conventionalcommits.org/), for example `feat(server): add commit validation`.
