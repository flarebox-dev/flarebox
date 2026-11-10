# Flarebox for macOS

A menu bar app that links the Rust core through [UniFFI](https://mozilla.github.io/uniffi-rs/). The session runs against `FakeCore` in `core/ffi`, which plays back scripted states, until the driver that runs the sync engine exists. Nothing is synced yet, and there is no File Provider extension.

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

# 2. Test the bindings: Swift calls into Rust and checks the event stream, sign-in, linking and questions.
swift test --package-path FlareboxFFI

# 3. Generate Flarebox.xcodeproj from project.yml.
scripts/generate-project.sh

# 4. Build the app without signing it.
xcodebuild -project Flarebox.xcodeproj -scheme Flarebox -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Release/Flarebox.app
```

You can also open `Flarebox.xcodeproj` in Xcode after step 3 and run the Flarebox scheme. Local builds are signed ad hoc, so no Apple developer account is needed.

Run `scripts/build-ffi.sh` again after every change to `core/ffi`. It rebuilds the Rust library and regenerates the Swift API together. UniFFI checksums stop the app at launch when the library and the bindings disagree on the API, but they do not detect a build that is merely out of date: an old library with its matching bindings still launches.

## Layout

| Path | Contents |
|---|---|
| `Flarebox/` | The SwiftUI menu bar app. |
| `project.yml` | The XcodeGen spec for `Flarebox.xcodeproj`. The generated project is not checked in, so edit `project.yml` and run `scripts/generate-project.sh` again after changing it or adding or removing source files. |
| `FlareboxFFI/` | A Swift package that wraps the Rust core: the generated bindings, the static library as an XCFramework, and the tests. |
| `scripts/generate-project.sh` | Generates `Flarebox.xcodeproj` with the pinned XcodeGen. |
| `scripts/build-ffi.sh` | Builds the Rust library and fills in `FlareboxFFI/Frameworks` and `FlareboxFFI/Sources`. These generated files are not checked in. |

The Rust library is built for the architecture of the machine you build on, not as a universal binary.

## License

Apache-2.0, like the rest of `apps/`. See [../LICENSE](../LICENSE).
