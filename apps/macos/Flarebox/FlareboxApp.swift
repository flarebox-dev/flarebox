// SPDX-License-Identifier: Apache-2.0

import AppKit
import FlareboxFFI
import FlareboxModel
import SwiftUI

@main
struct FlareboxApp: App {
    @State private var model: AppModel

    init() {
        let model: AppModel
        if let scenario = UserDefaults.standard.string(forKey: "FakeScenario").flatMap(FakeScenario.init(name:)) {
            model = AppModel.fake(scenario: scenario)
        } else {
            let profile = URL.applicationSupportDirectory.appending(path: "Flarebox", directoryHint: .isDirectory)
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
        alert.messageText = String(localized: "Flarebox can't start")
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

        Window("Set Up Flarebox", id: WindowID.setup) {
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
