# Uninstall

## Remove Koidrive from one device

Your files stay where they are.

1. Unlink each folder. Find them with `koi status`. Unlinking the last one stops and removes the [background service](install.md#background-sync):

   ```sh
   koi folder unlink ~/Koidrive
   ```

2. Revoke the device from another device with `koi device revoke <id>`.
3. Delete Koidrive's files on this machine: `~/.local/state/koidrive` and `~/.config/koidrive`, or the `koidrive` folders under `$XDG_STATE_HOME` and `$XDG_CONFIG_HOME` when those are set to absolute paths, and the `.koidrive/` folder in each folder it linked. `.koidrive/trash/` holds files that sync replaced in the last 7 days, so look through it first.
4. Remove the command: delete `~/.local/bin/koi`, run `brew uninstall koidrive` for Homebrew, or run `cargo uninstall koidrive-cli` for a build from source.

The macOS app is a separate device with its own profile. To remove it, quit the app, revoke its device from another device, and delete the app, `~/Library/Application Support/Koidrive`, and the `.koidrive/` folder in each folder the app linked. Look through each folder's `.koidrive/trash/` before you delete it. Until that folder is gone, another device can't link the folder.

## Remove everything

This deletes the server: the Worker, its Durable Objects and the R2 bucket with every file stored on the server, including the trash and old versions. It can't be undone. The copies in your linked folders stay on each device. To keep a copy of the bucket first, see [Back up the server](backup.md).

```sh
koi server delete --interactive
```

You log in with Cloudflare. Koidrive finds the server, names the bucket and the number of objects in it, and asks before it deletes anything. It deletes the Worker, then empties and deletes the bucket. If another Worker in the account uses the same bucket, Koidrive keeps the bucket. If the delete stops halfway, run the command again to finish it. When this device syncs with the deleted server at its workers.dev address, Koidrive also stops and removes this device's background service.

- `--worker <name>` picks the server when the account holds several.
- `--yes` deletes without asking.
- The command logs in with Cloudflare in a browser, so it runs only with `--interactive` on a terminal. Otherwise it stops with exit status 4 and prints the same command with `--interactive`.
- `--bucket <name>` empties and deletes a bucket whose Worker is already gone. Add `--jurisdiction` when the bucket is in the `eu`, `us`, `fedramp` or `fedramp-high` jurisdiction.

Then remove Koidrive from each device as the previous section describes. Skip the revoke step, since the server is gone.

> [!WARNING]
> `koi server delete` needs the Cloudflare login, which isn't available outside the project's test account yet. Until then, delete the Worker and the R2 bucket in the Cloudflare dashboard. First check that no other Worker binds the bucket and that it holds nothing but the `spaces/` folder Koidrive writes, since emptying it deletes everything in it. R2 deletes a bucket only once it is empty ([Delete buckets](https://developers.cloudflare.com/r2/buckets/delete-buckets/)). Whether deleting the Worker in the dashboard also deletes its Durable Objects' data is unverified.
