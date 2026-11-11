# Status and problems

## Read the status

```sh
koi status
```

```
Device: laptop (019a...) on https://koidrive.alice.workers.dev
Daemon: running
/home/alice/Koidrive  space Koidrive (019a...)  UpToDate  seq 1204 of 1204, 3120 remote nodes, 3118 local entries, 0 questions
```

Each linked folder has one line. Its state is one of these:

| State | Meaning |
|---|---|
| `Stopped` | The daemon isn't running |
| `Connecting` | Opening a session with the server |
| `Bootstrapping` | Reading the space's whole file tree, the first time or after a long time offline |
| `CatchingUp` | Applying changes from other devices |
| `UpToDate` | The folder has the server's latest changes, and new ones arrive as they happen. Items that wait on a problem stay out of sync, and the line below names them |
| `Offline` | The server can't be reached. The folder keeps trying |
| `Halted` | A problem stopped the folder. The line below says which |

`seq` counts the changes the space has had: how many this device has applied, and how many the server has. A second line under the folder shows a problem, or the last failure the folder is retrying. `--json` prints all of it.

## Calls the server puts off

Sometimes the server asks the device to wait, or a call keeps failing for a while. The folder keeps retrying. Once a call has failed for a minute, or at once when the server asks for a wait of a minute or more, the status shows it with the time of the next try:

```
  uploading a file's content failed: ...; trying again at 14:05 UTC
```

### Quota waits

On the Workers Free plan, Cloudflare stops the server's Durable Objects for the rest of the day once they use up a daily allowance (see [Limits](limits.md)). The server then answers that it reached a daily limit, and the folder waits until the next 00:00 UTC, when the allowances reset:

```
  the server did not take a commit: QUOTA_EXCEEDED: the server reached a Workers Free daily limit; it resets at 00:00 UTC; trying again at 00:00 UTC
```

Nothing is lost while it waits. Changes on this device stay on disk and go out after the reset. In the first 30 minutes after midnight UTC the folder tries again every 5 minutes, because these errors can outlast the reset by a few minutes.

When a space's storage on the server is full, the server answers "the server's storage is full", and the folder retries on the same schedule. That doesn't clear at midnight. On the Workers Free plan each space can store 1 GB of file tree, history and change log, and the Workers Paid plan raises that to 10 GB ([Durable Objects limits](https://developers.cloudflare.com/durable-objects/platform/limits/)). File contents are in R2 and don't count toward it.

## Restart a halted folder

Some problems clear by themselves. A folder that was moved away, or whose root check fails, syncs again on its own once its path leads to the same folder again.

Other problems need you. Fix what the problem names, then:

```sh
koi restart ~/Koidrive
```

This stops the folder and syncs it again from the start. While the daemon isn't running, the folder syncs at the next start.

Problems that end with "this build cannot recover the folder yet" mean the folder's sync state and the server disagree in a way that this version can't resolve, for example after the space was rewound on the server. A restart doesn't fix them. To sync the space again, unlink the folder and link a new, empty folder to the space. Then copy over any files you changed in the old folder.

## Send refused changes again

When the server refuses a folder's changes for a reason that retrying won't fix, such as a device without access to the space, the folder halts. The problem line names the cause and ends with what to run once it is fixed. After the daemon restarts, it gives only the server's numeric error code, for example `code 2` when the device lacks access to the space or `code 3` when the server found the changes invalid. Run one of these two commands, not both.

For a cause you fixed on the server, such as access restored to this device, send the refused changes again as they were:

```sh
koi lift ~/Koidrive --resend
```

For a cause fixed on this device, plan the changes again into a new commit:

```sh
koi lift ~/Koidrive --replan
```

If the fix is a newer `koi`, install it and restart the daemon before you run `lift`, because `lift` runs in the daemon when one is running. A restart alone doesn't send the refused changes again. When the problem says the space's owner must confirm the changes, `lift` can't help, because Koidrive can't ask for that confirmation yet.

## Questions

```sh
koi questions
koi questions answer <id> <answer>
```

`koi questions` lists the questions a folder is waiting on, with the answers each accepts, and `answer` gives one. The number of open questions is at the end of each folder's status line.

Sync asks before it deletes many files at once. It holds this device's deletions when, within 10 minutes, they number more than 1,000, or they would remove more than half of the files of a space that has at least 20. A removed folder counts as one deletion of all the files it held. Sync then holds every deletion it has not sent yet and asks one question. Until you answer, the files stay on the server and on your other devices, and every other change keeps syncing, also after a restart.

- `delete` deletes the held files on the server and on every device.
- `restore` downloads them to this device again.

`koi questions` lists the first 20 held files and folders, and `--json` lists all of them. If the held deletions change before you answer, for example because you put a file back or deleted more, the question is replaced by a new one with a new ID. Sync keeps holding what is left until you answer, even when it no longer crosses a limit. An answer covers only the files its question listed: if you deleted more before sync took your answer, it asks again.

Sync also asks after the first link of a folder to a space that already held live files or folders, when the comparison holds some of the folder's files, as [Link a folder](folders.md#link-a-folder) describes. `koi questions` shows that question as "files on this device that the space does not hold", and it takes `publish` or `discard`.
