# Uninstall

## Remove Koidrive from one device

Your files stay where they are.

1. Unlink each folder. Find them with `koi status`. Unlinking the last one stops and removes the [background service](install.md#background-sync):

   ```sh
   koi folder unlink ~/Koidrive
   ```

2. Sign the device out with `koi device leave`, or revoke it from another device with `koi device revoke <id>`. If this is the last active device or the server is unreachable, read [sign-out confirmation and owner recovery](devices.md#sign-this-device-out) before you add `--last-device` or delete the profile.
3. Delete Koidrive's files on this machine: `~/.local/state/koidrive` and `~/.config/koidrive`, or the `koidrive` folders under `$XDG_STATE_HOME` and `$XDG_CONFIG_HOME` when those are set to absolute paths, and the `.koidrive/` folder in each folder it linked. `.koidrive/trash/` holds files that sync replaced in the last 7 days, so look through it first.
4. Remove the command: delete `~/.local/bin/koi`, or run `cargo uninstall koidrive-cli` for a build from source.

The macOS app shows the same device as `koi`. If you installed only the app, run each step with `/Applications/Koidrive.app/Contents/Helpers/koi` in place of `koi` and skip step 4. Then quit the app and delete it.

## Remove everything

This deletes the server: the Worker, its Durable Objects and the R2 bucket with every file stored on the server, including the trash and old versions. It can't be undone. The copies in your linked folders stay on each device. To keep a copy of the bucket first, see [Back up the server](backup.md).

Without the [Cloudflare login](setup.md#set-up-with-the-cloudflare-login), delete the Worker and the R2 bucket in the Cloudflare dashboard. First check that no other Worker binds the bucket and that it holds nothing but the `spaces/` folder Koidrive writes, since emptying it deletes everything in it. R2 deletes a bucket only once it is empty ([Delete buckets](https://developers.cloudflare.com/r2/buckets/delete-buckets/)). Whether deleting the Worker in the dashboard also deletes its Durable Objects' data is unverified.

With the Cloudflare login, run this on a terminal:

```sh
koi server delete --interactive
```

You log in with Cloudflare. Koidrive finds the server, names the bucket and the number of objects in it, and asks before it deletes anything. It deletes the Worker, then empties and deletes the bucket. If another Worker in the account uses the same bucket, Koidrive keeps the bucket. If the delete stops halfway, run the command again to finish it. When this device syncs with the deleted server at its workers.dev address, Koidrive also stops and removes this device's background service.

- `--worker <name>` picks the server when the account holds several.
- `--yes` deletes without asking.
- `--bucket <name>` empties and deletes a bucket whose Worker is already gone. Add `--jurisdiction` when the bucket is in the `eu`, `us`, `fedramp` or `fedramp-high` jurisdiction.

Then remove Koidrive from each device as the previous section describes. Skip the revoke step, since the server is gone.
