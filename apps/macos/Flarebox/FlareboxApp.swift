// SPDX-License-Identifier: Apache-2.0

import AppKit
import FlareboxFFI
import FlareboxModel
import SwiftUI

@main
struct FlareboxApp: App {
    @State private var model: AppModel

    init() {
        let stateDirectory = URL.applicationSupportDirectory.appending(path: "Flarebox", directoryHint: .isDirectory)
        let scenario = UserDefaults.standard.string(forKey: "FakeScenario").flatMap(FakeScenario.init(name:))
        let model = AppModel.fake(stateDirectory: stateDirectory, scenario: scenario)
        model.start()
        _model = State(initialValue: model)
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
