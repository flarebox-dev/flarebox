# Usage reports

Koidrive can send anonymous usage reports to the Koidrive project. They help fix what breaks: which setup steps fail and where Koidrive crashes. Reports never hold your files' names, paths or contents, your server's address or your account.

## Turn reports on or off

The guided `koi setup` on a terminal asks once, after setup succeeds. Turning reports on here applies to future commands, not the setup that just finished:

```
Send reports? [Y/n]
```

Press Enter or type `y` to send them, or type `n` to not send them. Change your answer at any time:

```sh
koi telemetry status    # whether reports are sent, why, and what waits to be sent
koi telemetry on
koi telemetry off       # also deletes the install id and the reports not sent yet
koi telemetry preview   # prints the waiting reports as the daemon would send them
koi telemetry reset     # draws a new install id and deletes the reports not sent yet
```

The other commands never ask. They send reports only when this profile already chose to send them, or when `KOIDRIVE_TELEMETRY` turns them on. `koi setup`, `koi join`, `koi server deploy`, `koi server add-device` and `koi server claim` also take `--telemetry on` or `--telemetry off`, which stores the choice without asking. `koi setup` doesn't ask with `--json`, or when the `CI` variable is set to any value. With `--interactive` it asks unless `CI` is set to something other than `0` or `false`.

A running daemon notices `koi telemetry off` and `reset` at its next ten-minute tick. A daemon that started while reports were off records nothing of its own until it restarts, but it still sends what later commands queue. When the daemon stops, it writes what it holds to disk and leaves the sending to its next run.

These environment variables override the choice:

| Variable | Effect |
|---|---|
| `DO_NOT_TRACK=1` | Turns reports off. It wins over everything else |
| `KOIDRIVE_TELEMETRY=0` or `off` | Turns reports off |
| `KOIDRIVE_TELEMETRY=1` or `on` | Turns reports on |
| `KOIDRIVE_TELEMETRY=debug` | Prints each report to stderr instead of sending it |

The choice belongs to one profile on one machine. It lives in `telemetry/settings.json` in the profile's state directory (`~/.local/state/koidrive` by default), never in `config.toml`.

## What a report holds

Every report carries:

- A random install id, 32 hex characters, drawn when you turn reports on. It isn't derived from your machine, your account or your Koidrive device, and it stays until you reset it.
- Which program sent it (the daemon, a command or the macOS app), the Koidrive version, and whether it's a release or development build.
- The operating system, its major and minor version, the Linux distribution when it's a common one, and the processor architecture.
- The day, in UTC. No finer time is sent.

On top of that, reports hold:

- How many times each `koi` command group, such as `koi folder`, ran in a day. Only the group's name, never its action or arguments.
- The steps of `koi setup` and `koi server ...` (log in, R2, bucket, Worker upload, serving, claim, pairing), `koi server upgrade` and `koi server delete`: whether each one succeeded and roughly how long it took.
- Where Koidrive panicked: the source file and line in Koidrive's own code. Never the panic's message.
- Exact daily totals from the daemon: commits sent and received, bytes of file content uploaded and downloaded, and the day's highest number of linked spaces and of the files and folders they hold, trash included. Only totals over all your spaces, never a count per space or per file.
- How many times each kind of error happened in a day: why a folder stopped syncing, how an upload or download failed, the code of a server refusal and of each item a refused commit left out, each commit too large to send and each item it left out, which kind of question a folder asked, and why the daemon could not start. Never the error's message.

Later versions add sync timings to this page as they add them to the reports.

## What a report never holds

- File and folder names, paths, contents, or hashes of any of them. Sizes are sent only as daily totals.
- Space, device, user and node ids, and the names of spaces, devices and users.
- Your server's address, Worker and bucket names, and your Cloudflare account id.
- Pairing codes, claim codes, recovery keys, tokens or credentials.
- Error messages, log lines, stack traces, `.koiignore` rules and configuration values.
- Your host name or user name.

Your own Koidrive server sends nothing to the project. Its Worker logs stay in your Cloudflare account.

## Where reports go

Reports go to `https://tel.koidrive.com`, which forwards them to the project's [PostHog](https://posthog.com) project. Each report is an anonymous PostHog event: PostHog makes no person profile for it and doesn't look up a location from your address. A report waits on your machine for up to 14 days while it can't be sent, and is then dropped.
