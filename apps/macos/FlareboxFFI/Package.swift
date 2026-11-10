// swift-tools-version: 6.0
// SPDX-License-Identifier: Apache-2.0

import PackageDescription

let package = Package(
    name: "FlareboxFFI",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "FlareboxFFI", targets: ["FlareboxFFI"]),
        .library(name: "FlareboxModel", targets: ["FlareboxModel"]),
    ],
    targets: [
        .binaryTarget(name: "flarebox_ffiFFI", path: "Frameworks/flarebox_ffiFFI.xcframework"),
        .target(name: "FlareboxFFI", dependencies: ["flarebox_ffiFFI"]),
        .target(name: "FlareboxModel", dependencies: ["FlareboxFFI"]),
        .testTarget(name: "FlareboxFFITests", dependencies: ["FlareboxFFI"]),
        .testTarget(name: "FlareboxModelTests", dependencies: ["FlareboxModel"]),
    ]
)
