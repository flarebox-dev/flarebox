// SPDX-License-Identifier: Apache-2.0

import Foundation

extension SyncSession {
    /// Creates a stopped session backed by the fake core, and the stream of its events.
    ///
    /// With `stateDirectory`, the fake keeps its state there, so the account, linked folders and
    /// unanswered questions survive a relaunch. The stream finishes when the Rust session is
    /// dropped, which happens once the last reference to the returned session is released.
    public static func fakeWithEventStream(stateDirectory: URL? = nil) -> (SyncSession, AsyncStream<SessionEvent>) {
        let (events, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        let session = SyncSession.fake(
            listener: EventRelay(continuation: continuation),
            stateDir: stateDirectory?.path(percentEncoded: false)
        )
        return (session, events)
    }
}

/// Forwards session events into a stream and finishes it when Rust drops the listener.
///
/// Rust calls `onEvent` while holding the core's lock, so the relay only yields and never
/// calls back into the session.
private final class EventRelay: SessionListener {
    let continuation: AsyncStream<SessionEvent>.Continuation

    init(continuation: AsyncStream<SessionEvent>.Continuation) {
        self.continuation = continuation
    }

    deinit {
        continuation.finish()
    }

    func onEvent(event: SessionEvent) {
        continuation.yield(event)
    }
}
