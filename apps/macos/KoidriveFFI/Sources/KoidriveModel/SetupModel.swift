// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation
import Observation

/// The setup flow: connect to a server, sign this device in, and link a folder to a space.
@MainActor
@Observable
public final class SetupModel {
    /// Where the flow starts.
    public enum Mode: Equatable, Sendable {
        /// Picks up at the first step not done yet, or finishes at once if this device already
        /// syncs a folder.
        case firstRun
        /// Links one more folder on a signed-in device, finishing when that link succeeds.
        case addFolder
    }

    public enum Step: Equatable, Sendable {
        case server
        case signIn
        case folder
        case done
    }

    public private(set) var step: Step
    public var serverURL = ""
    public private(set) var server: ServerInfo?
    /// The one-time claim code for an unclaimed server, or a pairing code for a claimed one.
    public var code = ""
    /// The owner's name, asked only when claiming a server.
    public var userName = ""
    public var deviceName: String
    public private(set) var spaces: [Space] = []
    /// The space to link, or nil to create a space named `newSpaceName`.
    public var selectedSpaceID: String?
    public var newSpaceName = "Koidrive"
    public var folderPath: String?
    public private(set) var isBusy = false
    public private(set) var error: SessionError?

    private let app: AppModel

    public init(app: AppModel, mode: Mode, deviceName: String) {
        self.app = app
        self.deviceName = deviceName
        if app.state.account == nil {
            step = .server
        } else if mode == .addFolder || app.state.folders.isEmpty {
            step = .folder
        } else {
            step = .done
        }
    }

    /// Whether signing in claims the server rather than pairing with it.
    public var claimsServer: Bool { server?.claimed == false }

    public var canConnect: Bool { !isBusy && !serverURL.trimmed.isEmpty }

    public var canSignIn: Bool {
        !isBusy && !code.trimmed.isEmpty && !deviceName.trimmed.isEmpty
            && (!claimsServer || !userName.trimmed.isEmpty)
    }

    public var canLink: Bool {
        !isBusy && folderPath != nil && (selectedSpaceID != nil || !newSpaceName.trimmed.isEmpty)
    }

    public func connect() async {
        guard canConnect else { return }
        await attempt {
            server = try await app.connect(url: serverURL)
            step = .signIn
        }
    }

    public func signIn() async {
        guard canSignIn else { return }
        await attempt {
            if claimsServer {
                _ = try await app.claim(code: code, userName: userName, deviceName: deviceName)
            } else {
                _ = try await app.pair(code: code, deviceName: deviceName)
            }
            code = ""
            step = .folder
            try await reloadSpaces()
        }
    }

    public func loadSpaces() async {
        await attempt { try await reloadSpaces() }
    }

    public func link() async {
        guard canLink, let folderPath else { return }
        await attempt {
            let spaceID: String
            if let selectedSpaceID {
                spaceID = selectedSpaceID
            } else {
                spaceID = try await app.createSpace(name: newSpaceName).spaceId
                selectedSpaceID = spaceID
                try await reloadSpaces()
            }
            _ = try await app.linkFolder(spaceID: spaceID, path: folderPath)
            app.start()
            step = .done
        }
    }

    /// Goes back to the server step, for connecting to another server before signing in.
    public func changeServer() {
        guard step == .signIn else { return }
        error = nil
        step = .server
    }

    private func reloadSpaces() async throws {
        spaces = try await app.spaces()
        if selectedSpaceID == nil || !spaces.contains(where: { $0.spaceId == selectedSpaceID }) {
            selectedSpaceID = spaces.first?.spaceId
        }
    }

    /// Runs one step at a time: a step asked for while another runs is dropped, so two space
    /// reloads never race to pick the selected space.
    private func attempt(_ body: () async throws -> Void) async {
        guard !isBusy else { return }
        isBusy = true
        error = nil
        defer { isBusy = false }
        do {
            try await body()
        } catch let failure as SessionError {
            error = failure
        } catch {
            preconditionFailure("session calls only throw SessionError: \(error)")
        }
    }
}

extension String {
    fileprivate var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
