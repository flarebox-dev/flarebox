# Back up the server

The server keeps everything in your Cloudflare account. The R2 bucket holds the contents of every file and an export of each space's tree, trash and old versions. The Durable Objects hold the same tree, trash and versions, and the server works from them. Your linked folders hold only the live files.

If you delete the bucket or lose the Cloudflare account and have no copy elsewhere, nobody can get back what only the server held: the trash, the old versions, and every file that no device still has. Koidrive has no backup of its own. This page shows how to copy the bucket to storage outside Cloudflare with [rclone](https://rclone.org/), and what that copy can and can't restore.

> [!IMPORTANT]
> In 0.1.0 the copy can bring back objects the bucket lost while the server still runs. It can't bring back a server you lost with its account, because 0.1.0 has no tool that turns the copy into files. [After you lose the account](#after-you-lose-the-account) says what the copy keeps.

## What the bucket holds

Koidrive writes only under `spaces/`. The commands on this page write the bucket's name as `<bucket>`:

- `koi setup` names it `koidrive-` followed by 16 hex digits, and prints the name when it creates the bucket ("Creating the R2 bucket ...").
- The deploy button uses the name you kept or chose when you deployed, `koidrive` by default.

The **R2 object storage** page of the Cloudflare dashboard lists the account's buckets.

The bucket holds:

- `spaces/<space_id>/chunks/<2 hex>/<hex>/<incarnation>`: the pieces of file content, each stored as it is and named by its SHA-256.
- `spaces/<space_id>/meta/<history_id>/`: the export of the space's tree, trash and old versions, in the format [RFC 0004](../../spec/rfc/0004-r2-export-format.md) describes.

The server writes the export after it accepts changes, once 1,000 commits from devices are waiting, or 5 minutes after the first commit it hasn't exported. So a copy can miss the last few minutes of changes.

Koidrive doesn't encrypt chunks. Anyone who can read the copy can read your files, so keep it as private as the bucket.

## Copy the bucket

> [!NOTE]
> Nobody has run these steps against R2 yet. They follow Cloudflare's [rclone guide](https://developers.cloudflare.com/r2/examples/rclone/) and [R2 authentication](https://developers.cloudflare.com/r2/api/tokens/) pages. The filters and the copy behaviour were checked with rclone 1.76 between local folders.

1. In the Cloudflare dashboard, go to **R2 object storage**, then **Manage** next to **API Tokens**. Create a token with the **Object Read only** permission, scoped to `<bucket>`. Note its **Access Key ID** and **Secret Access Key**, and your account ID.
2. Install rclone 1.59 or later. Earlier versions can fail with `HTTP 401: Unauthorized` against R2, as Cloudflare's [rclone guide](https://developers.cloudflare.com/r2/examples/rclone/) notes.
3. Add a remote for R2 to the rclone configuration file. `rclone config file` prints where it is.

   ```ini
   [r2]
   type = s3
   provider = Cloudflare
   access_key_id = <Access Key ID>
   secret_access_key = <Secret Access Key>
   endpoint = https://<account ID>.r2.cloudflarestorage.com
   acl = private
   no_check_bucket = true
   ```

   A token with an Object permission needs `no_check_bucket = true`. If the bucket is in a jurisdiction, the endpoint is `https://<account ID>.eu.r2.cloudflarestorage.com` for `eu`, `.us.` for `us` and `.fedramp.` for FedRAMP.
4. Copy the export first and the chunks second. Here the copy goes to `/backup/koidrive`; any rclone remote works as the destination.

   ```sh
   rclone copy r2:<bucket> /backup/koidrive --include "/spaces/*/meta/**"
   rclone copy r2:<bucket> /backup/koidrive --include "/spaces/*/chunks/**"
   ```

   A device uploads a file's chunks before the server accepts the change that uses them, and the export comes after that. So when the export is copied first, the chunks it names are already in the bucket when the second command starts. The exception is a chunk the server deletes in between, because its version expired or its file left the trash.

5. Run both commands again on a schedule, for example from `cron`. Each run transfers only new objects.

Use `rclone copy`, not `rclone sync`. `copy` never deletes from the destination. `sync` deletes from the copy what is gone from the bucket, so a bucket emptied by mistake would empty the copy on the next run.

## Put lost objects back

This works while the server still runs, with its Worker and Durable Objects, but objects in its bucket are gone. Files whose chunks are gone fail to download, and sync doesn't upload a lost chunk again, because the server still lists it as stored.

1. Create a token as in step 1 above, with the **Object Read & Write** permission instead, and put it in the `r2` remote.
2. Copy everything back:

   ```sh
   rclone copy /backup/koidrive r2:<bucket>
   ```

The objects return under the same names, and the server serves each chunk again. `rclone copy` skips objects that are still in the bucket. The server deletes restored chunks that it had already deleted before the loss, after its grace period, 30 days by default.

This was checked against local R2 in the Workers test runtime, without rclone. A chunk deleted from the bucket made its download fail with 404. Putting the same bytes back under the same name made the download succeed with the same SHA-256.

If you deleted the bucket itself, create a bucket with the same name, in the same jurisdiction, before you copy. Whether the Worker uses the new bucket without a new deploy is unverified.

## After you lose the account

A copy can't bring back a server. A new server starts with empty Durable Objects and never reads `meta/`, so copying the backup into a new server's bucket restores nothing.

The copy still keeps every space's tree, trash and old versions, with the chunks they name. RFC 0004 describes how to read it, but 0.1.0 has no tool that does. The live files in your linked folders stay on each device.
