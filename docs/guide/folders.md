# Folders

## Link a folder

```sh
flarebox link ~/Flarebox --space Flarebox
```

`--space` takes the space's name or its ID. If two spaces have the same name, use the ID. The folder must exist. It may already hold files, and those files are uploaded. Files the space already holds are downloaded into it.

The first sync can't yet merge a folder and a space that hold the same name at their top level. If both hold `Notes`, for example, the folder halts with `the folder and the space it was linked to both hold "Notes"`. To combine such a folder with a space, link an empty folder, wait until the space has downloaded, then move your files in.

A space syncs to at most one folder on each device. Flarebox refuses a folder that:

- is inside another linked folder, or holds one;
- holds Flarebox's own state or configuration folder, or sits inside it;
- another device linked and hasn't unlinked;
- is on a file system without a rename that refuses to replace its target, which Flarebox needs to write files safely.

The folder syncs while [the daemon](install.md#run-the-sync-daemon) runs. To stop syncing it:

```sh
flarebox unlink ~/Flarebox
```

Unlinking keeps every file. It also keeps the folder's `.flarebox/` folder, whose [local trash](#the-local-trash) may still hold files that sync replaced. Delete `.flarebox/` yourself once you no longer need them.

## What syncs

Flarebox syncs folders and regular files: a file's bytes and whether it is executable.

It doesn't sync:

- symbolic links, sockets, FIFOs and device files. Flarebox skips them and never follows a symbolic link inside a linked folder;
- permissions other than the executable bit, the owner and group, ACLs and extended attributes such as macOS Finder tags;
- hard links as links. Each name of a hard-linked file syncs as a separate file;
- the platform files `.DS_Store`, `Thumbs.db`, `desktop.ini` and `Icon\r`, and the `.flarebox/` folder at the top of a linked folder. No rule can bring them back. When sync removes a folder that another device deleted, it deletes these platform files in it too, so that the folder can go. A `.flarebox` folder deeper down is an ordinary folder and syncs.

Names compare without regard to case or Unicode normalization. Two names in one folder that differ only in case, such as `Report.txt` and `report.txt`, can exist on a Linux disk but not in a space, so the second to arrive is renamed as a [conflict](conflicts.md#two-files-with-the-same-name). A name is at most 255 bytes, and a path at most 256 folders deep.

## Leave files out with .fbignore

A `.fbignore` file uses [`.gitignore` syntax](https://git-scm.com/docs/gitignore#_pattern_format) and can sit in any folder of a space. Its patterns apply to its folder and everything below it:

```gitignore
# Build output and disk images
build/
*.iso
!keep.iso
```

- A deeper `.fbignore` takes precedence over a shallower one, and within a file the last matching pattern wins.
- `.fbignore` files sync like other files, so every device follows the same rules.
- Patterns match names regardless of case and Unicode normalization, so a pattern means the same on every file system. `.FBIGNORE` is a rules file too.
- As in git, a file can't be brought back with `!` when a folder above it is ignored, and a `.fbignore` inside an ignored folder has no effect.

Ignoring a path never deletes it. The device that ignores a path stops syncing it: the files stay on that device's disk, on the server and on the other devices. A new file in an ignored path stays on the device where you created it.

There is one exception. If you move a synced file into an ignored folder while the daemon isn't running, and Flarebox can't tell afterwards that it was moved, it counts the file as deleted. The server keeps it in the [trash](history.md) for 30 days.

## Sync only part of a space on this device

`~/.config/flarebox/config.toml`, or `$XDG_CONFIG_HOME/flarebox/config.toml` when that variable is set, holds rules for this device alone. They apply to every folder it links:

```toml
[sync]
include = ["Projects/"]
exclude = ["*.iso", "build/"]
```

With `include`, the device syncs only the listed paths, what is under them, and the folders that lead to them. `exclude` takes `.fbignore` patterns and narrows that further. A path is left out when the device's rules or any `.fbignore` file leave it out, so a `.fbignore` file can't widen what this device syncs. Restart the daemon after you change the file.

These rules save disk space and bandwidth. They don't hide anything: the server still sends this device the names of every file in the space, and the device's credential can read every file.

## The local trash

When sync replaces or removes a file in a linked folder, Flarebox moves the old file to `.flarebox/trash/` in that folder instead of deleting it. A file stays there for 7 days. While the trash holds more than 2048 MB, the oldest files leave first. Set another size in `config.toml`:

```toml
[trash]
max_size_mb = 4096
```

The local trash is separate from the space's trash on the server, which [Trash, versions and restore](history.md) covers.
