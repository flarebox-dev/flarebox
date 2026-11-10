// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import Foundation
import Testing

import FlareboxModel

private func temporaryDirectory() throws -> URL {
    let url = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

private func item(_ path: String) -> HeldItem {
    HeldItem(nodeId: path, path: path, kind: .file, files: 1, detail: .plain)
}

@MainActor
@Suite struct AppModelTests {
    @Test func eventsReplaceTheirPartOfTheState() {
        let model = AppModel.fake()
        model.apply(.status(status: .syncing))
        model.apply(.progress(progress: SyncProgress(
            filesToUpload: 1, filesToDownload: 2, bytesDone: 25, bytesTotal: 100, current: nil
        )))
        #expect(model.state.status == .syncing)
        #expect(model.progressFraction == 0.25)

        model.apply(.progress(progress: nil))
        #expect(model.progressFraction == nil)
        model.apply(.progress(progress: SyncProgress(
            filesToUpload: 0, filesToDownload: 0, bytesDone: 0, bytesTotal: 0, current: nil
        )))
        #expect(model.progressFraction == nil)
    }

    @Test func firstRunNeedsSetupAndStartsInTheSetupStep() {
        let model = AppModel.fake(scenario: .firstRun)
        model.start()
        #expect(model.needsSetup)
        #expect(model.state.status == .needsSetup)
        #expect(SetupModel(app: model, mode: .firstRun, deviceName: "Mac").step == .server)
        #expect(SetupModel(app: model, mode: .addFolder, deviceName: "Mac").step == .server)

        let ready = AppModel.fake(scenario: .upToDate)
        ready.start()
        #expect(!ready.needsSetup)
        #expect(ready.state.status == .idle)
        #expect(SetupModel(app: ready, mode: .firstRun, deviceName: "Mac").step == .done)
        #expect(SetupModel(app: ready, mode: .addFolder, deviceName: "Mac").step == .folder)
    }

    @Test func pauseAndResume() throws {
        let model = AppModel.fake(scenario: .upToDate)
        model.start()
        try model.togglePause()
        #expect(model.isPaused)
        try model.togglePause()
        #expect(!model.isPaused)

        try #require(model.session.fakeCore()).failSaves(fail: true)
        #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
            try model.togglePause()
        }
        #expect(!model.isPaused)
        #expect(model.state.problems.count == 1)
        model.stop()
        #expect(model.state.status == .stopped)
    }

    @Test func unansweredQuestionsAreUnseenAtLaunch() throws {
        var model = AppModel.fake(scenario: .recovery)
        model.markQuestionsSeen()
        #expect(!model.hasUnseenQuestions)
        let questions = model.state.questions

        model = AppModel(session: model.session, events: AsyncStream { $0.finish() })
        #expect(model.state.questions == questions)
        #expect(model.unseenQuestionIDs == Set(questions.map(\.id)))
        model.markQuestionsSeen()
        #expect(!model.hasUnseenQuestions)
    }

    @Test func onlyNewQuestionsBecomeUnseen() throws {
        let model = AppModel.fake(scenario: .ambiguousDisappearance)
        let fake = try #require(model.session.fakeCore())
        model.markQuestionsSeen()
        let old = try #require(model.state.questions.first)

        let new = try fake.ask(spaceId: old.spaceId, kind: .ambiguousDisappearance, items: [item("a")])
        model.apply(.questions(questions: model.session.state().questions))
        #expect(model.unseenQuestionIDs == [new])

        try fake.withdrawQuestion(questionId: new)
        model.apply(.questions(questions: model.session.state().questions))
        #expect(!model.hasUnseenQuestions)
    }

    @Test func answeringRemovesTheQuestionAndRefusesOtherAnswers() async throws {
        let model = AppModel.fake(scenario: .massDelete)
        let question = try #require(model.state.questions.first)
        #expect(question.answers == [.delete, .restore])
        #expect(question.affectedFiles == 4_553)
        #expect(Answer.delete.isDestructive && !Answer.restore.isDestructive)

        await #expect(throws: SessionError.AnswerNotAllowed) {
            try await model.answer(question, with: .publish)
        }
        try await model.answer(question, with: .restore)
        #expect(model.state.questions.isEmpty)
        #expect(model.attentionCount == 0)
        await #expect(throws: SessionError.QuestionNotFound) {
            try await model.answer(question, with: .restore)
        }
    }

    @Test func questionSectionsGroupBySpaceAndOrderRecoveryGroups() throws {
        let model = AppModel.fake(scenario: .manyQuestions)
        let sections = model.questionSections
        #expect(sections.map(\.folder?.spaceName) == ["Documents", "Photos"])
        #expect(sections.reduce(0) { $0 + $1.questions.count } == 31)

        let documents = sections[0]
        #expect(documents.recoveryCause == nil)
        if case .massDelete = documents.questions[0].kind {} else {
            Issue.record("the mass delete comes first")
        }
        #expect(documents.questions[0].items.count == 1_500)

        let photos = sections[1]
        #expect(photos.recoveryCause?.trigger == .spaceRewound)
        #expect(photos.questions.map(\.recoveryGroup) == [
            .createdAfterRestorePoint, .changedOnThisDevice, .createdOnThisDevice,
            .deletedOnThisDevice, .requestedBeforeReset,
        ])
        #expect(photos.questions.allSatisfy { $0.items.count == 300 })
    }

    @Test func problemSectionsPutSessionProblemsFirst() {
        let model = AppModel.fake(scenario: .problems)
        let sections = model.problemSections
        #expect(sections.map(\.spaceID) == [nil, "space-documents", "space-photos", "space-archive"])
        #expect(sections.reduce(0) { $0 + $1.problems.count } == 19)
        #expect(model.attentionCount == 19)
        #expect(model.folder(spaceID: "space-photos")?.state == .halted)
    }

    @Test func noticesStayUntilDismissed() async throws {
        let model = AppModel.fake(scenario: .remoteMassDelete)
        #expect(model.state.questions.isEmpty)
        #expect(model.attentionCount == 0)
        let notice = try #require(model.state.notices.first)

        try #require(model.session.fakeCore()).failSaves(fail: true)
        await #expect(throws: SessionError.NotSaved(reason: "saving is scripted to fail")) {
            try await model.dismiss(notice)
        }
        #expect(model.state.notices == [notice])

        try #require(model.session.fakeCore()).failSaves(fail: false)
        try await model.dismiss(notice)
        #expect(model.state.notices.isEmpty)
        #expect(model.state.problems.isEmpty)
    }

    @Test func aRecoveryWithoutQuestionsShowsOnlyItsNotice() throws {
        let model = AppModel.fake(scenario: .recoveredWithoutQuestions)
        #expect(!model.hasUnseenQuestions)
        #expect(model.questionSections.isEmpty)
        let notice = try #require(model.state.notices.first)
        guard case .recovery(let cause, let heldFiles) = notice.kind else {
            Issue.record("expected a recovery notice")
            return
        }
        #expect(cause.trigger == .spaceRewound)
        #expect(heldFiles == 0)
    }

    @Test func accountActions() async throws {
        let model = AppModel.fake(scenario: .upToDate)
        #expect(try await model.devices() == .unavailable)
        let code = try await model.startPairing()
        #expect(code.code.count == 8)
        try await model.unlinkFolder(spaceID: "space-photos")
        #expect(model.state.folders.map(\.spaceName) == ["Documents"])
        try await model.signOut()
        #expect(model.state.account == nil)
        #expect(model.needsSetup)
        await #expect(throws: SessionError.NotSignedIn) { try await model.devices() }
    }
}

