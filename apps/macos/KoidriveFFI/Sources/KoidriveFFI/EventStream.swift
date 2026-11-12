// SPDX-License-Identifier: Apache-2.0

import Foundation

extension SyncSession {
    /// Opens the sync profile in `profileDirectory` with the sync driver, and the stream of its
    /// events. The profile keeps this Mac's credential, its linked folders and their sync state
    /// across relaunches; one process at a time can open it, so a second open throws
    /// `SessionError.ProfileInUse`.
    ///
    /// The stream finishes when the Rust session is dropped, which happens once the last reference
    /// to the returned session is released. Dropping it also releases the profile.
    public static func openWithEventStream(profileDirectory: URL) throws -> (SyncSession, AsyncStream<SessionEvent>) {
        let (events, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        let session = try SyncSession.open(
            listener: EventRelay(continuation: continuation),
            profileDir: profileDirectory.path(percentEncoded: false)
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
