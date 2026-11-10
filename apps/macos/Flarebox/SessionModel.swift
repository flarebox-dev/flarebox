// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import Observation

/// Holds the Rust sync session and mirrors its status on the main actor.
@MainActor
@Observable
final class SessionModel {
    let version = FlareboxFFI.version()
    private(set) var status: SyncStatus
    private let session: SyncSession
    @ObservationIgnored private var consumer: Task<Void, Never>?

    init() {
        let (session, events) = SyncSession.fakeWithEventStream()
        self.session = session
        status = session.state().status
        consumer = Task { [weak self] in
            for await event in events {
                if case .status(let status) = event {
                    self?.status = status
                }
            }
        }
    }

    deinit {
        consumer?.cancel()
    }

    func toggle() {
        if session.state().status == .stopped {
            session.start()
        } else {
            session.stop()
        }
    }
}
