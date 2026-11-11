// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import Foundation
import Observation

/// The app's view of a sync session: its state, kept current from the session's events, and
/// the actions the windows take. Calls that can reach the server run off the main actor.
@MainActor
@Observable
public final class AppModel {
    public private(set) var state: SessionState
    /// Questions not shown to the person yet. Every unanswered question counts as unseen when the
    /// app launches, so the questions window opens again after a relaunch.
    public private(set) var unseenQuestionIDs: Set<String>
    /// Whether this profile sends anonymous usage reports, read again after every session call.
    public private(set) var telemetry: TelemetrySetting
    public let session: SyncSession
    @ObservationIgnored private var consumer: Task<Void, Never>?

    public init(session: SyncSession, events: AsyncStream<SessionEvent>) {
        self.session = session
        let state = session.state()
        self.state = state
        unseenQuestionIDs = Set(state.questions.map(\.id))
        telemetry = session.telemetry()
        consumer = Task { [weak self] in
            for await event in events {
                self?.apply(event)
            }
        }
    }

    /// A model backed by the sync driver, on the sync profile in `profileDirectory`.
    public static func open(profileDirectory: URL) throws -> AppModel {
        let (session, events) = try SyncSession.openWithEventStream(profileDirectory: profileDirectory)
        return AppModel(session: session, events: events)
    }

    /// A model backed by the fake core, for previews and tests. With `scenario`, it starts from
    /// that scenario instead of a first run.
    public static func fake(scenario: FakeScenario? = nil) -> AppModel {
        let (session, events) = SyncSession.fakeWithEventStream()
        if let scenario {
            _ = try? session.fakeCore()?.loadScenario(scenario: scenario)
        }
        return AppModel(session: session, events: events)
    }

    /// The scenario that the `FakeScenario` preference names, such as `massDelete`, for starting the app on the fake
    /// core. Only a debug build reads the preference, so a release build always opens the sync profile.
    public static func launchScenario(in defaults: UserDefaults) -> FakeScenario? {
        #if DEBUG
        let all: [String: FakeScenario] = [
            "firstRun": .firstRun, "upToDate": .upToDate, "syncing": .syncing, "paused": .paused,
            "offline": .offline, "massDelete": .massDelete, "ambiguousDisappearance": .ambiguousDisappearance,
            "recovery": .recovery, "recoveredWithoutQuestions": .recoveredWithoutQuestions,
            "remoteMassDelete": .remoteMassDelete, "manyQuestions": .manyQuestions, "problems": .problems,
        ]
        return defaults.string(forKey: "FakeScenario").flatMap { all[$0] }
        #else
        return nil
        #endif
    }

    deinit {
        consumer?.cancel()
    }

    /// Applies one event. Each event replaces one part of the state.
    public func apply(_ event: SessionEvent) {
        switch event {
        case .status(let status): state.status = status
        case .progress(let progress): state.progress = progress
        case .account(let account): state.account = account
        case .folders(let folders): state.folders = folders
        case .recentChanges(let changes): state.recentChanges = changes
        case .questions(let questions): replaceQuestions(questions)
        case .problems(let problems): state.problems = problems
        case .notices(let notices): state.notices = notices
        }
    }

    private func replaceQuestions(_ questions: [Question]) {
        let old = Set(state.questions.map(\.id))
        let current = Set(questions.map(\.id))
        unseenQuestionIDs = unseenQuestionIDs.intersection(current).union(current.subtracting(old))
        state.questions = questions
    }

    private func refresh() {
        let latest = session.state()
        apply(.status(status: latest.status))
        apply(.progress(progress: latest.progress))
        apply(.account(account: latest.account))
        apply(.folders(folders: latest.folders))
        apply(.recentChanges(changes: latest.recentChanges))
        apply(.questions(questions: latest.questions))
        apply(.problems(problems: latest.problems))
        apply(.notices(notices: latest.notices))
        telemetry = session.telemetry()
    }

    /// Runs a session call off the main actor, then takes the session's latest state, so the
    /// call's changes are visible when it returns.
    private func run<T: Sendable>(_ call: @escaping @Sendable (SyncSession) throws -> T) async throws -> T {
        let session = session
        let result = await Task.detached { Result { try call(session) } }.value
        refresh()
        return try result.get()
    }

    // MARK: Derived state

    /// True until this device is signed in and has a linked folder.
    public var needsSetup: Bool { state.account == nil || state.folders.isEmpty }
    public var isPaused: Bool { state.status == .paused }
    public var hasUnseenQuestions: Bool { !unseenQuestionIDs.isEmpty }
    /// Questions and problems together.
    public var attentionCount: Int { state.questions.count + state.problems.count }

