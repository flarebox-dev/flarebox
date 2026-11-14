# Sync folders with the Mac app

Koidrive for Mac is the full sync client for macOS 26 or later on Apple silicon. Use the app to set up this Mac, link folders, check sync and answer questions. Your synced files are ordinary files on disk.

## Install and set up this Mac

1. Download `Koidrive-<version>-arm64.dmg` from the [latest release](https://github.com/koidrive/koidrive/releases/latest).
2. Open the disk image and drag Koidrive to Applications. Open Koidrive.
3. If you have no server, [deploy one](../deploy.md#deploy) in your Cloudflare account with R2 turned on.
4. In **Set Up Koidrive**, enter your server's address and click **Connect**.
5. Sign in:
   - For a new server, open `<your server>/setup` in a browser. Find `Koidrive claim code` in the Worker's **Observability** logs in Cloudflare. Enter the code, your name and this Mac's name in the app. Click **Claim Server**.
   - For an existing account, get a pairing code from **Settings**, **Account**, **Add a Device…** on a signed-in Mac. On a signed-in Linux machine, run `koi device invite`. Enter the code and this Mac's name on the new Mac. Click **Sign In**.
6. Choose an existing space or create one. Click **Choose…** to select a folder on this Mac, then click **Link Folder**. The folder syncs in the background.

To install with Homebrew, see [Install with Homebrew](install.md#install-with-homebrew).

## Check sync and answer questions

Click Koidrive in the menu bar to check sync status, read notices or open a linked folder in Finder. The icon shows these states:

- A circular arrow means Koidrive is connecting or syncing.
- Pause bars mean sync is paused.
- A minus sign means Koidrive is offline and retrying the server.
- A question mark means **Needs You** has a question to answer.
- An exclamation mark means there is a problem to review.
- With no badge, open the menu to see whether sync is up to date, setup is needed or the service is stopped.

Choose **Pause Sync** to pause, and **Resume Sync** to continue. Edits you make while paused stay on this Mac and sync when you resume.

When **Needs You** opens, select a question and review the affected files before choosing an answer. Koidrive asks before sending a large delete to your other machines. Destructive answers need confirmation.

To inspect a problem, choose **Review problems** in the menu bar. **Settings**, **Problems** lists each problem with advice. **Show in Finder** is available when the problem's space is still linked on this Mac.

## Manage folders and preferences

Open **Settings** from the menu bar:

- In **Folders**, choose **Add Folder…** to link another folder. Each folder has **Show in Finder** and **Unlink…** buttons. Unlinking keeps the files on this Mac and the server.
- In **Account**, check your server and devices, generate a pairing code with **Add a Device…**, remove another device or sign out. Signing out unlinks this Mac's folders but keeps their files.
- In **Storage**, check each space's storage use and choose **Refresh** to reload it.
- In **Problems**, read what stopped or failed and follow the advice for the affected folder or file.
- In **General**, choose whether Koidrive opens at login and sends anonymous usage reports.

## Use the CLI alongside the app

The app includes `koi` in `/Applications/Koidrive.app/Contents/Helpers/koi`. A disk image install does not add `koi` to `PATH`. In the linked guides, use that full path in place of `koi`.

The app and CLI share one device, profile and background sync service, so folders linked in either appear in both. Use the CLI for [scripts](agents.md) and [server administration](setup.md#upgrade-the-server).
