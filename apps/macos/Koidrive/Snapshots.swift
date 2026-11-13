// SPDX-License-Identifier: Apache-2.0

#if DEBUG
import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@MainActor
enum Snapshots {
    static var directory: URL? {
        UserDefaults.standard.string(forKey: "RenderSnapshots").map { URL(filePath: $0, directoryHint: .isDirectory) }
    }

    static func render(into directory: URL) async {
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for dark in [false, true] {
            let suffix = dark ? "dark" : "light"
            for (name, model) in await menuStates() {
                await capture(MenuContent().environment(model).background(.background), width: 330, titled: nil,
                              dark: dark, to: directory.appending(path: "menubar-\(name)-\(suffix).png"))
            }
            for (name, setup, app) in await setupStates() {
                await capture(SetupSteps(setup: setup).environment(app).frame(width: 480), width: 480,
                              titled: "Set Up Koidrive", dark: dark, to: directory.appending(path: "setup-\(name)-\(suffix).png"))
            }
            for (name, scenario) in [("mass-delete", FakeScenario.massDelete), ("recovery", .recovery),
                                     ("ambiguous", .ambiguousDisappearance), ("two-spaces", .manyQuestions),
                                     ("none", .upToDate)] {
                let model = AppModel.preview(scenario)
                await capture(QuestionsView().environment(model).frame(width: 860, height: 560), width: 860, height: 560, titled: "Needs You",
                              dark: dark, to: directory.appending(path: "questions-\(name)-\(suffix).png"))
            }
            let oneSpace = AppModel.preview(.massDelete)
            try? oneSpace.session.unlinkFolder(spaceId: "space-photos")
            await capture(QuestionsView().environment(oneSpace).frame(width: 860, height: 560), width: 860, height: 560,
                          titled: "Needs You", dark: dark, to: directory.appending(path: "questions-one-space-\(suffix).png"))
            for (name, scenario, error, answering) in [("answer-failed", FakeScenario.ambiguousDisappearance, SessionError.QuestionNotFound, false),
                                                       ("sending", .massDelete, nil, true)] {
                let model = AppModel.preview(scenario)
                if let question = model.state.questions.first {
                    await capture(QuestionDetail(question: question, folder: model.folder(spaceID: question.spaceId), error: error, answering: answering)
                        .environment(model).background(Color.panel).frame(width: 600, height: 528), width: 600, height: 528, titled: nil,
                                  dark: dark, to: directory.appending(path: "questions-detail-\(name)-\(suffix).png"))
                }
            }
            let mass = AppModel.preview(.massDelete)
            if let question = mass.state.questions.first {
                await capture(ConfirmSheet(title: question.confirmation(for: .delete),
                                           message: question.confirmationMessage(for: .delete),
                                           action: question.title(for: .delete), confirm: {}, cancel: {}),
                              width: 280, titled: nil, dark: dark,
                              to: directory.appending(path: "questions-confirm-\(suffix).png"))
            }
            await capture(ButtonGallery(), width: 432, titled: nil, dark: dark,
                          to: directory.appending(path: "buttons-\(suffix).png"))
            for (name, view) in await settingsStates() {
                await capture(view.frame(width: 600, height: 380), width: 600, height: 380, titled: nil, dark: dark,
                              to: directory.appending(path: "settings-\(name)-\(suffix).png"))
            }
        }
    }

    private static func menuStates() async -> [(String, AppModel)] {
        var states: [(String, AppModel)] = [
            ("1-up-to-date", .preview(.upToDate)), ("2-syncing", .preview(.syncing)), ("3-paused", .preview(.paused)),
            ("4-offline", .preview(.offline)),
        ]
        let question = AppModel.preview(.massDelete)
        if let fake = question.session.fakeCore(), let folder = question.state.folders.last {
            _ = try? fake.notify(spaceId: folder.spaceId, kind: .remoteMassDelete(
                deletions: 214, affectedFiles: 1_086, spaceFiles: 1_840, exceeded: [.shareOfSpace(percent: 50)]))
        }
        states.append(("5-question", question))
        states.append(("5b-recovery", .preview(.recovery)))
        let problem = AppModel.preview(.upToDate)
        if let fake = problem.session.fakeCore(), let folder = problem.state.folders.last {
            _ = try? fake.reportProblem(spaceId: folder.spaceId, path: nil, kind: .folderMissing)
            _ = try? fake.setFolderState(spaceId: folder.spaceId, folderState: .halted)
        }
        states.append(("6-problem", problem))
        states.append(("7-first-run", .preview(.firstRun)))
        let connecting = AppModel.preview(.upToDate)
        connecting.session.fakeCore()?.setActivity(activity: .connecting)
        states.append(("8-connecting", connecting))
        let stopped = AppModel.preview(.upToDate)
        stopped.stop()
        states.append(("9-stopped", stopped))
        try? await Task.sleep(for: .milliseconds(200))
        return states
    }