    /// What the menu bar popover leads with, in priority order: setup, then a folder that stopped,
    /// then unanswered questions, then the sync status.
    public var headline: Headline {
        if needsSetup { return .setup }
        if state.status == .halted { return .halted }
        if !state.questions.isEmpty { return .questions }
        return .status(state.status)
    }

    /// The badge on the menu bar icon. It follows the headline, except that a problem that
    /// stopped no folder still shows the problem badge.
    public var iconBadge: IconBadge {
        switch headline {
        case .setup: .none
        case .halted: .problem
        case _ where !state.problems.isEmpty: .problem
        case .questions: .question
        case .status(.offline): .offline
        case .status(.paused): .paused
        case .status(.syncing), .status(.connecting): .syncing
        case .status: .none
        }
    }

    /// How far the current sync has got, from 0 to 1, or nil when nothing is being transferred.
    public var progressFraction: Double? {
        guard let progress = state.progress, progress.bytesTotal > 0 else { return nil }
        return min(1, Double(progress.bytesDone) / Double(progress.bytesTotal))
    }

    public func folder(spaceID: String) -> LinkedFolder? {
        state.folders.first { $0.spaceId == spaceID }
    }

    /// Unanswered questions grouped by space, in the order of the linked folders. A space with a
    /// recovery carries its cause once, and its recovery groups come in the design's order.
    public var questionSections: [QuestionSection] {
        let bySpace = Dictionary(grouping: state.questions, by: \.spaceId)
        let order = state.folders.map(\.spaceId) + bySpace.keys.sorted().filter { id in
            !state.folders.contains { $0.spaceId == id }
        }
        return order.compactMap { spaceID in
            guard let questions = bySpace[spaceID] else { return nil }
            let cause = questions.lazy.compactMap(\.recoveryCause).first
            return QuestionSection(
                spaceID: spaceID,
                folder: folder(spaceID: spaceID),
                recoveryCause: cause,
                questions: questions.sorted { $0.sortKey < $1.sortKey }
            )
        }
    }

    /// False when every unanswered question is in this Mac's only linked folder, so the list needs no space headings.
    public var questionSectionsShowSpace: Bool {
        state.folders.count != 1 || questionSections.contains { $0.folder == nil }
    }

    /// Problems grouped by the space they concern; problems of the whole session come first.
    public var problemSections: [ProblemSection] {
        let bySpace = Dictionary(grouping: state.problems) { $0.spaceId ?? "" }
        let order = [""] + state.folders.map(\.spaceId) + bySpace.keys.sorted()
        var seen = Set<String>()
        return order.compactMap { key in
            guard seen.insert(key).inserted, let problems = bySpace[key] else { return nil }
            let spaceID = key.isEmpty ? nil : key
            return ProblemSection(spaceID: spaceID, folder: spaceID.flatMap(folder(spaceID:)), problems: problems)
        }
    }

    // MARK: Actions

    public func start() {
        session.start()
        refresh()
    }

    public func stop() {
        session.stop()
        refresh()
    }

    /// Pauses a running session or resumes a paused one. Throws `NotSaved` when the pause
    /// cannot be saved, in which case nothing changes.
    public func togglePause() throws {
        defer { refresh() }
        if isPaused { try session.resume() } else { try session.pause() }
    }

    /// Stores the usage-report choice, then reads the setting again. Throws `NotSaved` when the choice cannot be
    /// saved. While the setting is locked, the choice is stored but applies only once the environment variable is gone.
    public func setTelemetry(enabled: Bool) throws {
        defer { refresh() }
        try session.setTelemetry(enabled: enabled)
    }

    /// Records that the questions window showed every current question.
    public func markQuestionsSeen() {
        unseenQuestionIDs.removeAll()
    }

    public func answer(_ question: Question, with answer: Answer) async throws {
        let id = question.id
        try await run { try $0.answer(questionId: id, answer: answer) }
    }

    public func dismiss(_ notice: Notice) async throws {
        let id = notice.id
        try await run { try $0.dismissNotice(noticeId: id) }
    }

    public func connect(url: String) async throws -> ServerInfo {
        try await run { try $0.connect(url: url) }
    }

    public func claim(code: String, userName: String, deviceName: String) async throws -> Account {
        try await run { try $0.claim(code: code, userName: userName, deviceName: deviceName) }
    }

    public func pair(code: String, deviceName: String) async throws -> Account {
        try await run { try $0.pair(code: code, deviceName: deviceName) }
    }

