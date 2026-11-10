# Flarebox for macOS

A menu bar app that links the Rust core through [UniFFI](https://mozilla.github.io/uniffi-rs/). It has a menu bar menu (status, progress, recent changes, notices, pause and resume, open folder), a first-run setup window (server, sign-in, folder), a questions window, and settings (linked folders, account, problems, open at login). The session runs the sync driver of `core/driver` in-process through `core/ffi`: it signs this Mac in, links folders, follows each space's log and observes the disk. The driver does not run the sync engine yet, so no file is uploaded or downloaded, and progress, recent changes and notices stay empty until it does. There is no File Provider extension.

## Requirements

- macOS 14 or later on Apple silicon or Intel.
- Xcode 16 or later, with its command-line tools selected (`xcode-select -p`).
- Rust through [rustup](https://rustup.rs/). The toolchain version comes from `rust-toolchain.toml` at the repository root and is installed on first use.
- [XcodeGen](https://github.com/yonaskolb/XcodeGen) 2.46.0. You don't install it yourself: the first run of `scripts/generate-project.sh` downloads the release into `build/tools` and checks its SHA-256.

## Build and test

Run these from `apps/macos`:

```sh
# 1. Build core/ffi as a static library and generate the Swift bindings.
scripts/build-ffi.sh

# 2. Test the bindings and the app's models.
swift test --package-path FlareboxFFI

# 3. Generate Flarebox.xcodeproj from project.yml.
scripts/generate-project.sh

# 4. Build the app without signing it.
xcodebuild -project Flarebox.xcodeproj -scheme Flarebox -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Release/Flarebox.app
```

The app keeps its sync profile in `~/Library/Application Support/Flarebox`: the device credential (`state/credential.json`), one store per linked space (`state/spaces/`) and the pause. One process at a time can open the profile.

To try it against a local server, start the Worker in `server/` with `pnpm exec wrangler dev` (after `pnpm install`), enter `http://127.0.0.1:8787` in the setup window, open `http://127.0.0.1:8787/setup` in a browser, and claim the server with the code that `wrangler dev` prints. Plain `http` is accepted only on a loopback address.

To show a state the driver cannot reach yet, start the app on the fake core in `core/ffi` with a scripted scenario. The fake keeps its state in memory only:

```sh
open build/DerivedData/Build/Products/Release/Flarebox.app --args -FakeScenario manyQuestions
```

The scenarios are `firstRun`, `upToDate`, `syncing`, `paused`, `offline`, `massDelete`, `ambiguousDisappearance`, `recovery`, `recoveredWithoutQuestions`, `remoteMassDelete`, `manyQuestions` and `problems`.

You can also open `Flarebox.xcodeproj` in Xcode after step 3 and run the Flarebox scheme; the views have SwiftUI previews for these scenarios. Local builds are signed ad hoc, so no Apple developer account is needed.

`core/ffi/scripts/integration-test.sh`, run from anywhere in the repository, tests the session against the Worker under `wrangler dev`: claiming, pairing, linking, status events, the pause, questions and problems.

Run `scripts/build-ffi.sh` again after every change to `core/ffi`. It rebuilds the Rust library and regenerates the Swift API together. UniFFI checksums stop the app at launch when the library and the bindings disagree on the API, but they do not detect a build that is merely out of date: an old library with its matching bindings still launches.

## Layout

| Path | Contents |
|---|---|
| `Flarebox/` | The SwiftUI menu bar app and its windows. User-facing text is in `Localizable.xcstrings`, with English as the base language. |
| `project.yml` | The XcodeGen spec for `Flarebox.xcodeproj`. The generated project is not checked in, so edit `project.yml` and run `scripts/generate-project.sh` again after changing it or adding or removing source files. |
| `FlareboxFFI/` | A Swift package: the `FlareboxFFI` library wraps the Rust core (the generated bindings and the static library as an XCFramework), and `FlareboxModel` holds the app's view models. Both have tests. |
| `scripts/generate-project.sh` | Generates `Flarebox.xcodeproj` with the pinned XcodeGen. |
| `scripts/build-ffi.sh` | Builds the Rust library and fills in `FlareboxFFI/Frameworks` and `FlareboxFFI/Sources`. These generated files are not checked in. |
| `scripts/build-release.sh`, `sign-app.sh`, `make-dmg.sh`, `notarize.sh`, `verify-release.sh` | Build, sign, package, notarize and verify a release disk image. The macOS release workflow runs them; see [docs/release-macos.md](../../docs/release-macos.md). |
| `Flarebox.entitlements` | The entitlements release builds are signed with, deliberately empty. |

The Rust library is built for the architecture of the machine you build on, not as a universal binary.

## License

Apache-2.0, like the rest of `apps/`. See [../LICENSE](../LICENSE).