    private static func setupStates() async -> [(String, SetupModel, AppModel)] {
        func start(claimed: Bool) -> (SetupModel, AppModel) {
            let app = AppModel.preview(.firstRun)
            try? app.session.fakeCore()?.setServerClaimed(claimed: claimed)
            return (SetupModel(app: app, mode: .firstRun, deviceName: "Gunwoo's MacBook Pro"), app)
        }
        let (server, serverApp) = start(claimed: false)
        server.serverURL = "https://koi.example.com"

        let (unreachable, unreachableApp) = start(claimed: false)
        unreachable.serverURL = "https://koi.exmaple.com"
        unreachableApp.session.fakeCore()?.failNext(error: .ServerUnreachable(reason: "A server with the specified hostname could not be found."))
        await unreachable.connect()

        let decidedApp = AppModel.preview(.firstRun, telemetry: TelemetrySetting(enabled: true, decided: true, locked: false))
        let decided = SetupModel(app: decidedApp, mode: .firstRun, deviceName: "Gunwoo's MacBook Pro")
        decided.serverURL = "https://koi.example.com"

        let (pair, pairApp) = start(claimed: true)
        pair.serverURL = "https://koi.example.com"
        await pair.connect()
        pair.code = "K7Q4-M2XD"
        pairApp.session.fakeCore()?.failNext(error: .InvalidCode)
        await pair.signIn()

        let (pairing, pairingApp) = start(claimed: true)
        pairing.serverURL = "https://koi.example.com"
        await pairing.connect()
        pairing.code = "K7Q4-M2XD"

        let (claim, claimApp) = start(claimed: false)
        claim.serverURL = "https://koi.example.com"
        await claim.connect()
        claim.userName = "Gunwoo"
        claim.code = "R3VN-8KPT-Q2ZL"

        let (folder, folderApp) = start(claimed: false)
        folder.serverURL = "https://koi.example.com"
        await folder.connect()
        folder.userName = "Gunwoo"
        folder.code = "R3VN-8KPT-Q2ZL"
        await folder.signIn()
        folder.newSpaceName = "Projects"
        let projects = FileManager.default.temporaryDirectory.appending(path: "Projects", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: projects, withIntermediateDirectories: true)
        folder.folderPath = projects.path(percentEncoded: false)

        func addFolder() -> (SetupModel, AppModel) {
            let app = AppModel.preview(.upToDate)
            return (SetupModel(app: app, mode: .addFolder, deviceName: "Gunwoo's MacBook Pro"), app)
        }
        let documents = FileManager.default.temporaryDirectory.appending(path: "Documents", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: documents, withIntermediateDirectories: true)
        let (existing, existingApp) = addFolder()
        await existing.loadSpaces()
        existing.folderPath = documents.path(percentEncoded: false)
        let (noFolder, noFolderApp) = addFolder()
        await noFolder.loadSpaces()
        let (overlap, overlapApp) = addFolder()
        await overlap.loadSpaces()
        overlap.folderPath = documents.path(percentEncoded: false)
        overlapApp.session.fakeCore()?.failNext(error: .FolderOverlaps(other: "~/Koidrive/Photos"))
        await overlap.link()

        let (done, doneApp) = start(claimed: false)
        done.serverURL = "https://koi.example.com"
        await done.connect()
        done.userName = "Gunwoo"
        done.code = "R3VN-8KPT-Q2ZL"
        await done.signIn()
        done.newSpaceName = "Projects"
        done.folderPath = projects.path(percentEncoded: false)
        await done.link()

        return [("1-server", server, serverApp), ("1a-server-error", unreachable, unreachableApp),
                ("1b-server-decided", decided, decidedApp), ("2-sign-in", pairing, pairingApp),
                ("2a-sign-in-error", pair, pairApp), ("2b-claim", claim, claimApp),
                ("3-folder", folder, folderApp), ("3a-folder-existing", existing, existingApp),
                ("3b-folder-none", noFolder, noFolderApp), ("3c-folder-overlap", overlap, overlapApp),
                ("4-done", done, doneApp)]
    }

