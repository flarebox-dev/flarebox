# Releasing the macOS app

The [macOS release](../.github/workflows/release-macos.yml) workflow builds the app from `apps/macos` as a disk image (DMG). It never creates a GitHub release and never publishes anything: the DMG is only a workflow artifact on the run's page. Publishing a release is a separate, manual step.

| Trigger | What it builds |
|---|---|
| A tag like `v0.1.0` on a commit that is on `main` | The app at version 0.1.0, signed and notarized if the signing secrets are set |
| **Run workflow** (manual) from `main` | The same, at the version you enter, or `MARKETING_VERSION` from `apps/macos/project.yml` when you leave it empty |
| Pull requests, the merge queue and pushes to `main` | An unsigned DMG, to check that the build, packaging and verification still work. The `macos app` job of [CI](../.github/workflows/ci.yml) builds it after testing the app's Swift bindings. A pull request or merge queue run builds it only when the change touches `core/`, `apps/macos/`, `.cargo/`, the Cargo files (including `cli/Cargo.toml`, which Cargo loads as a workspace member), `rust-toolchain.toml` or the workflow. A push to `main` always builds it, and `ci-report` fails the push if it did not run and pass. It runs on the self-hosted Mac runners with the Xcode installed there, or on GitHub-hosted runners with Xcode 26.6 for a fork's pull request or when the `MACOS_RUNS_ON` variable is set (see [CONTRIBUTING.md](../CONTRIBUTING.md)). These runs never see the signing secrets. |

## What a signed run does

1. Checks, in a job without access to the secrets, that the tagged or dispatched commit is on `main` and that a manual run started from `main`. Otherwise the run stops before the signing job starts.
2. Waits for a required reviewer to approve the `macos-release` environment, which releases the secrets to the signing job.
3. Builds the Release app with Xcode 26.6 on a `macos-26` runner, without signing.
4. Creates a temporary keychain with a random password, imports the Developer ID certificate into it, and writes the notary key to a file readable only by the runner user.
5. Signs the app with the Developer ID Application identity, the hardened runtime, a secure timestamp and [`apps/macos/Flarebox.entitlements`](../apps/macos/Flarebox.entitlements).
6. Notarizes the app with `notarytool` and the App Store Connect API key, then staples the ticket to the app. It submits the app without waiting, prints the submission id, and then asks Apple for the submission's status every minute for up to 2.5 hours. When a request fails with a network error, it tries again after 15 seconds, doubling the wait up to 5 minutes.
7. Packages the stapled app in a DMG, signs the DMG, notarizes it the same way and staples it.
8. Verifies the DMG and the app inside it with `codesign --verify --deep --strict`, `xcrun stapler validate` and `spctl --assess`, and checks for the hardened runtime, the Developer ID authority and the absence of `get-task-allow`.
9. Uploads `Flarebox-<version>-arm64.dmg` as an artifact and writes its SHA-256 to the run summary.
10. Deletes the keychain and the key files, even when a step failed.

If Apple reports a submission as Invalid or Rejected, the log shows Apple's notarization log for it. If Apple gives no final answer within 2.5 hours, the step fails and names the submission id.

When none of the signing secrets are set, the run still builds the DMG but signs it ad hoc only, names it `Flarebox-<version>-arm64-unsigned.dmg`, and says "Signing skipped" in a warning and in the run summary. Gatekeeper blocks such a DMG on other Macs. When only some of the secrets are set, the run fails and names the missing ones.

## Before you add any secret

A Developer ID certificate signs software as you, so the secrets must reach only code that was reviewed and merged to `main`, and only when you approve. Set up these three things first. They are repository settings; the workflow cannot apply them itself.

