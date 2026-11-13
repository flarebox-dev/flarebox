# Self-hosted Mac runners

The project's Mac hosts four GitHub Actions runners, labelled `flarebox-mini`, which run as the user `ghrunner` in `/Users/ghrunner/runner-1` to `runner-4`. CI sends macOS jobs to them only when the repository variable `MACOS_RUNS_ON` is unset or names `self-hosted`. When it names only hosted labels, such as `["xcode-27"]`, every macOS job runs on GitHub-hosted runners and these runners take no jobs. To see which mode is in force:

```sh
gh variable get MACOS_RUNS_ON
```

No other workflow uses these runners.

## Choose where macOS jobs run

To run every macOS job on GitHub-hosted runners, set `MACOS_RUNS_ON` to a JSON array without `self-hosted`, for example `["xcode-27"]`, under **Settings**, **Secrets and variables**, **Actions**, **Variables**. CI then picks the hosted label for each job, whatever the array holds:

| CI job | Hosted label | Toolchain |
| --- | --- | --- |
| `rust (macos)` | `macos-15` | The Rust toolchain in `rust-toolchain.toml` and the image's Apple command-line tools |
| `server (macos)` | `macos-15` | Node 24 and pnpm |
| `spec (macos)` | `macos-15` | Buf |
| `runner hook` | `macos-15` | Shell scripts that test the hook and its installer without root |
| `macos app` | `xcode-27` | Xcode 27.2, the toolchain of `release-macos.yml`, through `DEVELOPER_DIR=/Applications/Xcode_27.2.app/Contents/Developer` |

To use these runners, delete the variable, which selects `["self-hosted","flarebox-mini"]`, or set it to another array that contains `self-hosted`. Then:

- Pushes to `main` and the merge queue run their macOS jobs here.
- A pull request from a branch of `koidrive/koidrive` sends each macOS job to an idle runner here. When none is idle, the job runs on its hosted label while fewer than five hosted macOS jobs of the repository are queued or running, and queues here otherwise. The `pick-macos` job lists the runners with the `RUNNER_STATUS_TOKEN` secret. If that secret is missing, the listing fails, or no runner is online, every macOS job of the pull request runs on hosted runners.
- A pull request from a fork always runs its macOS jobs on hosted runners.
- Jobs here keep a Rust target directory per runner (`.github/actions/self-hosted-rust`) and use `/Applications/Xcode.app/Contents/Developer`.

## Only trusted code runs as ghrunner

A pull request runs the `ci.yml` from its own branch, so a fork can point `runs-on` at these runners. The [job-started hook](#the-job-started-hook-refuses-other-jobs) refuses those jobs. Beyond that, only trusted code runs as `ghrunner`: code that someone with write access pushed to a branch of this repository, merged into `main` or put in the merge queue, with the dependencies it pulls in.

Pushing a branch and queuing a fork's pull request are therefore trust decisions. Push or queue only code you have reviewed, and give write access only to people you trust that far. The repository also requires approval before a fork pull request from an outside contributor runs.

## The job-started hook refuses other jobs

[`job-started.sh`](job-started.sh) runs before every job and reads the event payload. It allows only these jobs in `koidrive/koidrive`:

- a push to `refs/heads/main`
- a merge queue run for `main`: a `refs/heads/gh-readonly-queue/main/...` ref whose payload names `refs/heads/main` as its base
- a `pull_request` run on `refs/pull/N/merge` whose payload names `koidrive/koidrive` as the repository of its head branch and N as its number

It refuses every other job, including fork pull requests, `pull_request_target`, pushes to other branches and tags, `workflow_dispatch`, `schedule`, and any job whose event it cannot read. Its message shows under **Set up runner** in the job log.

The hook does not check where a job's checkout comes from. A new workflow that runs in a trusted context, such as `push`, `workflow_dispatch` or `pull_request_target`, and checks out code from a fork needs a security review before it runs here.

The `runner hook` job of CI tests the hook and the installer in the checkout, not the copies installed on the Mac. The installer's verification table and the hook's message under **Set up runner** show what is installed.

## Install or update the hook

1. On the Mac, run `sudo .github/runner/install-hook.sh` from a checkout. It copies the hook to `/usr/local/libexec/flarebox/job-started.sh`, owned by root. In each runner directory it writes `ACTIONS_RUNNER_HOOK_JOB_STARTED` into `.env` and a `PATH` with the system directories first into `.path`, and makes both files owned by root and immutable (`chflags uchg`). It prints a verification table and fails if a file is wrong.
2. Check the old `.path` that the installer prints. The new file replaces it, so make sure it held nothing the jobs need.
3. Restart each runner while it is idle: `sudo launchctl kickstart -k system/dev.flarebox.runner-N`.

The runner loads `.env` each time it starts, over the LaunchDaemon's variables, and looks up `bash` for the hook on that `PATH`, so `ghrunner` must not be able to write either file. The locks stop a stray edit, not code that already runs as `ghrunner`, which owns the rest of the runner directory. That is why the trust rule matters.

## If the installer fails

The installer updates one runner at a time. If it fails or is interrupted, it restores the runner it was updating, stops, and names the runners it already updated. launchd restarts a runner whenever it exits and at boot, with whatever `.env` it finds, and after a failed first install that file has no hook. So:

1. Stop each affected runner: `sudo launchctl bootout system/dev.flarebox.runner-N`.
2. Fix the cause and run the installer again until its verification passes.
3. Start each runner: `sudo launchctl bootstrap system /Library/LaunchDaemons/dev.flarebox.runner-N.plist`.

If restoring a runner fails too, the installer names the directory that holds its previous `.env` and `.path` (`backup.env`, `backup.path`). Restore them by hand.

## Configure a runner again

`config.sh` rewrites `.env` and `.path`. Before you run it, unlock both files with `sudo chflags nouchg` and give them back to `ghrunner`. Run the installer again afterwards.
