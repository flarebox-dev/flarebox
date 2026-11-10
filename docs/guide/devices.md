# Devices

## List devices

```sh
flarebox device list
```

This lists every device signed in to your server, revoked ones included, with its ID, name, platform, when it was last seen and its state. `--json` prints the same.

## Revoke a device

```sh
flarebox device revoke 019a3c4e-...
```

Take the ID from `flarebox device list`. The device stops syncing at once. The files already on it stay there, so revoking a stolen laptop stops new changes from reaching it but doesn't erase it. A device can't revoke itself: revoke it from another device.

A revoked device can't sync any more. To use that machine again, remove Flarebox from it as [Uninstall](uninstall.md#remove-flarebox-from-one-device) describes, then set it up again.

## If you lose every device

Your files stay on the server. Add a new device with:

```sh
flarebox server add-device
```

You log in with the Cloudflare account that holds the server. That proves you own the server, so Flarebox needs no other device. It sets a one-time recovery key on the Worker, adds this machine as a new device of your account, and deletes the key again. Then revoke the lost devices with `flarebox device revoke`, since recovery doesn't sign them out.

> [!WARNING]
> This needs the Cloudflare login, which isn't available outside the project's test account yet (see [Set up](setup.md#with-flarebox-setup)). Until then, no Flarebox client can add a device to a server that has no signed-in device left. Keep at least two devices signed in.
