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
    let workspacePackage = try #require(
        manifest.components(separatedBy: "[workspace.package]").dropFirst().first?
            .components(separatedBy: "\n[").first
    )
    let workspaceVersion = try #require(
        workspacePackage.firstMatch(of: /^version\s*=\s*"([^"]+)"/.anchorsMatchLineEndings())
    )

    #expect(String(workspaceVersion.1) == version())
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

@Test(.timeLimit(.minutes(1)))
func statusStreamFinishesWhenTheSessionIsReleased() async {
    var session: SyncSession?
    let statuses: AsyncStream<SyncStatus>
    (session, statuses) = SyncSession.withStatusStream()
    async let seen: [SyncStatus] = {
        var seen: [SyncStatus] = []
        for await status in statuses {
            seen.append(status)
        }
        return seen
    }()

    session?.start()
    session?.stop()
    session = nil

    #expect(await seen == [.idle, .stopped])
}
