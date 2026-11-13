// swift-tools-version: 6.0
// SPDX-License-Identifier: Apache-2.0

import PackageDescription

let package = Package(
    name: "KoidriveFFI",
    platforms: [.macOS("26.0")],
    products: [
        .library(name: "KoidriveFFI", targets: ["KoidriveFFI"]),
        .library(name: "KoidriveModel", targets: ["KoidriveModel"]),
    ],
    targets: [
        .binaryTarget(name: "koidrive_ffiFFI", path: "Frameworks/koidrive_ffiFFI.xcframework"),
        .target(name: "KoidriveFFI", dependencies: ["koidrive_ffiFFI"]),
        .target(name: "KoidriveModel", dependencies: ["KoidriveFFI"]),
        .testTarget(name: "KoidriveFFITests", dependencies: ["KoidriveFFI"]),
        .testTarget(name: "KoidriveModelTests", dependencies: ["KoidriveModel"]),
    ]
)
