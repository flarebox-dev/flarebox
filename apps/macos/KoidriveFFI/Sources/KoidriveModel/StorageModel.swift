// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Observation

/// The Storage tab's last answer: what each space stores, or why the list failed.
@MainActor
@Observable
public final class StorageModel {
    /// Nil while a load runs.
    public private(set) var spaces: [SpaceStorage]?
    public private(set) var error: SessionError?
    private var generation = 0

    public init() {}

    public var isLoading: Bool { spaces == nil && error == nil }

    /// Replaces the answer with what `fetch` returns. A load that a later load replaced, or
    /// whose task was cancelled, leaves the answer alone when it finishes.
    public func load(_ fetch: @MainActor () async throws -> [SpaceStorage]) async {
        generation += 1
        let current = generation
        spaces = nil
        error = nil
        let result: Result<[SpaceStorage], SessionError>
        do {
            result = .success(try await fetch())
        } catch let failure as SessionError {
            result = .failure(failure)
        } catch {
            return
        }
        guard current == generation, !Task.isCancelled else { return }
        switch result {
        case .success(let answer): spaces = answer
        case .failure(let failure): error = failure
        }
    }
}
