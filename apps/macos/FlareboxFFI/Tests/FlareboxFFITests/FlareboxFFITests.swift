// SPDX-License-Identifier: Apache-2.0

import Foundation
import os
import Testing

import FlareboxFFI

@Test func versionComesFromTheRustCore() throws {
    let cargoToml = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .appending(path: "../../../../../Cargo.toml")
        .standardizedFileURL
    let manifest = try String(contentsOf: cargoToml, encoding: .utf8)
    let workspacePackage = try #require(
        manifest.components(separatedBy: "[workspace.package]").dropFirst().first?
            .components(separatedBy: "\n[").first
    )
    let workspaceVersion = try #require(
        workspacePackage.firstMatch(of: /^version\s*=\s*"([^"]+)"/.anchorsMatchLineEndings())
    )

    #expect(String(workspaceVersion.1) == version())
}

private final class Recorder: SessionListener {
    let seen = OSAllocatedUnfairLock<[SessionEvent]>(initialState: [])

    func onEvent(event: SessionEvent) {
        seen.withLock { $0.append(event) }
    }

    var statuses: [SyncStatus] {
        seen.withLock { events in
            events.compactMap {
                if case .status(let status) = $0 { status } else { nil }
            }
        }
    }
}

@Test func sessionStreamsEachStatusChange() throws {
    let recorder = Recorder()
    let session = SyncSession.fake(listener: recorder, stateDir: nil)
    #expect(session.state().status == .stopped)

    session.start()
    #expect(session.state().status == .needsSetup)
    try #require(session.fakeCore()).loadScenario(scenario: .upToDate)
    try session.pause()
    try session.resume()
    session.stop()

    #expect(recorder.statuses == [.needsSetup, .idle, .paused, .idle, .stopped])
}

@Test func signInAndLinkAFolder() throws {
    let session = SyncSession.fake(listener: Recorder(), stateDir: nil)
    let fake = try #require(session.fakeCore())
    fake.failNext(error: .ServerUnreachable(reason: "timed out"))
    #expect(throws: SessionError.ServerUnreachable(reason: "timed out")) {
        try session.connect(url: "https://box.example.com")
    }
    let server = try session.connect(url: "https://box.example.com/")
    #expect(server.url == "https://box.example.com")
    #expect(!server.claimed)
    let account = try session.claim(code: "abcd-efgh-jklm", userName: "Ada", deviceName: "Mac")
    #expect(session.state().account == account)

    let space = try session.createSpace(name: "Documents")
    let folder = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: folder) }
    let linked = try session.linkFolder(spaceId: space.spaceId, path: folder.path(percentEncoded: false))
    #expect(session.state().folders == [linked])
    #expect(throws: SessionError.SpaceAlreadyLinked) {
        try session.linkFolder(spaceId: space.spaceId, path: folder.path(percentEncoded: false))
    }
}

@Test func questionsTakeOnlyTheirOwnAnswers() throws {
    let session = SyncSession.fake(listener: Recorder(), stateDir: nil)
    let fake = try #require(session.fakeCore())
    try fake.loadScenario(scenario: .recovery)
    let questions = session.state().questions
    #expect(questions.count == 5)
    let created = try #require(questions.first {
        if case .recovery(_, .createdAfterRestorePoint) = $0.kind { true } else { false }
    })
    #expect(created.answers == [.keep, .discard])

    #expect(throws: SessionError.AnswerNotAllowed) {
        try session.answer(questionId: created.id, answer: .publish)
    }
    try session.answer(questionId: created.id, answer: .keep)
    #expect(throws: SessionError.QuestionNotFound) {
        try session.answer(questionId: created.id, answer: .keep)
    }
    #expect(fake.givenAnswers() == [GivenAnswer(questionId: created.id, answer: .keep)])
}

@Test func noticesDevicesAndSaveFailuresAreTyped() throws {
    let session = SyncSession.fake(listener: Recorder(), stateDir: nil)
    let fake = try #require(session.fakeCore())
    try fake.loadScenario(scenario: .remoteMassDelete)
    #expect(try session.devices() == .unavailable)
    let notice = try #require(session.state().notices.first)
    guard case .remoteMassDelete(_, let affectedFiles, _, _) = notice.kind else {
        Issue.record("expected a remote mass delete, got \(notice.kind)")
        return
    }
    #expect(affectedFiles == 1_200)

    fake.failSaves(fail: true)
    #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
        try session.dismissNotice(noticeId: notice.id)
    }
    #expect(session.state().notices == [notice])
    #expect(session.state().problems.contains {
        if case .stateNotWritable = $0.kind { true } else { false }
    })
    fake.failSaves(fail: false)
    try session.dismissNotice(noticeId: notice.id)
    #expect(session.state().notices.isEmpty)
    #expect(session.state().problems.isEmpty)
}

@Test func unansweredQuestionsSurviveARelaunch() throws {
    let directory = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: directory) }
    var (session, _) = SyncSession.fakeWithEventStream(stateDirectory: directory)
    try #require(session.fakeCore()).loadScenario(scenario: .massDelete)
    let asked = session.state().questions
    #expect(asked.count == 1)
    #expect(asked[0].items.count == 14)

    (session, _) = SyncSession.fakeWithEventStream(stateDirectory: directory)
    #expect(session.state().questions == asked)
}

@Test(.timeLimit(.minutes(1)))
func eventStreamFinishesWhenTheSessionIsReleased() async {
    var session: SyncSession?
    let events: AsyncStream<SessionEvent>
    (session, events) = SyncSession.fakeWithEventStream()
    async let seen: [SessionEvent] = {
        var seen: [SessionEvent] = []
        for await event in events {
            seen.append(event)
        }
        return seen
    }()

    session?.start()
    session?.stop()
    session = nil

    #expect(await seen == [
        .questions(questions: []), .notices(notices: []), .status(status: .needsSetup), .status(status: .stopped),
    ])
}