    private static func settingsStates() async -> [(String, AnyView)] {
        let folders = AppModel.preview(.upToDate)
        if let fake = folders.session.fakeCore(), let folder = folders.state.folders.last {
            _ = try? fake.setFolderState(spaceId: folder.spaceId, folderState: .halted)
        }
        let pairing = PairingCode(code: "K7Q4-M2XD", expiresAtMs: Int64(Date.now.addingTimeInterval(600).timeIntervalSince1970 * 1000))
        return [
            ("1-folders", AnyView(FoldersSettings().environment(folders))),
            ("1b-folders-empty", AnyView(FoldersSettings().environment(AppModel.preview(.firstRun)))),
            ("2-account", AnyView(AccountSettings(pairing: pairing).environment(AppModel.preview(.upToDate)))),
            ("3-problems", AnyView(ProblemsSettings().environment(AppModel.preview(.problems)))),
            ("4-general", AnyView(GeneralSettings().environment(AppModel.preview(.upToDate)))),
            ("4b-general-usage-reports-off", AnyView(GeneralSettings().environment(
                AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: true, locked: false)))
            )),
            ("4c-general-usage-reports-locked", AnyView(GeneralSettings().environment(
                AppModel.preview(.upToDate, telemetry: TelemetrySetting(enabled: false, decided: false, locked: true)))
            )),
            ("5a-storage-one-space", AnyView(StorageSettings().environment(AppModel.previewWithOneSpace()))),
            ("5b-storage-two-spaces", AnyView(StorageSettings().environment(AppModel.preview(.upToDate)))),
        ]
    }

    private static func capture(_ view: some View, width: CGFloat, height: CGFloat? = nil, titled: String?,
                                dark: Bool, to url: URL) async {
        let appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        let host = NSHostingView(rootView: view
            .buttonStyle(.secondary)
            .background(Color(nsColor: .windowBackgroundColor))
            .environment(\.colorScheme, dark ? .dark : .light))
        host.appearance = appearance
        if height != nil {
            host.sizingOptions = [.minSize]
        }
        let style: NSWindow.StyleMask = titled == nil ? [.borderless] : [.titled, .closable, .miniaturizable]
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: height ?? 400), styleMask: style,
                              backing: .buffered, defer: false)
        window.appearance = appearance
        window.title = titled ?? ""
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFrontRegardless()
        for _ in 0..<3 {
            try? await Task.sleep(for: .milliseconds(250))
            let size = NSSize(width: width, height: height ?? host.fittingSize.height)
            if window.contentRect(forFrameRect: window.frame).size != size {
                window.setContentSize(size)
            }
        }
        try? await Task.sleep(for: .milliseconds(300))
        let screencapture = Process()
        screencapture.executableURL = URL(filePath: "/usr/sbin/screencapture")
        screencapture.arguments = ["-x", "-o", "-l", String(window.windowNumber), url.path(percentEncoded: false)]
        do {
            try screencapture.run()
        } catch {
            fatalError("screencapture could not start for \(url.lastPathComponent): \(error)")
        }
        screencapture.waitUntilExit()
        guard screencapture.terminationStatus == 0 else {
            fatalError("screencapture failed for \(url.lastPathComponent) with status \(screencapture.terminationStatus)")
        }
        window.close()
    }
}

private struct ButtonGallery: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            Button("Restore Version…") {}
                .buttonStyle(.primary)
            Button("Cancel") {}
            HStack(spacing: 12) {
                Button("Show in Finder") {}
                Button("Remove…", role: .destructive) {}
            }
            .controlSize(.small)
            HStack(spacing: 12) {
                Button("Remove Device") {}
                    .buttonStyle(.destructiveConfirm)
                Button("Remove…", role: .destructive) {}
            }
            HStack(spacing: 12) {
                Button("Restore Version…") {}
                    .buttonStyle(.primary)
                Button("Cancel") {}
            }
            .disabled(true)
            HStack(spacing: 8) {
                Spacer()
                Button("Cancel") {}
                Button("Restore Version…") {}
                    .buttonStyle(.primary)
            }
        }
        .padding(24)
    }
}
#endif