@MainActor
@Suite struct SetupModelTests {
    @Test func claimAServerAndLinkANewSpace() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .firstRun)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Ada's Mac")
        #expect(!setup.canConnect)

        setup.serverURL = "box.example.com"
        await setup.connect()
        #expect(setup.error == .InvalidServerUrl)
        #expect(setup.step == .server)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        #expect(setup.claimsServer)

        setup.code = "ABCD-EFGH-JKLM"
        #expect(!setup.canSignIn)
        setup.userName = "Ada"
        #expect(setup.canSignIn)
        await setup.signIn()
        #expect(setup.error == nil)
        #expect(setup.step == .folder)
        #expect(setup.spaces.isEmpty)
        #expect(setup.selectedSpaceID == nil)

        #expect(!setup.canLink)
        setup.folderPath = folder.path(percentEncoded: false)
        #expect(setup.canLink)
        await setup.link()
        #expect(setup.error == nil)
        #expect(setup.step == .done)
        #expect(app.state.folders.map(\.spaceName) == ["Flarebox"])
        #expect(app.state.status == .idle)
        #expect(!app.needsSetup)
    }

    @Test func pairWithAClaimedServerAndLinkAnExistingSpace() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).setServerClaimed(claimed: true)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(!setup.claimsServer)

        setup.code = "WRONG"
        await setup.signIn()
        #expect(setup.error == .InvalidCode)
        #expect(setup.step == .signIn)

        setup.code = "ABCD-EFGH"
        await setup.signIn()
        #expect(setup.step == .folder)
        let shared = try #require(setup.spaces.first)
        #expect(setup.selectedSpaceID == shared.spaceId)

        setup.folderPath = folder.appending(path: "missing").path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == .FolderNotFound)
        setup.folderPath = folder.path(percentEncoded: false)
        await setup.link()
        #expect(setup.step == .done)
        #expect(app.state.folders.map(\.spaceId) == [shared.spaceId])
    }

    @Test func serverErrorsKeepTheStep() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        try #require(app.session.fakeCore()).failNext(error: .ServerUnreachable(reason: "timed out"))
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.error == .ServerUnreachable(reason: "timed out"))
        #expect(setup.step == .server)
        await setup.connect()
        #expect(setup.step == .signIn)
        setup.changeServer()
        #expect(setup.step == .server)
    }

    @Test func addAFolderToAConfiguredDeviceFinishesWhenTheLinkSucceeds() async throws {
        let folder = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: folder) }
        let app = AppModel.fake(scenario: .upToDate)
        app.start()
        let linked = app.state.folders.map(\.spaceId)
        try #require(!linked.isEmpty)
        let setup = SetupModel(app: app, mode: .addFolder, deviceName: "Mac")
        #expect(setup.step == .folder)

        await setup.loadSpaces()
        setup.selectedSpaceID = nil
        setup.newSpaceName = "Projects"
        setup.folderPath = folder.appending(path: "missing").path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == .FolderNotFound)
        #expect(setup.step == .folder)

        setup.folderPath = folder.path(percentEncoded: false)
        await setup.link()
        #expect(setup.error == nil)
        #expect(setup.step == .done)
        #expect(app.state.folders.count == linked.count + 1)
        #expect(app.state.folders.filter { $0.spaceName == "Projects" }.count == 1)
        #expect(setup.spaces.filter { $0.name == "Projects" }.count == 1)
    }

    @Test func incompleteStepsMakeNoCall() async throws {
        let app = AppModel.fake(scenario: .firstRun)
        let setup = SetupModel(app: app, mode: .firstRun, deviceName: "Mac")
        await setup.connect()
        #expect(setup.error == nil)
        #expect(setup.step == .server)

        setup.serverURL = "https://box.example.com"
        await setup.connect()
        #expect(setup.step == .signIn)
        await setup.signIn()
        #expect(setup.error == nil)
        #expect(setup.step == .signIn)
        await setup.link()
        #expect(setup.error == nil)
        #expect(app.state.account == nil)
    }
}
