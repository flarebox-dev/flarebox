// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@main
struct KoidriveApp: App {
    @State private var model: AppModel

    init() {
        if CommandLine.arguments.dropFirst().first == "--unregister-login-item" {
            do {
                try LoginItemCommand.unregister()
                exit(0)
            } catch {
                FileHandle.standardError.write(Data("Could not unregister Koidrive's login item: \(error.localizedDescription)\n".utf8))
                exit(1)
            }
        }
        let model: AppModel
        #if DEBUG
        if let directory = Snapshots.directory {
            model = AppModel.fake(scenario: .upToDate)
            Task {
                await Snapshots.render(into: directory)
                exit(0)
            }
            _model = State(initialValue: model)
            return
        }
        #endif
        if let scenario = AppModel.launchScenario(in: .standard) {
            model = AppModel.fake(scenario: scenario)
        } else {
            let helper = Bundle.main.bundleURL.appending(path: "Contents/Helpers/koi")
            do {
                model = try AppModel.open(helper: helper)
            } catch {
                Self.quit(after: error)
            }
        }
        model.start()
        _model = State(initialValue: model)
    }

    private static func quit(after error: any Error) -> Never {
        let alert = NSAlert()
        alert.messageText = String(localized: "Koidrive can't start")
        alert.informativeText = (error as? SessionError)?.message ?? error.localizedDescription
        alert.runModal()
        exit(1)
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environment(model)
                .buttonStyle(.secondary)
        } label: {
            MenuBarLabel(model: model)
        }
        .menuBarExtraStyle(.window)

        Window("Set Up Koidrive", id: WindowID.setup) {
            SetupView()
                .environment(model)
                .buttonStyle(.secondary)
        }
        .windowResizability(.contentSize)

        Window("Needs You", id: WindowID.questions) {
            QuestionsView()
                .environment(model)
                .buttonStyle(.secondary)
        }
        .defaultSize(width: 860, height: 560)

        Settings {
            SettingsView()
                .environment(model)
                .buttonStyle(.secondary)
        }
    }
}

enum WindowID {
    static let setup = "setup"
    static let questions = "questions"
}

extension OpenWindowAction {
    /// Brings the app forward, which a menu bar app must do itself.
    @MainActor
    func show(_ id: String) {
        self(id: id)
        NSApp.activate()
    }
}
