// SPDX-License-Identifier: Apache-2.0

import AppKit
import KoidriveFFI
import KoidriveModel
import SwiftUI

@main
struct KoidriveApp: App {
    @State private var model: AppModel

    init() {
        let model: AppModel
        if let scenario = AppModel.launchScenario(in: .standard) {
            model = AppModel.fake(scenario: scenario)
        } else {
            let profile = URL.applicationSupportDirectory.appending(path: "Koidrive", directoryHint: .isDirectory)
            do {
                model = try AppModel.open(profileDirectory: profile)
            } catch {
                Self.quit(after: error)
            }
        }
        model.start()
        _model = State(initialValue: model)
    }

    /// Tells the person why the sync profile can't be opened, then quits.
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
        } label: {
            MenuBarLabel(model: model)
        }
        .menuBarExtraStyle(.window)

        Window("Set Up Koidrive", id: WindowID.setup) {
            SetupView()
                .environment(model)
        }
        .windowResizability(.contentSize)

        Window("Questions", id: WindowID.questions) {
            QuestionsView()
                .environment(model)
        }
        .defaultSize(width: 860, height: 560)

        Settings {
            SettingsView()
                .environment(model)
        }
    }
}

enum WindowID {
    static let setup = "setup"
    static let questions = "questions"
}

extension OpenWindowAction {
    /// Opens a window and brings the app forward, which a menu bar app must do itself.
    @MainActor
    func show(_ id: String) {
        self(id: id)
        NSApp.activate()
    }
}
