// SPDX-License-Identifier: Apache-2.0

import Foundation

extension SyncSession {
    /// Opens the CLI's sync profile on this Mac through the `koi` at `helper`, and the stream of its events.
    ///
    /// The stream finishes when the Rust session is dropped, which happens once the last reference
    /// to the returned session is released.
    public static func openWithEventStream(helper: URL) throws -> (SyncSession, AsyncStream<SessionEvent>) {
        let (events, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        let session = try SyncSession.open(
            listener: EventRelay(continuation: continuation),
            helper: helper.path(percentEncoded: false)
        )
        return (session, events)
    }

    /// Creates a stopped session backed by the fake core, which keeps a scripted state in memory,
    /// and the stream of its events. The stream finishes when the Rust session is dropped.
    public static func fakeWithEventStream() -> (SyncSession, AsyncStream<SessionEvent>) {
        let (events, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        let session = SyncSession.fake(listener: EventRelay(continuation: continuation))
        return (session, events)
    }
}

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
