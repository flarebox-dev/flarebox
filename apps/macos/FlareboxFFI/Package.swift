// swift-tools-version: 6.0
// SPDX-License-Identifier: Apache-2.0

import PackageDescription

let package = Package(
    name: "FlareboxFFI",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "FlareboxFFI", targets: ["FlareboxFFI"]),
    ],
    targets: [
        .binaryTarget(name: "flarebox_ffiFFI", path: "Frameworks/flarebox_ffiFFI.xcframework"),
        .target(name: "FlareboxFFI", dependencies: ["flarebox_ffiFFI"]),
        .testTarget(name: "FlareboxFFITests", dependencies: ["FlareboxFFI"]),
    ]
)
