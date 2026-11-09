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
    @ObservationIgnored private var consumer: Task<Void, Never>?

    init() {
        let (session, statuses) = SyncSession.withStatusStream()
        self.session = session
        status = session.status()
        consumer = Task { [weak self] in
            for await status in statuses {
                self?.status = status
            }
        }
    }

    deinit {
        consumer?.cancel()
    }

    func toggle() {
        if session.status() == .idle {
            session.stop()
        } else {
            session.start()
        }
    }
}
