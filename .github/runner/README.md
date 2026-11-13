# Self-hosted Mac runners

The project's Mac hosts four runners (label `flarebox-mini`), which run as the user `ghrunner` in `/Users/ghrunner/runner-1` to `runner-4`. When `MACOS_RUNS_ON` is unset, the merge queue and pushes to `main` run their macOS jobs there. For a pull request from a branch of `koidrive/koidrive`, the `pick-macos` job of CI sends each macOS job to an idle runner there or to a GitHub-hosted runner. Pull requests from forks always run their macOS jobs on GitHub-hosted runners.

## Trust rule

A pull request runs the `ci.yml` from its own branch, so a fork can point `runs-on` at these runners. The job-started hook below refuses those jobs. Beyond that, one rule holds: only trusted code runs as `ghrunner`. Trusted code is code that someone with write access pushed to a branch of this repository, merged into `main` or put in the merge queue, together with the dependencies it pulls in.

Pushing a branch and queuing a fork's pull request are trust steps, because that code then runs on these runners. Push or queue only code you have reviewed, and give write access only to people you trust that far. The repository also requires approval before a fork pull request from an outside contributor runs.

## The job-started hook

[`job-started.sh`](job-started.sh) runs before every job and reads the event payload. It allows only these jobs in `koidrive/koidrive`:

- a push to `refs/heads/main`
- a merge queue run for `main`: a `refs/heads/gh-readonly-queue/main/...` ref whose payload names `refs/heads/main` as its base
- a `pull_request` run on `refs/pull/N/merge` whose payload names `koidrive/koidrive` as the repository of its head branch and N as its number

It refuses every other job, including pull requests from forks, `pull_request_target`, pushes to other branches and tags, `workflow_dispatch` and `schedule`. It also refuses a job whose event it cannot read. Its message shows under **Set up runner** in the job log.

The hook does not check where a job's checkout comes from. A new workflow that runs in a trusted context, such as `push`, `workflow_dispatch` or `pull_request_target`, and checks out code from a fork needs a security review before it runs on these runners.

The `runner hook` job of CI tests the hook and the installer from the checkout, not the copy installed on the Mac. The installer's verification table and the hook's message under **Set up runner** show what is installed.

## Install or update the hook

1. On the Mac, run `sudo .github/runner/install-hook.sh` from a checkout. It copies the hook to `/usr/local/libexec/flarebox/job-started.sh`, owned by root. In each runner directory it writes `ACTIONS_RUNNER_HOOK_JOB_STARTED` into `.env` and a `PATH` with the system directories first into `.path`, and makes both files owned by root and immutable (`chflags uchg`). It prints a verification table and fails if a file is wrong.
2. Check the old `.path` that the installer prints. The new one replaces it, so make sure it held nothing the jobs need.
3. Restart each runner while it is idle: `sudo launchctl kickstart -k system/dev.flarebox.runner-N`.

Neither file may be writable by `ghrunner`. The runner loads `.env` each time it starts, over the LaunchDaemon's variables, and looks up `bash` for the hook on that `PATH`. The locks stop a stray edit, not code that already runs as `ghrunner`, which owns the rest of the runner directory. That is why the trust rule matters.

If the installer fails or is interrupted, it restores the runner it was updating and stops, and it names the runners it already updated. launchd restarts a runner whenever it exits and at boot, with whatever `.env` it finds, which after a failed first install has no hook. So stop each affected runner with `sudo launchctl bootout system/dev.flarebox.runner-N` and keep it offline until the installer runs to the end and its verification passes. Then start it with `sudo launchctl bootstrap system /Library/LaunchDaemons/dev.flarebox.runner-N.plist`.

`config.sh` rewrites both files. Before you configure a runner again, run `sudo chflags nouchg` on `.env` and `.path` and give them back to `ghrunner`. Run the installer again afterwards.

## Move the macOS jobs off these runners

Set the repository variable `MACOS_RUNS_ON` to `["macos-15"]` in Settings → Secrets and variables → Actions → Variables. An array without `self-hosted` selects hosted mode. The existing value `["xcode-27"]` selects the same mode. CI chooses the hosted label for each job, not from this array.

| CI job | Hosted label | Toolchain |
| --- | --- | --- |
| `rust (macos)` | `macos-15` | The Rust toolchain in `rust-toolchain.toml` and the runner's default Apple command-line tools. |
| `server (macos)` | `macos-15` | Node 24 and pnpm for TypeScript and Worker tests. |
| `spec (macos)` | `macos-15` | Buf for lint, code generation, and compatibility checks. |
| `runner hook` | `macos-15` | Shell scripts that test the hook and its installer without root. |
| `macos app` | `xcode-27` | Xcode 27.2, matching the toolchain used by `release-macos.yml`. |

The ordinary hosted jobs use the image's default developer tools. Only the hosted app job sets `DEVELOPER_DIR` to `/Applications/Xcode_27.2.app/Contents/Developer`.

Delete the variable to use `["self-hosted","flarebox-mini"]`, or set it to another JSON array containing `self-hosted` and the labels of those runners. Self-hosted jobs keep their per-runner Rust target directories and use `/Applications/Xcode.app/Contents/Developer`. Pull requests that overflow the idle self-hosted runners use the hosted labels above while fewer than five hosted macOS jobs are queued or running. That count includes both hosted labels. Jobs beyond that limit queue on the self-hosted runners.
