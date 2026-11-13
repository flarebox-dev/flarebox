# Koidrive for macOS

A menu bar app over the `koi` command. It has a menu bar menu (status, notices, pause and resume, open folder), a first-run setup window (server, sign-in, folder), a questions window, and settings (linked folders, account, storage used by each space, problems, open at login). The app carries `koi` in `Contents/Helpers/koi` and shows the same device, folders and background service as `koi status`. Through `core/ffi` and [UniFFI](https://mozilla.github.io/uniffi-rs/), it runs each change as a `koi --json` command and follows the state of the sync daemon over its socket. While no daemon runs, it reads the state with `koi status`. There is no File Provider extension.

## Requirements

- macOS 26 or later on Apple silicon.
- Xcode 16 or later, with its command-line tools selected (`xcode-select -p`).
- Rust through [rustup](https://rustup.rs/). The toolchain version comes from `rust-toolchain.toml` at the repository root and is installed on first use.
- [XcodeGen](https://github.com/yonaskolb/XcodeGen) 2.46.0. You don't install it yourself: the first run of `scripts/generate-project.sh` downloads the release into `build/tools` and checks its SHA-256.

## Build and test

Run these from `apps/macos`:

```sh
scripts/build-ffi.sh

# 2. Test the bindings and the app's models.
swift test --package-path KoidriveFFI

# 3. Generate Koidrive.xcodeproj from project.yml.
scripts/generate-project.sh

xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Release/Koidrive.app
```

The app syncs the profile of the installed launch agent (`~/Library/LaunchAgents/dev.koidrive.koi*.plist`), or else the profile `koi` uses in the app's environment (`~/.local/state/koidrive` by default). Its `koi` commands run with that profile's `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`. When it starts, it runs `koi _reconcile`, which brings the background service in line with the linked folders as `koi folder link` does and leaves a service switched off in Login Items off.

To try it against a local server without touching your own profile, give the app a home of its own and start it from a terminal:

```sh
profile="$(mktemp -d)"
HOME="$profile" XDG_CONFIG_HOME="$profile/config" XDG_STATE_HOME="$profile/state" \
	XDG_RUNTIME_DIR="$profile/run" build/DerivedData/Build/Products/Release/Koidrive.app/Contents/MacOS/Koidrive
```

Then start the Worker in `server/` with `pnpm exec wrangler dev` (after `pnpm install`), enter `http://127.0.0.1:8787` in the setup window, open `http://127.0.0.1:8787/setup` in a browser, and claim the server with the code that `wrangler dev` prints. Plain `http` is accepted only on a loopback address.

To show a state the driver cannot reach yet, start a Debug build on the fake core in `core/ffi` with a scripted scenario. Release builds ignore the `FakeScenario` setting. The fake keeps its state in memory only:

```sh
xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Debug \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Debug/Koidrive.app --args -FakeScenario manyQuestions
```

The scenarios are `firstRun`, `upToDate`, `syncing`, `paused`, `offline`, `massDelete`, `ambiguousDisappearance`, `recovery`, `recoveredWithoutQuestions`, `remoteMassDelete`, `manyQuestions` and `problems`.

You can also open `Koidrive.xcodeproj` in Xcode after step 3 and run the Koidrive scheme; the views have SwiftUI previews for these scenarios. Local builds are signed ad hoc, so no Apple developer account is needed.

`core/ffi/scripts/integration-test.sh`, run from anywhere in the repository, tests the session against the Worker under `wrangler dev` with a debug `koi` and its daemon: claiming, pairing, linking, status events, the pause, questions, problems and signing out.

Run `scripts/build-ffi.sh` again after every change to `core/ffi` or `cli`. It rebuilds the Rust library and regenerates the Swift API together. UniFFI checksums stop the app at launch when the library and the bindings disagree on the API, but they do not detect a build that is merely out of date: an old library with its matching bindings still launches.

## Layout

| Path | Contents |
|---|---|
| `Koidrive/` | The SwiftUI menu bar app and its windows. User-facing text is in `Localizable.xcstrings`, with English as the base language. |
| `project.yml` | The XcodeGen spec for `Koidrive.xcodeproj`. The generated project is not checked in, so edit `project.yml` and run `scripts/generate-project.sh` again after changing it or adding or removing source files. |
| `KoidriveFFI/` | A Swift package: the `KoidriveFFI` library wraps the Rust core (the generated bindings and the static library as an XCFramework), and `KoidriveModel` holds the app's view models. Both have tests. |
| `scripts/generate-project.sh` | Generates `Koidrive.xcodeproj` with the pinned XcodeGen. |
| `scripts/build-ffi.sh` | Builds the Rust library and `koi`, and fills in `KoidriveFFI/Frameworks`, `KoidriveFFI/Sources` and `build/koi`. These generated files are not checked in. |
| `scripts/build-release.sh`, `sign-app.sh`, `make-dmg.sh`, `notarize.sh`, `verify-release.sh` | Build, sign, package, notarize and verify a release disk image, with `koi` signed inside the app. The macOS release workflow runs them; see [docs/release-macos.md](../../docs/release-macos.md). |
| `Koidrive.entitlements` | The entitlements release builds are signed with, deliberately empty. |

The Rust library and `koi` are built for the architecture of the machine you build on, not as universal binaries.

## License

Apache-2.0, like the rest of `apps/`. See [../LICENSE](../LICENSE).
