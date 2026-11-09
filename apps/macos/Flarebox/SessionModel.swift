// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import Observation

/// Holds the Rust sync session and mirrors its status stream on the main actor.
@MainActor
@Observable
final class SessionModel {
    let version = FlareboxFFI.version()
    private(set) var status: SyncStatus
    private let session: SyncSession

    init() {
        let (statuses, continuation) = AsyncStream.makeStream(of: SyncStatus.self)
        session = SyncSession(listener: StatusRelay(continuation: continuation))
        status = session.status()
        Task { [weak self] in
            for await status in statuses {
                self?.status = status
            }
        }
    }

    func toggle() {
        if session.status() == .idle {
            session.stop()
        } else {
            session.start()
        }
    }
}

private final class StatusRelay: StatusListener {
    let continuation: AsyncStream<SyncStatus>.Continuation

    init(continuation: AsyncStream<SyncStatus>.Continuation) {
        self.continuation = continuation
    }

    func onStatus(status: SyncStatus) {
        continuation.yield(status)
    }
}
