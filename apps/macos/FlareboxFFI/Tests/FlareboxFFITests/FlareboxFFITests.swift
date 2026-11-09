// SPDX-License-Identifier: Apache-2.0

import Foundation
import os
import Testing

import FlareboxFFI

@Test func versionComesFromTheRustCore() throws {
    let cargoToml = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .appending(path: "../../../../../Cargo.toml")
        .standardizedFileURL
    let manifest = try String(contentsOf: cargoToml, encoding: .utf8)
    let workspaceVersion = try #require(
        manifest.split(separator: "\n").first { $0.hasPrefix("version = ") }
    )

    #expect(workspaceVersion == "version = \"\(version())\"")
}

@Test func sessionStreamsEachStatusChange() {
    final class Recorder: StatusListener {
        let seen = OSAllocatedUnfairLock<[SyncStatus]>(initialState: [])

        func onStatus(status: SyncStatus) {
            seen.withLock { $0.append(status) }
        }
    }

    let recorder = Recorder()
    let session = SyncSession(listener: recorder)
    #expect(session.status() == .stopped)

    session.start()
    #expect(session.status() == .idle)
    session.stop()
    #expect(session.status() == .stopped)

    #expect(recorder.seen.withLock { $0 } == [.idle, .stopped])
}
