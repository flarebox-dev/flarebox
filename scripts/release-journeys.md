# Check a published macOS release

Run **Release journeys** in GitHub Actions and enter the published tag, such as `v0.1.0-alpha.3`. The tag must match the release offered by `koidrive/tap`.

Each runtime case gets a separate [GitHub-hosted macOS 26 runner](https://docs.github.com/en/actions/reference/runners/github-hosted-runners). The check refuses local execution and existing Koidrive state. It uses released binaries, preserves quarantine, and does not use a test build or replace signing verification.

| Case | Required result |
| --- | --- |
| Published assets | The Mac CLI and DMG match `SHA256SUMS`; the signed app and DMG pass Gatekeeper assessment and stapled-ticket validation. |
| Clean cask | The public cask installs, and the next command, `koi --version`, reports the requested release. Plain uninstall removes the app and CLI. A second installation followed by zap removes app state without deleting a test document. |
| Formula first | The formula remains intact when cask installation fails on its existing `koi` binary. Evidence includes Homebrew's exact error and resulting installation state. |
| Shell installer | The pinned installer runs through `curl | sh` in a controlling terminal, completes guided setup with the released CLI, and leaves a usable installation. Cleanup stops the test's app and services. |

The shell case uses immutable installer commit `b31c0c0232e1083c8b13210c59893c9eecd057f2`, checked against SHA-256 `ccc71c1fcd8381391ab57109d9d365afeea662c142dbbbda6bc4fe700460c276`. It exercises the installer before publication without deploying it. A local Worker and public CLI commands supply test authentication and a shared folder. They do not substitute for the installer or pre-link its folder.

Each job uploads sanitized commands, transcripts, assertions, and cleanup results as `release-journeys-*` artifacts. A failed prerequisite, runtime assertion, or cleanup fails the job. Read the formula-conflict artifact first when checking coexistence.

Release publication starts this workflow. The CLI distribution workflow also calls it after tap publication because [events produced by `GITHUB_TOKEN` do not start another workflow](https://docs.github.com/en/actions/concepts/security/github_token#when-github_token-triggers-workflow-runs).

Before the workflow is on the default branch, run the existing **Installer** workflow from the proposed branch with `release_tag` set. That invokes the same reusable workflow without redeploying the site.

Run the non-mutating unit tests locally:

```sh
nice -n 10 python3 scripts/test-release-journeys-unit.py -v
```
