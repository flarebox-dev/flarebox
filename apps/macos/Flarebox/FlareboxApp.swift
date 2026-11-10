// SPDX-License-Identifier: Apache-2.0

import AppKit
import SwiftUI

@main
struct FlareboxApp: App {
    @State private var session = SessionModel()

    var body: some Scene {
        MenuBarExtra("Flarebox", systemImage: session.status == .stopped ? "arrow.triangle.2.circlepath.circle" : "arrow.triangle.2.circlepath.circle.fill") {
            Text("Flarebox core \(session.version)")
            Text(session.status == .stopped ? "Sync is stopped" : "Sync is running")
            Divider()
            Button(session.status == .stopped ? "Start Sync" : "Stop Sync") {
                session.toggle()
            }
            Divider()
            Button("Quit Flarebox") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
    }
}
