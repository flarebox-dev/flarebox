// SPDX-License-Identifier: Apache-2.0

import AppKit
import SwiftUI

@main
struct FlareboxApp: App {
    @State private var session = SessionModel()

    var body: some Scene {
        MenuBarExtra("Flarebox", systemImage: session.status == .idle ? "arrow.triangle.2.circlepath.circle.fill" : "arrow.triangle.2.circlepath.circle") {
            Text("Flarebox core \(session.version)")
            Text(session.status == .idle ? "Up to date" : "Sync is stopped")
            Divider()
            Button(session.status == .idle ? "Stop Sync" : "Start Sync") {
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