The trust boundary is your approval of the environment. A deployment rule like `v*` only checks a tag's name, not whether its commit was reviewed, and the workflow's own check that the commit is on `main` runs code from that same commit, so it is a second line of defense, not the boundary ([Deployment branches and tags](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments#deployment-branches-and-tags)).

### 1. Restrict who can create version tags

1. Open the repository on GitHub and click **Settings**.
2. In the left sidebar, under "Code, planning, and automation", click **Rules**, then **Rulesets**.
3. Click **New ruleset**, then **New tag ruleset**.
4. Under "Ruleset name", type `Release tags`.
5. Set "Enforcement status" to **Active**.
6. Under "Bypass list", click **Add bypass**, select **Repository admin**, and click **Add selected**.
7. Under "Target tags", click **Add target**, choose **Include by pattern**, type `v*`, and add the pattern.
8. Under "Tag protections", select **Restrict creations**, **Restrict updates** and **Restrict deletions**, and keep **Block force pushes** selected.
9. Click **Create**.

Only repository admins can then create, move or delete `v*` tags; everyone else, including apps with write access, is refused ([Available rules for rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)).

### 2. Create the `macos-release` environment with a required reviewer

1. In **Settings**, click **Environments** in the left sidebar, then **New environment**.
2. Type `macos-release` and click **Configure environment**.
3. Select **Required reviewers**, add `gunwooterry`, and leave **Prevent self-review** unselected, because you both start and approve release runs. Click **Save protection rules**.
4. Deselect **Allow administrators to bypass configured protection rules** and click **Save protection rules**.
5. Under "Deployment branches and tags", choose **Selected branches and tags**. Click **Add deployment branch or tag rule**, choose **Branch** as the ref type, type `main` and click **Add rule**. Click it again, choose **Tag**, type `v*` and click **Add rule**.

Each signed run then waits on its run page until you click **Review deployments**, select `macos-release` and **Approve and deploy**. Approve only runs for a commit and version you meant to release ([Managing environments for deployment](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments)).

### 3. Keep the secrets in the environment only

Add the five secrets below under **Environment secrets** of `macos-release` (**Add secret**), or from a terminal with `gh secret set <NAME> --env macos-release`, which reads the value from standard input. Never add them as repository or organization secrets: workflows that run for a pull request from a branch of this repository receive those, so anyone who can push a branch could change a workflow to read them. Environment secrets reach a job only after the environment's rules pass.

## Secrets

Five secrets, all required for a signed build:

| Secret | Holds |
|---|---|
| `MACOS_DEVELOPER_ID_P12_BASE64` | The Developer ID Application certificate with its private key, exported as a `.p12` file and base64-encoded |
| `MACOS_DEVELOPER_ID_P12_PASSWORD` | The password you chose when exporting the `.p12` |
| `APP_STORE_CONNECT_API_KEY_ID` | The key ID of an App Store Connect team API key |
| `APP_STORE_CONNECT_API_ISSUER_ID` | The issuer ID of your team's App Store Connect API keys |
| `APP_STORE_CONNECT_API_KEY_P8_BASE64` | The API key's private key, the `.p8` file, base64-encoded |

The keychain password is generated for each run, so it is not a secret you set.

### Create the Developer ID certificate and its `.p12`

Creating a Developer ID certificate needs the Account Holder role ([Developer ID certificates](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)).

1. On a Mac, open **Keychain Access** and choose **Keychain Access**, **Certificate Assistant**, **Request a Certificate From a Certificate Authority**. Enter your email address, choose **Saved to disk**, and save the `.certSigningRequest` file ([Create a certificate signing request](https://developer.apple.com/help/account/certificates/create-a-certificate-signing-request/)).
2. In [Certificates, Identifiers & Profiles](https://developer.apple.com/account/resources/certificates/list), click the add button, choose **Developer ID**, then **Developer ID Application**, upload the request and download the `.cer` file.
3. Double-click the `.cer` file. It appears under **My Certificates** in Keychain Access, together with the private key created in step 1.
4. In **My Certificates**, select the certificate "Developer ID Application: …" (it must show the private key under it), choose **File**, **Export Items**, save it as a `.p12`, and set a strong password.
5. Store both secrets:
   ```sh
   base64 -i DeveloperIDApplication.p12 | gh secret set MACOS_DEVELOPER_ID_P12_BASE64 --env macos-release
   gh secret set MACOS_DEVELOPER_ID_P12_PASSWORD --env macos-release   # prompts for the password
   ```
6. Delete the `.p12` and the `.certSigningRequest` files, and delete the certificate and its private key from Keychain Access. The signing key is not kept on any Mac; the environment secret is its only copy.

GitHub never shows a secret again after you save it. If the secret is lost, create a new certificate; a team can have up to five Developer ID Application certificates. Revoke the old certificate only if its private key leaked: apps signed with a revoked Developer ID certificate can no longer be installed or launched ([Developer ID certificates](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)).

### Create the App Store Connect API key

`notarytool` needs a team API key; individual API keys cannot use it ([Creating API Keys for App Store Connect API](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api)). Generating team keys needs the Admin role in App Store Connect, and the Account Holder must first request access to the App Store Connect API ([Role permissions](https://developer.apple.com/help/app-store-connect/reference/account-management/role-permissions/)).

1. In [App Store Connect](https://appstoreconnect.apple.com/), open **Users and Access**, **Integrations**, **App Store Connect API**. If the page asks you to request access, do that first.
2. On **Team Keys**, click **Generate API Key**, name it, for example, "Flarebox notarization", and choose the **Developer** role. That is the least privileged role commonly used for notarization; *unverified*: Apple's role table does not render as text, so if `notarytool` reports that the key is not authorized, generate a key with the **App Manager** role instead and revoke the first one.
3. Click **Download API Key**. Apple offers the `.p8` file only once and keeps no copy.
4. Copy the key's **Key ID** from its row and the **Issuer ID** shown above the list of keys.
5. Store the three secrets:
   ```sh
   gh secret set APP_STORE_CONNECT_API_KEY_ID --env macos-release      # prompts for the key ID
   gh secret set APP_STORE_CONNECT_API_ISSUER_ID --env macos-release   # prompts for the issuer ID
   base64 -i AuthKey_<key ID>.p8 | gh secret set APP_STORE_CONNECT_API_KEY_P8_BASE64 --env macos-release
   ```
6. Delete the `.p8` file. If the secret is lost or leaks, revoke the key on the same page and generate a new one.

## Run a release build

- **Manually:** open **Actions**, **macOS release**, **Run workflow**, keep `main` under "Use workflow from", and enter a version such as `0.1.0` or leave it empty. A run from any other branch fails in the "commit on main" job.
- **From a tag:** merge the release to `main`, then tag a commit on `main`, for example `git fetch origin main && git tag v0.1.0 origin/main && git push origin v0.1.0`, so the tag lands on the latest merged commit. The tag must be three numbers after the `v`, and a tag on a commit that is not on `main` fails in the "commit on main" job.

Either way, approve the deployment to `macos-release` on the run page when it asks.

Download the DMG from the **Artifacts** section of the run's page. To check it on your Mac:

```sh
hdiutil attach Flarebox-0.1.0-arm64.dmg -nobrowse -readonly
codesign --verify --deep --strict --verbose=2 /Volumes/Flarebox/Flarebox.app
xcrun stapler validate /Volumes/Flarebox/Flarebox.app
spctl --assess --type execute --verbose=2 /Volumes/Flarebox/Flarebox.app   # expect "source=Notarized Developer ID"
hdiutil detach /Volumes/Flarebox
```

### If the app's notarization was interrupted

Each notarization step prints its submission id in the log and as a notice on the run's page. If the run failed while Apple was still processing the app, for example after a network error or at the deadline, you can wait for that submission instead of submitting the app again. `xcrun notarytool info <id>` with the team's API key shows its status. Start a new manual run with the same version, enter the failed run's build number as **build-number** (its "Decide the version" step prints `Version <version>, build <number>.`), and enter the id as **app-submission-id**. The new run rebuilds the same app, waits for that submission and staples its ticket. If the rebuilt app differs from the submitted one, stapling fails, and the step says so; run again with **app-submission-id** empty to submit the app again.

The disk image always goes through a new submission, because a rebuilt disk image is never identical to the one submitted before.

## Entitlements

`apps/macos/Flarebox.entitlements` is deliberately empty. The app is distributed with Developer ID, not through the Mac App Store, so it is not sandboxed: it syncs folders the person chooses anywhere in their home folder. The hardened runtime needs an entitlement only to turn off one of its protections ([Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime)), and the app needs none of them: the Rust core is linked statically, and the app uses no JIT, unsigned executable memory, DYLD environment variables or libraries signed by someone else. Open at login uses `SMAppService.mainApp`, which needs no entitlement outside the sandbox. Apple recommends leaving false entitlements out rather than listing them as false. The File Provider extension planned for a later version will need the sandbox and an app group, and with them a Developer ID provisioning profile, which this workflow does not handle yet.

## Limits

- The DMG runs on Apple silicon only. `scripts/build-ffi.sh` builds the Rust library for the runner's architecture, and the release runners are arm64, so there is no Intel or universal build yet.
- Nothing is published: no GitHub release, no update feed, no Homebrew cask.
- A run cannot wait for an earlier submission of the disk image. Every run builds a new disk image and submits it.
