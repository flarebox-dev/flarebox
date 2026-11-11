# Flarebox user guide

Flarebox keeps folders in sync between your computers through a server that you deploy to your own Cloudflare account. File contents are stored as chunks in an R2 bucket in that account. A Durable Object for each space keeps its file tree, its trash and the old versions of its files. Flarebox is an independent project and is not affiliated with Cloudflare, Inc.

This guide covers the `flarebox` command on macOS and Linux, version 0.1.0.

> [!IMPORTANT]
> 0.1.0 is not released yet, and two parts of the setup are still missing:
>
> - There is no packaged release. You [build the command from source](install.md).
> - The Cloudflare login that `flarebox setup` uses needs Flarebox's public OAuth client, which doesn't exist yet. Until it does, only members of the project's own test account can log in. Everyone else deploys the server with the deploy button and claims it with `flarebox server claim`, as [Set up](setup.md#without-the-cloudflare-login) describes.

## Words this guide uses

- **Server.** The Worker, the two Durable Object classes and the R2 bucket in your Cloudflare account. One server serves one owner.
- **Device.** A computer signed in to the server. Each device has its own credential, which you can revoke.
- **Space.** A synced file tree on the server. A space has its own trash and keeps old versions of its files.
- **Linked folder.** A folder on a device that syncs one space. A device links each space to at most one folder.

## Pages

1. [Install](install.md): build the command and run the sync daemon.
2. [Set up](setup.md): deploy a server, add devices, create spaces and upgrade the server.
3. [Folders](folders.md): link a folder, choose what syncs with `.fbignore`, and what Flarebox doesn't sync.
4. [Conflicts](conflicts.md): what happens when two devices change the same file.
5. [Trash, versions and restore](history.md): get back a deleted file or an earlier version.
6. [Status and problems](status.md): read `flarebox status`, wait out a quota, restart a folder and send refused changes again.
7. [Devices](devices.md): list and revoke devices, and get back in after losing every device.
8. [Limits](limits.md): what the Workers Free plan allows, and Flarebox's own limits.
9. [Uninstall](uninstall.md): remove a device, or remove everything.
