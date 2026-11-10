// SPDX-License-Identifier: Apache-2.0

import AppKit
import FlareboxFFI
import FlareboxModel
import SwiftUI

/// The first-run setup window: server, sign-in, folder.
struct SetupView: View {
    @Environment(AppModel.self) private var app
    @State private var setup: SetupModel?

    var body: some View {
        Group {
            if let setup {
                SetupSteps(setup: setup)
            } else {
                ProgressView()
            }
        }
        .frame(width: 480)
        .onAppear {
            if setup == nil {
                setup = SetupModel(app: app, deviceName: Host.current().localizedName ?? "Mac")
            }
        }
    }
}

private struct SetupSteps: View {
    @Bindable var setup: SetupModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            StepIndicator(step: setup.step)
            switch setup.step {
            case .server: ServerStep(setup: setup)
            case .signIn: SignInStep(setup: setup)
            case .folder: FolderStep(setup: setup)
            case .done: DoneStep { dismissWindow(id: WindowID.setup) }
            }
            if let error = setup.error {
                Label(error.message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Divider()
            Text(independenceNotice)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(24)
        .disabled(setup.isBusy)
    }
}

private struct StepIndicator: View {
    let step: SetupModel.Step

    var body: some View {
        HStack(spacing: 6) {
            item("Server", for: .server)
            Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            item("Sign In", for: .signIn)
            Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            item("Folder", for: .folder)
        }
        .font(.callout)
    }

    private func item(_ title: LocalizedStringKey, for target: SetupModel.Step) -> some View {
        Text(title)
            .fontWeight(step == target ? .semibold : .regular)
            .foregroundStyle(step == target ? .primary : .secondary)
    }
}

private struct ServerStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Connect to your Flarebox server")
                .font(.title2.weight(.semibold))
            Text("Enter the address of the Flarebox server you deployed, such as https://box.example.com.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            TextField("Server address", text: $setup.serverURL, prompt: Text(verbatim: "https://"))
                .textFieldStyle(.roundedBorder)
                .onSubmit(connect)
            HStack {
                Spacer()
                Button("Connect", action: connect)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!setup.canConnect)
            }
        }
    }

    private func connect() {
        Task { await setup.connect() }
    }
}

private struct SignInStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(setup.claimsServer ? "Claim this server" : "Sign in this Mac")
                .font(.title2.weight(.semibold))
            if let server = setup.server {
                Text("\(server.url) · version \(server.serverVersion)")
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
            }
            if setup.claimsServer {
                Text("This server has no owner yet. Open the server's /setup page, then copy the one-time claim code from the server's logs.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("On a device that is already signed in, open Settings, choose Account, and click Add a Device to get a pairing code.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Form {
                if setup.claimsServer {
                    TextField("Your name", text: $setup.userName)
                    TextField("Claim code", text: $setup.code)
                } else {
                    TextField("Pairing code", text: $setup.code)
                }
                TextField("Name of this Mac", text: $setup.deviceName)
            }
            .onSubmit(signIn)
            HStack {
                Button("Change Server") { setup.changeServer() }
                Spacer()
                Button(setup.claimsServer ? "Claim Server" : "Sign In", action: signIn)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!setup.canSignIn)
            }
        }
    }

    private func signIn() {
        Task { await setup.signIn() }
    }
}

/// Chooses a space and a folder and links them. The settings window reuses it to add a folder.
struct FolderStep: View {
    @Bindable var setup: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Choose a folder to sync")
                .font(.title2.weight(.semibold))
            Text("Flarebox keeps the folder and a space on your server in sync. Everything in the folder is uploaded, and everything in the space is downloaded into it.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Form {
                Picker("Space", selection: $setup.selectedSpaceID) {
                    ForEach(setup.spaces, id: \.spaceId) { space in
                        Text(space.name).tag(Optional(space.spaceId))
                    }
                    Divider()
                    Text("New Space…").tag(String?.none)
                }
                if setup.selectedSpaceID == nil {
                    TextField("Space name", text: $setup.newSpaceName)
                }
                LabeledContent("Folder") {
                    HStack {
                        Text(setup.folderPath ?? String(localized: "None"))
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .foregroundStyle(setup.folderPath == nil ? .secondary : .primary)
                        Button("Choose…", action: chooseFolder)
                    }
                }
            }
            HStack {
                Spacer()
                Button("Link Folder") {
                    Task { await setup.link() }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!setup.canLink)
            }
        }
        .task { await setup.loadSpaces() }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = String(localized: "Choose")
        if panel.runModal() == .OK, let url = panel.url {
            setup.folderPath = url.path(percentEncoded: false)
        }
    }
}

private struct DoneStep: View {
    @Environment(AppModel.self) private var app
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Flarebox is set up", systemImage: "checkmark.circle.fill")
                .font(.title2.weight(.semibold))
                .foregroundStyle(.green)
            ForEach(app.state.folders, id: \.spaceId) { folder in
                LabeledContent(folder.spaceName, value: folder.path)
            }
            Text("Flarebox runs in the menu bar. Add more folders in Settings.")
                .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Done", action: close)
                    .keyboardShortcut(.defaultAction)
            }
        }
    }
}

#Preview("Server") {
    SetupView().environment(AppModel.preview(.firstRun))
}

#Preview("Folder") {
    let app = AppModel.preview(.upToDate)
    let setup = SetupModel(app: app, deviceName: "Mac")
    return FolderStep(setup: setup).padding(24).frame(width: 480).environment(app)
}
