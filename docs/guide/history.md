# Trash, versions and restore

The server keeps what you delete and the earlier contents of files you change, for 30 days each:

- A deleted file or folder stays in the space's trash for 30 days.
- When a file's content changes, the earlier version stays for 30 days after it was replaced. The current version of a file never expires.

After that, the server removes them the next time it cleans up, so don't count on restoring them later. These commands read the server, so the device must be online. They don't need the daemon.

## See what is in the trash

```sh
koi trash ~/Koidrive
```

This lists what was deleted in the linked folder and is still in the trash, with the size of each item. A deleted folder is one line, with the number of files and bytes that went to the trash with it. Name a folder inside a linked folder to list only what was deleted under it. A folder deleted as a whole is listed only at its own path: if `notes/` went to the trash with everything in it, `koi trash ~/Koidrive/notes/drafts` lists nothing, and `koi trash ~/Koidrive` shows `notes/`. Without a path, the command uses the current folder. `--json` prints each item with its node ID.

## See the versions of a file

```sh
koi versions ~/Koidrive/notes.txt
```

```
notes.txt: 2 versions, newest first
current       20 B  2 h ago         019a...e1
              10 B  1 d ago         019a...7c
```

The last column is the version ID. `--json` prints each version with its size, the device that wrote it and when.

## Restore a deleted file or folder

```sh
koi restore ~/Koidrive/notes/old.txt
```

Name the path the file or folder had. Koidrive finds the deletion in the trash and brings it back where it was, with its versions. A folder comes back with everything that went to the trash with it. A file that went to the trash with its folder can come back alone: the folders above it come back too, without their other files.

When several deletions had the same path, the command lists them and asks you to pick one with `--node <id>`. `koi trash --json` shows the node IDs.

## Restore an earlier version

```sh
koi restore ~/Koidrive/notes.txt --version 019a...7c
```

Take the version ID from `koi versions`. The old content becomes the file's current content, as a new version. The content it replaces stays as an earlier version for 30 days.

## How a restore goes

`koi restore` checks the request against the server, records it, and returns. The daemon sends it with the folder's next change to the server, after any changes to the same file that this device hasn't sent yet. The restored file then arrives in the folder like a change from another device. If the daemon isn't running, the request waits until it starts.

```sh
koi restores ~/Koidrive
```

This lists the restores asked for in the folder's space and how each went:

- **restored**: the file or folder is back.
- **restored as `<name>`, since its name was taken**: something else now has its name, so it came back under a conflict name.
- **restored as a new file `<name>`, since another device changed the file meanwhile**: the earlier version came back as a [conflict copy](conflicts.md#two-edits-of-one-file) next to the file.
- **refused**: the server said why. For example, the item was deleted for good after 30 days.
- **not sent**: it brings back more files and folders than one change can hold.
- **pending**: it hasn't been sent yet. The list says what the folder is waiting for, such as a stopped daemon or a [problem](status.md).

## See what each space stores

```sh
koi usage
```

This shows, for each space, the bytes in live files, in the trash and in old versions, then the unreferenced bytes and the total. Unreferenced chunks are uploads not committed yet and chunks that wait out a grace period before the server deletes them. A chunk shared by several files counts once.
