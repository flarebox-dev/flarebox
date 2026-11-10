# Uninstall

## Remove Flarebox from one device

Your files stay where they are.

1. Unlink each folder. Find them with `flarebox status`:

   ```sh
   flarebox unlink ~/Flarebox
   ```

2. Stop the daemon. On Linux with the systemd unit, run `systemctl --user disable --now flarebox`.
3. Revoke the device from another device with `flarebox device revoke <id>`.
4. Delete Flarebox's files on this machine: `~/.local/state/flarebox` and `~/.config/flarebox`, or the `flarebox` folders under `$XDG_STATE_HOME` and `$XDG_CONFIG_HOME` when those are set, and the `.flarebox/` folder in each folder it linked. `.flarebox/trash/` holds files that sync replaced in the last 7 days, so look through it first.
5. Remove the command with `cargo uninstall flarebox-cli`. If you copied it to `~/.local/bin` for the systemd unit, delete that copy and `~/.config/systemd/user/flarebox.service` too.

The macOS app is a separate device with its own profile. To remove it, quit the app, revoke its device from another device, and delete the app and `~/Library/Application Support/Flarebox`.

## Remove everything

This deletes the server: the Worker, its Durable Objects and the R2 bucket with every file stored on the server, including the trash and old versions. It can't be undone. The copies in your linked folders stay on each device.

```sh
flarebox server uninstall
```

You log in with Cloudflare. Flarebox finds the server, names the bucket and the number of objects in it, and asks before it deletes anything. It deletes the Worker, then empties and deletes the bucket. If another Worker in the account uses the same bucket, Flarebox keeps the bucket. If the uninstall stops halfway, run the command again to finish it.

- `--server <worker>` picks the server when the account holds several.
- `--yes` deletes without asking, for scripts.
- `--bucket <name>` empties and deletes a bucket whose Worker is already gone. Add `--jurisdiction` when the bucket is in the `eu`, `us`, `fedramp` or `fedramp-high` jurisdiction.

Then remove Flarebox from each device as the previous section describes. Skip the revoke step, since the server is gone.

> [!WARNING]
> `flarebox server uninstall` needs the Cloudflare login, which isn't available outside the project's test account yet. Until then, delete the Worker and the R2 bucket in the Cloudflare dashboard. First check that no other Worker binds the bucket and that it holds nothing but the `spaces/` folder Flarebox writes, since emptying it deletes everything in it. R2 deletes a bucket only once it is empty ([Delete buckets](https://developers.cloudflare.com/r2/buckets/delete-buckets/)). Whether deleting the Worker in the dashboard also deletes its Durable Objects' data is unverified.
