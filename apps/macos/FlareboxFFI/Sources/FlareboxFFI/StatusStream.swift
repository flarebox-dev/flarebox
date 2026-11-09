// SPDX-License-Identifier: Apache-2.0

extension SyncSession {
    /// Creates a stopped session and the stream of its status changes.
    ///
    /// The stream finishes when the Rust session is dropped, which happens
    /// once the last reference to the returned session is released.
    public static func withStatusStream() -> (SyncSession, AsyncStream<SyncStatus>) {
        let (statuses, continuation) = AsyncStream.makeStream(of: SyncStatus.self)
        return (SyncSession(listener: StatusRelay(continuation: continuation)), statuses)
    }
}

/// Forwards status changes into a stream and finishes it when Rust drops the listener.
///
/// Rust calls `onStatus` while holding the session lock, so the relay only
/// yields and never calls back into the session.
private final class StatusRelay: StatusListener {
    let continuation: AsyncStream<SyncStatus>.Continuation

    init(continuation: AsyncStream<SyncStatus>.Continuation) {
        self.continuation = continuation
    }

    deinit {
        continuation.finish()
    }

    func onStatus(status: SyncStatus) {
        continuation.yield(status)
    }
}