    /// Signs this Mac out and revokes it on the server. Unless `confirmLastDevice` is set, it changes nothing and throws
    /// `SessionError.LastDevice` when no other device of the account is active, or `SessionError.NotRevoked` when the
    /// server can't be reached, so the person can be told that they may lose their last device.
    public func signOut(confirmLastDevice: Bool = false) async throws {
        try await run { try $0.signOut(confirmLastDevice: confirmLastDevice) }
    }

    /// The account's devices that are not revoked.
    public func devices() async throws -> [Device] {
        try await run { try $0.devices() }
    }

    /// Revokes another device of the account: it can no longer sync, and the files already on it stay there. This Mac
    /// leaves with `signOut` instead, so revoking it throws `SessionError.ThisDevice`.
    public func revokeDevice(_ device: Device) async throws {
        let id = device.deviceId
        try await run { try $0.revokeDevice(deviceId: id) }
    }

    public func startPairing() async throws -> PairingCode {
        try await run { try $0.startPairing() }
    }

    public func spaces() async throws -> [Space] {
        try await run { try $0.spaces() }
    }

    public func createSpace(name: String) async throws -> Space {
        try await run { try $0.createSpace(name: name) }
    }

    /// What each of the account's spaces stores, in the order the server lists the spaces. A space
    /// the server refuses, such as one a rewind holds closed, keeps its refusal and does not hide
    /// the others. Only a device that is not signed in fails the whole list.
    public func storage() async throws -> [SpaceStorage] {
        try await run { session in
            try session.spaces().map { space in
                do {
                    return SpaceStorage(space: space, usage: .success(try session.spaceUsage(spaceId: space.spaceId)))
                } catch SessionError.NotSignedIn {
                    throw SessionError.NotSignedIn
                } catch let refusal as SessionError {
                    return SpaceStorage(space: space, usage: .failure(refusal))
                }
            }
        }
    }

    public func linkFolder(spaceID: String, path: String) async throws -> LinkedFolder {
        try await run { try $0.linkFolder(spaceId: spaceID, path: path) }
    }

    public func unlinkFolder(spaceID: String) async throws {
        try await run { try $0.unlinkFolder(spaceId: spaceID) }
    }
}

public enum Headline: Equatable, Sendable {
    case setup
    case halted
    case questions
    case status(SyncStatus)
}

public enum IconBadge: String, Equatable, Sendable {
    case none
    case syncing
    case paused
    case offline
    case question
    case problem
}

/// What one space stores (RFC 0010), or why the server refused to say.
public struct SpaceStorage: Identifiable, Equatable, Sendable {
    public var id: String { space.spaceId }
    public let space: Space
    public let usage: Result<SpaceUsage, SessionError>
}

extension SpaceUsage {
    /// The bytes of every chunk the space holds. Each chunk counts once.
    public var totalBytes: UInt64 {
        liveBytes + trashBytes + oldVersionBytes + unreferencedBytes
    }
}

/// The unanswered questions of one space.
public struct QuestionSection: Identifiable, Equatable, Sendable {
    public var id: String { spaceID }
    public let spaceID: String
    /// The space's linked folder, if it is still linked.
    public let folder: LinkedFolder?
    /// The cause of the space's recovery, shown once above its groups.
    public let recoveryCause: RecoveryCause?
    public let questions: [Question]
}

/// The problems of one space, or of the whole session when `spaceID` is nil.
public struct ProblemSection: Identifiable, Equatable, Sendable {
    public var id: String { spaceID ?? "" }
    public let spaceID: String?
    public let folder: LinkedFolder?
    public let problems: [Problem]
}

extension Question: Identifiable {
    /// Files the answer affects, over all items.
    public var affectedFiles: UInt64 { items.reduce(0) { $0 + $1.files } }

    public var recoveryCause: RecoveryCause? {
        if case .recovery(let cause, _) = kind { cause } else { nil }
    }

    public var recoveryGroup: RecoveryGroup? {
        if case .recovery(_, let group) = kind { group } else { nil }
    }

    fileprivate var sortKey: (Int, Int64) {
        let rank = switch kind {
        case .massDelete: 0
        case .ambiguousDisappearance: 1
        case .recovery(_, let group): 2 + group.rank
        }
        return (rank, askedAtMs)
    }
}

extension Problem: Identifiable {}

extension Notice: Identifiable {}

extension RecoveryGroup {
    fileprivate var rank: Int {
        switch self {
        case .createdAfterRestorePoint: 0
        case .changedOnThisDevice: 1
        case .createdOnThisDevice: 2
        case .deletedOnThisDevice: 3
        case .requestedBeforeReset: 4
        }
    }
}

extension Answer {
    /// Whether the answer removes something: from the server and every device for `delete`,
    /// or this device's copies, into the local trash, for `discard`.
    public var isDestructive: Bool { self == .delete || self == .discard }
}
